// Copyright © 2026 Jalapeno Labs

//! The LLM proxy every model request traverses.
//!
//! # Why a proxy at all
//!
//! Two things need a chokepoint between an agent and its model, and neither can
//! be had by asking the agent nicely.
//!
//! **Credentials.** A provider key in the agent's environment is a key the agent
//! can read, print into a log, or commit. The proxy holds it instead and
//! attaches it on the way out, so the agent holds a token that is worth nothing
//! anywhere else and expires when its turn ends.
//!
//! **Ceilings.** A budget written into a prompt is a suggestion, and an agent
//! under pressure routes around a suggestion. A budget enforced at the socket
//! the completions travel over is arithmetic. Every response is read for the
//! usage the provider reported, the total is added to the turn's [`Meter`], and
//! once the ceiling is reached the next request is refused here rather than
//! discouraged upstream. See [`budget`].
//!
//! # The grant
//!
//! One token per turn, minted when the turn starts and revoked when it ends. It
//! identifies the turn rather than authenticating a user, which is what lets the
//! proxy attribute tokens and cost to the right thread without the agent being
//! trusted to say who it is.
//!
//! It is carried twice: in the URL path, which is what routes the request, and
//! as the API key, which is what the CLI thinks it is sending. Both must match.
//! One carrier would do for routing; requiring both means a process that
//! guessed the URL still needs the token, and the CLI has a credential-shaped
//! thing to send so it does not refuse to start.
//!
//! # Every request is bounded
//!
//! A completion has no natural end either. An endpoint that accepted a request
//! and then stopped answering leaves the harness blocked on a socket, which from
//! the satellite's side is indistinguishable from a model thinking hard. So a
//! request past [`Grant::request_timeout`] is abandoned and the harness is
//! answered with an error shaped like the provider's own, which is what its
//! retry and failover policy is written against.
//!
//! **The bound covers the whole relay, not just the wait for headers.** A
//! response still streaming past it is cut off mid-body. That is the honest
//! reading of "one LLM request": a provider that sends a token a minute for an
//! hour has failed in a way that a bound on the handshake alone would never
//! catch. The alternative, an idle bound between chunks, would let a slow drip
//! run forever while never technically being idle.

use crate::proxy::budget::{Meter, UsageReader};
use crate::proxy::upstream::Upstream;
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures_util::{Stream, StreamExt as _, TryStreamExt as _};
use std::collections::HashMap;
use std::future::Future as _;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::mpsc::UnboundedSender;

pub mod budget;
pub mod upstream;

/// Headers that describe one hop and must not be forwarded to the next.
///
/// `host` is regenerated for the upstream. The rest are connection-scoped by
/// [RFC 9110], and passing them along can wedge the upstream connection.
///
/// [RFC 9110]: https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
const HOP_BY_HOP: [&str; 8] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Headers carrying the caller's own credential, which the proxy replaces.
///
/// Stripped rather than overwritten so a request cannot arrive with two and
/// have the upstream pick the one the agent supplied.
const CREDENTIAL_HEADERS: [&str; 3] = ["authorization", "x-api-key", "api-key"];

/// The largest completion request the proxy will forward.
///
/// A million-token conversation serialized as JSON is comfortably inside this.
/// The bound exists so a runaway agent cannot make the satellite buffer without
/// limit, not to constrain any request a harness legitimately makes.
const MAX_REQUEST_BODY: usize = 128 * 1024 * 1024;

/// What one turn is allowed to do with the proxy.
#[derive(Debug, Clone)]
pub struct Grant {
    thread_id: String,
    turn_id: String,
    upstream: Upstream,

    /// What this turn has spent and what it may spend.
    ///
    /// Shared with the runner rather than owned here: the proxy counts, and the
    /// runner is the only thing that knows how to end a turn.
    meter: Arc<Meter>,

    /// How long one of this turn's requests may take before it is abandoned.
    request_timeout: Duration,

    /// Where an incident this turn produced is sent to be recorded.
    ///
    /// The proxy holds no database, deliberately. It sees requests and not the
    /// turn they belong to the end of, so what it finds travels to the runner,
    /// which owns the turn's lifetime and is the one thing that records against
    /// it. A sender whose receiver has been dropped is the "nobody is running a
    /// turn behind this" case, and dropping the report is the right answer to
    /// it.
    incidents: UnboundedSender<Incident>,
}

