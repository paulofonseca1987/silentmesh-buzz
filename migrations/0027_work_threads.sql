-- Phase 2 (work threads): channel tiers, channel-repo bindings, and the
-- work-thread projection.
--
-- Additive-only: 0001's checksum is frozen (sqlx aborts brownfield startup
-- on any change to an applied file). Every new table is tenant-scoped with
-- community-prefixed keys; none is operator-global.
--
-- channels.tier is the immutable channel privacy tier (owned | private |
-- open): declared at creation, never updated. Enforced in depth — no code
-- path updates it, and a BEFORE UPDATE trigger raises on any change
-- (mirror of the community_id immutability guard from 0001). Existing rows
-- backfill to 'open', the loosest tier, which preserves their current
-- behavior.
--
-- channel_repos records the forge repository bound to a channel at
-- creation (channel = folder = repo). The repo is relay-owned: the binding
-- stores the announcement author so provisioning is auditable. repo_name
-- is unique per community, matching git_repo_names semantics.
--
-- work_threads is the relay-side projection of the 47xxx work-thread
-- events: the signed events are the truth; the row makes reads cheap and
-- makes state transitions TOCTOU-safe (UPDATE ... WHERE status = expected).
-- thread_id is the 32-byte root event id.
--
-- Legacy guest channel members are retired to 'member' — this fork
-- disables the Guest role. The enum label itself remains: Postgres cannot
-- drop an enum value in place; the relay rejects new guest grants.

CREATE TYPE channel_tier AS ENUM ('owned', 'private', 'open');

ALTER TABLE channels ADD COLUMN tier channel_tier NOT NULL DEFAULT 'open';

CREATE FUNCTION channels_tier_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.tier IS DISTINCT FROM OLD.tier THEN
        RAISE EXCEPTION 'channels.tier is immutable (declared at creation)';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_channels_tier_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW EXECUTE FUNCTION channels_tier_immutable();

CREATE TABLE channel_repos (
    community_id UUID NOT NULL REFERENCES communities(id),
    channel_id   UUID NOT NULL,
    repo_name    TEXT NOT NULL CHECK (char_length(repo_name) BETWEEN 1 AND 64),
    owner_pubkey TEXT NOT NULL CHECK (length(owner_pubkey) = 64),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, channel_id),
    UNIQUE (community_id, repo_name),
    FOREIGN KEY (community_id, channel_id)
        REFERENCES channels (community_id, id) ON DELETE CASCADE
);

CREATE TYPE work_thread_status AS ENUM
    ('open', 'snoozed', 'ready', 'closed', 'archived');

CREATE TABLE work_threads (
    community_id UUID NOT NULL REFERENCES communities(id),
    thread_id    BYTEA NOT NULL CHECK (length(thread_id) = 32),
    channel_id   UUID NOT NULL,
    goal         TEXT NOT NULL,
    deadline     TIMESTAMPTZ,
    dri_pubkey   BYTEA CHECK (dri_pubkey IS NULL OR length(dri_pubkey) = 32),
    status       work_thread_status NOT NULL DEFAULT 'open',
    canonicalize_on_close BOOLEAN NOT NULL DEFAULT FALSE,
    created_by   BYTEA NOT NULL CHECK (length(created_by) = 32),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at    TIMESTAMPTZ,
    PRIMARY KEY (community_id, thread_id),
    FOREIGN KEY (community_id, channel_id)
        REFERENCES channels (community_id, id) ON DELETE CASCADE
);

-- Channel-scoped thread lists (the primary read surface).
CREATE INDEX idx_work_threads_channel
    ON work_threads (community_id, channel_id, status);
-- Community-wide status list, newest first.
CREATE INDEX idx_work_threads_status
    ON work_threads (community_id, status, created_at DESC);
-- Overdue sweep over live threads with deadlines only.
CREATE INDEX idx_work_threads_overdue
    ON work_threads (deadline)
    WHERE status IN ('open', 'snoozed', 'ready') AND deadline IS NOT NULL;

-- Retire legacy guest memberships (fork policy: Guest is disabled).
UPDATE channel_members SET role = 'member' WHERE role = 'guest';
