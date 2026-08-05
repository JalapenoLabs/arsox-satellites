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

use crate::harness::spawn::{Session, command_for};
use crate::harness::{HarnessResult, claude};
use crate::store::{AppendEvent, ClaimedTurn, Store};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::event::v1::{ThreadEndReason, TurnCompleted, TurnStarted};
use arsox_sdk::proto::harness::v1::Harness;
use arsox_sdk::proto::incident::v1::{Disposition, Incident, IncidentCounts};
use arsox_sdk::proto::turn::v1::{Stage, StageDisposition, StageOutcome, TurnResult, TurnStatus};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt as _, BufReader};
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

/// What reading a harness's output produced.
#[derive(Debug, Default)]
struct Consumed {
    result: Option<HarnessResult>,
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
    ) -> Self {
        Self {
            store,
            workspace_root,
            notify,
            capacity: Arc::new(Semaphore::new(max_concurrent_threads as usize)),
            collector,
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
                self.record_incident(&thread_id, &turn_id, failure.code, &failure.message)
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

        let working_dir = self.workspace_root.join(thread_id);
        tokio::fs::create_dir_all(&working_dir)
            .await
            .map_err(|error| Failure {
                code: ErrorCode::HarnessLaunchFailed,
                message: format!("could not create the thread workspace: {error}"),
            })?;

        // A thread that has already opened a harness session resumes it, so the
        // second turn remembers the first. Without this a thread would be a
        // series of unrelated turns rather than a conversation.
        let existing = self
            .store
            .thread(thread_id)
            .await
            .ok()
            .and_then(|thread| thread.harness_session_id);

        let session = match existing {
            Some(session_id) => Session::Resume { session_id },
            // The thread id doubles as the session id: both are UUIDs, and
            // reusing it means one lookup fewer when correlating a run with the
            // harness transcripts on disk.
            None => Session::Start {
                session_id: thread_id.clone(),
            },
        };

        let harness = Harness::try_from(claimed.settings.harness).unwrap_or(Harness::Claude);
        let command = command_for(harness, &claimed.turn.prompt, &session, working_dir);

        let mut child = tokio::process::Command::new(&command.program)
            .args(&command.args)
            .current_dir(&command.working_dir)
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

        let consumed = self.consume(stdout, thread_id, turn_id).await;

        if consumed.cancelled {
            // Asked to stop cooperatively first. `kill_on_drop` is the backstop
            // for the case where it does not.
            drop(child.start_kill());
            return Ok((
                TurnStatus::Cancelled,
                assemble(claimed, None, TurnStatus::Cancelled),
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
                "the harness exited cleanly without reporting a result",
            )
            .await;
        }

        Ok((turn_status, assemble(claimed, consumed.result, turn_status)))
    }

    /// Reads the harness's output until it ends or the turn is cancelled.
    async fn consume(
        &self,
        stdout: tokio::process::ChildStdout,
        thread_id: &str,
        turn_id: &str,
    ) -> Consumed {
        let mut lines = BufReader::new(stdout).lines();
        let mut consumed = Consumed::default();
        let mut seen = 0_usize;

        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }

            let mapping = claude::map_line(&line);

            if let Some(session_id) = mapping.harness_session_id
                && let Err(error) = self.store.set_harness_session(thread_id, &session_id).await
            {
                tracing::warn!(
                    event.name = "turn.session.unrecorded",
                    "could not record the harness session id: {error}",
                );
            }

            for event in mapping.events {
                // An incident from the mapper is recorded as well as streamed:
                // the stream is ephemeral and the database is where "why did
                // last night go wrong" gets answered.
                if let Payload::Incident(incident) = &event.payload {
                    self.record_incident(
                        thread_id,
                        turn_id,
                        ErrorCode::try_from(incident.code).unwrap_or(ErrorCode::Internal),
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

            seen += 1;
            if seen.is_multiple_of(CANCEL_CHECK_EVERY)
                && self.store.is_turn_cancelled(turn_id).await.unwrap_or(false)
            {
                consumed.cancelled = true;
                break;
            }
        }

        consumed
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

    async fn record_incident(
        &self,
        thread_id: &str,
        turn_id: &str,
        code: ErrorCode,
        message: &str,
    ) {
        let incident = Incident {
            incident_id: uuid::Uuid::now_v7().to_string(),
            sequence: None,
            thread_id: Some(thread_id.to_owned()),
            turn_id: Some(turn_id.to_owned()),
            member_id: None,
            code: code.into(),
            disposition: Disposition::Degraded.into(),
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
