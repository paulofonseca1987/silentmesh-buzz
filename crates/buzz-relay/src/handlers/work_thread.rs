//! Work-thread command handlers — kind:47001 (task metadata) and
//! kind:47002 (state transitions), the relay-enforced half of the D41
//! machine (Silent Mesh Phase 2).
//!
//! The signed events are the truth; the `work_threads` projection makes
//! reads cheap and transitions TOCTOU-safe. Authority follows one
//! principle: **agents recommend, humans decide, and terminal transitions
//! have the narrowest authority** —
//!
//! ```text
//! open ⇄ snoozed            any full member
//! open → ready              any full member (done proposed)
//! ready → open              any full member (withdraw the proposal)
//! ready → closed            Channel Admin (channel owner/admin) or the
//! open|ready → archived       workspace Owner (relay_members 'owner')
//! closed → archived         same
//! archived → open           workspace Owner only
//! ```
//!
//! "Full member" = channel role owner/admin/member — bots (agents) never
//! transition; they emit kind:47003 recommendations, which are stored but
//! inert until a human confirms with a real 47001/47002.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use nostr::Event;
use uuid::Uuid;

use buzz_core::kind::{KIND_WORK_THREAD_METADATA, KIND_WORK_THREAD_STATE};
use buzz_core::tenant::TenantContext;
use buzz_db::work_thread::WorkThreadStatus;

use crate::state::AppState;

use super::command_executor::{persist_command_event, PersistResult};
use super::event::dispatch_persistent_event;
use super::ingest::{IngestAuth, IngestError, IngestResult};

/// The author's authority level for D41 decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadAuthority {
    /// Workspace Owner (`relay_members.role = 'owner'`) — every transition,
    /// including reopen from archived.
    WorkspaceOwner,
    /// Channel Admin (channel role owner/admin) — terminal transitions in
    /// their channel.
    ChannelAdmin,
    /// Full channel member — non-terminal transitions and metadata edits.
    Member,
    /// No authority over this thread (non-member, bot, guest).
    None,
}

/// Pure D41 authority table: is `authority` allowed to move a thread from
/// `from` to `to`?
///
/// Kept I/O-free (the `moderation_authz` pattern) so the whole matrix is
/// unit-testable.
pub(crate) fn transition_allowed(
    authority: ThreadAuthority,
    from: WorkThreadStatus,
    to: WorkThreadStatus,
) -> bool {
    use ThreadAuthority::*;
    use WorkThreadStatus::*;

    let member_up = matches!(authority, WorkspaceOwner | ChannelAdmin | Member);
    let admin_up = matches!(authority, WorkspaceOwner | ChannelAdmin);
    let owner_only = matches!(authority, WorkspaceOwner);

    match (from, to) {
        // Non-terminal: any full member.
        (Open, Snoozed) | (Snoozed, Open) | (Open, Ready) | (Ready, Open) => member_up,
        // Terminal: narrowest authority.
        (Ready, Closed) => admin_up,
        (Open, Archived) | (Ready, Archived) | (Closed, Archived) => admin_up,
        // Reopen from storage: Owner only, audit-logged via the stored event.
        (Archived, Open) => owner_only,
        _ => false,
    }
}

/// Resolve the author's D41 authority for a channel: workspace owner first
/// (relay_members), then the channel role. Fails closed — unknown roles,
/// bots, and non-members get [`ThreadAuthority::None`].
async fn resolve_authority(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    channel_id: Uuid,
    author: &[u8],
) -> Result<ThreadAuthority, IngestError> {
    let author_hex = hex::encode(author);
    if let Some(member) = state
        .db
        .get_relay_member(tenant.community(), &author_hex)
        .await
        .map_err(|e| IngestError::Internal(format!("error: relay member lookup: {e}")))?
    {
        if member.role == "owner" {
            return Ok(ThreadAuthority::WorkspaceOwner);
        }
    }
    let role = state
        .db
        .get_member_role(tenant.community(), channel_id, author)
        .await
        .map_err(|e| IngestError::Internal(format!("error: member role lookup: {e}")))?;
    Ok(match role.as_deref() {
        Some("owner") | Some("admin") => ThreadAuthority::ChannelAdmin,
        Some("member") => ThreadAuthority::Member,
        _ => ThreadAuthority::None,
    })
}

