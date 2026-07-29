//! Agent permission request CRUD -- the `agent_permission_requests` table.
//!
//! Pending human approvals for ACP tool calls made by supervised agents.
//! Mirrors the `workflow_approvals` idioms: community-prefixed keys, SHA-256
//! hashed tokens (never plaintext at rest), TOCTOU-safe pending-only updates
//! (0 rows updated => caller treats as a 409 conflict), and expiry.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use buzz_core::CommunityId;

use crate::error::{DbError, Result};

/// SHA-256 hash of a raw permission token. Returns the 32-byte digest.
///
/// Tokens are stored hashed so a DB read does not expose the raw value
/// (same pattern as workflow approval tokens and relay invites).
pub fn hash_permission_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

// -- Enums --------------------------------------------------------------------

/// Status of an agent permission request.
/// Stored as ENUM('pending','granted','denied','cancelled','expired').
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPermissionStatus {
    /// Waiting on a human decision.
    Pending,
    /// A human granted the request; the tool call may proceed.
    Granted,
    /// A human denied the request; the tool call is refused.
    Denied,
    /// The turn ended (teardown/cancel) before a decision arrived.
    Cancelled,
    /// The decision window elapsed; the agent received `cancelled`.
    Expired,
}

impl fmt::Display for AgentPermissionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentPermissionStatus::Pending => write!(f, "pending"),
            AgentPermissionStatus::Granted => write!(f, "granted"),
            AgentPermissionStatus::Denied => write!(f, "denied"),
            AgentPermissionStatus::Cancelled => write!(f, "cancelled"),
            AgentPermissionStatus::Expired => write!(f, "expired"),
        }
    }
}

impl FromStr for AgentPermissionStatus {
    type Err = DbError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(AgentPermissionStatus::Pending),
            "granted" => Ok(AgentPermissionStatus::Granted),
            "denied" => Ok(AgentPermissionStatus::Denied),
            "cancelled" => Ok(AgentPermissionStatus::Cancelled),
            "expired" => Ok(AgentPermissionStatus::Expired),
            other => Err(DbError::InvalidData(format!(
                "unknown agent permission status: {other}"
            ))),
        }
    }
}

/// The ACP decision recorded on a resolved request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPermissionDecision {
    /// Approve this one tool call.
    AllowOnce,
    /// Approve this and equivalent calls for the rest of the session.
    AllowAlways,
    /// Refuse this tool call; the turn continues.
    RejectOnce,
    /// Abandon the turn (teardown/expiry), distinct from a refusal.
    Cancel,
}

impl fmt::Display for AgentPermissionDecision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentPermissionDecision::AllowOnce => write!(f, "allow_once"),
            AgentPermissionDecision::AllowAlways => write!(f, "allow_always"),
            AgentPermissionDecision::RejectOnce => write!(f, "reject_once"),
            AgentPermissionDecision::Cancel => write!(f, "cancel"),
        }
    }
}

impl FromStr for AgentPermissionDecision {
    type Err = DbError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "allow_once" => Ok(AgentPermissionDecision::AllowOnce),
            "allow_always" => Ok(AgentPermissionDecision::AllowAlways),
            "reject_once" => Ok(AgentPermissionDecision::RejectOnce),
            "cancel" => Ok(AgentPermissionDecision::Cancel),
            other => Err(DbError::InvalidData(format!(
                "unknown agent permission decision: {other}"
            ))),
        }
    }
}

/// Human-facing classification of what the gated tool call does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentPermissionRequestKind {
    /// Shell command execution.
    Command,
    /// Read of a file or resource.
    FileRead,
    /// Write/edit of a file or resource.
    FileChange,
    /// Anything the harness could not classify.
    Other,
}

impl fmt::Display for AgentPermissionRequestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentPermissionRequestKind::Command => write!(f, "command"),
            AgentPermissionRequestKind::FileRead => write!(f, "file-read"),
            AgentPermissionRequestKind::FileChange => write!(f, "file-change"),
            AgentPermissionRequestKind::Other => write!(f, "other"),
        }
    }
}

impl FromStr for AgentPermissionRequestKind {
    type Err = DbError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "command" => Ok(AgentPermissionRequestKind::Command),
            "file-read" => Ok(AgentPermissionRequestKind::FileRead),
            "file-change" => Ok(AgentPermissionRequestKind::FileChange),
            "other" => Ok(AgentPermissionRequestKind::Other),
            other => Err(DbError::InvalidData(format!(
                "unknown agent permission request kind: {other}"
            ))),
        }
    }
}

