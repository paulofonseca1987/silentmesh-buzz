//! Personal channels — the `personal_channels` registry (Silent Mesh
//! Phase 2g, D29).
//!
//! A personal channel is a member's private space (self + their agents):
//! visibility is forced private at creation, the member is bootstrapped as
//! the channel owner (making them the implicit Channel Admin, D41/D42),
//! and the registry's primary key enforces **one personal channel per
//! member per community**. Channel row, owner membership, and registry row
//! are written in a single transaction so a personal channel can never
//! exist half-marked.

use sqlx::PgPool;
use uuid::Uuid;

use buzz_core::CommunityId;

use crate::channel::{ChannelRecord, ChannelTier, ChannelType};
use crate::error::{DbError, Result};

/// Outcome of a personal-channel creation attempt.
#[derive(Debug)]
pub enum CreatePersonalChannelResult {
    /// Created; the member is the channel owner.
    Created(Box<ChannelRecord>),
    /// The channel id already exists (client replay) — nothing changed.
    DuplicateChannel,
    /// The member already has a personal channel in this community.
    AlreadyHasPersonal(Uuid),
}

/// Create a member's personal channel: channel row (visibility **forced
/// `private`**), owner membership, and registry row, atomically.
#[allow(clippy::too_many_arguments)]
pub async fn create_personal_channel(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
    name: &str,
    channel_type: ChannelType,
    tier: ChannelTier,
    description: Option<&str>,
    owner_pubkey: &[u8],
    ttl_seconds: Option<i32>,
) -> Result<CreatePersonalChannelResult> {
    if owner_pubkey.len() != 32 {
        return Err(DbError::InvalidData(format!(
            "pubkey must be 32 bytes, got {}",
            owner_pubkey.len()
        )));
    }
    if channel_id.is_nil() {
        return Err(DbError::InvalidData(
            "channel_id must not be nil (reserved for global fan-out)".into(),
        ));
    }
    let name = buzz_core::channel::canonical_channel_name(name);
    if name.trim().is_empty() {
        return Err(DbError::InvalidData("channel name is required".into()));
    }

    let mut tx = pool.begin().await?;

    // Fail fast if the member already has a LIVE personal channel — before
    // any channel row exists to leak on rollback. A registry row whose
    // channel was soft-deleted is stale: reclaim it here so deleting a
    // personal channel does not lock the member out forever.
    let existing: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT pc.channel_id, (c.deleted_at IS NOT NULL) AS deleted \
         FROM personal_channels pc \
         JOIN channels c ON c.community_id = pc.community_id AND c.id = pc.channel_id \
         WHERE pc.community_id = $1 AND pc.owner_pubkey = $2",
    )
    .bind(community_id.as_uuid())
    .bind(owner_pubkey)
    .fetch_optional(tx.as_mut())
    .await?;
    match existing {
        Some((ch, false)) => {
            return Ok(CreatePersonalChannelResult::AlreadyHasPersonal(ch));
        }
        Some((_, true)) => {
            sqlx::query(
                "DELETE FROM personal_channels \
                 WHERE community_id = $1 AND owner_pubkey = $2",
            )
            .bind(community_id.as_uuid())
            .bind(owner_pubkey)
            .execute(tx.as_mut())
            .await?;
        }
        None => {}
    }

    // Channel insert — mirrors `channel::create_channel_with_id_tiered`
    // (kept in sync by the PG round-trip test) with visibility pinned to
    // `private`.
    let rows_affected = sqlx::query(
        r#"
        INSERT INTO channels (id, community_id, name, channel_type, visibility, description, created_by, ttl_seconds, ttl_deadline, tier)
        VALUES ($1, $2, $3, $4::channel_type, 'private', $5, $6, $7,
                CASE WHEN $7 IS NOT NULL THEN NOW() + ($7 || ' seconds')::interval ELSE NULL END,
                $8::channel_tier)
        ON CONFLICT (community_id, id) DO NOTHING
        "#,
    )
    .bind(channel_id)
    .bind(community_id.as_uuid())
    .bind(name)
    .bind(channel_type.as_str())
    .bind(description)
    .bind(owner_pubkey)
    .bind(ttl_seconds)
    .bind(tier.as_str())
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if rows_affected == 0 {
        return Ok(CreatePersonalChannelResult::DuplicateChannel);
    }

    sqlx::query(
        r#"
        INSERT INTO channel_members (community_id, channel_id, pubkey, role, invited_by)
        VALUES ($1, $2, $3, 'owner', $3)
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .bind(owner_pubkey)
    .execute(tx.as_mut())
    .await?;

    // The registry row — the PK is the one-per-member guarantee; the
    // SELECT above makes the common conflict friendly, this insert is the
    // race-proof backstop.
    let marked = sqlx::query(
        r#"
        INSERT INTO personal_channels (community_id, owner_pubkey, channel_id)
        VALUES ($1, $2, $3)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(owner_pubkey)
    .bind(channel_id)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    if marked == 0 {
        // Lost a concurrent-creation race — roll everything back.
        tx.rollback().await?;
        let winner: Option<(Uuid,)> = sqlx::query_as(
            "SELECT channel_id FROM personal_channels \
             WHERE community_id = $1 AND owner_pubkey = $2",
        )
        .bind(community_id.as_uuid())
        .bind(owner_pubkey)
        .fetch_optional(pool)
        .await?;
        return Ok(match winner {
            Some((ch,)) => CreatePersonalChannelResult::AlreadyHasPersonal(ch),
            None => CreatePersonalChannelResult::DuplicateChannel,
        });
    }

    let row = sqlx::query(
        r#"
        SELECT id, name, channel_type::text AS channel_type, visibility::text AS visibility,
               description, canvas,
               created_by, created_at, updated_at, archived_at, deleted_at,
               nip29_group_id, topic_required, max_members,
               topic, topic_set_by, topic_set_at,
               purpose, purpose_set_by, purpose_set_at,
               ttl_seconds, ttl_deadline, tier::text AS tier
        FROM channels WHERE community_id = $1 AND id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .fetch_one(tx.as_mut())
    .await?;
    let record = crate::channel::row_to_channel_record(row)?;

    tx.commit().await?;
    Ok(CreatePersonalChannelResult::Created(Box::new(record)))
}

/// Whose personal channel is `channel_id`, if anyone's? Soft-deleted
/// channels are not personal channels (their registry row is stale until
/// reclaimed by the owner's next create).
pub async fn get_personal_channel_owner(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
) -> Result<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT pc.owner_pubkey FROM personal_channels pc \
         JOIN channels c ON c.community_id = pc.community_id AND c.id = pc.channel_id \
         WHERE pc.community_id = $1 AND pc.channel_id = $2 AND c.deleted_at IS NULL",
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(pk,)| pk))
}

