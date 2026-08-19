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

use crate::harness::spawn::{ModelAccess, Session, command_for, process_for};
use crate::harness::{HarnessResult, claude};
use crate::proxy::budget::{Ceilings, Crossing, Meter};
use crate::store::{AppendEvent, ClaimedTurn, Store};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{
    BudgetWarning, Ceiling, ThreadEndReason, TurnCompleted, TurnStarted,
};
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::incident::v1::{Disposition, Incident, IncidentCounts};
use arsox_sdk::proto::turn::v1::{Stage, StageDisposition, StageOutcome, TurnResult, TurnStatus};
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

/// What reading a harness's output produced.
#[derive(Debug, Default)]
struct Consumed {
    result: Option<HarnessResult>,
    cancelled: bool,

    /// The ceiling that stopped the turn, when one did.
    exhausted: Option<Ceiling>,
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
                        runner.run(claimed).await;
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
            &thread_id,
            &turn_id,
            "turn.started",
            None,
            Payload::TurnStarted(TurnStarted {
                turn: Some(claimed.turn.clone()),
            }),
        )
        .await;

        let (status, result) = match self.drive(&claimed).await {
            Ok(finished) => finished,
            Err(failure) => {
                // Fatal by construction: `drive` only returns an error when the
                // turn could not go on.
                self.record_incident(
                    &thread_id,
                    &turn_id,
                    failure.code,
                    Disposition::Fatal,
                    &failure.message,
                )
                .await;
                (TurnStatus::Failed, failure.into_result(&claimed))
            }
        };

        if let Err(error) = self.store.finish_turn(&turn_id, status, &result).await {
            tracing::error!(
                event.name = "turn.finish.failed",
                turn.id = %turn_id,
                "could not record the turn result: {error}",
            );
        }

        self.append(
            &thread_id,
            &turn_id,
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

    /// Spawns the harness and decides what its run amounted to.
    async fn drive(&self, claimed: &ClaimedTurn) -> Result<(TurnStatus, TurnResult), Failure> {
        let thread_id = &claimed.turn.thread_id;
        let turn_id = &claimed.turn.turn_id;

        let ceilings = Ceilings::from_budget(claimed.settings.budget.as_ref());

        // Checked before anything is spawned. A thread that has already spent
        // its lifetime cost has nothing left to run a turn with, and launching
        // a harness to discover that would spend more of it.
        if self
            .cost_ceiling_reached(thread_id, turn_id, &ceilings)
            .await
        {
            return Err(Failure {
                code: ErrorCode::BudgetCostExhausted,
                message: "the thread has spent maxCostPerThread, so no further turns can run"
                    .to_owned(),
            });
        }

        let working_dir = self.workspace_root.join(thread_id);
        tokio::fs::create_dir_all(&working_dir)
            .await
            .map_err(|error| Failure {
                code: ErrorCode::HarnessLaunchFailed,
                message: format!("could not create the thread workspace: {error}"),
            })?;

        let session = self.session_for(thread_id).await;
        let harness = Harness::try_from(claimed.settings.harness).unwrap_or(Harness::Claude);

        // Minted per turn and withdrawn below, whatever the turn does. Every
        // model request this harness makes goes through the satellite, which is
        // what makes counting and ceilings arithmetic rather than a request.
        //
        // The meter is shared with the proxy: the proxy adds up what each
        // response reported, and the crossings it finds arrive here, on the one
        // thing that knows how to end a turn.
        let (crossings, mut reported) = mpsc::unbounded_channel();
        let meter = Arc::new(Meter::new(&ceilings, crossings));

        let upstream = crate::proxy::upstream::Upstream::resolve(&claimed.settings.models);
        let token = self
            .proxy
            .grant(thread_id, turn_id, upstream, Arc::clone(&meter))
            .await;

        let command = command_for(
            harness,
            &claimed.turn.prompt,
            &session,
            working_dir,
            Some(ModelAccess {
                base_url: self.proxy.base_url_for(&token),
                token: token.clone(),
            }),
        );

        // Revoked on every path out of this function, including the early
        // returns for cancellation and failure. A grant that outlived its turn
        // would keep spending after the work stopped.
        let _grant = RevokeOnDrop {
            proxy: self.proxy.clone(),
            token,
        };

        let (mut child, stdout) = spawn_harness(&command)?;

        let consumed = self
            .consume(stdout, thread_id, turn_id, &ceilings, &mut reported)
            .await;

        if consumed.cancelled {
            // Asked to stop cooperatively first. `kill_on_drop` is the backstop
            // for the case where it does not.
            drop(child.start_kill());
            return Ok((
                TurnStatus::Cancelled,
                assemble(claimed, None, TurnStatus::Cancelled),
            ));
        }

        if let Some(ceiling) = consumed.exhausted {
            drop(child.start_kill());
            return Ok((
                TurnStatus::Failed,
                self.stopped_by(claimed, consumed.result, ceiling).await,
            ));
        }

        let status = child.wait().await.map_err(|error| Failure {
            code: ErrorCode::HarnessCrashed,
            message: format!("could not wait on the harness: {error}"),
        })?;

        if !status.success() {
            return Err(Failure {
                code: ErrorCode::HarnessCrashed,
                message: format!("the harness exited with {status}"),
            });
        }

        let turn_status = match consumed.result.as_ref() {
            Some(result) if result.is_error => TurnStatus::Failed,
            Some(_reported) => TurnStatus::Completed,
            // A clean exit with no result line means the harness ended without
            // saying what it did, which is a defect worth naming rather than
            // reporting as success.
            None => TurnStatus::Failed,
        };

        if consumed.result.is_none() {
            self.record_incident(
                thread_id,
                turn_id,
                ErrorCode::HarnessCrashed,
                Disposition::Fatal,
                "the harness exited cleanly without reporting a result",
            )
            .await;
        }

        Ok((turn_status, assemble(claimed, consumed.result, turn_status)))
    }

    /// Reads the harness's output until it ends, is cancelled, or runs out of
    /// budget.
    ///
    /// Three things can end the loop and only one of them is the harness
    /// finishing, which is why this is a `select!` rather than a read loop with
    /// checks bolted on. A ceiling reached ten seconds into a thirty minute
    /// completion has to be acted on then, not when the next line happens to
    /// arrive.
    async fn consume(
        &self,
        stdout: tokio::process::ChildStdout,
        thread_id: &str,
        turn_id: &str,
        ceilings: &Ceilings,
        reported: &mut mpsc::UnboundedReceiver<Crossing>,
    ) -> Consumed {
        let mut lines = BufReader::new(stdout).lines();
        let mut consumed = Consumed::default();
        let mut seen = 0_usize;

        // Wall clock is the runner's to enforce. The proxy sees requests, not
        // the gaps between them, so a harness stuck in a shell command would
        // never reach it and would outlive any ceiling it counted.
        let limit = ceilings.wall_clock_per_turn;
        let started = tokio::time::Instant::now();
        let warn_at = started + limit.map_or(NEVER, |limit| limit.mul_f64(WARN_AT_FRACTION));
        let deadline = started + limit.unwrap_or(NEVER);
        let mut warned_on_clock = false;

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

                    if line.trim().is_empty() {
                        continue;
                    }

                    self.absorb(&line, thread_id, turn_id, &mut consumed).await;

                    seen += 1;
                    if seen.is_multiple_of(CANCEL_CHECK_EVERY)
                        && self.store.is_turn_cancelled(turn_id).await.unwrap_or(false)
                    {
                        consumed.cancelled = true;
                        break;
                    }
                }

                Some(crossing) = reported.recv() => {
                    self.publish_crossing(thread_id, turn_id, crossing).await;

                    if let Crossing::Reached { ceiling } = crossing {
                        consumed.exhausted = Some(ceiling);
                        break;
                    }
                }

                () = tokio::time::sleep_until(warn_at), if !warned_on_clock => {
                    warned_on_clock = true;
                    self.publish_crossing(thread_id, turn_id, Crossing::Approaching {
                        ceiling: Ceiling::WallClockPerTurn,
                        percent_used: crate::proxy::budget::WARN_AT_PERCENT,
                    })
                    .await;
                }

                () = tokio::time::sleep_until(deadline) => {
                    consumed.exhausted = Some(Ceiling::WallClockPerTurn);
                    break;
                }
            }
        }

        consumed
    }

    /// Turns one line of harness output into log entries and a result.
    async fn absorb(&self, line: &str, thread_id: &str, turn_id: &str, consumed: &mut Consumed) {
        let mapping = claude::map_line(line);

        if let Some(session_id) = mapping.harness_session_id
            && let Err(error) = self.store.set_harness_session(thread_id, &session_id).await
        {
            tracing::warn!(
                event.name = "turn.session.unrecorded",
                "could not record the harness session id: {error}",
            );
        }

        for event in mapping.events {
            // An incident from the mapper is recorded as well as streamed: the
            // stream is ephemeral and the database is where "why did last night
            // go wrong" gets answered.
            if let Payload::Incident(incident) = &event.payload {
                self.record_incident(
                    thread_id,
                    turn_id,
                    ErrorCode::try_from(incident.code).unwrap_or(ErrorCode::Internal),
                    Disposition::try_from(incident.disposition).unwrap_or(Disposition::Degraded),
                    &incident.message,
                )
                .await;
            }

            self.append(
                thread_id,
                turn_id,
                event.type_name,
                event.member_id,
                event.payload,
            )
            .await;
        }

        if let Some(result) = mapping.result {
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
    ) -> TurnResult {
        let code = crate::proxy::budget::code_for(ceiling);
        let message = format!(
            "the turn reached {}, so it was stopped and its work preserved",
            ceiling.as_str_name()
        );

        self.record_incident(
            &claimed.turn.thread_id,
            &claimed.turn.turn_id,
            code,
            Disposition::Fatal,
            &message,
        )
        .await;

        let mut result = assemble(claimed, harness, TurnStatus::Failed);
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
    async fn publish_crossing(&self, thread_id: &str, turn_id: &str, crossing: Crossing) {
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
                thread.id = thread_id,
                turn.id = turn_id,
                budget.ceiling = ceiling.as_str_name(),
                "{{budget.ceiling}} was reached and the turn is being stopped",
            );
            return;
        };

        tracing::info!(
            event.name = "turn.budget.warning",
            thread.id = thread_id,
            turn.id = turn_id,
            budget.ceiling = ceiling.as_str_name(),
            budget.percent_used = percent_used,
            "{{budget.ceiling}} is {{budget.percent_used}}% consumed",
        );

        self.append(
            thread_id,
            turn_id,
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
    async fn cost_ceiling_reached(
        &self,
        thread_id: &str,
        turn_id: &str,
        ceilings: &Ceilings,
    ) -> bool {
        let Some(ceiling) = ceilings.cost_per_thread.as_ref() else {
            return false;
        };

        // Absent means no finished turn reported a priced cost, which is not the
        // same as having spent nothing. Nothing comparable exists yet, so there
        // is nothing to enforce against.
        let Ok(Some(spent)) = self.store.thread_cost_nanos(thread_id).await else {
            return false;
        };

        let Some(used) = crate::proxy::budget::percent_of_cost(ceiling, spent) else {
            tracing::warn!(
                event.name = "turn.budget.incomparable",
                thread.id = thread_id,
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
                thread_id,
                turn_id,
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
        thread_id: &str,
        turn_id: &str,
        type_name: impl Into<String>,
        member_id: Option<String>,
        payload: Payload,
    ) {
        let append = AppendEvent {
            thread_id: thread_id.to_owned(),
            turn_id: Some(turn_id.to_owned()),
            member_id,
            type_name: type_name.into(),
            occurred_at: None,
            payload,
        };

        if let Err(error) = self.store.append_event(append).await {
            // Losing an event is a real defect, and the stream is where it would
            // otherwise vanish without trace.
            tracing::error!(
                event.name = "event.append.failed",
                thread.id = %thread_id,
                "could not append an event: {error}",
            );
        }
    }

    /// Records an incident, which outlives the turn it belongs to.
    ///
    /// The disposition is the caller's to state rather than assumed. A ceiling
    /// that ended a turn is `fatal` and a ticket that failed to prefetch is
    /// `degraded`, and collapsing the two would make the one query an operator
    /// runs after a bad night useless.
    async fn record_incident(
        &self,
        thread_id: &str,
        turn_id: &str,
        code: ErrorCode,
        disposition: Disposition,
        message: &str,
    ) {
        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            sequence: None,
            thread_id: Some(thread_id.to_owned()),
            turn_id: Some(turn_id.to_owned()),
            member_id: None,
            code: code.into(),
            disposition: disposition.into(),
            retryable: false,
            message: message.to_owned(),
            details: None,
            occurred_at: Some(Timestamp::now()),
        };

        if let Err(error) = self.store.record_incident(&incident).await {
            tracing::error!(
                event.name = "incident.record.failed",
                "could not record an incident: {error}",
            );
        }
    }
}

/// Starts the harness process with its pipes arranged the way the runner reads
/// them.
///
/// A free function rather than a method: it needs nothing from the runner, and
/// keeping it out of `drive` keeps the one function that has to be read start to
/// finish short enough to read.
fn spawn_harness(
    command: &crate::harness::spawn::HarnessCommand,
) -> Result<(tokio::process::Child, tokio::process::ChildStdout), Failure> {
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
        .map_err(|error| Failure {
            code: ErrorCode::HarnessLaunchFailed,
            message: format!("could not launch {}: {error}", command.program),
        })?;

    let stdout = child.stdout.take().ok_or_else(|| Failure {
        code: ErrorCode::HarnessLaunchFailed,
        message: "the harness produced no stdout to read".to_owned(),
    })?;

    Ok((child, stdout))
}

/// Folds what the harness reported into the full turn result.
///
/// A turn is bigger than a harness run: checkers, self-review, artifact
/// scanning, and suggestions are all stages the harness knows nothing about.
/// They are reported as skipped rather than omitted, so "not run" never reads as
/// "found nothing".
fn assemble(
    claimed: &ClaimedTurn,
    harness: Option<HarnessResult>,
    status: TurnStatus,
) -> TurnResult {
    let harness = harness.unwrap_or_default();

    let stages = [
        Stage::Plan,
        Stage::Checkers,
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
        incident_counts: Some(IncidentCounts::default()),
        members: Vec::new(),
        changed_files: Vec::new(),
        integrations: Vec::new(),
        checker_results: Vec::new(),
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

/// A reason a turn could not run.
struct Failure {
    code: ErrorCode,
    message: String,
}

impl Failure {
    fn into_result(self, claimed: &ClaimedTurn) -> TurnResult {
        TurnResult {
            turn_id: claimed.turn.turn_id.clone(),
            thread_id: claimed.turn.thread_id.clone(),
            status: TurnStatus::Failed.into(),
            summary: String::new(),
            error: Some(arsox_sdk::proto::error::v1::Error {
                code: self.code.into(),
                message: self.message,
                retryable: matches!(
                    self.code,
                    ErrorCode::HarnessLaunchFailed | ErrorCode::HarnessCrashed
                ),
                details: None,
                trace_id: None,
            }),
            metadata: claimed.turn.metadata.clone(),
            ..TurnResult::default()
        }
    }
}
