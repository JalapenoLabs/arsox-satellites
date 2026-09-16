// Copyright © 2026 Jalapeno Labs

//! Tool calls the host application answers, over a socket it opened itself.
//!
//! A thread's `relayed_mcp_servers` are tools whose implementation lives in the
//! host application rather than anywhere a satellite can reach. The host is
//! frequently not reachable from the satellite at all, so the calls travel the
//! one direction every SDK call already does: the host opens
//! `/v1/threads/{id}/relay` as a WebSocket, and each call an agent makes goes
//! down it while the agent waits.
//!
//! Three pieces meet here:
//!
//! - [`Hub`], the one table of which client is attached to which thread and
//!   which calls are waiting on it, shared by both halves below.
//! - [`socket`], the WebSocket a host application attaches through.
//! - [`mcp`], the streamable HTTP MCP server the agents are pointed at, served
//!   on the LLM proxy's loopback listener under the turn's own grant.
//!
//! # One client per thread
//!
//! Attaching replaces whichever client was attached before. Two clients
//! answering one thread's calls would race to answer each one, and a host
//! application that reconnected after a network blip would otherwise have to
//! wait for its dead socket to time out before its new one received anything.
//! The replaced socket is closed with [`REPLACED_CLOSE_CODE`] and a reason of
//! `RELAY_CLIENT_REPLACED`, and every call still waiting on it fails as a tool
//! error the agent reads.
//!
//! # Live only
//!
//! Nothing is persisted and nothing is replayed. A call means something only
//! while an agent is waiting on it, so a call made while no client is attached
//! fails at once with a message saying so, rather than waiting for a client
//! that may never come while the agent's turn burns wall clock. A host
//! application that attaches later has missed nothing it could still act on.
//!
//! # Every call ends
//!
//! A call waits at most [`CALL_DEADLINE`]. Past it the agent is told the call
//! timed out and the client is sent `ToolCallCancelled`. The same cancellation
//! is sent when the agent stops waiting first, because its turn ended or its
//! harness hung up, so a host application never works on a call nobody will
//! read the answer to without being told.

pub mod mcp;
pub mod socket;

use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::relay::v1::{
    SatelliteRelayFrame, ToolCall, ToolCallCancelled, ToolResult, satellite_relay_frame,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How long one relayed call may wait for its answer.
///
/// Long, because one call can carry a whole file transfer: an upload tool reads
/// the file out of the workspace, sends it to object storage, and answers only
/// once the object is written, which for a multi-gigabyte artifact is minutes.
/// Bounded, because a host application that accepted a call and then stopped
/// answering would otherwise hold the agent, and its turn's wall clock, forever.
///
/// The harness idle bound does not fire while a call waits (see
/// `docs/timeouts.md`), so this is the bound on that silence. Codex is told a
/// slightly longer timeout of its own, so this one is what ends a call.
pub const CALL_DEADLINE: Duration = Duration::from_mins(15);

/// The WebSocket close code a replaced relay client receives.
///
/// From the range RFC 6455 reserves for applications, because none of the
/// registered codes means "another client took your place". The reason carries
/// `RELAY_CLIENT_REPLACED`, which is what a client matches on.
pub const REPLACED_CLOSE_CODE: u16 = 4000;

/// What the relay socket for one attachment is told to do.
#[derive(Debug)]
pub enum Outbound {
    /// Send this frame to the client.
    Frame(SatelliteRelayFrame),

    /// Another client attached. Close with [`REPLACED_CLOSE_CODE`].
    Replaced,
}

/// How a relayed call ended without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallFailure {
    /// No client was attached when the call was made.
    NotConnected,

    /// The client that received the call detached or was replaced before it
    /// answered.
    Disconnected,

    /// [`CALL_DEADLINE`] passed.
    TimedOut,
}

impl CallFailure {
    /// The sentence the agent reads in place of the tool's output.
    ///
    /// Written for the model rather than for an operator: it says what happened
    /// and whether trying again could help, which is the decision the agent has
    /// to make next.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NotConnected => {
                "The host application is not connected to the satellite, so this tool cannot run \
                 right now. Nothing was done. Trying again later may work."
            }
            Self::Disconnected => {
                "The host application disconnected from the satellite before this tool call \
                 finished. Whether it took effect is unknown; check before trying again."
            }
            Self::TimedOut => {
                "The host application did not answer this tool call within 15 minutes, so the \
                 satellite stopped waiting. Whether it took effect is unknown; check before trying \
                 again."
            }
        }
    }
}

