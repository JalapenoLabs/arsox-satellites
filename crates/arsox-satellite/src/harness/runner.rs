// Copyright © 2026 Jalapeno Labs

//! Driving one turn from queued to finished.
//!
//! The runner is the loop that closes: it claims a queued turn, spawns the
//! harness, feeds every line of output through the mapper, appends the canonical
//! events to the log, and records the result.
//!
//! # One turn at a time, per thread
//!
//! Enforced by the claim query rather than by anything here, so it holds even if
//! a second runner appears. A thread is a conversation and conversations are
//! sequential.
//!
//! # Nothing is dropped
//!
//! A harness that fails to launch, crashes, or emits a line the mapper does not
//! recognize all produce incidents. A turn that ends badly ends with a reason
//! attached rather than a gap where its output should be.
//!
//! # A wedged harness is ended, and given one chance to recover
//!
//! Three endings say the process stopped without saying what the turn did: it
//! produced no output at all inside its idle bound, it died, or it exited
//! cleanly having never reported a result. Each is a fact about a process rather
//! than about the work, and a process is what a restart replaces. So the harness
//! is started again on the same session, under the same grant, meter, and wall
//! clock.
//!
//! A session that **did** report a result is none of those, however badly the
//! result reads and however badly its process then ended. The harness made a
//! statement, and restarting it would repeat the turn against the same answer.
//! So a harness that reports its result and then exits nonzero, or has to be
//! torn down, keeps its result: the messy ending is recorded as a `degraded`
//! incident carrying the exit status and the last output, and the restart stays
//! available for a session that has said nothing to lose. The boundary is the
//! result, not the exit status: a process that died before reporting one is
//! still replaced.
//!
//! **Once, never twice, and once for the whole turn.** A restart recovers a
//! process that wedged; it does not recover a prompt that wedges every process
//! that reads it. A second restart would spend another session reaching the same
//! place, so the second ending fails the turn with `HARNESS_IDLE_TIMEOUT` or
//! `HARNESS_CRASHED`, whichever it was.

use crate::harness::spawn::{HarnessCommand, ModelAccess, Session, command_for, process_for};
use crate::harness::{HarnessResult, Mapping, accounting, checkers, claude, codex};
use crate::proxy::budget::{Ceilings, Crossing, Meter};
use crate::redaction::Redactor;
use crate::store::{AppendEvent, ClaimedTurn, Store};
use crate::timeouts::Bounds;
use arsox_sdk::proto::common::v1::{Duration, Timestamp};
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{
    BudgetWarning, Ceiling, CheckerResultEvent, ThreadEndReason, TurnCompleted, TurnStarted,
};
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::incident::v1::{Disposition, Incident, IncidentCounts};
use arsox_sdk::proto::turn::v1::{
    CheckerResult, Stage, StageDisposition, StageOutcome, TurnResult, TurnStatus,
};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::mpsc;
use tokio::sync::{Notify, Semaphore};

/// How often the dispatcher looks for work in the absence of a nudge.
///
/// A queued turn normally wakes the dispatcher immediately. This is the safety
/// net for the case where that nudge is lost, so a missed notification costs a
/// short delay rather than a thread that never runs.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(2);

/// How often a running turn checks whether it has been cancelled.
///
/// Cancellation arrives as an ordinary HTTP request that writes to the database,
/// so the runner learns about it by asking. Checking every line would be a query
/// per line of output.
const CANCEL_CHECK_EVERY: usize = 20;

/// A deadline far enough out that it never arrives inside a turn.
///
/// `select!` wants a branch of one type whether or not a wall clock ceiling was
/// set, and a sentinel instant reads better than an optional future threaded
/// through the loop. A turn that ran for a year has other problems.
const NEVER: std::time::Duration = std::time::Duration::from_secs(SECONDS_IN_A_YEAR);

/// Seconds in a year, spelled out so [`NEVER`] reads as the span it is.
const SECONDS_IN_A_YEAR: u64 = 365 * 24 * 60 * 60;

/// Fraction of a wall clock ceiling that earns a warning.
///
/// The same 80% the token and cost ceilings warn at, expressed as a fraction
/// because a duration is scaled rather than divided into percent.
const WARN_AT_FRACTION: f64 = 0.8;

/// How many times one turn starts its harness again before it gives up.
///
/// One. A restart recovers a process that wedged, which is a real and common
/// thing; it does not recover a prompt, a repo, or a model that wedges every
/// process reading it, which is what a second failure after a clean restart says
/// is happening. A third attempt spends another session proving the second one.
///
/// **One budget for the turn, across every cause and every session.** What it
/// bounds is process instability inside a turn, and a harness that hung, was
/// restarted, and then died is unstable twice however differently the two
/// endings are named. A budget per cause would let one turn spend four sessions
/// to learn the same thing, and a budget per session would not be a ceiling the
/// turn holds at all, since a checker fix cycle is another session in it. See
/// [`TurnContext::restarts_left`].
const RESTARTS_ALLOWED: u32 = 1;

/// How many of a wedged session's last lines are kept as evidence.
///
/// Enough to carry a stack trace or the CLI's parting complaint, and bounded
/// because the alternative is a turn's entire output in an incident row. What
/// stdout carried is in the event log either way; this is the only place stderr
/// survives at all.
const TAIL_LINES: usize = 20;

/// How much of any one of those lines is kept.
///
/// A single line of harness stdout is a JSON object that can run to tens of
/// kilobytes, and twenty of those is a row nobody can read. The head of a line
/// is the part that says what it was.
const TAIL_LINE_CHARS: usize = 500;

/// Withdraws a turn's proxy grant however the turn ends.
///
/// A guard rather than a call at the end of the happy path, because a turn can
/// leave `drive` by cancellation, by a harness crash, or by any error added
/// later. Each of those is a path somebody could forget, and a forgotten revoke
/// is a token that keeps working after its turn stopped.
struct RevokeOnDrop {
    proxy: crate::proxy::LlmProxy,
    token: String,
}

impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        // `Drop` cannot await, so the revoke is handed to the runtime. It runs
        // before anything could use the token again: the harness process is
        // already gone by the time this drops.
        let proxy = self.proxy.clone();
        let token = std::mem::take(&mut self.token);
        tokio::spawn(async move { proxy.revoke(&token).await });
    }
}

/// Where something that happened belongs, and whose secrets mask it.
///
/// The three travel together everywhere: an event, an incident, and a budget
/// warning each need the thread, the turn, and the thread's redactor. Grouping
/// them means a signature cannot be called with one thread's ids and another
/// thread's mask, and it keeps the parameter lists readable now that every one
/// of them carries a redactor.
#[derive(Debug, Clone, Copy)]
struct Attribution<'a> {
    thread_id: &'a str,
    turn_id: &'a str,
    redactor: &'a Redactor,
}

impl<'a> Attribution<'a> {
    /// What a claimed turn attributes its output to.
    fn of(claimed: &'a ClaimedTurn) -> Self {
        Self {
            thread_id: &claimed.turn.thread_id,
            turn_id: &claimed.turn.turn_id,
            redactor: &claimed.redactor,
        }
    }
}

/// What reading a harness's output produced.
#[derive(Debug, Default)]
struct Consumed {
    result: Option<HarnessResult>,
    cancelled: bool,

    /// The ceiling that stopped the turn, when one did.
    exhausted: Option<Ceiling>,

    /// Whether the harness went silent past its idle bound.
    ///
    /// Not an error by itself, because one restart is allowed to recover it.
    /// [`Runner::session_with_restart`] is what turns a second one into a failed
    /// turn.
    idle: bool,

    /// How the process died, when it died rather than finished.
    ///
    /// Not an error by itself either, for the same reason and through the same
    /// restart. Absent for a session the runner tore down on purpose: the exit
    /// status of a process the satellite killed says nothing about the harness.
    crashed: Option<Crash>,

    /// The last lines the session wrote, on either pipe.
    ///
    /// The "last output" the README promises a crash captures, and the evidence
    /// that lets somebody act on the incident without reproducing the run.
    tail: Tail,

    /// A failure the proxy reported that the turn cannot go on from.
    ///
    /// The proxy sees requests and not the turn they belong to the end of, so it
    /// says what happened and the runner decides what it means. Every endpoint
    /// exhausted is the case this exists for: the harness has been answered with
    /// an error it can read, and there is nothing left for the turn to spend.
    failed: Option<Failure>,

    /// The last thing the agent said, for a harness whose result carries no
    /// summary of its own.
    ///
    /// Codex closes a turn with an ordinary `agent_message` item and reports
    /// `turn.completed` with token counts and nothing else, so its mapper, which
    /// is a pure function of one line and holds no state between lines, has no
    /// summary to give. The session does, and this is where it is kept.
    ///
    /// *Last* is load-bearing rather than incidental: a recorded turn opens with
    /// a preamble announcing what the agent is about to do and closes with the
    /// answer, so taking the first would report the plan as the result.
    last_agent_message: Option<String>,

    /// The harness session id this session has already written down.
    ///
    /// A harness names its session on more lines than one: Claude 2.1.237
    /// repeats it on every `thinking_tokens` progress line, and writing each
    /// sighting would be a database write per line, all storing the value the
    /// one before it stored.
    ///
    /// Held on the session rather than on the turn, because a restart is a new
    /// process and a restarted Codex process mints a fresh id that has to be
    /// recorded. A turn-scoped memory would keep the dead session's id and leave
    /// the thread unable to resume the live one.
    recorded_session_id: Option<String>,
}

