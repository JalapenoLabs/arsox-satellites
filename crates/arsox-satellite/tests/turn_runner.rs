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

/// The same, in the Codex vocabulary.
///
/// A live `codex exec --json` run that wrote a file and read it back, so it
/// carries the two lifecycle lines, a patch, a shell command, and the two agent
/// messages that bracket them. It is the recording the Codex mapper's own
/// conformance test asserts against, for the same one-copy reason.
const CODEX_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/codex/0.147.0/tool-call.stdout.jsonl"
);

/// A recording whose result reports a failure, from a clean exit.
///
/// The harness said what went wrong, which is a statement about the work rather
/// than a process that stopped. It is the case a restart must leave alone.
const ERROR_RESULT_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.237/error-result.stdout.jsonl"
);

/// A recording that names its session on four lines rather than one.
///
/// 2.1.237 reports reasoning progress as `system` lines and repeats the session
/// id on every one, which is what makes "record it once" a claim worth testing
/// against a real transcript rather than a constructed one.
const REPEATED_SESSION_TRANSCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../arsox-harness/fixtures/claude/2.1.237/multi-message.stdout.jsonl"
);

/// The session id that recording announces, on each of those lines.
const REPEATED_SESSION_ID: &str = "83526033-2860-40a4-a7cc-a84a263dbe5e";

