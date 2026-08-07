// Copyright © 2026 Jalapeno Labs

//! The LLM proxy, driven over a real socket against a stub upstream.
//!
//! What these assert is the property the proxy exists for: the agent holds a
//! token that is worth nothing, and the credential it never sees is attached on
//! the way out. Asserting that from the upstream's seat is the only way to know
//! it, since the agent's side cannot see what was added after it.

use arsox_satellite::proxy::LlmProxy;
use arsox_satellite::proxy::upstream::Upstream;
use arsox_sdk::proto::common::v1::Secret;
use arsox_sdk::proto::settings::v1::llm_auth::Credential;
use arsox_sdk::proto::settings::v1::{LlmAuth, ModelEndpoint};
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What the stub upstream saw, so a test can assert on it afterwards.
#[derive(Debug, Default, Clone)]
struct Seen {
    api_key: Option<String>,
    authorization: Option<String>,
    body: String,
    path: String,
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Records what the provider actually received, then streams a reply.
async fn record(
    State(seen): State<Arc<Mutex<Seen>>>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: String,
) -> &'static str {
    let mut seen = seen.lock().await;
    seen.api_key = header(&headers, "x-api-key");
    seen.authorization = header(&headers, "authorization");
    seen.body = body;
    uri.path().clone_into(&mut seen.path);

    "event: message_start\ndata: {}\n\n"
}

/// A stand-in provider that records the request and streams a reply.
async fn stub_upstream() -> (String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));

    let router = Router::new()
        .route("/v1/messages", post(record))
        .with_state(Arc::clone(&seen));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("should bind the stub upstream");
    let address = listener.local_addr().expect("should have an address");

    tokio::spawn(async move {
        let _served = axum::serve(listener, router).await;
    });

    (format!("http://{address}"), seen)
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
    let (upstream_url, seen) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let upstream = Upstream::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy.grant("thread-1", "turn-1", upstream).await;

    let response = post_completion(&proxy.base_url_for(&token), &token).await;
    assert_eq!(response.status(), 200);

    let seen = seen.lock().await;
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
    let (upstream_url, seen) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let upstream = Upstream::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy.grant("thread-1", "turn-1", upstream).await;

    // A token nobody granted. Nothing should reach the upstream, because
    // reaching it is what spends money.
    let invented = "019fd000-0000-7000-8000-000000000000";
    let response = post_completion(&proxy.base_url_for(invented), invented).await;

    assert_eq!(response.status(), 401);
    assert!(
        seen.lock().await.path.is_empty(),
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
    let (upstream_url, _seen) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let upstream = Upstream::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy.grant("thread-1", "turn-1", upstream).await;

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
    let (upstream_url, seen) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let upstream = Upstream::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy.grant("thread-1", "turn-1", upstream).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.base_url_for(&token)))
        .header("x-api-key", "not-the-granted-token")
        .body("{}")
        .send()
        .await
        .expect("should reach the proxy");

    assert_eq!(response.status(), 401);
    assert!(
        seen.lock().await.path.is_empty(),
        "a mismatched key must not reach the provider"
    );
}

#[tokio::test]
async fn a_credential_the_agent_supplied_is_replaced_rather_than_passed_along() {
    // An agent that sends its own Authorization header must not have it reach
    // the provider, or an agent with a stolen key could spend it through the
    // satellite and inherit the satellite's network access.
    let (upstream_url, seen) = stub_upstream().await;
    let proxy = LlmProxy::start().await.expect("should start");

    let upstream = Upstream::resolve(&[endpoint_at(&upstream_url, "sk-ant-real-key")]);
    let token = proxy.grant("thread-1", "turn-1", upstream).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", proxy.base_url_for(&token)))
        .header("x-api-key", &token)
        .header("authorization", "Bearer an-agents-own-token")
        .body("{}")
        .send()
        .await
        .expect("should reach the proxy");

    assert_eq!(response.status(), 200);

    let seen = seen.lock().await;
    assert_eq!(
        seen.authorization, None,
        "the agent's own credential must be stripped, not forwarded alongside ours"
    );
    assert_eq!(seen.api_key.as_deref(), Some("sk-ant-real-key"));
}
