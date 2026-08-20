// Copyright © 2026 Jalapeno Labs

//! The LLM proxy, driven over a real socket against a stub upstream.
//!
//! What these assert is the property the proxy exists for: the agent holds a
//! token that is worth nothing, and the credential it never sees is attached on
//! the way out. Asserting that from the upstream's seat is the only way to know
//! it, since the agent's side cannot see what was added after it.

use arsox_satellite::proxy::budget::{Ceilings, Crossing, Meter};
use arsox_satellite::proxy::failover::Route;
use arsox_satellite::proxy::{Grant, LlmProxy};
use arsox_sdk::proto::common::v1::{Secret, TokenCeiling, token_ceiling};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::Ceiling;
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use arsox_sdk::proto::settings::v1::llm_auth::Credential;
use arsox_sdk::proto::settings::v1::{Budget, LlmAuth, ModelEndpoint};
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use futures_util::StreamExt as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

/// What the stub upstream saw, so a test can assert on it afterwards.
#[derive(Debug, Default, Clone)]
struct Seen {
    api_key: Option<String>,
    authorization: Option<String>,
    body: String,
    path: String,

    /// How many requests actually reached the provider, which is the number
    /// that costs money.
    requests: usize,
}

/// A stand-in provider: what it records, and what it answers with.
#[derive(Debug)]
struct Provider {
    seen: Mutex<Seen>,
    content_type: &'static str,
    reply: String,
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Records what the provider actually received, then answers.
async fn record(
    State(provider): State<Arc<Provider>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: String,
) -> impl IntoResponse {
    let mut seen = provider.seen.lock().await;
    seen.api_key = header(&headers, "x-api-key");
    seen.authorization = header(&headers, "authorization");
    seen.body = body;
    uri.path().clone_into(&mut seen.path);
    seen.requests += 1;
    // Released before the reply is built, so a test asserting on what the
    // provider saw is never waiting on the handler that recorded it.
    drop(seen);

    (
        [(axum::http::header::CONTENT_TYPE, provider.content_type)],
        provider.reply.clone(),
    )
}

/// A stand-in provider that records the request and streams a reply.
async fn stub_upstream() -> (String, Arc<Provider>) {
    // The shape a completion returns when nothing about usage is under test.
    stub_replying("text/event-stream", "event: message_start\ndata: {}\n\n").await
}

/// Same, answering with a body of the test's choosing.
async fn stub_replying(content_type: &'static str, reply: &str) -> (String, Arc<Provider>) {
    let provider = Arc::new(Provider {
        seen: Mutex::new(Seen::default()),
        content_type,
        reply: reply.to_owned(),
    });

    let router = Router::new()
        .route("/v1/messages", post(record))
        .with_state(Arc::clone(&provider));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the stub upstream");
    let address = listener.local_addr().expect("should have an address");

    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });

    (format!("http://{address}"), provider)
}

fn endpoint_at(base_url: &str, key: &str) -> ModelEndpoint {
    ModelEndpoint {
        name: "primary".to_owned(),
        model: "claude-opus-5".to_owned(),
        base_url: Some(base_url.to_owned()),
        auth: Some(LlmAuth {
            credential: Some(Credential::ApiKey(Secret {
                value: Some(key.to_owned()),
                display: None,
            })),
            presentation: None,
        }),
        retry: None,
    }
}

/// Sends a completion request the way a harness would.
async fn post_completion(base_url: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base_url}/v1/messages"))
        .header("x-api-key", token)
        .header("content-type", "application/json")
        .body(r#"{"model":"claude-opus-5"}"#)
        .send()
        .await
        .expect("should reach the proxy")
}

#[tokio::test]
async fn the_real_credential_is_attached_and_the_agents_token_never_leaves() {
    let (upstream_url, provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new(
            "thread-1",
            "turn-1",
            route,
            Arc::new(Meter::unmetered()),
        ))
        .await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);

    let seen = provider.seen.lock().await;
    assert_eq!(
        seen.api_key.as_deref(),
        Some("sk-ant-real-key"),
        "the upstream must receive the real credential"
    );
    assert_ne!(
        seen.api_key.as_deref(),
        Some(token.as_str()),
        "the agent's turn token must not be forwarded as the credential"
    );
    assert_eq!(seen.path, "/v1/messages", "the path is preserved");
    assert_eq!(seen.body, r#"{"model":"claude-opus-5"}"#);
}

