// Copyright © 2026 Jalapeno Labs

//! The runner's view of the queue: claiming work and finishing it.
//!
//! Separate from the API-facing turn queries because the concerns differ. Those
//! answer questions about turns; these move turns between states, and every one
//! of them has to be safe against a second runner doing the same thing at the
//! same moment.

use super::{Store, StoreError, from_nanos, to_nanos};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::thread::v1::ThreadState;
use arsox_sdk::proto::turn::v1::{Turn, TurnResult, TurnStatus};
use prost::Message as _;
use sqlx::Row as _;
use std::collections::BTreeMap;

/// What a restart left behind, and what was done about it.
#[derive(Debug, Clone, Default)]
pub struct Interrupted {
    /// Put back in the queue, because the thread asked for that.
    pub resumed: Vec<String>,

    /// Left for a human to decide about.
    pub left_interrupted: Vec<String>,
}

/// A turn a runner has taken responsibility for.
#[derive(Debug, Clone)]
pub struct ClaimedTurn {
    pub turn: Turn,

    /// The thread's settings, needed to decide which harness to spawn and under
    /// what limits.
    pub settings: arsox_sdk::proto::settings::v1::ThreadSettings,

    /// The thread's secrets, compiled once here rather than per event.
    ///
    /// Built at the claim because that is where the settings are already
    /// decoded, and carried with the turn so every event, result, and incident
    /// the turn produces is masked by the same automaton.
    pub redactor: crate::redaction::Redactor,
}

impl Store {
    /// Takes the oldest queued turn on a thread that has nothing running.
    ///
    /// The claim is a single conditional update, so two runners racing for the
    /// same turn produce one winner and one `None` rather than two runners
    /// driving one harness. Polling and then updating would leave exactly that
    /// window open.
    ///
    /// Nothing is claimed from a thread that is paused or still provisioning.
    /// Both rules live in the query rather than in the runner, which is what
    /// makes "no turn ever runs in a half-cloned workspace" a property of the
    /// database instead of a promise a second runner could break.
    ///
    /// # Errors
    ///
    /// Returns a database error if the claim fails.
    pub async fn claim_next_turn(&self) -> Result<Option<ClaimedTurn>, StoreError> {
        let now = to_nanos(&Timestamp::now());

        let mut transaction = self.pool().begin().await?;

        // One turn at a time per thread, forever, and nothing at all from a
        // thread that is paused or still provisioning. Every rule lives in this
        // subquery rather than in the runner, which is what makes them hold even
        // if a second runner appears.
        let Some(row) = sqlx::query(
            "UPDATE turns
                SET status = ?, started_at = ?
              WHERE turn_id = (
                    SELECT candidate.turn_id
                      FROM turns candidate
                      JOIN threads owner ON owner.thread_id = candidate.thread_id
                     WHERE candidate.status = ?
                       AND owner.state NOT IN (?, ?)
                       AND candidate.thread_id NOT IN (
                             SELECT running.thread_id FROM turns running WHERE running.status = ?
                           )
                     ORDER BY candidate.queued_at ASC, candidate.turn_id ASC
                     LIMIT 1
                  )
             RETURNING *",
        )
        .bind(i32::from(TurnStatus::Running))
        .bind(now)
        .bind(i32::from(TurnStatus::Queued))
        .bind(i32::from(ThreadState::Paused))
        .bind(i32::from(ThreadState::Provisioning))
        .bind(i32::from(TurnStatus::Running))
        .fetch_optional(&mut *transaction)
        .await?
        else {
            return Ok(None);
        };

        let thread_id: String = row.get("thread_id");
        let turn_id: String = row.get("turn_id");

        sqlx::query("UPDATE threads SET state = ?, last_activity_at = ? WHERE thread_id = ?")
            .bind(i32::from(ThreadState::Running))
            .bind(now)
            .bind(&thread_id)
            .execute(&mut *transaction)
            .await?;

        let settings_bytes: Vec<u8> =
            sqlx::query_scalar("SELECT settings FROM threads WHERE thread_id = ?")
                .bind(&thread_id)
                .fetch_one(&mut *transaction)
                .await?;

        let metadata_rows = sqlx::query("SELECT key, value FROM turn_metadata WHERE turn_id = ?")
            .bind(&turn_id)
            .fetch_all(&mut *transaction)
            .await?;

        transaction.commit().await?;

        let metadata: BTreeMap<String, String> = metadata_rows
            .iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect();

        // A settings blob that will not decode was written by a different major
        // version, and the turn runs on the defaults. The redactor is built from
        // whatever decoded, which for an undecodable blob is nothing to mask.
        let settings =
            arsox_sdk::proto::settings::v1::ThreadSettings::decode(settings_bytes.as_slice())
                .unwrap_or_default();

        Ok(Some(ClaimedTurn {
            redactor: crate::redaction::Redactor::for_thread(&settings),
            settings,
            turn: Turn {
                turn_id,
                thread_id,
                status: TurnStatus::Running.into(),
                prompt: row.get("prompt"),
                satellite_initiated: row.get::<i32, _>("satellite_initiated") != 0,
                triggered_by_turn_id: row.get("triggered_by_turn_id"),
                queued_at: Some(from_nanos(row.get("queued_at"))),
                started_at: Some(from_nanos(now)),
                finished_at: None,
                metadata: metadata.into_iter().collect(),
            },
        }))
    }