// -- Record types -------------------------------------------------------------

/// A stored agent permission request.
#[derive(Debug, Clone)]
pub struct AgentPermissionRequestRecord {
    /// Server-resolved community that owns this request.
    pub community_id: CommunityId,
    /// Server-minted request id.
    pub request_id: Uuid,
    /// SHA-256 of the raw token (the kind:46010/46030/46031 `d`-tag handle).
    pub token_hash: Vec<u8>,
    /// Channel the requesting agent is operating in.
    pub channel_id: Uuid,
    /// Pubkey of the requesting agent (32-byte x-only).
    pub agent_pubkey: Vec<u8>,
    /// Opaque harness session/turn reference, if provided.
    pub session_ref: Option<String>,
    /// Human-facing classification of the gated call.
    pub request_kind: AgentPermissionRequestKind,
    /// Tool name as reported by the agent, if known.
    pub tool_name: Option<String>,
    /// Pre-rendered human-facing summary (<= 400 chars).
    pub detail: String,
    /// Full tool input payload for approver inspection.
    pub payload: Option<serde_json::Value>,
    /// The ACP option list the agent offered.
    pub options_offered: serde_json::Value,
    /// Current status.
    pub status: AgentPermissionStatus,
    /// The recorded decision, once resolved.
    pub decision: Option<AgentPermissionDecision>,
    /// Pubkey of the human who decided, once resolved.
    pub decider_pubkey: Option<Vec<u8>>,
    /// Optional note from the decider.
    pub note: Option<String>,
    /// When the request was created.
    pub created_at: DateTime<Utc>,
    /// When the request expires.
    pub expires_at: DateTime<Utc>,
    /// When the request was resolved (granted/denied/cancelled/expired).
    pub resolved_at: Option<DateTime<Utc>>,
}

/// Parameters for creating a new agent permission request.
pub struct CreateAgentPermissionRequestParams<'a> {
    /// Server-resolved community that owns this request.
    pub community_id: CommunityId,
    /// Raw token (hashed before storage; never persisted in the clear).
    pub token: &'a str,
    /// Channel the requesting agent is operating in.
    pub channel_id: Uuid,
    /// Pubkey of the requesting agent (32-byte x-only).
    pub agent_pubkey: &'a [u8],
    /// Opaque harness session/turn reference.
    pub session_ref: Option<&'a str>,
    /// Human-facing classification of the gated call.
    pub request_kind: AgentPermissionRequestKind,
    /// Tool name as reported by the agent.
    pub tool_name: Option<&'a str>,
    /// Pre-rendered human-facing summary (<= 400 chars, enforced by the DB).
    pub detail: &'a str,
    /// Full tool input payload.
    pub payload: Option<&'a serde_json::Value>,
    /// The ACP option list the agent offered.
    pub options_offered: &'a serde_json::Value,
    /// When the request expires.
    pub expires_at: DateTime<Utc>,
}

// -- CRUD ---------------------------------------------------------------------

