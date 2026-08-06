//! The Content Seal registry — the `content_seals` table (Silent Mesh D31).
//!
//! One row per seal: an exact literal permitted only in channels at or
//! above `min_tier`. **The literal lives in this table and nowhere else** —
//! never in an event, because events fan out to every member of every tier
//! (D31: "server-side-only literal matching"). The kind:47100 announcement
//! that clients see carries the label and tier only.
//!
//! Writes are owner-only; that authority check belongs to the API layer
//! (against the `relay_members` workspace role, D42), not here — the store
//! records what it is given, the endpoint decides who may give it.

use sqlx::{PgPool, Row};

use buzz_core::channel::ChannelTier;
use buzz_core::seal::SealedLiteral;

use crate::error::Result;
use crate::CommunityId;

/// Parameters for creating one seal.
pub struct CreateSealParams<'a> {
    /// Community that owns the seal.
    pub community_id: CommunityId,
    /// 16-lowercase-hex seal id — the reference `[sm-seal:<id>]` tokens use.
    pub id: &'a str,
    /// Human-readable label. This is what refusals name, so it must be
    /// writable in public: "the Zurich client", not the value itself.
    pub label: &'a str,
    /// The sealed value. Stored here, matched server-side, never published.
    pub literal: &'a str,
    /// The loosest tier the literal may appear in.
    pub min_tier: ChannelTier,
    /// The workspace Owner who created it (32-byte pubkey).
    pub created_by: &'a [u8],
}

/// Insert one seal. Fails on a duplicate id (the community-prefixed PK).
pub async fn create_seal(pool: &PgPool, params: CreateSealParams<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO content_seals \
         (community_id, id, label, literal, min_tier, created_by) \
         VALUES ($1, $2, $3, $4, $5::channel_tier, $6)",
    )
    .bind(params.community_id.as_uuid())
    .bind(params.id)
    .bind(params.label)
    .bind(params.literal)
    .bind(params.min_tier.as_str())
    .bind(params.created_by)
    .execute(pool)
    .await?;
    Ok(())
}

/// One channel where a sealed literal already exists.
///
/// Carries counts and identifiers only — never the matched text, and never
/// the literal. A sweep result is shown to the Owner, but it also lands in
/// logs and CLI output, so it obeys the same rule as every other seal
/// finding.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepChannel {
    /// The channel, or `None` for matches on events with no channel scope.
    pub channel_id: Option<uuid::Uuid>,
    /// Channel name, when there is a channel.
    pub name: Option<String>,
    /// The channel's privacy tier, when there is a channel.
    pub tier: Option<String>,
    /// How many stored events in this channel contain the literal.
    pub count: i64,
    /// True when this channel is looser than the seal's minimum — these are
    /// the occurrences the seal would have refused had it existed first,
    /// and the only ones that represent an actual exposure.
    pub exposed: bool,
}

/// What a sweep found. Deliberately an aggregate: the Owner needs to know
/// *where* and *how much*, and a list of event ids is neither actionable
/// nor safe to page through.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SweepReport {
    /// Per-channel breakdown, most occurrences first.
    pub channels: Vec<SweepChannel>,
    /// Total matching events across all channels.
    pub occurrences: i64,
    /// Matching events sitting in channels looser than the seal's minimum.
    pub exposed: i64,
    /// The scan hit `limit` — there may be more than reported.
    pub truncated: bool,
}

