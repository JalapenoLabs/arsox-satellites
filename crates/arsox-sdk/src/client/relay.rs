// Copyright © 2026 Jalapeno Labs

//! Answering the tool calls a thread's agents make to relayed MCP servers.
//!
//! A thread's `relayed_mcp_servers` are tools the host application answers
//! itself. The satellite serves them to the agents and forwards each call down a
//! WebSocket the host application opened, which is what lets a host that cannot
//! be reached from the satellite offer tools at all.
//!
//! [`ThreadHandle::relay`](super::ThreadHandle::relay) opens that socket. The
//! [`Relay`] it returns yields calls, and a [`RelayAnswerer`] cloned from it sends
//! results back. The two are separate so a call can be answered from whatever
//! task handles it, while one loop keeps reading:
//!
//! ```no_run
//! # async fn run(thread: arsox_sdk::client::ThreadHandle) -> arsox_sdk::client::Result<()> {
//! use arsox_sdk::client::RelayEvent;
//! use arsox_sdk::proto::relay::v1::{ToolContent, ToolResult, tool_content};
//!
//! let mut relay = thread.relay().await?;
//! let answerer = relay.answerer();
//!
//! while let Some(event) = relay.next().await {
//!     match event? {
//!         RelayEvent::Call(call) => {
//!             // Answered inline here. A clone of `answerer` moves into a
//!             // spawned task just as well, so a slow call holds up nothing.
//!             let result = ToolResult {
//!                 call_id: call.call_id,
//!                 content: vec![ToolContent {
//!                     kind: Some(tool_content::Kind::Text("done".to_owned())),
//!                 }],
//!                 is_error: false,
//!             };
//!             answerer.answer(result).await?;
//!         }
//!         RelayEvent::Cancelled(call_id) => { /* stop work on `call_id` */ }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # One client per thread
//!
//! A thread's relay has one attached client at a time. Opening a second replaces
//! the first, whose [`Relay::next`] then ends with an error whose code is
//! `RELAY_CLIENT_REPLACED`. Calls still in flight on the replaced socket fail as
//! tool errors to the agent, and an answer sent for one afterwards is ignored.
//!
//! # Nothing is replayed
//!
//! A call made while no client is attached fails at once, and a client that
//! attaches later never sees it. A tool call only means something while an agent
//! is waiting on it.

use super::{Error, Result};
use crate::proto::error::v1::{Error as ContractError, ErrorCode};
use crate::proto::relay::v1::{
    ClientRelayFrame, SatelliteRelayFrame, ToolCall, ToolResult, client_relay_frame,
    satellite_relay_frame,
};
use futures_util::lock::Mutex;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt as _, StreamExt as _};
use prost::Message as _;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// The socket a relay rides on.
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// The prefix protobuf puts on every `ErrorCode` name, which close reasons omit.
const ERROR_CODE_PREFIX: &str = "ERROR_CODE_";

/// Something the satellite sent down the relay.
#[derive(Debug, Clone, PartialEq)]
pub enum RelayEvent {
    /// An agent called a tool. Answer it with [`RelayAnswerer::answer`].
    Call(ToolCall),

    /// The satellite stopped waiting for the call with this id: its deadline
    /// passed, its turn ended, or its agent hung up. An answer sent for it now
    /// is ignored.
    Cancelled(String),
}

/// A thread's tool relay, yielding the calls its agents make.
///
/// Dropping it closes the socket. Calls still in flight then fail as tool errors
/// to the agent.
#[derive(Debug)]
pub struct Relay {
    incoming: SplitStream<Socket>,
    answerer: RelayAnswerer,
}

/// Sends results up a relay, from any task.
///
/// Cheap to clone: clones share the one socket, and results sent from several
/// tasks at once are written one frame at a time.
#[derive(Debug, Clone)]
pub struct RelayAnswerer {
    outgoing: Arc<Mutex<SplitSink<Socket, Message>>>,
}

// A relay is read on one task and answered from many, so both halves must cross
// task boundaries. Asserted here so a change that loses `Send` fails to compile
// in this crate rather than in a consumer's spawn.
const fn assert_send<Type: Send>() {}
const _: () = assert_send::<Relay>();
const _: () = assert_send::<RelayAnswerer>();

impl Relay {
    pub(super) fn new(socket: Socket) -> Self {
        let (outgoing, incoming) = socket.split();

        Self {
            incoming,
            answerer: RelayAnswerer {
                outgoing: Arc::new(Mutex::new(outgoing)),
            },
        }
    }

    /// A handle that answers calls on this relay.
    #[must_use]
    pub fn answerer(&self) -> RelayAnswerer {
        self.answerer.clone()
    }