impl Consumed {
    /// Whether this session id is news, saying so when it is a rename.
    ///
    /// The first sighting is what gets written. Every one after it repeats what
    /// the thread already knows, which is the common case: a harness reporting
    /// progress names its session on every line of it.
    ///
    /// **A sighting carrying a *different* id is not a correction to apply.**
    /// The id the thread is already resuming into is the one its events belong
    /// to, so a session that renamed itself mid-run is warned about and the
    /// first id stands. A rename nobody can see would be the worse of the two.
    fn first_sighting_of(&self, session_id: &str, at: Attribution<'_>) -> bool {
        let Some(recorded) = self.recorded_session_id.as_deref() else {
            return true;
        };

        if recorded != session_id {
            tracing::warn!(
                event.name = "turn.session.changed",
                thread.id = at.thread_id,
                turn.id = at.turn_id,
                harness.session_id = recorded,
                harness.reported_session_id = session_id,
                "the harness reported {{harness.reported_session_id}} after \
                 opening {{harness.session_id}}, and the session it opened is \
                 the one being kept",
            );
        }

        false
    }
}

/// A harness process that ended by dying rather than by finishing.
#[derive(Debug, Clone)]
struct Crash {
    /// The code it exited with, absent when a signal ended it instead.
    code: Option<i32>,

    /// The status as the platform renders it.
    ///
    /// Kept alongside the code because a signal has no code, and "killed by
    /// SIGKILL" is the sentence that explains an out-of-memory harness.
    status: String,
}

/// The last lines a harness wrote before it stopped.
///
/// A bounded ring rather than the whole session. See [`TAIL_LINES`].
#[derive(Debug, Default)]
struct Tail(std::collections::VecDeque<String>);

impl Tail {
    /// Keeps `line`, dropping the oldest once the ring is full.
    fn remember(&mut self, line: &str) {
        let kept: String = line.chars().take(TAIL_LINE_CHARS).collect();

        self.0.push_back(kept);
        if self.0.len() > TAIL_LINES {
            self.0.pop_front();
        }
    }

    /// What it amounts to, or nothing when the session said nothing.
    ///
    /// `None` rather than an empty string, so an incident carries no field
    /// pretending to be evidence.
    fn joined(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| {
            self.0
                .iter()
                .map(String::as_str)
                .collect::<Vec<&str>>()
                .join("\n")
        })
    }
}

/// A session ending that says the process stopped rather than what the work did.
///
/// Every variant is a fact about the process rather than about the work, which
/// is the line that decides what a restart is for. Whether one is spent is the
/// caller's to decide and turns on a second question: a session that already
/// reported its result is honored rather than repeated, however badly its
/// process then ended. See [`Runner::session_with_restart`].
///
/// A clean exit carrying a result is absent from this enum on purpose. The
/// harness said what happened and the process did what it was asked to.
#[derive(Debug, Clone, Copy)]
enum Wedged<'a> {
    /// No output at all inside the idle bound.
    Idle,

    /// The process died.
    Crashed(&'a Crash),

    /// A clean exit that never said what the turn did.
    Speechless,
}

impl<'a> Wedged<'a> {
    /// How a finished session's process ended, when it ended badly.
    fn of(consumed: &'a Consumed) -> Option<Self> {
        // An ending the runner chose is not a wedged process. Checked first,
        // because a harness killed mid-word exits like one that died on its own
        // and the difference is which of us stopped it.
        if consumed.cancelled || consumed.exhausted.is_some() {
            return None;
        }

        if consumed.idle {
            return Some(Self::Idle);
        }

        if let Some(crash) = consumed.crashed.as_ref() {
            return Some(Self::Crashed(crash));
        }

        consumed.result.is_none().then_some(Self::Speechless)
    }

    /// The code this ending is reported under, restarted or not.
    ///
    /// One code per ending rather than one per position, so a `recovered` row
    /// and the `fatal` row that may follow it are the same fact told twice.
    const fn code(self) -> ErrorCode {
        match self {
            Self::Idle => ErrorCode::HarnessIdleTimeout,
            Self::Crashed(..) | Self::Speechless => ErrorCode::HarnessCrashed,
        }
    }

    /// What happened, for the incident recording that it is being retried.
    fn restarting(self, bounds: &Bounds) -> String {
        match self {
            Self::Idle => format!(
                "the harness produced no output for {:?} and was restarted",
                bounds.harness_idle
            ),
            Self::Crashed(died) => {
                format!("the harness exited with {} and was restarted", died.status)
            }
            Self::Speechless => {
                "the harness exited cleanly without reporting a result and was restarted".to_owned()
            }
        }
    }

    /// What happened, for the incident beside a result that stands anyway.
    ///
    /// `None` for [`Self::Speechless`], which is by construction never an ending
    /// a result survived: it *is* the absence of one.
    fn despite_a_result(self, bounds: &Bounds) -> Option<String> {
        let message = match self {
            Self::Idle => format!(
                "the harness reported its result and then produced no output for \
                 {:?}, so it was torn down and its result kept",
                bounds.harness_idle
            ),
            Self::Crashed(died) => format!(
                "the harness reported its result and then exited with {}, and the \
                 result it reported stands",
                died.status
            ),
            Self::Speechless => return None,
        };

        Some(message)
    }

    /// How the turn ends when this ending arrives with the restart budget spent.
    ///
    /// `None` for [`Self::Speechless`], which `drive` reports for itself: it
    /// reads the missing result and fails the turn with the reason it has always
    /// given, and naming that failure here as well would be one ending told two
    /// ways.
    fn gave_up(self, bounds: &Bounds, evidence: Option<prost_types::Struct>) -> Option<Failure> {
        let message = match self {
            Self::Idle => format!(
                "the harness produced no output for {:?}, twice, and did not recover \
                 when it was restarted",
                bounds.harness_idle
            ),
            Self::Crashed(died) => format!(
                "the harness exited with {}, and did not recover when it was restarted",
                died.status
            ),
            Self::Speechless => return None,
        };

        Some(Failure::new(self.code(), message).carrying(evidence))
    }
}

/// The turn's wall clock ceiling, shared by every harness session in it.
///
/// Held on the turn rather than restarted per session, because
/// `maxWallClockPerTurn` bounds a turn. A checker fix cycle that started the
/// clock again would let a turn with a five minute ceiling run for fifteen, and
/// the ceiling would still report itself as held.
#[derive(Debug)]
struct WallClock {
    warn_at: tokio::time::Instant,
    deadline: tokio::time::Instant,

    /// Whether the eighty percent warning has already gone out. One warning per
    /// turn, not one per session.
    warned: bool,
}

impl WallClock {
    /// Starts the turn's clock now.
    fn starting_now(ceilings: &Ceilings) -> Self {
        let limit = ceilings.wall_clock_per_turn;
        let started = tokio::time::Instant::now();

        Self {
            warn_at: started + limit.map_or(NEVER, |limit| limit.mul_f64(WARN_AT_FRACTION)),
            deadline: started + limit.unwrap_or(NEVER),
            warned: false,
        }
    }
}

/// Everything every harness session in one turn has to share.
///
/// Grouped rather than threaded through as six parameters, because a checker fix
/// cycle is another session in the same turn and each of these has to be the
/// same object it already was. A second grant would spend outside the ceiling, a
/// second meter would restart the count, and a second wall clock would extend
/// the deadline. Passing one value makes that structural instead of remembered.
#[derive(Debug)]
struct TurnContext {
    harness: Harness,
    working_dir: PathBuf,

    /// The turn's admission to the model, minted once and revoked once.
    access: ModelAccess,

    clock: WallClock,

    /// The bounds this thread declared, or the documented defaults.
    bounds: Bounds,

    /// How many times this turn may still start its harness again.
    ///
    /// One budget for the turn, spent by whichever ending reaches it first. See
    /// [`RESTARTS_ALLOWED`] for why the causes share it, and why it sits here
    /// beside the grant, the meter, and the wall clock rather than inside the
    /// loop that spends it.
    restarts_left: u32,

    /// The thread's secrets, masked out of everything the turn emits.
    ///
    /// Carried here alongside the grant and the meter for the same reason they
    /// are: a session that compiled its own would be a second automaton over
    /// the same settings, and two of anything is one more thing that can
    /// disagree.
    redactor: Redactor,

    /// Budget crossings the proxy found while counting this turn's requests.
    crossings: mpsc::UnboundedReceiver<Crossing>,

    /// Incidents the proxy found while relaying this turn's requests.
    ///
    /// The proxy holds no database on purpose, so what it sees arrives here, at
    /// the one thing that owns the turn and can record against it.
    incidents: mpsc::UnboundedReceiver<Incident>,
}

/// What the checker stage amounted to.
#[derive(Debug)]
struct Checked {
    /// The last attempt's results, which is the state the turn ended in.
    ///
    /// Earlier attempts are not accumulated here. Each one reached the stream as
    /// it happened, and a result list holding three rounds of the same command
    /// would answer "did the checkers pass" with a history instead of a state.
    results: Vec<CheckerResult>,

    outcome: StageOutcome,

    /// A ceiling a fix cycle crossed, which ends the turn rather than the stage.
    exhausted: Option<Ceiling>,

    /// Whether a fix cycle was cancelled out from under the turn.
    cancelled: bool,
}

