//! Work-thread overdue sweep (Silent Mesh Phase 2d, D40).
//!
//! Leader-only periodic pass over the `work_threads` projection: any live
//! thread (open / snoozed / ready) whose deadline has passed and which has
//! not yet been notified gets a relay-signed kind:47011 notice into its
//! channel, tagging the DRI (`p`; falls back to the thread opener). The
//! claim (`overdue_notified_at`) is TOCTOU-safe, so concurrent sweeps and
//! crashed-then-restarted leaders emit at most one notice per deadline;
//! editing the deadline via kind:47001 re-arms it.

use std::collections::HashMap;
use std::sync::Arc;

use nostr::{EventBuilder, Kind, Tag};
use tracing::warn;
use uuid::Uuid;

use buzz_core::kind::KIND_WORK_THREAD_OVERDUE;
use buzz_core::tenant::TenantContext;
use buzz_db::work_thread::WorkThreadRecord;

use crate::handlers::event::dispatch_persistent_event;
use crate::state::AppState;

/// Max notices emitted per tick — a safety bound, not a fairness scheme;
/// unnotified threads simply carry to the next tick.
const SWEEP_BATCH: i64 = 200;

/// Run one overdue-sweep pass. Returns the number of notices emitted.
///
/// `host_map` maps community ids to hosts (the usage-metrics tick already
/// collects it); threads in communities missing from the map are skipped
/// this tick and retried on the next.
pub async fn run_overdue_sweep(state: &Arc<AppState>, host_map: &HashMap<Uuid, String>) -> usize {
    let overdue = match state.db.list_overdue_work_threads(SWEEP_BATCH).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("overdue sweep: listing failed: {e}");
            return 0;
        }
    };

    let mut emitted = 0usize;
    for thread in overdue {
        let Some(host) = host_map.get(thread.community_id.as_uuid()) else {
            continue;
        };
        // Claim before emitting: at-most-once per deadline. A crash between
        // claim and emit loses that one notice rather than ever duplicating.
        match state
            .db
            .claim_overdue_notification(thread.community_id, &thread.thread_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                warn!("overdue sweep: claim failed: {e}");
                continue;
            }
        }
        let tenant = TenantContext::resolved(thread.community_id, host.clone());
        if emit_overdue_notice(state, &tenant, &thread).await {
            emitted += 1;
        }
    }
    if emitted > 0 {
        metrics::counter!("buzz_work_thread_overdue_notices_total").increment(emitted as u64);
    }
    emitted
}

/// Build, store, and fan out the relay-signed kind:47011 notice.
async fn emit_overdue_notice(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    thread: &WorkThreadRecord,
) -> bool {
    let thread_hex = hex::encode(&thread.thread_id);
    let channel_str = thread.channel_id.to_string();
    // D40: the DRI is responsible for driving the task; a thread without a
    // DRI notifies its opener instead.
    let responsible_hex = hex::encode(
        thread
            .dri_pubkey
            .as_deref()
            .unwrap_or(thread.created_by.as_slice()),
    );
    let content = serde_json::json!({
        "goal": thread.goal,
        "deadline": thread.deadline.map(|d| d.timestamp()),
        "status": thread.status.to_string(),
    });

    let tags: Result<Vec<Tag>, _> = [
        ["e", thread_hex.as_str()],
        ["h", channel_str.as_str()],
        ["p", responsible_hex.as_str()],
    ]
    .into_iter()
    .map(Tag::parse)
    .collect();
    let tags = match tags {
        Ok(t) => t,
        Err(e) => {
            warn!(thread = %thread_hex, "overdue notice: tag build failed: {e}");
            return false;
        }
    };

    let event = match EventBuilder::new(
        Kind::Custom(KIND_WORK_THREAD_OVERDUE as u16),
        content.to_string(),
    )
    .tags(tags)
    .sign_with_keys(&state.relay_keypair)
    {
        Ok(e) => e,
        Err(e) => {
            warn!(thread = %thread_hex, "overdue notice: signing failed: {e}");
            return false;
        }
    };

    match state
        .db
        .insert_event(tenant.community(), &event, Some(thread.channel_id))
        .await
    {
        Ok((stored, true)) => {
            let _ = dispatch_persistent_event(
                tenant,
                state,
                &stored,
                KIND_WORK_THREAD_OVERDUE,
                &state.relay_keypair.public_key().to_hex(),
                None,
            )
            .await;
            true
        }
        Ok((_, false)) => true,
        Err(e) => {
            warn!(thread = %thread_hex, "overdue notice: persist failed: {e}");
            false
        }
    }
}
