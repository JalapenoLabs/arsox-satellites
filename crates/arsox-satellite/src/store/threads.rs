// Copyright © 2026 Jalapeno Labs

//! Thread rows: creation, lookup, listing, and teardown.

use super::{
    DEFAULT_PAGE, MAX_PAGE, Store, StoreError, build_cursor, from_nanos, split_cursor, to_nanos,
};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::settings::v1::ThreadSettings;
use arsox_sdk::proto::thread::v1::{Thread, ThreadOrder, ThreadState, ThreadSummary};
use arsox_sdk::proto::turn::v1::TurnStatus;
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

/// How a thread's workspace provisioning ended.
///
/// Two outcomes rather than a boolean, because the state each lands in is not
/// obvious from `true` and `false` at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionOutcome {
    /// The workspace holds what the thread was created to work in.
    Ready,

    /// A repo the thread declared is not there. The thread keeps its queue and
    /// refuses to run it.
    Failed,
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

impl Store {
    /// Opens a thread, or returns the existing one when the idempotency key
    /// matches.
    ///
    /// A thread that declared repos opens `PROVISIONING` and stays there until
    /// they are cloned and their setup commands have run. Turns may be queued
    /// against it in the meantime and simply wait, because the claim query will
    /// not hand a runner work from a thread in that state. A thread that
    /// declared none has nothing to provision and opens `IDLE`.
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

        let opening_state = if new.settings.repos.is_empty() {
            ThreadState::Idle
        } else {
            ThreadState::Provisioning
        };

        let mut transaction = self.pool().begin().await?;

        sqlx::query(
            "INSERT INTO threads
               (thread_id, state, settings, created_at, last_activity_at, expires_at, idempotency_key)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&thread_id)
        .bind(i32::from(opening_state))
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
                state: opening_state.into(),
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
        let thread = self.thread_including_collected(thread_id).await?;