/// The session id that recording's `thread.started` announces.
///
/// Minted by the CLI rather than chosen by the satellite, which is the whole
/// difference between the two harnesses' session handling.
const CODEX_SESSION_ID: &str = "01a01cd2-200b-77f0-b4b8-7421557ff5ed";

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
            // One stand-in binary and one transcript per harness. The binary
            // reads its own command line to tell which of the two it is being
            // asked to be, so both can be pointed at the same executable.
            std::env::set_var("ARSOX_CODEX_BIN", env!("CARGO_BIN_EXE_arsox-fake-harness"));
            std::env::set_var("ARSOX_FAKE_CODEX_TRANSCRIPT", CODEX_TRANSCRIPT);
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

    // Under this test's own root rather than the image's. Nothing is installed
    // into it: a test process is not a root satellite, so the broker declines to
    // engage and the harness runs with the satellite's own PATH.
    let broker = arsox_satellite::broker::Broker::at(workspace.path().join("broker"));

    let collector = Arc::new(Collector::new(
        store.clone(),
        workspace.path().to_path_buf(),
        EventBus::new(),
        broker.clone(),
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
        broker,
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
async fn a_session_id_repeated_on_every_line_is_recorded_once() {
    // 2.1.237 names its session on every progress line, and a write per line
    // would be a write per line storing what the one before it stored. The count
    // is the only thing that can tell one write from four: the row holds the
    // same id either way.
    let (harness, thread_id, turn_id) = start(&format!(
        "run the probe [[transcript={REPEATED_SESSION_TRANSCRIPT}]]"
    ))
    .await;

    settle(&harness.store, &thread_id, &turn_id).await;

    assert_eq!(
        harness.store.harness_session_writes(),
        1,
        "the transcript names its session four times"
    );

    let thread = harness.store.thread(&thread_id).await.expect("should read");
    assert_eq!(
        thread.harness_session_id.as_deref(),
        Some(REPEATED_SESSION_ID),
        "recording it once still has to record it"
    );
}

#[tokio::test]
async fn a_restarted_session_records_the_id_it_announces() {
    // The memory belongs to the session rather than to the turn. A restarted
    // Codex process mints a fresh id, and a turn-scoped memory would keep the
    // dead session's and leave the thread resuming a conversation that no longer
    // exists.
    //
    // `truncate` is the ending that reaches this: the harness announces its
    // session and then stops without reporting a result, which is a restart, and
    // the session that replaces it announces itself in turn. Both replay one
    // recording here, so the proof is the second write happening at all.
    let (harness, thread_id, turn_id) = start("run the probe [[truncate=3]]").await;

    settle(&harness.store, &thread_id, &turn_id).await;

    assert_eq!(
        harness.store.harness_session_writes(),
        2,
        "one session announced itself, was restarted, and the new one announced \
         itself too"
    );
}

#[tokio::test]
async fn a_session_id_that_changes_mid_session_keeps_the_one_it_opened() {
    // A recording with one field changed, because no CLI is known to rename a
    // session mid-run and a fixture must stay a recording. The id a thread
    // resumes into is the one its events belong to, so a later, different id is
    // warned about rather than obeyed.
    let scratch = tempdir::TempDir::new("renamed-session");
    let renamed = scratch.path().join("renamed.stdout.jsonl");
    std::fs::write(&renamed, transcript_renaming_its_session()).expect("should write");

    let (harness, thread_id, turn_id) = start(&format!(
        "run the probe [[transcript={}]]",
        renamed.display()
    ))
    .await;

    settle(&harness.store, &thread_id, &turn_id).await;

    let thread = harness.store.thread(&thread_id).await.expect("should read");
    assert_eq!(
        thread.harness_session_id.as_deref(),
        Some(REPEATED_SESSION_ID),
        "the first sighting is the session the turn opened"
    );
    assert_eq!(
        harness.store.harness_session_writes(),
        1,
        "a rename is reported, not written"
    );
}

/// The repeated-session recording, with its last progress line renamed.
///
/// One field of one line, so everything else about the transcript is still the
/// bytes a CLI produced.
fn transcript_renaming_its_session() -> String {
    let recorded = std::fs::read_to_string(REPEATED_SESSION_TRANSCRIPT).expect("should read");

    let renamed: Vec<String> = recorded
        .lines()
        .map(|line| {
            if line.contains(r#""subtype":"thinking_tokens""#) {
                line.replace(REPEATED_SESSION_ID, "00000000-dead-4000-8000-000000000000")
            } else {
                line.to_owned()
            }
        })
        .collect();

    renamed.join("\n")
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
async fn a_harness_that_reports_its_result_and_then_exits_nonzero_keeps_the_result() {
    // A reported result is a statement about the work. Discarding it because the
    // process that made it then exited badly would throw away the answer the
    // satellite was given and charge a session to hear it again.
    let (harness, thread_id, turn_id) = start("run the probe [[exit=3]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed,
        "the harness said what it did, and the turn reports it"
    );

    let result = result_of(&harness.store, &thread_id, &turn_id).await;
    assert!(
        result.error.is_none(),
        "the result stands: {:?}",
        result.error
    );
    assert!(
        !result.summary.is_empty(),
        "the summary the harness reported survives its exit"
    );

    // The messy ending is a fact worth seeing rather than a reason to redo the
    // work, so it is recorded with the evidence a crash carries.
    let recorded = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed).await;
    assert_eq!(recorded.len(), 1, "one bad exit, one incident");
    assert_eq!(
        recorded[0].disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Degraded),
        "the work happened; what is missing is a clean shutdown"
    );
    assert!(
        !recorded[0].retryable,
        "the work is done and reported, so a second turn would redo it"
    );

    let evidence = recorded[0]
        .details
        .as_ref()
        .expect("the exit is only readable afterwards if it was captured");
    assert_eq!(number_field(evidence, "exit_code"), Some(3.0));
}

#[tokio::test]
async fn a_harness_that_exits_nonzero_without_a_result_still_fails_the_turn() {
    // The other side of the boundary, and the one a restart is for. A process
    // that died before saying what it did left nothing to honor, so it is
    // replaced, and a second death ends the turn.
    let (harness, thread_id, turn_id) = start("run the probe [[truncate=3]] [[exit=3]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed
    );

    let error = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .error
        .expect("a failed turn says why");

    assert_eq!(error.code, i32::from(ErrorCode::HarnessCrashed));
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

/// One numeric field of an incident's evidence.
fn number_field(details: &prost_types::Struct, key: &str) -> Option<f64> {
    match details.fields.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::NumberValue(number) => Some(*number),
        _other => None,
    }
}

/// One text field of an incident's evidence.
fn text_field<'a>(details: &'a prost_types::Struct, key: &str) -> Option<&'a str> {
    match details.fields.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::StringValue(text) => Some(text),
        _other => None,
    }
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
async fn a_harness_that_crashes_once_is_restarted_and_the_turn_completes() {
    // The other ending a restart recovers. `crash_once` dies before the
    // transcript on the first run and lets the second through, which is what a
    // harness that died on startup looks like from here.
    let (harness, thread_id, turn_id) = start("run the probe [[crash_once=9]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed,
        "the restarted session finished the work"
    );

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
async fn a_restarted_crash_records_the_recovery_with_the_code_it_died_on() {
    // `recovered` is the disposition that pays for the incident feature: a
    // restart that worked looks exactly like a turn that never crashed, and a
    // harness dying on every turn is a pattern nobody sees unless the recovery
    // is written down. The exit code rides along because it is the first thing
    // anybody asks about a process that died.
    let (harness, thread_id, turn_id) = start("run the probe [[crash_once=9]]").await;

    settle(&harness.store, &thread_id, &turn_id).await;

    let recorded = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed).await;

    assert_eq!(recorded.len(), 1, "one restart, so one incident");
    assert_eq!(
        recorded[0].disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Recovered),
        "the turn went on, so this is not fatal"
    );
    assert!(
        recorded[0].retryable,
        "a process that died is worth another attempt"
    );
    assert_eq!(recorded[0].turn_id.as_deref(), Some(turn_id.as_str()));

    let evidence = recorded[0]
        .details
        .as_ref()
        .expect("a crash carries the exit code it died on");
    assert_eq!(number_field(evidence, "exit_code"), Some(9.0));
}

