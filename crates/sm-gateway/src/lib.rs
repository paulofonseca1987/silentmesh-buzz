//! Silent Mesh model gateway (Phase 3, D16).
//!
//! All model traffic in the workspace routes through here so two invariants
//! hold in one place: **tier-aware routing** — a request may only reach a
//! backend that satisfies its channel's minimum privacy tier (the policy
//! lives in [`buzz_core::model_route`]) — and **per-request metering** —
//! every routed request is attributed `(user, agent, channel, thread,
//! model, tier, backend, purpose)` in `model_usage`.
//!
//! The gateway is the routing gate, the metering write, an optional
//! per-user token budget, and a registry of [`ModelBackend`] impls. The
//! `local` class is real — [`ollama::OllamaBackend`], an Ollama server that
//! must prove its locality to be built at all. The remaining classes are
//! still [`stub`]s: a TEE provider with attestation-then-send, and per-user
//! vendor CLIs land in later slices; nothing about the routing or metering
//! contract changes when they do.
//!
//! Everything privacy-critical is decided by the pure policy, so the
//! gateway's own logic is small and testable against stubs with no
//! hardware or provider.

use std::collections::HashMap;

use async_trait::async_trait;
use uuid::Uuid;

use buzz_core::channel::ChannelTier;
use buzz_core::model_route::{self, Backend, InferencePurpose, RefuseReason, RouteDecision};
use buzz_core::CommunityId;
use buzz_db::model_usage::RecordModelUsageParams;
use buzz_db::Db;

pub mod assist;
pub mod ollama;
pub mod stub;

/// The backend a request routes to: its explicit choice, or the loosest
/// backend the tier/purpose permits (the most capable route still within
/// the channel's privacy floor). Pure over the request — the routing gate
/// in [`Gateway::route_and_record`] still refuses an out-of-floor explicit
/// choice.
pub fn resolve_backend(req: &InferenceRequest) -> Backend {
    req.backend.unwrap_or_else(|| {
        model_route::allowed_backends(req.tier, req.purpose)
            .last()
            .copied()
            .unwrap_or(Backend::Local)
    })
}

/// A model request presented to the gateway.
#[derive(Debug, Clone)]
pub struct InferenceRequest {
    /// Community the request belongs to.
    pub community_id: CommunityId,
    /// End user the request is attributed to (32-byte pubkey).
    pub user_pubkey: Vec<u8>,
    /// Agent making the request, if any (32-byte pubkey).
    pub agent_pubkey: Option<Vec<u8>>,
    /// Channel the request runs in, if any.
    pub channel_id: Option<Uuid>,
    /// Work-thread root, if thread-scoped (32-byte id).
    pub thread_id: Option<Vec<u8>>,
    /// The channel's privacy tier (the routing input).
    pub tier: ChannelTier,
    /// What the request is for.
    pub purpose: InferencePurpose,
    /// Requested model id.
    pub model: String,
    /// Requested backend, or `None` to take the gateway default for the
    /// tier/purpose (the loosest backend the tier permits — the most
    /// capable route still within the channel's privacy floor).
    pub backend: Option<Backend>,
    /// The prompt. (Opaque to routing/metering; the stubs echo its size.)
    pub prompt: String,
}

/// A completed inference plus the attribution the gateway recorded.
#[derive(Debug, Clone)]
pub struct InferenceResponse {
    /// The model's output text.
    pub text: String,
    /// The backend that served it.
    pub backend: Backend,
    /// The effective model id.
    pub model: String,
    /// Prompt (input) tokens.
    pub prompt_tokens: i64,
    /// Completion (output) tokens.
    pub completion_tokens: i64,
    /// The `model_usage` row id recorded for this request.
    pub usage_id: i64,
}

/// A backend's raw output, before the gateway wraps it with attribution.
#[derive(Debug, Clone)]
pub struct RawInference {
    /// Output text.
    pub text: String,
    /// Prompt tokens the backend counted.
    pub prompt_tokens: i64,
    /// Completion tokens the backend counted.
    pub completion_tokens: i64,
}

