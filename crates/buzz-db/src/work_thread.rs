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
    /// When the overdue sweep last emitted a kind:47011 notice, if it has.
    /// Cleared when the deadline is edited so a new deadline can go overdue.
    pub overdue_notified_at: Option<DateTime<Utc>>,
    /// When the canonicalize job claimed this thread (once-only). Re-armed
    /// by a new close-with-canonicalize transition.
    pub canonicalized_at: Option<DateTime<Utc>>,
    /// Recorded canonicalization outcome (`merged`, `unchanged`, `no_repo`,
    /// `no_checkpoint`, `commit_missing`, `conflict`, `error:...`).
    pub canonicalize_outcome: Option<String>,
    /// Parent thread root id when this thread was opened by a kind:47020
    /// fork (D27); `None` for ordinary kind:47000 roots.
    pub forked_from: Option<Vec<u8>>,
    /// Fork-point checkpoint commit (40/64-hex), or `None` for a fork at
    /// head (and for non-fork threads).
    pub fork_commit: Option<String>,
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
    /// Parent thread root id (32 bytes) when opened by a kind:47020 fork.
    pub forked_from: Option<&'a [u8]>,
    /// Fork-point checkpoint commit (40/64-hex), `None` = fork at head.
    pub fork_commit: Option<&'a str>,
}

