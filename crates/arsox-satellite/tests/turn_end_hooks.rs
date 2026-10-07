// Copyright © 2026 Jalapeno Labs

//! A thread's turn end hooks, driven through the SDK against a stand-in harness.
//!
//! The hooks themselves are real processes: `/bin/sh` scripts that write into
//! the workspace, fail, or hang, because what is under test is how the runner
//! supervises and reports a program it did not write.

#![cfg(all(feature = "test-util", unix))]

use arsox_satellite::{ServeOptions, assemble};
use arsox_sdk::client::{IncidentQuery, Satellite as Client, ThreadHandle};
use arsox_sdk::proto::common::v1::Duration as ProtoDuration;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::incident::v1::Disposition;
use arsox_sdk::proto::settings::v1::{Budget, EnvVar, ThreadSettings, TurnEndHook};
use arsox_sdk::proto::turn::v1::{
    Stage, StageDisposition, StageOutcome, TurnEndHookOutcome, TurnResult, TurnStatus,
};
use futures_util::StreamExt as _;
use std::time::Duration;

const SECRET: &str = "hooks-test-secret";

const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.221/tool-call.stdout.jsonl"
);

/// Distinguishes scratch directories created in the same clock tick.
static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct Running {
    url: String,
    workspace_root: std::path::PathBuf,
}

async fn start() -> Running {
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
    let directory =
        std::env::temp_dir().join(format!("arsox-hooks-{unique}-{}", uuid::Uuid::now_v7()));
    tokio::fs::create_dir_all(&directory)
        .await
        .expect("should create a scratch directory");

    let assembled = assemble(ServeOptions {
        secret: Some(SECRET.to_owned()),
        allow_insecure: false,
        database_path: directory.join("arsox.db").to_string_lossy().into_owned(),
        workspace_root: directory.to_string_lossy().into_owned(),
        broker_root: directory.join("broker").to_string_lossy().into_owned(),
        max_concurrent_threads: 2,
        port: 0,
        collect_interval: std::time::Duration::from_hours(1),
    })
    .await
    .expect("should assemble");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("should bind");
    let port = listener.local_addr().expect("an address").port();

    tokio::spawn(async move {
        drop(axum::serve(listener, assembled.router).await);
    });

    Running {
        url: format!("http://127.0.0.1:{port}"),
        workspace_root: directory,
    }
}

/// A hook that runs `script` through `/bin/sh`.
fn shell(name: &str, script: &str) -> TurnEndHook {
    TurnEndHook {
        name: name.to_owned(),
        argv: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        timeout: None,
    }
}

fn settings(hooks: Vec<TurnEndHook>) -> ThreadSettings {
    ThreadSettings {
        idle_ttl: Some(ProtoDuration {
            seconds: 3600,
            nanos: 0,
        }),
        budget: Some(Budget::default()),
        turn_end_hooks: hooks,
        env: vec![EnvVar {
            key: "EXPORT_TOKEN".to_owned(),
            value: Some(arsox_sdk::proto::common::v1::Secret {
                value: Some("hook-sees-this-secret-value".to_owned()),
                display: None,
            }),
            is_secret: None,
        }],
        ..ThreadSettings::default()
    }
}

async fn thread(running: &Running, hooks: Vec<TurnEndHook>) -> ThreadHandle {
    Client::connect(&running.url, SECRET)
        .await
        .expect("should connect")
        .threads()
        .create(settings(hooks))
        .await
        .expect("should create a thread")
        .handle
}

async fn finish(thread: &ThreadHandle, prompt: &str) -> TurnResult {
    let turn = thread.start_turn(prompt).await.expect("should queue");

    tokio::time::timeout(Duration::from_secs(30), turn.result())
        .await
        .expect("should not time out")
        .expect("should report a result")
}

fn stage(result: &TurnResult, stage: Stage) -> &StageOutcome {
    result
        .stages
        .iter()
        .find(|outcome| outcome.stage == i32::from(stage))
        .unwrap_or_else(|| panic!("{stage:?} is reported: {:?}", result.stages))
}

