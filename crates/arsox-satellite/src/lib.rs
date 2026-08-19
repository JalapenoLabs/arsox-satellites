// Copyright © 2026 Jalapeno Labs

//! The Arsox satellite: an HTTP and WebSocket API that runs agent harnesses and
//! normalizes their output into one protobuf contract.
//!
//! This is the library half. It owns everything the satellite does; the binary
//! beside it installs an allocator, starts tracing, and calls [`serve`]. The
//! split exists so the interesting parts can be exercised from tests rather than
//! only from a running process.
//!
//! # Boot
//!
//! The satellite refuses to start without `ARSOX_SECRET`. There is no
//! unauthenticated mode by accident: an open satellite is a remote shell with
//! your credentials in it. Running without one is possible and must be asked for
//! explicitly with `ARSOX_ALLOW_INSECURE=true`, which warns loudly on every boot.

mod api;
pub mod collector;
pub mod disk;
pub mod harness;
pub mod proxy;
pub mod store;
pub mod stream;
pub mod workspace;

use anyhow::{Context as _, Result, bail};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::{Error as ContractError, ErrorCode};
use arsox_sdk::proto::harness::v1::GetHarnessResponse;
use arsox_sdk::proto::satellite::v1::{DiskUsage, GetStatusResponse, GetVersionResponse};
use arsox_sdk::proto::thread::v1::ThreadState;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use prost::Message as _;
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq as _;

/// The proto contract major version this satellite serves.
///
/// An SDK refuses to talk to a satellite advertising a higher major, rather than
/// failing later with a confusing decode error.
pub const PROTO_MAJOR: u32 = 1;

/// The proto contract minor version this satellite serves.
///
/// An SDK meeting a higher minor warns once and proceeds, ignoring additive
/// fields it does not know about.
pub const PROTO_MINOR: u32 = 0;

/// Address the API listens on inside the container.
///
/// Fixed rather than configurable: the container's port mapping is the place to
/// change where the satellite is reachable, and a second knob for the same thing
/// only creates a way for the two to disagree.
const LISTEN_ADDR: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 8080);

/// Concurrent threads allowed when `ARSOX_MAX_CONCURRENT_THREADS` is unset.
///
/// Team mode multiplies the real process count behind each thread, so this is
/// deliberately low. Raise it when the satellite has the CPU and memory.
const DEFAULT_MAX_CONCURRENT_THREADS: u32 = 4;

/// Where thread workspaces live when `ARSOX_WORKSPACE_ROOT` is unset.
///
/// Mount this as a named volume too. Nothing under it survives a container
/// replacement otherwise, which silently breaks thread resumption.
const DEFAULT_WORKSPACE_ROOT: &str = "/workspace";

/// Where the embedded database lives when `ARSOX_DB_PATH` is unset.
///
/// Inside the container this directory is expected to be a named volume.
const DEFAULT_DB_PATH: &str = "/var/arsox/arsox.db";

/// How often expired threads are collected when `ARSOX_COLLECT_INTERVAL` is unset.
///
/// A TTL is measured in minutes, so a minute of slack past an expiry costs
/// nothing. Sweeping much more often would mean an index scan per second to
/// find, almost always, nothing.
const DEFAULT_COLLECT_INTERVAL_SECONDS: u32 = 60;

/// The wire type the SDK always speaks. JSON is a debugging affordance offered
/// through `Accept`, never what an SDK sends.
const PROTOBUF_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/protobuf");

/// Threads `/v1/status` lists before it stops.
///
/// Status is an operational snapshot that gets polled, not a listing. A
/// satellite holding thousands of idle threads should not answer every poll with
/// thousands of summaries, and `GET /v1/threads` pages properly for the caller
/// that wants all of them.
const STATUS_THREAD_LIMIT: u32 = 500;

/// Every state a thread the satellite still holds can be in.
///
/// Tombstones are deliberately absent. An expired or destroyed thread holds no
/// workspace, no queue, and no events, so listing it among the threads the
/// satellite is holding would misreport the fleet to whatever is deciding
/// whether to start another satellite.
const LIVE_THREAD_STATES: [ThreadState; 6] = [
    ThreadState::Provisioning,
    ThreadState::Idle,
    ThreadState::Running,
    ThreadState::AwaitingInput,
    ThreadState::Watching,
    ThreadState::Paused,
];

