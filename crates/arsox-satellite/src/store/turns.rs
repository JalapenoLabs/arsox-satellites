// Copyright © 2026 Jalapeno Labs

//! Turn rows and the per-thread queue.
//!
//! The queue is a query, not a structure. Turns run in the order they were
//! submitted, so the next one to run is the oldest queued row for the thread.
//! A separate queue table would be a second source of truth that could fall out
//! of step with this one.

use super::{Store, StoreError, from_nanos, to_nanos};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::turn::v1::{Turn, TurnResult, TurnStatus};
use prost::Message as _;
use sqlx::Row as _;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Everything needed to queue a turn.
#[derive(Debug, Clone)]
pub struct NewTurn {
    pub thread_id: String,
    pub prompt: String,
    pub metadata: BTreeMap<String, String>,
    pub idempotency_key: Option<String>,

    /// True when the satellite started this turn itself rather than the SDK
    /// asking for it.
    pub satellite_initiated: bool,
    pub triggered_by_turn_id: Option<String>,
}

/// A turn as stored, plus whether this call queued it.
#[derive(Debug, Clone)]
pub struct StoredTurn {
    pub turn: Turn,

    /// False when an idempotency key matched an existing turn.
    pub queued: bool,
}

/// Queued turns allowed per thread when the thread sets no cap.
///
/// A cap exists so a caller that submits in a loop is rejected rather than
/// accumulating work nobody will ever read the results of.
const DEFAULT_QUEUE_DEPTH: u32 = 32;

impl Store {
    /// Queues a turn, or returns the existing one when the idempotency key
    /// matches.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] for an unknown thread and
    /// [`StoreError::QueueFull`] when the thread's cap is reached.
    pub async fn create_turn(&self, new: NewTurn) -> Result<StoredTurn, StoreError> {
        let thread = self.thread(&new.thread_id).await?;

        if let Some(key) = new.idempotency_key.as_deref()
            && let Some(existing) = self.turn_by_idempotency_key(&new.thread_id, key).await?
        {
            return Ok(StoredTurn {
                turn: existing,
                queued: false,
            });
        }

        let cap = thread
            .settings
            .as_ref()
            .and_then(|settings| settings.resource_limits.as_ref())
            .and_then(|limits| limits.max_queued_turns)
            .unwrap_or(DEFAULT_QUEUE_DEPTH);

        let (queue_depth, _running) = self.queue_state(&new.thread_id).await?;
        if queue_depth >= cap {
            return Err(StoreError::QueueFull(new.thread_id));
        }

        let turn_id = uuid::Uuid::now_v7().to_string();
        let now = Timestamp::now();
        let now_nanos = to_nanos(&now);

        let mut transaction = self.pool().begin().await?;

        sqlx::query(
            "INSERT INTO turns
               (turn_id, thread_id, status, prompt, satellite_initiated,
                triggered_by_turn_id, queued_at, idempotency_key)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&turn_id)
        .bind(&new.thread_id)
        .bind(i32::from(TurnStatus::Queued))
        .bind(&new.prompt)
        .bind(i32::from(new.satellite_initiated))
        .bind(new.triggered_by_turn_id.as_deref())
        .bind(now_nanos)
        .bind(new.idempotency_key.as_deref())
        .execute(&mut *transaction)
        .await?;

        for (key, value) in &new.metadata {
            sqlx::query("INSERT INTO turn_metadata (turn_id, key, value) VALUES (?, ?, ?)")
                .bind(&turn_id)
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        self.touch_thread(&new.thread_id).await?;

        Ok(StoredTurn {
            turn: Turn {
                turn_id,
                thread_id: new.thread_id,
                status: TurnStatus::Queued.into(),
                prompt: new.prompt,
                satellite_initiated: new.satellite_initiated,
                triggered_by_turn_id: new.triggered_by_turn_id,
                queued_at: Some(now),
                started_at: None,
                finished_at: None,
                metadata: new.metadata.into_iter().collect(),
            },
            queued: true,
        })
    }

    /// Reads one turn and its result, if it has one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TurnNotFound`] when the id is unknown or belongs to
    /// a different thread.
    pub async fn turn(
        &self,
        thread_id: &str,
        turn_id: &str,
    ) -> Result<(Turn, Option<TurnResult>), StoreError> {
        let row = sqlx::query("SELECT * FROM turns WHERE turn_id = ? AND thread_id = ?")
            .bind(turn_id)
            .bind(thread_id)
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| StoreError::TurnNotFound(turn_id.to_owned()))?;

        let metadata = self.turn_metadata(turn_id).await?;
        let result = row
            .get::<Option<Vec<u8>>, _>("result")
            .and_then(|bytes| TurnResult::decode(bytes.as_slice()).ok());

        Ok((hydrate_turn(&row, metadata), result))
    }

