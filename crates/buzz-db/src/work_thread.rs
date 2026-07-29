//! Work-thread projection -- the `work_threads` table.
//!
//! Relay-side projection of the 47xxx work-thread events (Silent Mesh
//! Phase 2, D40/D41): the signed events are the truth; this table makes
//! reads cheap and state transitions TOCTOU-safe. `thread_id` is the
//! 32-byte root event id (kind:47000).
//!
//! Transitions use `UPDATE ... WHERE status = $expected` so two concurrent
//! commands cannot both succeed — 0 rows updated means the caller lost the
//! race (treat as a conflict), mirroring the approvals idiom.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use buzz_core::CommunityId;

use crate::error::{DbError, Result};

/// Lifecycle state of a work thread (the D41 machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkThreadStatus {
    /// Being worked (sub-signals like working/awaiting are computed, not stored).
    Open,
    /// Parked by a member; wakes back to open.
    Snoozed,
    /// Done proposed; awaiting a terminal human decision.
    Ready,
    /// Closed by a Channel Admin / the Owner (with or without canonicalization).
    Closed,
    /// Storage state — abandoned, or post-close.
    Archived,
}

impl fmt::Display for WorkThreadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkThreadStatus::Open => write!(f, "open"),
            WorkThreadStatus::Snoozed => write!(f, "snoozed"),
            WorkThreadStatus::Ready => write!(f, "ready"),
            WorkThreadStatus::Closed => write!(f, "closed"),
            WorkThreadStatus::Archived => write!(f, "archived"),
        }
    }
}

impl FromStr for WorkThreadStatus {
    type Err = DbError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "open" => Ok(WorkThreadStatus::Open),
            "snoozed" => Ok(WorkThreadStatus::Snoozed),
            "ready" => Ok(WorkThreadStatus::Ready),
            "closed" => Ok(WorkThreadStatus::Closed),
            "archived" => Ok(WorkThreadStatus::Archived),
            other => Err(DbError::InvalidData(format!(
                "unknown work thread status: {other}"
            ))),
        }
    }
}

/// A stored work-thread projection row.
#[derive(Debug, Clone)]
pub struct WorkThreadRecord {
    /// Community that owns the thread.
    pub community_id: CommunityId,
    /// 32-byte root event id (kind:47000).
    pub thread_id: Vec<u8>,
    /// Channel the thread lives in.
    pub channel_id: Uuid,
    /// The task goal (root event content).
    pub goal: String,
    /// Optional task deadline.
    pub deadline: Option<DateTime<Utc>>,
    /// Optional directly-responsible individual (human or agent pubkey).
    pub dri_pubkey: Option<Vec<u8>>,
    /// Current lifecycle state.
    pub status: WorkThreadStatus,
    /// Whether close should canonicalize (recorded at close time).
    pub canonicalize_on_close: bool,
    /// Pubkey of the human who opened the thread.
    pub created_by: Vec<u8>,
    /// When the thread was opened.
    pub created_at: DateTime<Utc>,
    /// When the projection last changed.
    pub updated_at: DateTime<Utc>,
    /// When the thread was closed, if it has been.
    pub closed_at: Option<DateTime<Utc>>,
}

/// Parameters for creating a work-thread projection row.
pub struct CreateWorkThreadParams<'a> {
    /// Community that owns the thread.
    pub community_id: CommunityId,
    /// 32-byte root event id.
    pub thread_id: &'a [u8],
    /// Channel the thread lives in.
    pub channel_id: Uuid,
    /// The task goal.
    pub goal: &'a str,
    /// Optional deadline.
    pub deadline: Option<DateTime<Utc>>,
    /// Optional DRI pubkey (32 bytes).
    pub dri_pubkey: Option<&'a [u8]>,
    /// Pubkey of the opener (32 bytes).
    pub created_by: &'a [u8],
}

