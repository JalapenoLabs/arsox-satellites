// Copyright © 2026 Jalapeno Labs

//! The satellite's embedded database.
//!
//! `SQLite` through `sqlx`, in WAL mode, with migrations embedded in the binary.
//! There is no external database to run, which is the point: a satellite is one
//! container and one volume.
//!
//! # What lives here
//!
//! Threads, queued turns, the event log, and incidents. Losing this file means
//! losing every thread you intended to resume and every record of what went
//! wrong, which is why `/var/arsox` is mounted as a named volume.
//!
//! # Errors speak the contract
//!
//! [`StoreError`] maps onto the same [`ErrorCode`] values a client receives, so
//! a missing thread is one concept from the query to the HTTP response rather
//! than being translated twice and drifting apart in the middle.

mod claims;
mod events;
mod incidents;
mod threads;
mod turns;

pub use claims::{ClaimedTurn, Interrupted};
pub use events::AppendEvent;
pub use incidents::{IncidentFilter, IncidentListing};
pub use threads::{Listing, NewThread, ProvisionOutcome, StoredThread, ThreadFilter};
pub use turns::{Drained, NewTurn, StoredTurn};

use anyhow::{Context as _, Result};
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::error::v1::ErrorCode;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Pool, Sqlite};
use std::path::Path;
use std::str::FromStr as _;

/// Nanoseconds in one second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Page size used when a caller asks for none.
const DEFAULT_PAGE: u32 = 50;

/// Largest page a caller can ask for.
const MAX_PAGE: u32 = 500;

/// Splits a listing cursor back into its sort key and row id.
///
/// Every listing pages the same way, so the format lives here rather than once
/// per listing. See [`build_cursor`] for why it carries two values.
pub(crate) fn split_cursor(cursor: &str) -> Option<(&str, &str)> {
    cursor.split_once('|')
}

/// Builds the cursor a client sends back to continue a listing.
///
/// The sort key travels with the row id because a sort key is not unique: a
/// cursor carrying only a timestamp would repeat or skip every row sharing a
/// value with the one at the page boundary. The pair makes paging total.
pub(crate) fn build_cursor(sort_key: &str, row_id: &str) -> String {
    format!("{sort_key}|{row_id}")
}

/// Migrations are compiled into the binary rather than shipped beside it.
///
/// A satellite that finds its schema at runtime can be started against the wrong
/// one; a satellite that carries it cannot.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Anything the store can refuse to do.
///
/// Every variant that a caller could reasonably act on carries the contract code
/// it becomes, so the mapping lives here once rather than at each call site.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("thread {0} not found")]
    ThreadNotFound(String),

    #[error("turn {0} not found")]
    TurnNotFound(String),

    #[error("thread {0} has reached its queue depth cap")]
    QueueFull(String),

    #[error("thread {0} has been destroyed")]
    ThreadDestroyed(String),

    #[error("thread {0} expired and its workspace was collected")]
    ThreadExpired(String),

    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl StoreError {
    /// The contract code this failure becomes on the wire.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::ThreadNotFound(_) => ErrorCode::ThreadNotFound,
            Self::TurnNotFound(_) => ErrorCode::TurnNotFound,
            Self::QueueFull(_) => ErrorCode::TurnQueueFull,
            Self::ThreadDestroyed(_) => ErrorCode::ThreadDestroyed,
            Self::ThreadExpired(_) => ErrorCode::ThreadExpired,
            // A database failure is a satellite bug or a broken volume, not
            // something the caller did.
            Self::Database(_) => ErrorCode::Internal,
        }
    }

    /// Whether retrying the same request could plausibly succeed.
    #[must_use]
    pub fn retryable(&self) -> bool {
        match self {
            // The queue drains, so the same submission may fit later, and a
            // database failure is usually a lock or a busy volume rather than
            // anything about the request.
            Self::QueueFull(_) | Self::Database(_) => true,
            Self::ThreadNotFound(_)
            | Self::TurnNotFound(_)
            | Self::ThreadDestroyed(_)
            | Self::ThreadExpired(_) => false,
        }
    }
}

/// A handle to the satellite's database.
///
/// Cheap to clone: the pool is shared, so handlers hold a handle rather than a
/// connection.
#[derive(Debug, Clone)]
pub struct Store {
    pool: Pool<Sqlite>,

    /// Where appended events are published for live consumers.
    ///
    /// Held here rather than left to callers because a publish that can be
    /// forgotten at one call site is a subscriber that silently misses events.
    /// Absent in tests that only exercise storage.
    bus: Option<crate::stream::EventBus>,