impl Grant {
    /// Admits one turn to the proxy, with the documented request bound and no
    /// incident reporting.
    #[must_use]
    pub fn new(thread_id: &str, turn_id: &str, upstream: Upstream, meter: Arc<Meter>) -> Self {
        let (nowhere, _nobody_listening) = tokio::sync::mpsc::unbounded_channel();

        Self {
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.to_owned(),
            upstream,
            meter,
            request_timeout: crate::timeouts::DEFAULT_LLM_REQUEST,
            incidents: nowhere,
        }
    }

    /// Bounds each of this turn's requests by `request_timeout`.
    #[must_use]
    pub fn bounded(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Sends what the proxy finds to `incidents`, which the runner records.
    #[must_use]
    pub fn reporting_to(mut self, incidents: UnboundedSender<Incident>) -> Self {
        self.incidents = incidents;
        self
    }

    /// Reports a request that ran past this turn's bound.
    ///
    /// **Degraded rather than fatal.** The harness is answered with an error it
    /// can read, so it may retry the request or fail over to the next endpoint
    /// and the turn goes on. That is exactly why the incident matters: a timeout
    /// that was quietly retried and then succeeded looks identical to success,
    /// and a tax paid on every turn forever is invisible until somebody
    /// reconciles a bill against it.
    fn report_timeout(&self, during: &'static str) {
        tracing::warn!(
            event.name = "proxy.request.timed_out",
            thread.id = self.thread_id,
            turn.id = self.turn_id,
            llm.bound_seconds = self.request_timeout.as_secs(),
            llm.phase = during,
            "abandoning a model request that ran past its {{llm.bound_seconds}} second \
             bound {{llm.phase}}",
        );

        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            // Assigned by the log if this incident reaches the stream. The proxy
            // does not append events, so it cannot know one.
            sequence: None,
            thread_id: Some(self.thread_id.clone()),
            turn_id: Some(self.turn_id.clone()),
            member_id: None,
            code: ErrorCode::LlmEndpointTimeout.into(),
            disposition: Disposition::Degraded.into(),
            // The next attempt meets a different socket, and an endpoint that
            // stopped answering frequently starts again.
            retryable: true,
            message: format!(
                "a model request exceeded the thread's {:?} bound {during} and was abandoned",
                self.request_timeout
            ),
            details: None,
            occurred_at: Some(Timestamp::now()),
        };

        let _unheard = self.incidents.send(incident);
    }
}

/// Routes an agent's model requests, holding the credential it must not.
#[derive(Debug, Clone)]
pub struct LlmProxy {
    grants: Arc<RwLock<HashMap<String, Grant>>>,
    client: reqwest::Client,

    /// Where the harness is pointed. Loopback only: this listener has no
    /// authentication beyond the per-turn token, and it holds a real provider
    /// credential, so it must never be reachable off the container.
    address: SocketAddr,
}

