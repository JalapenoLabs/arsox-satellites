// Copyright © 2026 Jalapeno Labs

//! The turn runner, driven end to end against a stand-in harness.
//!
//! These exercise the loop the unit tests cannot reach: a real process is
//! spawned, its stdout is streamed through the mapper, and the canonical events
//! land in the database. What is faked is the model, not the plumbing.

#![cfg(feature = "test-util")]

use arsox_satellite::collector::Collector;
use arsox_satellite::harness::runner::Runner;
use arsox_satellite::store::{NewThread, NewTurn, ProvisionOutcome, Store};
use arsox_satellite::stream::EventBus;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::incident::v1::Incident;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use arsox_sdk::proto::turn::v1::TurnStatus;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

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

struct Harness {
    store: Store,
    workspace: tempdir::TempDir,
    collector: Arc<Collector>,
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

    /// Distinguishes directories created in the same clock tick.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A name no other scratch directory in this process will take.
    ///
    /// A timestamp alone is not enough: Windows clocks tick at 100 nanoseconds
    /// and these tests run in parallel, so two can land on the same value and
    /// then share a database file.
    fn uuid_like() -> u128 {
        let unique = u128::from(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();

        now.wrapping_mul(1_000).wrapping_add(unique)
    }
}

async fn start(prompt: &str) -> (Harness, String, String) {
    start_with(prompt, ThreadSettings::default()).await
}

/// Same, with the thread's settings chosen by the caller.
async fn start_with(prompt: &str, settings: ThreadSettings) -> (Harness, String, String) {
    start_prepared(prompt, settings, |_workspace, _thread_id| {}).await
}

/// Same, with the thread's workspace filled in before its first turn is queued.
///
/// The turn is queued last on purpose. The runner claims work the moment it
/// exists, so anything a test needs on disk, such as the checkout a checker runs
/// in, has to be there before the queue moves rather than racing it.
async fn start_prepared(
    prompt: &str,
    settings: ThreadSettings,
    prepare: impl FnOnce(&std::path::Path, &str),
) -> (Harness, String, String) {
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
            settings,
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create a thread")
        .thread;

    prepare(workspace.path(), &thread.thread_id);

    // A thread that declared repos opens PROVISIONING, and the claim query
    // refuses to hand out work from one. No `Provisioner` runs here because
    // these tests fill the workspace themselves, so the thread is released the
    // same way provisioning would have released it. A no-op for every other
    // thread, which was IDLE from the moment it was created.
    store
        .finish_provisioning(&thread.thread_id, ProvisionOutcome::Ready)
        .await
        .expect("should release the thread");

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

    let collector = Arc::new(Collector::new(
        store.clone(),
        workspace.path().to_path_buf(),
        EventBus::new(),
    ));

    let runner = Runner::new(
        store.clone(),
        workspace.path().to_path_buf(),
        Arc::new(tokio::sync::Notify::new()),
        1,
        Arc::clone(&collector),
        arsox_satellite::proxy::LlmProxy::start()
            .await
            .expect("should start the proxy"),
    );
    tokio::spawn(runner.dispatch());

    (
        Harness {
            store,
            workspace,
            collector,
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
    let settled = store
        .settle_interrupted_turns()
        .await
        .expect("should settle");
    // The thread did not opt into automatic resumption, so it waits.
    assert_eq!(settled.left_interrupted, vec![claimed.turn.turn_id.clone()]);
    assert!(settled.resumed.is_empty());

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

#[tokio::test]
async fn a_thread_marked_delete_on_complete_takes_its_workspace_with_it() {
    // The setting exists so an application running one-shot jobs does not have
    // to wait out an idle TTL, or remember to destroy anything.
    let (harness, thread_id, _turn_id) = start_with(
        "say hello",
        ThreadSettings {
            delete_on_complete: true,
            ..Default::default()
        },
    )
    .await;

    // Waited on directly rather than by settling the turn first. The turn goes
    // with the thread, so polling it races collection. Its result still reached
    // the stream, which is where a caller watching a one-shot job reads it.
    let collected = await_collected(&harness.store, &thread_id).await;
    assert!(collected, "the thread should have been collected");

    let error = harness
        .store
        .thread(&thread_id)
        .await
        .expect_err("a collected thread is not readable");
    assert_eq!(error.code(), ErrorCode::ThreadDestroyed);

    assert!(
        !harness.workspace.path().join(&thread_id).exists(),
        "the workspace subtree goes with the thread"
    );
}

#[tokio::test]
async fn an_expired_thread_is_swept_and_its_workspace_removed() {
    // A TTL nothing acts on is a promise the satellite does not keep.
    let (harness, _thread_id, _turn_id) = start("say hello").await;

    let expiring = harness
        .store
        .create_thread(NewThread {
            settings: ThreadSettings {
                idle_ttl: Some(arsox_sdk::proto::common::v1::Duration {
                    seconds: 0,
                    nanos: 1,
                }),
                ..Default::default()
            },
            metadata: BTreeMap::new(),
            idempotency_key: None,
        })
        .await
        .expect("should create")
        .thread;

    // Stand in for the workspace the runner would have created.
    let directory = harness.workspace.path().join(&expiring.thread_id);
    std::fs::create_dir_all(directory.join("repos/api")).expect("should create");
    std::fs::write(directory.join("repos/api/work.txt"), b"in progress").expect("should write");

    let swept = harness.collector.sweep_once().await;

    assert_eq!(swept, 1, "the expired thread should have been swept");
    assert!(!directory.exists(), "its workspace goes with it");

    let error = harness
        .store
        .thread(&expiring.thread_id)
        .await
        .expect_err("an expired thread is not readable");
    assert_eq!(error.code(), ErrorCode::ThreadExpired);
}

/// Waits for a thread to become a tombstone, or gives up.
async fn await_collected(store: &Store, thread_id: &str) -> bool {
    for _attempt in 0..50 {
        if store.thread(thread_id).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Settings that declare the ceilings a budget test is about.
///
/// Everything not named is left unset, which reads as no ceiling. Budgets are
/// required at thread creation by the API, not by the runner, so a test may
/// declare exactly the one it is exercising.
fn budgeted(budget: arsox_sdk::proto::settings::v1::Budget) -> ThreadSettings {
    ThreadSettings {
        budget: Some(budget),
        ..Default::default()
    }
}

fn wall_clock_of(millis: i32) -> arsox_sdk::proto::settings::v1::Budget {
    use arsox_sdk::proto::common::v1::{DurationCeiling, duration_ceiling};

    arsox_sdk::proto::settings::v1::Budget {
        max_wall_clock_per_turn: Some(DurationCeiling {
            ceiling: Some(duration_ceiling::Ceiling::Duration(
                arsox_sdk::proto::common::v1::Duration {
                    seconds: 0,
                    nanos: millis.saturating_mul(1_000_000),
                },
            )),
        }),
        ..Default::default()
    }
}

fn cost_ceiling_of(units: i64, nanos: i32) -> arsox_sdk::proto::settings::v1::Budget {
    use arsox_sdk::proto::common::v1::{CostCeiling, Money, cost_ceiling};

    arsox_sdk::proto::settings::v1::Budget {
        max_cost_per_thread: Some(CostCeiling {
            ceiling: Some(cost_ceiling::Ceiling::Cost(Money::usd(units, nanos))),
        }),
        ..Default::default()
    }
}

/// Queues another turn on a thread that already has one.
async fn queue_another(store: &Store, thread_id: &str, prompt: &str) -> String {
    store
        .create_turn(NewTurn {
            thread_id: thread_id.to_owned(),
            prompt: prompt.to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        })
        .await
        .expect("should queue a turn")
        .turn
        .turn_id
}

#[tokio::test]
async fn a_turn_that_outruns_its_wall_clock_is_stopped_and_says_which_ceiling_did_it() {
    // A harness stuck in a long shell command asks for no completions, so the
    // proxy never sees it. Wall clock is the ceiling that ends this one, and it
    // has to end gracefully: the work already done stays in the log.
    let (harness, thread_id, turn_id) = start_with(
        "run the probe [[stall=10000]]",
        budgeted(wall_clock_of(500)),
    )
    .await;

    let status = settle(&harness.store, &thread_id, &turn_id).await;
    assert_eq!(status, TurnStatus::Failed);

    let (_turn, result) = harness
        .store
        .turn(&thread_id, &turn_id)
        .await
        .expect("should read");
    let result = result.expect("a stopped turn still carries a result");
    let error = result.error.expect("a stopped turn says which ceiling");

    assert_eq!(
        error.code,
        i32::from(ErrorCode::BudgetWallClockExhausted),
        "one code per ceiling, so a caller never reads a message to learn which"
    );
    assert!(
        !error.retryable,
        "a ceiling does not move by being asked again"
    );

    // The transcript replayed before the stall, and every event it produced is
    // still here. A ceiling stops a turn; it does not discard its work.
    let events = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay");
    let names: Vec<&str> = events.iter().map(|event| event.r#type.as_str()).collect();

    assert!(names.contains(&"tool.started"), "got {names:?}");
    assert!(names.contains(&"agent.message"), "got {names:?}");
    assert!(
        names.contains(&"budget.warning"),
        "eighty percent of the ceiling should have warned before the wall, got {names:?}"
    );

    let warning = events
        .iter()
        .find_map(|event| match event.payload.as_ref() {
            Some(Payload::BudgetWarning(warning)) => Some(warning),
            _other => None,
        })
        .expect("the warning should carry which ceiling");

    assert_eq!(
        warning.ceiling,
        i32::from(arsox_sdk::proto::event::v1::Ceiling::WallClockPerTurn)
    );
    assert_eq!(warning.percent_used, 80);
}

#[tokio::test]
async fn a_thread_that_has_spent_its_cost_ceiling_runs_no_further_turns() {
    // The transcript reports just over ten cents, so one turn spends this
    // ceiling and the next one has nothing to run on. Enforced before anything
    // is spawned: launching a harness to discover an exhausted budget would
    // spend more of it.
    let (harness, thread_id, first) =
        start_with("run the probe", budgeted(cost_ceiling_of(0, 100_000_000))).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &first).await,
        TurnStatus::Completed,
        "the turn that crosses the ceiling still completes"
    );

    let second = queue_another(&harness.store, &thread_id, "keep going").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &second).await,
        TurnStatus::Failed
    );

    let (_turn, result) = harness
        .store
        .turn(&thread_id, &second)
        .await
        .expect("should read");
    let error = result
        .expect("a refused turn carries a result")
        .error
        .expect("a refused turn says why");

    assert_eq!(error.code, i32::from(ErrorCode::BudgetCostExhausted));
}

#[tokio::test]
async fn a_thread_approaching_its_cost_ceiling_is_warned_and_keeps_working() {
    // Eighty percent is a warning, not a wall. A host application gets one
    // chance to react before the ceiling, and the thread runs on either way.
    let (harness, thread_id, first) =
        start_with("run the probe", budgeted(cost_ceiling_of(0, 120_000_000))).await;

    settle(&harness.store, &thread_id, &first).await;

    let second = queue_another(&harness.store, &thread_id, "keep going").await;
    assert_eq!(
        settle(&harness.store, &thread_id, &second).await,
        TurnStatus::Completed,
        "eighty-five percent of a ceiling is not a refusal"
    );

    let warning = harness
        .store
        .events_after(&thread_id, 0, 200)
        .await
        .expect("should replay")
        .into_iter()
        .find_map(|event| match event.payload {
            Some(Payload::BudgetWarning(warning)) => Some(warning),
            _other => None,
        })
        .expect("the approach should have been reported");

    assert_eq!(
        warning.ceiling,
        i32::from(arsox_sdk::proto::event::v1::Ceiling::CostPerThread)
    );
    assert!(
        (80..100).contains(&warning.percent_used),
        "expected an eighty-something percent warning, got {}",
        warning.percent_used
    );
}

#[tokio::test]
async fn a_thread_with_no_cost_ceiling_is_never_refused_for_cost() {
    // Budgets are required at thread creation and `Unlimited` is a thing a
    // caller may genuinely mean. Accounting must not become refusing on its own.
    let (harness, thread_id, first) = start("run the probe").await;
    settle(&harness.store, &thread_id, &first).await;

    let second = queue_another(&harness.store, &thread_id, "keep going").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &second).await,
        TurnStatus::Completed
    );
}

/// Settings whose harness idle bound is `millis`, so a test does not wait out
/// the documented fifteen minutes.
///
/// The bound is a per-thread setting in the contract rather than a satellite
/// constant, which is what makes it injectable at all: the test declares it
/// exactly as a host application would.
fn idle_bound_of(millis: i32) -> ThreadSettings {
    ThreadSettings {
        timeouts: Some(arsox_sdk::proto::settings::v1::Timeouts {
            harness_idle: Some(arsox_sdk::proto::common::v1::Duration {
                seconds: 0,
                nanos: millis.saturating_mul(1_000_000),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Every incident recorded against a thread, with the code a test names.
async fn incidents_coded(store: &Store, thread_id: &str, code: ErrorCode) -> Vec<Incident> {
    store
        .incidents_for_thread(thread_id)
        .await
        .expect("should read incidents")
        .into_iter()
        .filter(|incident| incident.code == i32::from(code))
        .collect()
}

#[tokio::test]
async fn a_harness_that_hangs_once_is_restarted_and_the_turn_completes() {
    // A harness that says nothing at all is stopped rather than slow, and a
    // stopped process is exactly what a restart recovers. `hang_once` wedges the
    // first process and lets the second through, which is what a real harness
    // that wedged on startup looks like from here.
    let (harness, thread_id, turn_id) =
        start_with("run the probe [[hang_once=5000]]", idle_bound_of(300)).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed,
        "the restarted session finished the work"
    );

    // The events the second session produced are in the log, so the restart
    // replaced the harness rather than the turn.
    let names: Vec<String> = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay")
        .into_iter()
        .map(|event| event.r#type)
        .collect();
    assert!(
        names.iter().any(|name| name == "agent.message"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_restarted_harness_records_the_recovery_rather_than_hiding_it() {
    // A restart that worked looks exactly like a turn that never stalled. That
    // is the whole reason `recovered` exists: a harness wedging on every turn is
    // a pattern nobody sees unless the recovery is written down.
    let (harness, thread_id, turn_id) =
        start_with("run the probe [[hang_once=5000]]", idle_bound_of(300)).await;

    settle(&harness.store, &thread_id, &turn_id).await;

    let recorded = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout).await;

    assert_eq!(recorded.len(), 1, "one restart, so one incident");
    assert_eq!(
        recorded[0].disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Recovered),
        "the turn went on, so this is not fatal"
    );
    assert!(
        recorded[0].retryable,
        "a wedged process is worth another attempt"
    );
    assert_eq!(recorded[0].turn_id.as_deref(), Some(turn_id.as_str()));
}

#[tokio::test]
async fn a_harness_that_hangs_every_time_fails_the_turn_with_the_idle_code() {
    // One restart, never two. A harness that wedges again after a clean restart
    // is wedging for a reason a restart does not fix, and a third attempt would
    // spend another session reaching the same place.
    let (harness, thread_id, turn_id) =
        start_with("run the probe [[hang=10000]]", idle_bound_of(300)).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed
    );

    let error = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .error
        .expect("a turn that gave up says why");

    assert_eq!(error.code, i32::from(ErrorCode::HarnessIdleTimeout));
    assert!(
        error.retryable,
        "the turn is worth running again, just not inside this one"
    );

    // Two incidents under one code, and they say different things: the restart
    // that was attempted, and the turn that ended anyway. Recording only the
    // second would lose the fact that a recovery was tried at all.
    let dispositions: Vec<i32> =
        incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout)
            .await
            .iter()
            .map(|incident| incident.disposition)
            .collect();

    assert_eq!(
        dispositions,
        vec![
            i32::from(arsox_sdk::proto::incident::v1::Disposition::Recovered),
            i32::from(arsox_sdk::proto::incident::v1::Disposition::Fatal),
        ],
        "one restart attempted, then the turn gave up"
    );
}

#[tokio::test]
async fn a_harness_that_keeps_talking_is_never_called_idle() {
    // The bound is measured against silence, not against elapsed time. A turn
    // whose transcript takes longer than the bound to replay must not be torn
    // down for doing its work.
    let (harness, thread_id, turn_id) = start_with("run the probe", idle_bound_of(30_000)).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );
    assert!(
        incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout)
            .await
            .is_empty()
    );
}

/// Settings for a thread whose one repo declares a `checker`.
///
/// The repo is never cloned. These tests are about what happens after the agents
/// say they are done, and a real remote would add a network to the fixture
/// without adding anything to what is under test. The checkout is created on
/// disk instead, which is the state provisioning would have left behind.
fn with_checker(checker: &str) -> ThreadSettings {
    ThreadSettings {
        repos: vec![arsox_sdk::proto::settings::v1::Repo {
            name: "api".to_owned(),
            url: "https://example.com/api.git".to_owned(),
            checker: checker.to_owned(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Creates the checkout a declared checker runs in.
fn make_checkout(workspace: &std::path::Path, thread_id: &str) {
    std::fs::create_dir_all(workspace.join(thread_id).join("repos").join("api"))
        .expect("should create the checkout");
}

/// Every checker command that reached the stream, in order.
async fn checker_events(
    store: &Store,
    thread_id: &str,
) -> Vec<arsox_sdk::proto::turn::v1::CheckerResult> {
    store
        .events_after(thread_id, 0, 200)
        .await
        .expect("should replay")
        .into_iter()
        .filter_map(|event| match event.payload {
            Some(Payload::CheckerResult(reported)) => reported.result,
            _other => None,
        })
        .collect()
}

/// The turn's own account of what the checker stage did.
fn checker_stage(
    result: &arsox_sdk::proto::turn::v1::TurnResult,
) -> &arsox_sdk::proto::turn::v1::StageOutcome {
    result
        .stages
        .iter()
        .find(|stage| stage.stage == i32::from(arsox_sdk::proto::turn::v1::Stage::Checkers))
        .expect("the checker stage is always reported, even when it did nothing")
}

/// Reads a finished turn's result.
async fn result_of(
    store: &Store,
    thread_id: &str,
    turn_id: &str,
) -> arsox_sdk::proto::turn::v1::TurnResult {
    store
        .turn(thread_id, turn_id)
        .await
        .expect("should read")
        .1
        .expect("a finished turn carries a result")
}

/// A checker command that prints a declared variable, whatever shell this is.
///
/// `cmd` spells expansion with percent signs and `sh` with a dollar, and the two
/// cannot be written as one string.
const ECHO_DEPLOY_TOKEN: &str = if cfg!(windows) {
    "echo %DEPLOY_TOKEN%"
} else {
    "echo $DEPLOY_TOKEN"
};

#[tokio::test]
async fn a_checker_that_echoed_a_secret_lands_masked_in_the_turn_result() {
    // A checker runs with the thread's credentials in its environment, exactly
    // as the agent that wrote the code did, so a lint that prints its own token
    // is an ordinary Tuesday. The turn result is what a host application logs
    // and reports on, and an unmasked one puts the credential there.
    let mut settings = with_checker(ECHO_DEPLOY_TOKEN);
    settings.env = vec![arsox_sdk::proto::settings::v1::EnvVar {
        key: "DEPLOY_TOKEN".to_owned(),
        value: Some(arsox_sdk::proto::common::v1::Secret {
            value: Some("ghp_the_real_token".to_owned()),
            display: None,
        }),
        // Absent, which means secret. It still reaches the command: secrecy
        // decides what may be rendered, never what a command is given.
        is_secret: None,
    }];

    let (harness, thread_id, turn_id) =
        start_prepared("run the probe", settings, make_checkout).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    let result = result_of(&harness.store, &thread_id, &turn_id).await;
    let recorded = format!("{result:?}");

    assert!(
        !recorded.contains("ghp_the_real_token"),
        "the token survived into the turn result: {recorded}"
    );
    assert!(
        result.checker_results[0].output.contains("******"),
        "the checker output should carry the mask rather than having been \
         dropped: {:?}",
        result.checker_results[0].output
    );

    // And on the stream, where a consumer watching a long build sees each
    // command finish rather than waiting for the turn to end.
    let streamed = format!("{:?}", checker_events(&harness.store, &thread_id).await);
    assert!(
        !streamed.contains("ghp_the_real_token"),
        "the token survived onto the stream: {streamed}"
    );
}

#[tokio::test]
async fn a_passing_checker_is_recorded_and_the_turn_completes() {
    // The verification that turns "the agent said it was done" into something
    // checked. A green checker is recorded rather than assumed.
    let (harness, thread_id, turn_id) =
        start_prepared("run the probe", with_checker("exit 0"), make_checkout).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    let result = result_of(&harness.store, &thread_id, &turn_id).await;

    assert_eq!(result.checker_results.len(), 1);
    assert_eq!(result.checker_results[0].command, "exit 0");
    assert_eq!(result.checker_results[0].exit_code, 0);

    assert_eq!(
        checker_stage(&result).disposition,
        i32::from(arsox_sdk::proto::turn::v1::StageDisposition::Ran),
        "a stage that ran must not report itself as skipped"
    );

    // A live consumer sees each command finish rather than learning about the
    // whole stage when the turn ends.
    assert_eq!(checker_events(&harness.store, &thread_id).await.len(), 1);
}

#[tokio::test]
async fn a_failing_checker_wakes_the_agent_and_a_later_pass_completes_the_turn() {
    // The whole point of the stage. A nonzero exit is not a crash: the agent is
    // resumed with the failure and its output, and the checker runs again
    // against whatever it did about it.
    //
    // The command fails the first time and passes the second, which is what a
    // fixed checker looks like from the satellite's side. `mkdir` is the one
    // stateful thing `sh` and `cmd` spell identically: it succeeds once and
    // refuses afterwards, so the first run reaches `exit 1` and the second is
    // routed to `exit 0`.
    let (harness, thread_id, turn_id) = start_prepared(
        "run the probe",
        with_checker("mkdir stamp && exit 1 || exit 0"),
        make_checkout,
    )
    .await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    let reported = checker_events(&harness.store, &thread_id).await;
    assert_eq!(reported.len(), 2, "the checker should have run twice");
    assert_eq!(reported[0].exit_code, 1);
    assert_eq!(reported[1].exit_code, 0);

    let result = result_of(&harness.store, &thread_id, &turn_id).await;

    // The turn's results are the state it ended in, not the history of getting
    // there. The history is on the stream.
    assert_eq!(result.checker_results.len(), 1);
    assert_eq!(result.checker_results[0].exit_code, 0);
    assert_eq!(
        checker_stage(&result).disposition,
        i32::from(arsox_sdk::proto::turn::v1::StageDisposition::Ran)
    );

    // Both sessions asked the same model through the same grant, and the
    // transcript reports usage each time. Reporting one of two would understate
    // every turn that had to fix a checker.
    let tokens = result.tokens.expect("usage should be recorded");
    let baseline = single_session_tokens().await;

    assert!(
        tokens.total_tokens > baseline.total_tokens,
        "a two-session turn should have spent more than a one-session turn: \
         {} against {}",
        tokens.total_tokens,
        baseline.total_tokens
    );
}

/// What one harness session of the same transcript reports, as a baseline.
async fn single_session_tokens() -> arsox_sdk::proto::usage::v1::TokenUsage {
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    result_of(&harness.store, &thread_id, &turn_id)
        .await
        .tokens
        .expect("usage should be recorded")
}

#[tokio::test]
async fn a_checker_that_never_passes_stops_at_the_cap_with_the_failure_recorded() {
    // A flake that fails at random would otherwise hold a thread open forever,
    // spending an agent session per attempt. The cap is what it cannot outlast,
    // and the turn still completes: a check that will not go green is a fact
    // about the work rather than a reason to throw the work away.
    let (harness, thread_id, turn_id) =
        start_prepared("run the probe", with_checker("exit 7"), make_checkout).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed,
        "reaching the end of the stack is what COMPLETED means"
    );

    // One initial run plus one per fix attempt, and no more.
    let reported = checker_events(&harness.store, &thread_id).await;
    assert_eq!(reported.len(), 3, "got {reported:?}");
    assert!(reported.iter().all(|result| result.exit_code == 7));

    let result = result_of(&harness.store, &thread_id, &turn_id).await;

    assert_eq!(result.checker_results.len(), 1);
    assert_eq!(result.checker_results[0].exit_code, 7);

    let stage = checker_stage(&result);
    assert_eq!(
        stage.disposition,
        i32::from(arsox_sdk::proto::turn::v1::StageDisposition::Failed)
    );
    assert!(
        stage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("fix attempts")),
        "the stage should say why it gave up, got {:?}",
        stage.reason
    );

    // The incident outlives the thread, which is where "why was last night's
    // run red" gets answered after the workspace is reclaimed.
    let incidents = harness
        .store
        .incidents_for_thread(&thread_id)
        .await
        .expect("should read incidents");
    let checker_failed = incidents
        .iter()
        .find(|incident| incident.code == i32::from(ErrorCode::CheckerFailed))
        .expect("a checker the agents could not fix is recorded");

    // Degraded rather than blocked: no permission gate closed. The work happened
    // and finished with its verification missing.
    assert_eq!(
        checker_failed.disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Degraded)
    );
}

#[tokio::test]
async fn a_thread_with_no_checkers_runs_exactly_as_it_did_before() {
    // The stage costs a thread that declared no checker one filter over its
    // repos, and it is still reported: "not run" must never read as "found
    // nothing".
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let result = result_of(&harness.store, &thread_id, &turn_id).await;

    assert!(result.checker_results.is_empty());
    assert!(checker_events(&harness.store, &thread_id).await.is_empty());

    let stage = checker_stage(&result);
    assert_eq!(
        stage.disposition,
        i32::from(arsox_sdk::proto::turn::v1::StageDisposition::Skipped)
    );
    assert_eq!(stage.reason.as_deref(), Some("no repo declares a checker"));
}
