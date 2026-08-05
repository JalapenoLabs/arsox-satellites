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

/// A turn a runner has taken responsibility for.
#[derive(Debug, Clone)]
pub struct ClaimedTurn {
    pub turn: Turn,

    /// The thread's settings, needed to decide which harness to spawn and under
    /// what limits.
    pub settings: arsox_sdk::proto::settings::v1::ThreadSettings,
}

impl Store {
    /// Takes the oldest queued turn on a thread that has nothing running.
    ///
    /// The claim is a single conditional update, so two runners racing for the
    /// same turn produce one winner and one `None` rather than two runners
    /// driving one harness. Polling and then updating would leave exactly that
    /// window open.
    ///
    /// # Errors
    ///
    /// Returns a database error if the claim fails.
    pub async fn claim_next_turn(&self) -> Result<Option<ClaimedTurn>, StoreError> {
        let now = to_nanos(&Timestamp::now());

        let mut transaction = self.pool().begin().await?;

        // One turn at a time per thread, forever. The subquery excludes any
        // thread that already has something running, which is what makes the
        // rule structural rather than something the runner has to remember.
        let Some(row) = sqlx::query(
            "UPDATE turns
                SET status = ?, started_at = ?
              WHERE turn_id = (
                    SELECT candidate.turn_id
                      FROM turns candidate
                     WHERE candidate.status = ?
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

        Ok(Some(ClaimedTurn {
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
            settings: arsox_sdk::proto::settings::v1::ThreadSettings::decode(
                settings_bytes.as_slice(),
            )
            .unwrap_or_default(),
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

    /// Records an incident, which outlives the thread it belongs to.
    ///
    /// # Errors
    ///
    /// Returns a database error if the insert fails.
    pub async fn record_incident(
        &self,
        incident: &arsox_sdk::proto::incident::v1::Incident,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO incidents
               (incident_id, thread_id, sequence, turn_id, member_id,
                code, disposition, retryable, message, details, occurred_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&incident.incident_id)
        .bind(incident.thread_id.as_deref())
        .bind(
            incident
                .sequence
                .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
        )
        .bind(incident.turn_id.as_deref())
        .bind(incident.member_id.as_deref())
        .bind(incident.code)
        .bind(incident.disposition)
        .bind(i32::from(incident.retryable))
        .bind(&incident.message)
        .bind(incident.details.as_ref().map(prost::Message::encode_to_vec))
        .bind(
            incident
                .occurred_at
                .as_ref()
                .map_or_else(|| to_nanos(&Timestamp::now()), to_nanos),
        )
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Returns turns left running when the satellite stopped.
    ///
    /// A turn in flight during a restart is not lost work: the thread and its
    /// workspace survive, and the turn is marked interrupted so a human or a
    /// policy can decide whether to resume it.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn mark_interrupted_turns(&self) -> Result<Vec<String>, StoreError> {
        let now = to_nanos(&Timestamp::now());

        let rows = sqlx::query(
            "UPDATE turns SET status = ?, finished_at = ?
              WHERE status = ?
             RETURNING turn_id",
        )
        .bind(i32::from(TurnStatus::Interrupted))
        .bind(now)
        .bind(i32::from(TurnStatus::Running))
        .fetch_all(self.pool())
        .await?;

        Ok(rows.iter().map(|row| row.get("turn_id")).collect())
    }
}