/// A model backend. Real backends (server GPU, TEE, vendor CLI) implement
/// this; the gateway never calls a backend without the routing policy
/// having allowed it first.
#[async_trait]
pub trait ModelBackend: Send + Sync {
    /// Which routing class this backend is — the gateway matches this
    /// against the policy's decision.
    fn kind(&self) -> Backend;
    /// Human-readable name (for logs/attribution detail).
    fn name(&self) -> &str;
    /// Run one inference. Errors are surfaced as [`GatewayError::Backend`].
    async fn infer(&self, req: &InferenceRequest) -> Result<RawInference, String>;
}

/// Gateway errors. Every variant is a refused or failed request; a routing
/// refusal ([`GatewayError::RouteRefused`]) is the enforcement point the
/// roadmap's "below-tier routes refused" describes.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The routing policy refused this backend for this tier/purpose.
    #[error("route refused: {0}")]
    RouteRefused(RefuseReason),
    /// No backend is registered for the resolved routing class.
    #[error("no backend registered for '{0}'")]
    NoBackend(Backend),
    /// The user's token budget for the window is exhausted.
    #[error("budget exceeded: {spent}/{budget} tokens this window")]
    BudgetExceeded {
        /// Tokens already spent in the window.
        spent: i64,
        /// The configured ceiling.
        budget: i64,
    },
    /// The backend failed to produce a response.
    #[error("backend error: {0}")]
    Backend(String),
    /// A metering/persistence error.
    #[error("db error: {0}")]
    Db(#[from] buzz_db::DbError),
}

/// The gateway: a backend registry + the metering store, plus an optional
/// per-user token budget.
pub struct Gateway {
    db: Db,
    backends: HashMap<Backend, Box<dyn ModelBackend>>,
    /// Optional per-user token ceiling (prompt + completion) checked
    /// against spend in the trailing `budget_window`. `None` = no budget.
    budget: Option<Budget>,
}

/// A simple per-user token budget over a trailing window.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Token ceiling per user for the window.
    pub tokens: i64,
    /// Trailing window; `None` means all-time.
    pub window: Option<chrono::Duration>,
}

impl Gateway {
    /// Build a gateway over `db` with no backends and no budget. Register
    /// backends with [`Gateway::with_backend`].
    pub fn new(db: Db) -> Self {
        Self {
            db,
            backends: HashMap::new(),
            budget: None,
        }
    }

    /// Register a backend for its routing class (replacing any existing one).
    pub fn with_backend(mut self, backend: Box<dyn ModelBackend>) -> Self {
        self.backends.insert(backend.kind(), backend);
        self
    }

    /// Set a per-user token budget.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }

    /// Route, dispatch, and meter one request.
    ///
    /// Order matters: the **routing policy is checked first** (a below-tier
    /// or owned-pinned refusal never reaches a backend), then the budget,
    /// then the backend runs, then attribution is recorded. Attribution is
    /// written only for requests that actually ran — a refused request
    /// consumes no tokens and records nothing.
    pub async fn route_and_record(
        &self,
        req: &InferenceRequest,
    ) -> Result<InferenceResponse, GatewayError> {
        let backend = resolve_backend(req);

        // 1. Tier-aware routing gate.
        if let RouteDecision::Refuse(reason) = model_route::route(req.tier, backend, req.purpose) {
            return Err(GatewayError::RouteRefused(reason));
        }

        // 2. Budget gate (best-effort ceiling, before spending tokens).
        if let Some(budget) = self.budget {
            let since = budget.window.map(|w| chrono::Utc::now() - w);
            let spent = self
                .db
                .user_token_spend(req.community_id, &req.user_pubkey, since)
                .await?;
            if spent >= budget.tokens {
                return Err(GatewayError::BudgetExceeded {
                    spent,
                    budget: budget.tokens,
                });
            }
        }

        // 3. Dispatch.
        let impl_backend = self
            .backends
            .get(&backend)
            .ok_or(GatewayError::NoBackend(backend))?;
        let raw = impl_backend
            .infer(req)
            .await
            .map_err(GatewayError::Backend)?;

        // 4. Meter.
        let usage_id = self
            .db
            .record_model_usage(RecordModelUsageParams {
                community_id: req.community_id,
                user_pubkey: &req.user_pubkey,
                agent_pubkey: req.agent_pubkey.as_deref(),
                channel_id: req.channel_id,
                thread_id: req.thread_id.as_deref(),
                model: &req.model,
                tier: req.tier,
                backend,
                purpose: req.purpose,
                prompt_tokens: raw.prompt_tokens,
                completion_tokens: raw.completion_tokens,
            })
            .await?;

        Ok(InferenceResponse {
            text: raw.text,
            backend,
            model: req.model.clone(),
            prompt_tokens: raw.prompt_tokens,
            completion_tokens: raw.completion_tokens,
            usage_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        tier: ChannelTier,
        backend: Option<Backend>,
        purpose: InferencePurpose,
    ) -> InferenceRequest {
        InferenceRequest {
            community_id: CommunityId::from_uuid(Uuid::from_u128(1)),
            user_pubkey: vec![0xaau8; 32],
            agent_pubkey: None,
            channel_id: Some(Uuid::from_u128(2)),
            thread_id: None,
            tier,
            purpose,
            model: "test-model".into(),
            backend,
            prompt: "hello".into(),
        }
    }

    #[test]
    fn default_backend_is_the_loosest_the_tier_permits() {
        assert_eq!(
            resolve_backend(&req(ChannelTier::Owned, None, InferencePurpose::AgentTurn)),
            Backend::Local
        );
        assert_eq!(
            resolve_backend(&req(
                ChannelTier::Private,
                None,
                InferencePurpose::AgentTurn
            )),
            Backend::Tee
        );
        assert_eq!(
            resolve_backend(&req(ChannelTier::Open, None, InferencePurpose::AgentTurn)),
            Backend::Vendor
        );
        // Owned-pinned purpose collapses the default to Local everywhere.
        assert_eq!(
            resolve_backend(&req(ChannelTier::Open, None, InferencePurpose::Gate)),
            Backend::Local
        );
        // An explicit choice is honored (the policy gate refuses it later
        // if it's below tier — that's route_and_record's job, not this).
        assert_eq!(
            resolve_backend(&req(
                ChannelTier::Owned,
                Some(Backend::Vendor),
                InferencePurpose::AgentTurn
            )),
            Backend::Vendor
        );
    }
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated: the end-to-end route → refuse/dispatch → meter → budget
    //! behavior against a real `model_usage` table.
    //! Run with:
    //!   `cargo test -p sm-gateway --lib pg_tests -- --ignored`
    use super::*;

    const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz";

    async fn pool() -> sqlx::PgPool {
        let url = std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_owned());
        sqlx::PgPool::connect(&url).await.expect("connect")
    }

    async fn make_community(pool: &sqlx::PgPool) -> CommunityId {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("gw-{}.example", id.simple()))
            .execute(pool)
            .await
            .expect("insert community");
        CommunityId::from_uuid(id)
    }

    fn full_gateway(db: Db) -> Gateway {
        Gateway::new(db)
            .with_backend(stub::local())
            .with_backend(stub::tee())
            .with_backend(stub::vendor())
    }