impl Default for Checked {
    fn default() -> Self {
        Self {
            results: Vec::new(),
            outcome: checker_stage(StageDisposition::Skipped, None, None),
            exhausted: None,
            cancelled: false,
        }
    }
}

impl Checked {
    /// The stage did not run, and says why.
    fn skipped(reason: &str) -> Self {
        Self {
            outcome: checker_stage(StageDisposition::Skipped, Some(reason.to_owned()), None),
            ..Self::default()
        }
    }

    /// Every checker passed.
    fn ran(&mut self, elapsed: std::time::Duration) {
        self.outcome = checker_stage(StageDisposition::Ran, None, Some(elapsed));
    }

    /// The stage ended with something still red, and says what.
    fn failed(&mut self, elapsed: std::time::Duration, reason: String) {
        self.outcome = checker_stage(StageDisposition::Failed, Some(reason), Some(elapsed));
    }
}

/// Why a fix cycle ended the stage rather than producing another attempt.
#[derive(Debug)]
struct StoppedEarly {
    reason: String,
    exhausted: Option<Ceiling>,
    cancelled: bool,
}

/// Claims queued turns and runs them.
#[derive(Debug, Clone)]
pub struct Runner {
    store: Store,
    workspace_root: PathBuf,

    /// Nudged when a turn is queued, so the common case does not wait for the
    /// idle poll.
    notify: Arc<Notify>,

    /// One permit per concurrently running thread.
    capacity: Arc<Semaphore>,

    /// Collects threads that asked to be deleted the moment their work is done.
    collector: Arc<crate::collector::Collector>,

    /// The chokepoint every model request traverses.
    proxy: crate::proxy::LlmProxy,
}

impl Runner {
    /// Builds a runner bounded by `max_concurrent_threads`.
    #[must_use]
    pub fn new(
        store: Store,
        workspace_root: PathBuf,
        notify: Arc<Notify>,
        max_concurrent_threads: u32,
        collector: Arc<crate::collector::Collector>,
        proxy: crate::proxy::LlmProxy,
    ) -> Self {
        Self {
            store,
            workspace_root,
            notify,
            capacity: Arc::new(Semaphore::new(max_concurrent_threads as usize)),
            collector,
            proxy,
        }
    }

    /// Runs the dispatch loop until the process ends.
    pub async fn dispatch(self) {
        loop {
            // Held across the whole turn, so the cap counts turns in flight
            // rather than turns started.
            let Ok(permit) = Arc::clone(&self.capacity).acquire_owned().await else {
                return;
            };

            match self.store.claim_next_turn().await {
                Ok(Some(claimed)) => {
                    let runner = self.clone();
                    tokio::spawn(async move {
                        // A turn's future is large by nature: it holds the
                        // turn's context, its reading loop, and its checker
                        // stage at once. Exactly one is created per turn,
                        // beside a process spawn, so putting it on the heap
                        // costs nothing measurable and keeps a turn's whole
                        // state off this task's stack.
                        Box::pin(runner.run(claimed)).await;
                        drop(permit);
                    });
                }
                Ok(None) => {
                    drop(permit);
                    tokio::select! {
                        () = self.notify.notified() => {}
                        () = tokio::time::sleep(IDLE_POLL) => {}
                    }
                }
                Err(error) => {
                    drop(permit);
                    tracing::error!(
                        event.name = "runner.claim.failed",
                        "could not claim a turn: {error}",
                    );
                    tokio::time::sleep(IDLE_POLL).await;
                }
            }
        }
    }

    /// Runs one claimed turn to completion.
    async fn run(&self, claimed: ClaimedTurn) {
        let turn_id = claimed.turn.turn_id.clone();
        let thread_id = claimed.turn.thread_id.clone();

        tracing::info!(
            event.name = "turn.started",
            thread.id = %thread_id,
            turn.id = %turn_id,
            "starting a turn",
        );

        self.append(
            Attribution::of(&claimed),
            "turn.started",
            None,
            Payload::TurnStarted(TurnStarted {
                turn: Some(claimed.turn.clone()),
            }),
        )
        .await;

        let (status, mut result) = match self.drive(&claimed).await {
            Ok(finished) => finished,
            Err(failure) => {
                // Fatal by construction: `drive` only returns an error when the
                // turn could not go on. A failure the proxy already recorded is
                // skipped here rather than written twice.
                if !failure.recorded {
                    self.record_incident_with(
                        Attribution::of(&claimed),
                        failure.code,
                        Disposition::Fatal,
                        failure.retryable,
                        &failure.message,
                        failure.details.clone(),
                    )
                    .await;
                }

                (TurnStatus::Failed, failure.into_result(&claimed))
            }
        };

        // Counted once, here, rather than inside `assemble`. This is the one
        // point every ending passes through, and it is past the last incident
        // any of them records, so a spent ceiling and a harness that never
        // launched are counted too rather than only the tidy endings.
        result.incident_counts = Some(self.counted_incidents(&turn_id).await);

        // Masked once, here, rather than at each of the two places the result
        // goes. The durable copy and the streamed one are then the same masked
        // text, and a summary quoting a credential cannot reach one of them
        // intact because somebody wired a new consumer later.
        claimed.redactor.redact_turn_result(&mut result);

        if let Err(error) = self.store.finish_turn(&turn_id, status, &result).await {
            tracing::error!(
                event.name = "turn.finish.failed",
                turn.id = %turn_id,
                "could not record the turn result: {error}",
            );
        }

        self.append(
            Attribution::of(&claimed),
            "turn.completed",
            None,
            Payload::TurnCompleted(TurnCompleted {
                result: Some(result),
            }),
        )
        .await;

        tracing::info!(
            event.name = "turn.completed",
            thread.id = %thread_id,
            turn.id = %turn_id,
            turn.status = ?status,
            "turn finished",
        );

        self.collect_if_finished(&thread_id).await;
    }

    /// Collects a thread that asked to be deleted once its work is done.
    ///
    /// Checked after the turn is recorded rather than before, so the result is
    /// durable and has already reached the stream. A caller watching for the
    /// completion still sees it, and then sees the thread end.
    ///
    /// The queue has to be empty. `delete_on_complete` means the thread is
    /// finished, and a thread with three turns still waiting is not, however
    /// complete the one that just ended was.
    async fn collect_if_finished(&self, thread_id: &str) {
        let Ok(thread) = self.store.thread(thread_id).await else {
            return;
        };

        let asked = thread
            .settings
            .as_ref()
            .is_some_and(|settings| settings.delete_on_complete);

        if !asked || thread.queue_depth > 0 {
            return;
        }

        if let Err(error) = self
            .collector
            .collect(thread_id, ThreadEndReason::Completed)
            .await
        {
            tracing::error!(
                event.name = "thread.collect.failed",
                thread.id = thread_id,
                "delete_on_complete was set but the thread could not be collected: {error}",
            );
        }
    }

    /// Runs the turn's harness sessions and decides what they amounted to.
    ///
    /// Usually one session, and sometimes more: a failing checker resumes the
    /// agent to fix it, and a harness that hung is started again. Every one of
    /// those runs inside this function on purpose,
    /// because the proxy grant, the meter, and the wall clock are all withdrawn
    /// or restarted at its edges. A fix cycle outside it would be a second turn
    /// wearing the first one's name, spending past the ceiling the first one set.
    async fn drive(&self, claimed: &ClaimedTurn) -> Result<(TurnStatus, TurnResult), Failure> {
        let ceilings = Ceilings::from_budget(claimed.settings.budget.as_ref());
        let working_dir = self.prepare(claimed, &ceilings).await?;

        // The guard is held here rather than inside `open`, because the grant
        // has to outlive every session in the turn, the checker fix cycle
        // included, and be withdrawn however the turn ends.
        let (mut context, _grant) = self.open(claimed, &ceilings, working_dir).await;

        let consumed = self
            .session_with_restart(claimed, &mut context, &claimed.turn.prompt)
            .await?;

        if consumed.cancelled {
            return Ok((
                TurnStatus::Cancelled,
                assemble(
                    claimed,
                    None,
                    TurnStatus::Cancelled,
                    &Checked::skipped("the turn was cancelled before the checkers ran"),
                ),
            ));
        }

        if let Some(ceiling) = consumed.exhausted {
            return Ok((
                TurnStatus::Failed,
                self.stopped_by(
                    claimed,
                    consumed.result,
                    ceiling,
                    &Checked::skipped("the budget was spent before the checkers ran"),
                )
                .await,
            ));
        }

        let mut reported_result = consumed.result;

        let turn_status = match reported_result.as_ref() {
            Some(result) if result.is_error => TurnStatus::Failed,
            Some(_reported) => TurnStatus::Completed,
            // A clean exit with no result line means the harness ended without
            // saying what it did, which is a defect worth naming rather than
            // reporting as success.
            None => TurnStatus::Failed,
        };

        if reported_result.is_none() {
            self.record_incident(
                Attribution::of(claimed),
                ErrorCode::HarnessCrashed,
                Disposition::Fatal,
                retryable(ErrorCode::HarnessCrashed),
                "the harness exited cleanly without reporting a result",
            )
            .await;
        }

        // Checkers verify work an agent claimed to have finished. A turn whose
        // harness crashed or reported an error made no such claim, and resuming
        // the session that just failed would spend two more of them proving it.
        let checked = if turn_status == TurnStatus::Completed {
            self.check(claimed, &mut context, &mut reported_result)
                .await
        } else {
            Checked::skipped("the harness did not finish, so there was nothing to verify")
        };

        // A ceiling crossed while fixing a checker ends the turn the same way it
        // would have during the work itself, with the code for the ceiling that
        // did it. The checker results already collected go with it.
        if let Some(ceiling) = checked.exhausted {
            return Ok((
                TurnStatus::Failed,
                self.stopped_by(claimed, reported_result, ceiling, &checked)
                    .await,
            ));
        }

        // A checker that is still red does not fail the turn: the turn reached
        // the end of the stack, which is what COMPLETED means, and `stages` plus
        // the incident say what the checkers did. A cancellation during a fix
        // cycle does, because nothing after it ran.
        let turn_status = if checked.cancelled {
            TurnStatus::Cancelled
        } else {
            turn_status
        };

        Ok((
            turn_status,
            assemble(claimed, reported_result, turn_status, &checked),
        ))
    }

