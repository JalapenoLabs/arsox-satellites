-- Copyright © 2026 Jalapeno Labs
--
-- What a turn decides for itself.
--
-- Encoded arsox.settings.v1.TurnOverrides, null for every turn that named
-- nothing, which is every turn written before this column existed. Stored as the
-- caller submitted it rather than as it resolved: the thread's defaults are read
-- at claim time, so a thread whose defaults change between queueing and running
-- runs on the defaults it holds when the turn starts, exactly like its posture.
ALTER TABLE turns ADD COLUMN overrides BLOB;
