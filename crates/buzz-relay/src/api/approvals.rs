//! Agent permission approval API — the HTTP surface of the approvals series.
//!
//! `POST /api/approvals` — a supervised agent harness registers a pending
//! permission request (NIP-98 signed by the agent key). The full tool input
//! stays in the row for approver inspection; the channel gets a relay-signed
//! kind:46010 notification carrying the summary only.
//!
//! `GET /api/approvals` — membership-scoped listing for the CLI and clients.
//! Callers see only requests from channels they can access.
//!
//! Grant/deny stays on the existing signed-command path (kind:46030/46031
//! through `POST /events`), which resolves the shared `d` token-hash tag —
//! see `handlers::command_executor`.

use std::sync::Arc;

use axum::extract::{Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use chrono::Utc;
use nostr::{EventBuilder, Kind, Tag};
use serde_json::Value;
use uuid::Uuid;

use buzz_core::kind::{KIND_WORKFLOW_APPROVAL_REQUESTED, KIND_WORKFLOW_APPROVAL_WITHDRAWN};
use buzz_core::TenantContext;
use buzz_db::agent_permission::{
    hash_permission_token, AgentPermissionRequestKind, AgentPermissionRequestRecord,
    AgentPermissionStatus, CreateAgentPermissionRequestParams,
};

use super::bridge::{check_nip98_replay, nip98_expected_url, verify_bridge_auth};
use super::relay_members::enforce_relay_membership;
use super::{api_error, internal_error};
use crate::handlers::event::dispatch_persistent_event;
use crate::state::AppState;

/// Default decision window when the harness does not specify one (15 min).
const DEFAULT_TTL_SECS: u64 = 900;
/// Ceiling on the requested decision window (24 h).
const MAX_TTL_SECS: u64 = 86_400;
/// Cap on rows returned by a single list read.
const LIST_LIMIT: i64 = 200;

/// Body of `POST /api/approvals`.
#[derive(serde::Deserialize)]
pub struct CreateApprovalBody {
    /// Channel the requesting agent is operating in.
    channel_id: Uuid,
    /// Opaque harness session/turn reference.
    #[serde(default)]
    session_ref: Option<String>,
    /// `command` | `file-read` | `file-change` | `other`.
    request_kind: String,
    /// Tool name as reported by the agent.
    #[serde(default)]
    tool_name: Option<String>,
    /// Pre-rendered human-facing summary (<= 400 chars).
    detail: String,
    /// Full tool input payload (stored for approver inspection only).
    #[serde(default)]
    payload: Option<Value>,
    /// The ACP option list the agent offered.
    options: Value,
    /// Decision window in seconds (default 900, capped at 86400).
    #[serde(default)]
    ttl_secs: Option<u64>,
}

/// Optional `?status=` / `?channel=` / `?limit=` query for the list read.
#[derive(serde::Deserialize, Default)]
pub struct ListApprovalsQuery {
    pub(crate) status: Option<String>,
    pub(crate) channel: Option<Uuid>,
    pub(crate) limit: Option<i64>,
}

/// Shared prelude: bind the tenant from the Host header, verify NIP-98 (or
/// dev X-Pubkey when the deployment allows it), check replay, and enforce
/// relay membership.
async fn authenticate(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    method: &str,
    path_with_query: &str,
    body: Option<&[u8]>,
) -> Result<(TenantContext, nostr::PublicKey), (StatusCode, Json<Value>)> {
    let raw_host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let tenant = crate::tenant::bind_community(&state.db, raw_host)
        .await
        .map_err(|_| {
            api_error(
                StatusCode::NOT_FOUND,
                "relay: no community is configured for this host",
            )
        })?;

    let url = nip98_expected_url(&state.config.relay_url, &tenant, path_with_query);
    let (pubkey, event_id_bytes) =
        verify_bridge_auth(headers, method, &url, body, state.config.require_auth_token)?;
    check_nip98_replay(state, &tenant, event_id_bytes).await?;

    let auth_tag = headers.get("x-auth-tag").and_then(|v| v.to_str().ok());
    enforce_relay_membership(state, tenant.community(), &pubkey.to_bytes(), auth_tag).await?;

    Ok((tenant, pubkey))
}

/// `POST /api/approvals` — register a pending agent permission request.
///
/// Signed by the requesting agent's key; the agent must be a member of the
/// target channel. Mints the server-side request id and token, stores the
/// row, and emits the kind:46010 notification into the channel.
pub async fn create_approval(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (tenant, agent_pubkey) =
        authenticate(&state, &headers, "POST", "/api/approvals", Some(&body)).await?;

    let body: CreateApprovalBody = serde_json::from_slice(&body)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")))?;

    let request_kind: AgentPermissionRequestKind = body
        .request_kind
        .parse()
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid request_kind"))?;
    if body.detail.trim().is_empty() || body.detail.chars().count() > 400 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "detail must be 1..=400 characters",
        ));
    }
    if !body.options.is_array() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "options must be the ACP option array",
        ));
    }

    // The requesting agent must be a member of the channel it is working in.
    let channel = state
        .db
        .get_channel(tenant.community(), body.channel_id)
        .await
        .map_err(|_| api_error(StatusCode::NOT_FOUND, "channel not found"))?;
    if channel.archived_at.is_some() {
        return Err(api_error(StatusCode::CONFLICT, "channel is archived"));
    }
    let agent_bytes = agent_pubkey.to_bytes().to_vec();
    let is_member = state
        .is_member_cached(tenant.community(), body.channel_id, &agent_bytes)
        .await
        .map_err(|e| internal_error(&format!("membership lookup: {e}")))?;
    if !is_member {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "agent is not a member of the channel",
        ));
    }

    let ttl_secs = body
        .ttl_secs
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(1, MAX_TTL_SECS);
    let expires_at = Utc::now() + chrono::Duration::seconds(ttl_secs as i64);
    let token = Uuid::new_v4().to_string();

    let request_id = state
        .db
        .create_agent_permission_request(CreateAgentPermissionRequestParams {
            community_id: tenant.community(),
            token: &token,
            channel_id: body.channel_id,
            agent_pubkey: &agent_bytes,
            session_ref: body.session_ref.as_deref(),
            request_kind,
            tool_name: body.tool_name.as_deref(),
            detail: &body.detail,
            payload: body.payload.as_ref(),
            options_offered: &body.options,
            expires_at,
        })
        .await
        .map_err(|e| internal_error(&format!("create agent permission request: {e}")))?;

    // Announce into the channel — summary only; the row carries the payload.
    // Best-effort: the row is the source of truth and stays approvable via
    // the list surface even if the notification fails.
    let token_hash_hex = hex::encode(hash_permission_token(&token));
    let event_id = emit_agent_approval_requested(
        &state,
        &tenant,
        &AgentApprovalNotice {
            request_id,
            channel_id: body.channel_id,
            agent_pubkey_hex: agent_pubkey.to_hex(),
            token_hash_hex: token_hash_hex.clone(),
            request_kind,
            tool_name: body.tool_name.clone(),
            detail: body.detail.clone(),
            expires_at,
        },
    )
    .await;

    Ok(Json(serde_json::json!({
        "request_id": request_id,
        "token_hash": token_hash_hex,
        "expires_at": expires_at.to_rfc3339(),
        "event_id": event_id,
    })))
}

