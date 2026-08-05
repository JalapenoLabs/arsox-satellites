// Copyright © 2026 Jalapeno Labs

//! The turn runner, driven end to end against a stand-in harness.
//!
//! These exercise the loop the unit tests cannot reach: a real process is
//! spawned, its stdout is streamed through the mapper, and the canonical events
//! land in the database. What is faked is the model, not the plumbing.

#![cfg(feature = "test-util")]

use arsox_satellite::harness::runner::Runner;
use arsox_satellite::store::{NewThread, NewTurn, Store};
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use arsox_sdk::proto::turn::v1::TurnStatus;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The recorded transcript the stand-in replays.
const TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/fixtures/claude/tool-call.jsonl"
);

struct Harness {
    store: Store,
    _workspace: tempdir::TempDir,
}

/// A minimal scratch directory, since the runner creates a workspace per thread.
mod tempdir {
    pub struct TempDir(std::path::PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("arsox-{label}-{}", uuid_like()));
            std::fs::create_dir_all(&path).expect("should create a scratch directory");
            Self(path)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default()
    }
}

async fn start(prompt: &str) -> (Harness, String, String) {
    // Set once for the process, and identical for every caller, so there is no
    // value here for one test to change out from under another. Anything that
    // does vary per run rides on the prompt instead.
    static CONFIGURE: std::sync::Once = std::sync::Once::new();
    CONFIGURE.call_once(|| {
        // SAFETY: runs exactly once, before any test spawns a child, and writes
        // values that never change for the lifetime of the process.
        unsafe {
            std::env::set_var("ARSOX_CLAUDE_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_TRANSCRIPT", TRANSCRIPT);
        }
    });

    let store = Store::open_in_memory().await.expect("should open");
    let workspace = tempdir::TempDir::new("workspace");

    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings::default(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create a thread")
        .thread;

    let turn = store
        .create_turn(NewTurn {
            thread_id: thread.thread_id.clone(),
            prompt: prompt.to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
        .expect("should queue a turn")
        .turn;

    let runner = Runner::new(
        store.clone(),
        workspace.path().to_path_buf(),
        Arc::new(tokio::sync::Notify::new()),
        1,
    );
    tokio::spawn(runner.dispatch());

    (
        Harness {
            store,
            _workspace: workspace,
        },
        thread.thread_id,
        turn.turn_id,
    )
}

/// Waits for a turn to reach a terminal state.
async fn settle(store: &Store, thread_id: &str, turn_id: &str) -> TurnStatus {
    for _attempt in 0..100 {
        let (turn, _result) = store.turn(thread_id, turn_id).await.expect("should read");
        let status = TurnStatus::try_from(turn.status).unwrap_or(TurnStatus::Unspecified);

        if !matches!(status, TurnStatus::Queued | TurnStatus::Running) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("the turn never reached a terminal state");
}

#[tokio::test]
async fn a_queued_turn_runs_and_lands_its_events_in_the_log() {
    let (harness, thread_id, turn_id) = start("run the probe").await;

    let status = settle(&harness.store, &thread_id, &turn_id).await;
    assert_eq!(
        status,
        TurnStatus::Completed,
        "the transcript is a clean run"
    );

    let events = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay");

    let names: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();

    // The mapper's four canonical events, bracketed by the runner's own two.
    // This is the loop closing: a real process produced these.
    assert_eq!(
        names,
        vec![
            "turn.started",
            "rate_limit.reported",
            "tool.started",
            "tool.completed",
            "agent.message",
            "turn.completed",
        ]
    );

    // Gapless and monotonic, assigned by the log rather than the mapper.
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3, 4, 5, 6]);

    assert!(
        events.iter().all(|event| event.turn_id.is_some()),
        "every event in a turn is attributable to it"
    );
}

#[tokio::test]
async fn the_result_carries_usage_and_marks_unimplemented_stages_skipped() {
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let (_turn, result) = harness
        .store
        .turn(&thread_id, &turn_id)
        .await
        .expect("should read");
    let result = result.expect("a finished turn carries a result");

    let tokens = result.tokens.expect("usage should be recorded");
    assert!(tokens.total_tokens > 0);
    // Absent rather than zero: this harness folds reasoning into output.
    assert_eq!(tokens.reasoning_output_tokens, None);

    assert!(
        result.by_model.len() >= 2,
        "failover cost stays visible per model"
    );

    // Stages that do not exist yet report as skipped rather than being omitted,
    // so "not run" never reads as "found nothing".
    let skipped = result
        .stages
        .iter()
        .filter(|stage| {
            stage.disposition == i32::from(arsox_sdk::proto::turn::v1::StageDisposition::Skipped)
        })
        .count();
    assert!(
        skipped >= 5,
        "unimplemented stages are reported, not hidden"
    );
}

#[tokio::test]
async fn the_harness_session_id_is_recorded_on_the_thread() {
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let thread = harness.store.thread(&thread_id).await.expect("should read");

    // The join key between an Arsox thread and the harness transcripts on disk.
    assert_eq!(
        thread.harness_session_id.as_deref(),
        Some("0199c0de-1111-7000-8000-000000000001")
    );
}

#[tokio::test]
async fn a_thread_returns_to_idle_once_its_turn_finishes() {
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let thread = harness.store.thread(&thread_id).await.expect("should read");

    assert_eq!(
        thread.state,
        i32::from(arsox_sdk::proto::thread::v1::ThreadState::Idle)
    );
    assert_eq!(thread.queue_depth, 0);
    assert_eq!(thread.current_turn_id, None);
}

#[tokio::test]
async fn a_harness_that_exits_nonzero_fails_the_turn_with_a_reason() {
    let (harness, thread_id, turn_id) = start("run the probe [[exit=3]]").await;

    let status = settle(&harness.store, &thread_id, &turn_id).await;
    assert_eq!(status, TurnStatus::Failed);

    let (_turn, result) = harness
        .store
        .turn(&thread_id, &turn_id)
        .await
        .expect("should read");
    let result = result.expect("even a failed turn carries a result");
    let error = result.error.expect("a failed turn says why");

    assert_eq!(
        error.code,
        i32::from(arsox_sdk::proto::error::v1::ErrorCode::HarnessCrashed)
    );
    // A crashed harness is worth retrying; a malformed request is not.
    assert!(error.retryable);
}

#[tokio::test]
async fn a_harness_that_stops_before_reporting_a_result_is_a_failure_not_a_success() {
    // A clean exit with no result line means the harness ended without saying
    // what it did. Reporting that as success would be the silent failure the
    // whole incident system exists to prevent.
    let (harness, thread_id, turn_id) = start("run the probe [[truncate=3]]").await;

    let status = settle(&harness.store, &thread_id, &turn_id).await;
    assert_eq!(status, TurnStatus::Failed);

    // The events it did emit are still in the log rather than being discarded
    // along with the turn.
    let events = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay");
    assert!(
        events.len() >= 2,
        "partial output is kept, got {}",
        events.len()
    );
}

#[tokio::test]
async fn a_turn_interrupted_by_a_restart_is_marked_rather_than_left_running() {
    let store = Store::open_in_memory().await.expect("should open");
    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings::default(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;
    store
        .create_turn(NewTurn {
            thread_id: thread.thread_id.clone(),
            prompt: "work".to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
        .expect("should queue");

    let claimed = store
        .claim_next_turn()
        .await
        .expect("should claim")
        .expect("there is a queued turn");

    // Nothing is driving it now, which is exactly the state a restart leaves
    // behind. Left RUNNING it would block its thread forever.
    let interrupted = store.mark_interrupted_turns().await.expect("should sweep");
    assert_eq!(interrupted, vec![claimed.turn.turn_id.clone()]);

    let (turn, _result) = store
        .turn(&thread.thread_id, &claimed.turn.turn_id)
        .await
        .expect("should read");
    assert_eq!(turn.status, i32::from(TurnStatus::Interrupted));
}

#[tokio::test]
async fn only_one_turn_per_thread_is_ever_claimed() {
    // A thread is a conversation and conversations are sequential. The claim
    // query enforces it, so it holds even if a second runner appears.
    let store = Store::open_in_memory().await.expect("should open");
    let thread = store
        .create_thread(NewThread {
            settings: ThreadSettings::default(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    for _queued in 0..3 {
        store
            .create_turn(NewTurn {
                thread_id: thread.thread_id.clone(),
                prompt: "work".to_owned(),
                metadata: BTreeMap::new(),
                idempotency_key: None,
                satellite_initiated: false,
                triggered_by_turn_id: None,
            })
            .await
            .expect("should queue");
    }

    assert!(store.claim_next_turn().await.expect("first").is_some());
    assert!(
        store.claim_next_turn().await.expect("second").is_none(),
        "the thread already has a turn running"
    );
}

#[tokio::test]
async fn tool_calls_survive_the_round_trip_into_the_log() {
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let events = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay");

    let started = events
        .iter()
        .find_map(|event| match event.payload.as_ref() {
            Some(Payload::ToolStarted(started)) => Some(started),
            _other => None,
        })
        .expect("the transcript contains a tool call");

    assert_eq!(started.tool_name, "Bash");
    // The command survives as structured input rather than being flattened,
    // which is what will let the exec broker inspect it.
    assert!(
        started
            .input
            .as_ref()
            .is_some_and(|input| input.fields.contains_key("command"))
    );
}