#[tokio::test]
async fn hooks_run_in_order_before_the_scan_so_what_they_write_is_announced() {
    let running = start().await;
    let thread = thread(
        &running,
        vec![
            shell(
                "first",
                "mkdir -p artifacts && echo one > artifacts/order.txt",
            ),
            shell(
                "second",
                "echo two >> artifacts/order.txt; pwd > artifacts/where.txt",
            ),
        ],
    )
    .await;

    let result = finish(&thread, "do the work").await;
    assert_eq!(result.status, i32::from(TurnStatus::Completed));

    let names: Vec<(&str, i32)> = result
        .turn_end_hooks
        .iter()
        .map(|hook| (hook.name.as_str(), hook.outcome))
        .collect();
    let succeeded = i32::from(TurnEndHookOutcome::Succeeded);
    assert_eq!(names, [("first", succeeded), ("second", succeeded)]);
    assert_eq!(result.turn_end_hooks[0].exit_code, Some(0));

    let workspace = running.workspace_root.join(thread.id());
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifacts/order.txt")).expect("written"),
        "one\ntwo\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("artifacts/where.txt"))
            .expect("written")
            .trim(),
        std::fs::canonicalize(&workspace)
            .expect("exists")
            .to_string_lossy(),
        "a hook runs from the thread's workspace root"
    );

    // The scan ran after the hooks, so the same turn announced their files.
    let announced: Vec<&str> = result
        .artifacts
        .iter()
        .map(|artifact| artifact.path.as_str())
        .collect();
    assert_eq!(announced, ["order.txt", "where.txt"]);

    assert_eq!(
        stage(&result, Stage::TurnEndHooks).disposition,
        i32::from(StageDisposition::Ran)
    );

    // Hooks report before artifacts, in the order the closing steps ran.
    let hooks_at = result
        .stages
        .iter()
        .position(|outcome| outcome.stage == i32::from(Stage::TurnEndHooks));
    let artifacts_at = result
        .stages
        .iter()
        .position(|outcome| outcome.stage == i32::from(Stage::Artifacts));
    assert!(hooks_at < artifacts_at);

    // One `hook.finished` per hook, on the stream, in order.
    let mut events = thread.events_from(0).await.expect("should stream");
    let mut finished = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), events.next())
        .await
        .expect("the stream should not stall")
    {
        let event = event.expect("an event");
        if let Some(Payload::TurnEndHookFinished(hook)) = event.payload {
            assert_eq!(event.r#type, "hook.finished");
            finished.push(hook.result.expect("a result").name);
        }
        if event.r#type == "turn.completed" {
            break;
        }
    }
    assert_eq!(finished, ["first", "second"]);
}

#[tokio::test]
async fn a_failing_hook_is_a_degraded_incident_and_the_turn_stands() {
    let running = start().await;
    let thread = thread(
        &running,
        vec![
            shell("leaks", "echo \"token is $EXPORT_TOKEN\"; exit 7"),
            TurnEndHook {
                timeout: Some(ProtoDuration {
                    seconds: 0,
                    nanos: 300_000_000,
                }),
                ..shell("hangs", "echo started; sleep 30")
            },
            TurnEndHook {
                name: "missing".to_owned(),
                argv: vec!["/no/such/exporter".to_owned()],
                timeout: None,
            },
            shell(
                "after",
                "mkdir -p artifacts && echo ran > artifacts/after.txt",
            ),
        ],
    )
    .await;

    let result = finish(&thread, "do the work").await;
    assert_eq!(
        result.status,
        i32::from(TurnStatus::Completed),
        "the turn stands"
    );

    let outcomes: Vec<(i32, Option<i32>)> = result
        .turn_end_hooks
        .iter()
        .map(|hook| (hook.outcome, hook.exit_code))
        .collect();
    assert_eq!(
        outcomes,
        [
            (i32::from(TurnEndHookOutcome::Failed), Some(7)),
            (i32::from(TurnEndHookOutcome::TimedOut), None),
            (i32::from(TurnEndHookOutcome::NotLaunched), None),
            (i32::from(TurnEndHookOutcome::Succeeded), Some(0)),
        ]
    );

    // The hook sees the thread's declared variables, and what it printed is
    // masked like everything else the turn produces.
    let leaked = &result.turn_end_hooks[0].output_tail;
    assert!(leaked.contains("token is "), "{leaked}");
    assert!(!leaked.contains("hook-sees-this-secret-value"), "{leaked}");

    let hooks = stage(&result, Stage::TurnEndHooks);
    assert_eq!(hooks.disposition, i32::from(StageDisposition::Failed));
    assert_eq!(
        hooks.reason.as_deref(),
        Some("3 of 4 turn end hooks did not succeed")
    );

    // A failure stopped neither the hooks after it nor the scan.
    assert_eq!(result.artifacts.len(), 1);

    let incidents = thread
        .incidents(IncidentQuery::default())
        .await
        .expect("should list incidents");
    let failed: Vec<_> = incidents
        .iter()
        .filter(|incident| incident.code == i32::from(ErrorCode::TurnEndHookFailed))
        .collect();
    assert_eq!(failed.len(), 3);
    assert!(failed.iter().all(
        |incident| incident.disposition == i32::from(Disposition::Degraded) && !incident.retryable
    ));
}

