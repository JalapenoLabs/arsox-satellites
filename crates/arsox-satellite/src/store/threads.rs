// Copyright © 2026 Jalapeno Labs

//! Thread rows: creation, lookup, listing, and teardown.

use super::{Store, StoreError, from_nanos, to_nanos};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use arsox_sdk::proto::thread::v1::{Thread, ThreadOrder, ThreadState, ThreadSummary};
use prost::Message as _;
use sqlx::Row as _;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Everything needed to open a thread.
#[derive(Debug, Clone)]
pub struct NewThread {
    pub settings: ThreadSettings,
    pub metadata: BTreeMap<String, String>,
    pub idempotency_key: Option<String>,
}

/// One page of a thread listing.
#[derive(Debug, Clone)]
pub struct Listing {
    pub threads: Vec<ThreadSummary>,

    /// Pass back to continue. Empty when the page was the last one.
    pub next_cursor: String,
}

/// A thread as stored, plus whether this call created it.
#[derive(Debug, Clone)]
pub struct StoredThread {
    pub thread: Thread,

    /// False when an idempotency key matched an existing thread and this is that
    /// thread rather than a new one.
    pub created: bool,
}

/// Which threads a listing should return.
#[derive(Debug, Clone, Default)]
pub struct ThreadFilter {
    /// Empty does not filter.
    pub states: Vec<i32>,

    /// Every entry must match. Empty does not filter.
    pub metadata: BTreeMap<String, String>,

    /// Resume after this opaque cursor, which encodes the sort key and the
    /// thread id together.
    ///
    /// Both are needed. Ordering by last activity alone is not unique, so a
    /// cursor carrying only the timestamp would either repeat or skip every
    /// thread that shares a millisecond with the one at the page boundary.
    pub after: Option<String>,

    pub limit: u32,

    pub order_by: ThreadOrder,
    pub descending: bool,
}

/// Splits a cursor back into its sort key and thread id.
fn split_cursor(cursor: &str) -> Option<(&str, &str)> {
    cursor.split_once('|')
}

/// Builds the cursor a client sends back to continue a listing.
fn build_cursor(sort_key: &str, thread_id: &str) -> String {
    format!("{sort_key}|{thread_id}")
}

/// Page size used when a caller asks for none.
const DEFAULT_PAGE: u32 = 50;

/// Largest page a caller can ask for.
const MAX_PAGE: u32 = 500;

impl Store {
    /// Opens a thread, or returns the existing one when the idempotency key
    /// matches.
    ///
    /// The thread opens `IDLE` rather than `PROVISIONING`, because nothing
    /// provisions a workspace yet and a state the thread never leaves would be a
    /// lie. `PROVISIONING` returns with repo cloning.
    ///
    /// The thread id is generated here and never accepted from a client. It
    /// becomes a filesystem path, so a client-supplied one is a path traversal
    /// waiting to happen. `UUIDv7` also sorts by creation time, which is what
    /// makes listing chronological without a separate index.
    ///
    /// # Errors
    ///
    /// Returns a database error if the insert fails for any reason other than
    /// an idempotency collision, which is handled rather than raised.
    pub async fn create_thread(&self, new: NewThread) -> Result<StoredThread, StoreError> {
        // Checked before inserting rather than by catching the unique violation,
        // because the existing thread has to be returned either way and a
        // successful read is cheaper than a failed write plus a read.
        if let Some(key) = new.idempotency_key.as_deref()
            && let Some(existing) = self.thread_by_idempotency_key(key).await?
        {
            return Ok(StoredThread {
                thread: existing,
                created: false,
            });
        }

        let thread_id = uuid::Uuid::now_v7().to_string();
        let now = Timestamp::now();
        let now_nanos = to_nanos(&now);

        let expires_at = new
            .settings
            .idle_ttl
            .as_ref()
            .map(|ttl| now_nanos.saturating_add(arsox_sdk::helpers::duration_to_nanos(ttl)));

        let mut transaction = self.pool().begin().await?;

        sqlx::query(
            "INSERT INTO threads
               (thread_id, state, settings, created_at, last_activity_at, expires_at, idempotency_key)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&thread_id)
        .bind(i32::from(ThreadState::Idle))
        .bind(new.settings.encode_to_vec())
        .bind(now_nanos)
        .bind(now_nanos)
        .bind(expires_at)
        .bind(new.idempotency_key.as_deref())
        .execute(&mut *transaction)
        .await?;

        for (key, value) in &new.metadata {
            sqlx::query("INSERT INTO thread_metadata (thread_id, key, value) VALUES (?, ?, ?)")
                .bind(&thread_id)
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;

        Ok(StoredThread {
            thread: Thread {
                thread_id,
                state: ThreadState::Idle.into(),
                settings: Some(new.settings),
                created_at: Some(now.clone()),
                last_activity_at: Some(now),
                expires_at: expires_at.map(from_nanos),
                queue_depth: 0,
                current_turn_id: None,
                pending_questions: None,
                pending_plan: None,
                latest_sequence: 0,
                metadata: new.metadata.into_iter().collect(),
                harness_session_id: None,
            },
            created: true,
        })
    }

    /// Reads one thread.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn thread(&self, thread_id: &str) -> Result<Thread, StoreError> {
        let row = sqlx::query("SELECT * FROM threads WHERE thread_id = ?")
            .bind(thread_id)
            .fetch_optional(self.pool())
            .await?
            .ok_or_else(|| StoreError::ThreadNotFound(thread_id.to_owned()))?;

        let metadata = self.thread_metadata(thread_id).await?;
        let (queue_depth, current_turn_id) = self.queue_state(thread_id).await?;

        Ok(hydrate_thread(&row, metadata, queue_depth, current_turn_id))
    }