#[tokio::test]
async fn a_harness_that_crashes_every_time_fails_the_turn_and_says_what_it_died_of() {
    // One restart, never two. `truncate` with `exit` dies before reporting a
    // result on every run, so the second death is the turn's ending, and it
    // carries the evidence somebody needs to act on it without reproducing the
    // run. Truncated on purpose: a death after a result is honored rather than
    // restarted, which is a different ending entirely.
    let (harness, thread_id, turn_id) = start("run the probe [[truncate=3]] [[exit=3]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed
    );

    let error = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .error
        .expect("a turn that gave up says why");

    assert_eq!(error.code, i32::from(ErrorCode::HarnessCrashed));
    assert!(
        error.retryable,
        "the workspace survives, so the same turn is worth submitting again"
    );

    // Two incidents under one code, saying different things: the restart that
    // was attempted, and the turn that ended anyway.
    let recorded = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed).await;
    let dispositions: Vec<i32> = recorded
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

    let evidence = recorded[1]
        .details
        .as_ref()
        .expect("the fatal incident carries the exit code and the last output");
    assert_eq!(number_field(evidence, "exit_code"), Some(3.0));
    assert!(
        text_field(evidence, "output_tail").is_some_and(|tail| tail.contains("assistant")),
        "the tail should hold what the process said last, which is the third \
         line of the recording: {:?}",
        text_field(evidence, "output_tail")
    );
}

#[tokio::test]
async fn a_clean_exit_that_reported_an_error_result_is_not_restarted() {
    // The line that decides what a restart is for. A harness that emitted a
    // well-formed error result made a statement about the work, and running it
    // again would spend another session reaching the same answer. Only a process
    // that stopped without saying anything is worth replacing.
    let (harness, thread_id, turn_id) = start(&format!(
        "run the probe [[transcript={ERROR_RESULT_TRANSCRIPT}]]"
    ))
    .await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed,
        "the harness reported a failure, and the turn reports it too"
    );

    // A restart always records the recovery it attempted, so an empty listing
    // under both restartable codes is the proof that no session was spent twice.
    assert!(
        incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed)
            .await
            .is_empty(),
        "an error result is an answer rather than a crash"
    );
    assert!(
        incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_idle_restart_and_a_crash_spend_one_budget_between_them() {
    // One budget for the turn, across both causes. What it bounds is process
    // instability inside a turn, and a harness that hung, was restarted, and
    // then died is unstable twice however differently the two endings read.
    //
    // The second session dies before reporting a result, since a death after one
    // is honored rather than restarted and would prove nothing about the budget.
    let (harness, thread_id, turn_id) = start_with(
        "run the probe [[hang_once=5000]] [[truncate=3]] [[exit=4]]",
        idle_bound_of(300),
    )
    .await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed
    );

    let error = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .error
        .expect("a turn that gave up says why");
    assert_eq!(
        error.code,
        i32::from(ErrorCode::HarnessCrashed),
        "the turn ends on the ending that actually stopped it"
    );

    // The hang was recovered, and the crash after it found the budget spent.
    let hung = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout).await;
    assert_eq!(hung.len(), 1);
    assert_eq!(
        hung[0].disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Recovered)
    );

    let died = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed).await;
    assert_eq!(
        died.iter()
            .map(|incident| incident.disposition)
            .collect::<Vec<i32>>(),
        vec![i32::from(
            arsox_sdk::proto::incident::v1::Disposition::Fatal
        )],
        "a second restart would be a third session proving the second one"
    );
}