#[tokio::test]
async fn a_thread_without_hooks_reports_the_stage_skipped() {
    let running = start().await;
    let thread = thread(&running, Vec::new()).await;

    let result = finish(&thread, "do the work").await;

    let hooks = stage(&result, Stage::TurnEndHooks);
    assert_eq!(hooks.disposition, i32::from(StageDisposition::Skipped));
    assert!(result.turn_end_hooks.is_empty());

    // Every stage the contract names is reported, run or not.
    assert_eq!(
        stage(&result, Stage::PullRequestWatch).disposition,
        i32::from(StageDisposition::Skipped)
    );
}

#[tokio::test]
async fn a_cancelled_turn_runs_no_hooks() {
    let running = start().await;
    let thread = thread(&running, vec![shell("marker", "touch hook-ran")]).await;

    // Silent long enough to be cancelled mid-run, then enough lines for the
    // runner's periodic check to notice.
    let turn = thread
        .start_turn("[[hang=1500]] [[unrecognized=60]]")
        .await
        .expect("should queue");
    tokio::time::sleep(Duration::from_millis(400)).await;
    turn.cancel().await.expect("should cancel");

    // Read from the stream rather than polled: a cancelled turn reads as
    // terminal the moment it is cancelled, before the runner has recorded what
    // its closing steps did, and `turn.completed` carries exactly that.
    let mut events = thread.events_from(0).await.expect("should stream");
    let result = loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.next())
            .await
            .expect("the stream should not stall")
            .expect("the stream stays open")
            .expect("an event");

        if let Some(Payload::TurnCompleted(completed)) = event.payload {
            break completed.result.expect("a result");
        }
    };

    assert_eq!(result.status, i32::from(TurnStatus::Cancelled));
    assert!(result.turn_end_hooks.is_empty());
    assert_eq!(
        stage(&result, Stage::TurnEndHooks).disposition,
        i32::from(StageDisposition::Skipped)
    );
    assert!(
        !running
            .workspace_root
            .join(thread.id())
            .join("hook-ran")
            .exists()
    );
}

#[tokio::test]
async fn a_hook_that_could_never_run_is_refused_at_creation() {
    let running = start().await;
    let client = Client::connect(&running.url, SECRET)
        .await
        .expect("should connect");

    let refused = client
        .threads()
        .create(settings(vec![TurnEndHook {
            name: "too-slow".to_owned(),
            argv: vec!["/bin/true".to_owned()],
            timeout: Some(ProtoDuration {
                seconds: 2 * 3600,
                nanos: 0,
            }),
        }]))
        .await
        .expect_err("a timeout past an hour is refused");

    assert_eq!(refused.code(), Some(ErrorCode::RequestFieldInvalid));
    assert!(
        refused.to_string().contains("settings.turn_end_hooks"),
        "{refused}"
    );
}
