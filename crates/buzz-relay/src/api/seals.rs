//! The Content Seal registry endpoint (Silent Mesh D31, slice 2).
//!
//! `POST /api/seals` — the workspace Owner declares that one exact literal
//! may only appear in channels at or above a minimum tier.
//!
//! This is an HTTP endpoint, not an event kind, for one load-bearing
//! reason: **the literal must reach the relay without ever being an
//! event**. Events fan out to every member of every tier, so a literal in
//! any event would republish the value the seal exists to contain. The
//! same shape as the approvals API: the sensitive value travels over
//! NIP-98-authenticated HTTP into the relay's own store, and what fans out
//! is a relay-signed record carrying everything *except* the value — here
//! a kind:47100 with the seal id, label and tier.
//!
//! Owner-only (D42: seals are exclusively the workspace Owner's). The
//! check is the `relay_members` workspace role, the same source
//! `resolve_authority` uses for thread close/archive authority.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use nostr::{EventBuilder, Kind, Tag};
use serde_json::Value;
use uuid::Uuid;

use buzz_core::channel::ChannelTier;
use buzz_core::kind::KIND_SEAL_ANNOUNCE;
use buzz_core::TenantContext;

use super::bridge::{check_nip98_replay, nip98_expected_url, verify_bridge_auth};
use super::relay_members::enforce_relay_membership;
use super::{api_error, internal_error};
use crate::handlers::event::dispatch_persistent_event;
use crate::state::AppState;

#[derive(serde::Deserialize)]
struct CreateSealBody {
    /// What refusals will call it — must itself be safe to say anywhere.
    label: String,
    /// The value being sealed. Stored server-side; never echoed back.
    literal: String,
    /// The loosest tier the literal may appear in: `owned` or `private`.
    min_tier: String,
}

async fn authenticate(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    method: &str,
    path: &str,
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
    let url = nip98_expected_url(&state.config.relay_url, &tenant, path);
    let (pubkey, event_id_bytes) =
        verify_bridge_auth(headers, method, &url, body, state.config.require_auth_token)?;
    check_nip98_replay(state, &tenant, event_id_bytes).await?;
    let auth_tag = headers.get("x-auth-tag").and_then(|v| v.to_str().ok());
    enforce_relay_membership(state, tenant.community(), &pubkey.to_bytes(), auth_tag).await?;
    Ok((tenant, pubkey))
}

/// `POST /api/seals` — create a seal. Owner-only.
pub async fn create_seal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (tenant, pubkey) =
        authenticate(&state, &headers, "POST", "/api/seals", Some(&body)).await?;

    // Owner-only, and by the workspace role rather than any channel role: a
    // seal binds every channel in the workspace, so no lesser authority can
    // create one (D42).
    let creator_hex = pubkey.to_hex();
    let is_owner = state
        .db
        .get_relay_member(tenant.community(), &creator_hex)
        .await
        .map_err(|e| internal_error(&format!("relay member lookup: {e}")))?
        .is_some_and(|m| m.role == "owner");
    if !is_owner {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "sealing is the workspace Owner's alone",
        ));
    }

    let body: CreateSealBody = serde_json::from_slice(&body)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")))?;

    let label = body.label.trim();
    if label.is_empty() || label.chars().count() > 120 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "label must be 1..=120 characters",
        ));
    }
    // The label travels in refusals shown in channels the literal is barred
    // from — a label containing the literal would leak through the refusal.
    if !body.literal.is_empty() && label.contains(&body.literal) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "label must not contain the sealed value — refusals say the label out loud",
        ));
    }
    if body.literal.is_empty() || body.literal.len() > 512 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "literal must be 1..=512 bytes",
        ));
    }
    let min_tier: ChannelTier = body.min_tier.parse().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "min_tier must be owned|private|open",
        )
    })?;
    if min_tier == ChannelTier::Open {
        // Open is the loosest tier: a seal at `open` permits the literal
        // everywhere, i.e. seals nothing. Refusing it keeps "there is a
        // seal" meaning "somewhere this is contained".
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "min_tier open would seal nothing — use owned or private",
        ));
    }

    // 16 lowercase hex from a v4 uuid's first 8 bytes: unguessable enough
    // for a reference (it is not a secret), short enough to read in a token.
    let id: String = Uuid::new_v4().simple().to_string()[..16].to_string();

    state
        .db
        .create_seal(buzz_db::seal::CreateSealParams {
            community_id: tenant.community(),
            id: &id,
            label,
            literal: &body.literal,
            min_tier,
            created_by: &pubkey.to_bytes(),
        })
        .await
        .map_err(|e| internal_error(&format!("seal insert: {e}")))?;

    emit_seal_announce(&state, &tenant, &id, label, min_tier, &creator_hex).await;

    // The workspace sweep (D31). A seal binds what happens next; it does
    // nothing about what is already stored, and without telling the Owner
    // so, creating one implies a containment it has not delivered. So the
    // creation response says plainly where the value already is.
    //
    // Best-effort, and deliberately after the seal is stored and announced:
    // a sweep that fails must not unwind a seal that is already enforcing.
    // A missing report reads as "unknown", never as "clean".
    let sweep = match state
        .db
        .sweep_sealed_literal(tenant.community(), &body.literal, min_tier, SWEEP_LIMIT)
        .await
    {
        Ok(report) => serde_json::to_value(&report).ok(),
        Err(e) => {
            tracing::warn!("seal sweep failed for {id}: {e}");
            None
        }
    };

    Ok(Json(serde_json::json!({
        "seal_id": id,
        "token": buzz_core::seal::token(&id),
        // `null` when the sweep could not run — distinguishable from a
        // sweep that ran and found nothing (`occurrences: 0`).
        "sweep": sweep,
    })))
}