/// Extract the thread root (`e` tag, 32-byte event id) and channel (`h`
/// tag) references from a work-thread command.
fn extract_refs(event: &Event) -> Result<(Vec<u8>, Uuid), IngestError> {
    let root_hex = event
        .tags
        .iter()
        .find_map(|t| {
            let s = t.as_slice();
            (s.first().map(|v| v.as_str()) == Some("e"))
                .then(|| s.get(1).map(|v| v.to_string()))
                .flatten()
        })
        .ok_or_else(|| IngestError::Rejected("invalid: missing thread root (e tag)".into()))?;
    let root = hex::decode(&root_hex)
        .ok()
        .filter(|b| b.len() == 32)
        .ok_or_else(|| IngestError::Rejected("invalid: bad thread root id".into()))?;

    let channel = event
        .tags
        .iter()
        .find_map(|t| {
            let s = t.as_slice();
            (s.first().map(|v| v.as_str()) == Some("h"))
                .then(|| s.get(1).and_then(|v| v.parse::<Uuid>().ok()))
                .flatten()
        })
        .ok_or_else(|| IngestError::Rejected("invalid: missing channel (h tag)".into()))?;
    Ok((root, channel))
}

/// Idempotent-replay guard: a command event that is already stored has had
/// its mutation applied, so resubmitting it must succeed as a duplicate —
/// checked *before* the authority/state validation, which would otherwise
/// reject the replay against the post-mutation projection state (e.g. a
/// replayed close reads as an illegal closed → closed transition).
/// `persist_command_event`'s ON CONFLICT insert stays the atomic backstop
/// for the concurrent-submission race.
async fn replayed_command(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
) -> Result<Option<IngestResult>, IngestError> {
    let stored = state
        .db
        .get_event_by_id(tenant.community(), event.id.as_bytes())
        .await
        .map_err(|e| IngestError::Internal(format!("error: event lookup: {e}")))?;
    Ok(stored.map(|_| IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: "duplicate: already processed".into(),
    }))
}

/// Handle kind:47001 — task-metadata edit (goal / deadline / DRI).
///
/// Any full channel member; agents (bots) only recommend. Content is a JSON
/// object; absent fields are untouched, `null` clears deadline/DRI.
pub(crate) async fn handle_thread_metadata(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let (thread_id, channel_id) = extract_refs(event)?;
    let author = auth.pubkey().to_bytes().to_vec();

    if let Some(dup) = replayed_command(tenant, state, event).await? {
        return Ok(dup);
    }

    let thread = state
        .db
        .get_work_thread(tenant.community(), &thread_id)
        .await
        .map_err(|e| IngestError::Internal(format!("error: thread lookup: {e}")))?
        .ok_or_else(|| IngestError::Rejected("invalid: unknown work thread".into()))?;
    if thread.channel_id != channel_id {
        return Err(IngestError::Rejected(
            "invalid: thread does not belong to this channel".into(),
        ));
    }

    let authority = resolve_authority(tenant, state, channel_id, &author).await?;
    if authority == ThreadAuthority::None {
        return Err(IngestError::Rejected(
            "forbidden: only a full channel member may edit task metadata".into(),
        ));
    }

    let body: serde_json::Value = serde_json::from_str(&event.content)
        .map_err(|e| IngestError::Rejected(format!("invalid: metadata body: {e}")))?;
    let goal = body.get("goal").and_then(|v| v.as_str());
    if goal.is_some_and(|g| g.trim().is_empty()) {
        return Err(IngestError::Rejected(
            "invalid: goal must not be empty".into(),
        ));
    }
    let deadline: Option<Option<DateTime<Utc>>> = match body.get("deadline") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => {
            let secs = v.as_i64().ok_or_else(|| {
                IngestError::Rejected("invalid: deadline must be unix seconds".into())
            })?;
            let ts = DateTime::from_timestamp(secs, 0)
                .ok_or_else(|| IngestError::Rejected("invalid: deadline out of range".into()))?;
            Some(Some(ts))
        }
    };
    let dri_bytes: Option<Option<Vec<u8>>> = match body.get("dri") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(v) => {
            let hex_str = v
                .as_str()
                .ok_or_else(|| IngestError::Rejected("invalid: dri must be a pubkey hex".into()))?;
            let bytes = hex::decode(hex_str)
                .ok()
                .filter(|b| b.len() == 32)
                .ok_or_else(|| IngestError::Rejected("invalid: dri must be a pubkey hex".into()))?;
            Some(Some(bytes))
        }
    };
    if goal.is_none() && deadline.is_none() && dri_bytes.is_none() {
        return Err(IngestError::Rejected(
            "invalid: metadata edit must change at least one of goal/deadline/dri".into(),
        ));
    }

    let tx = match persist_command_event(state, tenant, event, Some(channel_id)).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    let updated = state
        .db
        .update_work_thread_metadata(
            tenant.community(),
            &thread_id,
            goal,
            deadline,
            dri_bytes.as_ref().map(|d| d.as_deref()),
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: metadata update: {e}")))?;
    if !updated {
        return Err(IngestError::Rejected("invalid: unknown work thread".into()));
    }

    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    // Fan out + audit the stored command event (commands skip the generic
    // dispatch, so the handler owes it explicitly).
    if let Ok(Some(stored)) = state
        .db
        .get_event_by_id(tenant.community(), event.id.as_bytes())
        .await
    {
        let _ = dispatch_persistent_event(
            tenant,
            state,
            &stored,
            KIND_WORK_THREAD_METADATA,
            &auth.pubkey().to_hex(),
            None,
        )
        .await;
    }

    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({ "thread_id": hex::encode(&thread_id) })
        ),
    })
}