/// `GET /api/approvals` — list requests visible to the caller.
///
/// Scope is the caller's accessible channels (same enforcement as the WS REQ
/// handler); `?channel=` narrows within it, `?status=` filters, `?limit=`
/// caps (max 200).
pub async fn list_approvals(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    Query(q): Query<ListApprovalsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path_with_query = match raw_query.as_deref() {
        Some(query) if !query.is_empty() => format!("/api/approvals?{query}"),
        _ => "/api/approvals".to_owned(),
    };
    let (tenant, pubkey) = authenticate(&state, &headers, "GET", &path_with_query, None).await?;

    let status = q
        .status
        .as_deref()
        .map(str::parse::<AgentPermissionStatus>)
        .transpose()
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "invalid status filter"))?;
    let limit = q
        .limit
        .filter(|n| *n > 0)
        .map(|n| n.min(LIST_LIMIT))
        .unwrap_or(LIST_LIMIT);

    let accessible = state
        .get_accessible_channel_ids_cached(tenant.community(), &pubkey.to_bytes())
        .await
        .map_err(|e| internal_error(&format!("channel access lookup: {e}")))?;
    let channels: Vec<Uuid> = match q.channel {
        // An inaccessible ?channel= narrows to nothing rather than erroring —
        // no existence oracle.
        Some(ch) => accessible.into_iter().filter(|c| *c == ch).collect(),
        None => accessible,
    };

    let rows = state
        .db
        .list_agent_permission_requests(tenant.community(), &channels, status, limit)
        .await
        .map_err(|e| internal_error(&format!("list agent permission requests: {e}")))?;

    Ok(Json(Value::Array(
        rows.iter().map(record_to_json).collect(),
    )))
}

