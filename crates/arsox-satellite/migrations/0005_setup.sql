-- Copyright © 2026 Jalapeno Labs
--
-- The satellite's setup script.
--
-- One host application script per satellite, set with PUT /v1/setup and run as
-- root to install what the image does not ship. At most one row, which the
-- CHECK makes structural rather than a convention every writer has to keep.
-- No row means no script: clearing it is a DELETE, and reading it back as
-- SETUP_STATE_NONE needs no sentinel.
--
-- The script is kept rather than only its hash, because a replaced container
-- has lost everything outside /var/arsox and /workspace, and the satellite
-- runs it again at boot from this copy.
CREATE TABLE setup (
    id              INTEGER PRIMARY KEY CHECK (id = 1),

    script          TEXT    NOT NULL,

    -- Lowercase hex SHA-256 of `script`, which is what a host compares to know
    -- whether the satellite already holds its script.
    script_sha256   TEXT    NOT NULL,

    -- arsox.satellite.v1.SetupState. The claim query refuses work while this is
    -- RUNNING, so the gate holds for whatever does the claiming.
    state           INTEGER NOT NULL,

    -- Null while the script runs, and when it never exited on its own.
    exit_code       INTEGER,

    output_tail     TEXT    NOT NULL DEFAULT '',

    started_at      INTEGER NOT NULL,

    -- Null while the script runs.
    finished_at     INTEGER
);