/// Insert the projection row for a new thread. Returns `false` when the
/// thread id already exists (duplicate root event replay).
pub async fn create_work_thread(pool: &PgPool, params: CreateWorkThreadParams<'_>) -> Result<bool> {
    let inserted = sqlx::query(
        r#"
        INSERT INTO work_threads
            (community_id, thread_id, channel_id, goal, deadline, dri_pubkey, created_by)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(params.community_id.as_uuid())
    .bind(params.thread_id)
    .bind(params.channel_id)
    .bind(params.goal)
    .bind(params.deadline)
    .bind(params.dri_pubkey)
    .bind(params.created_by)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(inserted > 0)
}

/// Fetch a thread by its root event id.
pub async fn get_work_thread(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
) -> Result<Option<WorkThreadRecord>> {
    let row = sqlx::query(
        r#"
        SELECT community_id, thread_id, channel_id, goal, deadline, dri_pubkey,
               status::text AS status, canonicalize_on_close, created_by,
               created_at, updated_at, closed_at
        FROM work_threads
        WHERE community_id = $1 AND thread_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .fetch_optional(pool)
    .await?;
    row.map(row_to_record).transpose()
}

/// List threads in a channel, optionally filtered by status, newest first.
pub async fn list_work_threads(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
    status: Option<WorkThreadStatus>,
    limit: i64,
) -> Result<Vec<WorkThreadRecord>> {
    let limit = limit.clamp(1, 500);
    let rows = sqlx::query(
        r#"
        SELECT community_id, thread_id, channel_id, goal, deadline, dri_pubkey,
               status::text AS status, canonicalize_on_close, created_by,
               created_at, updated_at, closed_at
        FROM work_threads
        WHERE community_id = $1 AND channel_id = $2
          AND ($3::text IS NULL OR status = $3::work_thread_status)
        ORDER BY created_at DESC
        LIMIT $4
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(channel_id)
    .bind(status.map(|s| s.to_string()))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(row_to_record).collect()
}

/// Update task metadata (goal / deadline / DRI). `None` leaves a field
/// untouched; `deadline`/`dri` use double-`Option` so `Some(None)` clears.
/// Returns `false` when the thread does not exist.
pub async fn update_work_thread_metadata(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
    goal: Option<&str>,
    deadline: Option<Option<DateTime<Utc>>>,
    dri_pubkey: Option<Option<&[u8]>>,
) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        UPDATE work_threads
        SET goal       = COALESCE($3, goal),
            deadline   = CASE WHEN $4 THEN $5 ELSE deadline END,
            dri_pubkey = CASE WHEN $6 THEN $7 ELSE dri_pubkey END,
            updated_at = NOW()
        WHERE community_id = $1 AND thread_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .bind(goal)
    .bind(deadline.is_some())
    .bind(deadline.flatten())
    .bind(dri_pubkey.is_some())
    .bind(dri_pubkey.flatten())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// Transition a thread from `expected` to `next` (TOCTOU-safe).
///
/// Returns `false` when the thread is not currently in `expected` — the
/// caller lost a race or the transition is stale; treat as a conflict.
/// Close-shaped transitions stamp `closed_at` and record the
/// canonicalize choice.
pub async fn transition_work_thread(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
    expected: WorkThreadStatus,
    next: WorkThreadStatus,
    canonicalize: Option<bool>,
) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        UPDATE work_threads
        SET status     = $4::work_thread_status,
            canonicalize_on_close = COALESCE($5, canonicalize_on_close),
            closed_at  = CASE WHEN $4 = 'closed' THEN NOW() ELSE closed_at END,
            updated_at = NOW()
        WHERE community_id = $1 AND thread_id = $2
          AND status = $3::work_thread_status
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .bind(expected.to_string())
    .bind(next.to_string())
    .bind(canonicalize)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

fn row_to_record(row: sqlx::postgres::PgRow) -> Result<WorkThreadRecord> {
    let community_id: Uuid = row.try_get("community_id")?;
    let status_str: String = row.try_get("status")?;
    Ok(WorkThreadRecord {
        community_id: CommunityId::from_uuid(community_id),
        thread_id: row.try_get("thread_id")?,
        channel_id: row.try_get("channel_id")?,
        goal: row.try_get("goal")?,
        deadline: row.try_get("deadline")?,
        dri_pubkey: row.try_get("dri_pubkey")?,
        status: status_str.parse()?,
        canonicalize_on_close: row.try_get("canonicalize_on_close")?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        closed_at: row.try_get("closed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for status in [
            WorkThreadStatus::Open,
            WorkThreadStatus::Snoozed,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            WorkThreadStatus::Archived,
        ] {
            assert_eq!(
                status.to_string().parse::<WorkThreadStatus>().unwrap(),
                status
            );
        }
        assert!("done".parse::<WorkThreadStatus>().is_err());
    }
}
