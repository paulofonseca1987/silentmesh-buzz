//! Per-request model-usage attribution — the `model_usage` table
//! (Silent Mesh Phase 3, D16, model plane).
//!
//! Every model request the gateway routes records one row:
//! `(user, agent, channel, thread, model, tier, backend, purpose)` plus
//! token counts. The owner's usage view — "per-user totals by tier and
//! backend" — is a straight aggregate over this table, and later budgets
//! read the same rows. Community-scoped; the signed 44200 turn-metric
//! events remain the per-turn wire record, while this table is the
//! queryable metering substrate the gateway owns.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use buzz_core::channel::ChannelTier;
use buzz_core::model_route::{Backend, InferencePurpose};

use crate::error::Result;
use crate::CommunityId;

/// Parameters for recording one routed model request.
pub struct RecordModelUsageParams<'a> {
    /// Community that owns the request.
    pub community_id: CommunityId,
    /// The end user the request is attributed to (32-byte pubkey).
    pub user_pubkey: &'a [u8],
    /// The agent that made the request, if any (32-byte pubkey). `None`
    /// for a human's direct request (e.g. a copilot call).
    pub agent_pubkey: Option<&'a [u8]>,
    /// The channel the request ran in, if any.
    pub channel_id: Option<Uuid>,
    /// The work thread, if the request is thread-scoped (32-byte root id).
    pub thread_id: Option<&'a [u8]>,
    /// The effective model id.
    pub model: &'a str,
    /// The channel tier the request ran under (the routing input).
    pub tier: ChannelTier,
    /// The backend the request was served by.
    pub backend: Backend,
    /// What the request was for.
    pub purpose: InferencePurpose,
    /// Prompt (input) tokens.
    pub prompt_tokens: i64,
    /// Completion (output) tokens.
    pub completion_tokens: i64,
}

/// A per-user rollup row: totals for one `(user, tier, backend)` bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageTotal {
    /// The user the totals are for.
    pub user_pubkey: Vec<u8>,
    /// The tier bucket.
    pub tier: String,
    /// The backend bucket.
    pub backend: String,
    /// Number of requests in the bucket.
    pub requests: i64,
    /// Summed prompt tokens.
    pub prompt_tokens: i64,
    /// Summed completion tokens.
    pub completion_tokens: i64,
}

