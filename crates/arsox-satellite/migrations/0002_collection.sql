-- Copyright © 2026 Jalapeno Labs
--
-- Thread collection.
--
-- A collected thread becomes a tombstone rather than disappearing. The row
-- stays, carrying EXPIRED or DESTROYED, while its turns, events, and metadata
-- are removed. That is what lets a later request answer "this thread expired"
-- instead of "no such thread", which are different facts a caller acts on
-- differently.

-- The collector asks one question on an interval: which threads are past their
-- expiry. Without this it is a full scan of every thread the satellite has ever
-- held, including the tombstones, which only accumulate.
--
-- Partial, because a tombstone has no expiry and a thread with no idle TTL
-- cannot be collected on a clock. Neither belongs in the index.
CREATE INDEX threads_by_expiry
    ON threads (expires_at)
    WHERE expires_at IS NOT NULL;
