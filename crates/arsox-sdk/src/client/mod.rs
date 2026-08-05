// Copyright © 2026 Jalapeno Labs

//! Talking to a satellite.
//!
//! Protobuf is always on the wire, in both directions, and this module converts
//! it into ordinary Rust structs. You never hold a protobuf type by accident,
//! though the generated types are right there under [`crate::proto`] when you
//! want them.
//!
//! # A thread lives on the satellite
//!
//! A [`ThreadHandle`] holds an id and a connection, nothing else. Any process
//! with the URL, the secret, and the thread id can [`Threads::attach`] to a
//! running thread and do everything the process that created it could: read the
//! stream from any sequence, queue turns, and destroy it. There is no handoff
//! and no ownership, which is what lets a horizontally scaled application
//! survive a replica dying mid-turn.

mod error;

pub use error::{Error, Result};

use crate::proto::common::v1::PageRequest;
use crate::proto::error::v1::Error as ContractError;
use crate::proto::event::v1::ThreadEvent;
use crate::proto::harness::v1::GetHarnessResponse;
use crate::proto::satellite::v1::{GetStatusResponse, GetVersionResponse};
use crate::proto::settings::v1::ThreadSettings;
use crate::proto::thread::v1::{
    CreateThreadRequest, CreateThreadResponse, DestroyThreadResponse, DrainThreadResponse,
    GetThreadResponse, ListThreadsRequest, ListThreadsResponse, PauseThreadResponse,
    ResumeThreadResponse, Thread, ThreadOrder, ThreadSummary,
};
use crate::proto::turn::v1::{
    CancelTurnResponse, GetTurnResponse, ListTurnsRequest, ListTurnsResponse, StartTurnRequest,
    StartTurnResponse, Turn, TurnResult, TurnStatus,
};
use futures_util::{Stream, StreamExt as _};
use prost::Message as _;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The proto major this SDK speaks.
///
/// An SDK refuses a satellite serving a higher major rather than failing later
/// with a confusing decode error, and warns once on a higher minor before
/// proceeding.
const SDK_PROTO_MAJOR: u32 = 1;

/// What the SDK sends and asks for.
const PROTOBUF: &str = "application/protobuf";

/// How often a pending turn is re-read while waiting for it to finish.
///
/// Polling rather than watching the stream, because a caller awaiting a result
/// has not necessarily subscribed and should not have to.
const RESULT_POLL: Duration = Duration::from_millis(500);

#[derive(Debug)]
struct Inner {
    base: String,
    secret: String,
    http: reqwest::Client,
}

/// A connection to one satellite.
///
/// Cheap to clone: clones share one connection pool.
#[derive(Debug, Clone)]
pub struct Satellite {
    inner: Arc<Inner>,
}

impl Satellite {
    /// Connects, and refuses a satellite this SDK cannot speak to.
    ///
    /// The version check happens here rather than lazily so a mismatch is
    /// reported at the point a human can act on it, instead of surfacing as a
    /// decode failure three calls later.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable or serves a proto
    /// major above this SDK's.
    pub async fn connect(url: impl Into<String>, secret: impl Into<String>) -> Result<Self> {
        let satellite = Self {
            inner: Arc::new(Inner {
                base: url.into().trim_end_matches('/').to_owned(),
                secret: secret.into(),
                http: reqwest::Client::new(),
            }),
        };

        let version = satellite.version().await?;

        if version.proto_major > SDK_PROTO_MAJOR {
            return Err(Error::incompatible(version.proto_major, SDK_PROTO_MAJOR));
        }
        if version.proto_major == SDK_PROTO_MAJOR && version.proto_minor > 0 {
            // Additive fields this SDK does not know about are ignored, which is
            // safe. Saying so once beats saying nothing.
            tracing_warn(&format!(
                "satellite serves proto v{}.{} and this SDK was built against v{SDK_PROTO_MAJOR}.0; \
                 unknown fields will be ignored",
                version.proto_major, version.proto_minor
            ));
        }

        Ok(satellite)
    }

