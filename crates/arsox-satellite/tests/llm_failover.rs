// Copyright © 2026 Jalapeno Labs

//! Endpoint failover, driven over real sockets against stub upstreams.
//!
//! What these assert is the promise the endpoint list makes: the order is
//! followed strictly, each endpoint is tried exactly as hard as its own policy
//! says, and nothing about a failover is silent. The stubs are asserted from the
//! provider's seat, because how many requests actually reached a provider is the
//! number that costs money and the only place it is visible.
//!
//! Every backoff here is the real schedule a policy produced. The waits are
//! recorded rather than taken, so a five second first backoff is exercised in
//! microseconds and asserted exactly, instead of being approximated by a
//! scaled-down copy that proves nothing about the numbers the README publishes.

#![cfg(feature = "test-util")]

use arsox_satellite::proxy::budget::{Ceilings, Meter};
use arsox_satellite::proxy::failover::{Recorder, Route, Waits};
use arsox_satellite::proxy::{Grant, LlmProxy};
use arsox_sdk::proto::common::v1::{Secret, TokenCeiling, token_ceiling};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::incident::v1::{Disposition, Incident};
use arsox_sdk::proto::settings::v1::llm_auth::Credential;
use arsox_sdk::proto::settings::v1::{Budget, LlmAuth, ModelEndpoint, RetryPolicy};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::post;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A stand-in provider that answers with whatever a test told it to.
#[derive(Debug)]
struct Provider {
    /// How many requests actually reached it, which is the number that costs
    /// money.
    requests: AtomicUsize,

    status: StatusCode,
    content_type: &'static str,
    reply: String,

    /// The `Retry-After` this provider asks for, when it asks for one.
    retry_after: Option<&'static str>,
}

impl Provider {
    fn answering(status: u16) -> Self {
        Self {
            requests: AtomicUsize::new(0),
            status: StatusCode::from_u16(status).expect("a real status"),
            content_type: "application/json",
            reply: r#"{"type":"error","error":{"type":"api_error"}}"#.to_owned(),
            retry_after: None,
        }
    }

    fn streaming(reply: String) -> Self {
        Self {
            requests: AtomicUsize::new(0),
            status: StatusCode::OK,
            content_type: "text/event-stream",
            reply,
            retry_after: None,
        }
    }

    fn asking_to_wait(mut self, seconds: &'static str) -> Self {
        self.retry_after = Some(seconds);
        self
    }

    /// Requests this provider has been sent.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::Relaxed)
    }
}

/// Counts the request, then answers as configured.
async fn answer(State(provider): State<Arc<Provider>>) -> impl IntoResponse {
    provider.requests.fetch_add(1, Ordering::Relaxed);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        provider.content_type.parse().expect("a header value"),
    );

    if let Some(retry_after) = provider.retry_after {
        headers.insert(
            header::RETRY_AFTER,
            retry_after.parse().expect("a header value"),
        );
    }

    (provider.status, headers, provider.reply.clone())
}

/// Serves one provider on loopback, and hands back its URL.
async fn serving(provider: Provider) -> (String, Arc<Provider>) {
    let provider = Arc::new(provider);

    let router = Router::new()
        .route("/v1/messages", post(answer))
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

/// A stub upstream that accepts a request and then never answers.
async fn stub_that_never_answers() -> String {
    let router = Router::new().route(
        "/v1/messages",
        // Longer than any bound a test sets, and no test waits it out.
        post(|| async { tokio::time::sleep(Duration::from_secs(30)).await }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the stub upstream");
    let address = listener.local_addr().expect("should have an address");

    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });

    format!("http://{address}")
}

/// The usage a streamed completion reports, at a size the test picks.
fn streamed_usage(input: u64, output: u64) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"usage\":\
         {{\"input_tokens\":{input},\"output_tokens\":1}}}}}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"usage\":{{\"output_tokens\":{output}}}}}\n\n"
    )
}

/// One declared endpoint, tried at most `attempts` times.
fn endpoint(name: &str, base_url: &str, attempts: u32) -> ModelEndpoint {
    ModelEndpoint {
        name: name.to_owned(),
        model: "claude-opus-5".to_owned(),
        base_url: Some(base_url.to_owned()),
        auth: Some(LlmAuth {
            credential: Some(Credential::ApiKey(Secret {
                value: Some(format!("sk-ant-{name}")),
                display: None,
            })),
            presentation: None,
        }),
        retry: Some(RetryPolicy {
            max_attempts: Some(attempts),
            ..RetryPolicy::default()
        }),
    }
}