    /// Everything that has to hold before a harness is spawned.
    ///
    /// Returns the thread's working directory, which is where every session in
    /// the turn runs.
    async fn prepare(
        &self,
        claimed: &ClaimedTurn,
        ceilings: &Ceilings,
    ) -> Result<PathBuf, Failure> {
        let thread_id = &claimed.turn.thread_id;

        // Checked before anything is spawned. A thread that has already spent
        // its lifetime cost has nothing left to run a turn with, and launching
        // a harness to discover that would spend more of it.
        if self
            .cost_ceiling_reached(Attribution::of(claimed), ceilings)
            .await
        {
            return Err(Failure::new(
                ErrorCode::BudgetCostExhausted,
                "the thread has spent maxCostPerThread, so no further turns can run",
            ));
        }

        // Handed to the agent account, because the harness that works in it runs
        // as that account and a root-owned workspace is one it cannot write to.
        let working_dir = self.workspace_root.join(thread_id);
        crate::privilege::create_dir_for_agent(&working_dir)
            .await
            .map_err(|error| {
                Failure::new(
                    ErrorCode::HarnessLaunchFailed,
                    format!("could not create the thread workspace: {error}"),
                )
            })?;

        Ok(working_dir)
    }

    /// Mints the turn's admission to the model, and the context its sessions
    /// share.
    ///
    /// Minted per turn and withdrawn when the returned guard drops, whatever the
    /// turn does. Every model request a session makes goes through the
    /// satellite, which is what makes counting and ceilings arithmetic rather
    /// than a request.
    ///
    /// The meter is shared with the proxy: the proxy adds up what each response
    /// reported, and the crossings it finds arrive on the channel here, at the
    /// one thing that knows how to end a turn.
    async fn open(
        &self,
        claimed: &ClaimedTurn,
        ceilings: &Ceilings,
        working_dir: PathBuf,
    ) -> (TurnContext, RevokeOnDrop) {
        let (crossings, reported) = mpsc::unbounded_channel();
        let (incidents, reported_incidents) = mpsc::unbounded_channel();
        let meter = Arc::new(Meter::new(ceilings, crossings));
        let bounds = Bounds::for_thread(&claimed.settings);

        // Every endpoint the thread declared, in the order the contract promises
        // they are tried. Resolving the list here rather than one destination is
        // what makes failover the proxy's to perform.
        let route = crate::proxy::failover::Route::resolve(&claimed.settings.models);
        let token = self
            .proxy
            .grant(
                crate::proxy::Grant::new(
                    &claimed.turn.thread_id,
                    &claimed.turn.turn_id,
                    route,
                    Arc::clone(&meter),
                )
                .bounded(bounds.llm_request)
                .reporting_to(incidents),
            )
            .await;

        let context = TurnContext {
            harness: Harness::try_from(claimed.settings.harness).unwrap_or(Harness::Claude),
            working_dir,
            access: ModelAccess {
                base_url: self.proxy.base_url_for(&token),
                token: token.clone(),
            },
            clock: WallClock::starting_now(ceilings),
            bounds,
            restarts_left: RESTARTS_ALLOWED,
            redactor: claimed.redactor.clone(),
            crossings: reported,
            incidents: reported_incidents,
        };

        let grant = RevokeOnDrop {
            proxy: self.proxy.clone(),
            token,
        };

        (context, grant)
    }

    /// Runs a harness session, starting it once more if the process wedged.
    ///
    /// A harness that said nothing inside its idle bound, one that died, and one
    /// that exited cleanly having reported no result are all stopped processes,
    /// and a stopped process is exactly what a restart replaces. It resumes the
    /// same session under the same grant, so the second attempt continues the
    /// conversation rather than starting one, and spends against the ceilings the
    /// turn already set.
    ///
    /// **Once, never twice, and once for the turn.** See [`RESTARTS_ALLOWED`].
    /// The second ending fails the turn with the code for what it was, both of
    /// which are retryable: the turn is worth running again, just not inside
    /// this one.
    ///
    /// **A session that already reported its result is honored, not repeated.**
    /// The harness made a statement about the work, and a second session would
    /// reach the same answer by the same route, so a process that then exits
    /// nonzero or has to be torn down is recorded as `degraded` and costs the
    /// turn nothing. Spending the restart there would discard an answer the
    /// satellite was given and leave the next session with no budget for an
    /// ending that a restart can actually recover.
    ///
    /// [`Wedged::Speechless`] is the one ending that leaves through the happy
    /// path rather than as a failure. `drive` reads a missing result and fails
    /// the turn with the reason it has always given, and returning a failure
    /// here would rename that ending for the sake of sharing this loop.
    async fn session_with_restart(
        &self,
        claimed: &ClaimedTurn,
        context: &mut TurnContext,
        prompt: &str,
    ) -> Result<Consumed, Failure> {
        let thread_id = &claimed.turn.thread_id;
        let turn_id = &claimed.turn.turn_id;

        loop {
            // Rebuilt per attempt rather than reused, because `session_for` is
            // what decides between opening a session and resuming one. A harness
            // that got far enough to report its session id is resumed into it,
            // which is the "same context" a restart is supposed to preserve, and
            // one that died before announcing it opens a session again under the
            // id the first attempt was given.
            let command = self.command_for_turn(claimed, context, prompt).await;
            let consumed = self
                .run_session(&command, thread_id, turn_id, context)
                .await?;

            let Some(wedged) = Wedged::of(&consumed) else {
                return Ok(consumed);
            };

            // A result already reported is an answer, and how the process
            // carrying it then stopped is a fact about the process. The answer
            // is kept, the ending is written down, and the restart stays
            // available for a session that has said nothing to lose.
            if consumed.result.is_some() {
                self.note_messy_exit(claimed, &context.bounds, wedged, &consumed)
                    .await;
                return Ok(consumed);
            }

            // The exit code and the last output, captured whichever way this
            // ends, so the incident carries the same evidence whether the
            // restart recovered the turn or the turn gave up.
            let evidence = evidence_of(&consumed);
            let code = wedged.code();

            if context.restarts_left == 0 {
                return match wedged.gave_up(&context.bounds, evidence) {
                    Some(failure) => Err(failure),
                    None => Ok(consumed),
                };
            }

            context.restarts_left -= 1;
            let message = wedged.restarting(&context.bounds);

            tracing::warn!(
                event.name = "turn.harness.restarted",
                thread.id = thread_id,
                turn.id = turn_id,
                harness.ending = code.as_str_name(),
                "restarting a harness session that ended with {{harness.ending}}: {message}",
            );

            // Recovered, and recorded because it recovered. A restart that
            // worked looks exactly like a turn that never stalled, and a harness
            // that wedges on every turn is a pattern nobody sees unless the
            // recovery is written down.
            self.record_incident_with(
                Attribution::of(claimed),
                code,
                Disposition::Recovered,
                retryable(code),
                &message,
                evidence,
            )
            .await;
        }
    }

    /// Records a session that reported its result and then ended badly anyway.
    ///
    /// **Degraded**, which is the disposition for work that happened with
    /// something about it missing. It is not `fatal`, because the turn keeps the
    /// answer and goes on to its checkers; it is not `recovered`, because
    /// nothing was recovered and no session was spent trying.
    ///
    /// It carries the evidence a crash carries, through the same plumbing: "the
    /// harness answered and then died on the way out" is a diagnosis nobody
    /// reaches from an event log that simply stops, and the exit status and the
    /// tail are the only place the answer to "died of what" survives.
    async fn note_messy_exit(
        &self,
        claimed: &ClaimedTurn,
        bounds: &Bounds,
        wedged: Wedged<'_>,
        consumed: &Consumed,
    ) {
        let Some(message) = wedged.despite_a_result(bounds) else {
            return;
        };

        tracing::warn!(
            event.name = "turn.harness.exited_badly",
            thread.id = %claimed.turn.thread_id,
            turn.id = %claimed.turn.turn_id,
            harness.ending = wedged.code().as_str_name(),
            "keeping the result of a harness session that ended with \
             {{harness.ending}}: {message}",
        );

        self.record_incident_with(
            Attribution::of(claimed),
            wedged.code(),
            Disposition::Degraded,
            // Not worth submitting again: the work is done and reported, so a
            // second turn would redo it rather than fix anything.
            false,
            &message,
            evidence_of(consumed),
        )
        .await;
    }