    /// Waits for the next thing the satellite sends.
    ///
    /// Returns `None` once the socket has closed cleanly. Keep calling it: ping
    /// and pong frames are answered while it waits, and a relay nobody reads
    /// stops answering them.
    ///
    /// # Errors
    ///
    /// Yields an error, and then `None`, when the socket closes with a reason.
    /// A relay replaced by another client carries `RELAY_CLIENT_REPLACED` in
    /// [`Error::code`]. Also yields an error for a frame that does not decode,
    /// and for a socket that fails underneath.
    pub async fn next(&mut self) -> Option<Result<RelayEvent>> {
        loop {
            let frame = match self.incoming.next().await? {
                Ok(frame) => frame,
                Err(error) => return Some(Err(Error::transport(error.to_string()))),
            };

            match frame {
                Message::Binary(bytes) => return Some(event_from(bytes.as_ref())),
                Message::Close(frame) => {
                    let reason = frame
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty());

                    return reason.map(|reason| Err(closed_with(&reason)));
                }
                // Ping, pong, and text frames are not part of the contract.
                _other => {}
            }
        }
    }
}

impl RelayAnswerer {
    /// Sends one call's result to the satellite.
    ///
    /// Answering a call the satellite has stopped waiting for is not an error:
    /// the satellite ignores it, because a result racing a cancellation is an
    /// ordinary outcome rather than a mistake.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket is closed or fails while sending.
    pub async fn answer(&self, result: ToolResult) -> Result<()> {
        let frame = ClientRelayFrame {
            frame: Some(client_relay_frame::Frame::Result(result)),
        };

        self.outgoing
            .lock()
            .await
            .send(Message::Binary(frame.encode_to_vec()))
            .await
            .map_err(|error| Error::transport(error.to_string()))
    }
}

/// Decodes one frame the satellite sent.
fn event_from(bytes: &[u8]) -> Result<RelayEvent> {
    let decoded = SatelliteRelayFrame::decode(bytes)
        .map_err(|error| Error::transport(format!("undecodable relay frame: {error}")))?;

    match decoded.frame {
        Some(satellite_relay_frame::Frame::Call(call)) => Ok(RelayEvent::Call(call)),
        Some(satellite_relay_frame::Frame::Cancelled(cancelled)) => {
            Ok(RelayEvent::Cancelled(cancelled.call_id))
        }
        // A frame arm added in a newer contract decodes to nothing at all, and
        // saying so beats yielding an event this build cannot describe.
        None => Err(Error::transport(
            "the satellite sent a relay frame this SDK does not know",
        )),
    }
}

/// The error a close reason names.
///
/// A reason naming a contract code becomes a contract error carrying it, so a
/// caller matches on [`Error::code`] rather than on text.
fn closed_with(reason: &str) -> Error {
    let named = ErrorCode::from_str_name(&format!("{ERROR_CODE_PREFIX}{reason}"));

    let Some(code) = named else {
        return Error::transport(format!("relay closed: {reason}"));
    };

    Error::contract(ContractError {
        code: code.into(),
        message: format!("relay closed: {reason}"),
        retryable: false,
        details: None,
        trace_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::relay::v1::ToolCallCancelled;

    #[test]
    fn a_call_frame_becomes_a_call_event() {
        let call = ToolCall {
            call_id: "call-1".to_owned(),
            server: "storage".to_owned(),
            tool: "upload".to_owned(),
            ..ToolCall::default()
        };
        let frame = SatelliteRelayFrame {
            frame: Some(satellite_relay_frame::Frame::Call(call.clone())),
        };

        assert_eq!(
            event_from(&frame.encode_to_vec()).expect("decodes"),
            RelayEvent::Call(call)
        );
    }

    #[test]
    fn a_cancellation_frame_names_its_call() {
        let frame = SatelliteRelayFrame {
            frame: Some(satellite_relay_frame::Frame::Cancelled(ToolCallCancelled {
                call_id: "call-2".to_owned(),
            })),
        };

        assert_eq!(
            event_from(&frame.encode_to_vec()).expect("decodes"),
            RelayEvent::Cancelled("call-2".to_owned())
        );
    }

    #[test]
    fn a_replacement_reads_as_its_contract_code() {
        // A caller deciding whether to reconnect matches on the code, never on
        // the words around it.
        assert_eq!(
            closed_with("RELAY_CLIENT_REPLACED").code(),
            Some(ErrorCode::RelayClientReplaced)
        );
        assert_eq!(closed_with("SOMETHING_NEW").code(), None);
    }
}