#[tokio::test]
async fn a_request_carrying_no_valid_token_is_refused() {
    let (upstream_url, provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new(
            "thread-1",
            "turn-1",
            route,
            Arc::new(Meter::unmetered()),
        ))
        .await;

    // A token nobody granted. Nothing should reach the upstream, because
    // reaching it is what spends money.
    let invented = "019fd000-0000-7000-8000-000000000000";
    let response = post_completion(&proxy.base_url_for(invented), invented).await;

    assert_eq!(response.status(), 401);
    assert!(
        provider.seen.lock().await.path.is_empty(),
        "an unauthorized request must not reach the provider"
    );

    // And the real token still works, so the refusal was about the token rather
    // than the proxy being broken.
    assert_eq!(
        post_completion(&proxy.base_url_for(&token), &token)
            .await
            .status(),
        200
    );
}

#[tokio::test]
async fn a_revoked_token_stops_working_the_moment_its_turn_ends() {
    let (upstream_url, _provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new(
            "thread-1",
            "turn-1",
            route,
            Arc::new(Meter::unmetered()),
        ))
        .await;

    assert_eq!(
        post_completion(&proxy.base_url_for(&token), &token)
            .await
            .status(),
        200
    );

    proxy.revoke(&token).await;

    assert_eq!(
        post_completion(&proxy.base_url_for(&token), &token)
            .await
            .status(),
        401,
        "a grant that outlived its turn would keep spending after the work stopped"
    );
}

#[tokio::test]
async fn the_path_token_and_the_presented_key_must_agree() {
    // Routing on the path alone would let a process that guessed a URL spend a
    // turn's budget without ever holding its token.
    let (upstream_url, provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new(
            "thread-1",
            "turn-1",
            route,
            Arc::new(Meter::unmetered()),
        ))
        .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.base_url_for(&token)))
        .header("x-api-key", "not-the-granted-token")
        .body("{}")
        .send()
        .await
        .expect("should reach the proxy");

    assert_eq!(response.status(), 401);
    assert!(
        provider.seen.lock().await.path.is_empty(),
        "a mismatched key must not reach the provider"
    );
}

#[tokio::test]
async fn a_credential_the_agent_supplied_is_replaced_rather_than_passed_along() {
    // An agent that sends its own Authorization header must not have it reach
    // the provider, or an agent with a stolen key could spend it through the
    // satellite and inherit the satellite's network access.
    let (upstream_url, provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new(
            "thread-1",
            "turn-1",
            route,
            Arc::new(Meter::unmetered()),
        ))
        .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.base_url_for(&token)))
        .header("x-api-key", &token)
        .header("authorization", "Bearer an-agents-own-token")
        .body("{}")
        .send()
        .await
        .expect("should reach the proxy");

    assert_eq!(response.status(), 200);

    let seen = provider.seen.lock().await;
    assert_eq!(
        seen.authorization, None,
        "the agent's own credential must be stripped, not forwarded alongside ours"
    );
    assert_eq!(seen.api_key.as_deref(), Some("sk-ant-real-key"));
}

/// The usage a streamed Anthropic completion reports, at a size the test picks.
///
/// The real shape rather than an invented one: input arrives on `message_start`
/// and output is reported cumulatively on `message_delta`.
fn streamed_usage(input: u64, output: u64) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"usage\":\
         {{\"input_tokens\":{input},\"cache_read_input_tokens\":4096,\"output_tokens\":1}}}}}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":{output}}}}}\n\n"
    )
}

/// A meter with a token ceiling, plus the channel its crossings arrive on.
fn metered(tokens: u64) -> (Arc<Meter>, tokio::sync::mpsc::UnboundedReceiver<Crossing>) {
    let (crossings, reported) = tokio::sync::mpsc::unbounded_channel();

    let ceilings = Ceilings::from_budget(Some(&Budget {
        max_tokens_per_turn: Some(TokenCeiling {
            ceiling: Some(token_ceiling::Ceiling::Tokens(tokens)),
        }),
        ..Budget::default()
    }));

    (Arc::new(Meter::new(&ceilings, crossings)), reported)
}

#[tokio::test]
async fn the_usage_a_response_reports_is_counted_against_the_turn() {
    // The proxy reads what the provider said it cost on the way past, without
    // buffering the stream: the whole point of counting here rather than after.
    let (upstream_url, _provider) =
        stub_replying("text/event-stream", &streamed_usage(300, 40)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let (meter, _reported) = metered(10_000);
    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new("thread-1", "turn-1", route, Arc::clone(&meter)))
        .await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);
    // Drained, because the count lands once the body is done rather than when
    // the headers arrive.
    let _body = response.text().await.expect("should read the reply");

    assert_eq!(
        meter.tokens_spent(),
        340,
        "input plus output, with the cache reads excluded exactly as the \
         canonical total excludes them"
    );
}

