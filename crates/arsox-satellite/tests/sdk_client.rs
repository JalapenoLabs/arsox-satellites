// Copyright © 2026 Jalapeno Labs

//! The SDK driving a real satellite.
//!
//! This is the point of the exercise rather than a formality. The Rust SDK is
//! published, and the CLI is built in a separate repository against nothing but
//! its public surface. Anything a consumer needs and cannot reach from here is a
//! hole in the SDK, not something to reach around, and a test written from the
//! consumer's seat is the only place that shows up.

#![cfg(feature = "test-util")]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::Satellite as Client;
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::settings::v1::{Budget, ThreadSettings};
use arsox_sdk::proto::thread::v1::ThreadState;
use arsox_sdk::proto::turn::v1::TurnStatus;
use futures_util::StreamExt as _;
use std::collections::BTreeMap;
use std::time::Duration;

/// Distinguishes scratch directories created in the same clock tick.
///
/// A timestamp alone is not unique: Windows clocks tick at 100 nanoseconds and
/// these tests run in parallel, so two of them can name the same directory,
/// share a database file, and race each other's migration.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const SECRET: &str = "sdk-test-secret";

/// The recorded transcript the stand-in replays.
///
/// Lives in `arsox-harness` because it is evidence about Claude's output rather
/// than about the satellite, and the mapper's own conformance tests assert
/// against the same bytes. One copy, so the two can never drift into proving
/// different things.
const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/tool-call.jsonl"
);

/// Starts a satellite and returns the URL a client would be given.
async fn start() -> String {
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    CONFIGURE.call_once(|| {
        // SAFETY: runs once, before any child is spawned, and writes values that
        // never change for the lifetime of the process.
        unsafe {
            std::env::set_var("ARSOX_CLAUDE_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_TRANSCRIPT", TRANSCRIPT);
        }
    });

    let unique = NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let directory = std::env::temp_dir().join(format!(
        "arsox-sdk-{unique}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    ));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("should create a scratch directory");

    let assembled = assemble(ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: directory.join("arsox.db").to_string_lossy().into_owned(),
        workspace_root: directory.to_string_lossy().into_owned(),
        max_concurrent_threads: 2,
        // Long enough that no test races the collector.
        collect_interval: std::time::Duration::from_hours(1),
    })
    .await
    .expect("should assemble");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("should bind");
    let port = listener
        .local_addr()
        .expect("should have an address")
        .port();

    tokio::spawn(async move {
        drop(axum::serve(listener, assembled.router).await);
    });

    format!("http://127.0.0.1:{port}")
}

/// The settings a thread must declare: an idle TTL and a budget.
fn settings() -> ThreadSettings {
    ThreadSettings {
        idle_ttl: Some(ProtoDuration {
            seconds: 3600,
            nanos: 0,
        }),
        budget: Some(Budget::default()),
        ..ThreadSettings::default()
    }
}

#[tokio::test]
async fn connecting_checks_the_contract_version() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let version = client.version().await.expect("should report a version");

    assert_eq!(version.proto_major, 1);
    assert!(!version.satellite_version.is_empty());
}

#[tokio::test]
async fn a_bad_secret_is_rejected_with_a_code_the_caller_can_match_on() {
    let url = start().await;
    let client = Client::connect(&url, "wrong")
        .await
        .expect("version is unauthenticated");

    let error = client.status().await.expect_err("should be rejected");

    assert_eq!(
        error.code(),
        Some(arsox_sdk::proto::error::v1::ErrorCode::AuthSecretInvalid)
    );
    // Wrong credentials do not become right by trying again.
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn a_thread_can_be_created_read_listed_and_destroyed() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let mut metadata = BTreeMap::new();
    metadata.insert("tenant".to_owned(), "acme".to_owned());

    let created = client
        .threads()
        .create_with(settings(), Some("sdk-1".to_owned()), metadata.clone())
        .await
        .expect("should create");

    assert!(!created.deduplicated);
    assert_eq!(created.thread.state, i32::from(ThreadState::Idle));

    let repeat = client
        .threads()
        .create_with(settings(), Some("sdk-1".to_owned()), metadata.clone())
        .await
        .expect("should deduplicate");
    assert!(repeat.deduplicated);
    assert_eq!(repeat.thread.thread_id, created.thread.thread_id);

    let listed = client.threads().list(metadata).await.expect("should list");
    assert!(
        listed
            .iter()
            .any(|summary| summary.thread_id == created.thread.thread_id)
    );

    created.handle.destroy().await.expect("should destroy");

    // Gone, not missing. A destroyed thread reports what happened to it, so an
    // application can tell "my record is stale" from "my id is wrong".
    let gone = created.handle.get().await.expect_err("should be gone");
    assert!(gone.is_gone());
    assert!(!gone.is_not_found());
    assert_eq!(
        gone.code(),
        Some(arsox_sdk::proto::error::v1::ErrorCode::ThreadDestroyed)
    );
}