impl LlmProxy {
    /// Binds the proxy to loopback and starts serving.
    ///
    /// # Errors
    ///
    /// Returns an error when the loopback listener cannot be bound.
    pub async fn start() -> anyhow::Result<Self> {
        // Port zero: the operator never configures this and nothing outside the
        // container connects to it, so a fixed port would only be a collision
        // waiting to happen on a host running several satellites.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;

        let proxy = Self {
            grants: Arc::new(RwLock::new(HashMap::new())),
            client: reqwest::Client::builder()
                // A backstop far outside any bound a thread would set. The real
                // per-request bound is the thread's and is applied per grant,
                // because this client is shared by every turn on the satellite
                // and one of them must not decide the limit for the rest.
                .timeout(Duration::from_hours(1))
                .build()?,
            address,
        };

        let router = Router::new()
            .route("/t/{token}/{*path}", any(forward))
            .with_state(proxy.clone());

        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                tracing::error!(
                    event.name = "satellite.proxy.stopped",
                    "the llm proxy stopped serving: {error}",
                );
            }
        });

        tracing::info!(
            event.name = "satellite.boot.proxy_ready",
            server.address = %address,
            "llm proxy listening",
        );

        Ok(proxy)
    }

    /// The base URL a harness should be pointed at for one turn.
    #[must_use]
    pub fn base_url_for(&self, token: &str) -> String {
        format!("http://{}/t/{token}", self.address)
    }

    /// Issues a turn its token.
    ///
    /// The token is a UUID rather than anything derived from the turn, because
    /// a token an agent can predict is a token it can mint for a turn that is
    /// not its own.
    pub async fn grant(&self, grant: Grant) -> String {
        let token = uuid::Uuid::now_v7().to_string();

        self.grants.write().await.insert(token.clone(), grant);

        token
    }

    /// Withdraws a token the moment its turn is over.
    ///
    /// A grant that outlived its turn would let an agent that leaked its token,
    /// or a process that kept a handle to it, keep spending after the work
    /// stopped.
    pub async fn revoke(&self, token: &str) {
        self.grants.write().await.remove(token);
    }

    async fn grant_for(&self, token: &str) -> Option<Grant> {
        self.grants.read().await.get(token).cloned()
    }
}

/// Forwards one request upstream with the real credential attached.
async fn forward(
    State(proxy): State<LlmProxy>,
    Path((token, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(grant) = proxy.grant_for(&token).await else {
        // Deliberately indistinguishable from a wrong token: an unknown token
        // and a revoked one are the same answer, and saying which would tell a
        // caller whether it had guessed a real turn.
        return provider_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "this request carries no valid Arsox turn token",
        );
    };

    let (parts, body) = request.into_parts();

    if !presented_token_matches(&parts.headers, &token) {
        return provider_error(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "the presented key does not match the turn this request was routed to",
        );
    }

    if let Some(ceiling) = grant.meter.reached() {
        tracing::info!(
            event.name = "proxy.budget.refused",
            thread.id = grant.thread_id,
            turn.id = grant.turn_id,
            budget.ceiling = ceiling.as_str_name(),
            budget.tokens_spent = grant.meter.tokens_spent(),
            "refusing a completion: this turn has reached {{budget.ceiling}}",
        );

        // A 403 rather than a 429. Both would stop the request, and only one of
        // them tells the CLI to try again in a moment against a wall that will
        // not move before the turn ends.
        return provider_error(
            StatusCode::FORBIDDEN,
            "permission_error",
            "this turn has reached the token ceiling its thread was created with",
        );
    }

    let url = grant.upstream.url_for(&path, parts.uri.query());

    // The request body is collected, the response body is not. A completion
    // request is one bounded JSON document, and buffering it costs a copy while
    // avoiding chunked-encoding differences between providers. The response is
    // the half that streams, and that one is relayed as it arrives.
    let body = match axum::body::to_bytes(body, MAX_REQUEST_BODY).await {
        Ok(collected) => collected,
        Err(_too_large) => {
            return provider_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid_request_error",
                "the request body exceeded what the Arsox proxy will forward",
            );
        }
    };

    let mut outbound = proxy.client.request(parts.method.clone(), &url).body(body);

    for (name, value) in &parts.headers {
        if is_hop_by_hop(name) || is_credential(name) {
            continue;
        }
        outbound = outbound.header(name, value);
    }

    for (name, value) in grant.upstream.credential_headers() {
        outbound = outbound.header(name, value);
    }

    // One deadline for the whole relay, taken before the request goes out. The
    // streaming half inherits it rather than starting a second clock, so a
    // response that arrives at the last moment and then dribbles cannot spend
    // the bound twice.
    let deadline = tokio::time::Instant::now() + grant.request_timeout;

    match tokio::time::timeout_at(deadline, outbound.send()).await {
        Ok(Ok(response)) => relay(response, grant, deadline),

        Ok(Err(error)) => {
            tracing::warn!(
                event.name = "proxy.upstream.failed",
                thread.id = grant.thread_id,
                turn.id = grant.turn_id,
                "the upstream endpoint could not be reached: {error}",
            );

            provider_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream model endpoint could not be reached",
            )
        }

        Err(_elapsed) => {
            grant.report_timeout("while waiting for a response");

            // A gateway timeout rather than the satellite's own contract error,
            // for the same reason every other refusal here wears the provider's
            // shape: the harness speaks one provider's error format and nothing
            // else. A 5xx is also what its retry and failover policy is written
            // against, which is what the contract says a timeout should trigger.
            provider_error(
                StatusCode::GATEWAY_TIMEOUT,
                "api_error",
                "the upstream model endpoint did not answer inside this thread's request timeout",
            )
        }
    }
}