#[tokio::test]
async fn a_turn_is_warned_at_eighty_percent_of_its_token_ceiling() {
    // The warning is the only chance a host application has to react before the
    // wall, so it has to arrive while the turn is still running.
    let (upstream_url, _provider) =
        stub_replying("text/event-stream", &streamed_usage(700, 100)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let (meter, mut reported) = metered(1_000);
    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new("thread-1", "turn-1", route, Arc::clone(&meter)))
        .await;

    let _body = post_completion(&proxy.base_url_for(&token), &token)
        .await
        .text()
        .await
        .expect("should read the reply");

    assert_eq!(
        reported.recv().await.expect("should report the crossing"),
        Crossing::Approaching {
            ceiling: Ceiling::TokensPerTurn,
            percent_used: 80,
        }
    );
    assert_eq!(
        meter.reached(),
        None,
        "eighty percent is a warning, not a wall"
    );
}

#[tokio::test]
async fn a_turn_past_its_token_ceiling_is_refused_before_it_reaches_the_provider() {
    // The property the whole feature rests on. A refusal that still called the
    // upstream would be a ceiling that costs exactly as much as no ceiling.
    let (upstream_url, provider) =
        stub_replying("text/event-stream", &streamed_usage(400, 100)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let (meter, mut reported) = metered(500);
    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new("thread-1", "turn-1", route, Arc::clone(&meter)))
        .await;

    let first = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(
        first.status(),
        200,
        "the request that spends the ceiling still completes"
    );
    let _body = first.text().await.expect("should read the reply");

    assert_eq!(
        reported.recv().await.expect("should report the crossing"),
        Crossing::Reached {
            ceiling: Ceiling::TokensPerTurn,
        }
    );

    let second = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(
        second.status(),
        403,
        "a 403 rather than a 429: this wall does not move before the turn ends"
    );

    assert_eq!(
        provider.seen.lock().await.requests,
        1,
        "the refused completion must not have reached the provider"
    );
}

#[tokio::test]
async fn usage_is_counted_from_a_response_that_did_not_stream() {
    // A harness may ask for a whole document rather than events, and a ceiling
    // that only counted server-sent events would be silently unenforced for it.
    let (upstream_url, _provider) = stub_replying(
        "application/json",
        r#"{"type":"message","usage":{"input_tokens":90,"output_tokens":10}}"#,
    )
    .await;
    let proxy = LlmProxy::start().await.expect("should start");

    let (meter, _reported) = metered(10_000);
    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new("thread-1", "turn-1", route, Arc::clone(&meter)))
        .await;

    let _body = post_completion(&proxy.base_url_for(&token), &token)
        .await
        .text()
        .await
        .expect("should read the reply");

    assert_eq!(meter.tokens_spent(), 100);
}

/// A stub upstream that accepts a request and then never answers.
///
/// The failure the request bound exists for: from the satellite's side an
/// endpoint that stopped answering is indistinguishable from a model thinking
/// hard, and the harness blocked on the socket cannot tell either.
async fn stub_that_never_answers() -> (String, Arc<AtomicUsize>) {
    let reached = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reached);

    let router = Router::new().route(
        "/v1/messages",
        post(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            // Longer than any bound a test sets, and the test never waits it out.
            async { tokio::time::sleep(Duration::from_secs(30)).await }
        }),
    );

    (serving(router).await, reached)
}

/// A stub upstream that starts streaming and then stops, without ending.
///
/// A slow drip is the shape a bound on the handshake alone would never catch:
/// the headers arrived, the body began, and the request runs forever.
async fn stub_that_stalls_mid_stream() -> String {
    let router = Router::new().route(
        "/v1/messages",
        post(|| async {
            let opening = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                    b"event: message_start\ndata: {}\n\n",
                ))
            });

            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                axum::body::Body::from_stream(opening.chain(futures_util::stream::pending())),
            )
        }),
    );

    serving(router).await
}

/// Binds a router to loopback and serves it, returning its base URL.
async fn serving(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the stub upstream");
    let address = listener.local_addr().expect("should have an address");

    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });

    format!("http://{address}")
}