    /// Reports the satellite version and the proto contract it serves.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable.
    pub async fn version(&self) -> Result<GetVersionResponse> {
        self.get("/v1/version").await
    }

    /// Reports what the satellite is currently doing.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable or rejects the secret.
    pub async fn status(&self) -> Result<GetStatusResponse> {
        self.get("/v1/status").await
    }

    /// Reports which harnesses this satellite offers and what each supports.
    ///
    /// Worth calling before relying on a capability. Discovering that a harness
    /// has no plan mode by its absence, three turns into a run, is the failure
    /// this endpoint exists to prevent.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable or rejects the secret.
    pub async fn harness(&self) -> Result<GetHarnessResponse> {
        self.get("/v1/harness").await
    }

    /// Threads on this satellite.
    #[must_use]
    pub fn threads(&self) -> Threads {
        Threads {
            satellite: self.clone(),
        }
    }

    async fn get<M: prost::Message + Default>(&self, path: &str) -> Result<M> {
        let response = self
            .inner
            .http
            .get(format!("{}{path}", self.inner.base))
            .header("Authorization", format!("Bearer {}", self.inner.secret))
            .header("Accept", PROTOBUF)
            .send()
            .await
            .map_err(|error| Error::transport(error.to_string()))?;

        decode(response).await
    }

    async fn send<M: prost::Message + Default>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &impl prost::Message,
    ) -> Result<M> {
        let response = self
            .inner
            .http
            .request(method, format!("{}{path}", self.inner.base))
            .header("Authorization", format!("Bearer {}", self.inner.secret))
            .header("Content-Type", PROTOBUF)
            .header("Accept", PROTOBUF)
            .body(body.encode_to_vec())
            .send()
            .await
            .map_err(|error| Error::transport(error.to_string()))?;

        decode(response).await
    }
}

/// Decodes a response, turning a contract error into an [`Error`].
async fn decode<M: prost::Message + Default>(response: reqwest::Response) -> Result<M> {
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| Error::transport(error.to_string()))?;

    if status.is_success() {
        return M::decode(body).map_err(|error| {
            Error::transport(format!(
                "the satellite sent an undecodable response: {error}"
            ))
        });
    }

    // Every failure on every transport is the same shape, so an error body that
    // will not decode means something other than a satellite answered.
    ContractError::decode(body)
        .map_or_else(
            |_undecodable| Error::transport(format!("the satellite answered {status}")),
            Error::contract,
        )
        .pipe_err()
}

/// Turns an error value into the `Err` arm, for readability at the call site.
trait PipeErr {
    fn pipe_err<T>(self) -> Result<T>;
}

impl PipeErr for Error {
    fn pipe_err<T>(self) -> Result<T> {
        Err(self)
    }
}

/// Emits a one-time compatibility warning.
///
/// Routed through a function so the SDK does not force a logging framework on
/// consumers that have their own.
fn tracing_warn(message: &str) {
    eprintln!("arsox: {message}");
}

/// Thread operations.
#[derive(Debug, Clone)]
pub struct Threads {
    satellite: Satellite,
}

/// A newly opened thread.
#[derive(Debug, Clone)]
pub struct ThreadCreated {
    pub thread: Thread,

    /// True when an idempotency key matched an existing thread, so this is that
    /// thread rather than a new one.
    pub deduplicated: bool,

    pub handle: ThreadHandle,
}

impl Threads {
    /// Opens a thread.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings are incomplete or the satellite is
    /// unreachable.
    pub async fn create(&self, settings: ThreadSettings) -> Result<ThreadCreated> {
        self.create_with(settings, None, BTreeMap::new()).await
    }