    /// Records a turn's outcome and returns the thread to rest.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn finish_turn(
        &self,
        turn_id: &str,
        status: TurnStatus,
        result: &TurnResult,
    ) -> Result<(), StoreError> {
        let now = to_nanos(&Timestamp::now());

        let mut transaction = self.pool().begin().await?;

        let thread_id: Option<String> = sqlx::query_scalar(
            "UPDATE turns SET status = ?, finished_at = ?, result = ?
              WHERE turn_id = ?
             RETURNING thread_id",
        )
        .bind(i32::from(status))
        .bind(now)
        .bind(result.encode_to_vec())
        .bind(turn_id)
        .fetch_optional(&mut *transaction)
        .await?;

        if let Some(thread_id) = thread_id {
            // Back to idle rather than to whatever it was: the thread has no
            // work in flight now, and the idle clock should start counting.
            sqlx::query("UPDATE threads SET state = ?, last_activity_at = ? WHERE thread_id = ?")
                .bind(i32::from(ThreadState::Idle))
                .bind(now)
                .bind(&thread_id)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;

        Ok(())
    }

    /// Whether a turn has been cancelled out from under its runner.
    ///
    /// Cancellation arrives as an ordinary HTTP request that writes to this
    /// table, so the runner learns about it by asking rather than by being
    /// interrupted.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn is_turn_cancelled(&self, turn_id: &str) -> Result<bool, StoreError> {
        let status: Option<i32> = sqlx::query_scalar("SELECT status FROM turns WHERE turn_id = ?")
            .bind(turn_id)
            .fetch_optional(self.pool())
            .await?;

        Ok(status == Some(i32::from(TurnStatus::Cancelled)))
    }

    /// Settles turns left running when the satellite stopped.
    ///
    /// A turn in flight during a restart is not lost work: the thread and its
    /// workspace survive. What happens next is the thread's own decision, taken
    /// when it was created: `resume_interrupted_turns` puts it back in the queue,
    /// and otherwise it stays interrupted until a human looks at it.
    ///
    /// Automatic resumption is right for an unattended fleet and wrong when
    /// somebody would want to see what happened first, which is why it is a
    /// setting rather than a behaviour.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn settle_interrupted_turns(&self) -> Result<Interrupted, StoreError> {
        let now = to_nanos(&Timestamp::now());

        let rows = sqlx::query(
            "UPDATE turns SET status = ?, finished_at = ?
              WHERE status = ?
             RETURNING turn_id, thread_id",
        )
        .bind(i32::from(TurnStatus::Interrupted))
        .bind(now)
        .bind(i32::from(TurnStatus::Running))
        .fetch_all(self.pool())
        .await?;

        let mut settled = Interrupted::default();

        for row in &rows {
            let turn_id: String = row.get("turn_id");
            let thread_id: String = row.get("thread_id");

            let resume = self
                .thread(&thread_id)
                .await
                .ok()
                .and_then(|thread| thread.settings)
                .is_some_and(|settings| settings.resume_interrupted_turns);

            if resume {
                // Back to the queue with its start time cleared, so the runner
                // treats it as work that has never begun rather than work that
                // began and vanished.
                sqlx::query(
                    "UPDATE turns SET status = ?, started_at = NULL, finished_at = NULL
                      WHERE turn_id = ?",
                )
                .bind(i32::from(TurnStatus::Queued))
                .bind(&turn_id)
                .execute(self.pool())
                .await?;

                settled.resumed.push(turn_id);
            } else {
                settled.left_interrupted.push(turn_id);
            }
        }

        Ok(settled)
    }
}
