// Copyright © 2026 Jalapeno Labs

//! Incidents: everything that went wrong without failing a request.
//!
//! # One call writes both copies
//!
//! An incident lives in two places at once. It is a row, because "why did last
//! night's run go wrong" is asked long after the turn ended, and it is an event,
//! because a consumer watching a thread has to learn about a failure while the
//! turn is still running. [`Store::report_incident`] writes both.
//!
//! Callers reach for that rather than for [`Store::record_incident`], which is
//! the durable half on its own. Two calls at each site would be two rules to
//! remember, and the one that gets forgotten is the stream: a failure recorded
//! and never emitted is invisible to every consumer until somebody thinks to
//! query for it.
//!
//! # Incidents outlive their thread
//!
//! The table carries no foreign key to `threads` and is never touched by
//! collection. That is deliberate, and it is why the listing here refuses to
//! care whether a thread is alive: an incident from a collected thread is
//! exactly the incident an operator came looking for.

use super::{Store, StoreError, from_nanos, to_nanos};
use crate::redaction::Redactor;
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::incident::v1::Incident;
use sqlx::Row as _;

impl Store {
    /// Records an incident and puts it on its thread's stream.
    ///
    /// The event goes first, so the row can carry the sequence it landed at.
    /// That is what lets a query result be located in the stream, and a stream
    /// frame be looked up afterwards.
    ///
    /// A satellite-scoped incident carries no thread and reaches no thread
    /// stream: an unreachable proxy at boot belongs to the satellite, and the
    /// thread sockets deliberately carry thread content only. It is still
    /// recorded, which is the whole reason `thread_id` is nullable.
    ///
    /// A stream that will not take the event does not cost the record. The
    /// durable copy is the half an operator queries later, and losing it because
    /// a thread was collected a moment ago would be precisely the silent failure
    /// incidents exist to prevent.
    ///
    /// # Errors
    ///
    /// Returns a database error if the incident cannot be recorded.
    pub async fn report_incident(
        &self,
        mut incident: Incident,
        redactor: &Redactor,
    ) -> Result<Incident, StoreError> {
        if let Some(thread_id) = incident.thread_id.clone() {
            let append = super::AppendEvent {
                thread_id,
                turn_id: incident.turn_id.clone(),
                member_id: incident.member_id.clone(),
                type_name: "incident".to_owned(),
                occurred_at: incident.occurred_at.clone(),
                payload: Payload::Incident(incident.clone()),
                redactor: redactor.clone(),
            };

            match self.append_event(append).await {
                Ok(event) => incident.sequence = Some(event.sequence),
                Err(error) => tracing::error!(
                    event.name = "incident.stream.failed",
                    thread.id = incident.thread_id.as_deref().unwrap_or_default(),
                    "an incident could not reach the stream and was recorded anyway: {error}",
                ),
            }
        }

        self.record_incident(&incident, redactor).await?;

        Ok(incident)
    }

    /// Writes an incident to the database without streaming it.
    ///
    /// The thread's secrets are masked out of the message and the evidence
    /// here, for the same reason they are masked in [`Store::append_event`]:
    /// this is the one door, and an incident row is readable long after the
    /// workspace it describes has been collected.
    ///
    /// Prefer [`Store::report_incident`], which also puts the incident on its
    /// thread's stream. This is for the caller that has already streamed it.
    ///
    /// # Errors
    ///
    /// Returns a database error if the insert fails.
    pub async fn record_incident(
        &self,
        incident: &Incident,
        redactor: &Redactor,
    ) -> Result<(), StoreError> {
        let incident = redactor.redacted_incident(incident);

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

    /// Reads a thread's incidents, oldest first.
    ///
    /// Deliberately readable after the thread is gone. Incidents carry no
    /// foreign key to `threads` and are never removed by collection, because
    /// "why did last night's run go wrong" is asked after the workspace has
    /// been reclaimed. Losing the evidence with the thread would defeat them.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn incidents_for_thread(&self, thread_id: &str) -> Result<Vec<Incident>, StoreError> {
        let rows = sqlx::query(
            "SELECT * FROM incidents WHERE thread_id = ? ORDER BY occurred_at ASC, incident_id ASC",
        )
        .bind(thread_id)
        .fetch_all(self.pool())
        .await?;

        Ok(rows.iter().map(hydrate).collect())
    }
}

/// Rebuilds a contract incident from its row.
fn hydrate(row: &sqlx::sqlite::SqliteRow) -> Incident {
    Incident {
        incident_id: row.get("incident_id"),
        thread_id: row.get("thread_id"),
        sequence: row
            .get::<Option<i64>, _>("sequence")
            .map(|value| u64::try_from(value).unwrap_or_default()),
        turn_id: row.get("turn_id"),
        member_id: row.get("member_id"),
        code: row.get("code"),
        disposition: row.get("disposition"),
        retryable: row.get::<i32, _>("retryable") != 0,
        message: row.get("message"),
        details: row
            .get::<Option<Vec<u8>>, _>("details")
            .and_then(|bytes| prost::Message::decode(bytes.as_slice()).ok()),
        occurred_at: Some(from_nanos(row.get("occurred_at"))),
    }
}
