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

use super::{
    DEFAULT_PAGE, MAX_PAGE, Store, StoreError, build_cursor, from_nanos, split_cursor, to_nanos,
};
use crate::redaction::Redactor;
use arsox_sdk::proto::common::v1::Timestamp;
use arsox_sdk::proto::event::v1::thread_event::Payload;
use arsox_sdk::proto::incident::v1::{Disposition, Incident, IncidentCounts};
use sqlx::Row as _;
use std::fmt::Write as _;

/// Which incidents a listing should return.
///
/// Every list filter is "match any of these", and an empty one does not filter
/// rather than matching nothing. That is what the contract says an empty
/// repeated field means, and the alternative would make the default request
/// return nothing at all.
#[derive(Debug, Clone, Default)]
pub struct IncidentFilter {
    pub thread_ids: Vec<String>,
    pub turn_ids: Vec<String>,
    pub member_ids: Vec<String>,

    /// `arsox.error.v1.ErrorCode` values, as stored.
    pub codes: Vec<i32>,

    /// `arsox.incident.v1.Disposition` values, as stored.
    pub dispositions: Vec<i32>,

    /// Half-open range in nanoseconds since the epoch: `occurred_at` must be at
    /// or after `occurred_after`, and strictly before `occurred_before`.
    ///
    /// Half-open rather than inclusive at both ends so that adjacent windows
    /// tile: an operator walking an incident log an hour at a time sees every
    /// incident exactly once, instead of seeing the ones on a boundary twice.
    pub occurred_after: Option<i64>,
    pub occurred_before: Option<i64>,

    /// Resume after this opaque cursor, which encodes the sort key and the
    /// incident id together.
    ///
    /// Both are needed. Incidents sort by when they happened, and a timestamp is
    /// not unique, so a cursor carrying only the time would repeat or skip every
    /// incident sharing a nanosecond with the one at the page boundary.
    pub after: Option<String>,

    pub limit: u32,
}

/// One page of an incident listing.
#[derive(Debug, Clone)]
pub struct IncidentListing {
    pub incidents: Vec<Incident>,

    /// Pass back to continue. Empty when the page was the last one.
    pub next_cursor: String,
}

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

    /// Lists incidents, oldest first, filtered and paged.
    ///
    /// Nothing here asks whether a thread is alive. An incident from a collected
    /// thread is exactly the incident an operator came looking for, so filtering
    /// on a tombstoned thread returns its evidence rather than `THREAD_EXPIRED`.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn list_incidents(
        &self,
        filter: &IncidentFilter,
    ) -> Result<IncidentListing, StoreError> {
        let limit = match filter.limit {
            0 => DEFAULT_PAGE,
            asked => asked.min(MAX_PAGE),
        };

        // Built rather than written out because every filter is optional and
        // SQLite has no array binding. Every value is still bound, never
        // interpolated: the fragments written here are fixed strings and the
        // only thing that varies is how many placeholders they carry.
        let mut sql = String::from("SELECT * FROM incidents WHERE 1 = 1");
        for (column, values) in [
            ("thread_id", filter.thread_ids.len()),
            ("turn_id", filter.turn_ids.len()),
            ("member_id", filter.member_ids.len()),
            ("code", filter.codes.len()),
            ("disposition", filter.dispositions.len()),
        ] {
            if values > 0 {
                let slots = vec!["?"; values].join(", ");
                write!(sql, " AND {column} IN ({slots})")
                    .expect("writing to a String is infallible");
            }
        }
        if filter.occurred_after.is_some() {
            sql.push_str(" AND occurred_at >= ?");
        }
        if filter.occurred_before.is_some() {
            sql.push_str(" AND occurred_at < ?");
        }
        if filter.after.is_some() {
            sql.push_str(" AND (occurred_at, incident_id) > (?, ?)");
        }
        sql.push_str(" ORDER BY occurred_at ASC, incident_id ASC LIMIT ?");

        let mut query = sqlx::query(&sql);
        for thread_id in &filter.thread_ids {
            query = query.bind(thread_id);
        }
        for turn_id in &filter.turn_ids {
            query = query.bind(turn_id);
        }
        for member_id in &filter.member_ids {
            query = query.bind(member_id);
        }
        for code in &filter.codes {
            query = query.bind(code);
        }
        for disposition in &filter.dispositions {
            query = query.bind(disposition);
        }
        if let Some(after) = filter.occurred_after {
            query = query.bind(after);
        }
        if let Some(before) = filter.occurred_before {
            query = query.bind(before);
        }
        if let Some(cursor) = filter.after.as_deref() {
            // A cursor that did not come from this satellite is treated as a
            // sort key with no id rather than refused: it still pages forward
            // from somewhere sensible, and an opaque token is not the caller's
            // to get right.
            let (sort_key, incident_id) = split_cursor(cursor).unwrap_or((cursor, ""));
            query = query
                .bind(sort_key.parse::<i64>().unwrap_or_default())
                .bind(incident_id);
        }
        query = query.bind(limit);

        let rows = query.fetch_all(self.pool()).await?;

        let next_cursor = rows.last().map_or_else(String::new, |row| {
            build_cursor(
                &row.get::<i64, _>("occurred_at").to_string(),
                &row.get::<String, _>("incident_id"),
            )
        });

        Ok(IncidentListing {
            incidents: rows.iter().map(hydrate).collect(),
            next_cursor,
        })
    }

    /// Counts a turn's incidents by disposition.
    ///
    /// Rides along on every turn report so the common case needs no query at
    /// all: a consumer reacting to `turn.completed` learns that four things went
    /// wrong without asking a second question, and asks only when it wants to
    /// know what they were.
    ///
    /// # Errors
    ///
    /// Returns a database error if the query fails.
    pub async fn incident_counts(&self, turn_id: &str) -> Result<IncidentCounts, StoreError> {
        let rows = sqlx::query(
            "SELECT disposition, count(*) AS total FROM incidents
              WHERE turn_id = ?
             GROUP BY disposition",
        )
        .bind(turn_id)
        .fetch_all(self.pool())
        .await?;

        let mut counts = IncidentCounts::default();

        for row in &rows {
            let total = u32::try_from(row.get::<i64, _>("total")).unwrap_or(u32::MAX);

            match Disposition::try_from(row.get::<i32, _>("disposition")) {
                Ok(Disposition::Fatal) => counts.fatal = total,
                Ok(Disposition::Recovered) => counts.recovered = total,
                Ok(Disposition::Degraded) => counts.degraded = total,
                Ok(Disposition::Blocked) => counts.blocked = total,
                // A disposition this build does not know about was written by a
                // different major version. It is a real incident and it is in
                // the listing; there is simply no field here to count it in.
                Ok(Disposition::Unspecified) | Err(_) => {}
            }
        }

        Ok(counts)
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