/// A grant under test, and everything a test reads back out of it.
struct Driven {
    grant: Grant,
    waits: Recorder,
    incidents: tokio::sync::mpsc::UnboundedReceiver<Incident>,
    meter: Arc<Meter>,
}

/// A grant over `route` whose backoffs are written down rather than taken.
fn driven(route: Route, ceiling: Option<u64>, bound: Duration) -> Driven {
    let (incidents, reported) = tokio::sync::mpsc::unbounded_channel();
    let (waits, recorder) = Waits::recorded();

    let meter = ceiling.map_or_else(
        || Arc::new(Meter::unmetered()),
        |tokens| {
            let (crossings, _nobody_listening) = tokio::sync::mpsc::unbounded_channel();
            let ceilings = Ceilings::from_budget(Some(&Budget {
                max_tokens_per_turn: Some(TokenCeiling {
                    ceiling: Some(token_ceiling::Ceiling::Tokens(tokens)),
                }),
                ..Budget::default()
            }));

            Arc::new(Meter::new(&ceilings, crossings))
        },
    );

    let grant = Grant::new("thread-1", "turn-1", route, Arc::clone(&meter))
        .bounded(bound)
        .reporting_to(incidents)
        .waiting_with(waits);

    Driven {
        grant,
        waits: recorder,
        incidents: reported,
        meter,
    }
}