    /// Builds the command one session of this turn runs.
    ///
    /// One builder for the work, for a checker fix cycle, and for a restart, so
    /// the three cannot drift into launching the harness three slightly
    /// different ways.
    async fn command_for_turn(
        &self,
        claimed: &ClaimedTurn,
        context: &TurnContext,
        prompt: &str,
    ) -> HarnessCommand {
        let session = self.session_for(&claimed.turn.thread_id).await;

        command_for(
            context.harness,
            prompt,
            &session,
            context.working_dir.clone(),
            Some(context.access.clone()),
            &claimed.settings.env,
            // Read per turn rather than held on the runner, so a thread's
            // posture is whatever its settings say now.
            claimed.settings.permissions.as_ref(),
        )
    }

    /// Runs one harness process to whatever end, and tears it down.
    ///
    /// A turn is one of these, or several: the first runs the work and any that
    /// follow answer a failing checker or replace one that hung. Each is driven
    /// identically, through the grant, meter, and wall clock the turn already
    /// holds.
    async fn run_session(
        &self,
        command: &HarnessCommand,
        thread_id: &str,
        turn_id: &str,
        context: &mut TurnContext,
    ) -> Result<Consumed, Failure> {
        let (mut child, stdout, stderr) = spawn_harness(command)?;

        let mut consumed = self
            .consume(stdout, stderr, thread_id, turn_id, context)
            .await;

        // Ahead of everything else, because a session the proxy ended has
        // nothing further to say and a restart would only spend another one
        // reaching the same wall.
        if let Some(failure) = consumed.failed.take() {
            drop(child.start_kill());
            return Err(failure);
        }

        if consumed.cancelled || consumed.exhausted.is_some() || consumed.idle {
            // Asked to stop cooperatively first. `kill_on_drop` is the backstop
            // for the case where it does not.
            drop(child.start_kill());
            return Ok(consumed);
        }

        let status = child.wait().await.map_err(|error| {
            Failure::new(
                ErrorCode::HarnessCrashed,
                format!("could not wait on the harness: {error}"),
            )
        })?;

        if !status.success() {
            // Reported rather than raised. A process that died is a fact about
            // a process, and [`Runner::session_with_restart`] is what decides
            // whether this turn has a restart left to spend on it.
            consumed.crashed = Some(Crash {
                code: status.code(),
                status: status.to_string(),
            });
        }

        Ok(consumed)
    }

    /// Runs the thread's checkers, waking the agent back up while any fail.
    ///
    /// This is the verification that turns "the agent said it was done" into
    /// something checked. A nonzero exit is a normal outcome routed back to the
    /// agent, not a crash: it resumes the same session with the failing commands
    /// and their output, and is asked either to fix them or to say why the
    /// failure should stand.
    ///
    /// The loop is capped by [`checkers::MAX_FIX_ATTEMPTS`]. Past the cap the
    /// turn completes with the failures recorded and a `CHECKER_FAILED`
    /// incident, because a check that will not go green is a fact about the work
    /// rather than a reason to throw the work away.
    ///
    /// `result` is the turn's running harness accounting, which every fix
    /// session is folded into. See [`accounting`] for why that matters.
    async fn check(
        &self,
        claimed: &ClaimedTurn,
        context: &mut TurnContext,
        result: &mut Option<HarnessResult>,
    ) -> Checked {
        let thread_id = &claimed.turn.thread_id;
        let turn_id = &claimed.turn.turn_id;

        let Ok(repos_root) = crate::workspace::repos_directory(&self.workspace_root, thread_id)
        else {
            return Checked::skipped("the thread's workspace path is not usable");
        };

        let declared = checkers::declared(&claimed.settings.repos, &repos_root);

        // The whole cost of this stage for a thread that declared no checker:
        // one filter over its repos, and a stage reported as skipped rather than
        // omitted so "not run" never reads as "found nothing".
        if declared.is_empty() {
            return Checked::skipped("no repo declares a checker");
        }

        // The same variables the agent works under, because a lint that needs a
        // registry token needs it whoever is running it.
        let execution = crate::commands::Execution::for_thread(&claimed.settings);
        let started = tokio::time::Instant::now();
        let mut checked = Checked::default();

        for attempt in 0..=checkers::MAX_FIX_ATTEMPTS {
            let outcomes = checkers::run_all(&declared, &execution).await;
            self.publish_checkers(Attribution::of(claimed), &outcomes)
                .await;

            checked.results = outcomes
                .iter()
                .map(|outcome| outcome.result.clone())
                .collect();

            let failures: Vec<&checkers::Outcome> = outcomes
                .iter()
                .filter(|outcome| !outcome.passed())
                .collect();

            if failures.is_empty() {
                checked.ran(started.elapsed());
                return checked;
            }

            if attempt == checkers::MAX_FIX_ATTEMPTS {
                break;
            }

            if let Some(stopped) = self.fix(claimed, context, result, &failures).await {
                checked.exhausted = stopped.exhausted;
                checked.cancelled = stopped.cancelled;
                checked.failed(started.elapsed(), stopped.reason);
                return checked;
            }
        }

        let still_failing = checked
            .results
            .iter()
            .filter(|result| result.exit_code != 0)
            .count();
        let reason = format!(
            "{still_failing} checker commands were still failing after \
             {} fix attempts",
            checkers::MAX_FIX_ATTEMPTS
        );

        // Degraded rather than blocked. `blocked` is for a permission gate that
        // closed as designed, and no gate closed here: the work happened and
        // finished with its verification missing, which is what `degraded`
        // names. Recording it as blocked would put a red build in the same query
        // an operator runs to find an allowlist that needs widening.
        self.record_incident(
            Attribution::of(claimed),
            ErrorCode::CheckerFailed,
            Disposition::Degraded,
            // The same commands will exit the same way against the same code.
            false,
            &reason,
        )
        .await;

        tracing::warn!(
            event.name = "turn.checkers.failed",
            thread.id = thread_id,
            turn.id = turn_id,
            checker.failures = still_failing,
            "{{checker.failures}} checker commands are still failing, the turn is ending with them recorded",
        );

        checked.failed(started.elapsed(), reason);
        checked
    }

    /// Resumes the agent to answer a failing checker, once.
    ///
    /// Returns `None` when the session ran and the checkers are worth trying
    /// again, and [`StoppedEarly`] when something ended the stage instead.
    ///
    /// Resumed rather than started fresh. The agent that wrote the code is the
    /// one that can fix it, and a clean context would meet a failing lint with
    /// no idea what the work was for.
    async fn fix(
        &self,
        claimed: &ClaimedTurn,
        context: &mut TurnContext,
        result: &mut Option<HarnessResult>,
        failures: &[&checkers::Outcome],
    ) -> Option<StoppedEarly> {
        let thread_id = &claimed.turn.thread_id;
        let turn_id = &claimed.turn.turn_id;

        tracing::info!(
            event.name = "turn.checkers.fixing",
            thread.id = thread_id,
            turn.id = turn_id,
            checker.failures = failures.len(),
            "resuming the agent to answer {{checker.failures}} failing checker commands",
        );

        match self
            .session_with_restart(claimed, context, &checkers::fix_prompt(failures))
            .await
        {
            Ok(consumed) => {
                // Folded before anything else is decided: what a fix session
                // spent is spent whether or not it fixed anything.
                if let Some(later) = consumed.result {
                    accounting::fold(result, later);
                }

                if consumed.cancelled {
                    return Some(StoppedEarly {
                        reason: "the turn was cancelled while the checkers were being fixed"
                            .to_owned(),
                        exhausted: None,
                        cancelled: true,
                    });
                }

                consumed.exhausted.map(|ceiling| StoppedEarly {
                    reason: format!(
                        "the turn reached {} while the checkers were being fixed",
                        ceiling.as_str_name()
                    ),
                    exhausted: Some(ceiling),
                    cancelled: false,
                })
            }
            Err(failure) => {
                // Degraded rather than fatal: the work the turn already did
                // survives on disk, and what is missing is its verification. A
                // failure the proxy already recorded keeps the disposition it
                // arrived with rather than being written down twice.
                if !failure.recorded {
                    self.record_incident_with(
                        Attribution::of(claimed),
                        failure.code,
                        Disposition::Degraded,
                        failure.retryable,
                        &failure.message,
                        failure.details.clone(),
                    )
                    .await;
                }

                Some(StoppedEarly {
                    reason: format!(
                        "the agent could not be resumed to fix the checkers: {}",
                        failure.message
                    ),
                    exhausted: None,
                    cancelled: false,
                })
            }
        }
    }

    /// Puts every checker command's outcome on the thread's stream.
    ///
    /// Per command rather than per attempt, so a consumer watching a long build
    /// sees it finish rather than learning about the whole stage at the end.
    async fn publish_checkers(&self, at: Attribution<'_>, outcomes: &[checkers::Outcome]) {
        for outcome in outcomes {
            tracing::info!(
                event.name = "turn.checker.finished",
                thread.id = at.thread_id,
                turn.id = at.turn_id,
                checker.repo = outcome.repo,
                checker.command = outcome.result.command,
                checker.exit_code = outcome.result.exit_code,
                "{{checker.command}} in {{checker.repo}} exited with {{checker.exit_code}}",
            );

            self.append(
                at,
                "checker.result",
                None,
                Payload::CheckerResult(CheckerResultEvent {
                    repo: outcome.repo.clone(),
                    result: Some(outcome.result.clone()),
                }),
            )
            .await;
        }
    }