    /// Lists threads, newest last, filtered and paged.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn list_threads(&self, filter: &ThreadFilter) -> Result<Listing, StoreError> {
        let limit = match filter.limit {
            0 => DEFAULT_PAGE,
            asked => asked.min(MAX_PAGE),
        };

        // Ordering by last activity needs a second key, because a timestamp is
        // not unique and a cursor on it alone would repeat or skip whatever
        // shares a value with the row at the page boundary. Creation order gets
        // this for free: thread ids are UUIDv7 and already sort by time.
        let by_activity = matches!(filter.order_by, ThreadOrder::LastActivity);
        let sort_column = if by_activity {
            "t.last_activity_at"
        } else {
            "t.thread_id"
        };
        let comparison = if filter.descending { "<" } else { ">" };
        let direction = if filter.descending { "DESC" } else { "ASC" };

        // Built rather than written out because the filters are optional and
        // SQLite has no array binding. Every value is still bound, never
        // interpolated: the fragments chosen here are fixed strings.
        let mut sql = String::from("SELECT t.* FROM threads t");
        if !filter.metadata.is_empty() {
            for index in 0..filter.metadata.len() {
                write!(
                    sql,
                    " JOIN thread_metadata m{index} ON m{index}.thread_id = t.thread_id \
                      AND m{index}.key = ? AND m{index}.value = ?"
                )
                .expect("writing to a String is infallible");
            }
        }
        sql.push_str(" WHERE 1 = 1");
        if !filter.states.is_empty() {
            let slots = vec!["?"; filter.states.len()].join(", ");
            write!(sql, " AND t.state IN ({slots})").expect("writing to a String is infallible");
        }
        if filter.after.is_some() {
            write!(sql, " AND ({sort_column}, t.thread_id) {comparison} (?, ?)")
                .expect("writing to a String is infallible");
        }
        write!(
            sql,
            " ORDER BY {sort_column} {direction}, t.thread_id {direction} LIMIT ?"
        )
        .expect("writing to a String is infallible");

        let mut query = sqlx::query(&sql);
        for (key, value) in &filter.metadata {
            query = query.bind(key).bind(value);
        }
        for state in &filter.states {
            query = query.bind(state);
        }
        if let Some(after) = filter.after.as_deref() {
            let (sort_key, thread_id) = split_cursor(after).unwrap_or((after, after));
            if by_activity {
                query = query.bind(sort_key.parse::<i64>().unwrap_or_default());
            } else {
                query = query.bind(sort_key);
            }
            query = query.bind(thread_id);
        }
        query = query.bind(limit);

        let rows = query.fetch_all(self.pool()).await?;

        let mut summaries = Vec::with_capacity(rows.len());
        let mut cursors = Vec::with_capacity(rows.len());
        for row in &rows {
            let thread_id: String = row.get("thread_id");
            let metadata = self.thread_metadata(&thread_id).await?;
            let (queue_depth, current_turn_id) = self.queue_state(&thread_id).await?;

            let sort_key = if by_activity {
                row.get::<i64, _>("last_activity_at").to_string()
            } else {
                thread_id.clone()
            };
            cursors.push(build_cursor(&sort_key, &thread_id));

            summaries.push(ThreadSummary {
                thread_id: thread_id.clone(),
                state: row.get("state"),
                queue_depth,
                current_turn_id,
                created_at: Some(from_nanos(row.get("created_at"))),
                last_activity_at: Some(from_nanos(row.get("last_activity_at"))),
                expires_at: row.get::<Option<i64>, _>("expires_at").map(from_nanos),
                // Not measured until the workspace manager exists. Zero here
                // would claim the thread holds nothing.
                workspace_bytes: 0,
                latest_sequence: row.get::<i64, _>("latest_sequence").unsigned_abs(),
                metadata: metadata.into_iter().collect(),
            });
        }