    /// How many harness session ids have been written since this store opened.
    ///
    /// The runner records a session id once per session rather than once per
    /// line repeating it, and "once" is a claim about writes that no row can
    /// answer: every write leaves the same value behind. The count is the honest
    /// observable, so it is kept here, behind `test-util` because nothing in the
    /// satellite reads it.
    ///
    /// Shared across clones, since the runner holds one handle and whatever asks
    /// the question holds another.
    #[cfg(feature = "test-util")]
    session_writes: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Store {
    /// Opens the database at `path`, creating and migrating it if needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent directory cannot be created, the file
    /// cannot be opened, or a migration fails.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // Readers do not block the writer, which matters because every
            // event append is a write and every stream replay is a read.
            .journal_mode(SqliteJournalMode::Wal)
            // Durable enough for a crash of the process, which is the failure
            // this database is meant to survive. FULL would fsync on every
            // event append and events arrive continuously.
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true);

        Self::connect(options).await
    }

    /// Opens a private in-memory database, for tests.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema cannot be created.
    pub async fn open_in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .context("in-memory connection string should parse")?
            .foreign_keys(true);

        Self::connect(options).await
    }

    async fn connect(options: SqliteConnectOptions) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            // `SQLite` takes one writer at a time regardless, and an in-memory
            // database is per-connection, so a single connection keeps both
            // cases honest rather than fast in one and wrong in the other.
            .max_connections(1)
            .connect_with(options)
            .await
            .context("failed to open the satellite database")?;

        MIGRATIONS
            .run(&pool)
            .await
            .context("failed to migrate the satellite database")?;

        Ok(Self {
            pool,
            bus: None,
            #[cfg(feature = "test-util")]
            session_writes: std::sync::Arc::default(),
        })
    }

    /// Attaches the bus that appended events are published to.
    #[must_use]
    pub fn with_bus(mut self, bus: crate::stream::EventBus) -> Self {
        self.bus = Some(bus);
        self
    }

    pub(crate) fn bus(&self) -> Option<&crate::stream::EventBus> {
        self.bus.as_ref()
    }

    pub(crate) fn pool(&self) -> &Pool<Sqlite> {
        &self.pool
    }

    /// Notes that a harness session id reached the database.
    #[cfg(feature = "test-util")]
    pub(crate) fn count_session_write(&self) {
        self.session_writes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// How many harness session ids have been written since this store opened.
    ///
    /// The observable behind "recorded once per session": a row holds the id
    /// whether it was written once or forty times, so only the count can tell
    /// the two apart.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn harness_session_writes(&self) -> usize {
        self.session_writes
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Collapses a contract timestamp into the single integer the schema stores.
///
/// The display zone is dropped rather than persisted: every instant recorded is
/// UTC, and the zone is presentational.
pub(crate) fn to_nanos(stamp: &Timestamp) -> i64 {
    stamp
        .epoch_seconds
        .saturating_mul(NANOS_PER_SECOND)
        .saturating_add(i64::from(stamp.nanos))
}

/// Rebuilds a contract timestamp from stored nanoseconds.
pub(crate) fn from_nanos(nanos: i64) -> Timestamp {
    let seconds = nanos.div_euclid(NANOS_PER_SECOND);
    let fraction = nanos.rem_euclid(NANOS_PER_SECOND);

    Timestamp {
        epoch_seconds: seconds,
        // `rem_euclid` is never negative, which is what the contract requires of
        // this field even for instants before the epoch.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a euclidean remainder of a billion always fits u32 and is never negative"
        )]
        nanos: fraction as u32,
        timezone: arsox_sdk::helpers::DEFAULT_TIMEZONE.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_in_memory_store_migrates_cleanly() {
        let store = Store::open_in_memory()
            .await
            .expect("should open and migrate");

        // Proves the schema exists rather than only that the file opened.
        let tables: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table'")
                .fetch_one(store.pool())
                .await
                .expect("should query the schema");

        assert!(
            tables >= 6,
            "expected the full schema, found {tables} tables"
        );
    }

    #[test]
    fn timestamps_round_trip_through_the_stored_integer() {
        let stamp = Timestamp {
            epoch_seconds: 1_700_000_000,
            nanos: 123_456_789,
            timezone: "America/Denver".to_owned(),
        };

        let restored = from_nanos(to_nanos(&stamp));

        assert_eq!(restored.epoch_seconds, stamp.epoch_seconds);
        assert_eq!(restored.nanos, stamp.nanos);
        // The zone is presentational and deliberately not persisted, so it comes
        // back as UTC rather than as whatever it went in as.
        assert_eq!(restored.timezone, "Etc/UTC");
    }

    #[test]
    fn instants_before_the_epoch_keep_a_non_negative_nanos_field() {
        let stamp = Timestamp {
            epoch_seconds: -1,
            nanos: 500_000_000,
            timezone: "Etc/UTC".to_owned(),
        };

        let restored = from_nanos(to_nanos(&stamp));

        assert_eq!(restored.epoch_seconds, -1);
        assert_eq!(restored.nanos, 500_000_000);
    }
}