    fn request(
        community: CommunityId,
        tier: ChannelTier,
        backend: Option<Backend>,
        purpose: InferencePurpose,
    ) -> InferenceRequest {
        InferenceRequest {
            community_id: community,
            user_pubkey: vec![0xaau8; 32],
            agent_pubkey: Some(vec![0xbbu8; 32]),
            channel_id: Some(Uuid::new_v4()),
            thread_id: None,
            tier,
            purpose,
            model: "test-model".into(),
            backend,
            prompt: "please do the thing".into(), // 4 words
        }
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn routes_meters_refuses_and_budgets() {
        let raw_pool = pool().await;
        let community = make_community(&raw_pool).await;
        let db = Db::from_pool(raw_pool.clone());
        let gw = full_gateway(db);

        // Allowed: owned channel, agent turn, default backend → Local.
        // Runs and records exactly one row.
        let resp = gw
            .route_and_record(&request(
                community,
                ChannelTier::Owned,
                None,
                InferencePurpose::AgentTurn,
            ))
            .await
            .expect("owned/local allowed");
        assert_eq!(resp.backend, Backend::Local);
        assert_eq!(resp.prompt_tokens, 4, "4-word prompt");
        assert!(resp.text.contains("local stub"));

        // Refused: owned channel, explicit Vendor → below the channel floor.
        // Records nothing.
        let before =
            buzz_db::model_usage::user_token_spend(&raw_pool, community, &[0xaau8; 32], None)
                .await
                .expect("spend before");
        let refused = gw
            .route_and_record(&request(
                community,
                ChannelTier::Owned,
                Some(Backend::Vendor),
                InferencePurpose::AgentTurn,
            ))
            .await;
        assert!(matches!(
            refused,
            Err(GatewayError::RouteRefused(
                RefuseReason::BelowChannelMinimum { .. }
            ))
        ));
        let after =
            buzz_db::model_usage::user_token_spend(&raw_pool, community, &[0xaau8; 32], None)
                .await
                .expect("spend after refusal");
        assert_eq!(before, after, "a refused request meters nothing");

        // Refused: owned-pinned purpose (gate) with an egress backend, even
        // in an open channel.
        assert!(matches!(
            gw.route_and_record(&request(
                community,
                ChannelTier::Open,
                Some(Backend::Vendor),
                InferencePurpose::Gate
            ))
            .await,
            Err(GatewayError::RouteRefused(
                RefuseReason::OwnedPinnedPurpose { .. }
            ))
        ));

        // Allowed egress: open channel, agent turn, default → Vendor.
        let vendor = gw
            .route_and_record(&request(
                community,
                ChannelTier::Open,
                None,
                InferencePurpose::AgentTurn,
            ))
            .await
            .expect("open/vendor allowed");
        assert_eq!(vendor.backend, Backend::Vendor);

        // The rollup now shows two buckets for this user: owned/local and
        // open/vendor. tee was never used.
        let totals = buzz_db::model_usage::user_usage_totals(&raw_pool, community, None)
            .await
            .expect("totals");
        assert!(totals
            .iter()
            .any(|t| t.tier == "owned" && t.backend == "local"));
        assert!(totals
            .iter()
            .any(|t| t.tier == "open" && t.backend == "vendor"));
        assert!(!totals.iter().any(|t| t.backend == "tee"));

        // Budget: a ceiling below the already-spent total refuses the next
        // request before it runs.
        let spent =
            buzz_db::model_usage::user_token_spend(&raw_pool, community, &[0xaau8; 32], None)
                .await
                .expect("spend");
        let budgeted = full_gateway(Db::from_pool(raw_pool.clone())).with_budget(Budget {
            tokens: spent, // ceiling == spend → next request is over
            window: None,
        });
        let over = budgeted
            .route_and_record(&request(
                community,
                ChannelTier::Owned,
                None,
                InferencePurpose::AgentTurn,
            ))
            .await;
        assert!(matches!(over, Err(GatewayError::BudgetExceeded { .. })));

        // A generous budget lets it through again.
        let ok = full_gateway(Db::from_pool(raw_pool))
            .with_budget(Budget {
                tokens: spent + 1_000_000,
                window: None,
            })
            .route_and_record(&request(
                community,
                ChannelTier::Owned,
                None,
                InferencePurpose::AgentTurn,
            ))
            .await;
        assert!(ok.is_ok());
    }
}
