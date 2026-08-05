-- Copyright © 2026 Jalapeno Labs

-- The satellite's embedded database.
--
-- Three decisions shape this schema, and each one is deliberate.
--
-- **Contract messages are stored encoded, not re-modelled.** `settings` and
-- `result` are protobuf bytes rather than a column per field. The contract has
-- 147 messages; mirroring it in DDL would mean a migration for every additive
-- proto change, and the two models would drift the first time somebody forgot.
-- What gets its own column is what gets queried or ordered by.
--
-- **Anything filterable gets a real table.** Thread metadata is queried by
-- `ListThreads`, so it is rows rather than a blob. That is the whole test for
-- whether something earns columns.
--
-- **Timestamps are nanoseconds since the Unix epoch, as one signed integer.**
-- The contract carries seconds plus nanos plus a display zone; the zone is
-- presentational and every instant recorded here is UTC, so it is reconstructed
-- rather than stored. A signed 64-bit nanosecond count spans 1678 to 2262,
-- which outlives anything this row will ever describe.

CREATE TABLE threads (
    thread_id           TEXT    PRIMARY KEY,
    state               INTEGER NOT NULL,

    -- Encoded arsox.settings.v1.ThreadSettings, as resolved at creation with
    -- defaults filled in.
    settings            BLOB    NOT NULL,

    created_at          INTEGER NOT NULL,
    last_activity_at    INTEGER NOT NULL,

    -- Null while a turn or a watch window holds the idle clock.
    expires_at          INTEGER,

    -- Highest sequence handed out on this thread's stream. Incremented under
    -- the same transaction that appends the event, which is what makes the
    -- sequence gapless and monotonic.
    latest_sequence     INTEGER NOT NULL DEFAULT 0,

    -- The harness's own session identifier, null until the first turn spawns
    -- one.
    harness_session_id  TEXT,

    -- Null when the caller did not send one. Unique when present, which is the
    -- whole mechanism: a retried create collides here and returns the original
    -- thread instead of provisioning a second workspace.
    idempotency_key     TEXT    UNIQUE
);

CREATE INDEX threads_by_state ON threads (state);

-- Rows rather than a blob, because `ListThreads` filters on them. "Every thread
-- still running for this tenant" is the query this table exists to answer.
CREATE TABLE thread_metadata (
    thread_id   TEXT NOT NULL REFERENCES threads (thread_id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,

    PRIMARY KEY (thread_id, key)
);

CREATE INDEX thread_metadata_by_pair ON thread_metadata (key, value);

CREATE TABLE turns (
    turn_id                 TEXT    PRIMARY KEY,
    thread_id               TEXT    NOT NULL REFERENCES threads (thread_id) ON DELETE CASCADE,
    status                  INTEGER NOT NULL,
    prompt                  TEXT    NOT NULL,

    -- True when the satellite started this turn itself, which today means a
    -- pull request watch reacting to a failing check run.
    satellite_initiated     INTEGER NOT NULL DEFAULT 0,
    triggered_by_turn_id    TEXT,

    queued_at               INTEGER NOT NULL,
    started_at              INTEGER,
    finished_at             INTEGER,

    -- Encoded arsox.turn.v1.TurnResult, null until the turn is terminal.
    result                  BLOB,

    idempotency_key         TEXT
);

-- The queue is a query, not a structure. Turns run in the order they were
-- queued, so the next one to run is the oldest QUEUED row for the thread, and
-- there is no separate queue table to fall out of step with this one.
CREATE INDEX turns_by_thread_queue ON turns (thread_id, status, queued_at);

-- Scoped to the thread rather than global: two threads may each retry a submit
-- with the caller's own request id without colliding.
CREATE UNIQUE INDEX turns_idempotency ON turns (thread_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE turn_metadata (
    turn_id     TEXT NOT NULL REFERENCES turns (turn_id) ON DELETE CASCADE,
    key         TEXT NOT NULL,
    value       TEXT NOT NULL,

    PRIMARY KEY (turn_id, key)
);

-- The event log. Persisted before an event is sent, so a consumer that
-- reconnects with `from_sequence` replays exactly what it missed.
CREATE TABLE events (
    thread_id   TEXT    NOT NULL REFERENCES threads (thread_id) ON DELETE CASCADE,
    sequence    INTEGER NOT NULL,

    turn_id     TEXT,

    -- Hoisted out of the payload so a consumer can filter by member without
    -- decoding every frame.
    member_id   TEXT,

    -- The stable wire name, e.g. "agent.message". Stored alongside the payload
    -- for the same reason the contract carries it: a reader that cannot decode
    -- a future payload arm can still name and forward the event.
    type        TEXT    NOT NULL,

    occurred_at INTEGER NOT NULL,

    -- Encoded arsox.event.v1.ThreadEvent.
    payload     BLOB    NOT NULL,

    PRIMARY KEY (thread_id, sequence)
);

CREATE INDEX events_by_turn ON events (turn_id);

-- Incidents outlive their thread on purpose, which is why this table does not
-- cascade from `threads`. "Why did last night's run go wrong" is asked after
-- the workspace is gone, and losing the evidence with it would defeat the
-- feature.
--
-- `thread_id` is nullable because a failure can belong to the satellite rather
-- than to any thread: an unreachable LLM proxy at boot, a failed migration, a
-- volume that will not mount.
CREATE TABLE incidents (
    incident_id     TEXT    PRIMARY KEY,
    thread_id       TEXT,
    sequence        INTEGER,
    turn_id         TEXT,
    member_id       TEXT,

    code            INTEGER NOT NULL,
    disposition     INTEGER NOT NULL,
    retryable       INTEGER NOT NULL DEFAULT 0,
    message         TEXT    NOT NULL,

    -- Encoded google.protobuf.Struct, null when the incident carried no
    -- structured detail.
    details         BLOB,

    occurred_at     INTEGER NOT NULL
);

CREATE INDEX incidents_by_thread ON incidents (thread_id, occurred_at);
CREATE INDEX incidents_by_disposition ON incidents (disposition, occurred_at);