/// How the satellite authenticates callers.
#[derive(Debug, Clone)]
enum Auth {
    /// Every request must carry this secret as a bearer token.
    Bearer(String),

    /// No authentication, requested deliberately with `ARSOX_ALLOW_INSECURE`.
    Insecure,
}

impl Auth {
    /// Whether a presented bearer token authenticates the caller.
    ///
    /// The comparison runs in constant time, so the secret cannot be recovered
    /// by timing repeated requests against it. A naive `==` returns as soon as
    /// two bytes differ, which leaks a prefix one byte at a time and turns a
    /// long random secret into a few thousand requests' work.
    ///
    /// An insecure satellite accepts anything, including an absent token. That
    /// is the whole meaning of `ARSOX_ALLOW_INSECURE`.
    fn authenticates(&self, presented: &str) -> bool {
        match self {
            Self::Insecure => true,
            // `ct_eq` on slices reports a length mismatch without branching on
            // content. The length itself is not secret material.
            Self::Bearer(expected) => bool::from(expected.as_bytes().ct_eq(presented.as_bytes())),
        }
    }
}

/// State shared by every request handler.
#[derive(Debug)]
pub(crate) struct Satellite {
    auth: Auth,
    max_concurrent_threads: u32,
    started_at: Timestamp,
    pub(crate) store: store::Store,

    /// Nudged when a turn is queued, so the runner picks it up immediately
    /// rather than waiting for its idle poll.
    pub(crate) work_queued: Arc<tokio::sync::Notify>,

    pub(crate) bus: stream::EventBus,

    /// Removes threads and the workspaces they own.
    pub(crate) collector: Arc<collector::Collector>,

    /// Fills a new thread's workspace before its first turn can claim it.
    pub(crate) provisioner: workspace::Provisioner,

    /// What the satellite is holding on disk, measured on a refresh interval
    /// rather than per request.
    disk: disk::Meter,

    /// What this satellite's harnesses support, resolved once at boot.
    harness: GetHarnessResponse,
}

impl Satellite {
    fn is_insecure(&self) -> bool {
        matches!(self.auth, Auth::Insecure)
    }
}

/// Decides the boot authentication mode.
///
/// Failing closed is deliberate. An accidentally unauthenticated satellite is a
/// remote shell with the caller's credentials in it, so an absent secret is a
/// refusal to start rather than a warning that scrolls past.
///
/// # Errors
///
/// Returns an error when no secret is set and insecure mode was not explicitly
/// requested, and when a secret is set but empty.
fn resolve_auth(secret: Option<String>, allow_insecure: bool) -> Result<Auth> {
    match secret {
        Some(value) if !value.trim().is_empty() => Ok(Auth::Bearer(value)),
        Some(_) => bail!("ARSOX_SECRET is set but empty, refusing to start"),
        None if allow_insecure => Ok(Auth::Insecure),
        None => bail!(
            "ARSOX_SECRET is not set, refusing to start. \
             Set it, or set ARSOX_ALLOW_INSECURE=true if this satellite really \
             should accept unauthenticated requests on a trusted network."
        ),
    }
}

/// Renders a protobuf message as a response body.
pub(crate) fn protobuf<M: prost::Message>(message: &M) -> Response {
    (
        [(header::CONTENT_TYPE, PROTOBUF_CONTENT_TYPE)],
        message.encode_to_vec(),
    )
        .into_response()
}

/// Renders a contract error, in the one shape every Arsox error uses.
///
/// Clients match on `code`, never on `message`: the wording may change within a
/// major version, the code may not.
pub(crate) fn contract_error(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    // Nothing routed through here is worth retrying unchanged: a malformed body
    // or a missing field will be just as malformed next time.
    contract_error_retryable(status, code, message, false)
}

/// Renders a contract error that a caller may reasonably retry.
///
/// Separate from [`contract_error`] because `retryable` is the field an SDK
/// falls back on when it meets a code it has never heard of, so guessing it is
/// worse than stating it.
pub(crate) fn contract_error_retryable(
    status: StatusCode,
    code: ErrorCode,
    message: &str,
    retryable: bool,
) -> Response {
    let body = ContractError {
        code: code.into(),
        message: message.to_owned(),
        retryable,
        details: None,
        trace_id: None,
    };

    (
        status,
        [(header::CONTENT_TYPE, PROTOBUF_CONTENT_TYPE)],
        body.encode_to_vec(),
    )
        .into_response()
}