/// Insert the projection row for a new thread. Returns `false` when the
/// thread id already exists (duplicate root event replay).
pub async fn create_work_thread(pool: &PgPool, params: CreateWorkThreadParams<'_>) -> Result<bool> {
    let inserted = sqlx::query(
        r#"
        INSERT INTO work_threads
            (community_id, thread_id, channel_id, goal, deadline, dri_pubkey,
             created_by, forked_from, fork_commit)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
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
    .bind(params.forked_from)
    .bind(params.fork_commit)
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
               created_at, updated_at, closed_at, overdue_notified_at,
               canonicalized_at, canonicalize_outcome, forked_from, fork_commit
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
               created_at, updated_at, closed_at, overdue_notified_at,
               canonicalized_at, canonicalize_outcome, forked_from, fork_commit
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
///
/// Editing the deadline resets `overdue_notified_at`, so an extended
/// deadline can trigger a fresh overdue notice when it passes.
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
            overdue_notified_at = CASE WHEN $4 THEN NULL ELSE overdue_notified_at END,
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

/// The shared TOCTOU transition statement — used by
/// [`transition_work_thread`] (pool, autocommit) and
/// [`close_thread_archiving_siblings`] (inside its family-locked
/// transaction) so the two paths cannot drift.
const TRANSITION_SQL: &str = r#"
        UPDATE work_threads
        SET status     = $4::work_thread_status,
            canonicalize_on_close = COALESCE($5, canonicalize_on_close),
            closed_at  = CASE WHEN $4 = 'closed' THEN NOW() ELSE closed_at END,
            canonicalized_at = CASE WHEN $4 = 'closed' AND COALESCE($5, FALSE)
                                    THEN NULL ELSE canonicalized_at END,
            canonicalize_outcome = CASE WHEN $4 = 'closed' AND COALESCE($5, FALSE)
                                        THEN NULL ELSE canonicalize_outcome END,
            updated_at = NOW()
        WHERE community_id = $1 AND thread_id = $2
          AND status = $3::work_thread_status
        "#;

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
    let updated = sqlx::query(TRANSITION_SQL)
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

/// Close the winner and archive its fork family (D28 — losing variations
/// archive via the winner's close flow) in **one transaction**, serialized
/// per family by a Postgres advisory transaction lock.
///
/// The family is the connected component of `forked_from` edges: walk up
/// from the winner to the original root, then archive the whole subtree
/// below that root — except the winner. Eligible source states are `open`,
/// `snoozed`, `ready`, and `closed`; `snoozed` is deliberately included
/// even though a client kind:47002 cannot archive from it — the batch runs
/// under the closer's admin authority as part of the close flow, and a
/// parked loser still loses. Two carve-outs:
///
/// - A sibling with a **pending canonicalization** (closed with the flag,
///   not yet claimed) is skipped — archiving it would silently cancel an
///   admin-authorized canon/ merge (the claim and the recovery sweep both
///   require `status = 'closed'`). It can be archived once its kind:47012
///   outcome lands.
/// - Already-archived threads are untouched (idempotent).
///
/// Atomicity and serialization close two races: the winner's close and the
/// batch commit or roll back together (a batch failure can no longer
/// strand a closed-in-projection thread whose close event was never
/// stored), and two concurrent closes-with-archive-siblings in the same
/// family serialize on the lock — the second finds its winner already
/// archived and loses its status guard cleanly, so exactly one winner
/// survives. The `forked_from` edges are immutable, so the family root can
/// be resolved before the lock is taken.
///
/// `channel_id` is a defense-in-depth guard: forks are validated at ingest
/// to live in the parent's channel, so the family never spans channels.
///
/// Returns `None` when the winner is not currently in `expected` (lost a
/// race / stale — treat as a conflict; nothing was changed), otherwise the
/// root ids of the siblings actually archived, for the relay-signed
/// kind:47013 notices.
pub async fn close_thread_archiving_siblings(
    pool: &PgPool,
    community_id: CommunityId,
    channel_id: Uuid,
    winner_thread_id: &[u8],
    expected: WorkThreadStatus,
    canonicalize: Option<bool>,
) -> Result<Option<Vec<Vec<u8>>>> {
    let mut tx = pool.begin().await?;

    let root: Option<(Vec<u8>,)> = sqlx::query_as(
        r#"
        WITH RECURSIVE ancestors AS (
            SELECT thread_id, forked_from
            FROM work_threads
            WHERE community_id = $1 AND thread_id = $2
            UNION ALL
            SELECT w.thread_id, w.forked_from
            FROM work_threads w
            JOIN ancestors a
              ON w.community_id = $1 AND w.thread_id = a.forked_from
        )
        SELECT thread_id FROM ancestors WHERE forked_from IS NULL
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(winner_thread_id)
    .fetch_optional(tx.as_mut())
    .await?;
    let Some((root_id,)) = root else {
        return Ok(None);
    };

    // Family-scoped advisory lock (FNV-1a over community + family root —
    // the command-executor coordinate-lock idiom). Held to commit/rollback.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in community_id.as_uuid().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    for b in &root_id {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(h as i64)
        .execute(tx.as_mut())
        .await?;

    let closed = sqlx::query(TRANSITION_SQL)
        .bind(community_id.as_uuid())
        .bind(winner_thread_id)
        .bind(expected.to_string())
        .bind(WorkThreadStatus::Closed.to_string())
        .bind(canonicalize)
        .execute(tx.as_mut())
        .await?
        .rows_affected();
    if closed == 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
        r#"
        WITH RECURSIVE ancestors AS (
            SELECT thread_id, forked_from
            FROM work_threads
            WHERE community_id = $1 AND thread_id = $2
            UNION ALL
            SELECT w.thread_id, w.forked_from
            FROM work_threads w
            JOIN ancestors a
              ON w.community_id = $1 AND w.thread_id = a.forked_from
        ),
        family AS (
            SELECT thread_id
            FROM ancestors
            WHERE forked_from IS NULL
            UNION ALL
            SELECT w.thread_id
            FROM work_threads w
            JOIN family f
              ON w.community_id = $1 AND w.forked_from = f.thread_id
        )
        UPDATE work_threads
        SET status = 'archived', updated_at = NOW()
        WHERE community_id = $1
          AND channel_id = $3
          AND thread_id IN (SELECT thread_id FROM family)
          AND thread_id <> $2
          AND status IN ('open', 'snoozed', 'ready', 'closed')
          AND NOT (status = 'closed'
                   AND canonicalize_on_close
                   AND canonicalized_at IS NULL)
        RETURNING thread_id
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(winner_thread_id)
    .bind(channel_id)
    .fetch_all(tx.as_mut())
    .await?;

    tx.commit().await?;
    Ok(Some(rows.into_iter().map(|(id,)| id).collect()))
}

/// List live threads whose deadline has passed and which have not yet been
/// notified — the overdue sweep's work list, across all communities.
pub async fn list_overdue_work_threads(pool: &PgPool, limit: i64) -> Result<Vec<WorkThreadRecord>> {
    let limit = limit.clamp(1, 1000);
    let rows = sqlx::query(
        r#"
        SELECT community_id, thread_id, channel_id, goal, deadline, dri_pubkey,
               status::text AS status, canonicalize_on_close, created_by,
               created_at, updated_at, closed_at, overdue_notified_at,
               canonicalized_at, canonicalize_outcome, forked_from, fork_commit
        FROM work_threads
        WHERE deadline IS NOT NULL
          AND deadline < NOW()
          AND status IN ('open', 'snoozed', 'ready')
          AND overdue_notified_at IS NULL
        ORDER BY deadline ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(row_to_record).collect()
}

/// Claim the overdue notification for a thread (TOCTOU-safe): stamps
/// `overdue_notified_at` only if it is still unset and the thread is still
/// live and overdue. Returns `false` when another sweep won the race, the
/// deadline moved, or the thread left a live state — the caller must not
/// emit a notice in that case.
pub async fn claim_overdue_notification(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
) -> Result<bool> {
    let claimed = sqlx::query(
        r#"
        UPDATE work_threads
        SET overdue_notified_at = NOW()
        WHERE community_id = $1 AND thread_id = $2
          AND overdue_notified_at IS NULL
          AND deadline IS NOT NULL
          AND deadline < NOW()
          AND status IN ('open', 'snoozed', 'ready')
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(claimed > 0)
}

/// Claim a thread's canonicalization (TOCTOU-safe once-only): stamps
/// `canonicalized_at` only if the thread is closed with the flag set and
/// unclaimed. `Ok(false)` = lost the race or nothing to do.
pub async fn claim_canonicalization(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
) -> Result<bool> {
    let claimed = sqlx::query(
        r#"
        UPDATE work_threads
        SET canonicalized_at = NOW()
        WHERE community_id = $1 AND thread_id = $2
          AND status = 'closed'
          AND canonicalize_on_close
          AND canonicalized_at IS NULL
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(claimed > 0)
}

/// Record the canonicalization outcome for operators and the kind:47012
/// notice. Only meaningful after a successful claim.
pub async fn record_canonicalize_outcome(
    pool: &PgPool,
    community_id: CommunityId,
    thread_id: &[u8],
    outcome: &str,
) -> Result<bool> {
    let updated = sqlx::query(
        r#"
        UPDATE work_threads
        SET canonicalize_outcome = $3
        WHERE community_id = $1 AND thread_id = $2
        "#,
    )
    .bind(community_id.as_uuid())
    .bind(thread_id)
    .bind(outcome)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated > 0)
}

/// List closed, flag-set, unclaimed threads — the canonicalize crash-recovery
/// sweep's work list, across all communities.
pub async fn list_pending_canonicalizations(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<WorkThreadRecord>> {
    let limit = limit.clamp(1, 1000);
    let rows = sqlx::query(
        r#"
        SELECT community_id, thread_id, channel_id, goal, deadline, dri_pubkey,
               status::text AS status, canonicalize_on_close, created_by,
               created_at, updated_at, closed_at, overdue_notified_at,
               canonicalized_at, canonicalize_outcome, forked_from, fork_commit
        FROM work_threads
        WHERE status = 'closed'
          AND canonicalize_on_close
          AND canonicalized_at IS NULL
        ORDER BY closed_at ASC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(row_to_record).collect()
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
        overdue_notified_at: row.try_get("overdue_notified_at")?,
        canonicalized_at: row.try_get("canonicalized_at")?,
        canonicalize_outcome: row.try_get("canonicalize_outcome")?,
        forked_from: row.try_get("forked_from")?,
        fork_commit: row.try_get("fork_commit")?,
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

#[cfg(test)]
mod pg_tests {
    //! Postgres-gated tests for the work-thread projection: create/read,
    //! metadata edits with clearable fields, and TOCTOU-safe transitions.
    //! Run with:
    //!   `cargo test -p buzz-db --lib work_thread -- --ignored`
    use super::*;
    use crate::channel::{ChannelType, ChannelVisibility};

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

    async fn make_channel(pool: &PgPool, community: CommunityId) -> Uuid {
        let creator = vec![0xc7u8; 32];
        crate::channel::create_channel(
            pool,
            community,
            "threads",
            ChannelType::Stream,
            ChannelVisibility::Open,
            None,
            &creator,
            None,
        )
        .await
        .expect("create channel")
        .id
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn projection_round_trip_and_metadata() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let channel_id = make_channel(&pool, community).await;
        let thread_id = vec![0x11u8; 32];
        let created_by = vec![0xaau8; 32];

        let created = create_work_thread(
            &pool,
            CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id,
                goal: "ship the fix",
                deadline: None,
                dri_pubkey: None,
                created_by: &created_by,
                forked_from: None,
                fork_commit: None,
            },
        )
        .await
        .expect("create thread");
        assert!(created);

        // Duplicate root replay is a no-op.
        let replayed = create_work_thread(
            &pool,
            CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id,
                goal: "ship the fix",
                deadline: None,
                dri_pubkey: None,
                created_by: &created_by,
                forked_from: None,
                fork_commit: None,
            },
        )
        .await
        .expect("replay create");
        assert!(!replayed);

        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.status, WorkThreadStatus::Open);
        assert_eq!(thread.goal, "ship the fix");
        assert!(thread.deadline.is_none() && thread.dri_pubkey.is_none());

        let listed = list_work_threads(&pool, community, channel_id, None, 10)
            .await
            .expect("list threads");
        assert_eq!(listed.len(), 1);
        let ready_only = list_work_threads(
            &pool,
            community,
            channel_id,
            Some(WorkThreadStatus::Ready),
            10,
        )
        .await
        .expect("list ready");
        assert!(ready_only.is_empty());

        // Set goal + deadline + DRI, then clear the clearable fields.
        let deadline = DateTime::from_timestamp(1_900_000_000, 0).expect("valid ts");
        let dri = vec![0xbbu8; 32];
        let updated = update_work_thread_metadata(
            &pool,
            community,
            &thread_id,
            Some("ship the fix, tested"),
            Some(Some(deadline)),
            Some(Some(&dri)),
        )
        .await
        .expect("update metadata");
        assert!(updated);
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.goal, "ship the fix, tested");
        assert_eq!(thread.deadline, Some(deadline));
        assert_eq!(thread.dri_pubkey.as_deref(), Some(dri.as_slice()));

        let cleared =
            update_work_thread_metadata(&pool, community, &thread_id, None, Some(None), Some(None))
                .await
                .expect("clear metadata");
        assert!(cleared);
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.goal, "ship the fix, tested");
        assert!(thread.deadline.is_none() && thread.dri_pubkey.is_none());

        // Unknown thread ids update nothing.
        let missing =
            update_work_thread_metadata(&pool, community, &[0x99u8; 32], Some("nope"), None, None)
                .await
                .expect("update unknown");
        assert!(!missing);
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn transitions_are_toctou_safe() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let channel_id = make_channel(&pool, community).await;
        let thread_id = vec![0x22u8; 32];
        let created_by = vec![0xaau8; 32];

        assert!(create_work_thread(
            &pool,
            CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id,
                goal: "close me",
                deadline: None,
                dri_pubkey: None,
                created_by: &created_by,
                forked_from: None,
                fork_commit: None,
            },
        )
        .await
        .expect("create thread"));

        // open → ready succeeds once; the losing replay (still expecting
        // `open`) is refused — the WHERE status = expected guard.
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("open → ready"));
        assert!(!transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("stale transition"));

        // ready → closed stamps closed_at and records canonicalize.
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            Some(true),
        )
        .await
        .expect("ready → closed"));
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.status, WorkThreadStatus::Closed);
        assert!(thread.canonicalize_on_close);
        assert!(thread.closed_at.is_some());

        // closed → archived → open (the Owner-only reopen path at the DB
        // layer; authority is enforced above in the relay handler).
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Closed,
            WorkThreadStatus::Archived,
            None,
        )
        .await
        .expect("closed → archived"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Archived,
            WorkThreadStatus::Open,
            None,
        )
        .await
        .expect("archived → open"));
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get thread")
            .expect("thread exists");
        assert_eq!(thread.status, WorkThreadStatus::Open);
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn overdue_claim_is_once_and_resets_on_deadline_edit() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let channel_id = make_channel(&pool, community).await;
        let thread_id = vec![0x33u8; 32];
        let created_by = vec![0xaau8; 32];

        // Past-deadline live thread → appears in the sweep list.
        let past = DateTime::from_timestamp(1_600_000_000, 0).expect("valid ts");
        assert!(create_work_thread(
            &pool,
            CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id,
                goal: "overdue task",
                deadline: Some(past),
                dri_pubkey: None,
                created_by: &created_by,
                forked_from: None,
                fork_commit: None,
            },
        )
        .await
        .expect("create thread"));
        let overdue = list_overdue_work_threads(&pool, 100).await.expect("list");
        assert!(
            overdue
                .iter()
                .any(|t| t.community_id == community && t.thread_id == thread_id),
            "past-deadline live thread must be listed"
        );

        // First claim wins; the replay loses (at-most-once notice).
        assert!(claim_overdue_notification(&pool, community, &thread_id)
            .await
            .expect("first claim"));
        assert!(!claim_overdue_notification(&pool, community, &thread_id)
            .await
            .expect("second claim"));
        let overdue = list_overdue_work_threads(&pool, 100).await.expect("list");
        assert!(
            !overdue
                .iter()
                .any(|t| t.community_id == community && t.thread_id == thread_id),
            "claimed thread must leave the sweep list"
        );

        // Editing the deadline re-arms the notice.
        let new_past = DateTime::from_timestamp(1_650_000_000, 0).expect("valid ts");
        assert!(update_work_thread_metadata(
            &pool,
            community,
            &thread_id,
            None,
            Some(Some(new_past)),
            None,
        )
        .await
        .expect("edit deadline"));
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get")
            .expect("exists");
        assert!(thread.overdue_notified_at.is_none(), "deadline edit resets");
        assert!(claim_overdue_notification(&pool, community, &thread_id)
            .await
            .expect("re-armed claim"));

        // Closed threads never claim, even when overdue again.
        assert!(update_work_thread_metadata(
            &pool,
            community,
            &thread_id,
            None,
            Some(Some(past)),
            None,
        )
        .await
        .expect("re-arm again"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("open → ready"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            None,
        )
        .await
        .expect("ready → closed"));
        assert!(!claim_overdue_notification(&pool, community, &thread_id)
            .await
            .expect("closed thread must not claim"));
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn canonicalize_claim_is_once_and_rearms_on_reclose() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let channel_id = make_channel(&pool, community).await;
        let thread_id = vec![0x44u8; 32];
        let created_by = vec![0xaau8; 32];

        assert!(create_work_thread(
            &pool,
            CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id,
                goal: "canon me",
                deadline: None,
                dri_pubkey: None,
                created_by: &created_by,
                forked_from: None,
                fork_commit: None,
            },
        )
        .await
        .expect("create thread"));

        // Open threads never claim, flag or not.
        assert!(!claim_canonicalization(&pool, community, &thread_id)
            .await
            .expect("open thread must not claim"));

        // Close WITHOUT the flag: still no claim.
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("open → ready"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            None,
        )
        .await
        .expect("ready → closed (no canonicalize)"));
        assert!(!claim_canonicalization(&pool, community, &thread_id)
            .await
            .expect("unflagged close must not claim"));
        let pending = list_pending_canonicalizations(&pool, 100)
            .await
            .expect("list");
        assert!(!pending
            .iter()
            .any(|t| t.community_id == community && t.thread_id == thread_id));

        // Reopen (via archive) and re-close WITH the flag: claim works once.
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Closed,
            WorkThreadStatus::Archived,
            None,
        )
        .await
        .expect("closed → archived"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Archived,
            WorkThreadStatus::Open,
            None,
        )
        .await
        .expect("archived → open"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("open → ready again"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            Some(true),
        )
        .await
        .expect("ready → closed with canonicalize"));
        let pending = list_pending_canonicalizations(&pool, 100)
            .await
            .expect("list pending");
        assert!(pending
            .iter()
            .any(|t| t.community_id == community && t.thread_id == thread_id));
        assert!(claim_canonicalization(&pool, community, &thread_id)
            .await
            .expect("first claim"));
        assert!(!claim_canonicalization(&pool, community, &thread_id)
            .await
            .expect("second claim must lose"));
        assert!(
            record_canonicalize_outcome(&pool, community, &thread_id, "no_repo")
                .await
                .expect("record outcome")
        );
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(thread.canonicalize_outcome.as_deref(), Some("no_repo"));

        // Reopen → re-close with the flag re-arms claim AND clears outcome.
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Closed,
            WorkThreadStatus::Archived,
            None,
        )
        .await
        .expect("archive again"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Archived,
            WorkThreadStatus::Open,
            None,
        )
        .await
        .expect("reopen again"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("ready again"));
        assert!(transition_work_thread(
            &pool,
            community,
            &thread_id,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            Some(true),
        )
        .await
        .expect("re-close with canonicalize"));
        let thread = get_work_thread(&pool, community, &thread_id)
            .await
            .expect("get")
            .expect("exists");
        assert!(thread.canonicalized_at.is_none(), "re-close must re-arm");
        assert!(thread.canonicalize_outcome.is_none(), "outcome cleared");
        assert!(claim_canonicalization(&pool, community, &thread_id)
            .await
            .expect("re-armed claim"));
    }

    #[tokio::test]
    #[ignore = "requires Postgres"]
    async fn sibling_fork_archiving_walks_the_family() {
        let pool = setup_pool().await;
        let community = make_community(&pool).await;
        let channel_id = make_channel(&pool, community).await;
        let other_channel = make_channel(&pool, community).await;
        let created_by = vec![0xaau8; 32];
        let sha1 = "c".repeat(40);

        // Family: original T ← forks F1, F2; F3 forks F1 (fork-of-fork).
        // D is an unrelated thread in the same channel; X sits in another
        // channel but claims T as parent (cannot happen via ingest — the
        // channel guard is defense-in-depth).
        let t = vec![0x01u8; 32];
        let f1 = vec![0x02u8; 32];
        let f2 = vec![0x03u8; 32];
        let f3 = vec![0x04u8; 32];
        let decoy = vec![0x05u8; 32];
        let cross = vec![0x06u8; 32];
        for (id, chan, parent, commit) in [
            (&t, channel_id, None, None),
            (&f1, channel_id, Some(&t), Some(sha1.as_str())),
            (&f2, channel_id, Some(&t), None),
            (&f3, channel_id, Some(&f1), None),
            (&decoy, channel_id, None, None),
            (&cross, other_channel, Some(&t), None),
        ] {
            assert!(create_work_thread(
                &pool,
                CreateWorkThreadParams {
                    community_id: community,
                    thread_id: id,
                    channel_id: chan,
                    goal: "family member",
                    deadline: None,
                    dri_pubkey: None,
                    created_by: &created_by,
                    forked_from: parent.map(|p| p.as_slice()),
                    fork_commit: commit,
                },
            )
            .await
            .expect("create thread"));
        }

        // Fork provenance round-trips through the projection.
        let rec = get_work_thread(&pool, community, &f1)
            .await
            .expect("get f1")
            .expect("f1 exists");
        assert_eq!(rec.forked_from.as_deref(), Some(t.as_slice()));
        assert_eq!(rec.fork_commit.as_deref(), Some(sha1.as_str()));
        let rec = get_work_thread(&pool, community, &t)
            .await
            .expect("get t")
            .expect("t exists");
        assert!(rec.forked_from.is_none() && rec.fork_commit.is_none());

        // Spread the family across the batch-eligible states: T stays
        // open, F2 snoozed, F3 closed WITH a pending canonicalization
        // (flag set, unclaimed); the winner F1 goes ready.
        assert!(transition_work_thread(
            &pool,
            community,
            &f2,
            WorkThreadStatus::Open,
            WorkThreadStatus::Snoozed,
            None,
        )
        .await
        .expect("snooze f2"));
        assert!(transition_work_thread(
            &pool,
            community,
            &f3,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("ready f3"));
        assert!(transition_work_thread(
            &pool,
            community,
            &f3,
            WorkThreadStatus::Ready,
            WorkThreadStatus::Closed,
            Some(true),
        )
        .await
        .expect("close f3 with canonicalize"));
        assert!(transition_work_thread(
            &pool,
            community,
            &f1,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("ready f1"));

        // Winner F1 (mid-family): one transaction closes it and archives
        // T and F2 — not the winner, not the unrelated thread, not the
        // cross-channel row, and NOT F3 (its pending canonicalization
        // must not be silently cancelled).
        let mut archived = close_thread_archiving_siblings(
            &pool,
            community,
            channel_id,
            &f1,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("close with siblings")
        .expect("winner close must win");
        archived.sort();
        let mut expected = vec![t.clone(), f2.clone()];
        expected.sort();
        assert_eq!(archived, expected);
        for (id, status) in [
            (&t, WorkThreadStatus::Archived),
            (&f1, WorkThreadStatus::Closed),
            (&f2, WorkThreadStatus::Archived),
            (&f3, WorkThreadStatus::Closed),
            (&decoy, WorkThreadStatus::Open),
            (&cross, WorkThreadStatus::Open),
        ] {
            let rec = get_work_thread(&pool, community, id)
                .await
                .expect("get")
                .expect("exists");
            assert_eq!(rec.status, status, "thread {:02x?}", id[0]);
        }
        let rec = get_work_thread(&pool, community, &f1)
            .await
            .expect("get f1")
            .expect("f1 exists");
        assert!(rec.closed_at.is_some(), "winner close stamps closed_at");

        // The serialized-second-close path: F3 is the only live-ish family
        // member left, but its close already happened — a competing
        // close-with-archive-siblings on the archived F2 loses its status
        // guard and changes nothing (this is the shape a concurrent family
        // close takes after the lock serializes it behind the winner).
        let conflict = close_thread_archiving_siblings(
            &pool,
            community,
            channel_id,
            &f2,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("competing close");
        assert!(
            conflict.is_none(),
            "a family member archived by the winner's batch must lose its own close"
        );
        // And a replayed close on the winner itself (now closed) also
        // conflicts instead of double-batching.
        let replay = close_thread_archiving_siblings(
            &pool,
            community,
            channel_id,
            &f1,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("replayed close");
        assert!(replay.is_none());

        // Once F3's pending canonicalization records its outcome, a later
        // close in the family may archive it (the carve-out is only for
        // *pending* jobs).
        assert!(claim_canonicalization(&pool, community, &f3)
            .await
            .expect("claim f3"));
        assert!(
            record_canonicalize_outcome(&pool, community, &f3, "no_repo")
                .await
                .expect("record f3 outcome")
        );
        assert!(transition_work_thread(
            &pool,
            community,
            &f2,
            WorkThreadStatus::Archived,
            WorkThreadStatus::Open,
            None,
        )
        .await
        .expect("reopen f2"));
        assert!(transition_work_thread(
            &pool,
            community,
            &f2,
            WorkThreadStatus::Open,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("ready f2 again"));
        let mut archived = close_thread_archiving_siblings(
            &pool,
            community,
            channel_id,
            &f2,
            WorkThreadStatus::Ready,
            None,
        )
        .await
        .expect("close f2 with siblings")
        .expect("f2 close must win");
        archived.sort();
        let mut expected = vec![f1.clone(), f3.clone()];
        expected.sort();
        assert_eq!(
            archived, expected,
            "completed-canon F3 and closed F1 archive; T already archived"
        );

        // From the cross-channel row's side the family walk reaches T's
        // tree, but the channel guard keeps the batch inside the winner's
        // channel — nothing in the main channel is touched.
        let archived = close_thread_archiving_siblings(
            &pool,
            community,
            other_channel,
            &cross,
            WorkThreadStatus::Open,
            None,
        )
        .await
        .expect("close cross")
        .expect("cross close wins in its own channel");
        assert!(
            archived.is_empty(),
            "the batch must never leave the winner's channel"
        );
        let rec = get_work_thread(&pool, community, &decoy)
            .await
            .expect("get decoy")
            .expect("decoy exists");
        assert_eq!(rec.status, WorkThreadStatus::Open);
    }
}
