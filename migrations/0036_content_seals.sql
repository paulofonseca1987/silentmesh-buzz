-- Content Seals (Silent Mesh D31, slice 2): the owner's registry.
--
-- One row per seal: an exact literal that may only appear in channels at or
-- above `min_tier`. THE LITERAL LIVES HERE AND ONLY HERE — never in an
-- event. Events fan out to every member of every tier, so a literal in any
-- event would republish the value the seal exists to contain; the
-- kind:47100 announcement carries the label and tier only, and matching
-- text against the value itself is something only the relay can do
-- (D31: "server-side-only literal matching").
--
-- Additive-only, tenant-scoped, community-prefixed key, reusing the
-- `channel_tier` enum from 0027. The id is the 16-lowercase-hex reference
-- that appears inside `[sm-seal:<id>]` tokens; its shape is CHECKed so a
-- malformed id cannot enter the registry and mint unfindable tokens.
-- Writes are owner-only, enforced at the API layer against the workspace
-- role in `relay_members` (D42: seals are exclusively the Owner's).

CREATE TABLE content_seals (
    community_id UUID NOT NULL REFERENCES communities(id),
    id           TEXT NOT NULL CHECK (id ~ '^[0-9a-f]{16}$'),
    label        TEXT NOT NULL CHECK (char_length(label) BETWEEN 1 AND 120),
    literal      TEXT NOT NULL CHECK (char_length(literal) BETWEEN 1 AND 512),
    min_tier     channel_tier NOT NULL,
    created_by   BYTEA NOT NULL CHECK (length(created_by) = 32),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (community_id, id)
);