    /// Opens a thread with an idempotency key and correlation metadata.
    ///
    /// The key is what makes a timed-out create safe to retry. Without it, a
    /// response lost in transit is indistinguishable from a thread that was
    /// never created, and the only safe move is to retry and leak a workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings are incomplete or the satellite is
    /// unreachable.
    pub async fn create_with(
        &self,
        settings: ThreadSettings,
        idempotency_key: Option<String>,
        metadata: BTreeMap<String, String>,
    ) -> Result<ThreadCreated> {
        let response: CreateThreadResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                "/v1/threads",
                &CreateThreadRequest {
                    settings: Some(settings),
                    idempotency_key,
                    metadata: metadata.into_iter().collect(),
                },
            )
            .await?;

        let thread = response.thread.ok_or_else(|| {
            Error::transport("the satellite created a thread without returning it")
        })?;

        Ok(ThreadCreated {
            handle: ThreadHandle {
                satellite: self.satellite.clone(),
                thread_id: thread.thread_id.clone(),
            },
            deduplicated: response.deduplicated,
            thread,
        })
    }

    /// Picks up a thread that already exists.
    ///
    /// There is no handoff and no lease. The process that created the thread has
    /// no privileged claim on it and may have exited hours ago.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn attach(&self, thread_id: impl Into<String>) -> Result<ThreadHandle> {
        let thread_id = thread_id.into();

        // Proves the thread exists, so attaching to a typo fails here rather
        // than at the first operation on the handle.
        let handle = ThreadHandle {
            satellite: self.satellite.clone(),
            thread_id,
        };
        handle.get().await?;

        Ok(handle)
    }

    /// Lists threads, optionally filtered by metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable.
    pub async fn list(&self, metadata: BTreeMap<String, String>) -> Result<Vec<ThreadSummary>> {
        self.list_ordered(metadata, ThreadOrder::Unspecified, false)
            .await
    }

    /// Lists threads in a chosen order.
    ///
    /// `ThreadOrder::LastActivity` with `descending` is the operator's view: the
    /// threads that did something most recently, first. Creation order is the
    /// default because it is free, since thread ids already sort by time.
    ///
    /// # Errors
    ///
    /// Returns an error when the satellite is unreachable.
    pub async fn list_ordered(
        &self,
        metadata: BTreeMap<String, String>,
        order: ThreadOrder,
        descending: bool,
    ) -> Result<Vec<ThreadSummary>> {
        let response: ListThreadsResponse = self
            .satellite
            .send(
                reqwest::Method::GET,
                "/v1/threads",
                &ListThreadsRequest {
                    states: Vec::new(),
                    metadata: metadata.into_iter().collect(),
                    page: Some(PageRequest::default()),
                    order_by: order.into(),
                    descending,
                },
            )
            .await?;

        Ok(response.threads)
    }
}

/// A handle to one thread.
///
/// Holds an id and a connection. All the state lives on the satellite, which is
/// what makes a handle disposable and a thread durable.
#[derive(Debug, Clone)]
pub struct ThreadHandle {
    satellite: Satellite,
    thread_id: String,
}