#[tokio::test]
async fn a_result_that_survived_a_bad_exit_leaves_the_restart_budget_alone() {
    // The reason honoring the result is not just tidier. A session spent on an
    // answer already given is a session the turn does not have when a later one
    // wedges for a reason a restart actually fixes.
    //
    // The work session reports its result and exits nonzero. Its checker then
    // fails, which resumes the agent, and that second session hangs: if the bad
    // exit had cost the restart there would be none left for it, and the fix
    // would have died with the hang instead of finishing.
    //
    // The hang directive rides on the checker command, because a fix session's
    // prompt is written by the runner out of the failing command and that
    // command is therefore the only text a test can put in front of the session
    // it resumes. The work session never reads it, which is what this needs: the
    // session that wedges has to be a later one.
    let (harness, thread_id, turn_id) = start_prepared(
        "run the probe [[exit=3]]",
        ThreadSettings {
            timeouts: idle_bound_of(300).timeouts,
            ..with_checker("echo [[hang_once=5000]] && mkdir stamp && exit 1 || exit 0")
        },
        make_checkout,
    )
    .await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    // The restart was there for the hang, and was spent on it.
    let hung = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessIdleTimeout).await;
    assert_eq!(
        hung.iter()
            .map(|incident| incident.disposition)
            .collect::<Vec<i32>>(),
        vec![i32::from(
            arsox_sdk::proto::incident::v1::Disposition::Recovered
        )],
        "the fix session hung once and was restarted"
    );

    // Every bad exit was honored rather than restarted, so none of them is
    // recorded as a recovery or as an ending.
    let died = incidents_coded(&harness.store, &thread_id, ErrorCode::HarnessCrashed).await;
    assert!(!died.is_empty(), "a bad exit is still written down");
    assert!(
        died.iter().all(|incident| incident.disposition
            == i32::from(arsox_sdk::proto::incident::v1::Disposition::Degraded)),
        "a result that stands is neither a recovery nor a fatal ending: {died:?}"
    );

    // And the checkers finished, which is what a fix session that survived its
    // restart looks like from the outside.
    let reported = checker_events(&harness.store, &thread_id).await;
    assert_eq!(reported.len(), 2, "the checker should have run twice");
    assert_eq!(reported[1].exit_code, 0);
}

