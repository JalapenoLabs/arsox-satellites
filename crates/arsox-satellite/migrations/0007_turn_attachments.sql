-- Copyright © 2026 Jalapeno Labs
--
-- The files a turn was submitted with.
--
-- One row per attachment, in the order the caller named them, carrying what the
-- satellite found when it checked the file at submission: its size and the
-- media type its first bytes announced. The file itself stays in the workspace;
-- the runner reads it again when the harness starts, because the agent owns the
-- workspace in between.
--
-- A child table rather than a blob on `turns`, the way `turn_metadata` is, so a
-- turn reads back as its rows and the cascade removes them with it.
CREATE TABLE turn_attachments (
    turn_id         TEXT    NOT NULL REFERENCES turns (turn_id) ON DELETE CASCADE,

    -- Zero based, the position in StartTurnRequest.attachments.
    position        INTEGER NOT NULL,

    -- Relative to the thread's workspace root, `/`-separated.
    path            TEXT    NOT NULL,

    -- Sniffed from the file's first bytes. Null when nothing was recognized.
    content_type    TEXT,

    size_bytes      INTEGER NOT NULL,

    PRIMARY KEY (turn_id, position)
);
