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
use arsox_sdk::client::{IncidentQuery, Satellite as Client};
use arsox_sdk::proto::common::v1::{Duration as ProtoDuration, Secret};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::settings::v1::{
    Budget, EnvVar, GithubIntegration, LlmAuth, ModelEndpoint, Redaction, RedactionMode,
    ThreadSettings, llm_auth::Credential,
};
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
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);

/// The same, in the Codex vocabulary.
const CODEX_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/codex/0.147.0/tool-call.stdout.jsonl"
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
            // Pointed somewhere deliberate rather than left to whatever `codex`
            // resolves to on the machine running this. The capabilities endpoint
            // lists Codex only when its binary answered, so leaving this unset
            // would make the assertion below depend on what the developer
            // happens to have installed.
            std::env::set_var("ARSOX_CODEX_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_CODEX_TRANSCRIPT", CODEX_TRANSCRIPT);
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
async fn the_harness_endpoint_reports_what_this_satellite_offers() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let harness = client.harness().await.expect("should report harnesses");

    // Both binaries answered on this satellite, so both are offered, and the
    // endpoint says which one a thread that names none gets.
    assert_eq!(harness.default_harness, i32::from(Harness::Claude));
    assert_eq!(harness.harnesses.len(), 2);

    let claude = &harness.harnesses[0];
    assert_eq!(claude.harness, i32::from(Harness::Claude));
    assert!(claude.supports_mcp);
    assert!(claude.reports_cache_tokens);
    // The stand-in harness replays a transcript when asked for its version,
    // and that JSON must not have been mistaken for one.
    assert!(!claude.cli_version.starts_with('{'));

    // The point of the endpoint: the two harnesses differ, and a consumer is
    // told up front rather than discovering it by absence three turns into a
    // run.
    let codex = &harness.harnesses[1];
    assert_eq!(codex.harness, i32::from(Harness::Codex));
    assert!(codex.supports_thinking_events);
    assert!(!codex.supports_subagents);
    assert!(!codex.supports_native_plan_mode);
    assert!(!codex.cli_version.starts_with('{'));
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
async fn a_declared_variable_that_would_undo_the_scrub_is_refused_at_creation() {
    // Declared variables are set on top of a scrubbed environment, so a key
    // named like a satellite setting or a provider credential would hand an
    // agent back exactly what the scrub exists to withhold. The caller learns
    // that here, while it is still listening, rather than mid-turn.
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let declared = |key: &str| ThreadSettings {
        env: vec![EnvVar {
            key: key.to_owned(),
            value: Some(Secret {
                value: Some("a-value-no-error-should-carry".to_owned()),
                display: None,
            }),
            is_secret: None,
        }],
        ..settings()
    };

    for reintroduced in ["ARSOX_SECRET", "ANTHROPIC_API_KEY"] {
        let error = client
            .threads()
            .create(declared(reintroduced))
            .await
            .expect_err("should be refused");

        assert_eq!(
            error.code(),
            Some(arsox_sdk::proto::error::v1::ErrorCode::RequestFieldInvalid),
            "{reintroduced}"
        );

        // The key is named, so the caller can fix it without guessing which of
        // its variables was the problem.
        let said = error.to_string();
        assert!(said.contains(reintroduced), "{said}");
        // And the value never is. Half of these are credentials by definition,
        // and an error body is a log line somewhere.
        assert!(!said.contains("a-value-no-error-should-carry"), "{said}");
    }

    // An ordinary variable is untouched by the rule.
    client
        .threads()
        .create(declared("NPM_TOKEN"))
        .await
        .expect("an ordinary declared variable is fine");
}

#[tokio::test]
async fn status_reports_the_threads_it_holds_and_the_disk_they_sit_on() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    let status = client.status().await.expect("should report status");

    assert_eq!(status.max_concurrent_threads, 2);
    assert_eq!(status.running_threads, 0, "nothing has been queued yet");
    assert!(
        status
            .threads
            .iter()
            .any(|summary| summary.thread_id == created.thread.thread_id),
        "a live thread is one the satellite is holding"
    );

    let disk = status
        .disk
        .expect("a real volume answers what it holds and what it has left");
    assert!(disk.available_bytes > 0, "a writable volume has room left");
    // The satellite's own database sits under this test's workspace root, so the
    // walk finds bytes whether or not a thread has provisioned anything.
    assert!(disk.workspace_bytes > 0);
    assert!(disk.database_bytes > 0);
    // No satellite-wide ceiling is configured, and absent says exactly that
    // rather than claiming a ceiling of zero.
    assert_eq!(disk.aggregate_quota_bytes, None);

    created.handle.destroy().await.expect("should destroy");

    let after = client.status().await.expect("should report status");
    assert!(
        !after
            .threads
            .iter()
            .any(|summary| summary.thread_id == created.thread.thread_id),
        "a destroyed thread is not one the satellite still holds"
    );
}