/// Extracts a bearer token, distinguishing "absent" from "wrong scheme".
///
/// Two failures rather than one, because a caller sending Basic auth has a
/// different bug from a caller sending nothing, and telling them apart is the
/// entire reason the error list is as long as it is.
fn bearer_token(headers: &HeaderMap) -> Result<&str, ErrorCode> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(ErrorCode::AuthHeaderMissing);
    };

    let value = value
        .to_str()
        .map_err(|_ignored| ErrorCode::AuthSchemeUnsupported)?;

    value
        .strip_prefix("Bearer ")
        .ok_or(ErrorCode::AuthSchemeUnsupported)
}

/// Rejects any request that does not authenticate.
async fn require_auth(
    State(satellite): State<Arc<Satellite>>,
    request: Request,
    next: Next,
) -> Response {
    // An insecure satellite skips the header entirely: demanding a token it
    // would not check is theatre.
    if satellite.is_insecure() {
        return next.run(request).await;
    }

    let presented = match bearer_token(request.headers()) {
        Ok(token) => token,
        Err(code) => {
            return contract_error(
                StatusCode::UNAUTHORIZED,
                code,
                "this endpoint requires an Authorization: Bearer <ARSOX_SECRET> header",
            );
        }
    };

    if !satellite.auth.authenticates(presented) {
        return contract_error(
            StatusCode::UNAUTHORIZED,
            ErrorCode::AuthSecretInvalid,
            "the bearer token does not match ARSOX_SECRET",
        );
    }

    next.run(request).await
}

/// Liveness. Always cheap, and never touches the database.
///
/// Unauthenticated on purpose: an orchestrator needs to know whether a process
/// is alive before it holds a credential, and this endpoint exposes no thread
/// content and no configuration.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Reports the satellite version and the proto contract it serves.
///
/// Also unauthenticated, for the same reason as [`healthz`], and the first thing
/// an SDK calls so a major version mismatch fails immediately and clearly.
async fn version() -> Response {
    protobuf(&GetVersionResponse {
        satellite_version: env!("CARGO_PKG_VERSION").to_owned(),
        proto_major: PROTO_MAJOR,
        proto_minor: PROTO_MINOR,
    })
}

/// Full satellite state. Authenticated, because it names running threads.
async fn status(State(satellite): State<Arc<Satellite>>) -> Response {
    let filter = store::ThreadFilter {
        states: LIVE_THREAD_STATES.iter().copied().map(i32::from).collect(),
        limit: STATUS_THREAD_LIMIT,
        ..store::ThreadFilter::default()
    };

    let mut threads = match satellite.store.list_threads(&filter).await {
        Ok(listing) => listing.threads,
        Err(error) => return api::store_failure(&error),
    };

    // Read rather than derived from the listing: a thread can be RUNNING,
    // AWAITING_INPUT, or WATCHING with a turn in flight, and the cap counts
    // turns rather than states.
    let running_threads = match satellite.store.running_thread_count().await {
        Ok(count) => count,
        Err(error) => return api::store_failure(&error),
    };

    // Cached and measured off the runtime, so polling this endpoint against a
    // satellite holding a large workspace does not walk the tree per request.
    // The figures can lag by one refresh interval; see the `disk` module.
    let usage = satellite.disk.usage().await;
    for summary in &mut threads {
        summary.workspace_bytes = usage.thread_bytes(&summary.thread_id);
    }

    protobuf(&GetStatusResponse {
        satellite_version: env!("CARGO_PKG_VERSION").to_owned(),
        max_concurrent_threads: satellite.max_concurrent_threads,
        running_threads,
        threads,
        started_at: Some(satellite.started_at.clone()),
        insecure_mode: satellite.is_insecure(),
        // Absent when the volume would not say what it has left. Every other
        // field is measured, but a `DiskUsage` reporting zero free bytes claims
        // the satellite is out of room, which is a worse statement than "not
        // measured". The whole message waits on the one field that cannot be
        // faked.
        disk: usage.available_bytes.map(|available_bytes| DiskUsage {
            workspace_bytes: usage.workspace_bytes,
            available_bytes,
            // No satellite-wide ceiling exists to report. Quotas are per thread
            // today, and absent already means the volume's own capacity is the
            // only limit, which is exactly the truth.
            aggregate_quota_bytes: None,
            database_bytes: usage.database_bytes,
        }),
    })
}

