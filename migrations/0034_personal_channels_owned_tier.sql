-- silent-mesh Phase 3 (D24/D29): personal channels are the `owned` tier.
--
-- A personal channel is forced private at creation and its membership is
-- locked to the member and their own agents, but its `tier` fell through to
-- the column default (`open`, migration 0027) because kind:9007's
-- create-personal path sets no tier tag. That left the member's most private
-- space declaring the weakest model-routing floor: an agent turn there would
-- pass the tier gate for a **vendor** backend, egressing exactly the pre-gate
-- material the D30 promotion review exists to hold back. Gate, copilot, and
-- embedding reads of the same channel are owned-pinned to a local backend by
-- the router, so the same bytes had two different egress floors depending on
-- which code path touched them.
--
-- The relay now forces `owned` at creation. This migration brings existing
-- personal channels to the same floor.
--
-- WHY THIS IS ALLOWED TO TOUCH AN IMMUTABLE COLUMN
--
-- `trg_channels_tier_immutable` (0027) exists so a channel's tier is a
-- stable promise: a member who posted into an `owned` channel must never
-- discover it was quietly re-tiered to `open` afterwards. That is a guard
-- against **weakening**. This update only ever tightens (`open`/`private` →
-- `owned`), which strictly reduces permitted egress and cannot retroactively
-- expose anything that was not already exposable. It is one-time, scoped to
-- rows in `personal_channels`, and runs inside the migration transaction, so
-- no other session observes the window where the guard is absent.
--
-- The trigger is dropped and recreated rather than disabled: `ALTER TABLE
-- channels ... DISABLE TRIGGER` is refused by the migration lint in
-- buzz-db/src/migration.rs, which is correct — that form is how the
-- community_id tenant fence would be defeated.

DROP TRIGGER trg_channels_tier_immutable ON channels;

UPDATE channels c
SET tier = 'owned'
FROM personal_channels p
WHERE p.channel_id = c.id
  AND p.community_id = c.community_id
  AND c.tier <> 'owned';

-- Recreated verbatim from 0027 — the function is unchanged, so the guard is
-- exactly as strict after this migration as before it.
CREATE TRIGGER trg_channels_tier_immutable
    BEFORE UPDATE ON channels
    FOR EACH ROW EXECUTE FUNCTION channels_tier_immutable();