/// Find where `literal` already appears in stored events (Silent Mesh D31,
/// the workspace sweep).
///
/// A seal created today says nothing about content stored yesterday, and
/// without this the Owner has no way to learn that sealing a value left it
/// sitting in an open channel. This is the detection half; rewriting that
/// history is a separate decision (the D-log's open question 10, tied to
/// deep seal).
///
/// Scans `content` **and** `tags`, matching the ingestion guard — a literal
/// smuggled in a tag is as exposed as one in the body. The tag scan casts
/// JSONB to text, so a literal containing a quote or backslash would be
/// JSON-escaped in storage and could be missed there; content matching is
/// exact either way.
///
/// Bounded by `limit`: a substring scan cannot use an index, and a sweep
/// that hangs on a large workspace is worse than one that reports honestly
/// that it stopped early.
pub async fn sweep_literal(
    pool: &PgPool,
    community: CommunityId,
    literal: &str,
    min_tier: ChannelTier,
    limit: i64,
) -> Result<SweepReport> {
    // limit + 1 so a full page is distinguishable from an exact fit.
    let rows = sqlx::query(
        "SELECT e.channel_id, c.name AS channel_name, c.tier::text AS channel_tier \
         FROM events e \
         LEFT JOIN channels c ON c.id = e.channel_id AND c.community_id = e.community_id \
         WHERE e.community_id = $1 \
           AND e.deleted_at IS NULL \
           AND (strpos(e.content, $2) > 0 OR strpos(e.tags::text, $2) > 0) \
         LIMIT $3",
    )
    .bind(community.as_uuid())
    .bind(literal)
    .bind(limit.saturating_add(1))
    .fetch_all(pool)
    .await?;

    let truncated = rows.len() as i64 > limit;
    let mut by_channel: std::collections::HashMap<Option<uuid::Uuid>, SweepChannel> =
        std::collections::HashMap::new();
    for row in rows.iter().take(limit as usize) {
        let channel_id: Option<uuid::Uuid> = row.try_get("channel_id")?;
        let name: Option<String> = row.try_get("channel_name")?;
        let tier_text: Option<String> = row.try_get("channel_tier")?;
        // An unparseable tier is treated as exposed rather than silently
        // clean: the whole point of the sweep is to over-report, not to
        // reassure.
        let exposed = tier_text
            .as_deref()
            .map(|t| {
                t.parse::<ChannelTier>()
                    .map(|tier| !tier.is_at_least_as_strict_as(min_tier))
                    .unwrap_or(true)
            })
            .unwrap_or(false);
        let entry = by_channel.entry(channel_id).or_insert(SweepChannel {
            channel_id,
            name,
            tier: tier_text,
            count: 0,
            exposed,
        });
        entry.count += 1;
    }

    let mut channels: Vec<SweepChannel> = by_channel.into_values().collect();
    // Most occurrences first, then by name so the output is deterministic.
    channels.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.channel_id.cmp(&b.channel_id))
    });
    let occurrences = channels.iter().map(|c| c.count).sum();
    let exposed = channels.iter().filter(|c| c.exposed).map(|c| c.count).sum();

    Ok(SweepReport {
        channels,
        occurrences,
        exposed,
        truncated,
    })
}

/// Every seal in the community, as the matcher's input type.
///
/// This is the one read that returns literals, and its callers are the
/// enforcement points (ingest guard, later the gate and delivery
/// redaction) — relay-internal code. Anything member-facing reads the
/// kind:47100 announcements instead, which carry no literal.
pub async fn load_sealed_literals(
    pool: &PgPool,
    community: CommunityId,
) -> Result<Vec<SealedLiteral>> {
    let rows = sqlx::query(
        "SELECT id, label, literal, min_tier::text AS min_tier \
         FROM content_seals WHERE community_id = $1 ORDER BY id",
    )
    .bind(community.as_uuid())
    .fetch_all(pool)
    .await?;

    let mut seals = Vec::with_capacity(rows.len());
    for row in rows {
        let tier_text: String = row.try_get("min_tier")?;
        // A tier this code cannot parse would have to have entered through
        // a migration adding an enum variant without updating ChannelTier —
        // fail loudly rather than silently skipping a seal, because a
        // skipped seal is an unenforced one.
        let min_tier = tier_text.parse::<ChannelTier>().map_err(|_| {
            crate::error::DbError::InvalidData(format!("unknown channel_tier {tier_text:?}"))
        })?;
        seals.push(SealedLiteral {
            id: row.try_get("id")?,
            label: row.try_get("label")?,
            literal: row.try_get("literal")?,
            min_tier,
        });
    }
    Ok(seals)
}