        // A collected thread is a tombstone, and saying which kind it is matters:
        // "this expired" tells a caller its TTL was too short, and "this was
        // destroyed" tells it something else did this deliberately. Both are
        // different facts from "no such thread", and a caller acts on each
        // differently.
        match ThreadState::try_from(thread.state).unwrap_or(ThreadState::Unspecified) {
            ThreadState::Expired => Err(StoreError::ThreadExpired(thread_id.to_owned())),
            ThreadState::Destroyed => Err(StoreError::ThreadDestroyed(thread_id.to_owned())),
            _live => Ok(thread),
        }
    }

    /// Reads a thread whether or not it has been collected.
    ///
    /// The collector needs this, because everything it operates on is either
    /// about to become a tombstone or already is one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn thread_including_collected(&self, thread_id: &str) -> Result<Thread, StoreError> {
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
                // Left to the caller, which fills it from the cached disk
                // measurement. Sizing a subtree is filesystem work, and a store
                // that walked the volume on every listing would make one `stat`
                // storm out of a query that touches no disk of its own.
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

    /// How many threads have a turn in flight.
    ///
    /// Counted from turns rather than from the thread state column, because a
    /// thread blocked on a question or watching a pull request still holds a
    /// running turn, and it is turns in flight that
    /// `ARSOX_MAX_CONCURRENT_THREADS` caps. This is the same set the claim query
    /// excludes when it looks for work, so the number a status response reports
    /// is the number the runner is arbitrating against.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn running_thread_count(&self) -> Result<u32, StoreError> {
        let running: i64 =
            sqlx::query_scalar("SELECT count(DISTINCT thread_id) FROM turns WHERE status = ?")
                .bind(i32::from(TurnStatus::Running))
                .fetch_one(self.pool())
                .await?;

        Ok(u32::try_from(running).unwrap_or(u32::MAX))
    }

    /// Releases a thread from `PROVISIONING` into the state its outcome earned.
    ///
    /// A provisioned thread goes to `IDLE` and its queue starts moving. A thread
    /// whose repos are not there goes to `PAUSED`, which is exactly the shape of
    /// the situation: alive, holding everything it was given, and refusing to
    /// start work it cannot do. The queue is kept rather than drained, because
    /// the incident says what failed and an operator who fixes the remote can
    /// resume the thread onto the turns already waiting.
    ///
    /// Only a thread that is still provisioning is moved. A thread destroyed
    /// while its repos cloned must stay destroyed, and a tombstone that came
    /// back as idle would be a thread nothing can collect twice.
    ///
    /// # Errors
    ///
    /// Returns a database error if the update fails.
    pub async fn finish_provisioning(
        &self,
        thread_id: &str,
        outcome: ProvisionOutcome,
    ) -> Result<(), StoreError> {
        let target = match outcome {
            ProvisionOutcome::Ready => ThreadState::Idle,
            ProvisionOutcome::Failed => ThreadState::Paused,
        };

        sqlx::query("UPDATE threads SET state = ? WHERE thread_id = ? AND state = ?")
            .bind(i32::from(target))
            .bind(thread_id)
            .bind(i32::from(ThreadState::Provisioning))
            .execute(self.pool())
            .await?;

        Ok(())
    }

    /// Threads still provisioning, which after a restart means none of them are.
    ///
    /// Nothing else ever revisits that state, so a thread the satellite stopped
    /// halfway through building would hold its queue forever. This is how the
    /// boot sweep finds them.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn provisioning_threads(&self) -> Result<Vec<Thread>, StoreError> {
        let rows = sqlx::query("SELECT thread_id FROM threads WHERE state = ?")
            .bind(i32::from(ThreadState::Provisioning))
            .fetch_all(self.pool())
            .await?;

        let mut threads = Vec::with_capacity(rows.len());
        for row in &rows {
            threads.push(self.thread(row.get::<&str, _>("thread_id")).await?);
        }

        Ok(threads)
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
        self.thread(thread_id).await?;
        self.collect_thread(thread_id, ThreadState::Destroyed).await
    }

    /// Turns a thread into a tombstone and frees everything it held.
    ///
    /// Turns, events, and metadata go. The thread row stays, carrying the state
    /// it was collected into, so a later request can be told what happened
    /// rather than that the thread never existed.
    ///
    /// Incidents are deliberately untouched. They have no foreign key to the
    /// thread and their own retention, because "why did last night's run go
    /// wrong" is a question asked after the thread is gone.
    ///
    /// Removing the workspace subtree is the caller's job. Doing it here would
    /// put a filesystem operation inside a database transaction, where a slow
    /// unlink holds a write lock against every other thread on the satellite.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the id is unknown.
    pub async fn collect_thread(
        &self,
        thread_id: &str,
        state: ThreadState,
    ) -> Result<Thread, StoreError> {
        let thread = self.thread_including_collected(thread_id).await?;

        let mut transaction = self.pool().begin().await?;

        for statement in [
            "DELETE FROM events WHERE thread_id = ?",
            "DELETE FROM turn_metadata WHERE turn_id IN (SELECT turn_id FROM turns WHERE thread_id = ?)",
            "DELETE FROM turns WHERE thread_id = ?",
            "DELETE FROM thread_metadata WHERE thread_id = ?",
        ] {
            sqlx::query(statement)
                .bind(thread_id)
                .execute(&mut *transaction)
                .await?;
        }

        // The expiry is cleared so the collector cannot pick the same tombstone
        // up on its next pass, which would remove a workspace that is already
        // gone once a minute forever.
        sqlx::query("UPDATE threads SET state = ?, expires_at = NULL WHERE thread_id = ?")
            .bind(i32::from(state))
            .bind(thread_id)
            .execute(&mut *transaction)
            .await?;

        transaction.commit().await?;

        Ok(Thread {
            state: state.into(),
            queue_depth: 0,
            current_turn_id: None,
            expires_at: None,
            metadata: std::collections::HashMap::new(),
            ..thread
        })
    }

    /// Threads whose idle TTL has elapsed.
    ///
    /// A thread with work in flight is never returned, however old its expiry
    /// looks. The TTL is idle time, and collecting a thread mid-turn would
    /// delete the work it is doing right now. Activity slides the expiry
    /// forward, so this only matters for a turn that runs longer than the TTL
    /// without producing anything, but "only matters rarely" is not a reason to
    /// leave a race that destroys work.
    ///
    /// A thread still provisioning is held back for the same reason. A clone in
    /// progress is work in flight, and removing the subtree underneath it would
    /// leave git writing into a directory that no longer exists.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn expired_threads(&self, limit: u32) -> Result<Vec<String>, StoreError> {
        let now = to_nanos(&Timestamp::now());

        let rows = sqlx::query(
            "SELECT t.thread_id
               FROM threads t
              WHERE t.expires_at IS NOT NULL
                AND t.expires_at <= ?
                AND t.state NOT IN (?, ?, ?)
                AND NOT EXISTS (
                      SELECT 1 FROM turns running
                       WHERE running.thread_id = t.thread_id
                         AND running.status = ?
                    )
              ORDER BY t.expires_at ASC
              LIMIT ?",
        )
        .bind(now)
        .bind(i32::from(ThreadState::Expired))
        .bind(i32::from(ThreadState::Destroyed))
        .bind(i32::from(ThreadState::Provisioning))
        .bind(i32::from(TurnStatus::Running))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        Ok(rows.iter().map(|row| row.get("thread_id")).collect())
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
