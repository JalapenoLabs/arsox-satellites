// Copyright © 2026 Jalapeno Labs

//! The event log.
//!
//! Every event is persisted before it is sent, so a consumer that reconnects
//! with `from_sequence` replays exactly what it missed and a network blip costs
//! nothing.
//!
//! # Secrets are masked here, once
//!
//! [`Store::append_event`] is the one door every event goes through, so it is
//! where a thread's secrets are masked out. Masking above it, at each of the
//! places that build an event, would be a rule somebody could forget on the
//! next one; masking here means the persisted copy and the published copy are
//! the same masked bytes and cannot disagree about what a consumer was allowed
//! to see.
//!
//! The redactor arrives on [`AppendEvent`] rather than being looked up, because
//! building one costs a settings decode and an automaton and an event is not
//! the place to pay for either. It is built once per thread, at the claim or at
//! provisioning, and travels with the work.

use super::{Store, StoreError, from_nanos, to_nanos};
use crate::redaction::Redactor;
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::event::v1::ThreadEvent;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use prost::Message as _;
use sqlx::Row as _;

/// An event on its way into the log, before it has a sequence number.
#[derive(Debug, Clone)]
pub struct AppendEvent {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub member_id: Option<String>,
    pub type_name: String,
    pub occurred_at: Option<Timestamp>,
    pub payload: Payload,

    /// The thread's secrets, masked out of the payload before it is written.
    ///
    /// A required field rather than an optional one, so a new place that
    /// appends events has to say which thread's secrets apply instead of
    /// inheriting silence. [`Redactor::none`] is the honest answer for text the
    /// satellite wrote itself and for a thread that declared no credentials.
    pub redactor: Redactor,
}

impl Store {
    /// Appends an event and returns it with its sequence number.
    ///
    /// The sequence is issued inside the same transaction that writes the row,
    /// which is what makes it gapless and monotonic per thread. Handing out the
    /// number first and writing afterwards would leave a hole in the stream
    /// whenever the write failed, and a consumer replaying across that hole
    /// would wait forever for an event that does not exist.
    ///
    /// Every secret the thread declared is masked out of the payload before it
    /// is written, so the row on disk and the frame on the wire carry the same
    /// masked text. See the module docs for why that happens here.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ThreadNotFound`] when the thread is unknown.
    pub async fn append_event(&self, event: AppendEvent) -> Result<ThreadEvent, StoreError> {
        let occurred_at = event.occurred_at.unwrap_or_else(Timestamp::now);
        let occurred_nanos = to_nanos(&occurred_at);

        let mut transaction = self.pool().begin().await?;

        let sequence: i64 = sqlx::query_scalar(
            "UPDATE threads SET latest_sequence = latest_sequence + 1
              WHERE thread_id = ?
             RETURNING latest_sequence",
        )
        .bind(&event.thread_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| StoreError::ThreadNotFound(event.thread_id.clone()))?;

        let mut stored = ThreadEvent {
            sequence: sequence.unsigned_abs(),
            thread_id: event.thread_id.clone(),
            turn_id: event.turn_id.clone(),
            occurred_at: Some(occurred_at),
            r#type: event.type_name.clone(),
            member_id: event.member_id.clone(),
            payload: Some(event.payload),
        };

        // Before the encode, so the masked text is what is persisted and what
        // is published. A thread with no secrets pays one branch for this.
        event.redactor.redact_event(&mut stored);

        sqlx::query(
            "INSERT INTO events
               (thread_id, sequence, turn_id, member_id, type, occurred_at, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&event.thread_id)
        .bind(sequence)
        .bind(event.turn_id.as_deref())
        .bind(event.member_id.as_deref())
        .bind(&event.type_name)
        .bind(occurred_nanos)
        .bind(stored.encode_to_vec())
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        // Published only after the write commits. Publishing first would let a
        // live consumer see an event that a reconnecting one could never replay,
        // which is a difference between two views of the same stream.
        if let Some(bus) = self.bus() {
            bus.publish(std::sync::Arc::new(stored.clone()));
        }

        Ok(stored)
    }

    /// Reads a thread's events after a sequence number, oldest first.
    ///
    /// `after` is exclusive, so a consumer passes the last sequence it actually
    /// handled and receives everything since.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn events_after(
        &self,
        thread_id: &str,
        after: u64,
        limit: u32,
    ) -> Result<Vec<ThreadEvent>, StoreError> {
        let rows = sqlx::query(
            "SELECT payload, sequence, occurred_at FROM events
              WHERE thread_id = ? AND sequence > ?
              ORDER BY sequence ASC
              LIMIT ?",
        )
        .bind(thread_id)
        .bind(i64::try_from(after).unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .filter_map(|row| {
                // A payload that will not decode was written by a different
                // major version. Skipping it keeps the rest of the replay
                // usable, and the sequence gap is visible to the consumer.
                ThreadEvent::decode(row.get::<Vec<u8>, _>("payload").as_slice()).ok()
            })
            .collect())
    }

    /// The oldest sequence still retained for a thread.
    ///
    /// A consumer asking to resume from before this has lost history and is told
    /// so with `STREAM_SEQUENCE_EXPIRED`, rather than silently receiving a
    /// stream that skips events it will never see.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn oldest_sequence(&self, thread_id: &str) -> Result<Option<u64>, StoreError> {
        let oldest: Option<i64> =
            sqlx::query_scalar("SELECT min(sequence) FROM events WHERE thread_id = ?")
                .bind(thread_id)
                .fetch_one(self.pool())
                .await?;

        Ok(oldest.map(i64::unsigned_abs))
    }

    /// Reconstructs when an event was recorded, for callers that only kept the
    /// sequence.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn event_recorded_at(
        &self,
        thread_id: &str,
        sequence: u64,
    ) -> Result<Option<Timestamp>, StoreError> {
        let nanos: Option<i64> = sqlx::query_scalar(
            "SELECT occurred_at FROM events WHERE thread_id = ? AND sequence = ?",
        )
        .bind(thread_id)
        .bind(i64::try_from(sequence).unwrap_or(i64::MAX))
        .fetch_optional(self.pool())
        .await?;

        Ok(nanos.map(from_nanos))
    }
}
