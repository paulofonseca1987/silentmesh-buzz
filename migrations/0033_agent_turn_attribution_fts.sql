-- ── Exclude kind 44201 (Agent Turn Attribution) from full-text search ─────────
-- Silent Mesh Phase 3: kind 44201 is the CLEARTEXT metering sibling of the
-- encrypted 44200 turn metric. It is p-gated (owner-only reads), so its
-- content must be storage-level unsearchable exactly like the other p-gated
-- persistent kinds — a NIP-50 search must never leak model names, token
-- counts, or user attribution to non-owners.
--
-- Pattern follows 0014_push_lease_fts.sql: PostgreSQL cannot alter a
-- generated expression in place, and installations diverge deliberately —
-- fresh databases carry 0008's positive allowlist (which already excludes
-- 44201 by omission), while brownfield databases retain their denylist
-- expression. Capture whatever expression is live and WRAP it with the
-- 44201 exclusion, preserving both policies for every other kind.
DO $$
DECLARE
    existing_expression TEXT;
BEGIN
    SELECT pg_get_expr(d.adbin, d.adrelid)
      INTO existing_expression
      FROM pg_attrdef d
      JOIN pg_attribute a
        ON a.attrelid = d.adrelid
       AND a.attnum = d.adnum
     WHERE d.adrelid = 'events'::regclass
       AND a.attname = 'search_tsv';

    IF existing_expression IS NULL THEN
        RAISE EXCEPTION 'events.search_tsv generated expression not found';
    END IF;

    ALTER TABLE events DROP COLUMN search_tsv;
    EXECUTE format(
        'ALTER TABLE events ADD COLUMN search_tsv TSVECTOR GENERATED ALWAYS AS (CASE WHEN kind = 44201 THEN NULL::tsvector ELSE (%s) END) STORED',
        existing_expression
    );
    CREATE INDEX idx_events_search_tsv ON events USING GIN (search_tsv);
END $$;