/// The member's live personal channel in this community, if they have one.
pub async fn get_personal_channel_for(
    pool: &PgPool,
    community_id: CommunityId,
    owner_pubkey: &[u8],
) -> Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT pc.channel_id FROM personal_channels pc \
         JOIN channels c ON c.community_id = pc.community_id AND c.id = pc.channel_id \
         WHERE pc.community_id = $1 AND pc.owner_pubkey = $2 AND c.deleted_at IS NULL",
    )
    .bind(community_id.as_uuid())
    .bind(owner_pubkey)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ch,)| ch))
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated tests: atomic creation, forced privacy, owner
    //! bootstrap, one-per-member, and duplicate-replay handling.
    //! Run with:
    //!   `cargo test -p buzz-db --lib personal_channel -- --ignored`
    use super::*;

    const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz";

    async fn setup_pool() -> PgPool {
        let database_url = std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_owned());
        PgPool::connect(&database_url)
            .await
            .expect("connect to test DB")
    }

    async fn make_community(pool: &PgPool) -> CommunityId {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("test-{}.example", id.simple()))
            .execute(pool)
            .await
            .expect("insert community");
        CommunityId::from_uuid(id)
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn one_personal_channel_per_member() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let owner = vec![0xaau8; 32];
        let ch1 = Uuid::new_v4();

        let created = create_personal_channel(
            &pool,
            community,
            ch1,
            "my-space",
            ChannelType::Stream,
            ChannelTier::Private,
            Some("personal"),
            &owner,
            None,
        )
        .await
        .expect("create personal channel");
        let record = match created {
            CreatePersonalChannelResult::Created(r) => r,
            other => panic!("expected Created, got {other:?}"),
        };
        assert_eq!(record.visibility, "private", "visibility is forced");
        assert_eq!(record.tier, "private");

        // The creator is the channel owner (implicit Channel Admin).
        let members = crate::channel::get_members(&pool, community, ch1)
            .await
            .expect("members");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].pubkey, owner);
        assert_eq!(members[0].role, "owner");

        // Lookups round-trip.
        assert_eq!(
            get_personal_channel_owner(&pool, community, ch1)
                .await
                .expect("owner lookup"),
            Some(owner.clone())
        );
        assert_eq!(
            get_personal_channel_for(&pool, community, &owner)
                .await
                .expect("channel lookup"),
            Some(ch1)
        );

        // Same event replayed (same channel id) → duplicate, not an error.
        let dup = create_personal_channel(
            &pool,
            community,
            ch1,
            "my-space",
            ChannelType::Stream,
            ChannelTier::Private,
            None,
            &owner,
            None,
        )
        .await
        .expect("replay");
        assert!(matches!(
            dup,
            CreatePersonalChannelResult::AlreadyHasPersonal(ch) if ch == ch1
        ));

        // A second personal channel for the same member is refused; nothing
        // is created.
        let ch2 = Uuid::new_v4();
        let second = create_personal_channel(
            &pool,
            community,
            ch2,
            "my-other-space",
            ChannelType::Stream,
            ChannelTier::Private,
            None,
            &owner,
            None,
        )
        .await
        .expect("second attempt");
        assert!(matches!(
            second,
            CreatePersonalChannelResult::AlreadyHasPersonal(ch) if ch == ch1
        ));
        let orphan = crate::channel::get_channel(&pool, community, ch2).await;
        assert!(orphan.is_err(), "refused creation must not leave a channel");

        // A different member in the same community is unaffected.
        let other = vec![0xbbu8; 32];
        let ch3 = Uuid::new_v4();
        let created = create_personal_channel(
            &pool,
            community,
            ch3,
            "their-space",
            ChannelType::Stream,
            ChannelTier::Open,
            None,
            &other,
            None,
        )
        .await
        .expect("other member");
        assert!(matches!(created, CreatePersonalChannelResult::Created(_)));

        // Ordinary channels are not personal.
        let team = crate::channel::create_channel(
            &pool,
            community,
            "team",
            ChannelType::Stream,
            crate::channel::ChannelVisibility::Open,
            None,
            &owner,
            None,
        )
        .await
        .expect("team channel");
        assert_eq!(
            get_personal_channel_owner(&pool, community, team.id)
                .await
                .expect("team lookup"),
            None
        );

        // Soft-deleting the personal channel frees the slot: the stale
        // registry row stops answering lookups and is reclaimed by the
        // owner's next create.
        sqlx::query("UPDATE channels SET deleted_at = NOW() WHERE community_id = $1 AND id = $2")
            .bind(community.as_uuid())
            .bind(ch1)
            .execute(&pool)
            .await
            .expect("soft delete");
        assert_eq!(
            get_personal_channel_owner(&pool, community, ch1)
                .await
                .expect("deleted lookup"),
            None,
            "a soft-deleted channel is no longer anyone's personal channel"
        );
        assert_eq!(
            get_personal_channel_for(&pool, community, &owner)
                .await
                .expect("owner lookup after delete"),
            None
        );
        let ch4 = Uuid::new_v4();
        let recreated = create_personal_channel(
            &pool,
            community,
            ch4,
            "my-new-space",
            ChannelType::Stream,
            ChannelTier::Private,
            None,
            &owner,
            None,
        )
        .await
        .expect("recreate after delete");
        assert!(
            matches!(recreated, CreatePersonalChannelResult::Created(_)),
            "the member must be able to create a fresh personal channel"
        );
        assert_eq!(
            get_personal_channel_for(&pool, community, &owner)
                .await
                .expect("owner lookup after recreate"),
            Some(ch4)
        );
    }
}