/// A grant with the bound no test is trying to trip.
fn unhurried(route: Route) -> Driven {
    driven(route, None, Duration::from_secs(30))
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

/// Everything the proxy reported about one request.
fn reported(driven: &mut Driven) -> Vec<Incident> {
    let mut incidents = Vec::new();

    while let Ok(incident) = driven.incidents.try_recv() {
        incidents.push(incident);
    }

    incidents
}

/// The one incident carrying `code`, or a failure naming what did arrive.
fn incident_with(incidents: &[Incident], code: ErrorCode) -> &Incident {
    incidents
        .iter()
        .find(|incident| incident.code == i32::from(code))
        .unwrap_or_else(|| {
            panic!(
                "expected an incident with {}, found {:?}",
                code.as_str_name(),
                incidents
                    .iter()
                    .map(|incident| incident.message.clone())
                    .collect::<Vec<_>>()
            )
        })
}

/// Every endpoint named in an incident's `details.attempts`, in order.
fn attempted(incident: &Incident) -> Vec<(String, Option<f64>, f64)> {
    let details = incident.details.as_ref().expect("should carry details");

    let Some(prost_types::value::Kind::ListValue(attempts)) = details
        .fields
        .get("attempts")
        .and_then(|value| value.kind.clone())
    else {
        panic!("attempts is the key the contract names for this code");
    };

    attempts
        .values
        .iter()
        .map(|value| {
            let Some(prost_types::value::Kind::StructValue(attempt)) = value.kind.clone() else {
                panic!("every attempt is a struct");
            };

            let string = |key: &str| match attempt.fields.get(key).and_then(|v| v.kind.clone()) {
                Some(prost_types::value::Kind::StringValue(value)) => value,
                _absent => panic!("{key} is missing from an attempt"),
            };
            let number = |key: &str| match attempt.fields.get(key).and_then(|v| v.kind.clone()) {
                Some(prost_types::value::Kind::NumberValue(value)) => Some(value),
                _absent => None,
            };

            (
                string("endpoint"),
                number("status"),
                number("attempts").expect("every attempt records how many it made"),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_rate_limited_endpoint_is_retried_on_its_own_schedule_then_failed_over() {
    // The whole shape of the feature in one test: wait it out as long as the
    // policy says, then move on rather than waiting forever.
    let (limited_url, limited) = serving(Provider::answering(429)).await;
    let (spare_url, spare) = serving(Provider::streaming(streamed_usage(10, 5))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let mut driven = unhurried(Route::resolve(&[
        endpoint("primary", &limited_url, 3),
        endpoint("backup", &spare_url, 1),
    ]));
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200, "the backup answered");
    let _body = response.text().await.expect("should read the reply");

    assert_eq!(limited.requests(), 3, "exactly what the policy allowed");
    assert_eq!(spare.requests(), 1);
    assert_eq!(
        driven.waits.taken(),
        [Duration::from_secs(5), Duration::from_secs(10)],
        "the documented schedule: five seconds, doubling, and no wait after the \
         attempt that gave up"
    );

    let incidents = reported(&mut driven);
    let recovered = incident_with(&incidents, ErrorCode::LlmEndpointRateLimited);

    assert_eq!(
        recovered.disposition,
        i32::from(Disposition::Recovered),
        "a failover that works looks exactly like success unless it is recorded"
    );
    assert_eq!(
        attempted(recovered),
        [("primary".to_owned(), Some(429.0), 3.0)]
    );
}

#[tokio::test]
async fn an_endpoint_that_rejects_the_credential_is_given_up_on_without_retrying() {
    // A key that is wrong is wrong on the tenth attempt too, and a second
    // endpoint carrying different credentials is what the list exists for.
    let (rejecting_url, rejecting) = serving(Provider::answering(401)).await;
    let (spare_url, spare) = serving(Provider::streaming(streamed_usage(10, 5))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let mut driven = unhurried(Route::resolve(&[
        // Ten attempts allowed, and an auth rejection must still spend one.
        endpoint("expired", &rejecting_url, 10),
        endpoint("backup", &spare_url, 1),
    ]));
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);
    let _body = response.text().await.expect("should read the reply");

    assert_eq!(rejecting.requests(), 1, "no retry buys a rejected key back");
    assert_eq!(spare.requests(), 1);
    assert!(
        driven.waits.taken().is_empty(),
        "nothing was worth waiting for"
    );

    let incidents = reported(&mut driven);
    let recovered = incident_with(&incidents, ErrorCode::LlmEndpointUnauthorized);
    assert_eq!(recovered.disposition, i32::from(Disposition::Recovered));
}

#[tokio::test]
async fn every_endpoint_exhausted_says_why_each_one_was_given_up_on() {
    // `details.attempts` is the only place a caller learns which endpoint failed
    // and how. Without it the aggregate code says "everything broke" and stops.
    let (first_url, first) = serving(Provider::answering(429)).await;
    let (second_url, second) = serving(Provider::answering(401)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let mut driven = unhurried(Route::resolve(&[
        endpoint("primary", &first_url, 2),
        endpoint("backup", &second_url, 5),
    ]));
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(
        response.status(),
        502,
        "a 5xx is what the harness's own retry policy is written against"
    );

    assert_eq!(first.requests(), 2);
    assert_eq!(second.requests(), 1, "an auth rejection is never retried");

    let incidents = reported(&mut driven);
    let exhausted = incident_with(&incidents, ErrorCode::LlmAllEndpointsExhausted);

    assert_eq!(
        exhausted.disposition,
        i32::from(Disposition::Fatal),
        "the turn ends on this, which is what the contract promises"
    );
    assert!(
        exhausted.retryable,
        "the workspace survives, so a turn submitted after the credentials are \
         fixed resumes where this one stopped"
    );
    assert_eq!(
        attempted(exhausted),
        [
            ("primary".to_owned(), Some(429.0), 2.0),
            ("backup".to_owned(), Some(401.0), 1.0),
        ],
        "in the order they were tried, with what each answered and how many \
         requests it took to decide"
    );
}

#[tokio::test]
async fn an_endpoint_that_says_when_to_come_back_is_waited_for_that_long() {
    // A provider knows better than our schedule does when it will serve again.
    let (limited_url, _limited) = serving(Provider::answering(429).asking_to_wait("2")).await;
    let (spare_url, _spare) = serving(Provider::streaming(streamed_usage(10, 5))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let driven = unhurried(Route::resolve(&[
        endpoint("primary", &limited_url, 2),
        endpoint("backup", &spare_url, 1),
    ]));
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);
    let _body = response.text().await.expect("should read the reply");

    assert_eq!(
        driven.waits.taken(),
        [Duration::from_secs(2)],
        "two seconds because the endpoint asked for two, not five because our \
         schedule starts there"
    );
}

#[tokio::test]
async fn a_policy_of_one_attempt_sends_exactly_one_request() {
    // "Zero disables retries" and one attempt mean the same thing: the endpoint
    // is tried, once, and then the satellite moves on.
    let (limited_url, limited) = serving(Provider::answering(429)).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let mut driven = unhurried(Route::resolve(&[endpoint("only", &limited_url, 1)]));
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 502);

    assert_eq!(limited.requests(), 1);
    assert!(
        driven.waits.taken().is_empty(),
        "there is no waiting after the attempt that gave up"
    );

    let incidents = reported(&mut driven);
    assert_eq!(
        attempted(incident_with(
            &incidents,
            ErrorCode::LlmAllEndpointsExhausted
        )),
        [("only".to_owned(), Some(429.0), 1.0)]
    );
}

#[tokio::test]
async fn usage_is_metered_from_the_endpoint_that_answered() {
    // The ceiling has to keep counting across a failover, or a turn that failed
    // over once spends the rest of its life unmetered.
    let (limited_url, _limited) = serving(Provider::answering(429)).await;
    let (spare_url, _spare) = serving(Provider::streaming(streamed_usage(300, 40))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let driven = driven(
        Route::resolve(&[
            endpoint("primary", &limited_url, 1),
            endpoint("backup", &spare_url, 1),
        ]),
        Some(10_000),
        Duration::from_secs(30),
    );
    let token = proxy.grant(driven.grant.clone()).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);
    // Drained, because the count lands once the body is done rather than when
    // the headers arrive.
    let _body = response.text().await.expect("should read the reply");

    assert_eq!(
        driven.meter.tokens_spent(),
        340,
        "what the endpoint that answered reported, and nothing from the one that \
         refused"
    );
}

#[tokio::test]
async fn a_turn_past_its_ceiling_reaches_no_endpoint_at_all() {
    // Failover must not become a way around the ceiling. A refusal that walked
    // the endpoint list would cost exactly as much as no ceiling.
    let (first_url, first) = serving(Provider::streaming(streamed_usage(400, 100))).await;
    let (second_url, second) = serving(Provider::streaming(streamed_usage(10, 5))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let driven = driven(
        Route::resolve(&[
            endpoint("primary", &first_url, 1),
            endpoint("backup", &second_url, 1),
        ]),
        Some(500),
        Duration::from_secs(30),
    );
    let token = proxy.grant(driven.grant.clone()).await;

    let spent = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(spent.status(), 200);
    let _body = spent.text().await.expect("should read the reply");

    let refused = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(
        refused.status(),
        403,
        "a 403 rather than a 429: this wall does not move before the turn ends"
    );

    assert_eq!(first.requests(), 1);
    assert_eq!(
        second.requests(),
        0,
        "the refusal precedes the first endpoint, let alone the second"
    );
}

#[tokio::test]
async fn a_timed_out_endpoint_feeds_failover_rather_than_only_the_harness() {
    // Before this the bound answered the harness with a 504 and left the second
    // endpoint untouched, which made a list of two endpoints no better than one
    // against the failure the bound exists for.
    let silent_url = stub_that_never_answers().await;
    let (spare_url, spare) = serving(Provider::streaming(streamed_usage(10, 5))).await;
    let proxy = LlmProxy::start().await.expect("should start");

    let mut driven = driven(
        Route::resolve(&[
            endpoint("silent", &silent_url, 3),
            endpoint("backup", &spare_url, 1),
        ]),
        None,
        Duration::from_millis(300),
    );
    let token = proxy.grant(driven.grant.clone()).await;

    let started = std::time::Instant::now();
    let response = post_completion(&proxy.base_url_for(&token), &token).await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the bound should have ended the wait, not the upstream"
    );
    assert_eq!(response.status(), 200, "the backup answered");
    let _body = response.text().await.expect("should read the reply");
    assert_eq!(spare.requests(), 1);

    let incidents = reported(&mut driven);

    assert_eq!(
        incident_with(&incidents, ErrorCode::LlmEndpointTimeout).disposition,
        i32::from(Disposition::Degraded),
        "the timeout itself is degraded: the turn went on"
    );
    assert!(
        incidents
            .iter()
            .any(|incident| incident.disposition == i32::from(Disposition::Recovered)),
        "and the failover it caused is recorded, because it was paid for"
    );
    assert!(
        driven.waits.taken().is_empty(),
        "a timed-out attempt is not retried, so the worst case stays one bound \
         per endpoint rather than one per attempt"
    );
}