/// Which harnesses this satellite offers and what each supports.
///
/// Authenticated like `/v1/status`: capability facts are not thread content,
/// but which CLI a satellite runs and at what version is configuration, and the
/// unauthenticated surface deliberately exposes none of that.
async fn harness_capabilities(State(satellite): State<Arc<Satellite>>) -> Response {
    protobuf(&satellite.harness)
}

fn router(satellite: Arc<Satellite>) -> Router {
    let authenticated = Router::new()
        .route("/v1/status", get(status))
        .route("/v1/harness", get(harness_capabilities))
        .merge(api::routes())
        .merge(stream::sockets::routes())
        .route_layer(from_fn_with_state(Arc::clone(&satellite), require_auth));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/version", get(version))
        .merge(authenticated)
        .with_state(satellite)
}

/// Interprets an optional configuration value as a positive integer.
///
/// Kept separate from the environment read so the decision is testable without
/// mutating process state, which is both unsafe in edition 2024 and racy when
/// tests run in parallel.
fn parse_positive_u32(key: &str, raw: Option<&str>, fallback: u32) -> u32 {
    let Some(raw) = raw else {
        return fallback;
    };

    match raw.parse::<u32>() {
        Ok(value) if value > 0 => value,
        _unparseable => {
            // A typo in an optional knob should not stop a satellite booting,
            // but it must not pass silently either.
            tracing::warn!(
                event.name = "satellite.config.invalid",
                config.key = key,
                config.value = %raw,
                config.fallback = fallback,
                "{{config.key}} is not a positive integer, using {{config.fallback}}",
            );
            fallback
        }
    }
}

/// Reads a positive integer from the environment.
fn env_u32(key: &str, fallback: u32) -> u32 {
    parse_positive_u32(key, std::env::var(key).ok().as_deref(), fallback)
}

/// Everything a satellite needs to boot, with nothing read from the
/// environment.
///
/// Separated from [`serve`] so a test can start a real satellite on an
/// ephemeral port without setting process-global variables, which is both
/// unsafe in edition 2024 and a race when tests run in parallel.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub secret: Option<String>,
    pub allow_insecure: bool,
    pub database_path: String,
    pub workspace_root: String,
    pub max_concurrent_threads: u32,

    /// How often to sweep for expired threads.
    pub collect_interval: std::time::Duration,
}

impl ServeOptions {
    /// Reads the options a container boot uses.
    #[must_use]
    pub fn from_environment() -> Self {
        Self {
            secret: std::env::var("ARSOX_SECRET").ok(),
            allow_insecure: std::env::var("ARSOX_ALLOW_INSECURE")
                .is_ok_and(|value| value == "true"),
            database_path: std::env::var("ARSOX_DB_PATH")
                .unwrap_or_else(|_ignored| DEFAULT_DB_PATH.to_owned()),
            workspace_root: std::env::var("ARSOX_WORKSPACE_ROOT")
                .unwrap_or_else(|_ignored| DEFAULT_WORKSPACE_ROOT.to_owned()),
            max_concurrent_threads: env_u32(
                "ARSOX_MAX_CONCURRENT_THREADS",
                DEFAULT_MAX_CONCURRENT_THREADS,
            ),
            collect_interval: std::time::Duration::from_secs(u64::from(env_u32(
                "ARSOX_COLLECT_INTERVAL",
                DEFAULT_COLLECT_INTERVAL_SECONDS,
            ))),
        }
    }
}

/// A satellite that has been built but is not yet listening.
#[derive(Debug)]
pub struct Assembled {
    pub router: Router,

    /// The store, already wired to the event bus.
    ///
    /// Handed back because live delivery is in-process: the bus lives beside
    /// this handle, and a second `Store` opened on the same file writes the same
    /// rows while publishing to nobody. That is fine for a satellite, which is
    /// one process per database, and it is a trap for anything that assumes
    /// otherwise.
    pub store: store::Store,
}