/// How many relayed calls one turn's agents are waiting on.
///
/// Shared by the grant, which counts a call for as long as it waits, and the
/// runner, which asks before it decides a silent harness has stopped. An agent
/// waiting on the host application produces no output, and without this the
/// idle bound would tear down a harness for doing exactly what it was asked.
#[derive(Debug, Clone, Default)]
pub struct InFlight {
    in_flight: Arc<AtomicUsize>,
}

/// One relayed call being waited on, counted until it drops.
#[derive(Debug)]
pub struct InFlightCall {
    in_flight: Arc<AtomicUsize>,
}

impl InFlight {
    /// Counts one call as waiting until the returned guard drops.
    #[must_use]
    pub fn start(&self) -> InFlightCall {
        self.in_flight.fetch_add(1, Ordering::SeqCst);

        InFlightCall {
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// Whether any call is being waited on right now.
    #[must_use]
    pub fn any(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }
}

impl Drop for InFlightCall {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Everything about one relayed call except its answer.
#[derive(Debug, Clone)]
pub struct CallRequest {
    pub server: String,
    pub tool: String,
    pub arguments_json: String,
    pub turn_id: String,
}

/// Which client is attached to which thread, and what each is being asked.
///
/// Cheap to clone: clones share one table. Every critical section is a map
/// operation with no await inside it, so a blocking mutex is the right lock and
/// cannot be held across a suspension point by construction.
#[derive(Debug, Clone, Default)]
pub struct Hub {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    attachments: Mutex<HashMap<String, Attachment>>,

    /// Distinguishes one connection from the next on the same thread, so a
    /// socket that was replaced cannot detach or answer for its replacement.
    next_connection: AtomicU64,
}

/// The client attached to one thread.
#[derive(Debug)]
struct Attachment {
    connection: u64,
    outbound: mpsc::UnboundedSender<Outbound>,

    /// Calls sent to this client and not yet answered.
    ///
    /// Dropping the attachment drops every sender here, which is what fails
    /// every call still waiting on a client that detached or was replaced: the
    /// waiting side sees its channel close.
    pending: HashMap<String, oneshot::Sender<ToolResult>>,
}

/// One attached client, as the socket that serves it holds it.
#[derive(Debug)]
pub struct Connection {
    pub id: u64,
    pub outbound: mpsc::UnboundedReceiver<Outbound>,
}

impl Hub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches a client to a thread, replacing any client already attached.
    pub fn attach(&self, thread_id: &str) -> Connection {
        let id = self.inner.next_connection.fetch_add(1, Ordering::Relaxed);
        let (outbound, receiver) = mpsc::unbounded_channel();

        let replaced = self.attachments().insert(
            thread_id.to_owned(),
            Attachment {
                connection: id,
                outbound,
                pending: HashMap::new(),
            },
        );

        if let Some(replaced) = replaced {
            tracing::info!(
                event.name = "relay.client.replaced",
                thread.id = thread_id,
                relay.calls_failed = replaced.pending.len(),
                "a relay client replaced the one attached before it, failing \
                 {{relay.calls_failed}} calls in flight",
            );

            // Unheard when the old socket is already gone, which is fine: it
            // has nobody left to close.
            drop(replaced.outbound.send(Outbound::Replaced));
        }

        Connection {
            id,
            outbound: receiver,
        }
    }

    /// Detaches a client, failing every call still waiting on it.
    ///
    /// Does nothing when `connection` has already been replaced, so a replaced
    /// socket winding down cannot detach the client that took its place.
    pub fn detach(&self, thread_id: &str, connection: u64) {
        let mut attachments = self.attachments();

        if attachments
            .get(thread_id)
            .is_some_and(|attachment| attachment.connection == connection)
        {
            attachments.remove(thread_id);
        }
    }

    /// Hands a client's answer to the call waiting on it.
    ///
    /// An answer for a call nothing is waiting on is dropped with a log line
    /// rather than treated as a protocol error. A result racing a cancellation
    /// is an ordinary outcome, and closing the socket over it would fail every
    /// other call the client is working on.
    pub fn deliver(&self, thread_id: &str, connection: u64, result: ToolResult) {
        let waiting = self
            .attachments()
            .get_mut(thread_id)
            .filter(|attachment| attachment.connection == connection)
            .and_then(|attachment| attachment.pending.remove(&result.call_id));

        let Some(waiting) = waiting else {
            tracing::debug!(
                event.name = "relay.result.unmatched",
                thread.id = thread_id,
                relay.call_id = result.call_id,
                "ignoring a relay result for a call nothing is waiting on",
            );
            return;
        };

        if waiting.send(result).is_err() {
            tracing::debug!(
                event.name = "relay.result.abandoned",
                thread.id = thread_id,
                "a relay result arrived as its caller stopped waiting",
            );
        }
    }

    /// Sends a call to the thread's client and waits for its answer.
    ///
    /// Dropping the returned future stops the wait and tells the client, which
    /// is how a turn that ended or a harness that hung up cancels a call.
    ///
    /// # Errors
    ///
    /// Returns the [`CallFailure`] that ended the call without an answer.
    pub async fn call(
        &self,
        thread_id: &str,
        request: CallRequest,
    ) -> Result<ToolResult, CallFailure> {
        let call_id = uuid::Uuid::now_v7().to_string();
        let deadline = tokio::time::Instant::now() + CALL_DEADLINE;
        let (answer, answered) = oneshot::channel();

        let call = ToolCall {
            call_id: call_id.clone(),
            server: request.server,
            tool: request.tool,
            arguments_json: request.arguments_json,
            turn_id: request.turn_id,
            member_id: None,
            deadline: Some(deadline_timestamp()),
        };

        let connection = {
            let mut attachments = self.attachments();

            let Some(attachment) = attachments.get_mut(thread_id) else {
                return Err(CallFailure::NotConnected);
            };

            let frame = SatelliteRelayFrame {
                frame: Some(satellite_relay_frame::Frame::Call(call)),
            };

            // A socket that has already stopped reading its channel is a client
            // on its way out; the call is failed as a disconnection rather than
            // left to wait out the deadline.
            if attachment.outbound.send(Outbound::Frame(frame)).is_err() {
                return Err(CallFailure::Disconnected);
            }

            attachment.pending.insert(call_id.clone(), answer);
            attachment.connection
        };

        let mut waiting = Waiting {
            hub: self.clone(),
            thread_id: thread_id.to_owned(),
            connection,
            call_id,
            settled: false,
        };

        let outcome = match tokio::time::timeout_at(deadline, answered).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_client_gone)) => {
                // The client took the sender with it, so there is nobody to
                // tell about a cancellation.
                waiting.settled = true;
                Err(CallFailure::Disconnected)
            }
            Err(_elapsed) => Err(CallFailure::TimedOut),
        };