/// Streams the upstream's response back without buffering it, counting as it
/// goes.
///
/// Completions arrive as server-sent events. Collecting one before returning it
/// would turn a streaming API into a blocking one and defeat every event the
/// harness emits as it goes, so the usage the provider reports is read out of
/// the bytes on their way past instead.
fn relay(response: reqwest::Response, grant: Grant, deadline: tokio::time::Instant) -> Response {
    let mut builder = Response::builder().status(response.status());

    for (name, value) in response.headers() {
        if is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let reader = UsageReader::for_content_type(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
    );

    let counted = Metered {
        // The error type crosses to `io::Error` here rather than staying
        // `reqwest`'s, because the body has a second way to fail that reqwest
        // has no word for: the bound below expiring mid-stream.
        inner: Box::pin(response.bytes_stream().map_err(std::io::Error::other)),
        reader,
        grant,
        deadline: Box::pin(tokio::time::sleep_until(deadline)),
    };

    builder
        .body(Body::from_stream(counted))
        .unwrap_or_else(|_error| {
            provider_error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                "the upstream response could not be relayed",
            )
        })
}

/// A response body that counts the usage it carries on its way past, and ends
/// once the request's bound elapses.
///
/// The total is committed on drop rather than when the stream ends. A harness
/// that hangs up mid-response still spent the tokens the provider generated, and
/// a turn whose accounting is discarded because its last request was abandoned
/// is a ceiling with a hole in it. A response cut off at the bound is charged
/// for the same reason: the provider generated those tokens whether or not they
/// arrived.
struct Metered {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>,
    reader: UsageReader,
    grant: Grant,

    /// Fires when the request's bound elapses.
    ///
    /// A timer rather than a comparison against the clock on each chunk: an
    /// upstream that stops sending stops waking this stream too, so a check that
    /// only ran when a chunk arrived would never run at all for exactly the
    /// failure it exists to catch.
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl Stream for Metered {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Every field is `Unpin`, so the pin places no restriction on taking a
        // mutable reference and no projection machinery is needed.
        let this = self.get_mut();

        if this.deadline.as_mut().poll(cx).is_ready() {
            this.grant
                .report_timeout("while the response was streaming");

            // An error rather than a clean end. A truncated server-sent event
            // stream that ended tidily would reach the harness as a completion
            // that simply stopped, which is the silent failure this whole
            // system exists to prevent.
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the model response exceeded this thread's request timeout",
            ))));
        }

        let polled = this.inner.poll_next_unpin(cx);

        if let Poll::Ready(Some(Ok(chunk))) = &polled {
            this.reader.push(chunk);
        }

        polled
    }
}

impl Drop for Metered {
    fn drop(&mut self) {
        self.grant.meter.record_tokens(self.reader.take_total());
    }
}

/// Whether the API key the caller presented is the token it was routed with.
fn presented_token_matches(headers: &HeaderMap, token: &str) -> bool {
    let presented = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
        });

    // Constant time, for the same reason the satellite's own bearer check is:
    // a comparison that returns early leaks the token one character at a time.
    presented.is_some_and(|presented| {
        use subtle::ConstantTimeEq as _;
        presented.as_bytes().ct_eq(token.as_bytes()).into()
    })
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

fn is_credential(name: &HeaderName) -> bool {
    CREDENTIAL_HEADERS.contains(&name.as_str())
}

/// An error shaped like the provider's own, so the CLI reports it usefully.
///
/// The harness on the other side of this speaks one provider's error format and
/// nothing else. Returning the satellite's own contract error here would reach
/// an agent that cannot read it, and surface to the operator as an unexplained
/// CLI failure.
fn provider_error(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message },
    });

    (status, axum::Json(body)).into_response()
}
