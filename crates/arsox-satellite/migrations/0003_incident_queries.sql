-- Copyright © 2026 Jalapeno Labs
--
-- Querying incidents.
--
-- The initial schema indexed the two questions provisioning and the runner
-- asked: this thread's incidents, and this disposition's. Serving GET
-- /v1/incidents adds two more, and both are on the hot path of a listing rather
-- than of a write.

-- Every turn report counts its own incidents by disposition, which is a query
-- per finished turn. Without this it is a full scan of a table that only grows,
-- since incidents are never collected with their thread.
CREATE INDEX incidents_by_turn ON incidents (turn_id);

-- The satellite-wide listing orders by when an incident happened, with the id
-- breaking ties so that paging is total. The index carries both for the same
-- reason the cursor does: the pair is what makes a page boundary unambiguous.
CREATE INDEX incidents_by_time ON incidents (occurred_at, incident_id);