    /// Lists a thread's turns, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] for an unknown thread.
    pub async fn list_turns(
        &self,
        thread_id: &str,
        statuses: &[i32],
    ) -> Result<Vec<Turn>, StoreError> {
        // Proves the thread exists, so an unknown id is a clear 404 rather than
        // an empty list that looks like a thread with no turns.
        self.thread(thread_id).await?;

        let mut sql = String::from("SELECT * FROM turns WHERE thread_id = ?");
        if !statuses.is_empty() {
            let slots = vec!["?"; statuses.len()].join(", ");
            write!(sql, " AND status IN ({slots})").expect("writing to a String is infallible");
        }
        sql.push_str(" ORDER BY queued_at ASC, turn_id ASC");

        let mut query = sqlx::query(&sql).bind(thread_id);
        for status in statuses {
            query = query.bind(status);
        }

        let rows = query.fetch_all(self.pool()).await?;

        let mut turns = Vec::with_capacity(rows.len());
        for row in &rows {
            let turn_id: String = row.get("turn_id");
            let metadata = self.turn_metadata(&turn_id).await?;
            turns.push(hydrate_turn(row, metadata));
        }

        Ok(turns)
    }

    /// Cancels a turn.
    ///
    /// A queued turn is dropped outright. A running turn is marked cancelled
    /// here and the runner stops it cooperatively; work already committed to a
    /// branch survives either way.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TurnNotFound`] when the id is unknown, and reports
    /// the turn unchanged when it has already finished.
    pub async fn cancel_turn(&self, thread_id: &str, turn_id: &str) -> Result<Turn, StoreError> {
        let (turn, _result) = self.turn(thread_id, turn_id).await?;

        // Cancelling something already terminal is a no-op rather than an error:
        // a client racing the turn's own completion has done nothing wrong.
        if is_terminal(turn.status) {
            return Ok(turn);
        }

        let now = to_nanos(&Timestamp::now());
        sqlx::query("UPDATE turns SET status = ?, finished_at = ? WHERE turn_id = ?")
            .bind(i32::from(TurnStatus::Cancelled))
            .bind(now)
            .bind(turn_id)
            .execute(self.pool())
            .await?;

        self.touch_thread(thread_id).await?;

        Ok(Turn {
            status: TurnStatus::Cancelled.into(),
            finished_at: Some(from_nanos(now)),
            ..turn
        })
    }

    /// How many turns are waiting, and which one is running.
    pub(crate) async fn queue_state(
        &self,
        thread_id: &str,
    ) -> Result<(u32, Option<String>), StoreError> {
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM turns WHERE thread_id = ? AND status = ?")
                .bind(thread_id)
                .bind(i32::from(TurnStatus::Queued))
                .fetch_one(self.pool())
                .await?;

        let running: Option<String> = sqlx::query_scalar(
            "SELECT turn_id FROM turns WHERE thread_id = ? AND status = ? ORDER BY started_at ASC LIMIT 1",
        )
        .bind(thread_id)
        .bind(i32::from(TurnStatus::Running))
        .fetch_optional(self.pool())
        .await?;

        Ok((u32::try_from(queued).unwrap_or(u32::MAX), running))
    }

    async fn turn_by_idempotency_key(
        &self,
        thread_id: &str,
        key: &str,
    ) -> Result<Option<Turn>, StoreError> {
        let Some(row) =
            sqlx::query("SELECT * FROM turns WHERE thread_id = ? AND idempotency_key = ?")
                .bind(thread_id)
                .bind(key)
                .fetch_optional(self.pool())
                .await?
        else {
            return Ok(None);
        };

        let turn_id: String = row.get("turn_id");
        let metadata = self.turn_metadata(&turn_id).await?;

        Ok(Some(hydrate_turn(&row, metadata)))
    }

    async fn turn_metadata(&self, turn_id: &str) -> Result<BTreeMap<String, String>, StoreError> {
        let rows = sqlx::query("SELECT key, value FROM turn_metadata WHERE turn_id = ?")
            .bind(turn_id)
            .fetch_all(self.pool())
            .await?;

        Ok(rows
            .iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect())
    }
}

/// Whether a status means the turn is finished and will not change again.
fn is_terminal(status: i32) -> bool {
    matches!(
        TurnStatus::try_from(status),
        Ok(TurnStatus::Completed | TurnStatus::Failed | TurnStatus::Cancelled)
    )
}

fn hydrate_turn(row: &sqlx::sqlite::SqliteRow, metadata: BTreeMap<String, String>) -> Turn {
    Turn {
        turn_id: row.get("turn_id"),
        thread_id: row.get("thread_id"),
        status: row.get("status"),
        prompt: row.get("prompt"),
        satellite_initiated: row.get::<i32, _>("satellite_initiated") != 0,
        triggered_by_turn_id: row.get("triggered_by_turn_id"),
        queued_at: Some(from_nanos(row.get("queued_at"))),
        started_at: row.get::<Option<i64>, _>("started_at").map(from_nanos),
        finished_at: row.get::<Option<i64>, _>("finished_at").map(from_nanos),
        metadata: metadata.into_iter().collect(),
    }
}
