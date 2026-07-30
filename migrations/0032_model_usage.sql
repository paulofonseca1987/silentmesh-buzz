-- Phase 3 (model plane): per-request model-usage attribution (D16).
--
-- Every model request the gateway routes records one row here:
-- (user, agent, channel, thread, model, tier, backend) plus token counts.
-- This is the metering substrate the owner's usage queries and (later)
-- budgets read — "usage totals by tier and backend" is a straight
-- aggregate over it.
--
-- Additive-only, tenant-scoped, community-prefixed keys. Reuses the
-- existing `channel_tier` enum (from 0027) for the tier the request ran
-- under. backend is a free-ish text with a CHECK to the known set so a
-- typo can't silently create a phantom backend in the rollups. agent and
-- channel/thread are nullable: a request may be a human's copilot call
-- (no agent) or not tied to a specific thread.

CREATE TABLE model_usage (
    community_id      UUID NOT NULL REFERENCES communities(id),
    id                BIGSERIAL,
    user_pubkey       BYTEA NOT NULL CHECK (length(user_pubkey) = 32),
    agent_pubkey      BYTEA CHECK (agent_pubkey IS NULL OR length(agent_pubkey) = 32),
    channel_id        UUID,
    thread_id         BYTEA CHECK (thread_id IS NULL OR length(thread_id) = 32),
    model             TEXT NOT NULL CHECK (char_length(model) BETWEEN 1 AND 128),
    tier              channel_tier NOT NULL,
    backend           TEXT NOT NULL CHECK (backend IN ('local', 'tee', 'vendor')),
    purpose           TEXT NOT NULL CHECK (purpose IN ('agent_turn', 'copilot', 'gate', 'embedding')),
    prompt_tokens     BIGINT NOT NULL DEFAULT 0 CHECK (prompt_tokens >= 0),
    completion_tokens BIGINT NOT NULL DEFAULT 0 CHECK (completion_tokens >= 0),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, id)
);

-- The owner's headline read: per-user totals by tier and backend, over a
-- time window.
CREATE INDEX idx_model_usage_rollup
    ON model_usage (community_id, user_pubkey, tier, backend, created_at DESC);