/// Handle kind:47002 — a D41 state transition.
///
/// The `state` tag names the target; the current projection state is the
/// expected source. Both transition legality and author authority are
/// validated, then the projection update is TOCTOU-safe (`WHERE status =
/// expected`) so two concurrent commands cannot both win.
pub(crate) async fn handle_thread_state(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    event: &Event,
    auth: &IngestAuth,
) -> Result<IngestResult, IngestError> {
    let (thread_id, channel_id) = extract_refs(event)?;
    let author = auth.pubkey().to_bytes().to_vec();

    if let Some(dup) = replayed_command(tenant, state, event).await? {
        return Ok(dup);
    }

    let target_str = event
        .tags
        .iter()
        .find_map(|t| {
            let s = t.as_slice();
            (s.first().map(|v| v.as_str()) == Some("state"))
                .then(|| s.get(1).map(|v| v.to_string()))
                .flatten()
        })
        .ok_or_else(|| IngestError::Rejected("invalid: missing state tag".into()))?;
    let target: WorkThreadStatus = target_str
        .parse()
        .map_err(|_| IngestError::Rejected(format!("invalid: unknown state: {target_str}")))?;
    let canonicalize = event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        (s.first().map(|v| v.as_str()) == Some("canonicalize"))
            .then(|| s.get(1).map(|v| v.as_str() == "true").unwrap_or(true))
    });
    if canonicalize.is_some() && target != WorkThreadStatus::Closed {
        return Err(IngestError::Rejected(
            "invalid: canonicalize only applies to close".into(),
        ));
    }

    let thread = state
        .db
        .get_work_thread(tenant.community(), &thread_id)
        .await
        .map_err(|e| IngestError::Internal(format!("error: thread lookup: {e}")))?
        .ok_or_else(|| IngestError::Rejected("invalid: unknown work thread".into()))?;
    if thread.channel_id != channel_id {
        return Err(IngestError::Rejected(
            "invalid: thread does not belong to this channel".into(),
        ));
    }

    let authority = resolve_authority(tenant, state, channel_id, &author).await?;
    if !transition_allowed(authority, thread.status, target) {
        return Err(IngestError::Rejected(format!(
            "forbidden: {} → {} is not permitted for this author (D41)",
            thread.status, target
        )));
    }

    let tx = match persist_command_event(state, tenant, event, Some(channel_id)).await? {
        PersistResult::Duplicate => {
            return Ok(IngestResult {
                event_id: event.id.to_hex(),
                accepted: true,
                message: "duplicate: already processed".into(),
            });
        }
        PersistResult::Inserted(tx) => tx,
    };

    let transitioned = state
        .db
        .transition_work_thread(
            tenant.community(),
            &thread_id,
            thread.status,
            target,
            canonicalize,
        )
        .await
        .map_err(|e| IngestError::Internal(format!("error: transition: {e}")))?;
    if !transitioned {
        return Err(IngestError::Rejected(
            "invalid: thread state changed concurrently (race)".into(),
        ));
    }

    tx.commit()
        .await
        .map_err(|e| IngestError::Internal(format!("error: commit transaction: {e}")))?;

    if let Ok(Some(stored)) = state
        .db
        .get_event_by_id(tenant.community(), event.id.as_bytes())
        .await
    {
        let _ = dispatch_persistent_event(
            tenant,
            state,
            &stored,
            KIND_WORK_THREAD_STATE,
            &auth.pubkey().to_hex(),
            None,
        )
        .await;
    }

    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!(
            "response:{}",
            serde_json::json!({
                "thread_id": hex::encode(&thread_id),
                "status": target.to_string(),
            })
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ThreadAuthority::*;
    use WorkThreadStatus::*;

    const ALL_AUTHORITIES: [ThreadAuthority; 4] = [WorkspaceOwner, ChannelAdmin, Member, None];
    const ALL_STATES: [WorkThreadStatus; 5] = [Open, Snoozed, Ready, Closed, Archived];

    /// The full D41 matrix, exhaustively.
    #[test]
    fn d41_authority_matrix() {
        for authority in ALL_AUTHORITIES {
            for from in ALL_STATES {
                for to in ALL_STATES {
                    let allowed = transition_allowed(authority, from, to);
                    let expected = match (from, to) {
                        (Open, Snoozed) | (Snoozed, Open) | (Open, Ready) | (Ready, Open) => {
                            authority != None
                        }
                        (Ready, Closed)
                        | (Open, Archived)
                        | (Ready, Archived)
                        | (Closed, Archived) => {
                            matches!(authority, WorkspaceOwner | ChannelAdmin)
                        }
                        (Archived, Open) => authority == WorkspaceOwner,
                        _ => false,
                    };
                    assert_eq!(
                        allowed, expected,
                        "authority {authority:?}: {from} → {to} expected {expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_self_transitions_or_resurrections() {
        // Same-state writes and closed→open (skipping archive) are never legal.
        for authority in ALL_AUTHORITIES {
            for s in ALL_STATES {
                assert!(!transition_allowed(authority, s, s));
            }
            assert!(!transition_allowed(authority, Closed, Open));
            assert!(!transition_allowed(authority, Closed, Ready));
            assert!(!transition_allowed(authority, Archived, Ready));
            assert!(!transition_allowed(authority, Snoozed, Ready));
        }
    }
}

#[cfg(test)]
mod pg_tests {
    //! D41 end-to-end over live Postgres: the kind:47000 side effect creates
    //! the projection, kind:47001 metadata edits respect member authority,
    //! and kind:47002 transitions walk the full matrix through the real
    //! command handlers (member proposes, Channel Admin closes, workspace
    //! Owner reopens; bots stay inert).
    //!
    //! Run with:
    //!   `cargo test -p buzz-relay --lib work_thread::pg_tests -- --ignored`
    use super::*;
    use buzz_core::channel::{ChannelType, ChannelVisibility, MemberRole};
    use buzz_core::kind::KIND_WORK_THREAD_OPEN;
    use buzz_db::CreateCommunityWithOwnerResult;

    use crate::handlers::ingest::HttpAuthMethod;

    /// Real-PG state mirroring `command_executor::approval_outcome_tests`.
    async fn test_state() -> Arc<AppState> {
        let mut config = crate::config::Config::from_env().expect("default config loads");
        config.require_relay_membership = false;
        config.redis_url = "redis://127.0.0.1:1".to_string();
        let pool = sqlx::PgPool::connect_lazy(&config.database_url).expect("lazy pg pool");
        let db = buzz_db::Db::from_pool(pool.clone());
        let redis_pool = deadpool_redis::Config::from_url(&config.redis_url)
            .create_pool(Some(deadpool_redis::Runtime::Tokio1))
            .expect("redis pool");
        let pubsub = Arc::new(
            buzz_pubsub::PubSubManager::new(&config.redis_url, redis_pool.clone())
                .await
                .expect("pubsub manager"),
        );
        let audit = buzz_audit::AuditService::new(pool.clone());
        let auth = buzz_auth::AuthService::new(config.auth.clone());
        let search = buzz_search::SearchService::new(pool.clone());
        let workflow_engine = Arc::new(buzz_workflow::WorkflowEngine::new(
            db.clone(),
            buzz_workflow::WorkflowConfig::default(),
        ));
        let media_storage = buzz_media::MediaStorage::new(&config.media).expect("media storage");
        let (state, _audit_shutdown) = AppState::new(
            config,
            db,
            redis_pool,
            audit,
            pubsub,
            auth,
            search,
            workflow_engine,
            nostr::Keys::generate(),
            media_storage,
        );
        Arc::new(state)
    }

    fn signed_event(keys: &nostr::Keys, kind: u32, content: &str, tags: &[Vec<String>]) -> Event {
        let tags: Vec<nostr::Tag> = tags
            .iter()
            .map(|t| nostr::Tag::parse(t.iter().map(|s| s.as_str())).expect("tag"))
            .collect();
        nostr::EventBuilder::new(nostr::Kind::Custom(kind as u16), content)
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign event")
    }

    fn http_auth(keys: &nostr::Keys) -> IngestAuth {
        IngestAuth::Http {
            pubkey: keys.public_key(),
            scopes: buzz_auth::Scope::all_known(),
            auth_method: HttpAuthMethod::DevPubkey,
        }
    }

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn d41_lifecycle_end_to_end() {
        let state = test_state().await;

        let ws_owner = nostr::Keys::generate();
        let ch_admin = nostr::Keys::generate();
        let member = nostr::Keys::generate();
        let bot = nostr::Keys::generate();

        let host = format!("d41-{}.example", Uuid::new_v4().simple());
        let community = match state
            .db
            .create_community_with_owner(&host, &ws_owner.public_key().to_hex())
            .await
            .expect("create community")
        {
            CreateCommunityWithOwnerResult::Created(rec) => rec.id,
            other => panic!("expected fresh community, got {other:?}"),
        };
        let tenant = TenantContext::resolved(community, host);

        for keys in [&ws_owner, &ch_admin, &member, &bot] {
            state
                .db
                .ensure_user(community, keys.public_key().to_bytes().as_ref())
                .await
                .expect("ensure user");
        }

        let channel = state
            .db
            .create_channel(
                community,
                "threads",
                ChannelType::Stream,
                ChannelVisibility::Open,
                None,
                &ch_admin.public_key().to_bytes(),
                None,
            )
            .await
            .expect("create channel");
        let admin_bytes = ch_admin.public_key().to_bytes();
        for (keys, role) in [(&member, MemberRole::Member), (&bot, MemberRole::Bot)] {
            state
                .db
                .add_member(
                    community,
                    channel.id,
                    &keys.public_key().to_bytes(),
                    role,
                    Some(&admin_bytes),
                )
                .await
                .expect("add member");
        }

        // kind:47000 root (opened by the member) → side effect creates the
        // projection row.
        let channel_hex = channel.id.to_string();
        let root = signed_event(
            &member,
            KIND_WORK_THREAD_OPEN,
            "ship the fix",
            &[tag(&["h", &channel_hex]), tag(&["deadline", "1900000000"])],
        );
        crate::handlers::side_effects::handle_side_effects(
            &tenant,
            KIND_WORK_THREAD_OPEN,
            &root,
            &state,
        )
        .await
        .expect("47000 side effect");
        let root_hex = root.id.to_hex();
        let thread = state
            .db
            .get_work_thread(community, root.id.as_bytes())
            .await
            .expect("get thread")
            .expect("projection row created");
        assert_eq!(thread.status, WorkThreadStatus::Open);
        assert_eq!(thread.goal, "ship the fix");
        assert!(thread.deadline.is_some());

        // kind:47001 metadata edit by a full member updates the projection.
        let meta = signed_event(
            &member,
            KIND_WORK_THREAD_METADATA,
            r#"{"goal":"ship the fix, tested","deadline":null}"#,
            &[tag(&["e", &root_hex]), tag(&["h", &channel_hex])],
        );
        handle_thread_metadata(&tenant, &state, &meta, &http_auth(&member))
            .await
            .expect("member metadata edit");
        let thread = state
            .db
            .get_work_thread(community, root.id.as_bytes())
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.goal, "ship the fix, tested");
        assert!(thread.deadline.is_none());

        // Bots never edit metadata — they recommend via kind:47003.
        let bot_meta = signed_event(
            &bot,
            KIND_WORK_THREAD_METADATA,
            r#"{"goal":"bot goal"}"#,
            &[tag(&["e", &root_hex]), tag(&["h", &channel_hex])],
        );
        let denied = handle_thread_metadata(&tenant, &state, &bot_meta, &http_auth(&bot)).await;
        assert!(
            matches!(&denied, Err(IngestError::Rejected(msg)) if msg.contains("full channel member")),
            "bot metadata edit must be rejected"
        );

        let transition = |keys: &nostr::Keys, target: &str, canonicalize: Option<&str>| {
            let mut tags = vec![
                tag(&["e", &root_hex]),
                tag(&["h", &channel_hex]),
                tag(&["state", target]),
            ];
            if let Some(c) = canonicalize {
                tags.push(tag(&["canonicalize", c]));
            }
            signed_event(keys, KIND_WORK_THREAD_STATE, "", &tags)
        };

        // canonicalize is close-only.
        let bad_canon = transition(&member, "ready", Some("true"));
        let res = handle_thread_state(&tenant, &state, &bad_canon, &http_auth(&member)).await;
        assert!(
            matches!(&res, Err(IngestError::Rejected(msg)) if msg.contains("canonicalize")),
            "canonicalize on non-close must be rejected"
        );

        // Member proposes done: open → ready.
        let propose = transition(&member, "ready", None);
        handle_thread_state(&tenant, &state, &propose, &http_auth(&member))
            .await
            .expect("member open → ready");

        // Terminal close is above a member's authority.
        let member_close = transition(&member, "closed", None);
        let res = handle_thread_state(&tenant, &state, &member_close, &http_auth(&member)).await;
        assert!(
            matches!(&res, Err(IngestError::Rejected(msg)) if msg.contains("D41")),
            "member close must be rejected"
        );

        // A bot cannot transition at all.
        let bot_snooze = transition(&bot, "snoozed", None);
        let res = handle_thread_state(&tenant, &state, &bot_snooze, &http_auth(&bot)).await;
        assert!(
            matches!(&res, Err(IngestError::Rejected(msg)) if msg.contains("D41")),
            "bot transition must be rejected"
        );

        // Channel Admin closes with canonicalization.
        let close = transition(&ch_admin, "closed", Some("true"));
        handle_thread_state(&tenant, &state, &close, &http_auth(&ch_admin))
            .await
            .expect("admin ready → closed");
        let thread = state
            .db
            .get_work_thread(community, root.id.as_bytes())
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.status, WorkThreadStatus::Closed);
        assert!(thread.canonicalize_on_close);
        assert!(thread.closed_at.is_some());

        // Replaying the same close event is an idempotent duplicate, not an
        // error (persist_command_event dedups on event id).
        let replay = handle_thread_state(&tenant, &state, &close, &http_auth(&ch_admin))
            .await
            .expect("replayed close is idempotent");
        assert!(replay.message.contains("duplicate"));

        // Admin archives; only the workspace Owner may reopen from storage.
        let archive = transition(&ch_admin, "archived", None);
        handle_thread_state(&tenant, &state, &archive, &http_auth(&ch_admin))
            .await
            .expect("admin closed → archived");
        let member_reopen = transition(&member, "open", None);
        let res = handle_thread_state(&tenant, &state, &member_reopen, &http_auth(&member)).await;
        assert!(
            matches!(&res, Err(IngestError::Rejected(msg)) if msg.contains("D41")),
            "member reopen from archived must be rejected"
        );
        let owner_reopen = transition(&ws_owner, "open", None);
        handle_thread_state(&tenant, &state, &owner_reopen, &http_auth(&ws_owner))
            .await
            .expect("workspace owner archived → open");
        let thread = state
            .db
            .get_work_thread(community, root.id.as_bytes())
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.status, WorkThreadStatus::Open);
    }

    /// Phase 2d: the overdue sweep emits exactly one relay-signed kind:47011
    /// notice per passed deadline, tagging the DRI, and never repeats.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn overdue_sweep_emits_one_dri_tagged_notice() {
        use buzz_core::kind::KIND_WORK_THREAD_OVERDUE;

        let state = test_state().await;
        let ws_owner = nostr::Keys::generate();
        let opener = nostr::Keys::generate();
        let dri = nostr::Keys::generate();

        let host = format!("overdue-{}.example", Uuid::new_v4().simple());
        let community = match state
            .db
            .create_community_with_owner(&host, &ws_owner.public_key().to_hex())
            .await
            .expect("create community")
        {
            CreateCommunityWithOwnerResult::Created(rec) => rec.id,
            other => panic!("expected fresh community, got {other:?}"),
        };
        state
            .db
            .ensure_user(community, opener.public_key().to_bytes().as_ref())
            .await
            .expect("ensure opener");
        let channel = state
            .db
            .create_channel(
                community,
                "overdue",
                ChannelType::Stream,
                ChannelVisibility::Open,
                None,
                &opener.public_key().to_bytes(),
                None,
            )
            .await
            .expect("create channel");

        // Past-deadline thread with a DRI, projected directly.
        let thread_id = nostr::Keys::generate().public_key().to_bytes().to_vec();
        let past = chrono::DateTime::from_timestamp(1_600_000_000, 0).expect("ts");
        assert!(state
            .db
            .create_work_thread(buzz_db::work_thread::CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id: channel.id,
                goal: "overdue task",
                deadline: Some(past),
                dri_pubkey: Some(&dri.public_key().to_bytes()),
                created_by: &opener.public_key().to_bytes(),
            })
            .await
            .expect("create thread"));

        let host_map: std::collections::HashMap<Uuid, String> =
            [(*community.as_uuid(), host.clone())].into_iter().collect();
        let emitted = crate::overdue_sweep::run_overdue_sweep(&state, &host_map).await;
        assert!(emitted >= 1, "sweep must emit the notice");

        // The stored notice is relay-signed and carries e/h/p correlation tags.
        let pool = sqlx::PgPool::connect(&state.config.database_url)
            .await
            .expect("pg pool");
        let fetch_notices = |pool: sqlx::PgPool| async move {
            let rows: Vec<(Vec<u8>, serde_json::Value, String)> = sqlx::query_as(
                "SELECT pubkey, tags, content FROM events \
                 WHERE community_id = $1 AND kind = $2 AND deleted_at IS NULL",
            )
            .bind(community.as_uuid())
            .bind(KIND_WORK_THREAD_OVERDUE as i32)
            .fetch_all(&pool)
            .await
            .expect("query notices");
            rows
        };
        let notices = fetch_notices(pool.clone()).await;
        let thread_hex = hex::encode(&thread_id);
        let tag_of = |tags: &serde_json::Value, name: &str| -> Option<String> {
            tags.as_array()?.iter().find_map(|t| {
                let t = t.as_array()?;
                (t.first()?.as_str()? == name).then(|| t.get(1)?.as_str().map(str::to_owned))?
            })
        };
        let notice = notices
            .iter()
            .find(|(_, tags, _)| tag_of(tags, "e").as_deref() == Some(thread_hex.as_str()))
            .expect("notice stored for the thread");
        assert_eq!(
            notice.0,
            state.relay_keypair.public_key().to_bytes().to_vec(),
            "notice must be relay-signed"
        );
        assert_eq!(
            tag_of(&notice.1, "h").as_deref(),
            Some(channel.id.to_string().as_str())
        );
        assert_eq!(
            tag_of(&notice.1, "p").as_deref(),
            Some(dri.public_key().to_hex().as_str()),
            "notice must tag the DRI"
        );
        let body: serde_json::Value =
            serde_json::from_str(&notice.2).expect("notice content is JSON");
        assert_eq!(body["goal"].as_str(), Some("overdue task"));
        assert_eq!(body["status"].as_str(), Some("open"));

        // A second sweep pass emits nothing new — the claim is once-per-deadline.
        let _ = crate::overdue_sweep::run_overdue_sweep(&state, &host_map).await;
        let thread = state
            .db
            .get_work_thread(community, &thread_id)
            .await
            .expect("get thread")
            .expect("exists");
        assert!(thread.overdue_notified_at.is_some());
        let notices_after = fetch_notices(pool).await;
        assert_eq!(
            notices_after.len(),
            notices.len(),
            "second sweep must not add a notice in this community"
        );
    }
}
