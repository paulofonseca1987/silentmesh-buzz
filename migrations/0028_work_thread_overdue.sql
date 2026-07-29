-- Phase 2d (work threads): overdue-notice bookkeeping.
--
-- overdue_notified_at records when the deadline sweep emitted the
-- relay-signed kind:47011 notice for a thread, so a notice fires at most
-- once per deadline. The claim is TOCTOU-safe
-- (UPDATE ... WHERE overdue_notified_at IS NULL), and editing the deadline
-- via kind:47001 clears it so an extended deadline can go overdue again.

ALTER TABLE work_threads ADD COLUMN overdue_notified_at TIMESTAMPTZ;
