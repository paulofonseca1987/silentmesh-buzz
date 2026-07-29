-- Phase 2f (work threads): fork provenance (D27/D28).
--
-- forked_from records the parent thread's root event id when a thread was
-- opened by a kind:47020 fork event (NULL for ordinary kind:47000 roots).
-- fork_commit records the fork point — a kind:47010 checkpoint commit of
-- the parent validated at ingest — or NULL for a fork at head. Both are
-- projection copies of the signed fork event's tags; the event is the
-- truth.
--
-- The partial index serves the sibling-fork family walk in the winner's
-- close-with-`archive-siblings` flow (batch archiving of losing forks).

ALTER TABLE work_threads ADD COLUMN forked_from BYTEA
    CHECK (forked_from IS NULL OR length(forked_from) = 32);
ALTER TABLE work_threads ADD COLUMN fork_commit TEXT
    CHECK (fork_commit IS NULL OR length(fork_commit) IN (40, 64));

CREATE INDEX idx_work_threads_forked_from
    ON work_threads (community_id, forked_from)
    WHERE forked_from IS NOT NULL;