    /// Reads the harness's output until it ends, is cancelled, runs out of
    /// budget, or goes silent.
    ///
    /// Several things can end this loop and only one of them is the harness
    /// finishing, which is why it is a `select!` rather than a read loop with
    /// checks bolted on. A ceiling reached ten seconds into a thirty minute
    /// completion has to be acted on then, not when the next line happens to
    /// arrive, and a harness that will never produce another line has to be
    /// noticed by something other than the read that is waiting on it.
    ///
    /// **Both pipes count as output.** The idle bound is the README's "no output
    /// at all", and a harness writing progress to stderr while stdout stays
    /// quiet is working. Reading stderr is also what keeps its pipe from filling
    /// and blocking the process the loop is waiting on.
    async fn consume(
        &self,
        stdout: tokio::process::ChildStdout,
        stderr: tokio::process::ChildStderr,
        thread_id: &str,
        turn_id: &str,
        context: &mut TurnContext,
    ) -> Consumed {
        let mut lines = BufReader::new(stdout).lines();
        let mut complaints = BufReader::new(stderr).lines();
        let mut consumed = Consumed::default();
        let mut seen = 0_usize;

        // Destructured so the fields this loop touches are borrowed separately.
        // `select!` holds a future over the receivers while another branch
        // writes the clock, and one borrow of the whole context could not span
        // both.
        //
        // Wall clock is the runner's to enforce, and it belongs to the turn
        // rather than to this session. The proxy sees requests, not the gaps
        // between them, so a harness stuck in a shell command would never reach
        // it and would outlive any ceiling it counted.
        let TurnContext {
            harness,
            clock,
            bounds,
            redactor,
            crossings,
            incidents,
            ..
        } = context;
        let harness = *harness;

        let at = Attribution {
            thread_id,
            turn_id,
            redactor,
        };

        // Reset by output on either pipe. Unlike the wall clock this belongs to
        // the session rather than to the turn: a fresh process that has said
        // nothing yet has not been idle for however long its predecessor was.
        let idle_bound = bounds.harness_idle;
        let mut silent_since = tokio::time::Instant::now();

        // Stderr can close before stdout, and a closed pipe answers `next_line`
        // with `None` the instant it is asked. Without this guard the branch
        // below would win every race forever and spin the loop at full speed.
        let mut complaining = true;

        loop {
            tokio::select! {
                // `next_line` is cancel safe: `Lines` owns the buffer, so a
                // partial line survives another branch winning the race. Reading
                // through a bare `read_line` here would silently lose whatever
                // had arrived when a budget crossing interrupted it.
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else {
                        break;
                    };

                    silent_since = tokio::time::Instant::now();

                    if line.trim().is_empty() {
                        continue;
                    }

                    consumed.tail.remember(&line);
                    self.absorb(harness, &line, at, &mut consumed).await;

                    seen += 1;
                    if seen.is_multiple_of(CANCEL_CHECK_EVERY)
                        && self.store.is_turn_cancelled(turn_id).await.unwrap_or(false)
                    {
                        consumed.cancelled = true;
                        break;
                    }
                }

                // Stderr never reaches the event log: it is a CLI's diagnostics
                // rather than anything the contract has a shape for. It is read
                // so that it counts as life, and so the pipe cannot fill.
                complaint = complaints.next_line(), if complaining => {
                    let Ok(Some(complaint)) = complaint else {
                        // Closed, or unreadable. Either way there is nothing
                        // further to hear from this pipe, and stdout decides
                        // when the session is over.
                        complaining = false;
                        continue;
                    };

                    silent_since = tokio::time::Instant::now();

                    if !complaint.trim().is_empty() {
                        // Kept as well as traced. Stderr reaches no event, so a
                        // crash that explained itself there would otherwise
                        // leave the incident with nothing to show.
                        consumed.tail.remember(&complaint);

                        tracing::debug!(
                            event.name = "turn.harness.stderr",
                            thread.id = thread_id,
                            turn.id = turn_id,
                            "{complaint}",
                        );
                    }
                }

                Some(crossing) = crossings.recv() => {
                    self.publish_crossing(at, crossing).await;

                    if let Crossing::Reached { ceiling } = crossing {
                        consumed.exhausted = Some(ceiling);
                        break;
                    }
                }

                Some(incident) = incidents.recv() => {
                    if self.absorb_proxy_incident(incident, at, &mut consumed).await {
                        break;
                    }
                }

                () = tokio::time::sleep_until(clock.warn_at), if !clock.warned => {
                    clock.warned = true;
                    self.publish_crossing(at, Crossing::Approaching {
                        ceiling: Ceiling::WallClockPerTurn,
                        percent_used: crate::proxy::budget::WARN_AT_PERCENT,
                    })
                    .await;
                }

                () = tokio::time::sleep_until(clock.deadline) => {
                    consumed.exhausted = Some(Ceiling::WallClockPerTurn);
                    break;
                }

                () = tokio::time::sleep_until(silent_since + idle_bound) => {
                    tracing::warn!(
                        event.name = "turn.harness.idle",
                        thread.id = thread_id,
                        turn.id = turn_id,
                        harness.idle_seconds = idle_bound.as_secs(),
                        "the harness produced no output for {{harness.idle_seconds}} \
                         seconds and is being torn down",
                    );

                    consumed.idle = true;
                    break;
                }
            }
        }

        self.drain_incidents(incidents, at, &mut consumed).await;