/// Body of `POST /api/approvals/resolve`.
#[derive(serde::Deserialize)]
pub struct ResolveApprovalBody {
    /// The request to resolve.
    request_id: Uuid,
    /// `cancelled` (turn torn down) or `expired` (decision window elapsed).
    outcome: String,
}

/// `POST /api/approvals/resolve` — the requesting harness withdraws its own
/// pending request as `cancelled` (turn teardown) or `expired` (decision
/// window elapsed). Signed by the same agent key that created the request;
/// grant/deny remain exclusively on the human-signed kind:46030/46031 path.
///
/// Responds `409` when the request was already decided (the harness then
/// reads the decision), `404` when it does not exist.
pub async fn resolve_approval(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    use buzz_db::agent_permission::AgentPermissionDecision;

    let (tenant, agent_pubkey) = authenticate(
        &state,
        &headers,
        "POST",
        "/api/approvals/resolve",
        Some(&body),
    )
    .await?;

    let body: ResolveApprovalBody = serde_json::from_slice(&body)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")))?;
    let status = match body.outcome.as_str() {
        "cancelled" => AgentPermissionStatus::Cancelled,
        "expired" => AgentPermissionStatus::Expired,
        _ => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "outcome must be 'cancelled' or 'expired'",
            ));
        }
    };

    let request = state
        .db
        .get_agent_permission_request(tenant.community(), body.request_id)
        .await
        .map_err(|_| api_error(StatusCode::NOT_FOUND, "request not found"))?;
    if request.agent_pubkey != agent_pubkey.to_bytes().to_vec() {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "only the requesting agent may withdraw its request",
        ));
    }

    let updated = state
        .db
        .resolve_agent_permission_by_stored_hash(
            tenant.community(),
            &request.token_hash,
            status,
            Some(AgentPermissionDecision::Cancel),
            None,
            None,
        )
        .await
        .map_err(|e| internal_error(&format!("resolve agent permission request: {e}")))?;
    if !updated {
        return Err(api_error(StatusCode::CONFLICT, "request already resolved"));
    }

    // Say so in the channel. Updating the row and emitting nothing left an
    // event-driven client with no way to learn the request was dead: it kept
    // rendering as pending, and a member acting on it got a refusal instead
    // of an action. The decision path right beside this one has always
    // written its outcome; this completes the symmetry.
    //
    // Best-effort, like the other two: the row is the source of truth, and a
    // failure to announce must not undo a resolution that already committed.
    emit_agent_approval_withdrawn(
        &state,
        &tenant,
        &request,
        status,
        hex::encode(&request.token_hash),
    )
    .await;

    Ok(Json(serde_json::json!({
        "request_id": body.request_id,
        "status": status.to_string(),
    })))
}

/// Emit the relay-signed kind:46013 for a withdrawn or lapsed request.
///
/// Same correlation handle as the rest of the series — the `d` token hash —
/// so a client folds it onto the request it retires without needing to know
/// anything new.
async fn emit_agent_approval_withdrawn(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    request: &AgentPermissionRequestRecord,
    status: AgentPermissionStatus,
    token_hash_hex: String,
) {
    let channel_str = request.channel_id.to_string();
    let agent_hex = hex::encode(&request.agent_pubkey);
    let tags: Result<Vec<Tag>, _> = [
        ["d", token_hash_hex.as_str()],
        ["h", channel_str.as_str()],
        ["p", agent_hex.as_str()],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect();
    let tags = match tags {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("agent approval withdrawal: tag build failed: {e}");
            return;
        }
    };

    let content = serde_json::json!({
        "domain": "agent",
        "request_id": request.request_id,
        "status": status.to_string(),
    });

    let event = match EventBuilder::new(
        Kind::Custom(KIND_WORKFLOW_APPROVAL_WITHDRAWN as u16),
        content.to_string(),
    )
    .tags(tags)
    .sign_with_keys(&state.relay_keypair)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("agent approval withdrawal: signing failed: {e}");
            return;
        }
    };

    match state
        .db
        .insert_event(tenant.community(), &event, Some(request.channel_id))
        .await
    {
        Ok((stored, true)) => {
            let _ = dispatch_persistent_event(
                tenant,
                state,
                &stored,
                KIND_WORKFLOW_APPROVAL_WITHDRAWN,
                &state.relay_keypair.public_key().to_hex(),
                None,
            )
            .await;
        }
        Ok((_, false)) => {}
        Err(e) => tracing::warn!("agent approval withdrawal: persist failed: {e}"),
    }
}