/// Builds everything a satellite is made of, without binding a port.
///
/// # Errors
///
/// Returns an error when the secret is absent without an explicit insecure opt
/// in, or when the database cannot be opened.
pub async fn assemble(options: ServeOptions) -> Result<Assembled> {
    let auth = resolve_auth(options.secret, options.allow_insecure)?;

    if matches!(auth, Auth::Insecure) {
        // Event identity travels as `event.name` rather than tracing's `name:`
        // argument. Both are wanted: named events make entries groupable, and
        // OTel dotted attributes make them portable. tracing's macro cannot
        // parse `name:` alongside a dotted field name, so the identity moves
        // into a field, and `event.name` is an OTel convention in its own right.
        tracing::warn!(
            event.name = "satellite.boot.insecure",
            "ARSOX_ALLOW_INSECURE is set. This satellite accepts unauthenticated \
             requests and must not be reachable from an untrusted network."
        );
    }

    let bus = stream::EventBus::new();

    let store = store::Store::open(&options.database_path)
        .await
        .with_context(|| format!("failed to open the database at {}", options.database_path))?
        .with_bus(bus.clone());

    tracing::info!(
        event.name = "satellite.boot.database_ready",
        db.path = %options.database_path,
        "database open and migrated",
    );

    // A turn that was in flight when the satellite stopped is not lost work:
    // the thread and its workspace survive, and the turn is marked interrupted
    // so a policy or a human can decide whether to resume it. Leaving it
    // RUNNING would block its thread forever behind a turn nothing is driving.
    match store.settle_interrupted_turns().await {
        Ok(settled) if !settled.resumed.is_empty() || !settled.left_interrupted.is_empty() => {
            tracing::warn!(
                event.name = "satellite.boot.interrupted_turns",
                turn.resumed = settled.resumed.len(),
                turn.left_interrupted = settled.left_interrupted.len(),
                "a restart interrupted turns: {{turn.resumed}} requeued, \
                 {{turn.left_interrupted}} awaiting a decision",
            );
        }
        Ok(_none) => {}
        Err(error) => {
            tracing::error!(
                event.name = "satellite.boot.interrupt_sweep_failed",
                "could not sweep interrupted turns: {error}",
            );
        }
    }

    let work_queued = Arc::new(tokio::sync::Notify::new());

    let collector = Arc::new(collector::Collector::new(
        store.clone(),
        std::path::PathBuf::from(&options.workspace_root),
        bus.clone(),
    ));
    tokio::spawn(Arc::clone(&collector).sweep_forever(options.collect_interval));

    let proxy = proxy::LlmProxy::start()
        .await
        .context("failed to start the llm proxy")?;

    let runner = harness::runner::Runner::new(
        store.clone(),
        std::path::PathBuf::from(&options.workspace_root),
        Arc::clone(&work_queued),
        options.max_concurrent_threads,
        Arc::clone(&collector),
        proxy,
    );
    tokio::spawn(runner.dispatch());

    tracing::info!(
        event.name = "satellite.boot.runner_ready",
        workspace.root = %options.workspace_root,
        thread.max_concurrent = options.max_concurrent_threads,
        "turn runner started",
    );

    let harness = harness::capabilities::resolve().await;

    let provisioner = workspace::Provisioner::new(
        store.clone(),
        std::path::PathBuf::from(&options.workspace_root),
        Arc::clone(&work_queued),
    );

    // A workspace half-built when the satellite stopped is rebuilt rather than
    // left as it was. Nothing else ever revisits PROVISIONING, so each one would
    // otherwise hold its queue for the life of the process while reporting
    // itself perfectly healthy.
    provisioner.resume_interrupted().await;

    Ok(Assembled {
        router: router(Arc::new(Satellite {
            auth,
            max_concurrent_threads: options.max_concurrent_threads,
            started_at: Timestamp::now(),
            store: store.clone(),
            work_queued,
            bus,
            collector,
            provisioner,
            disk: disk::Meter::new(
                std::path::PathBuf::from(&options.workspace_root),
                std::path::PathBuf::from(&options.database_path),
            ),
            harness,
        })),
        store,
    })
}

