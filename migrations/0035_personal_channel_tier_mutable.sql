-- silent-mesh (D24/D26/D29): a personal channel's tier is the member's to
-- change; a team channel's is still immutable.
--
-- 0027 made `channels.tier` immutable for everyone, which is right for a
-- team channel: members post into it on the strength of a declared privacy
-- floor, and re-tiering underneath them would break a promise they already
-- acted on. A personal channel has exactly one member, who is also its
-- owner. There is no one to surprise, and forcing them to re-create the
-- channel (the clone path) to change their own default is friction with no
-- privacy benefit — the alternative is that they simply never change it.
--
-- So the guard is narrowed rather than dropped: a tier change is refused
-- unless the row is a personal channel. Two properties survive that a blunt
-- relaxation would lose:
--
--   * Team channels keep the original, absolute guarantee.
--   * The check is data-driven (membership of `personal_channels`), not a
--     flag the caller passes, so no relay bug can talk the database into
--     re-tiering a team channel.
--
-- Authority — that the actor is the personal channel's *owner* — is enforced
-- above this, in `set_personal_channel_tier`, whose SQL joins the registry on
-- the owner pubkey. This trigger is the backstop, not the gate.
--
-- Direction is deliberately NOT constrained here. A member may loosen their
-- own space as well as tighten it; what content movement out of it must
-- satisfy is enforced at promotion time (a thread may only be promoted into
-- a channel no stricter than the space it came from).

CREATE OR REPLACE FUNCTION channels_tier_immutable() RETURNS trigger AS $$
BEGIN
    IF NEW.tier IS DISTINCT FROM OLD.tier
       AND NOT EXISTS (
           SELECT 1 FROM personal_channels p
           WHERE p.channel_id = NEW.id
             AND p.community_id = NEW.community_id
       ) THEN
        RAISE EXCEPTION 'channels.tier is immutable (declared at creation)';
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
