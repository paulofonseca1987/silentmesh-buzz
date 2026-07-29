//! Channel-repo bindings -- the `channel_repos` table.
//!
//! Records the forge repository provisioned for a channel at creation
//! (Silent Mesh D5/D6: channel = folder = repo). The repo itself lives on
//! the forge (kind:30617 announcement + CAS manifest); this table is the
//! relay-side binding used for lookups and reconciliation. One repo per
//! channel; repo names are unique per community, mirroring
//! `git_repo_names` semantics.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use buzz_core::CommunityId;

use crate::error::{DbError, Result};

/// A stored channel-repo binding.
#[derive(Debug, Clone)]
pub struct ChannelRepoRecord {
    /// Community that owns the channel and binding.
    pub community_id: CommunityId,
    /// The bound channel.
    pub channel_id: Uuid,
    /// Forge repo name (the kind:30617 `d` tag / smart-HTTP `{repo}` segment).
    pub repo_name: String,
    /// Pubkey hex of the announcement author (the relay for auto-provisioned
    /// repos).
    pub owner_pubkey: String,
    /// When the binding was recorded.
    pub created_at: DateTime<Utc>,
}

/// Record a channel-repo binding. Idempotent for the same pair; a conflict
/// on either key (channel already bound, or repo name taken by another
/// channel) returns `Ok(false)`.
pub async fn bind_channel_repo(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
    repo_name: &str,
    owner_pubkey: &str,
) -> Result<bool> {
    let inserted = sqlx::query(
        r#"
        INSERT INTO channel_repos (community_id, channel_id, repo_name, owner_pubkey)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .bind(repo_name)
    .bind(owner_pubkey.to_ascii_lowercase())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted > 0)
}

/// Fetch the binding for a channel, if one exists.
pub async fn get_channel_repo(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
) -> Result<Option<ChannelRepoRecord>> {
    let row = sqlx::query(
        r#"
        SELECT community_id, channel_id, repo_name, owner_pubkey, created_at
        FROM channel_repos
        WHERE community_id = $1 AND channel_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_record).transpose()
}

/// Reverse lookup: the channel bound to a repo name, if any.
pub async fn get_channel_for_repo(
    pool: &PgPool,
    community_id: CommunityId,
    repo_name: &str,
) -> Result<Option<ChannelRepoRecord>> {
    let row = sqlx::query(
        r#"
        SELECT community_id, channel_id, repo_name, owner_pubkey, created_at
        FROM channel_repos
        WHERE community_id = $1 AND repo_name = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(repo_name)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_record).transpose()
}