/// Insert a new agent permission request. Returns the server-minted
/// `request_id`.
pub async fn create_agent_permission_request(
    pool: &PgPool,
    params: CreateAgentPermissionRequestParams<'_>,
) -> Result<Uuid> {
    let CreateAgentPermissionRequestParams {
        community_id,
        token,
        channel_id,
        agent_pubkey,
        session_ref,
        request_kind,
        tool_name,
        detail,
        payload,
        options_offered,
        expires_at,
    } = params;
    let token_hash = hash_permission_token(token);

    let row = sqlx::query(
        r#"
        INSERT INTO agent_permission_requests
            (community_id, token, channel_id, agent_pubkey, session_ref,
             request_kind, tool_name, detail, payload, options_offered,
             status, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 'pending', $11)
        RETURNING request_id
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(token_hash)
    .bind(channel_id)
    .bind(agent_pubkey)
    .bind(session_ref)
    .bind(request_kind.to_string())
    .bind(tool_name)
    .bind(detail)
    .bind(payload)
    .bind(options_offered)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;

    Ok(row.try_get("request_id")?)
}

/// Fetch a request by its already-hashed token value.
///
/// The lookup binds the server-resolved community alongside the token so the
/// same hash can never resolve across communities.
pub async fn get_agent_permission_by_stored_hash(
    pool: &PgPool,
    community_id: CommunityId,
    token_hash: &[u8],
) -> Result<AgentPermissionRequestRecord> {
    let row = sqlx::query(
        r#"
        SELECT community_id, request_id, token, channel_id, agent_pubkey, session_ref,
               request_kind, tool_name, detail, payload, options_offered,
               status::text AS status, decision, decider_pubkey, note,
               created_at, expires_at, resolved_at
        FROM agent_permission_requests
        WHERE community_id = $1 AND token = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(token_hash)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DbError::NotFound("agent permission request (hashed token)".to_string()))?;

    row_to_record(row)
}

/// Fetch a request by its server-minted id.
pub async fn get_agent_permission_request(
    pool: &PgPool,
    community_id: CommunityId,
    request_id: Uuid,
) -> Result<AgentPermissionRequestRecord> {
    let row = sqlx::query(
        r#"
        SELECT community_id, request_id, token, channel_id, agent_pubkey, session_ref,
               request_kind, tool_name, detail, payload, options_offered,
               status::text AS status, decision, decider_pubkey, note,
               created_at, expires_at, resolved_at
        FROM agent_permission_requests
        WHERE community_id = $1 AND request_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(request_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| DbError::NotFound(format!("agent permission request {request_id}")))?;

    row_to_record(row)
}

/// List requests in the given channels, optionally filtered by status,
/// newest first. `channel_ids` is the caller's membership-scoped visibility
/// set — an empty slice returns nothing (fail-closed).
pub async fn list_agent_permission_requests(
    pool: &PgPool,
    community_id: CommunityId,
    channel_ids: &[Uuid],
    status: Option<AgentPermissionStatus>,
    limit: i64,
) -> Result<Vec<AgentPermissionRequestRecord>> {
    if channel_ids.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, super::workflow::LIST_MAX_LIMIT);

    let rows = sqlx::query(
        r#"
        SELECT community_id, request_id, token, channel_id, agent_pubkey, session_ref,
               request_kind, tool_name, detail, payload, options_offered,
               status::text AS status, decision, decider_pubkey, note,
               created_at, expires_at, resolved_at
        FROM agent_permission_requests
        WHERE community_id = $1 AND channel_id = ANY($2)
          AND ($3::text IS NULL OR status = $3::agent_permission_status)
        ORDER BY created_at DESC
        LIMIT $4
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_ids)
    .bind(status.map(|s| s.to_string()))
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_record).collect()
}

/// Resolve a pending request: set status, decision, decider, and note, and
/// stamp `resolved_at`.
///
/// # TOCTOU safety
/// The predicate includes `AND status = 'pending'` so two concurrent
/// decisions cannot both succeed. Returns `Ok(false)` when the request was
/// already acted on — callers should treat that as a conflict (HTTP 409).
pub async fn resolve_agent_permission_by_stored_hash(
    pool: &PgPool,
    community_id: CommunityId,
    token_hash: &[u8],
    status: AgentPermissionStatus,
    decision: Option<AgentPermissionDecision>,
    decider_pubkey: Option<&[u8]>,
    note: Option<&str>,
) -> Result<bool> {
    let affected = sqlx::query(
        r#"
        UPDATE agent_permission_requests
        SET status         = $1::agent_permission_status,
            decision       = $2,
            decider_pubkey = $3,
            note           = $4,
            resolved_at    = NOW()
        WHERE community_id = $5 AND token = $6 AND status = 'pending'
        "#,
    )
    .bind(status.to_string())
    .bind(decision.map(|d| d.to_string()))
    .bind(decider_pubkey)
    .bind(note)
    .bind(community_id.as_uuid())
    .bind(token_hash)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(affected > 0)
}

// -- Row mapper ---------------------------------------------------------------