/// How many matching events one sweep will look at. A substring scan cannot
/// use an index, so this is a latency ceiling on seal creation rather than a
/// judgement about how much exposure matters; past it the report says
/// `truncated`.
const SWEEP_LIMIT: i64 = 500;

#[derive(serde::Deserialize)]
struct RevokeSealBody {
    /// The 16-hex id of the seal to revoke.
    seal_id: String,
}

/// `POST /api/seals/revoke` — revoke a seal. Owner-only, like creation:
/// a seal binds every channel in the workspace, so only the authority that
/// could impose it may lift it (D42).
///
/// Revocation means "this value no longer binds anything from now on" —
/// the literal is hard-deleted (it is the most sensitive value the relay
/// stores, and after revocation retaining it is pure liability), and every
/// enforcement point stops seeing it on its next read. It does NOT mean
/// "unpublish": stored tokens keep standing as text, and history the seal
/// once refused stays as it was. Rewriting history is deep seal (D32) and
/// deliberately out of scope here.
pub async fn revoke_seal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (tenant, pubkey) =
        authenticate(&state, &headers, "POST", "/api/seals/revoke", Some(&body)).await?;

    let revoker_hex = pubkey.to_hex();
    let is_owner = state
        .db
        .get_relay_member(tenant.community(), &revoker_hex)
        .await
        .map_err(|e| internal_error(&format!("relay member lookup: {e}")))?
        .is_some_and(|m| m.role == "owner");
    if !is_owner {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "sealing is the workspace Owner's alone",
        ));
    }

    let body: RevokeSealBody = serde_json::from_slice(&body)
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, &format!("invalid body: {e}")))?;
    let id = body.seal_id.trim().to_ascii_lowercase();
    if !buzz_core::seal::is_valid_id(&id) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "seal_id must be 16 lowercase hex characters",
        ));
    }

    let revoked = state
        .db
        .revoke_seal(tenant.community(), &id)
        .await
        .map_err(|e| internal_error(&format!("seal revoke: {e}")))?;
    let Some(revoked) = revoked else {
        return Err(api_error(StatusCode::NOT_FOUND, "no such seal"));
    };

    // The revocation announcement: a second kind:47100 under the same `d`,
    // carrying the same public fields plus `revoked: true`. 47100 is not a
    // replaceable kind, so both events exist and readers take the newest —
    // which also means the registry's history (sealed, then revoked, when,
    // by whom) stays reconstructible from events alone. Best-effort like
    // the creation announcement: enforcement already stopped when the row
    // died, and a failed announcement must not resurrect it.
    let min_tier = revoked
        .min_tier
        .parse::<ChannelTier>()
        .unwrap_or(ChannelTier::Private);
    emit_seal_announce_full(
        &state,
        &tenant,
        &id,
        &revoked.label,
        min_tier,
        &revoker_hex,
        true,
    )
    .await;

    Ok(Json(serde_json::json!({
        "seal_id": id,
        "revoked": true,
    })))
}

/// Publish the relay-signed kind:47100 announcement — the member-visible
/// record. Carries the id, label, tier and creator; **never the literal**.
///
/// Best-effort like the approval notices: the seal is already stored and
/// enforced, and a failed announcement must not unwind that — better a seal
/// that enforces invisibly than a leak window while the announcement is
/// retried.
async fn emit_seal_announce(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: &str,
    label: &str,
    min_tier: ChannelTier,
    creator_hex: &str,
) {
    emit_seal_announce_full(state, tenant, id, label, min_tier, creator_hex, false).await;
}

/// The full announcement shape: `revoked: false` is a creation, `true` a
/// revocation. Split so the two call sites cannot drift apart in tags or
/// content — a reader deduping by `d` must be able to trust that every
/// 47100 for one seal has the same shape.
async fn emit_seal_announce_full(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    id: &str,
    label: &str,
    min_tier: ChannelTier,
    creator_hex: &str,
    revoked: bool,
) {
    let tags: Result<Vec<Tag>, _> = [["d", id], ["tier", min_tier.as_str()], ["p", creator_hex]]
        .into_iter()
        .map(Tag::parse)
        .collect();
    let tags = match tags {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("seal announce: tag build failed: {e}");
            return;
        }
    };
    let content = serde_json::json!({
        "label": label,
        "min_tier": min_tier.as_str(),
        "revoked": revoked,
    });
    let event =
        match EventBuilder::new(Kind::Custom(KIND_SEAL_ANNOUNCE as u16), content.to_string())
            .tags(tags)
            .sign_with_keys(&state.relay_keypair)
        {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("seal announce: signing failed: {e}");
                return;
            }
        };
    match state
        .db
        .insert_event(tenant.community(), &event, None)
        .await
    {
        Ok((stored, true)) => {
            let _ = dispatch_persistent_event(
                tenant,
                state,
                &stored,
                KIND_SEAL_ANNOUNCE,
                &state.relay_keypair.public_key().to_hex(),
                None,
            )
            .await;
        }
        Ok((_, false)) => {}
        Err(e) => tracing::warn!("seal announce: persist failed: {e}"),
    }
}