#[tokio::test]
async fn a_second_client_can_attach_to_a_thread_it_did_not_create() {
    // The property a horizontally scaled application depends on: a replica that
    // dies mid-turn costs nothing, because whichever replica comes up next can
    // pick the thread back up from its id alone.
    let url = start().await;

    let creator = Client::connect(&url, SECRET).await.expect("should connect");
    let created = creator
        .threads()
        .create(settings())
        .await
        .expect("should create");

    let other = Client::connect(&url, SECRET).await.expect("should connect");
    let attached = other
        .threads()
        .attach(created.thread.thread_id.clone())
        .await
        .expect("should attach");

    assert_eq!(attached.id(), created.thread.thread_id);

    // An attached handle can do everything a creating handle can.
    let turn = attached
        .start_turn("work through the probe")
        .await
        .expect("should queue a turn");
    assert_eq!(turn.queued().status, i32::from(TurnStatus::Queued));
}

#[tokio::test]
async fn attaching_to_an_unknown_thread_fails_at_attach_rather_than_later() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let error = client
        .threads()
        .attach("not-a-thread")
        .await
        .expect_err("should not attach");

    assert!(error.is_not_found());
}

#[tokio::test]
async fn a_turn_runs_and_its_result_comes_back_through_the_sdk() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    let turn = created
        .handle
        .start_turn("replay the probe")
        .await
        .expect("should queue");

    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");

    assert_eq!(result.status, i32::from(TurnStatus::Completed));

    let tokens = result.tokens.expect("usage should be reported");
    assert!(tokens.total_tokens > 0);
    // Absent rather than zero, all the way out to a consumer.
    assert_eq!(tokens.reasoning_output_tokens, None);
    assert!(result.by_model.len() >= 2, "failover cost stays visible");
}

#[tokio::test]
async fn events_stream_to_a_consumer_over_the_socket() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    let mut events = created
        .handle
        .events()
        .await
        .expect("should open the stream");

    let turn = created
        .handle
        .start_turn("replay the probe")
        .await
        .expect("should queue");
    drop(turn);

    let mut seen = Vec::new();
    let collect = async {
        while let Some(event) = events.next().await {
            let event = event.expect("frames should decode");
            seen.push(event.r#type.clone());
            if event.r#type == "turn.completed" {
                break;
            }
        }
    };

    tokio::time::timeout(Duration::from_secs(30), collect)
        .await
        .expect("should not time out");

    // The whole turn, watched rather than polled.
    assert_eq!(seen.first().map(String::as_str), Some("turn.started"));
    assert_eq!(seen.last().map(String::as_str), Some("turn.completed"));
    assert!(
        seen.iter().any(|name| name == "tool.started"),
        "the tool call should reach a consumer, got {seen:?}"
    );
}

#[tokio::test]
async fn a_cancelled_turn_reports_as_cancelled() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    // Queued behind a running turn, so cancelling is deterministic rather than a
    // race with the runner reaching a terminal state first.
    let running = created
        .handle
        .start_turn("replay the probe")
        .await
        .expect("should queue");
    let queued = created
        .handle
        .start_turn("wait your turn")
        .await
        .expect("should queue");

    let cancelled = queued.cancel().await.expect("should cancel");
    assert_eq!(cancelled.status, i32::from(TurnStatus::Cancelled));

    // The running one is unaffected.
    let result = tokio::time::timeout(Duration::from_secs(30), running.result())
        .await
        .expect("should not time out")
        .expect("should report a result");
    assert_eq!(result.status, i32::from(TurnStatus::Completed));
}
