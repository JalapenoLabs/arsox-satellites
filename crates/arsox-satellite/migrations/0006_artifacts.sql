-- Copyright © 2026 Jalapeno Labs
--
-- What each thread's artifacts/ directory held when a turn last ended.
--
-- The artifact scan announces a file as `artifact.created` when it is new or its
-- contents changed, so it has to remember what it saw last time, and remember it
-- across a restart: a satellite that forgot would announce every artifact of
-- every thread again on its first turn back. One row per file the last scan
-- found, replaced whole by the next scan.
--
-- The rows double as a hash cache. A file whose inode, size, and change time
-- all match its row still has the contents it was hashed with, because the
-- kernel moves the change time on every write and an agent cannot move it
-- back. A listing reuses the stored SHA-256 for such a file rather than reading
-- the whole file again.
--
-- Not foreign-keyed to `threads`: collection keeps the thread row as a
-- tombstone, so a cascade would never fire. Collection deletes these rows
-- itself, with the thread's turns and events.
CREATE TABLE artifacts (
    thread_id       TEXT    NOT NULL,

    -- Relative to the thread's artifacts/ directory, `/`-separated.
    path            TEXT    NOT NULL,

    size_bytes      INTEGER NOT NULL,

    -- The identity the hash is valid for. See above.
    inode           INTEGER NOT NULL,
    changed_at      INTEGER NOT NULL,

    -- Lowercase hex SHA-256 of the contents.
    sha256          TEXT    NOT NULL,

    -- Best effort, from the bytes and then the name. Null when neither said.
    content_type    TEXT,

    PRIMARY KEY (thread_id, path)
);