        consumed
    }

    /// Records whatever the proxy reported after the reading loop let go.
    ///
    /// A timeout the proxy found on the request that ended a session arrives
    /// while nothing is left to select on, and an incident recorded nowhere is
    /// the silent failure the whole incident system exists to prevent.
    async fn drain_incidents(
        &self,
        incidents: &mut mpsc::UnboundedReceiver<Incident>,
        at: Attribution<'_>,
        consumed: &mut Consumed,
    ) {
        while let Ok(incident) = incidents.try_recv() {
            // The drain keeps going whatever the answer: every incident still
            // in the channel deserves its row, fatal or not.
            let _turn_over = self.absorb_proxy_incident(incident, at, consumed).await;
        }
    }

    /// Stores a proxy incident and notes a fatal one as the turn's failure.
    ///
    /// Returns whether the turn is over. Fatal is the proxy saying the turn has
    /// nowhere left to go, which today means every declared endpoint was given
    /// up on. Reading the disposition rather than the code keeps this from
    /// needing an edit every time the proxy learns a new way to end a turn.
    async fn absorb_proxy_incident(
        &self,
        incident: Incident,
        at: Attribution<'_>,
        consumed: &mut Consumed,
    ) -> bool {
        // Judged before the incident is handed to the unified report path,
        // which takes ownership so the row and the frame are one encode.
        let fatal = incident.disposition == i32::from(Disposition::Fatal);
        if fatal && consumed.failed.is_none() {
            consumed.failed = Some(Failure::from_incident(&incident));
        }

        self.report_incident(incident, at.redactor).await;

        fatal
    }

    /// Turns one line of harness output into log entries and a result.
    async fn absorb(
        &self,
        harness: Harness,
        line: &str,
        at: Attribution<'_>,
        consumed: &mut Consumed,
    ) {
        let mapping = map_line(harness, line);

        // Written down the moment it first arrives, so a process that dies
        // mid-turn still leaves the thread able to resume what it opened, and
        // only then: a harness naming its session on every progress line would
        // otherwise be a database write per line, each storing what the one
        // before it stored.
        if let Some(session_id) = mapping.harness_session_id
            && consumed.first_sighting_of(&session_id, at)
        {
            match self
                .store
                .set_harness_session(at.thread_id, &session_id)
                .await
            {
                // Marked only once the write landed, so a line naming the
                // session again retries what a transient failure lost.
                Ok(()) => consumed.recorded_session_id = Some(session_id),
                Err(error) => tracing::warn!(
                    event.name = "turn.session.unrecorded",
                    thread.id = at.thread_id,
                    "could not record the harness session id: {error}",
                ),
            }
        }

        for event in mapping.events {
            if let Payload::AgentMessage(spoken) = &event.payload {
                consumed.last_agent_message = Some(spoken.text.clone());
            }

            // An incident from the mapper takes the path that records it and
            // streams it in one call, and takes it instead of the append below.
            // Doing both would put two copies of one failure on the stream.
            if let Payload::Incident(incident) = event.payload {
                self.report_incident(attributed(incident, at, event.member_id), at.redactor)
                    .await;
                continue;
            }

            self.append(at, event.type_name, event.member_id, event.payload)
                .await;
        }

        if let Some(mut result) = mapping.result {
            // A harness whose closing message is an item rather than part of its
            // result reports no summary, and a turn that says nothing about
            // itself is a report with a hole in it. The session saw what the
            // agent said last, so it fills the gap the mapper structurally
            // cannot. A harness that reported one keeps it.
            if result.summary.trim().is_empty()
                && let Some(spoken) = consumed.last_agent_message.take()
            {
                result.summary = spoken;
            }

            consumed.result = Some(result);
        }
    }

    /// Whether this turn opens a harness session or resumes the thread's.
    ///
    /// A thread that has already opened one resumes it, so the second turn
    /// remembers the first. Without this a thread would be a series of unrelated
    /// turns rather than a conversation.
    async fn session_for(&self, thread_id: &str) -> Session {
        let existing = self
            .store
            .thread(thread_id)
            .await
            .ok()
            .and_then(|thread| thread.harness_session_id);

        match existing {
            Some(session_id) => Session::Resume { session_id },
            // The thread id doubles as the session id: both are UUIDs, and
            // reusing it means one lookup fewer when correlating a run with the
            // harness transcripts on disk.
            None => Session::Start {
                session_id: thread_id.to_owned(),
            },
        }
    }

    /// The result of a turn a ceiling stopped.
    ///
    /// A graceful stop rather than a crash. Work already committed to a branch
    /// survives, the events the harness produced are already in the log, and the
    /// result carries the code for the ceiling that ended it alongside whatever
    /// the harness had reported by then.
    async fn stopped_by(
        &self,
        claimed: &ClaimedTurn,
        harness: Option<HarnessResult>,
        ceiling: Ceiling,
        checked: &Checked,
    ) -> TurnResult {
        let code = crate::proxy::budget::code_for(ceiling);
        let message = format!(
            "the turn reached {}, so it was stopped and its work preserved",
            ceiling.as_str_name()
        );

        self.record_incident(
            Attribution::of(claimed),
            code,
            Disposition::Fatal,
            // A ceiling does not move by being asked again.
            false,
            &message,
        )
        .await;

        let mut result = assemble(claimed, harness, TurnStatus::Failed, checked);
        result.error = Some(arsox_sdk::proto::error::v1::Error {
            code: code.into(),
            message,
            // A ceiling does not move by being asked again. Retrying costs
            // another turn and ends the same way.
            retryable: false,
            details: None,
            trace_id: None,
        });

        result
    }

    /// Puts a budget crossing where a host application can see it.
    ///
    /// A warning reaches the thread's stream as `budget.warning`, which is the
    /// event the README promises at 80% and the only chance an application has
    /// to react before the wall. A ceiling actually reached produces no warning:
    /// it ends the turn, and the turn's own error names which ceiling did it.
    async fn publish_crossing(&self, at: Attribution<'_>, crossing: Crossing) {
        let (ceiling, percent_used) = match crossing {
            Crossing::Approaching {
                ceiling,
                percent_used,
            } => (ceiling, Some(percent_used)),
            Crossing::Reached { ceiling } => (ceiling, None),
        };

        let Some(percent_used) = percent_used else {
            tracing::info!(
                event.name = "turn.budget.exhausted",
                thread.id = at.thread_id,
                turn.id = at.turn_id,
                budget.ceiling = ceiling.as_str_name(),
                "{{budget.ceiling}} was reached and the turn is being stopped",
            );
            return;
        };

        tracing::info!(
            event.name = "turn.budget.warning",
            thread.id = at.thread_id,
            turn.id = at.turn_id,
            budget.ceiling = ceiling.as_str_name(),
            budget.percent_used = percent_used,
            "{{budget.ceiling}} is {{budget.percent_used}}% consumed",
        );

        self.append(
            at,
            "budget.warning",
            None,
            Payload::BudgetWarning(BudgetWarning {
                ceiling: ceiling.into(),
                percent_used,
            }),
        )
        .await;
    }

    /// Whether the thread has already spent its lifetime cost ceiling.
    ///
    /// Cost is decided at turn boundaries rather than per request, because a
    /// request carries tokens and not a price: nothing in the contract publishes
    /// a rate, so the only cost the satellite actually knows is what each
    /// harness reports when its turn ends. Enforcing at the boundary is the
    /// strongest honest guarantee available, and it is a real one: a thread that
    /// has spent its ceiling runs no further turns.
    async fn cost_ceiling_reached(&self, at: Attribution<'_>, ceilings: &Ceilings) -> bool {
        let Some(ceiling) = ceilings.cost_per_thread.as_ref() else {
            return false;
        };

        // Absent means no finished turn reported a priced cost, which is not the
        // same as having spent nothing. Nothing comparable exists yet, so there
        // is nothing to enforce against.
        let Ok(Some(spent)) = self.store.thread_cost_nanos(at.thread_id).await else {
            return false;
        };

        let Some(used) = crate::proxy::budget::percent_of_cost(ceiling, spent) else {
            tracing::warn!(
                event.name = "turn.budget.incomparable",
                thread.id = at.thread_id,
                budget.currency = ceiling.currency_code,
                "maxCostPerThread is denominated in {{budget.currency}}, which no harness \
                 reports, so it is not being enforced",
            );
            return false;
        };

        if used >= 100 {
            return true;
        }

        if used >= crate::proxy::budget::WARN_AT_PERCENT {
            self.publish_crossing(
                at,
                Crossing::Approaching {
                    ceiling: Ceiling::CostPerThread,
                    percent_used: used,
                },
            )
            .await;
        }

        false
    }

    async fn append(
        &self,
        at: Attribution<'_>,
        type_name: impl Into<String>,
        member_id: Option<String>,
        payload: Payload,
    ) {
        let append = AppendEvent {
            thread_id: at.thread_id.to_owned(),
            turn_id: Some(at.turn_id.to_owned()),
            member_id,
            type_name: type_name.into(),
            occurred_at: None,
            payload,
            redactor: at.redactor.clone(),
        };

        if let Err(error) = self.store.append_event(append).await {
            // Losing an event is a real defect, and the stream is where it would
            // otherwise vanish without trace.
            tracing::error!(
                event.name = "event.append.failed",
                thread.id = %at.thread_id,
                "could not append an event: {error}",
            );
        }
    }

    /// Records an incident, which outlives the turn it belongs to.
    ///
    /// The disposition is the caller's to state rather than assumed. A ceiling
    /// that ended a turn is `fatal` and a ticket that failed to prefetch is
    /// `degraded`, and collapsing the two would make the one query an operator
    /// runs after a bad night useless. `retryable` is the caller's for the same
    /// reason: it is the field an older SDK falls back to when it meets a code
    /// it has never heard of, so guessing it would mislead exactly the client
    /// that has nothing else to go on.
    async fn record_incident(
        &self,
        at: Attribution<'_>,
        code: ErrorCode,
        disposition: Disposition,
        retryable: bool,
        message: &str,
    ) {
        self.record_incident_with(at, code, disposition, retryable, message, None)
            .await;
    }

    /// Records an incident that carries the evidence behind it.
    ///
    /// `details` is what lets somebody act on the incident without reproducing
    /// the run: a crash carries the exit code and the tail of what the process
    /// said before it died. Absent when there is nothing to show, rather than an
    /// empty object pretending there is.
    async fn record_incident_with(
        &self,
        at: Attribution<'_>,
        code: ErrorCode,
        disposition: Disposition,
        retryable: bool,
        message: &str,
        details: Option<prost_types::Struct>,
    ) {
        self.report_incident(
            Incident {
                incident_id: uuid::Uuid::now_v7().to_string(),
                // Filled in by the append, so the row can be located in the
                // stream and the frame looked up afterwards.
                sequence: None,
                thread_id: Some(at.thread_id.to_owned()),
                turn_id: Some(at.turn_id.to_owned()),
                member_id: None,
                code: code.into(),
                disposition: disposition.into(),
                retryable,
                message: message.to_owned(),
                details,
                occurred_at: Some(Timestamp::now()),
            },
            at.redactor,
        )
        .await;
    }

    /// Counts a turn's incidents by disposition, for its report.
    ///
    /// A query rather than a tally kept in the runner, because incidents reach
    /// the database from three places: this loop, the mappers, and the proxy.
    /// A counter here would count the ones it happened to see, and a report that
    /// says "one degraded" when three were recorded is worse than one that says
    /// nothing.
    ///
    /// A count that cannot be read reports zeroes rather than failing the turn.
    /// The incidents themselves are recorded and queryable; this field is the
    /// convenience that saves the common case a round trip.
    async fn counted_incidents(&self, turn_id: &str) -> IncidentCounts {
        match self.store.incident_counts(turn_id).await {
            Ok(counts) => counts,
            Err(error) => {
                tracing::error!(
                    event.name = "incident.count.failed",
                    turn.id = turn_id,
                    "could not count a turn's incidents for its report: {error}",
                );
                IncidentCounts::default()
            }
        }
    }

    /// Records an already assembled incident and puts it on the thread's stream.
    ///
    /// Separate from [`Self::record_incident`] because an incident the proxy or
    /// the mapper built arrives whole: it knows its own code, disposition, and
    /// message, and re-deriving any of those here would let the two disagree.
    ///
    /// One store call writes both copies. Appending the event at each call site
    /// and recording the row separately would be two rules to remember, and the
    /// one that gets forgotten is the stream.
    async fn report_incident(&self, incident: Incident, redactor: &Redactor) {
        if let Err(error) = self.store.report_incident(incident, redactor).await {
            tracing::error!(
                event.name = "incident.record.failed",
                "could not record an incident: {error}",
            );
        }
    }
}

/// Maps one native line with the mapper the thread's harness speaks.
///
/// The whole difference a harness makes to the runner, in one function. Every
/// other line of this loop is written against the canonical contract, which is
/// the property the project exists to hold: adding a harness is a mapper and an
/// arm here, and nothing above it changes.
///
/// A thread that named no harness is read as Claude, which is what
/// `GET /v1/harness` reports as the default.
fn map_line(harness: Harness, line: &str) -> Mapping {
    match harness {
        Harness::Codex => codex::map_line(line),
        Harness::Unspecified | Harness::Claude => claude::map_line(line),
    }
}