#[tokio::test]
async fn status_counts_a_turn_in_flight_and_stops_counting_it_afterwards() {
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    // The stand-in harness replays its transcript and then holds the process
    // open, so the turn is still in flight when status is polled rather than
    // finishing before the first poll lands.
    let turn = created
        .handle
        .start_turn("replay the probe [[stall=3000]]")
        .await
        .expect("should queue");

    let running = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let status = client.status().await.expect("should report status");
            if status.running_threads > 0 {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the runner should claim the turn");

    assert_eq!(running.running_threads, 1);

    let summary = running
        .threads
        .iter()
        .find(|summary| summary.thread_id == created.thread.thread_id)
        .expect("the running thread is listed");
    assert_eq!(summary.state, i32::from(ThreadState::Running));
    assert!(
        summary.current_turn_id.is_some(),
        "a running thread names the turn it is running"
    );

    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");
    assert_eq!(result.status, i32::from(TurnStatus::Completed));

    let settled = client.status().await.expect("should report status");
    assert_eq!(
        settled.running_threads, 0,
        "a finished turn frees its slot against the cap"
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
async fn incidents_are_listed_through_the_sdk_per_thread_and_per_satellite() {
    // The question a host application asks after a bad night, from a consumer's
    // seat: what went wrong on this thread, and what went wrong anywhere.
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings())
        .await
        .expect("should create");

    // The stand-in stops after three lines, which is a harness that exited
    // without saying what it did. The turn starts it once more and then gives
    // up, so this thread carries the recovery that was attempted and the failure
    // it ended on.
    let turn = created
        .handle
        .start_turn("replay the probe [[truncate=3]]")
        .await
        .expect("should queue");

    let result = tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result");

    assert_eq!(result.status, i32::from(TurnStatus::Failed));

    let counts = result.incident_counts.expect("a report carries its counts");
    assert_eq!(
        (counts.recovered, counts.fatal),
        (1, 1),
        "the counts ride along, so the common case needs no query"
    );

    let listed = created
        .handle
        .incidents(IncidentQuery::default())
        .await
        .expect("should list this thread's incidents");

    // Oldest first, which is the order they happened in: the restart, then the
    // turn giving up.
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .all(|incident| incident.code == i32::from(ErrorCode::HarnessCrashed))
    );
    assert_eq!(listed[0].disposition, i32::from(Disposition::Recovered));
    assert_eq!(listed[1].disposition, i32::from(Disposition::Fatal));
    assert_eq!(listed[1].turn_id.as_deref(), Some(turn.id()));
    // The sequence is what lets a query result be located in the stream the same
    // incident was emitted on.
    assert!(listed[1].sequence.is_some());

    // A filter narrows. A disposition nothing carries returns nothing rather
    // than falling back to everything.
    let blocked = created
        .handle
        .incidents(IncidentQuery {
            dispositions: vec![Disposition::Blocked],
            ..IncidentQuery::default()
        })
        .await
        .expect("should filter");
    assert!(blocked.is_empty());

    let fatal = created
        .handle
        .incidents(IncidentQuery {
            dispositions: vec![Disposition::Fatal],
            turn_ids: vec![turn.id().to_owned()],
            ..IncidentQuery::default()
        })
        .await
        .expect("should filter");
    assert_eq!(fatal.len(), 1);

    // The satellite-wide listing finds the same incident without being told
    // which thread to look at.
    let fleet = client
        .incidents(IncidentQuery {
            codes: vec![ErrorCode::HarnessCrashed],
            ..IncidentQuery::default()
        })
        .await
        .expect("should list every thread's incidents");
    assert!(
        fleet.iter().any(
            |incident| incident.thread_id.as_deref() == Some(created.thread.thread_id.as_str())
        )
    );

    // Destroying the thread takes its workspace, its turns, and its events. The
    // evidence stays, which is the whole reason incidents are not ephemeral.
    created.handle.destroy().await.expect("should destroy");

    let after_teardown = client
        .incidents(IncidentQuery {
            thread_ids: vec![created.thread.thread_id.clone()],
            ..IncidentQuery::default()
        })
        .await
        .expect("a tombstoned thread is not an error to filter on");

    assert_eq!(
        after_teardown.len(),
        2,
        "incidents outlive the thread they describe"
    );
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

/// A thread's settings carrying one of every credential an endpoint could echo.
///
/// No repos, deliberately: a repo would send the thread through provisioning
/// against a URL nothing serves, and these tests are about what comes back
/// rather than about what a clone does.
fn settings_with_credentials() -> ThreadSettings {
    let plaintext = |value: &str| {
        Some(Secret {
            value: Some(value.to_owned()),
            display: None,
        })
    };

    ThreadSettings {
        env: vec![EnvVar {
            key: "DEPLOY_TOKEN".to_owned(),
            value: plaintext(ENV_PLAINTEXT),
            is_secret: None,
        }],
        github: Some(GithubIntegration {
            token: plaintext(GITHUB_PLAINTEXT),
        }),
        models: vec![ModelEndpoint {
            name: "primary".to_owned(),
            model: "claude-opus-5[1m]".to_owned(),
            auth: Some(LlmAuth {
                credential: Some(Credential::ApiKey(Secret {
                    value: Some(MODEL_PLAINTEXT.to_owned()),
                    display: None,
                })),
                // Undeclared, so the satellite infers presentation as before.
                presentation: None,
            }),
            ..ModelEndpoint::default()
        }],
        // Postfix, because a mask that can still tell two credentials apart is
        // the interesting case: it proves the thread's own settings chose the
        // rendering rather than a default applied everywhere.
        redaction: Some(Redaction {
            mode: RedactionMode::PostfixShown.into(),
            ..Redaction::default()
        }),
        ..settings()
    }
}

/// The credentials `settings_with_credentials` carries, and none of them short
/// enough for the anonymous rule to swallow the reveal.
const ENV_PLAINTEXT: &str = "declared-env-credential";
const GITHUB_PLAINTEXT: &str = "ghp_the_real_github_token";
const MODEL_PLAINTEXT: &str = "sk-ant-the-real-api-key";

/// Everything the thread was created with, rendered for scanning.
///
/// Rendered whole rather than field by field, because the failure worth
/// catching is a credential nothing thought to assert on.
fn rendered(thread: &arsox_sdk::proto::thread::v1::Thread) -> String {
    format!("{:?}", thread.settings)
}

/// Asserts a thread carries no plaintext credential, whatever endpoint returned
/// it.
fn carries_no_plaintext(thread: &arsox_sdk::proto::thread::v1::Thread, endpoint: &str) {
    let settings = rendered(thread);

    for plaintext in [ENV_PLAINTEXT, GITHUB_PLAINTEXT, MODEL_PLAINTEXT] {
        assert!(
            !settings.contains(plaintext),
            "{endpoint} echoed {plaintext}: {settings}"
        );
    }
}

#[tokio::test]
async fn creating_a_thread_hands_its_credentials_back_masked() {
    // The contract says the satellite never populates Secret.value on a
    // response, at any endpoint, at any authentication level. This is the
    // endpoint that would otherwise hand every credential straight back to the
    // caller that just sent it.
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings_with_credentials())
        .await
        .expect("should create");

    carries_no_plaintext(&created.thread, "create");

    let settings = created
        .thread
        .settings
        .expect("a thread carries its settings");

    // Masked, and readable enough to tell two credentials apart, which is what
    // `display` exists for.
    let token = settings
        .github
        .and_then(|github| github.token)
        .expect("the token survives as a rendering");
    assert_eq!(token.value, None);
    assert_eq!(token.display.as_deref(), Some("******token"));

    // A declared variable is a credential unless the caller said otherwise, so
    // absent means secret here exactly as it does everywhere else.
    let declared = settings.env[0].value.clone().expect("should be carried");
    assert_eq!(declared.value, None);
    assert_eq!(declared.display.as_deref(), Some("******tial"));

    // And the endpoint answered with everything else untouched, so a caller can
    // still read back the configuration it asked for.
    assert_eq!(settings.models[0].model, "claude-opus-5[1m]");
}

#[tokio::test]
async fn every_endpoint_that_returns_a_thread_masks_its_credentials() {
    // One scrub at the response boundary rather than one per handler. Get is the
    // endpoint the issue named; pause, resume, and destroy carry the same whole
    // thread and would each be their own leak.
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings_with_credentials())
        .await
        .expect("should create");
    let handle = created.handle;

    carries_no_plaintext(&handle.get().await.expect("should read"), "get");
    carries_no_plaintext(&handle.pause().await.expect("should pause"), "pause");
    carries_no_plaintext(&handle.resume().await.expect("should resume"), "resume");
    carries_no_plaintext(&handle.destroy().await.expect("should destroy"), "destroy");
}