/// JSON shape for one request row. The payload is included — the read is
/// already membership-scoped, and approvers decide from it.
fn record_to_json(rec: &AgentPermissionRequestRecord) -> Value {
    serde_json::json!({
        "request_id": rec.request_id,
        "token_hash": hex::encode(&rec.token_hash),
        "channel_id": rec.channel_id,
        "agent_pubkey": hex::encode(&rec.agent_pubkey),
        "session_ref": rec.session_ref,
        "request_kind": rec.request_kind.to_string(),
        "tool_name": rec.tool_name,
        "detail": rec.detail,
        "payload": rec.payload,
        "options": rec.options_offered,
        "status": rec.status.to_string(),
        "decision": rec.decision.map(|d| d.to_string()),
        "decider_pubkey": rec.decider_pubkey.as_ref().map(hex::encode),
        "note": rec.note,
        "created_at": rec.created_at.to_rfc3339(),
        "expires_at": rec.expires_at.to_rfc3339(),
        "resolved_at": rec.resolved_at.map(|t| t.to_rfc3339()),
    })
}

/// Data for the kind:46010 agent-domain notification.
struct AgentApprovalNotice {
    request_id: Uuid,
    channel_id: Uuid,
    agent_pubkey_hex: String,
    token_hash_hex: String,
    request_kind: AgentPermissionRequestKind,
    tool_name: Option<String>,
    detail: String,
    expires_at: chrono::DateTime<Utc>,
}

/// Emit the relay-signed kind:46010 for an agent permission request.
///
/// Tags: `d` = token hash hex (the kind:46030/46031 correlation handle,
/// tag-discriminating the agent domain from workflow approvals via the
/// `request_id` in content), `h` = channel, `p` = the requesting agent.
/// Content is the summary only — never the tool payload.
///
/// Returns the event id hex, or `None` when emission failed (logged).
async fn emit_agent_approval_requested(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    notice: &AgentApprovalNotice,
) -> Option<String> {
    let channel_str = notice.channel_id.to_string();
    let tags: Result<Vec<Tag>, _> = [
        ["d", notice.token_hash_hex.as_str()],
        ["h", channel_str.as_str()],
        ["p", notice.agent_pubkey_hex.as_str()],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect();
    let tags = match tags {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("agent approval notice: tag build failed: {e}");
            return None;
        }
    };

    let content = serde_json::json!({
        "domain": "agent",
        "request_id": notice.request_id,
        "request_kind": notice.request_kind.to_string(),
        "tool_name": notice.tool_name,
        "detail": notice.detail,
        "expires_at": notice.expires_at.to_rfc3339(),
    });

    let kind = Kind::Custom(KIND_WORKFLOW_APPROVAL_REQUESTED as u16);
    let event = match EventBuilder::new(kind, content.to_string())
        .tags(tags)
        .sign_with_keys(&state.relay_keypair)
    {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("agent approval notice: signing failed: {e}");
            return None;
        }
    };
    let event_id_hex = event.id.to_hex();

    match state
        .db
        .insert_event(tenant.community(), &event, Some(notice.channel_id))
        .await
    {
        Ok((stored, true)) => {
            let _ = dispatch_persistent_event(
                tenant,
                state,
                &stored,
                KIND_WORKFLOW_APPROVAL_REQUESTED,
                &state.relay_keypair.public_key().to_hex(),
                None,
            )
            .await;
            Some(event_id_hex)
        }
        Ok((_, false)) => Some(event_id_hex),
        Err(e) => {
            tracing::warn!("agent approval notice: persist failed: {e}");
            None
        }
    }
}