/// Fills in what a mapper-produced incident cannot know about itself.
///
/// The mapper reads one native line. It knows the code, the disposition, and
/// what went wrong, and nothing about which thread or turn was reading that
/// line, so the attribution is supplied here rather than invented there. Its own
/// judgement, `retryable` included, is carried through untouched.
fn attributed(mut incident: Incident, at: Attribution<'_>, member_id: Option<String>) -> Incident {
    if incident.incident_id.is_empty() {
        incident.incident_id = uuid::Uuid::now_v7().to_string();
    }

    incident.thread_id = Some(at.thread_id.to_owned());
    incident.turn_id = Some(at.turn_id.to_owned());
    incident.member_id = incident.member_id.or(member_id);

    if incident.occurred_at.is_none() {
        // The native line that caused this is by definition one the mapper could
        // not read, so it carried no timestamp. Arrival time is the honest one.
        incident.occurred_at = Some(Timestamp::now());
    }

    incident
}

/// Starts the harness process with its pipes arranged the way the runner reads
/// them.
///
/// A free function rather than a method: it needs nothing from the runner, and
/// keeping it out of `drive` keeps the one function that has to be read start to
/// finish short enough to read.
fn spawn_harness(
    command: &crate::harness::spawn::HarnessCommand,
) -> Result<
    (
        tokio::process::Child,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    ),
    Failure,
> {
    // Never `Command::new` directly. A spawned process inherits its parent's
    // environment, and the satellite's holds `ARSOX_SECRET`.
    let mut child = process_for(command)
        // The CLI waits on stdin for several seconds otherwise, which looks
        // exactly like a hung process.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            Failure::new(
                ErrorCode::HarnessLaunchFailed,
                format!("could not launch {}: {error}", command.program),
            )
        })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        Failure::new(
            ErrorCode::HarnessLaunchFailed,
            "the harness produced no stdout to read",
        )
    })?;

    // Taken as well as piped. An unread pipe fills its buffer and blocks the
    // process writing to it, which would look exactly like the hang the idle
    // bound is there to catch and would be caused by the satellite.
    let stderr = child.stderr.take().ok_or_else(|| {
        Failure::new(
            ErrorCode::HarnessLaunchFailed,
            "the harness produced no stderr to read",
        )
    })?;

    Ok((child, stdout, stderr))
}

/// The outcome for the checker stage, whatever it did.
///
/// A free function so [`Checked`] can name it before a `Runner` exists, and so
/// the one place the stage's identity is spelled is the one place it is built.
fn checker_stage(
    disposition: StageDisposition,
    reason: Option<String>,
    elapsed: Option<std::time::Duration>,
) -> StageOutcome {
    StageOutcome {
        stage: Stage::Checkers.into(),
        disposition: disposition.into(),
        reason,
        elapsed: elapsed.map(|elapsed| Duration {
            // Saturating rather than wrapping: a stage that ran for longer than
            // an `i64` of seconds has a problem this number would not describe.
            seconds: i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
            // Sub-second nanoseconds always fit.
            nanos: i32::try_from(elapsed.subsec_nanos()).unwrap_or_default(),
        }),
    }
}

/// Folds what the harness and the checkers reported into the full turn result.
///
/// A turn is bigger than a harness run: self-review, artifact scanning, and
/// suggestions are all stages the harness knows nothing about. They are reported
/// as skipped rather than omitted, so "not run" never reads as "found nothing".
///
/// Checkers are the one of those stages that exists, so its outcome comes from
/// `checked` rather than from the unimplemented list.
fn assemble(
    claimed: &ClaimedTurn,
    harness: Option<HarnessResult>,
    status: TurnStatus,
    checked: &Checked,
) -> TurnResult {
    let harness = harness.unwrap_or_default();

    let stages = [
        Stage::Plan,
        Stage::SelfReview,
        Stage::Merge,
        Stage::Artifacts,
        Stage::Suggestions,
    ]
    .into_iter()
    .map(|stage| StageOutcome {
        stage: stage.into(),
        disposition: StageDisposition::Skipped.into(),
        reason: Some("not implemented yet".to_owned()),
        elapsed: None,
    })
    .chain(std::iter::once(StageOutcome {
        stage: Stage::TeamWork.into(),
        disposition: StageDisposition::Ran.into(),
        reason: None,
        elapsed: harness.timing.total,
    }))
    .chain(std::iter::once(checked.outcome.clone()))
    .collect();

    TurnResult {
        turn_id: claimed.turn.turn_id.clone(),
        thread_id: claimed.turn.thread_id.clone(),
        status: status.into(),
        summary: harness.summary,
        tokens: Some(harness.tokens),
        cost: Some(harness.cost),
        by_model: harness.by_model,
        error: None,
        // Filled in by `run` once the turn is over, because an incident this
        // assembly cannot see yet is one it would report as not having happened.
        incident_counts: None,
        members: Vec::new(),
        changed_files: Vec::new(),
        integrations: Vec::new(),
        checker_results: checked.results.clone(),
        artifacts: Vec::new(),
        suggestions: None,
        stages,
        unanswered_questions: Vec::new(),
        watch: None,
        agents_repo_commit: None,
        metadata: claimed.turn.metadata.clone(),
        timing: Some(harness.timing),
        stop_reason: harness.stop_reason.map(Into::into),
        rate_limits: None,
    }
}

/// The evidence behind a wedged session, in the shape `Incident.details` takes.
///
/// The exit code and the last output, which is what the README promises a crash
/// captures. A reader with both can tell an out-of-memory kill from a bad flag
/// without reproducing the run, and neither is recoverable from the event log:
/// the log holds what the harness said, not how it stopped saying it.
///
/// `None` when there is neither, so an incident never carries an empty object
/// pretending to be evidence.
fn evidence_of(consumed: &Consumed) -> Option<prost_types::Struct> {
    let mut fields = std::collections::BTreeMap::new();

    if let Some(crash) = consumed.crashed.as_ref() {
        fields.insert("status".to_owned(), text(crash.status.clone()));

        if let Some(code) = crash.code {
            fields.insert("exit_code".to_owned(), number(f64::from(code)));
        }
    }

    if let Some(tail) = consumed.tail.joined() {
        fields.insert("output_tail".to_owned(), text(tail));
    }

    (!fields.is_empty()).then(|| prost_types::Struct {
        fields: fields.into_iter().collect(),
    })
}

fn text(value: String) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value)),
    }
}

fn number(value: f64) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::NumberValue(value)),
    }
}

/// Whether a turn that ended with `code` is worth submitting again.
///
/// A launch that failed, a harness that died, and a harness that hung are facts
/// about a process rather than about the work, so the same turn submitted again
/// may well succeed. Everything else this runner ends a turn with is a fact
/// about the request or the ceilings, and asking twice reaches the same answer.
///
/// Read by the turn's error and by the incident beside it, so a client that
/// matches on one and a client that falls back to the other are told the same
/// thing.
const fn retryable(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::HarnessLaunchFailed | ErrorCode::HarnessCrashed | ErrorCode::HarnessIdleTimeout
    )
}

/// A reason a turn could not run.
#[derive(Debug)]
struct Failure {
    code: ErrorCode,
    message: String,

    /// Whether this turn is worth submitting again.
    ///
    /// Usually derived from the code, and carried when the failure arrived from
    /// somewhere that had already decided. A proxy incident states its own, and
    /// re-deriving it here would let the turn's error and the incident beside it
    /// disagree about the one field a client falls back to.
    retryable: bool,

    /// Structured evidence, in the shape `Error.details` takes.
    ///
    /// `LLM_ALL_ENDPOINTS_EXHAUSTED` carries `attempts` here, which is the only
    /// place a caller learns why each endpoint was given up on.
    details: Option<prost_types::Struct>,

    /// Whether the incident behind this failure is already in the database.
    ///
    /// A failure the proxy found arrives as a whole incident and is recorded
    /// where it is received. Recording it a second time on the way out would put
    /// one failure in the log twice.
    recorded: bool,
}

impl Failure {
    /// A failure the runner itself decided, with the disposition its code
    /// implies.
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: retryable(code),
            details: None,
            recorded: false,
        }
    }

    /// The same failure, carrying the evidence behind it.
    ///
    /// Separate from [`Self::new`] because most failures have none: a request
    /// that was refused says everything it has to say in its code and message.
    fn carrying(mut self, details: Option<prost_types::Struct>) -> Self {
        self.details = details;
        self
    }

    /// The failure an incident the proxy already recorded amounts to.
    ///
    /// Everything is carried through rather than re-derived: the proxy saw the
    /// failure and this function did not.
    fn from_incident(incident: &Incident) -> Self {
        Self {
            code: ErrorCode::try_from(incident.code).unwrap_or(ErrorCode::Internal),
            message: incident.message.clone(),
            retryable: incident.retryable,
            details: incident.details.clone(),
            recorded: true,
        }
    }

    fn into_result(self, claimed: &ClaimedTurn) -> TurnResult {
        TurnResult {
            turn_id: claimed.turn.turn_id.clone(),
            thread_id: claimed.turn.thread_id.clone(),
            status: TurnStatus::Failed.into(),
            summary: String::new(),
            error: Some(arsox_sdk::proto::error::v1::Error {
                code: self.code.into(),
                message: self.message,
                retryable: self.retryable,
                details: self.details,
                trace_id: None,
            }),
            metadata: claimed.turn.metadata.clone(),
            ..TurnResult::default()
        }
    }
}