impl ThreadHandle {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.thread_id
    }

    /// Reads the thread's current state.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn get(&self) -> Result<Thread> {
        let response: GetThreadResponse = self
            .satellite
            .get(&format!("/v1/threads/{}", self.thread_id))
            .await?;

        response
            .thread
            .ok_or_else(|| Error::transport("the satellite returned a thread with no thread in it"))
    }

    /// Queues a turn.
    ///
    /// Returns as soon as the turn is queued. Await [`TurnHandle::result`] for
    /// the outcome.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue is full, the thread is unknown, or the
    /// satellite is unreachable.
    pub async fn start_turn(&self, prompt: impl Into<String>) -> Result<TurnHandle> {
        self.start_turn_with(prompt, None, BTreeMap::new()).await
    }

    /// Queues a turn with an idempotency key and correlation metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the queue is full, the thread is unknown, or the
    /// satellite is unreachable.
    pub async fn start_turn_with(
        &self,
        prompt: impl Into<String>,
        idempotency_key: Option<String>,
        metadata: BTreeMap<String, String>,
    ) -> Result<TurnHandle> {
        let response: StartTurnResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                &format!("/v1/threads/{}/turns", self.thread_id),
                &StartTurnRequest {
                    thread_id: self.thread_id.clone(),
                    prompt: prompt.into(),
                    idempotency_key,
                    metadata: metadata.into_iter().collect(),
                },
            )
            .await?;

        let turn = response
            .turn
            .ok_or_else(|| Error::transport("the satellite queued a turn without returning it"))?;

        Ok(TurnHandle {
            satellite: self.satellite.clone(),
            thread_id: self.thread_id.clone(),
            turn_id: turn.turn_id.clone(),
            turn,
        })
    }

    /// Lists this thread's turns, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn turns(&self) -> Result<Vec<Turn>> {
        let response: ListTurnsResponse = self
            .satellite
            .send(
                reqwest::Method::GET,
                &format!("/v1/threads/{}/turns", self.thread_id),
                &ListTurnsRequest {
                    thread_id: self.thread_id.clone(),
                    statuses: Vec::new(),
                    page: Some(PageRequest::default()),
                    order_by: crate::proto::turn::v1::TurnOrder::Unspecified.into(),
                    descending: false,
                },
            )
            .await?;

        Ok(response.turns)
    }

    /// Destroys the thread and everything under it.
    ///
    /// Incidents survive on their own retention, because "why did last night go
    /// wrong" is asked after the workspace is gone.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn destroy(&self) -> Result<Thread> {
        let response: DestroyThreadResponse = self
            .satellite
            .send(
                reqwest::Method::DELETE,
                &format!("/v1/threads/{}", self.thread_id),
                &(),
            )
            .await?;

        response
            .thread
            .ok_or_else(|| Error::transport("the satellite destroyed a thread without saying so"))
    }

    /// Stops the thread claiming queued work, without losing anything.
    ///
    /// Turns may still be submitted and still queue; the queue simply does not
    /// move until the thread resumes. The state an operator reaches for when
    /// destroying the thread would lose the workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn pause(&self) -> Result<Thread> {
        let response: PauseThreadResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                &format!("/v1/threads/{}/pause", self.thread_id),
                &(),
            )
            .await?;

        response
            .thread
            .ok_or_else(|| Error::transport("the satellite paused a thread without saying so"))
    }

    /// Returns a paused thread to service.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn resume(&self) -> Result<Thread> {
        let response: ResumeThreadResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                &format!("/v1/threads/{}/resume", self.thread_id),
                &(),
            )
            .await?;

        response
            .thread
            .ok_or_else(|| Error::transport("the satellite resumed a thread without saying so"))
    }

    /// Cancels every queued turn, leaving any running turn alone.
    ///
    /// One call rather than a loop, because cancelling turns one at a time races
    /// the runner claiming them, and that is a race an operator should not have
    /// to win. Pause first if the intent is to stop the thread rather than clear
    /// a backlog.
    ///
    /// # Errors
    ///
    /// Returns an error when the thread is unknown or the satellite is
    /// unreachable.
    pub async fn drain(&self) -> Result<Vec<String>> {
        let response: DrainThreadResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                &format!("/v1/threads/{}/drain", self.thread_id),
                &(),
            )
            .await?;

        Ok(response.cancelled_turn_ids)
    }

    /// Streams this thread's events, from the beginning of retained history.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be opened.
    pub async fn events(&self) -> Result<EventStream> {
        self.events_from(0).await
    }

    /// Streams this thread's events, resuming after a sequence.
    ///
    /// `from_sequence` is exclusive: pass the last sequence actually handled and
    /// receive everything since. This is how a replica that died mid-turn picks
    /// up without losing an event.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket cannot be opened, including when the
    /// requested sequence is older than retained history.
    pub async fn events_from(&self, from_sequence: u64) -> Result<EventStream> {
        let base = self
            .satellite
            .inner
            .base
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);

        let url = format!(
            "{base}/v1/threads/{}/stream?from_sequence={from_sequence}",
            self.thread_id
        );

        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url)
                .map_err(|error| Error::transport(error.to_string()))?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", self.satellite.inner.secret)
                .parse()
                .map_err(|_invalid| Error::transport("the secret is not a valid header value"))?,
        );

        let (socket, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|error| Error::transport(error.to_string()))?;

        // Boxed and pinned so the stream a caller receives is Unpin and can be
        // polled with an ordinary .
        // Handing back an unpinned stream would make every consumer pin it, which
        // is an ergonomic tax the SDK exists to absorb.
        Ok(Box::pin(socket.filter_map(|frame| async move {
            match frame {
                Ok(tokio_tungstenite::tungstenite::Message::Binary(bytes)) => {
                    Some(ThreadEvent::decode(bytes.as_ref()).map_err(|error| {
                        Error::transport(format!("undecodable event frame: {error}"))
                    }))
                }
                // A close frame carries the contract code in its reason, so a
                // lagging consumer learns why rather than seeing the stream stop.
                Ok(tokio_tungstenite::tungstenite::Message::Close(frame)) => {
                    let reason = frame
                        .map(|frame| frame.reason.to_string())
                        .filter(|reason| !reason.is_empty());

                    reason.map(|reason| Err(Error::transport(format!("stream closed: {reason}"))))
                }
                // Ping, pong, and text frames are not part of the contract.
                Ok(_other) => None,
                Err(error) => Some(Err(Error::transport(error.to_string()))),
            }
        })))
    }
}