/// A grant bounded in milliseconds, plus the channel its incidents arrive on.
///
/// The bound rides on the grant rather than on the proxy, so one turn's short
/// bound cannot decide the limit for every other turn on the satellite. That is
/// also what makes it injectable here without a satellite-wide constant.
fn bounded_grant(
    route: Route,
    millis: u64,
) -> (Grant, tokio::sync::mpsc::UnboundedReceiver<Incident>) {
    let (incidents, reported) = tokio::sync::mpsc::unbounded_channel();

    let grant = Grant::new("thread-1", "turn-1", route, Arc::new(Meter::unmetered()))
        .bounded(Duration::from_millis(millis))
        .reporting_to(incidents);

    (grant, reported)
}

/// Asserts an incident is the timeout the contract names, however it was found.
fn assert_is_a_timeout(incident: &Incident) {
    assert_eq!(incident.code, i32::from(ErrorCode::LlmEndpointTimeout));
    assert_eq!(
        incident.disposition,
        i32::from(Disposition::Degraded),
        "the harness may retry or fail over, so the turn is not over"
    );
    assert!(incident.retryable, "the next attempt meets a new socket");
    assert_eq!(incident.thread_id.as_deref(), Some("thread-1"));
    assert_eq!(incident.turn_id.as_deref(), Some("turn-1"));
}

#[tokio::test]
async fn a_request_the_endpoint_never_answers_is_abandoned_at_the_bound() {
    // Without this the harness waits on a socket forever and its thread waits
    // on the harness, which is the hung thread the whole timeout table exists to
    // prevent.
    let (upstream_url, reached) = stub_that_never_answers().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let (grant, mut reported) = bounded_grant(route, 300);
    let token = proxy.grant(grant).await;

    let started = std::time::Instant::now();
    let response = post_completion(&proxy.base_url_for(&token), &token).await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the bound should have ended the wait, not the upstream"
    );
    assert_eq!(
        response.status(),
        504,
        "a 5xx is what the harness's retry and failover policy is written against"
    );
    assert_eq!(
        reached.load(Ordering::Relaxed),
        1,
        "the request did reach the provider, which is why this is a timeout \
         rather than a refusal"
    );

    assert_is_a_timeout(&reported.recv().await.expect("should report the timeout"));
}

#[tokio::test]
async fn a_response_still_streaming_past_the_bound_is_cut_off() {
    // The bound covers time to completion of the relay. A provider that sends a
    // token a minute for an hour has failed, and a bound on the handshake alone
    // would call that a success.
    let upstream_url = stub_that_stalls_mid_stream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let (grant, mut reported) = bounded_grant(route, 500);
    let token = proxy.grant(grant).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(
        response.status(),
        200,
        "the headers arrived, so the failure is in the body"
    );

    // An error rather than a tidy end. A truncated event stream that ended
    // cleanly would reach the harness as a completion that simply stopped.
    let body = tokio::time::timeout(Duration::from_secs(10), response.bytes())
        .await
        .expect("the bound should have ended the body");
    assert!(
        body.is_err(),
        "a body cut off at its bound must fail rather than end quietly"
    );

    assert_is_a_timeout(&reported.recv().await.expect("should report the timeout"));
}

#[tokio::test]
async fn a_request_answered_inside_its_bound_reports_nothing() {
    // A bound that fired on healthy traffic would fill the incident log with the
    // one thing an operator most needs to be able to trust.
    let (upstream_url, _provider) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let (grant, mut reported) = bounded_grant(route, 30_000);
    let token = proxy.grant(grant).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);
    let _body = response.text().await.expect("should read the reply");

    assert!(reported.try_recv().is_err(), "nothing went wrong");
}

#[tokio::test]
async fn a_turn_that_declared_no_ceiling_is_never_refused() {
    // Budgets are required at thread creation, and `Unlimited` is a thing a
    // caller may genuinely mean. Counting must not become refusing on its own.
    let (upstream_url, provider) =
        stub_replying("text/event-stream", &streamed_usage(100_000, 50_000)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let meter = Arc::new(Meter::unmetered());
    let route = Route::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy
        .grant(Grant::new("thread-1", "turn-1", route, Arc::clone(&meter)))
        .await;

    for _request in 0..2 {
        let response = post_completion(&proxy.base_url_for(&token), &token).await;
        assert_eq!(response.status(), 200);
        let _body = response.text().await.expect("should read the reply");
    }

    assert_eq!(provider.seen.lock().await.requests, 2);
    assert_eq!(meter.tokens_spent(), 300_000, "counted, and never refused");
    assert_eq!(meter.reached(), None);
}