fn row_to_record(row: sqlx::postgres::PgRow) -> Result<ChannelRepoRecord> {
    let community_id: Uuid = row.try_get("community_id")?;
    Ok(ChannelRepoRecord {
        community_id: CommunityId::from_uuid(community_id),
        channel_id: row.try_get("channel_id")?,
        repo_name: row.try_get("repo_name")?,
        owner_pubkey: row.try_get("owner_pubkey")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Validate a repo name against the forge's `{repo}` segment rules
/// (`[a-zA-Z0-9._-]{1,64}`, no leading dot, no `..`).
pub fn is_valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Guard against malformed pubkeys reaching the binding row.
pub fn is_valid_owner_hex(owner: &str) -> bool {
    owner.len() == 64 && owner.chars().all(|c| c.is_ascii_hexdigit())
}

/// Validate binding inputs, returning a typed error for callers that want
/// to reject before touching the pool.
pub fn validate_binding(repo_name: &str, owner_pubkey: &str) -> Result<()> {
    if !is_valid_repo_name(repo_name) {
        return Err(DbError::InvalidData(format!(
            "invalid repo name: {repo_name}"
        )));
    }
    if !is_valid_owner_hex(owner_pubkey) {
        return Err(DbError::InvalidData("invalid owner pubkey hex".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_validation() {
        assert!(is_valid_repo_name("abc123"));
        assert!(is_valid_repo_name("a-b_c.d"));
        assert!(is_valid_repo_name(&"a".repeat(64)));
        assert!(!is_valid_repo_name(""));
        assert!(!is_valid_repo_name(&"a".repeat(65)));
        assert!(!is_valid_repo_name(".hidden"));
        assert!(!is_valid_repo_name("a..b"));
        assert!(!is_valid_repo_name("a/b"));
    }

    #[test]
    fn owner_hex_validation() {
        assert!(is_valid_owner_hex(&"a".repeat(64)));
        assert!(!is_valid_owner_hex(&"a".repeat(63)));
        assert!(!is_valid_owner_hex(&"g".repeat(64)));
    }
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated tests for the Phase 2a data layer: tier immutability,
    //! repo bindings, guest rejection, and workspace-authority membership.
    //! Run with:
    //!   `cargo test -p buzz-db --lib channel_repo -- --ignored`
    use super::*;
    use crate::channel::{ChannelTier, ChannelType, ChannelVisibility, MemberRole};

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
    async fn tier_persists_and_is_immutable() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let creator = vec![0xa1u8; 32];

        let channel = crate::channel::create_channel_tiered(
            &pool,
            community,
            "tiered",
            ChannelType::Stream,
            ChannelVisibility::Private,
            ChannelTier::Private,
            None,
            &creator,
            None,
        )
        .await
        .expect("create tiered channel");
        assert_eq!(channel.tier, "private");

        let fetched = crate::channel::get_channel(&pool, community, channel.id)
            .await
            .expect("fetch channel");
        assert_eq!(fetched.tier, "private");

        // The BEFORE UPDATE trigger blocks any tier change, even raw SQL.
        let direct =
            sqlx::query("UPDATE channels SET tier = 'open' WHERE community_id = $1 AND id = $2")
                .bind(community.as_uuid())
                .bind(channel.id)
                .execute(&pool)
                .await;
        let err = direct.expect_err("tier update must be rejected by the trigger");
        assert!(
            err.to_string().contains("immutable"),
            "unexpected error: {err}"
        );

        // Untiered creation defaults to open.
        let plain = crate::channel::create_channel(
            &pool,
            community,
            "untiered",
            ChannelType::Stream,
            ChannelVisibility::Open,
            None,
            &creator,
            None,
        )
        .await
        .expect("create default channel");
        assert_eq!(plain.tier, "open");
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn repo_binding_round_trip_and_conflicts() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let creator = vec![0xa2u8; 32];
        let channel = crate::channel::create_channel(
            &pool,
            community,
            "bound",
            ChannelType::Stream,
            ChannelVisibility::Open,
            None,
            &creator,
            None,
        )
        .await
        .expect("create channel");
        let other = crate::channel::create_channel(
            &pool,
            community,
            "other",
            ChannelType::Stream,
            ChannelVisibility::Open,
            None,
            &creator,
            None,
        )
        .await
        .expect("create other channel");

        let repo = channel.id.simple().to_string();
        let owner = "c".repeat(64);
        assert!(
            bind_channel_repo(&pool, community, channel.id, &repo, &owner)
                .await
                .expect("bind")
        );
        // Replay is a no-op, not an error.
        assert!(
            !bind_channel_repo(&pool, community, channel.id, &repo, &owner)
                .await
                .expect("rebind")
        );
        // The repo name cannot bind a second channel.
        assert!(
            !bind_channel_repo(&pool, community, other.id, &repo, &owner)
                .await
                .expect("cross-bind")
        );

        let by_channel = get_channel_repo(&pool, community, channel.id)
            .await
            .expect("get binding")
            .expect("binding exists");
        assert_eq!(by_channel.repo_name, repo);
        assert_eq!(by_channel.owner_pubkey, owner);
        let by_repo = get_channel_for_repo(&pool, community, &repo)
            .await
            .expect("reverse lookup")
            .expect("binding exists");
        assert_eq!(by_repo.channel_id, channel.id);

        // Bindings are community-fenced.
        let community_b = make_community(&pool).await;
        assert!(get_channel_for_repo(&pool, community_b, &repo)
            .await
            .expect("lookup other community")
            .is_none());
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn guest_rejected_and_workspace_authority_grants() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let creator = vec![0xa3u8; 32];
        let target = vec![0xa4u8; 32];
        let workspace_admin = vec![0xa5u8; 32];

        for pk in [&creator, &target, &workspace_admin] {
            crate::user::ensure_user(&pool, community, pk)
                .await
                .expect("ensure user");
        }

        let channel = crate::channel::create_channel(
            &pool,
            community,
            "authority",
            ChannelType::Stream,
            ChannelVisibility::Private,
            None,
            &creator,
            None,
        )
        .await
        .expect("create channel");

        // Guest is disabled at the membership choke point.
        let guest = crate::channel::add_member(
            &pool,
            community,
            channel.id,
            &target,
            MemberRole::Guest,
            Some(&creator),
        )
        .await;
        assert!(guest.is_err(), "guest grants must be rejected on this fork");

        // A non-member without workspace authority cannot grant into a
        // private channel.
        let plain = crate::channel::add_member(
            &pool,
            community,
            channel.id,
            &target,
            MemberRole::Admin,
            Some(&workspace_admin),
        )
        .await;
        assert!(plain.is_err(), "non-member grant must fail");

        // The same grant under verified workspace authority succeeds and
        // lands the elevated role.
        let granted = crate::channel::add_member_as_workspace_authority(
            &pool,
            community,
            channel.id,
            &target,
            MemberRole::Admin,
            Some(&workspace_admin),
        )
        .await
        .expect("workspace-authority grant");
        assert_eq!(granted.role, "admin");

        // Workspace authority still cannot demote the last owner.
        let demote = crate::channel::add_member_as_workspace_authority(
            &pool,
            community,
            channel.id,
            &creator,
            MemberRole::Member,
            Some(&workspace_admin),
        )
        .await;
        assert!(
            demote.is_err(),
            "last-owner demotion must fail even under workspace authority"
        );
    }
}