/// Record one routed model request. Returns the new row's community-local id.
pub async fn record_model_usage(pool: &PgPool, params: RecordModelUsageParams<'_>) -> Result<i64> {
    let id: i64 = sqlx::query(
        r#"
        INSERT INTO model_usage
            (community_id, user_pubkey, agent_pubkey, channel_id, thread_id,
             model, tier, backend, purpose, prompt_tokens, completion_tokens)
        VALUES ($1, $2, $3, $4, $5, $6, $7::channel_tier, $8, $9, $10, $11)
        RETURNING id
        "#,
    )
    .bind(params.community_id.as_uuid())
    .bind(params.user_pubkey)
    .bind(params.agent_pubkey)
    .bind(params.channel_id)
    .bind(params.thread_id)
    .bind(params.model)
    .bind(params.tier.as_str())
    .bind(params.backend.as_str())
    .bind(params.purpose.as_str())
    .bind(params.prompt_tokens)
    .bind(params.completion_tokens)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

/// Per-user usage totals grouped by `(user, tier, backend)`, optionally
/// since a cutoff. Rows are returned in a stable `(user_pubkey, tier,
/// backend)` order (not by activity) so a caller can group them
/// deterministically. The owner's headline metering read (roadmap Phase 3
/// exit: "usage query shows per-user totals by tier and backend").
pub async fn user_usage_totals(
    pool: &PgPool,
    community_id: CommunityId,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<UsageTotal>> {
    let rows = sqlx::query(
        r#"
        SELECT user_pubkey, tier::text AS tier, backend,
               COUNT(*) AS requests,
               COALESCE(SUM(prompt_tokens), 0)::bigint AS prompt_tokens,
               COALESCE(SUM(completion_tokens), 0)::bigint AS completion_tokens
        FROM model_usage
        WHERE community_id = $1
          AND ($2::timestamptz IS NULL OR created_at >= $2)
        GROUP BY user_pubkey, tier, backend
        ORDER BY user_pubkey, tier, backend
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(since)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(UsageTotal {
                user_pubkey: row.try_get("user_pubkey")?,
                tier: row.try_get("tier")?,
                backend: row.try_get("backend")?,
                requests: row.try_get("requests")?,
                prompt_tokens: row.try_get("prompt_tokens")?,
                completion_tokens: row.try_get("completion_tokens")?,
            })
        })
        .collect()
}

/// Which backends have actually served inference **in this channel**
/// (Silent Mesh D24/D30).
///
/// Declared tiers say what a space is *allowed* to do; these rows say what
/// it *did*. Promotion consults both, because a member who re-tiers their
/// personal channel from `open` to `owned` has changed a permission, not
/// un-sent the prompts a vendor already saw.
///
/// Evidence, not proof: it covers inference the harness or gateway
/// metered, not text pasted in from elsewhere. It can therefore refuse
/// wrongly-optimistic movement, never certify a channel as clean.
pub async fn channel_backends_used(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT backend
        FROM model_usage
        WHERE community_id = $1 AND channel_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A single user's total token spend (prompt + completion) since a cutoff —
/// the number a per-user budget check compares against.
pub async fn user_token_spend(
    pool: &PgPool,
    community_id: CommunityId,
    user_pubkey: &[u8],
    since: Option<DateTime<Utc>>,
) -> Result<i64> {
    let total: i64 = sqlx::query_scalar(
        r#"
        SELECT COALESCE(SUM(prompt_tokens + completion_tokens), 0)::bigint
        FROM model_usage
        WHERE community_id = $1 AND user_pubkey = $2
          AND ($3::timestamptz IS NULL OR created_at >= $3)
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(user_pubkey)
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated tests: attribution insert + the per-user rollup and
    //! spend queries. Run with:
    //!   `cargo test -p buzz-db --lib model_usage -- --ignored`
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
            .bind(format!("usage-{}.example", id.simple()))
            .execute(pool)
            .await
            .expect("insert community");
        CommunityId::from_uuid(id)
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn records_and_rolls_up_by_user_tier_backend() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let alice = vec![0xaau8; 32];
        let bob = vec![0xbbu8; 32];
        let agent = vec![0xccu8; 32];
        let channel = Uuid::new_v4();

        let rec = |user: &'static [u8],
                   tier: ChannelTier,
                   backend: Backend,
                   purpose: InferencePurpose,
                   p: i64,
                   c: i64| {
            let pool = pool.clone();
            let user = user.to_vec();
            let agent = agent.clone();
            async move {
                record_model_usage(
                    &pool,
                    RecordModelUsageParams {
                        community_id: community,
                        user_pubkey: &user,
                        agent_pubkey: Some(&agent),
                        channel_id: Some(channel),
                        thread_id: None,
                        model: "test-model",
                        tier,
                        backend,
                        purpose,
                        prompt_tokens: p,
                        completion_tokens: c,
                    },
                )
                .await
                .expect("record")
            }
        };

        // Alice: two owned/local agent turns + one open/vendor turn.
        rec(
            &[0xaau8; 32],
            ChannelTier::Owned,
            Backend::Local,
            InferencePurpose::AgentTurn,
            100,
            20,
        )
        .await;
        rec(
            &[0xaau8; 32],
            ChannelTier::Owned,
            Backend::Local,
            InferencePurpose::AgentTurn,
            50,
            10,
        )
        .await;
        rec(
            &[0xaau8; 32],
            ChannelTier::Open,
            Backend::Vendor,
            InferencePurpose::AgentTurn,
            200,
            40,
        )
        .await;
        // Bob: one private/tee turn.
        rec(
            &[0xbbu8; 32],
            ChannelTier::Private,
            Backend::Tee,
            InferencePurpose::AgentTurn,
            300,
            60,
        )
        .await;

        let totals = user_usage_totals(&pool, community, None)
            .await
            .expect("totals");
        // Buckets: Alice(owned,local), Alice(open,vendor), Bob(private,tee).
        let get = |user: &[u8], tier: &str, backend: &str| {
            totals
                .iter()
                .find(|t| t.user_pubkey == user && t.tier == tier && t.backend == backend)
                .cloned()
        };
        let a_local = get(&alice, "owned", "local").expect("alice owned/local bucket");
        assert_eq!(a_local.requests, 2);
        assert_eq!(a_local.prompt_tokens, 150);
        assert_eq!(a_local.completion_tokens, 30);
        let a_vendor = get(&alice, "open", "vendor").expect("alice open/vendor bucket");
        assert_eq!(a_vendor.requests, 1);
        assert_eq!(a_vendor.prompt_tokens, 200);
        let b_tee = get(&bob, "private", "tee").expect("bob private/tee bucket");
        assert_eq!(b_tee.requests, 1);
        assert_eq!(b_tee.completion_tokens, 60);
        // Alice never used tee; no such bucket.
        assert!(get(&alice, "private", "tee").is_none());

        // Per-user spend = prompt + completion across all buckets.
        let alice_spend = user_token_spend(&pool, community, &alice, None)
            .await
            .expect("alice spend");
        assert_eq!(alice_spend, 100 + 20 + 50 + 10 + 200 + 40);
        let bob_spend = user_token_spend(&pool, community, &bob, None)
            .await
            .expect("bob spend");
        assert_eq!(bob_spend, 300 + 60);

        // A future cutoff excludes everything.
        let future = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(
            user_token_spend(&pool, community, &alice, Some(future))
                .await
                .expect("future spend"),
            0
        );
        assert!(user_usage_totals(&pool, community, Some(future))
            .await
            .expect("future totals")
            .is_empty());
    }
}