#[cfg(test)]
mod store_behaviour {
    use super::*;
    use crate::redaction::Redactor;
    use arsox_sdk::proto::common::v1::Duration;
    use arsox_sdk::proto::event::v1::{AgentMessage, thread_event::Payload};
    use arsox_sdk::proto::incident::v1::{Disposition, Incident};
    use arsox_sdk::proto::settings::v1::{ResourceLimits, ThreadSettings};
    use arsox_sdk::proto::thread::v1::{ThreadOrder, ThreadState};
    use arsox_sdk::proto::turn::v1::{TurnOrder, TurnStatus};
    use std::collections::BTreeMap;

    async fn store() -> Store {
        Store::open_in_memory().await.expect("should open")
    }

    fn thread_named(tenant: &str) -> NewThread {
        let mut metadata = BTreeMap::new();
        metadata.insert("tenant".to_owned(), tenant.to_owned());

        NewThread {
            settings: ThreadSettings::default(),
            metadata,
            idempotency_key: None,
        }
    }

    fn work_on(thread_id: &str) -> NewTurn {
        NewTurn {
            thread_id: thread_id.to_owned(),
            prompt: "work".to_owned(),
            metadata: BTreeMap::new(),
            idempotency_key: None,
            satellite_initiated: false,
            triggered_by_turn_id: None,
        }
    }

    #[tokio::test]
    async fn a_created_thread_reads_back_with_its_metadata() {
        let store = store().await;
        let created = store
            .create_thread(thread_named("acme"))
            .await
            .expect("should create");

        assert!(created.created);
        // Generated by the satellite, never accepted from a client, because it
        // becomes a filesystem path.
        assert!(!created.thread.thread_id.is_empty());

        let fetched = store
            .thread(&created.thread.thread_id)
            .await
            .expect("should read back");

        assert_eq!(fetched.thread_id, created.thread.thread_id);
        assert_eq!(
            fetched.metadata.get("tenant").map(String::as_str),
            Some("acme")
        );
    }