/// What a revoked seal was, for the revocation announcement. The label and
/// tier are public already (they ride in the kind:47100 announcement);
/// the literal is deliberately **not** here — it has just been erased and
/// nothing downstream may need it.
#[derive(Debug, Clone)]
pub struct RevokedSeal {
    /// The seal's public label.
    pub label: String,
    /// The tier the seal enforced, as its DB string.
    pub min_tier: String,
}

/// Revoke one seal: a hard DELETE, returning what it was, or `None` if no
/// such seal exists in this community.
///
/// Hard delete, not a `revoked_at` column, and that is a privacy decision:
/// the literal is the most sensitive value the relay stores — the entire
/// D31 design exists to keep it from travelling — and once the Owner says
/// it no longer binds, retaining it is pure liability. Enforcement stops by
/// construction: every enforcement point (ingest guard, thread metadata,
/// promotion, gateway resolve/scrub) reads [`load_sealed_literals`], and a
/// deleted row is simply absent from that read.
///
/// What revocation does NOT do: rewrite history. Tokens already stored keep
/// standing as text, and content the seal once refused stays refused-then
/// (deep seal / purge is D32 and the D-log's open question 10). Announcement
/// lifecycle is the API layer's job — kind:47100 is not replaceable, so the
/// relay publishes a second announcement with the same `d` and a revoked
/// marker, and readers take the newest.
pub async fn revoke_seal(
    pool: &PgPool,
    community: CommunityId,
    id: &str,
) -> Result<Option<RevokedSeal>> {
    let row = sqlx::query(
        "DELETE FROM content_seals \
         WHERE community_id = $1 AND id = $2 \
         RETURNING label, min_tier::text AS min_tier",
    )
    .bind(community.as_uuid())
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.map(|r| -> Result<RevokedSeal> {
        Ok(RevokedSeal {
            label: r.try_get("label")?,
            min_tier: r.try_get("min_tier")?,
        })
    })
    .transpose()
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated: the workspace sweep against real stored events.
    //! Run with: `cargo test -p buzz-db --lib seal::pg_tests -- --ignored`
    use super::*;
    use uuid::Uuid;

    const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz";

    async fn pool() -> PgPool {
        let url = std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_owned());
        PgPool::connect(&url).await.expect("connect")
    }

    async fn community(pool: &PgPool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("sweep-{}.example", id.simple()))
            .execute(pool)
            .await
            .expect("community");
        id
    }

    async fn channel(pool: &PgPool, community_id: Uuid, name: &str, tier: ChannelTier) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO channels (id, community_id, name, channel_type, visibility, created_by, tier) \
             VALUES ($1, $2, $3, 'stream'::channel_type, 'open'::channel_visibility, $4, $5::channel_tier)",
        )
        .bind(id)
        .bind(community_id)
        .bind(name)
        .bind(vec![7_u8; 32])
        .bind(tier.as_str())
        .execute(pool)
        .await
        .expect("channel");
        id
    }

    async fn event(
        pool: &PgPool,
        community_id: Uuid,
        channel_id: Option<Uuid>,
        content: &str,
        tags: &str,
        deleted: bool,
    ) {
        sqlx::query(
            "INSERT INTO events (community_id, id, pubkey, created_at, kind, tags, content, sig, channel_id, deleted_at) \
             VALUES ($1, $2, $3, NOW(), 9, $4::jsonb, $5, $6, $7, $8)",
        )
        .bind(community_id)
        .bind(Uuid::new_v4().as_bytes().repeat(2))
        .bind(vec![9_u8; 32])
        .bind(tags)
        .bind(content)
        .bind(vec![3_u8; 64])
        .bind(channel_id)
        .bind(if deleted { Some(chrono::Utc::now()) } else { None })
        .execute(pool)
        .await
        .expect("event");
    }

    /// The sweep's whole job: tell the Owner that sealing a value did not
    /// retroactively contain it, and say exactly where it still sits.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn the_sweep_counts_prior_occurrences_and_marks_the_exposed_ones() {
        const LITERAL: &str = "Aurora Dynamics GmbH";
        let pool = pool().await;
        let cid = community(&pool).await;
        let cdb = CommunityId::from_uuid(cid);

        let open = channel(&pool, cid, "open-ch", ChannelTier::Open).await;
        let private = channel(&pool, cid, "priv-ch", ChannelTier::Private).await;
        let owned = channel(&pool, cid, "owned-ch", ChannelTier::Owned).await;

        // Two exposed occurrences in the open channel...
        event(
            &pool,
            cid,
            Some(open),
            &format!("re {LITERAL} renewal"),
            "[]",
            false,
        )
        .await;
        event(
            &pool,
            cid,
            Some(open),
            &format!("{LITERAL} again"),
            "[]",
            false,
        )
        .await;
        // ...one in a tag, which the ingestion guard also treats as exposure.
        event(
            &pool,
            cid,
            Some(open),
            "see attached",
            &format!(r#"[["alt","{LITERAL}"]]"#),
            false,
        )
        .await;
        // At and above the seal's floor: present, but not exposed.
        event(
            &pool,
            cid,
            Some(private),
            &format!("{LITERAL} brief"),
            "[]",
            false,
        )
        .await;
        event(
            &pool,
            cid,
            Some(owned),
            &format!("{LITERAL} notes"),
            "[]",
            false,
        )
        .await;
        // Deleted rows are gone, not hidden exposure.
        event(
            &pool,
            cid,
            Some(open),
            &format!("{LITERAL} oops"),
            "[]",
            true,
        )
        .await;
        // Unrelated text must not inflate the count.
        event(&pool, cid, Some(open), "some other client", "[]", false).await;

        let report = sweep_literal(&pool, cdb, LITERAL, ChannelTier::Private, 500)
            .await
            .expect("sweep");

        assert_eq!(report.occurrences, 5, "{:?}", report.channels);
        assert_eq!(report.exposed, 3, "only the open channel is below private");
        assert!(!report.truncated);

        let open_row = report
            .channels
            .iter()
            .find(|c| c.channel_id == Some(open))
            .expect("open channel in report");
        assert_eq!(open_row.count, 3);
        assert!(open_row.exposed);
        assert_eq!(open_row.tier.as_deref(), Some("open"));
        // Most occurrences first.
        assert_eq!(report.channels[0].channel_id, Some(open));

        for (ch, name) in [(private, "priv-ch"), (owned, "owned-ch")] {
            let row = report
                .channels
                .iter()
                .find(|c| c.channel_id == Some(ch))
                .unwrap_or_else(|| panic!("{name} in report"));
            assert!(!row.exposed, "{name} is strict enough");
        }

        // The report travels to the Owner and into logs — it must carry
        // where and how much, never the value itself.
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(
            !json.contains(LITERAL),
            "a sweep report must never echo the sealed value: {json}"
        );

        // A tighter seal exposes more: at `owned`, the private channel
        // counts too. Same stored rows, different verdict — so the tier is
        // provably the deciding input.
        let stricter = sweep_literal(&pool, cdb, LITERAL, ChannelTier::Owned, 500)
            .await
            .expect("sweep");
        assert_eq!(stricter.occurrences, 5);
        assert_eq!(stricter.exposed, 4, "open (3) + private (1)");

        // Truncation is reported, not silently swallowed.
        let capped = sweep_literal(&pool, cdb, LITERAL, ChannelTier::Private, 2)
            .await
            .expect("sweep");
        assert!(capped.truncated);
        assert_eq!(capped.occurrences, 2);
    }
}
