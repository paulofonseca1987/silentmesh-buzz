-- Phase 2g (personal channels, D29): the personal-channel registry.
--
-- A personal channel is a member's private space (self + their agents).
-- Rather than widening the channels table, the marker lives in this
-- additive side table: the PK enforces **one personal channel per member
-- per community**, and the UNIQUE on the channel side makes the reverse
-- lookup (is this channel personal? whose?) cheap. Rows are created in the
-- same transaction as the channel itself (create_personal_channel), so a
-- personal channel can never exist half-marked.
--
-- Tenant-scoped, community-prefixed keys; cascade follows the channel.

CREATE TABLE personal_channels (
    community_id UUID NOT NULL REFERENCES communities(id),
    owner_pubkey BYTEA NOT NULL CHECK (length(owner_pubkey) = 32),
    channel_id   UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, owner_pubkey),
    UNIQUE (community_id, channel_id),
    FOREIGN KEY (community_id, channel_id)
        REFERENCES channels (community_id, id) ON DELETE CASCADE
);