/// Boots the satellite and serves until interrupted.
///
/// # Errors
///
/// Returns an error when `ARSOX_SECRET` is absent without an explicit insecure
/// opt in, when the listen address cannot be bound, or when the server itself
/// fails.
pub async fn serve() -> Result<()> {
    let assembled = assemble(ServeOptions::from_environment()).await?;

    let listener = tokio::net::TcpListener::bind(LISTEN_ADDR)
        .await
        .with_context(|| format!("failed to bind {LISTEN_ADDR}"))?;

    tracing::info!(
        event.name = "satellite.boot.listening",
        server.address = %LISTEN_ADDR,
        satellite.version = env!("CARGO_PKG_VERSION"),
        proto.major = PROTO_MAJOR,
        proto.minor = PROTO_MINOR,
        "satellite listening on {{server.address}}",
    );

    axum::serve(listener, assembled.router)
        .with_graceful_shutdown(async {
            let _ignored = tokio::signal::ctrl_c().await;
            tracing::info!(event.name = "satellite.shutdown.requested", "shutting down");
        })
        .await
        .context("server error")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(value).expect("test header should be valid"),
        );
        headers
    }

    #[test]
    fn a_present_secret_authenticates_with_a_bearer_token() {
        let auth = resolve_auth(Some("s3cret".to_owned()), false).expect("should accept a secret");
        assert!(matches!(auth, Auth::Bearer(ref value) if value == "s3cret"));
    }

    #[test]
    fn an_absent_secret_refuses_to_start() {
        // The whole point of failing closed: no secret and no explicit opt in
        // means the process does not come up at all.
        let error = resolve_auth(None, false).expect_err("should refuse to start");
        assert!(error.to_string().contains("ARSOX_SECRET is not set"));
    }

    #[test]
    fn an_absent_secret_is_allowed_only_when_insecure_mode_is_requested() {
        let auth = resolve_auth(None, true).expect("should allow an explicit opt in");
        assert!(matches!(auth, Auth::Insecure));
    }

    #[test]
    fn an_empty_secret_is_refused_rather_than_treated_as_absent() {
        // Otherwise `ARSOX_SECRET=` in a compose file silently produces a
        // satellite that authenticates every caller with an empty string.
        let error =
            resolve_auth(Some("   ".to_owned()), true).expect_err("should refuse an empty secret");
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn the_configured_secret_authenticates_and_nothing_else_does() {
        let auth = resolve_auth(Some("s3cret".to_owned()), false).expect("should accept a secret");

        assert!(auth.authenticates("s3cret"));
        assert!(!auth.authenticates("wrong"));
        assert!(!auth.authenticates(""));
        // A correct prefix must not pass. This is the case a length check alone
        // would let through if the comparison ever regressed to `starts_with`.
        assert!(!auth.authenticates("s3cre"));
        assert!(!auth.authenticates("s3cret "));
    }

    #[test]
    fn an_insecure_satellite_accepts_any_token_including_none() {
        let auth = resolve_auth(None, true).expect("should allow an explicit opt in");

        assert!(auth.authenticates(""));
        assert!(auth.authenticates("anything at all"));
    }

    #[test]
    fn a_missing_header_is_a_different_code_from_a_wrong_scheme() {
        // Codes are specific on purpose: a caller sending Basic auth has a
        // different bug from a caller sending nothing.
        assert_eq!(
            bearer_token(&HeaderMap::new()),
            Err(ErrorCode::AuthHeaderMissing)
        );
        assert_eq!(
            bearer_token(&header_map("Basic aGk6dGhlcmU=")),
            Err(ErrorCode::AuthSchemeUnsupported)
        );
        assert_eq!(bearer_token(&header_map("Bearer s3cret")), Ok("s3cret"));
    }

    #[test]
    fn a_bearer_prefix_without_a_space_is_not_a_bearer_token() {
        // `Bearers3cret` must not authenticate as `s3cret`.
        assert_eq!(
            bearer_token(&header_map("Bearers3cret")),
            Err(ErrorCode::AuthSchemeUnsupported)
        );
    }

    #[test]
    fn version_reports_the_proto_contract_it_serves() {
        let body = GetVersionResponse {
            satellite_version: env!("CARGO_PKG_VERSION").to_owned(),
            proto_major: PROTO_MAJOR,
            proto_minor: PROTO_MINOR,
        };

        // Round-trips through the wire format the SDK actually receives.
        let decoded = GetVersionResponse::decode(body.encode_to_vec().as_slice())
            .expect("should decode its own encoding");

        assert_eq!(decoded.proto_major, 1);
        assert_eq!(decoded.satellite_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn an_invalid_thread_cap_falls_back_rather_than_refusing_to_boot() {
        assert_eq!(parse_positive_u32("ARSOX_TEST_CAP", None, 4), 4);
        assert_eq!(
            parse_positive_u32("ARSOX_TEST_CAP", Some("not-a-number"), 4),
            4
        );
        // Zero concurrent threads would be a satellite that accepts work and
        // never runs it, which is worse than the documented default.
        assert_eq!(parse_positive_u32("ARSOX_TEST_CAP", Some("0"), 4), 4);
        assert_eq!(parse_positive_u32("ARSOX_TEST_CAP", Some("12"), 4), 12);
    }
}
