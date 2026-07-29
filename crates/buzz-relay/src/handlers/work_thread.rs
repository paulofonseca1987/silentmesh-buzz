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
use nostr::{Event, EventBuilder, Kind, Tag};
use tracing::warn;
use uuid::Uuid;

use buzz_core::kind::{
    KIND_WORK_THREAD_CHECKPOINT, KIND_WORK_THREAD_METADATA, KIND_WORK_THREAD_SIBLING_ARCHIVED,
    KIND_WORK_THREAD_STATE,
};
use buzz_core::tenant::TenantContext;
use buzz_db::work_thread::WorkThreadStatus;
use buzz_db::EventQuery;

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

/// Page size for the fork-point checkpoint scan.
const FORK_CHECKPOINT_PAGE: i64 = 500;

/// Is `commit` a checkpoint commit the parent thread actually recorded?
///
/// Walks the thread's kind:47010 events newest-first with a keyset cursor
/// (`until` + `before_id`), page by page until a matching `commit` tag is
/// found or the history is exhausted — the pre-storage validation behind a
/// kind:47020 fork's named fork point (D27: fork at head or *any*
/// checkpoint, not just the newest N).
pub(crate) async fn fork_point_is_recorded_checkpoint(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    channel_id: Uuid,
    parent_thread_id: &[u8],
    commit: &str,
) -> Result<bool, IngestError> {
    let commit_lower = commit.to_ascii_lowercase();
    let mut until: Option<DateTime<Utc>> = None;
    let mut before_id: Option<Vec<u8>> = None;
    loop {
        let mut query = EventQuery::for_community(tenant.community());
        query.channel_id = Some(channel_id);
        query.kinds = Some(vec![KIND_WORK_THREAD_CHECKPOINT as i32]);
        query.e_tags = Some(vec![hex::encode(parent_thread_id)]);
        query.limit = Some(FORK_CHECKPOINT_PAGE);
        query.until = until;
        query.before_id = before_id.clone();
        let events = state
            .db
            .query_events(&query)
            .await
            .map_err(|e| IngestError::Internal(format!("error: checkpoint query: {e}")))?;
        let found = events.iter().any(|stored| {
            stored.event.tags.iter().any(|t| {
                let s = t.as_slice();
                s.first().map(|v| v.as_str()) == Some("commit")
                    && s.get(1)
                        .is_some_and(|v| v.eq_ignore_ascii_case(&commit_lower))
            })
        });
        if found {
            return Ok(true);
        }
        if (events.len() as i64) < FORK_CHECKPOINT_PAGE {
            return Ok(false);
        }
        // Advance the keyset cursor past the oldest event of this page; the
        // cursor is strictly monotonic, so the loop terminates.
        let Some(last) = events.last() else {
            return Ok(false);
        };
        until = DateTime::from_timestamp(last.event.created_at.as_secs() as i64, 0);
        before_id = Some(last.event.id.as_bytes().to_vec());
        if until.is_none() {
            return Err(IngestError::Internal(
                "error: checkpoint cursor timestamp out of range".into(),
            ));
        }
    }
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
///
/// Two close-only flags ride the event: `canonicalize` fires the Phase 2e
/// canon/ merge job, and `archive-siblings` (D28) archives every losing
/// thread in the winner's fork family in one atomic batch — under the
/// closer's admin authority, spanning `snoozed` (a parked loser still
/// loses) — and emits a relay-signed kind:47013 notice per archived
/// sibling.
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
    let archive_siblings = event.tags.iter().find_map(|t| {
        let s = t.as_slice();
        (s.first().map(|v| v.as_str()) == Some("archive-siblings"))
            .then(|| s.get(1).map(|v| v.as_str() == "true").unwrap_or(true))
    });
    if archive_siblings.is_some() && target != WorkThreadStatus::Closed {
        return Err(IngestError::Rejected(
            "invalid: archive-siblings only applies to close".into(),
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

    // silent-mesh: a close with archive-siblings runs the winner's TOCTOU
    // close and the D28 family batch in ONE family-locked DB transaction
    // (`close_thread_archiving_siblings`) — the close and the batch commit
    // or roll back together, and concurrent family closes serialize so
    // exactly one winner survives. Every other transition keeps the plain
    // single-row TOCTOU update.
    let (transitioned, archived_siblings) =
        if target == WorkThreadStatus::Closed && archive_siblings == Some(true) {
            match state
                .db
                .close_work_thread_archiving_siblings(
                    tenant.community(),
                    channel_id,
                    &thread_id,
                    thread.status,
                    canonicalize,
                )
                .await
                .map_err(|e| IngestError::Internal(format!("error: close with siblings: {e}")))?
            {
                Some(archived) => (true, archived),
                None => (false, Vec::new()),
            }
        } else {
            let ok = state
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
            (ok, Vec::new())
        };
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

    // silent-mesh: the kind:47013 notices make the batch archive visible in
    // the event stream (the signed events are the truth clients fold).
    // Best-effort after the commit — the projection batch above is the
    // authority; a lost notice degrades the fold, not the state.
    if !archived_siblings.is_empty() {
        emit_sibling_archived_notices(tenant, state, channel_id, &thread_id, &archived_siblings)
            .await;
    }

    // silent-mesh: close-with-canonicalize fires the canon/ merge job
    // (Phase 2e, D38/D40) after the close has committed. Best-effort — the
    // job claims via `canonicalized_at` and the leader sweep recovers jobs
    // lost to a crash between this commit and the spawn.
    if target == WorkThreadStatus::Closed && canonicalize == Some(true) {
        let state = Arc::clone(state);
        let tenant = tenant.clone();
        let thread_id = thread_id.clone();
        tokio::spawn(async move {
            crate::api::git::canonicalize::canonicalize_thread(&state, &tenant, &thread_id).await;
        });
    }

    let mut response = serde_json::json!({
        "thread_id": hex::encode(&thread_id),
        "status": target.to_string(),
    });
    if archive_siblings == Some(true) {
        response["archived_siblings"] = serde_json::Value::from(archived_siblings.len());
    }
    Ok(IngestResult {
        event_id: event.id.to_hex(),
        accepted: true,
        message: format!("response:{response}"),
    })
}

/// Relay-signed kind:47013 sibling-archive notices — one per losing fork
/// archived by the winner's close (D28). Best-effort: failures are logged,
/// never propagated — the projection batch is already committed.
async fn emit_sibling_archived_notices(
    tenant: &TenantContext,
    state: &Arc<AppState>,
    channel_id: Uuid,
    winner_thread_id: &[u8],
    archived: &[Vec<u8>],
) {
    let winner_hex = hex::encode(winner_thread_id);
    let channel_str = channel_id.to_string();
    for sibling in archived {
        let sibling_hex = hex::encode(sibling);
        let tag_rows = [
            vec!["e".to_owned(), sibling_hex.clone()],
            vec!["h".to_owned(), channel_str.clone()],
            vec!["winner".to_owned(), winner_hex.clone()],
        ];
        let tags: Result<Vec<Tag>, _> = tag_rows
            .iter()
            .map(|t| Tag::parse(t.iter().map(String::as_str)))
            .collect();
        let tags = match tags {
            Ok(t) => t,
            Err(e) => {
                warn!(thread = %sibling_hex, "sibling-archive notice: tag build failed: {e}");
                continue;
            }
        };
        let content = serde_json::json!({ "winner": winner_hex }).to_string();
        let signed = match EventBuilder::new(
            Kind::Custom(KIND_WORK_THREAD_SIBLING_ARCHIVED as u16),
            content,
        )
        .tags(tags)
        .sign_with_keys(&state.relay_keypair)
        {
            Ok(e) => e,
            Err(e) => {
                warn!(thread = %sibling_hex, "sibling-archive notice: signing failed: {e}");
                continue;
            }
        };
        match state
            .db
            .insert_event(tenant.community(), &signed, Some(channel_id))
            .await
        {
            Ok((stored, true)) => {
                let _ = dispatch_persistent_event(
                    tenant,
                    state,
                    &stored,
                    KIND_WORK_THREAD_SIBLING_ARCHIVED,
                    &state.relay_keypair.public_key().to_hex(),
                    None,
                )
                .await;
            }
            Ok((_, false)) => {}
            Err(e) => warn!(thread = %sibling_hex, "sibling-archive notice: persist failed: {e}"),
        }
    }
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

        // Phase 2e: the close-with-canonicalize spawned the canon job. This
        // channel has no bound repo, so the job must claim, record the
        // honest `no_repo` outcome, and emit a relay-signed kind:47012
        // notice into the channel.
        let mut outcome = None;
        for _ in 0..50 {
            let t = state
                .db
                .get_work_thread(community, root.id.as_bytes())
                .await
                .expect("get thread")
                .expect("thread exists");
            if t.canonicalize_outcome.is_some() {
                outcome = t.canonicalize_outcome;
                assert!(t.canonicalized_at.is_some());
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(
            outcome.as_deref(),
            Some("no_repo"),
            "canonicalize job must record the no-repo outcome"
        );
        let notice_pool = sqlx::PgPool::connect(&state.config.database_url)
            .await
            .expect("pg pool");
        let notices: Vec<(Vec<u8>, serde_json::Value, String)> = sqlx::query_as(
            "SELECT pubkey, tags, content FROM events \
             WHERE community_id = $1 AND kind = $2 AND deleted_at IS NULL",
        )
        .bind(community.as_uuid())
        .bind(buzz_core::kind::KIND_WORK_THREAD_CANON as i32)
        .fetch_all(&notice_pool)
        .await
        .expect("query canon notices");
        let notice = notices
            .iter()
            .find(|(_, tags, _)| {
                tags.as_array().is_some_and(|ts| {
                    ts.iter().any(|t| {
                        t.as_array().is_some_and(|t| {
                            t.first().and_then(|v| v.as_str()) == Some("e")
                                && t.get(1).and_then(|v| v.as_str()) == Some(root_hex.as_str())
                        })
                    })
                })
            })
            .expect("kind:47012 notice stored for the thread");
        assert_eq!(
            notice.0,
            state.relay_keypair.public_key().to_bytes().to_vec(),
            "canon notice must be relay-signed"
        );
        let body: serde_json::Value = serde_json::from_str(&notice.2).expect("notice JSON");
        assert_eq!(body["outcome"].as_str(), Some("no_repo"));

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
                forked_from: None,
                fork_commit: None,
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

    /// Phase 2f: forks project with provenance, fork points must be
    /// recorded checkpoints, and the winner's close-with-`archive-siblings`
    /// archives the losing family members and emits relay-signed kind:47013
    /// notices.
    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn fork_family_archives_with_the_winners_close() {
        use buzz_core::kind::{
            KIND_WORK_THREAD_CHECKPOINT, KIND_WORK_THREAD_FORK, KIND_WORK_THREAD_SIBLING_ARCHIVED,
        };

        let state = test_state().await;
        let ws_owner = nostr::Keys::generate();
        let ch_admin = nostr::Keys::generate();
        let member = nostr::Keys::generate();

        let host = format!("fork-{}.example", Uuid::new_v4().simple());
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
        for keys in [&ws_owner, &ch_admin, &member] {
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
                "forks",
                ChannelType::Stream,
                ChannelVisibility::Open,
                None,
                &ch_admin.public_key().to_bytes(),
                None,
            )
            .await
            .expect("create channel");
        let admin_bytes = ch_admin.public_key().to_bytes();
        state
            .db
            .add_member(
                community,
                channel.id,
                &member.public_key().to_bytes(),
                MemberRole::Member,
                Some(&admin_bytes),
            )
            .await
            .expect("add member");
        let channel_hex = channel.id.to_string();

        // Original thread, opened by the member.
        let root = signed_event(
            &member,
            KIND_WORK_THREAD_OPEN,
            "original approach",
            &[tag(&["h", &channel_hex])],
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

        // A recorded checkpoint on the original thread.
        let sha1 = "c".repeat(40);
        let checkpoint = signed_event(
            &member,
            KIND_WORK_THREAD_CHECKPOINT,
            "turn 1",
            &[
                tag(&["e", &root_hex]),
                tag(&["h", &channel_hex]),
                tag(&["commit", &sha1]),
            ],
        );
        state
            .db
            .insert_event(community, &checkpoint, Some(channel.id))
            .await
            .expect("store checkpoint");
        assert!(fork_point_is_recorded_checkpoint(
            &state,
            &tenant,
            channel.id,
            root.id.as_bytes(),
            &sha1
        )
        .await
        .expect("checkpoint lookup"));
        assert!(!fork_point_is_recorded_checkpoint(
            &state,
            &tenant,
            channel.id,
            root.id.as_bytes(),
            &"d".repeat(40)
        )
        .await
        .expect("unknown commit lookup"));

        // Two variations: the eventual winner forks at the checkpoint, the
        // loser forks at head.
        let winner = signed_event(
            &member,
            KIND_WORK_THREAD_FORK,
            "try approach B",
            &[
                tag(&["e", &root_hex]),
                tag(&["h", &channel_hex]),
                tag(&["commit", &sha1]),
            ],
        );
        crate::handlers::side_effects::handle_side_effects(
            &tenant,
            KIND_WORK_THREAD_FORK,
            &winner,
            &state,
        )
        .await
        .expect("47020 side effect (winner)");
        let loser = signed_event(
            &member,
            KIND_WORK_THREAD_FORK,
            "try approach C",
            &[tag(&["e", &root_hex]), tag(&["h", &channel_hex])],
        );
        crate::handlers::side_effects::handle_side_effects(
            &tenant,
            KIND_WORK_THREAD_FORK,
            &loser,
            &state,
        )
        .await
        .expect("47020 side effect (loser)");

        let winner_rec = state
            .db
            .get_work_thread(community, winner.id.as_bytes())
            .await
            .expect("get winner")
            .expect("winner projected");
        assert_eq!(
            winner_rec.forked_from.as_deref(),
            Some(root.id.as_bytes().as_slice())
        );
        assert_eq!(winner_rec.fork_commit.as_deref(), Some(sha1.as_str()));
        let loser_rec = state
            .db
            .get_work_thread(community, loser.id.as_bytes())
            .await
            .expect("get loser")
            .expect("loser projected");
        assert!(loser_rec.fork_commit.is_none());

        // Member proposes done on the winner; the Channel Admin closes it
        // with archive-siblings — the batch archives the original and the
        // losing fork under the closer's authority.
        let winner_hex = winner.id.to_hex();
        let propose = signed_event(
            &member,
            KIND_WORK_THREAD_STATE,
            "",
            &[
                tag(&["e", &winner_hex]),
                tag(&["h", &channel_hex]),
                tag(&["state", "ready"]),
            ],
        );
        handle_thread_state(&tenant, &state, &propose, &http_auth(&member))
            .await
            .expect("member open → ready");
        let close = signed_event(
            &ch_admin,
            KIND_WORK_THREAD_STATE,
            "",
            &[
                tag(&["e", &winner_hex]),
                tag(&["h", &channel_hex]),
                tag(&["state", "closed"]),
                tag(&["archive-siblings", "true"]),
            ],
        );
        let result = handle_thread_state(&tenant, &state, &close, &http_auth(&ch_admin))
            .await
            .expect("admin close with archive-siblings");
        assert!(
            result.message.contains("\"archived_siblings\":2"),
            "close response must report the batch size: {}",
            result.message
        );

        for (id, expected) in [
            (winner.id.as_bytes().to_vec(), WorkThreadStatus::Closed),
            (root.id.as_bytes().to_vec(), WorkThreadStatus::Archived),
            (loser.id.as_bytes().to_vec(), WorkThreadStatus::Archived),
        ] {
            let rec = state
                .db
                .get_work_thread(community, &id)
                .await
                .expect("get thread")
                .expect("thread exists");
            assert_eq!(rec.status, expected);
        }

        // One relay-signed kind:47013 notice per archived sibling, tagging
        // the winner.
        let pool = sqlx::PgPool::connect(&state.config.database_url)
            .await
            .expect("pg pool");
        let notices: Vec<(Vec<u8>, serde_json::Value, String)> = sqlx::query_as(
            "SELECT pubkey, tags, content FROM events \
             WHERE community_id = $1 AND kind = $2 AND deleted_at IS NULL",
        )
        .bind(community.as_uuid())
        .bind(KIND_WORK_THREAD_SIBLING_ARCHIVED as i32)
        .fetch_all(&pool)
        .await
        .expect("query notices");
        assert_eq!(notices.len(), 2, "one notice per archived sibling");
        let tag_of = |tags: &serde_json::Value, name: &str| -> Option<String> {
            tags.as_array()?.iter().find_map(|t| {
                let t = t.as_array()?;
                (t.first()?.as_str()? == name).then(|| t.get(1)?.as_str().map(str::to_owned))?
            })
        };
        let mut noticed: Vec<String> = Vec::new();
        for (pubkey, tags, content) in &notices {
            assert_eq!(
                pubkey,
                &state.relay_keypair.public_key().to_bytes().to_vec(),
                "notice must be relay-signed"
            );
            assert_eq!(
                tag_of(tags, "winner").as_deref(),
                Some(winner_hex.as_str()),
                "notice must reference the winner"
            );
            assert_eq!(
                tag_of(tags, "h").as_deref(),
                Some(channel_hex.as_str()),
                "notice must be channel-scoped"
            );
            let body: serde_json::Value = serde_json::from_str(content).expect("notice JSON");
            assert_eq!(body["winner"].as_str(), Some(winner_hex.as_str()));
            noticed.push(tag_of(tags, "e").expect("notice e tag"));
        }
        noticed.sort();
        let mut expected = vec![root.id.to_hex(), loser.id.to_hex()];
        expected.sort();
        assert_eq!(noticed, expected);

        // Replaying the winner's close is an idempotent duplicate — no
        // second batch, no extra notices.
        let replay = handle_thread_state(&tenant, &state, &close, &http_auth(&ch_admin))
            .await
            .expect("replayed close");
        assert!(replay.message.contains("duplicate"));
        let notices_after: Vec<(Vec<u8>,)> = sqlx::query_as(
            "SELECT pubkey FROM events \
             WHERE community_id = $1 AND kind = $2 AND deleted_at IS NULL",
        )
        .bind(community.as_uuid())
        .bind(KIND_WORK_THREAD_SIBLING_ARCHIVED as i32)
        .fetch_all(&pool)
        .await
        .expect("recount notices");
        assert_eq!(notices_after.len(), 2, "replay must not re-emit notices");
    }
}
