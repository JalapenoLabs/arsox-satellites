// Copyright © 2026 Jalapeno Labs

//! The Arsox satellite: an HTTP and WebSocket API that runs agent harnesses and
//! normalizes their output into one protobuf contract.
//!
//! This binary is the process that lives inside the satellite container. It owns
//! the workspace, the thread and turn lifecycle, the job queue, and the event
//! stream. Clients reach it through one of the SDKs or the `arsox` CLI, never by
//! hand-rolling requests, though the wire protocol is documented well enough to
//! drive from `curl` when debugging.
//!
//! # Boot
//!
//! The satellite refuses to start without `ARSOX_SECRET`. There is no
//! unauthenticated mode by accident: an open satellite is a remote shell with
//! your credentials in it. Running without one is possible and must be asked for
//! explicitly with `ARSOX_ALLOW_INSECURE=true`, which warns loudly on every boot.

use anyhow::{Context as _, Result, bail};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::{Error as ContractError, ErrorCode};
use arsox_sdk::proto::satellite::v1::{GetStatusResponse, GetVersionResponse};
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

/// Notably faster along allocating hot paths, which here means request decoding
/// and event fan-out. Unavailable on MSVC, where the system allocator stands in.
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The proto contract major version this satellite serves.
///
/// An SDK refuses to talk to a satellite advertising a higher major, rather than
/// failing later with a confusing decode error.
const PROTO_MAJOR: u32 = 1;

/// The proto contract minor version this satellite serves.
///
/// An SDK meeting a higher minor warns once and proceeds, ignoring additive
/// fields it does not know about.
const PROTO_MINOR: u32 = 0;

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

/// The wire type the SDK always speaks. JSON is a debugging affordance offered
/// through `Accept`, never what an SDK sends.
const PROTOBUF_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/protobuf");

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
struct Satellite {
    auth: Auth,
    max_concurrent_threads: u32,
    started_at: Timestamp,
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
fn protobuf<M: prost::Message>(message: &M) -> Response {
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
fn contract_error(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let body = ContractError {
        code: code.into(),
        message: message.to_owned(),
        // Nothing this function reports is worth retrying unchanged. Retryable
        // failures are produced where they happen, with their own context.
        retryable: false,
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
    protobuf(&GetStatusResponse {
        satellite_version: env!("CARGO_PKG_VERSION").to_owned(),
        max_concurrent_threads: satellite.max_concurrent_threads,
        running_threads: 0,
        threads: Vec::new(),
        started_at: Some(satellite.started_at.clone()),
        insecure_mode: satellite.is_insecure(),
        // Absent rather than zeroed. A zero here would claim the satellite is
        // holding no disk and has none free, which is a different and worse
        // statement than "not measured yet". Populated when the workspace
        // manager lands.
        disk: None,
    })
}

fn router(satellite: Arc<Satellite>) -> Router {
    let authenticated = Router::new()
        .route("/v1/status", get(status))
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_ignored| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let auth = resolve_auth(
        std::env::var("ARSOX_SECRET").ok(),
        std::env::var("ARSOX_ALLOW_INSECURE").is_ok_and(|value| value == "true"),
    )?;

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

    let satellite = Arc::new(Satellite {
        auth,
        max_concurrent_threads: env_u32(
            "ARSOX_MAX_CONCURRENT_THREADS",
            DEFAULT_MAX_CONCURRENT_THREADS,
        ),
        started_at: Timestamp::now(),
    });

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

    axum::serve(listener, router(satellite))
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
