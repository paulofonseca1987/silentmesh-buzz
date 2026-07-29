-- Phase 2e (work threads): canonicalization bookkeeping.
--
-- canonicalized_at is the once-only claim for the canon/ merge job
-- (TOCTOU-safe UPDATE ... WHERE canonicalized_at IS NULL); a new
-- close-with-canonicalize transition re-arms it so a reopened thread can
-- canonicalize again. canonicalize_outcome records what happened
-- ('merged', 'unchanged', 'no_repo', 'no_checkpoint', 'commit_missing',
-- 'conflict', 'error:...') for operators and the kind:47012 notice.

ALTER TABLE work_threads ADD COLUMN canonicalized_at TIMESTAMPTZ;
ALTER TABLE work_threads ADD COLUMN canonicalize_outcome TEXT;
