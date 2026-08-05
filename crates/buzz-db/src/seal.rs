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