        if outcome.is_ok() {
            waiting.settled = true;
        }

        outcome
    }

    fn attachments(&self) -> MutexGuard<'_, HashMap<String, Attachment>> {
        // A poisoned lock means a thread panicked mid-update, which is a bug the
        // process should stop on rather than a state to reason about.
        self.inner
            .attachments
            .lock()
            .expect("the relay table lock is never poisoned")
    }
}

/// One call still waiting, cancelled with the client if it ends unanswered.
///
/// A guard rather than a call at each exit, for the reason `RevokeOnDrop` is:
/// the wait ends by an answer, by the deadline, or by the future being dropped
/// when the agent's request goes away, and the last of those has no exit to put
/// a call at.
#[derive(Debug)]
struct Waiting {
    hub: Hub,
    thread_id: String,
    connection: u64,
    call_id: String,
    settled: bool,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.settled {
            return;
        }

        let mut attachments = self.hub.attachments();

        let Some(attachment) = attachments
            .get_mut(&self.thread_id)
            .filter(|attachment| attachment.connection == self.connection)
        else {
            return;
        };

        if attachment.pending.remove(&self.call_id).is_none() {
            return;
        }

        tracing::debug!(
            event.name = "relay.call.cancelled",
            thread.id = self.thread_id,
            relay.call_id = self.call_id,
            "a relayed call ended unanswered, telling the client",
        );

        let frame = SatelliteRelayFrame {
            frame: Some(satellite_relay_frame::Frame::Cancelled(ToolCallCancelled {
                call_id: std::mem::take(&mut self.call_id),
            })),
        };
        drop(attachment.outbound.send(Outbound::Frame(frame)));
    }
}

/// The wall-clock instant [`CALL_DEADLINE`] from now, for the client to read.
///
/// The satellite waits on a monotonic clock; this is only what it tells the
/// client, so a clock step on either machine moves nothing that matters.
fn deadline_timestamp() -> Timestamp {
    let now = Timestamp::now();

    Timestamp {
        epoch_seconds: now.epoch_seconds.saturating_add(
            i64::try_from(CALL_DEADLINE.as_secs()).expect("fifteen minutes fits in an i64"),
        ),
        ..now
    }
}
