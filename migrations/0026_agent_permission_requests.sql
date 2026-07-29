-- Agent permission requests: pending human approvals for ACP tool calls,
-- created when a supervised agent harness asks permission to run a tool.
--
-- Mirrors workflow_approvals' proven idioms — community-prefixed keys, hashed
-- token (the kind:46010 / 46030 / 46031 `d`-tag handle), TOCTOU-safe
-- `AND status = 'pending'` updates, expiry — but is its own table:
-- workflow_approvals is workflow-keyed (composite FKs into workflows and
-- workflow_runs) and agent requests have no workflow to reference.
--
-- `token` stores SHA-256(raw token) only; the raw token is minted server-side
-- at creation and carried by the kind:46010 notification, never persisted.
-- `payload` carries the full tool input for approver inspection; the 46010
-- event content carries only the pre-rendered summary (`detail`, ≤400 chars).
--
-- Every lookup binds (community_id, token) — a grant presented on the wrong
-- tenant host resolves nothing (same fence as workflow_approvals and
-- relay_invites).

CREATE TYPE agent_permission_status AS ENUM
    ('pending', 'granted', 'denied', 'cancelled', 'expired');

CREATE TABLE agent_permission_requests (
    community_id    UUID        NOT NULL REFERENCES communities(id),
    request_id      UUID        NOT NULL DEFAULT gen_random_uuid(),
    token           BYTEA       NOT NULL CHECK (length(token) = 32),
    channel_id      UUID        NOT NULL,
    agent_pubkey    BYTEA       NOT NULL CHECK (length(agent_pubkey) = 32),
    session_ref     TEXT,
    request_kind    TEXT        NOT NULL
        CHECK (request_kind IN ('command', 'file-read', 'file-change', 'other')),
    tool_name       TEXT,
    detail          TEXT        NOT NULL CHECK (char_length(detail) <= 400),
    payload         JSONB,
    options_offered JSONB       NOT NULL,
    status          agent_permission_status NOT NULL DEFAULT 'pending',
    decision        TEXT
        CHECK (decision IN ('allow_once', 'allow_always', 'reject_once', 'cancel')),
    decider_pubkey  BYTEA       CHECK (decider_pubkey IS NULL OR length(decider_pubkey) = 32),
    note            TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at      TIMESTAMPTZ NOT NULL,
    resolved_at     TIMESTAMPTZ,
    PRIMARY KEY (community_id, request_id),
    UNIQUE (community_id, token),
    FOREIGN KEY (community_id, channel_id)
        REFERENCES channels (community_id, id) ON DELETE CASCADE
);

-- Channel-scoped pending list (the approvals read surface is
-- membership-scoped, so queries always carry channel ids).
CREATE INDEX idx_agent_permission_requests_channel
    ON agent_permission_requests (community_id, channel_id, status);
-- Community-wide status list, newest first.
CREATE INDEX idx_agent_permission_requests_status
    ON agent_permission_requests (community_id, status, created_at DESC);
-- Expiry sweep over pending rows only.
CREATE INDEX idx_agent_permission_requests_expiry
    ON agent_permission_requests (expires_at) WHERE status = 'pending';