#[tokio::test]
async fn no_credential_appears_in_the_bytes_a_response_is_made_of() {
    // Asserting on what the SDK decoded proves the fields were masked. This
    // proves the encoding carries nothing else: a plaintext left in an unread
    // field, or in a field this SDK version has no name for, would still be a
    // credential in a proxy log.
    let url = start().await;
    let client = Client::connect(&url, SECRET).await.expect("should connect");

    let created = client
        .threads()
        .create(settings_with_credentials())
        .await
        .expect("should create");

    let http = reqwest::Client::new();
    let thread_id = created.thread.thread_id.clone();

    for path in [
        format!("/v1/threads/{thread_id}"),
        // The listing too, which returns summaries rather than threads. It is
        // the stronger answer, and it is worth proving rather than assuming.
        "/v1/threads".to_owned(),
    ] {
        let bytes = http
            .get(format!("{url}{path}"))
            .header("Authorization", format!("Bearer {SECRET}"))
            .header("Accept", "application/protobuf")
            .send()
            .await
            .expect("should answer")
            .bytes()
            .await
            .expect("should read");

        let wire = String::from_utf8_lossy(&bytes);

        for plaintext in [ENV_PLAINTEXT, GITHUB_PLAINTEXT, MODEL_PLAINTEXT] {
            assert!(
                !wire.contains(plaintext),
                "{path} put {plaintext} on the wire"
            );
        }
    }
}