/// A thread's events, in sequence order.
///
/// Boxed so it is , which is what lets a caller poll it directly rather
/// than pinning it first.
pub type EventStream = std::pin::Pin<Box<dyn Stream<Item = Result<ThreadEvent>> + Send>>;

/// A handle to one turn.
#[derive(Debug, Clone)]
pub struct TurnHandle {
    satellite: Satellite,
    thread_id: String,
    turn_id: String,
    turn: Turn,
}

impl TurnHandle {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.turn_id
    }

    /// The turn as it was when queued.
    #[must_use]
    pub fn queued(&self) -> &Turn {
        &self.turn
    }

    /// Waits for the turn to reach a terminal state and returns its result.
    ///
    /// # Errors
    ///
    /// Returns an error when the turn is unknown or the satellite is
    /// unreachable.
    pub async fn result(&self) -> Result<TurnResult> {
        loop {
            let response: GetTurnResponse = self
                .satellite
                .get(&format!(
                    "/v1/threads/{}/turns/{}",
                    self.thread_id, self.turn_id
                ))
                .await?;

            let status = response
                .turn
                .as_ref()
                .and_then(|turn| TurnStatus::try_from(turn.status).ok())
                .unwrap_or(TurnStatus::Unspecified);

            if !matches!(status, TurnStatus::Queued | TurnStatus::Running) {
                return response.result.ok_or_else(|| {
                    Error::transport("the satellite finished a turn without recording a result")
                });
            }

            tokio::time::sleep(RESULT_POLL).await;
        }
    }

    /// Asks the satellite to stop this turn.
    ///
    /// A running turn is asked to stop cooperatively first. Work already
    /// committed to a branch survives either way.
    ///
    /// # Errors
    ///
    /// Returns an error when the turn is unknown or the satellite is
    /// unreachable.
    pub async fn cancel(&self) -> Result<Turn> {
        let response: CancelTurnResponse = self
            .satellite
            .send(
                reqwest::Method::POST,
                &format!(
                    "/v1/threads/{}/turns/{}/cancel",
                    self.thread_id, self.turn_id
                ),
                &(),
            )
            .await?;

        response
            .turn
            .ok_or_else(|| Error::transport("the satellite cancelled a turn without saying so"))
    }
}