fn row_to_record(row: sqlx::postgres::PgRow) -> Result<AgentPermissionRequestRecord> {
    let community_id: Uuid = row.try_get("community_id")?;

    let status_str: String = row.try_get("status")?;
    let status = status_str.parse::<AgentPermissionStatus>()?;

    let request_kind_str: String = row.try_get("request_kind")?;
    let request_kind = request_kind_str.parse::<AgentPermissionRequestKind>()?;

    let decision_str: Option<String> = row.try_get("decision")?;
    let decision = decision_str
        .map(|d| d.parse::<AgentPermissionDecision>())
        .transpose()?;

    Ok(AgentPermissionRequestRecord {
        community_id: CommunityId::from_uuid(community_id),
        request_id: row.try_get("request_id")?,
        token_hash: row.try_get("token")?,
        channel_id: row.try_get("channel_id")?,
        agent_pubkey: row.try_get("agent_pubkey")?,
        session_ref: row.try_get("session_ref")?,
        request_kind,
        tool_name: row.try_get("tool_name")?,
        detail: row.try_get("detail")?,
        payload: row.try_get("payload")?,
        options_offered: row.try_get("options_offered")?,
        status,
        decision,
        decider_pubkey: row.try_get("decider_pubkey")?,
        note: row.try_get("note")?,
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        resolved_at: row.try_get("resolved_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for status in [
            AgentPermissionStatus::Pending,
            AgentPermissionStatus::Granted,
            AgentPermissionStatus::Denied,
            AgentPermissionStatus::Cancelled,
            AgentPermissionStatus::Expired,
        ] {
            assert_eq!(
                status.to_string().parse::<AgentPermissionStatus>().unwrap(),
                status
            );
        }
        assert!("bogus".parse::<AgentPermissionStatus>().is_err());
    }

    #[test]
    fn decision_round_trips() {
        for decision in [
            AgentPermissionDecision::AllowOnce,
            AgentPermissionDecision::AllowAlways,
            AgentPermissionDecision::RejectOnce,
            AgentPermissionDecision::Cancel,
        ] {
            assert_eq!(
                decision
                    .to_string()
                    .parse::<AgentPermissionDecision>()
                    .unwrap(),
                decision
            );
        }
        assert!("allow".parse::<AgentPermissionDecision>().is_err());
    }

    #[test]
    fn request_kind_round_trips() {
        for kind in [
            AgentPermissionRequestKind::Command,
            AgentPermissionRequestKind::FileRead,
            AgentPermissionRequestKind::FileChange,
            AgentPermissionRequestKind::Other,
        ] {
            assert_eq!(
                kind.to_string()
                    .parse::<AgentPermissionRequestKind>()
                    .unwrap(),
                kind
            );
        }
        assert!("shell".parse::<AgentPermissionRequestKind>().is_err());
    }

    #[test]
    fn token_hash_is_sha256() {
        let hash = hash_permission_token("tok");
        assert_eq!(hash.len(), 32);
        assert_eq!(hash, Sha256::digest(b"tok").to_vec());
    }
}

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated round-trip tests. Run with:
    //!   `cargo test -p buzz-db --lib agent_permission -- --ignored`
    use super::*;
    use chrono::Duration;

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
        let host = format!("test-{}.example", id.simple());
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(id)
            .bind(&host)
            .execute(pool)
            .await
            .expect("insert community");
        CommunityId::from_uuid(id)
    }

    async fn make_channel(pool: &PgPool, community: CommunityId) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO channels (id, community_id, name, created_by)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(id)
        .bind(community.as_uuid())
        .bind(format!("ch-{}", id.simple()))
        .bind(vec![0xa1u8; 32])
        .execute(pool)
        .await
        .expect("insert channel");
        id
    }

    fn create_params<'a>(
        community: CommunityId,
        token: &'a str,
        channel_id: Uuid,
        agent_pubkey: &'a [u8],
        payload: &'a serde_json::Value,
        options: &'a serde_json::Value,
        expires_at: DateTime<Utc>,
    ) -> CreateAgentPermissionRequestParams<'a> {
        CreateAgentPermissionRequestParams {
            community_id: community,
            token,
            channel_id,
            agent_pubkey,
            session_ref: Some("sess-1/turn-3"),
            request_kind: AgentPermissionRequestKind::Command,
            tool_name: Some("shell"),
            detail: "Run `cargo test`",
            payload: Some(payload),
            options_offered: options,
            expires_at,
        }
    }

    /// Create → fetch (by hash and by id) → list (channel-scoped) →
    /// resolve (TOCTOU) → community confinement, in one connected flow.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn round_trip_resolve_and_confinement() {
        let pool = setup_pool().await;
        let community_a = make_community(&pool).await;
        let community_b = make_community(&pool).await;
        let channel_a = make_channel(&pool, community_a).await;
        let other_channel_a = make_channel(&pool, community_a).await;

        let agent_pubkey = vec![0x7cu8; 32];
        let payload = serde_json::json!({ "command": "cargo test" });
        let options = serde_json::json!([
            { "optionId": "yes", "kind": "allow_once" },
            { "optionId": "no", "kind": "reject_once" },
        ]);
        let token = format!("tok-{}", Uuid::new_v4());
        let expires_at = Utc::now() + Duration::minutes(15);

        let request_id = create_agent_permission_request(
            &pool,
            create_params(
                community_a,
                &token,
                channel_a,
                &agent_pubkey,
                &payload,
                &options,
                expires_at,
            ),
        )
        .await
        .expect("create request");

        // Fetch by hashed token round-trips every field.
        let hash = hash_permission_token(&token);
        let rec = get_agent_permission_by_stored_hash(&pool, community_a, &hash)
            .await
            .expect("fetch by hash");
        assert_eq!(rec.request_id, request_id);
        assert_eq!(rec.channel_id, channel_a);
        assert_eq!(rec.agent_pubkey, agent_pubkey);
        assert_eq!(rec.session_ref.as_deref(), Some("sess-1/turn-3"));
        assert_eq!(rec.request_kind, AgentPermissionRequestKind::Command);
        assert_eq!(rec.tool_name.as_deref(), Some("shell"));
        assert_eq!(rec.detail, "Run `cargo test`");
        assert_eq!(rec.payload, Some(payload.clone()));
        assert_eq!(rec.options_offered, options);
        assert_eq!(rec.status, AgentPermissionStatus::Pending);
        assert!(rec.decision.is_none());
        assert!(rec.resolved_at.is_none());

        // Fetch by id agrees.
        let by_id = get_agent_permission_request(&pool, community_a, request_id)
            .await
            .expect("fetch by id");
        assert_eq!(by_id.token_hash, hash);

        // List is channel-scoped and fail-closed on an empty visibility set.
        let listed = list_agent_permission_requests(
            &pool,
            community_a,
            &[channel_a],
            Some(AgentPermissionStatus::Pending),
            10,
        )
        .await
        .expect("list");
        assert!(listed.iter().any(|r| r.request_id == request_id));
        let other =
            list_agent_permission_requests(&pool, community_a, &[other_channel_a], None, 10)
                .await
                .expect("list other channel");
        assert!(other.iter().all(|r| r.request_id != request_id));
        let none = list_agent_permission_requests(&pool, community_a, &[], None, 10)
            .await
            .expect("list no channels");
        assert!(none.is_empty(), "empty visibility set must return nothing");

        // Community confinement: the same hash resolves nothing under B.
        assert!(
            get_agent_permission_by_stored_hash(&pool, community_b, &hash)
                .await
                .is_err(),
            "token hash must not resolve across communities"
        );

        // Resolve grants once; the second decision loses (TOCTOU).
        let decider = vec![0x99u8; 32];
        let first = resolve_agent_permission_by_stored_hash(
            &pool,
            community_a,
            &hash,
            AgentPermissionStatus::Granted,
            Some(AgentPermissionDecision::AllowOnce),
            Some(&decider),
            Some("looks safe"),
        )
        .await
        .expect("first resolve");
        assert!(first, "first decision must win");
        let second = resolve_agent_permission_by_stored_hash(
            &pool,
            community_a,
            &hash,
            AgentPermissionStatus::Denied,
            Some(AgentPermissionDecision::RejectOnce),
            Some(&decider),
            None,
        )
        .await
        .expect("second resolve");
        assert!(!second, "second decision must conflict");

        let resolved = get_agent_permission_request(&pool, community_a, request_id)
            .await
            .expect("fetch resolved");
        assert_eq!(resolved.status, AgentPermissionStatus::Granted);
        assert_eq!(resolved.decision, Some(AgentPermissionDecision::AllowOnce));
        assert_eq!(resolved.decider_pubkey, Some(decider));
        assert_eq!(resolved.note.as_deref(), Some("looks safe"));
        assert!(resolved.resolved_at.is_some());
    }
}