#[tokio::test]
async fn a_crash_before_a_session_id_restarts_under_the_id_the_first_attempt_used() {
    // A first turn that died before the harness announced a session has nothing
    // to resume, so the restart opens one under the id the satellite chose the
    // first time. Resuming a session that was never created would fail the
    // restart on the one path it exists to recover.
    let (harness, thread_id, turn_id) =
        start("run the probe [[crash_once=9]] [[record_argv=restart.argv]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    // The file holds the last spawn's command line, which is the restart's.
    let restarted = recorded_argv(harness.workspace.path(), &thread_id, "restart.argv");

    assert!(
        !restarted.contains(&"--resume".to_owned()),
        "there was no session to resume: {restarted:?}"
    );
    assert!(
        restarted
            .windows(2)
            .any(|pair| pair == ["--session-id".to_owned(), thread_id.clone()]),
        "the restart should open the session the first attempt was given: {restarted:?}"
    );
}

#[tokio::test]
async fn a_codex_crash_before_its_thread_started_restarts_without_a_resume() {
    // The same case from the other side of the session disagreement. Codex mints
    // its own id and announces it on `thread.started`, so a process that died
    // before saying anything left the satellite with nothing to pass, and the
    // restart asks for a fresh one exactly as a first turn does.
    let (harness, thread_id, turn_id) = start_with(
        "run the probe [[crash_once=9]] [[record_argv=restart.argv]]",
        codex_thread(),
    )
    .await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    let restarted = recorded_argv(harness.workspace.path(), &thread_id, "restart.argv");

    assert_eq!(restarted.first().map(String::as_str), Some("exec"));
    assert!(
        !restarted.contains(&"resume".to_owned()),
        "the CLI had not minted an id yet: {restarted:?}"
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
async fn a_turn_report_carries_its_own_incident_counts() {
    // The counts ride along so the common case needs no query at all: a consumer
    // reacting to `turn.completed` learns that something went wrong without
    // asking a second question, and queries only to find out what.
    let (harness, thread_id, turn_id) = start("run the probe [[truncate=3]]").await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed,
        "a harness that stops before reporting a result fails the turn"
    );

    let result = result_of(&harness.store, &thread_id, &turn_id).await;
    let counts = result
        .incident_counts
        .expect("every turn reports its counts, even when they are all zero");

    // A harness that exited without saying what it did is started once more,
    // and the turn gives up when the second one says nothing either. Both are
    // counted, which is the point: a report saying only that the turn failed
    // would hide that a recovery was attempted.
    assert_eq!(counts.recovered, 1);
    assert_eq!(counts.fatal, 1);
    assert_eq!(counts.degraded, 0);
    assert_eq!(counts.blocked, 0);

    // The counts describe the rows a listing would return rather than a tally
    // kept beside them that could drift.
    let recorded = harness
        .store
        .incidents_for_thread(&thread_id)
        .await
        .expect("should read incidents");
    assert_eq!(
        recorded
            .iter()
            .filter(|incident| incident.turn_id.as_deref() == Some(turn_id.as_str()))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_clean_turn_reports_counts_of_zero_rather_than_nothing() {
    // "Not counted" and "nothing went wrong" are different facts, and an absent
    // message would collapse them into the first.
    let (harness, thread_id, turn_id) = start("run the probe").await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let counts = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .incident_counts
        .expect("a clean turn still reports its counts");

    assert_eq!(counts.fatal, 0);
    assert_eq!(counts.degraded, 0);
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

/// A stub upstream that rejects every credential it is shown.
///
/// The failure a second endpoint exists for, and the one an expired
/// subscription actually produces.
async fn stub_that_rejects() -> String {
    let router = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                r#"{"type":"error","error":{"type":"authentication_error"}}"#,
            )
        }),
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

#[tokio::test]
async fn a_turn_whose_every_endpoint_failed_ends_on_the_aggregate_error() {
    // The loop closing on the failover path: the proxy finds the failure, the
    // runner ends the turn on it, and the caller is told which endpoints were
    // tried rather than that something went wrong.
    let settings = ThreadSettings {
        models: vec![arsox_sdk::proto::settings::v1::ModelEndpoint {
            name: "expired".to_owned(),
            model: "claude-opus-5".to_owned(),
            base_url: Some(stub_that_rejects().await),
            auth: None,
            retry: Some(arsox_sdk::proto::settings::v1::RetryPolicy {
                max_attempts: Some(1),
                ..arsox_sdk::proto::settings::v1::RetryPolicy::default()
            }),
        }],
        ..ThreadSettings::default()
    };

    let (harness, thread_id, turn_id) = start_with("[[complete=1]] run the probe", settings).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Failed
    );

    let error = result_of(&harness.store, &thread_id, &turn_id)
        .await
        .error
        .expect("a failed turn says why");

    assert_eq!(error.code, i32::from(ErrorCode::LlmAllEndpointsExhausted));
    assert!(
        error.retryable,
        "the workspace survives, so the same turn submitted against working \
         credentials resumes from where this one stopped"
    );
    assert!(
        error
            .details
            .is_some_and(|details| details.fields.contains_key("attempts")),
        "the aggregate code is useless without the per-endpoint reasons"
    );

    let recorded = incidents_coded(
        &harness.store,
        &thread_id,
        ErrorCode::LlmAllEndpointsExhausted,
    )
    .await;

    assert_eq!(
        recorded.len(),
        1,
        "recorded once, where the proxy found it, rather than again on the way \
         out of the turn"
    );
    assert_eq!(
        recorded[0].disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Fatal)
    );
}

/// A thread that names Codex as its harness.
///
/// The one setting that changes, so anything these tests find is about the
/// harness rather than about a differently configured thread.
fn codex_thread() -> ThreadSettings {
    ThreadSettings {
        harness: arsox_sdk::proto::harness::v1::Harness::Codex.into(),
        ..Default::default()
    }
}

/// What a turn's stand-in harness was asked to run, as it saw it.
///
/// Read from the file the child wrote rather than rebuilt from the settings,
/// because "did this turn resume a session" is a fact about the command line and
/// the child is the only thing that can report one.
///
/// The program's own path leads the list, exactly as `argv` does, and is dropped
/// here so a caller reads the arguments the satellite chose.
fn recorded_argv(workspace: &std::path::Path, thread_id: &str, file: &str) -> Vec<String> {
    let recorded = std::fs::read_to_string(workspace.join(thread_id).join(file))
        .expect("the stand-in should have recorded its command line");

    recorded.lines().skip(1).map(str::to_owned).collect()
}

#[tokio::test]
async fn a_codex_thread_runs_a_turn_and_lands_its_mapped_events_in_the_log() {
    // The normalization claim closing end to end: a different CLI, a different
    // native vocabulary, and the same canonical events in the same log. Nothing
    // below this line knows which harness produced them.
    let (harness, thread_id, turn_id) = start_with("run the probe", codex_thread()).await;

    assert_eq!(
        settle(&harness.store, &thread_id, &turn_id).await,
        TurnStatus::Completed
    );

    let names: Vec<String> = harness
        .store
        .events_after(&thread_id, 0, 100)
        .await
        .expect("should replay")
        .into_iter()
        .map(|event| event.r#type)
        .collect();

    assert_eq!(
        names,
        vec![
            "turn.started",
            // The preamble, the patch, the command, and the answer. Codex
            // delivers a patch as an item rather than as a tool call, and the
            // contract has one shape for a tool call either way.
            "agent.message",
            "tool.started",
            "tool.completed",
            "tool.started",
            "tool.completed",
            "agent.message",
            "turn.completed",
        ]
    );

    let result = result_of(&harness.store, &thread_id, &turn_id).await;

    // Codex closes with an `agent_message` item and reports `turn.completed`
    // with counts and nothing else, so the summary comes from the last thing the
    // agent said. The first message is the preamble, which is what makes *last*
    // load-bearing.
    assert!(
        result.summary.contains("Created `hello.txt`"),
        "the turn should report the answer rather than the plan: {:?}",
        result.summary
    );
}

#[tokio::test]
async fn a_codex_turn_reports_usage_with_the_cached_tokens_taken_out() {
    // Codex counts cached tokens inside `input_tokens` and Anthropic counts them
    // beside it, so the mapper subtracts. This asserts the corrected number
    // survives the whole path into the turn's stored result, which is where a
    // cost reconciliation reads it.
    let (harness, thread_id, turn_id) = start_with("run the probe", codex_thread()).await;
    settle(&harness.store, &thread_id, &turn_id).await;

    let result = result_of(&harness.store, &thread_id, &turn_id).await;
    let tokens = result.tokens.expect("usage should be recorded");

    assert_eq!(tokens.cache_read_tokens, Some(17_920));
    assert_eq!(
        tokens.input_tokens, 11_117,
        "29,037 reported minus 17,920 cached"
    );
    assert_eq!(
        tokens.total_tokens,
        tokens.input_tokens + tokens.output_tokens,
        "cache reads are reported beside input and must not be added back"
    );

    // Absent rather than zero. The provider behind this harness has no
    // cache-write concept, so the zero it reports is a placeholder and claiming
    // "this run wrote nothing to cache" would be a different and false statement.
    assert_eq!(tokens.cache_write_tokens, None);

    // The usage event names no model, so the mapper splits by none. Inventing
    // one here would be the harness leaking into a number a consumer groups by.
    assert!(result.by_model.is_empty());
}

#[tokio::test]
async fn a_codex_thread_resumes_the_session_the_cli_minted_for_it() {
    // The session handling is inverted from Claude's. The satellite has no say
    // in the id, so the first turn asks for nothing and the mapper records what
    // `thread.started` announced. Without this a thread would be a series of
    // unrelated turns rather than a conversation.
    let (harness, thread_id, first) =
        start_with("run the probe [[record_argv=first.argv]]", codex_thread()).await;

    settle(&harness.store, &thread_id, &first).await;

    let thread = harness.store.thread(&thread_id).await.expect("should read");
    assert_eq!(
        thread.harness_session_id.as_deref(),
        Some(CODEX_SESSION_ID),
        "the id belongs to the CLI, not to the satellite"
    );
    assert_ne!(
        thread.harness_session_id.as_deref(),
        Some(thread_id.as_str()),
        "reusing the thread id here would hide the CLI ignoring it"
    );

    let opened = recorded_argv(harness.workspace.path(), &thread_id, "first.argv");
    assert!(!opened.contains(&"resume".to_owned()), "{opened:?}");
    assert!(
        !opened.iter().any(|argument| argument == CODEX_SESSION_ID),
        "a first turn cannot know an id the CLI has not minted yet: {opened:?}"
    );

    let second = queue_another(
        &harness.store,
        &thread_id,
        "and now this [[record_argv=second.argv]]",
    )
    .await;
    settle(&harness.store, &thread_id, &second).await;

    let resumed = recorded_argv(harness.workspace.path(), &thread_id, "second.argv");

    assert_eq!(
        resumed.get(..2).map(<[String]>::to_vec),
        Some(vec!["exec".to_owned(), "resume".to_owned()]),
        "{resumed:?}"
    );
    assert!(
        resumed.iter().any(|argument| argument == CODEX_SESSION_ID),
        "the second turn should carry the id the first one recorded: {resumed:?}"
    );
}

#[tokio::test]
async fn a_command_a_shim_refused_becomes_a_blocked_incident_on_the_turn_that_met_it() {
    // The satellite half of the exec broker, end to end: a record the shim
    // dropped into the thread's spool becomes a `PERMISSION_COMMAND_DENIED`
    // incident carrying the argv an operator needs to widen an allowlist.
    //
    // The record is written here by hand rather than by a real shim, because a
    // shim needs a Linux host and a root satellite and this runs on neither.
    // What a real shim writes is the same `Denial`, through the same `record`,
    // and its own decision is unit tested in `broker::shim`.
    let (harness, thread_id, first_turn) = start("do the thing").await;
    settle(&harness.store, &thread_id, &first_turn).await;

    let spool = arsox_satellite::broker::Broker::at(harness.workspace.path().join("broker"))
        .spool_directory(&thread_id);
    std::fs::create_dir_all(&spool).expect("should create the spool");
    arsox_satellite::broker::spool::record(
        &spool,
        &arsox_satellite::broker::spool::Denial::now(
            &thread_id,
            arsox_satellite::broker::spool::Kind::CommandDenied,
            "docker",
            vec!["docker".to_owned(), "build".to_owned(), ".".to_owned()],
            "is not on this thread's exec allowlist",
        ),
    )
    .expect("should record a refusal");

    // Seeded between turns rather than before the first, because the runner
    // claims work the moment it exists and a refusal written into that race
    // would be reported against whichever turn won it.
    let second_turn = queue_another(&harness.store, &thread_id, "and now this").await;
    settle(&harness.store, &thread_id, &second_turn).await;

    let recorded = incidents_coded(
        &harness.store,
        &thread_id,
        ErrorCode::PermissionCommandDenied,
    )
    .await;

    assert_eq!(recorded.len(), 1, "one refusal, one incident");
    let incident = &recorded[0];

    assert_eq!(
        incident.disposition,
        i32::from(arsox_sdk::proto::incident::v1::Disposition::Blocked),
        "a permission gate closing as designed is blocked, never degraded"
    );
    assert!(
        !incident.retryable,
        "the same argv meets the same allowlist next time"
    );
    assert_eq!(
        incident.turn_id.as_deref(),
        Some(second_turn.as_str()),
        "a refusal belongs to the turn that was running when it happened"
    );
    assert!(incident.message.contains("docker build ."), "{incident:?}");

    let details = incident.details.as_ref().expect("the evidence travels");
    assert_eq!(text_field(details, "command"), Some("docker"));
    assert_eq!(
        argv_field(details),
        vec!["docker", "build", "."],
        "details.argv is what the README promises and what widens an allowlist"
    );

    // Drained rather than re-read, so a third turn does not report the same
    // refusal again.
    let third_turn = queue_another(&harness.store, &thread_id, "and again").await;
    settle(&harness.store, &thread_id, &third_turn).await;
    assert_eq!(
        incidents_coded(
            &harness.store,
            &thread_id,
            ErrorCode::PermissionCommandDenied,
        )
        .await
        .len(),
        1,
        "the spool was drained, not merely read"
    );
}

/// The argv an incident's evidence carries, as a list of words.
fn argv_field(details: &prost_types::Struct) -> Vec<String> {
    let Some(prost_types::value::Kind::ListValue(list)) = details
        .fields
        .get("argv")
        .and_then(|argv| argv.kind.as_ref())
    else {
        return Vec::new();
    };

    list.values
        .iter()
        .filter_map(|word| match word.kind.as_ref()? {
            prost_types::value::Kind::StringValue(text) => Some(text.clone()),
            _other => None,
        })
        .collect()
}