    #[tokio::test]
    async fn an_idempotency_key_returns_the_original_rather_than_a_second_thread() {
        // The whole point: a create that times out in transit is safe to retry
        // instead of leaking a second workspace.
        let store = store().await;
        let mut request = thread_named("acme");
        request.idempotency_key = Some("request-1".to_owned());

        let first = store
            .create_thread(request.clone())
            .await
            .expect("should create");
        let second = store
            .create_thread(request)
            .await
            .expect("should deduplicate");

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.thread.thread_id, second.thread.thread_id);
    }

    #[tokio::test]
    async fn threads_list_chronologically_and_filter_on_metadata() {
        let store = store().await;
        let acme = store.create_thread(thread_named("acme")).await.expect("a");
        let other = store
            .create_thread(thread_named("globex"))
            .await
            .expect("b");

        let all = store
            .list_threads(&ThreadFilter::default())
            .await
            .expect("should list");
        assert_eq!(all.threads.len(), 2);
        // UUIDv7 sorts by creation time, so ordering by id is chronological and
        // needs no separate sort key.
        assert_eq!(all.threads[0].thread_id, acme.thread.thread_id);
        assert_eq!(all.threads[1].thread_id, other.thread.thread_id);

        let mut filter = ThreadFilter::default();
        filter
            .metadata
            .insert("tenant".to_owned(), "globex".to_owned());
        let filtered = store.list_threads(&filter).await.expect("should filter");

        assert_eq!(filtered.threads.len(), 1);
        assert_eq!(filtered.threads[0].thread_id, other.thread.thread_id);
    }

    #[tokio::test]
    async fn an_unknown_thread_is_a_specific_error_rather_than_an_empty_result() {
        let store = store().await;
        let error = store.thread("nope").await.expect_err("should not be found");

        assert!(matches!(error, StoreError::ThreadNotFound(_)));
        assert_eq!(error.code(), ErrorCode::ThreadNotFound);
        assert!(!error.retryable());
    }

    #[tokio::test]
    async fn destroying_a_thread_leaves_a_tombstone() {
        // Not a deletion. The row stays so a later request can be told the
        // thread was destroyed rather than that it never existed.
        let store = store().await;
        let created = store.create_thread(thread_named("acme")).await.expect("a");
        let id = created.thread.thread_id;

        store.destroy_thread(&id).await.expect("should destroy");

        assert!(matches!(
            store.thread(&id).await,
            Err(StoreError::ThreadDestroyed(_))
        ));

        let tombstone = store
            .thread_including_collected(&id)
            .await
            .expect("the tombstone is readable");
        assert_eq!(tombstone.state, i32::from(ThreadState::Destroyed));
    }

    #[tokio::test]
    async fn queued_turns_raise_the_queue_depth_and_run_oldest_first() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        for prompt in ["first", "second"] {
            let mut submit = work_on(&thread.thread_id);
            submit.prompt = prompt.to_owned();
            store.create_turn(submit).await.expect("should queue");
        }

        let refreshed = store.thread(&thread.thread_id).await.expect("should read");
        assert_eq!(refreshed.queue_depth, 2);
        assert_eq!(refreshed.current_turn_id, None);

        let turns = store
            .list_turns(&thread.thread_id, &[], TurnOrder::Unspecified, false)
            .await
            .expect("should list");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].prompt, "first");
    }

    #[tokio::test]
    async fn a_turn_idempotency_key_is_scoped_to_its_thread() {
        // Two threads may each retry a submit carrying the caller's own request
        // id without colliding with each other.
        let store = store().await;
        let one = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;
        let two = store
            .create_thread(thread_named("globex"))
            .await
            .expect("b")
            .thread;

        let keyed = |thread_id: &str| {
            let mut submit = work_on(thread_id);
            submit.idempotency_key = Some("req-9".to_owned());
            submit
        };

        let first = store
            .create_turn(keyed(&one.thread_id))
            .await
            .expect("first");
        let repeat = store
            .create_turn(keyed(&one.thread_id))
            .await
            .expect("repeat");
        let other = store
            .create_turn(keyed(&two.thread_id))
            .await
            .expect("other thread");

        assert!(first.queued);
        assert!(!repeat.queued, "the same key on one thread deduplicates");
        assert_eq!(first.turn.turn_id, repeat.turn.turn_id);
        assert!(other.queued, "the same key on another thread is a new turn");
    }

    #[tokio::test]
    async fn the_queue_cap_rejects_rather_than_accumulating_without_bound() {
        let store = store().await;
        let settings = ThreadSettings {
            resource_limits: Some(ResourceLimits {
                max_queued_turns: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        };
        let thread = store
            .create_thread(NewThread {
                settings,
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("a")
            .thread;

        store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect("first fits");
        let error = store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect_err("second does not");

        assert!(matches!(error, StoreError::QueueFull(_)));
        assert_eq!(error.code(), ErrorCode::TurnQueueFull);
        // The queue drains, so the same submission may fit later.
        assert!(error.retryable());
    }

    #[tokio::test]
    async fn cancelling_is_idempotent_once_a_turn_is_terminal() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;
        let turn = store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect("should queue")
            .turn;

        let cancelled = store
            .cancel_turn(&thread.thread_id, &turn.turn_id)
            .await
            .expect("should cancel");
        assert_eq!(cancelled.status, i32::from(TurnStatus::Cancelled));

        // A client racing the turn's own completion has done nothing wrong, so a
        // second cancel reports the state rather than failing.
        let again = store
            .cancel_turn(&thread.thread_id, &turn.turn_id)
            .await
            .expect("should be a no-op");
        assert_eq!(again.status, i32::from(TurnStatus::Cancelled));
    }

    #[tokio::test]
    async fn event_sequences_are_gapless_monotonic_and_replayable() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        for index in 0..5 {
            store
                .append_event(AppendEvent {
                    thread_id: thread.thread_id.clone(),
                    turn_id: None,
                    member_id: None,
                    type_name: "agent.message".to_owned(),
                    occurred_at: None,
                    payload: Payload::AgentMessage(AgentMessage {
                        author: None,
                        text: format!("message {index}"),
                    }),
                    redactor: Redactor::none(),
                })
                .await
                .expect("should append");
        }

        let all = store
            .events_after(&thread.thread_id, 0, 100)
            .await
            .expect("should replay");

        assert_eq!(all.len(), 5);
        // Gapless and one-based. A consumer replaying across a hole would wait
        // forever for an event that does not exist.
        let sequences: Vec<u64> = all.iter().map(|event| event.sequence).collect();
        assert_eq!(sequences, vec![1, 2, 3, 4, 5]);

        // `after` is exclusive, so a consumer passes the last sequence it
        // actually handled and receives everything since.
        let resumed = store
            .events_after(&thread.thread_id, 3, 100)
            .await
            .expect("should resume");
        assert_eq!(resumed.len(), 2);
        assert_eq!(resumed[0].sequence, 4);

        let refreshed = store.thread(&thread.thread_id).await.expect("should read");
        assert_eq!(refreshed.latest_sequence, 5);
    }

    #[tokio::test]
    async fn appending_to_an_unknown_thread_does_not_burn_a_sequence_number() {
        let store = store().await;
        let error = store
            .append_event(AppendEvent {
                thread_id: "nope".to_owned(),
                turn_id: None,
                member_id: None,
                type_name: "agent.message".to_owned(),
                occurred_at: None,
                payload: Payload::AgentMessage(AgentMessage::default()),
                redactor: Redactor::none(),
            })
            .await
            .expect_err("should not append");

        assert!(matches!(error, StoreError::ThreadNotFound(_)));
    }

    #[tokio::test]
    async fn a_paused_thread_holds_its_queue_instead_of_running_it() {
        // The point of pausing: nothing is lost, nothing starts. An operator
        // stopping a misbehaving thread should not have to destroy its
        // workspace to do it.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store
            .pause_thread(&thread.thread_id)
            .await
            .expect("should pause");
        store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect("should still accept work");

        assert!(
            store.claim_next_turn().await.expect("claim").is_none(),
            "a paused thread's work must not be claimed"
        );

        store
            .resume_thread(&thread.thread_id)
            .await
            .expect("should resume");

        assert!(
            store.claim_next_turn().await.expect("claim").is_some(),
            "resuming releases the queue"
        );
    }

    #[tokio::test]
    async fn pausing_twice_reports_the_state_rather_than_failing() {
        // An operator racing their own second click has done nothing wrong.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store.pause_thread(&thread.thread_id).await.expect("first");
        let again = store.pause_thread(&thread.thread_id).await.expect("second");

        assert_eq!(again.state, i32::from(ThreadState::Paused));
    }

    #[tokio::test]
    async fn draining_cancels_the_queue_and_leaves_the_running_turn_alone() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        for _queued in 0..3 {
            store
                .create_turn(work_on(&thread.thread_id))
                .await
                .expect("should queue");
        }

        let running = store
            .claim_next_turn()
            .await
            .expect("claim")
            .expect("there is work");

        let drained = store
            .drain_thread(&thread.thread_id)
            .await
            .expect("should drain");

        assert_eq!(drained.cancelled_turn_ids.len(), 2);
        assert_eq!(
            drained.running_turn_id.as_deref(),
            Some(running.turn.turn_id.as_str()),
            "draining reports what it deliberately left alone"
        );

        let (still_running, _result) = store
            .turn(&thread.thread_id, &running.turn.turn_id)
            .await
            .expect("should read");
        assert_eq!(still_running.status, i32::from(TurnStatus::Running));
    }

    #[tokio::test]
    async fn threads_can_be_ordered_by_most_recent_activity() {
        // What an operator scanning a fleet wants, and what creation order
        // cannot approximate on a satellite whose oldest thread is its busiest.
        let store = store().await;
        let first = store.create_thread(thread_named("acme")).await.expect("a");
        let second = store
            .create_thread(thread_named("globex"))
            .await
            .expect("b");

        // Touching the older thread makes it the most recently active.
        store
            .touch_thread(&first.thread.thread_id)
            .await
            .expect("should touch");

        let filter = ThreadFilter {
            order_by: ThreadOrder::LastActivity,
            descending: true,
            ..ThreadFilter::default()
        };
        let listed = store.list_threads(&filter).await.expect("should list");

        assert_eq!(listed.threads[0].thread_id, first.thread.thread_id);
        assert_eq!(listed.threads[1].thread_id, second.thread.thread_id);

        // Creation order is unchanged and still the default.
        let default = store
            .list_threads(&ThreadFilter::default())
            .await
            .expect("should list");
        assert_eq!(default.threads[0].thread_id, first.thread.thread_id);
        assert_eq!(default.threads[1].thread_id, second.thread.thread_id);
    }

    #[tokio::test]
    async fn a_cursor_carries_the_sort_key_so_paging_does_not_skip() {
        let store = store().await;
        let mut ids = Vec::new();
        for tenant in ["a", "b", "c"] {
            ids.push(
                store
                    .create_thread(thread_named(tenant))
                    .await
                    .expect("create")
                    .thread
                    .thread_id,
            );
        }

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let filter = ThreadFilter {
                limit: 2,
                after: cursor.clone(),
                ..ThreadFilter::default()
            };
            let page = store.list_threads(&filter).await.expect("should list");
            if page.threads.is_empty() {
                break;
            }
            seen.extend(page.threads.iter().map(|t| t.thread_id.clone()));
            cursor = Some(page.next_cursor);
        }

        assert_eq!(seen, ids, "paging must cover every thread exactly once");
    }

    #[tokio::test]
    async fn an_interrupted_turn_is_requeued_only_when_the_thread_asked_for_it() {
        let store = store().await;

        let opted_in = store
            .create_thread(NewThread {
                settings: ThreadSettings {
                    resume_interrupted_turns: true,
                    ..Default::default()
                },
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("a")
            .thread;
        let opted_out = store
            .create_thread(thread_named("acme"))
            .await
            .expect("b")
            .thread;

        for thread_id in [&opted_in.thread_id, &opted_out.thread_id] {
            store
                .create_turn(work_on(thread_id))
                .await
                .expect("should queue");
            store.claim_next_turn().await.expect("claim").expect("work");
        }

        let settled = store
            .settle_interrupted_turns()
            .await
            .expect("should settle");

        assert_eq!(settled.resumed.len(), 1, "one thread opted in");
        assert_eq!(settled.left_interrupted.len(), 1, "the other did not");

        // The opted-in turn is claimable again; the other waits for a human.
        let requeued = store.claim_next_turn().await.expect("claim");
        assert!(requeued.is_some());
        assert_eq!(
            requeued.expect("claimed").turn.thread_id,
            opted_in.thread_id
        );
    }

    #[tokio::test]
    async fn a_destroyed_thread_answers_destroyed_rather_than_missing() {
        // These are different facts. "Your id is wrong" and "this thread was
        // deliberately torn down" lead a caller to do different things, and
        // deleting the row would collapse them into the first.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store
            .destroy_thread(&thread.thread_id)
            .await
            .expect("should destroy");

        let error = store
            .thread(&thread.thread_id)
            .await
            .expect_err("a destroyed thread is not readable");

        assert_eq!(error.code(), ErrorCode::ThreadDestroyed);
        assert!(!error.retryable());
    }

    #[tokio::test]
    async fn collecting_a_thread_takes_its_turns_and_events_with_it() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect("should queue");
        store
            .append_event(AppendEvent {
                thread_id: thread.thread_id.clone(),
                turn_id: None,
                member_id: None,
                type_name: "agent.message".to_owned(),
                occurred_at: None,
                payload: Payload::AgentMessage(AgentMessage {
                    author: None,
                    text: "something happened".to_owned(),
                }),
                redactor: Redactor::none(),
            })
            .await
            .expect("should append");

        store
            .collect_thread(&thread.thread_id, ThreadState::Expired)
            .await
            .expect("should collect");

        // Retained history is bounded by the thread's lifetime, so an expired
        // thread cannot be replayed.
        let events = store
            .events_after(&thread.thread_id, 0, 100)
            .await
            .expect("should read");
        assert!(events.is_empty(), "history goes with the thread");

        // Its turns are gone with it, which the tombstone reports as an empty
        // queue. Listing them is refused outright, because the thread itself is
        // no longer readable.
        let tombstone = store
            .thread_including_collected(&thread.thread_id)
            .await
            .expect("the tombstone is readable");
        assert_eq!(tombstone.queue_depth, 0, "turns go with the thread");

        assert_eq!(
            store
                .list_turns(&thread.thread_id, &[], TurnOrder::Unspecified, false)
                .await
                .expect_err("a collected thread lists nothing")
                .code(),
            ErrorCode::ThreadExpired
        );
    }

    #[tokio::test]
    async fn incidents_outlive_the_thread_they_describe() {
        // The one thing in a thread's life that is deliberately not ephemeral.
        // "Why did last night's run go wrong" is asked after the thread is gone.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store
            .record_incident(
                &Incident {
                    incident_id: "incident-1".to_owned(),
                    thread_id: Some(thread.thread_id.clone()),
                    code: ErrorCode::HarnessCrashed.into(),
                    disposition: Disposition::Recovered.into(),
                    message: "the harness died and restarted".to_owned(),
                    ..Default::default()
                },
                &Redactor::none(),
            )
            .await
            .expect("should record");

        store
            .collect_thread(&thread.thread_id, ThreadState::Expired)
            .await
            .expect("should collect");

        let incidents = store
            .incidents_for_thread(&thread.thread_id)
            .await
            .expect("should read");

        assert_eq!(incidents.len(), 1, "the evidence survives the workspace");
    }

    /// Records an incident against a thread, with the fields a test names.
    async fn record(
        store: &Store,
        thread_id: &str,
        turn_id: Option<&str>,
        code: ErrorCode,
        disposition: Disposition,
        occurred_at: i64,
    ) -> String {
        let incident_id = uuid::Uuid::now_v7().to_string();

        store
            .record_incident(
                &Incident {
                    incident_id: incident_id.clone(),
                    thread_id: Some(thread_id.to_owned()),
                    turn_id: turn_id.map(str::to_owned),
                    code: code.into(),
                    disposition: disposition.into(),
                    message: "something happened".to_owned(),
                    occurred_at: Some(from_nanos(occurred_at)),
                    ..Default::default()
                },
                &Redactor::none(),
            )
            .await
            .expect("should record");

        incident_id
    }

    /// Three incidents over two threads, distinct on every filterable axis.
    struct Seeded {
        store: Store,
        thread_id: String,

        /// Fatal, `HARNESS_CRASHED`, `turn-1`, at 1000.
        crashed: String,

        /// Blocked, `PERMISSION_COMMAND_DENIED`, `turn-2`, at 2000.
        denied: String,

        /// Fatal, `HARNESS_CRASHED`, no turn, on the other thread, at 3000.
        elsewhere: String,
    }

    async fn seeded_incidents() -> Seeded {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;
        let other = store
            .create_thread(thread_named("globex"))
            .await
            .expect("b")
            .thread;

        Seeded {
            crashed: record(
                &store,
                &thread.thread_id,
                Some("turn-1"),
                ErrorCode::HarnessCrashed,
                Disposition::Fatal,
                1_000,
            )
            .await,
            denied: record(
                &store,
                &thread.thread_id,
                Some("turn-2"),
                ErrorCode::PermissionCommandDenied,
                Disposition::Blocked,
                2_000,
            )
            .await,
            elsewhere: record(
                &store,
                &other.thread_id,
                None,
                ErrorCode::HarnessCrashed,
                Disposition::Fatal,
                3_000,
            )
            .await,
            thread_id: thread.thread_id,
            store,
        }
    }

    /// The incidents a filter selects, in the order the store returns them.
    async fn listed(store: &Store, filter: IncidentFilter) -> Vec<String> {
        store
            .list_incidents(&filter)
            .await
            .expect("should list")
            .incidents
            .into_iter()
            .map(|incident| incident.incident_id)
            .collect()
    }

    #[tokio::test]
    async fn an_incident_listing_filters_on_thread_turn_code_and_disposition() {
        let seeded = seeded_incidents().await;
        let store = &seeded.store;

        // An empty filter does not filter, which is what an empty repeated field
        // means in the contract. The alternative returns nothing for the request
        // a caller is most likely to send first.
        assert_eq!(
            listed(store, IncidentFilter::default()).await,
            vec![
                seeded.crashed.clone(),
                seeded.denied.clone(),
                seeded.elsewhere.clone()
            ]
        );

        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    thread_ids: vec![seeded.thread_id.clone()],
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.crashed.clone(), seeded.denied.clone()]
        );
        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    turn_ids: vec!["turn-2".to_owned()],
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.denied.clone()]
        );
        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    codes: vec![ErrorCode::HarnessCrashed.into()],
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.crashed, seeded.elsewhere]
        );
        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    dispositions: vec![Disposition::Blocked.into()],
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.denied]
        );
    }

    #[tokio::test]
    async fn an_incident_time_window_is_half_open_so_adjacent_windows_tile() {
        // An operator walking an incident log an hour at a time sees every
        // incident exactly once, rather than seeing the ones on a boundary in
        // both windows.
        let seeded = seeded_incidents().await;
        let store = &seeded.store;

        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    occurred_after: Some(1_000),
                    occurred_before: Some(2_000),
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.crashed]
        );
        assert_eq!(
            listed(
                store,
                IncidentFilter {
                    occurred_after: Some(2_000),
                    occurred_before: Some(3_000),
                    ..IncidentFilter::default()
                }
            )
            .await,
            vec![seeded.denied]
        );
    }

    #[tokio::test]
    async fn an_incident_cursor_carries_its_sort_key_so_paging_does_not_skip() {
        // Incidents sort by when they happened, and a timestamp is not unique.
        // These three share one, so an id-only cursor would repeat or skip them.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        let mut ids = Vec::new();
        for _same_instant in 0..3 {
            ids.push(
                record(
                    &store,
                    &thread.thread_id,
                    None,
                    ErrorCode::CheckerFailed,
                    Disposition::Degraded,
                    7_000,
                )
                .await,
            );
        }
        ids.sort();

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = store
                .list_incidents(&IncidentFilter {
                    limit: 2,
                    after: cursor.clone(),
                    ..IncidentFilter::default()
                })
                .await
                .expect("should list");

            if page.incidents.is_empty() {
                break;
            }
            seen.extend(
                page.incidents
                    .iter()
                    .map(|incident| incident.incident_id.clone()),
            );
            cursor = Some(page.next_cursor);
        }

        assert_eq!(seen, ids, "paging must cover every incident exactly once");
    }

    #[tokio::test]
    async fn a_collected_thread_still_answers_an_incident_listing() {
        // The endpoint must not 410 on a tombstone. "Why did last night's run go
        // wrong" is asked precisely when the workspace is already gone.
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        let recorded = record(
            &store,
            &thread.thread_id,
            None,
            ErrorCode::HarnessCrashed,
            Disposition::Fatal,
            1_000,
        )
        .await;

        store
            .collect_thread(&thread.thread_id, ThreadState::Expired)
            .await
            .expect("should collect");

        let listing = store
            .list_incidents(&IncidentFilter {
                thread_ids: vec![thread.thread_id],
                ..IncidentFilter::default()
            })
            .await
            .expect("should list");

        assert_eq!(listing.incidents.len(), 1);
        assert_eq!(listing.incidents[0].incident_id, recorded);
    }

    #[tokio::test]
    async fn a_turn_counts_its_incidents_by_disposition() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        for disposition in [
            Disposition::Degraded,
            Disposition::Degraded,
            Disposition::Fatal,
        ] {
            record(
                &store,
                &thread.thread_id,
                Some("turn-1"),
                ErrorCode::CheckerFailed,
                disposition,
                1_000,
            )
            .await;
        }
        record(
            &store,
            &thread.thread_id,
            Some("turn-2"),
            ErrorCode::CheckerFailed,
            Disposition::Blocked,
            1_000,
        )
        .await;

        let counts = store.incident_counts("turn-1").await.expect("should count");

        assert_eq!(counts.degraded, 2);
        assert_eq!(counts.fatal, 1);
        // Another turn's incidents are another turn's.
        assert_eq!(counts.blocked, 0);
        assert_eq!(counts.recovered, 0);
    }

    #[tokio::test]
    async fn only_threads_past_their_expiry_are_collected() {
        let store = store().await;

        let expiring = store
            .create_thread(NewThread {
                settings: ThreadSettings {
                    idle_ttl: Some(Duration {
                        seconds: 0,
                        nanos: 1,
                    }),
                    ..Default::default()
                },
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("a")
            .thread;

        let living = store
            .create_thread(NewThread {
                settings: ThreadSettings {
                    idle_ttl: Some(Duration {
                        seconds: 3_600,
                        nanos: 0,
                    }),
                    ..Default::default()
                },
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("b")
            .thread;

        // A thread with no TTL at all cannot be collected on a clock.
        let forever = store
            .create_thread(thread_named("acme"))
            .await
            .expect("c")
            .thread;

        let expired = store.expired_threads(32).await.expect("should query");

        assert_eq!(expired, vec![expiring.thread_id]);
        assert!(!expired.contains(&living.thread_id));
        assert!(!expired.contains(&forever.thread_id));
    }

    #[tokio::test]
    async fn a_thread_with_work_in_flight_is_never_collected() {
        // The TTL is idle time. Collecting a thread mid-turn would delete the
        // work it is doing right now.
        let store = store().await;
        let thread = store
            .create_thread(NewThread {
                settings: ThreadSettings {
                    idle_ttl: Some(Duration {
                        seconds: 0,
                        nanos: 1,
                    }),
                    ..Default::default()
                },
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("a")
            .thread;

        store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect("should queue");
        store.claim_next_turn().await.expect("claim").expect("work");

        let expired = store.expired_threads(32).await.expect("should query");

        assert!(
            expired.is_empty(),
            "a running turn holds the thread against collection"
        );
    }

    #[tokio::test]
    async fn a_tombstone_is_not_collected_twice() {
        // Its expiry is cleared, so the sweep cannot pick it back up and remove
        // a workspace that is already gone once a minute forever.
        let store = store().await;
        let thread = store
            .create_thread(NewThread {
                settings: ThreadSettings {
                    idle_ttl: Some(Duration {
                        seconds: 0,
                        nanos: 1,
                    }),
                    ..Default::default()
                },
                metadata: BTreeMap::new(),
                idempotency_key: None,
            })
            .await
            .expect("a")
            .thread;

        assert_eq!(store.expired_threads(32).await.expect("query").len(), 1);

        store
            .collect_thread(&thread.thread_id, ThreadState::Expired)
            .await
            .expect("should collect");

        assert!(
            store.expired_threads(32).await.expect("query").is_empty(),
            "a tombstone has no expiry left to trip over"
        );
    }

    #[tokio::test]
    async fn work_cannot_be_queued_onto_a_collected_thread() {
        let store = store().await;
        let thread = store
            .create_thread(thread_named("acme"))
            .await
            .expect("a")
            .thread;

        store
            .collect_thread(&thread.thread_id, ThreadState::Expired)
            .await
            .expect("should collect");

        let error = store
            .create_turn(work_on(&thread.thread_id))
            .await
            .expect_err("an expired thread takes no work");

        assert_eq!(error.code(), ErrorCode::ThreadExpired);
    }

    #[tokio::test]
    async fn events_survive_a_reopen() {
        // The database is the reason a satellite restart does not lose a thread.
        let directory = std::env::temp_dir().join(format!("arsox-test-{}", uuid::Uuid::now_v7()));
        let path = directory.join("arsox.db");

        let thread_id = {
            let store = Store::open(&path).await.expect("should open");
            let thread = store
                .create_thread(thread_named("acme"))
                .await
                .expect("a")
                .thread;
            store
                .append_event(AppendEvent {
                    thread_id: thread.thread_id.clone(),
                    turn_id: None,
                    member_id: None,
                    type_name: "agent.message".to_owned(),
                    occurred_at: None,
                    payload: Payload::AgentMessage(AgentMessage {
                        author: None,
                        text: "survives".to_owned(),
                    }),
                    redactor: Redactor::none(),
                })
                .await
                .expect("should append");
            thread.thread_id
        };

        let reopened = Store::open(&path).await.expect("should reopen");
        let events = reopened
            .events_after(&thread_id, 0, 10)
            .await
            .expect("should replay");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 1);

        // Best effort: a leftover temp directory is noise, not a test failure.
        drop(tokio::fs::remove_dir_all(&directory).await);
    }
}