        Ok(Listing {
            next_cursor: cursors.last().cloned().unwrap_or_default(),
            threads: summaries,
        })
    }

    /// Stops a thread claiming queued work, without losing anything.
    ///
    /// Pausing an already paused thread is a no-op rather than an error: an
    /// operator racing their own second click has done nothing wrong.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn pause_thread(&self, thread_id: &str) -> Result<Thread, StoreError> {
        self.set_paused(thread_id, true).await
    }

    /// Returns a paused thread to service.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn resume_thread(&self, thread_id: &str) -> Result<Thread, StoreError> {
        self.set_paused(thread_id, false).await
    }

    async fn set_paused(&self, thread_id: &str, paused: bool) -> Result<Thread, StoreError> {
        let thread = self.thread(thread_id).await?;
        let current = ThreadState::try_from(thread.state).unwrap_or(ThreadState::Unspecified);

        // Pausing something already paused, or resuming something that was never
        // paused, reports the state rather than failing: an operator racing
        // their own second click has done nothing wrong.
        if (current == ThreadState::Paused) == paused {
            return Ok(thread);
        }

        // Resuming returns the thread to idle rather than to whatever it was
        // doing when it stopped. Anything that had been running was already
        // settled, so idle is the honest state and the runner takes it from
        // there.
        let target = if paused {
            ThreadState::Paused
        } else {
            ThreadState::Idle
        };

        sqlx::query("UPDATE threads SET state = ? WHERE thread_id = ?")
            .bind(i32::from(target))
            .bind(thread_id)
            .execute(self.pool())
            .await?;

        Ok(Thread {
            state: target.into(),
            ..thread
        })
    }

    /// Marks a thread destroyed and removes its rows.
    ///
    /// Incidents survive: they are not foreign-keyed to the thread precisely so
    /// that "why did last night go wrong" outlives the workspace.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn destroy_thread(&self, thread_id: &str) -> Result<Thread, StoreError> {
        let thread = self.thread(thread_id).await?;

        sqlx::query("DELETE FROM threads WHERE thread_id = ?")
            .bind(thread_id)
            .execute(self.pool())
            .await?;

        Ok(Thread {
            state: ThreadState::Destroyed.into(),
            ..thread
        })
    }

    /// Resets the idle clock.
    ///
    /// Called on every turn and on any interaction with the thread, which is
    /// what makes the TTL measure idleness rather than age.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn touch_thread(&self, thread_id: &str) -> Result<(), StoreError> {
        let now = to_nanos(&Timestamp::now());

        sqlx::query(
            "UPDATE threads
                SET last_activity_at = ?,
                    expires_at = CASE
                        WHEN expires_at IS NULL THEN NULL
                        ELSE ? + (expires_at - last_activity_at)
                    END
              WHERE thread_id = ?",
        )
        .bind(now)
        .bind(now)
        .bind(thread_id)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Records the harness's own session id for a thread.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn set_harness_session(
        &self,
        thread_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("UPDATE threads SET harness_session_id = ? WHERE thread_id = ?")
            .bind(session_id)
            .bind(thread_id)
            .execute(self.pool())
            .await?;

        Ok(())
    }

    async fn thread_by_idempotency_key(&self, key: &str) -> Result<Option<Thread>, StoreError> {
        let Some(row) = sqlx::query("SELECT thread_id FROM threads WHERE idempotency_key = ?")
            .bind(key)
            .fetch_optional(self.pool())
            .await?
        else {
            return Ok(None);
        };

        let thread_id: String = row.get("thread_id");
        self.thread(&thread_id).await.map(Some)
    }

    async fn thread_metadata(
        &self,
        thread_id: &str,
    ) -> Result<BTreeMap<String, String>, StoreError> {
        let rows = sqlx::query("SELECT key, value FROM thread_metadata WHERE thread_id = ?")
            .bind(thread_id)
            .fetch_all(self.pool())
            .await?;

        Ok(rows
            .iter()
            .map(|row| (row.get("key"), row.get("value")))
            .collect())
    }
}

fn hydrate_thread(
    row: &sqlx::sqlite::SqliteRow,
    metadata: BTreeMap<String, String>,
    queue_depth: u32,
    current_turn_id: Option<String>,
) -> Thread {
    Thread {
        thread_id: row.get("thread_id"),
        state: row.get("state"),
        // A settings blob that will not decode means the row was written by a
        // different major version, which is a satellite bug rather than
        // something to crash a request over.
        settings: ThreadSettings::decode(row.get::<Vec<u8>, _>("settings").as_slice()).ok(),
        created_at: Some(from_nanos(row.get("created_at"))),
        last_activity_at: Some(from_nanos(row.get("last_activity_at"))),
        expires_at: row.get::<Option<i64>, _>("expires_at").map(from_nanos),
        queue_depth,
        current_turn_id,
        pending_questions: None,
        pending_plan: None,
        latest_sequence: row.get::<i64, _>("latest_sequence").unsigned_abs(),
        metadata: metadata.into_iter().collect(),
        harness_session_id: row.get("harness_session_id"),
    }
}
