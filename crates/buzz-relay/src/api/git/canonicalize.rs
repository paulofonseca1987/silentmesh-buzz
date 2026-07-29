//! Canonicalize-on-close (Silent Mesh Phase 2e, D38/D40).
//!
//! When a work thread closes with `canonicalize=true` (kind:47002), the
//! relay merges the thread's latest kind:47010 checkpoint commit into the
//! channel repo's `canon/<thread>/` layer on the default branch —
//! server-side, with no client push. The merge is a **graft under a
//! prefix**: each thread owns its own `canon/<thread-short>/` subtree, so
//! v1 has no merge-conflict semantics by construction.
//!
//! Mechanics reuse the forge's writer-agnostic substrate: hydrate the bare
//! repo to a tempdir ([`hydrate_for_write`]), advance `refs/heads/…` with
//! pre-2.0 git plumbing (`read-tree` / `write-tree` / `commit-tree` /
//! `update-ref` — deliberately not `merge-tree`, whose `--merge-base` flag
//! needs git ≥ 2.40), then publish through [`cas_publish`] with the same
//! `ParentState`. On a lost pointer CAS the whole job re-hydrates and
//! retries (bounded) — never re-publishing state derived from a superseded
//! parent. Channel repos are relay-owned and the update is a fast-forward,
//! so no push-policy bypass is involved.
//!
//! Failure honesty: canonicalization never fails the already-committed
//! close. Every outcome — merged, unchanged, no checkpoint, commit missing
//! from the repo, retries exhausted — is recorded on the projection and
//! surfaced as a relay-signed kind:47012 notice in the channel. The
//! `canonicalized_at` claim is once-only (TOCTOU-safe) and re-armed by a
//! new close-with-canonicalize, giving reopen → re-close a natural retry
//! path; a leader-side sweep recovers jobs lost to crashes.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;

use nostr::{EventBuilder, Kind, Tag};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

use buzz_core::kind::{KIND_WORK_THREAD_CANON, KIND_WORK_THREAD_CHECKPOINT};
use buzz_core::tenant::TenantContext;
use buzz_db::work_thread::WorkThreadRecord;
use buzz_db::EventQuery;

use super::cas_publish::{cas_publish, CasError, PublishLimits};
use super::hydrate::{hydrate_for_write, HydrationOptions};
use super::manifest_event::{build_ref_state_event, RefStateInputs};
use crate::handlers::event::dispatch_persistent_event;
use crate::state::AppState;

/// Bounded rehydrate-and-retry attempts on a lost pointer CAS.
const MAX_CAS_ATTEMPTS: u32 = 3;
/// Sweep batch bound (crash recovery; leftovers carry to the next tick).
const SWEEP_BATCH: i64 = 50;
/// Checkpoint-event scan bound per thread.
const CHECKPOINT_SCAN_LIMIT: i64 = 500;

/// Terminal result of one canonicalization job, recorded on the projection
/// and reported in the kind:47012 notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonOutcome {
    /// Merged: the default branch advanced to this commit.
    Merged(String),
    /// The checkpoint tree matched what `canon/<thread>/` already held.
    Unchanged,
    /// The channel has no bound repo.
    NoRepo,
    /// The thread has no kind:47010 checkpoint to merge.
    NoCheckpoint,
    /// The recorded checkpoint commit is not present in the repo (the agent
    /// recorded a checkpoint without pushing).
    CommitMissing,
    /// Pointer CAS lost against concurrent pushes, retries exhausted.
    Conflict,
    /// Any other failure.
    Error(String),
}

impl CanonOutcome {
    fn as_str(&self) -> String {
        match self {
            CanonOutcome::Merged(_) => "merged".to_owned(),
            CanonOutcome::Unchanged => "unchanged".to_owned(),
            CanonOutcome::NoRepo => "no_repo".to_owned(),
            CanonOutcome::NoCheckpoint => "no_checkpoint".to_owned(),
            CanonOutcome::CommitMissing => "commit_missing".to_owned(),
            CanonOutcome::Conflict => "conflict".to_owned(),
            CanonOutcome::Error(e) => format!("error:{e}"),
        }
    }
}

/// The `canon/<short>/` prefix a thread's output is grafted under.
fn canon_prefix(thread_id: &[u8]) -> String {
    let hex = hex::encode(thread_id);
    format!("canon/{}", &hex[..hex.len().min(12)])
}

/// Run a git subprocess with the forge's hardened env plus job-specific
/// vars (identity, index/work-tree redirection). Returns trimmed stdout.
async fn run_git_env(
    cwd: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args).kill_on_drop(true);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("HOME", cwd);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("git spawn: {e}"))?;
    if let Some(bytes) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(bytes)
                .await
                .map_err(|e| format!("git stdin: {e}"))?;
            drop(pipe);
        }
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("git wait: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Resolve the latest checkpoint commit for a thread from its kind:47010
/// events (newest `(created_at, id)` wins). Returns the 40/64-hex oid.
async fn latest_checkpoint_commit(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    thread: &WorkThreadRecord,
) -> Result<Option<String>, String> {
    let mut query = EventQuery::for_community(tenant.community());
    query.channel_id = Some(thread.channel_id);
    query.kinds = Some(vec![KIND_WORK_THREAD_CHECKPOINT as i32]);
    query.e_tags = Some(vec![hex::encode(&thread.thread_id)]);
    query.limit = Some(CHECKPOINT_SCAN_LIMIT);
    let events = state
        .db
        .query_events(&query)
        .await
        .map_err(|e| format!("checkpoint query: {e}"))?;

    let mut best: Option<(i64, Vec<u8>, String)> = None;
    for stored in &events {
        let commit = stored.event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            (s.first().map(|v| v.as_str()) == Some("commit"))
                .then(|| s.get(1).map(|v| v.to_string()))
                .flatten()
        });
        let Some(commit) = commit else { continue };
        let key = (
            stored.event.created_at.as_secs() as i64,
            stored.event.id.as_bytes().to_vec(),
        );
        if best
            .as_ref()
            .is_none_or(|(ts, id, _)| (key.0, &key.1) > (*ts, id))
        {
            best = Some((key.0, key.1, commit));
        }
    }
    Ok(best.map(|(_, _, commit)| commit))
}

/// One merge attempt over a fresh hydration. `Ok(None)` means the graft
/// produced no tree change (checkpoint already canonicalized).
async fn attempt_merge(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    owner: &str,
    repo_name: &str,
    thread: &WorkThreadRecord,
    ckpt_commit: &str,
) -> Result<Option<(String, super::cas_publish::CasSuccess)>, CanonOutcome> {
    let (repo, parent) = hydrate_for_write(
        &state.git_store,
        tenant,
        owner,
        repo_name,
        HydrationOptions {
            pack_cache: &state.git_pack_cache,
            scratch_dir: &state.config.git_repo_path,
            max_pack_bytes: state.config.git_max_pack_bytes,
            max_repo_bytes: state.config.git_max_repo_bytes,
        },
    )
    .await
    .map_err(|e| CanonOutcome::Error(format!("hydrate: {e}")))?;
    let repo_path = repo.path().to_path_buf();

    // The recorded checkpoint must actually exist in the repo — a 47010
    // event proves nothing about a push having happened.
    let commit_check = run_git_env(
        &repo_path,
        &["cat-file", "-e", &format!("{ckpt_commit}^{{commit}}")],
        &[],
        None,
    )
    .await;
    if commit_check.is_err() {
        return Err(CanonOutcome::CommitMissing);
    }

    let head = parent.parent.head.clone();
    let main_tip = parent.parent.refs.get(&head).cloned();
    let prefix = canon_prefix(&thread.thread_id);

    // Plumbing sandbox: private index + empty work tree so index plumbing
    // behaves identically in the bare repo.
    let work = tempfile::TempDir::new_in(&state.config.git_repo_path)
        .map_err(|e| CanonOutcome::Error(format!("workdir: {e}")))?;
    let index_path = work.path().join("canon-index");
    let index_str = index_path.to_string_lossy().into_owned();
    let git_dir = repo_path.to_string_lossy().into_owned();
    let work_str = work.path().to_string_lossy().into_owned();
    let plumbing_env: Vec<(&str, &str)> = vec![
        ("GIT_DIR", git_dir.as_str()),
        ("GIT_WORK_TREE", work_str.as_str()),
        ("GIT_INDEX_FILE", index_str.as_str()),
    ];

    // 1. Seed the index from the current default-branch tree (or empty).
    match &main_tip {
        Some(tip) => {
            run_git_env(
                &repo_path,
                &["read-tree", &format!("{tip}^{{tree}}")],
                &plumbing_env,
                None,
            )
            .await
            .map_err(CanonOutcome::Error)?;
        }
        None => {
            run_git_env(&repo_path, &["read-tree", "--empty"], &plumbing_env, None)
                .await
                .map_err(CanonOutcome::Error)?;
        }
    }

    // 2. Drop any previous canonicalization of this thread (replace, not
    //    overlay — `read-tree --prefix` refuses colliding entries).
    let existing = run_git_env(
        &repo_path,
        &["ls-files", "-z", "--", &format!("{prefix}/")],
        &plumbing_env,
        None,
    )
    .await
    .map_err(CanonOutcome::Error)?;
    if !existing.is_empty() {
        run_git_env(
            &repo_path,
            &["update-index", "--force-remove", "-z", "--stdin"],
            &plumbing_env,
            Some(existing.as_bytes()),
        )
        .await
        .map_err(CanonOutcome::Error)?;
    }

    // 3. Graft the checkpoint tree under canon/<short>/ and write the tree.
    run_git_env(
        &repo_path,
        &[
            "read-tree",
            &format!("--prefix={prefix}/"),
            &format!("{ckpt_commit}^{{tree}}"),
        ],
        &plumbing_env,
        None,
    )
    .await
    .map_err(CanonOutcome::Error)?;
    let new_tree = run_git_env(&repo_path, &["write-tree"], &plumbing_env, None)
        .await
        .map_err(CanonOutcome::Error)?;

    if let Some(tip) = &main_tip {
        let old_tree = run_git_env(
            &repo_path,
            &["rev-parse", &format!("{tip}^{{tree}}")],
            &[],
            None,
        )
        .await
        .map_err(CanonOutcome::Error)?;
        if old_tree == new_tree {
            return Ok(None);
        }
    }

    // 4. Commit with the relay's identity; parent the checkpoint commit too
    //    so provenance is in-graph.
    let relay_hex = state.relay_keypair.public_key().to_hex();
    let email = format!("{}@relay.silent-mesh", &relay_hex[..12]);
    let message = format!(
        "canonicalize: thread {} → {prefix}/",
        &hex::encode(&thread.thread_id)[..12]
    );
    let mut commit_args: Vec<String> = vec![
        "commit-tree".to_owned(),
        new_tree.clone(),
        "-m".to_owned(),
        message,
    ];
    if let Some(tip) = &main_tip {
        commit_args.push("-p".to_owned());
        commit_args.push(tip.clone());
    }
    commit_args.push("-p".to_owned());
    commit_args.push(ckpt_commit.to_owned());
    let identity_env: Vec<(&str, &str)> = vec![
        ("GIT_DIR", git_dir.as_str()),
        ("GIT_AUTHOR_NAME", "Silent Mesh Relay"),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", "Silent Mesh Relay"),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let args_ref: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    let new_commit = run_git_env(&repo_path, &args_ref, &identity_env, None)
        .await
        .map_err(CanonOutcome::Error)?;

    // 5. Advance the default branch (guarded by the expected old tip).
    let mut update_args: Vec<String> =
        vec!["update-ref".to_owned(), head.clone(), new_commit.clone()];
    if let Some(tip) = &main_tip {
        update_args.push(tip.clone());
    }
    let args_ref: Vec<&str> = update_args.iter().map(String::as_str).collect();
    run_git_env(
        &repo_path,
        &args_ref,
        &[("GIT_DIR", git_dir.as_str())],
        None,
    )
    .await
    .map_err(CanonOutcome::Error)?;

    // 6. Publish through the pointer CAS with the same parent state.
    let success = cas_publish(
        &state.git_store,
        tenant,
        &repo_path,
        owner,
        repo_name,
        &parent,
        PublishLimits {
            parent_hydrated_bytes: repo.hydrated_bytes(),
            max_pack_bytes: state.config.git_max_pack_bytes,
            max_repo_bytes: state.config.git_max_repo_bytes,
        },
    )
    .await
    .map_err(|e| match e {
        CasError::Conflict { .. } => CanonOutcome::Conflict,
        other => CanonOutcome::Error(format!("publish: {other}")),
    })?;

    Ok(Some((new_commit, success)))
}

/// Run the full canonicalization job for a thread: claim, merge, publish,
/// record, notify. Never returns an error — every failure mode is recorded
/// as an outcome (the close is already committed and must not be failed).
pub async fn canonicalize_thread(state: &Arc<AppState>, tenant: &TenantContext, thread_id: &[u8]) {
    let thread = match state
        .db
        .get_work_thread(tenant.community(), thread_id)
        .await
    {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(e) => {
            warn!("canonicalize: thread lookup failed: {e}");
            return;
        }
    };
    match state
        .db
        .claim_canonicalization(tenant.community(), thread_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            warn!("canonicalize: claim failed: {e}");
            return;
        }
    }

    let outcome = run_canonicalize_job(state, tenant, &thread).await;
    if let Err(e) = state
        .db
        .record_canonicalize_outcome(tenant.community(), thread_id, &outcome.as_str())
        .await
    {
        warn!("canonicalize: outcome record failed: {e}");
    }
    emit_canon_notice(state, tenant, &thread, &outcome).await;
    metrics::counter!(
        "buzz_work_thread_canonicalizations_total",
        "outcome" => match &outcome {
            CanonOutcome::Merged(_) => "merged",
            CanonOutcome::Unchanged => "unchanged",
            CanonOutcome::NoRepo => "no_repo",
            CanonOutcome::NoCheckpoint => "no_checkpoint",
            CanonOutcome::CommitMissing => "commit_missing",
            CanonOutcome::Conflict => "conflict",
            CanonOutcome::Error(_) => "error",
        }
    )
    .increment(1);
}

/// The claimed job body: resolve repo + checkpoint, then merge with a
/// bounded rehydrate-retry on pointer-CAS conflicts.
async fn run_canonicalize_job(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    thread: &WorkThreadRecord,
) -> CanonOutcome {
    let binding = match state
        .db
        .get_channel_repo(tenant.community(), thread.channel_id)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => return CanonOutcome::NoRepo,
        Err(e) => return CanonOutcome::Error(format!("repo binding: {e}")),
    };
    let ckpt = match latest_checkpoint_commit(state, tenant, thread).await {
        Ok(Some(c)) => c,
        Ok(None) => return CanonOutcome::NoCheckpoint,
        Err(e) => return CanonOutcome::Error(e),
    };

    // One git-subprocess permit for the whole job (hydrate + plumbing +
    // publish), same global bound the transport paths share.
    let _permit = match Arc::clone(&state.git_semaphore).acquire_owned().await {
        Ok(p) => p,
        Err(_) => return CanonOutcome::Error("git semaphore closed".into()),
    };

    let mut last = CanonOutcome::Conflict;
    for attempt in 1..=MAX_CAS_ATTEMPTS {
        match attempt_merge(
            state,
            tenant,
            &binding.owner_pubkey,
            &binding.repo_name,
            thread,
            &ckpt,
        )
        .await
        {
            Ok(Some((new_commit, success))) => {
                publish_ref_state(state, tenant, &binding.repo_name, &success).await;
                info!(
                    thread = %hex::encode(&thread.thread_id),
                    commit = %new_commit,
                    "canonicalized thread into canon/ layer"
                );
                return CanonOutcome::Merged(new_commit);
            }
            Ok(None) => return CanonOutcome::Unchanged,
            Err(CanonOutcome::Conflict) => {
                warn!(
                    attempt,
                    thread = %hex::encode(&thread.thread_id),
                    "canonicalize lost pointer CAS; rehydrating"
                );
                last = CanonOutcome::Conflict;
            }
            Err(other) => return other,
        }
    }
    last
}

/// Mirror `finalize_push`'s post-CAS kind:30618 emission (relay actor).
async fn publish_ref_state(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    repo_id: &str,
    success: &super::cas_publish::CasSuccess,
) {
    let relay_hex = state.relay_keypair.public_key().to_hex();
    let inputs = RefStateInputs {
        repo_id,
        head: &success.manifest.head,
        refs: &success.manifest.refs,
        actor_pubkey_hex: &relay_hex,
    };
    match build_ref_state_event(&inputs, &state.relay_keypair) {
        Ok(event) => match state
            .db
            .insert_event(tenant.community(), &event, None)
            .await
        {
            Ok((stored, true)) => {
                crate::handlers::event::fan_out_event_to_local_subscribers(
                    state,
                    tenant.community(),
                    &stored,
                )
                .await;
            }
            Ok((_, false)) => {}
            Err(e) => warn!("canonicalize: kind:30618 insert failed: {e}"),
        },
        Err(e) => warn!("canonicalize: kind:30618 build failed: {e}"),
    }
}

/// Relay-signed kind:47012 outcome notice into the thread's channel.
async fn emit_canon_notice(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    thread: &WorkThreadRecord,
    outcome: &CanonOutcome,
) {
    let thread_hex = hex::encode(&thread.thread_id);
    let channel_str = thread.channel_id.to_string();
    let mut content = serde_json::json!({
        "outcome": outcome.as_str(),
        "prefix": canon_prefix(&thread.thread_id),
    });
    let mut tag_rows: Vec<Vec<String>> = vec![
        vec!["e".into(), thread_hex.clone()],
        vec!["h".into(), channel_str],
    ];
    if let CanonOutcome::Merged(commit) = outcome {
        content["commit"] = serde_json::Value::String(commit.clone());
        tag_rows.push(vec!["commit".into(), commit.clone()]);
    }
    let tags: Result<Vec<Tag>, _> = tag_rows
        .iter()
        .map(|t| Tag::parse(t.iter().map(String::as_str)))
        .collect();
    let tags = match tags {
        Ok(t) => t,
        Err(e) => {
            warn!(thread = %thread_hex, "canon notice: tag build failed: {e}");
            return;
        }
    };
    let event = match EventBuilder::new(
        Kind::Custom(KIND_WORK_THREAD_CANON as u16),
        content.to_string(),
    )
    .tags(tags)
    .sign_with_keys(&state.relay_keypair)
    {
        Ok(e) => e,
        Err(e) => {
            warn!(thread = %thread_hex, "canon notice: signing failed: {e}");
            return;
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
                KIND_WORK_THREAD_CANON,
                &state.relay_keypair.public_key().to_hex(),
                None,
            )
            .await;
        }
        Ok((_, false)) => {}
        Err(e) => warn!(thread = %thread_hex, "canon notice: persist failed: {e}"),
    }
}

/// Crash-recovery sweep: run the job for closed, flag-set, unclaimed
/// threads (normally the close handler fires the job immediately; this
/// catches restarts between commit and job). Leader-only, like the overdue
/// sweep. Returns the number of jobs run.
pub async fn run_canonicalize_sweep(
    state: &Arc<AppState>,
    host_map: &HashMap<Uuid, String>,
) -> usize {
    let pending = match state.db.list_pending_canonicalizations(SWEEP_BATCH).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("canonicalize sweep: listing failed: {e}");
            return 0;
        }
    };
    let mut ran = 0usize;
    for thread in pending {
        let Some(host) = host_map.get(thread.community_id.as_uuid()) else {
            continue;
        };
        let tenant = TenantContext::resolved(thread.community_id, host.clone());
        canonicalize_thread(state, &tenant, &thread.thread_id).await;
        ran += 1;
    }
    ran
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_prefix_is_short_and_stable() {
        let id = [0xabu8; 32];
        assert_eq!(canon_prefix(&id), "canon/abababababab");
    }

    #[test]
    fn outcome_strings_are_stable() {
        assert_eq!(CanonOutcome::Merged("x".into()).as_str(), "merged");
        assert_eq!(CanonOutcome::Unchanged.as_str(), "unchanged");
        assert_eq!(CanonOutcome::NoRepo.as_str(), "no_repo");
        assert_eq!(CanonOutcome::NoCheckpoint.as_str(), "no_checkpoint");
        assert_eq!(CanonOutcome::CommitMissing.as_str(), "commit_missing");
        assert_eq!(CanonOutcome::Conflict.as_str(), "conflict");
        assert_eq!(CanonOutcome::Error("x".into()).as_str(), "error:x");
    }
}

#[cfg(test)]
mod s3_probe_tests {
    //! Full-path probe: hydrate → graft → CAS publish over live MinIO +
    //! Postgres. Gated like the store probes — run with:
    //!   `BUZZ_GIT_S3_PROBE=1 cargo test -p buzz-relay --lib \
    //!    canonicalize::s3_probe_tests -- --ignored`
    //! Pre-req: `docker compose up minio` (bucket `buzz-git`) + Postgres.
    use super::*;
    use buzz_db::CreateCommunityWithOwnerResult;

    fn probe_enabled() -> bool {
        std::env::var("BUZZ_GIT_S3_PROBE").as_deref() == Ok("1")
    }

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

    #[tokio::test]
    #[ignore = "requires Postgres + MinIO (BUZZ_GIT_S3_PROBE=1)"]
    async fn canonicalize_merges_checkpoint_into_canon_layer() {
        if !probe_enabled() {
            eprintln!("skipping: set BUZZ_GIT_S3_PROBE=1 to run against live MinIO");
            return;
        }
        let state = test_state().await;

        let owner_keys = nostr::Keys::generate();
        let member = nostr::Keys::generate();
        let host = format!("canon-{}.example", Uuid::new_v4().simple());
        let community = match state
            .db
            .create_community_with_owner(&host, &owner_keys.public_key().to_hex())
            .await
            .expect("create community")
        {
            CreateCommunityWithOwnerResult::Created(rec) => rec.id,
            other => panic!("expected fresh community, got {other:?}"),
        };
        let tenant = TenantContext::resolved(community, host);
        state
            .db
            .ensure_user(community, member.public_key().to_bytes().as_ref())
            .await
            .expect("ensure member");
        let channel = state
            .db
            .create_channel(
                community,
                "canon-src",
                buzz_core::channel::ChannelType::Stream,
                buzz_core::channel::ChannelVisibility::Open,
                None,
                &member.public_key().to_bytes(),
                None,
            )
            .await
            .expect("create channel");

        // Bind a relay-owned repo (same shape as provision_channel_repo).
        let relay_hex = state.relay_keypair.public_key().to_hex();
        let repo_name = format!("r{}", channel.id.simple());
        assert!(state
            .db
            .bind_channel_repo(community, channel.id, &repo_name, &relay_hex)
            .await
            .expect("bind repo"));

        // Seed the repo with a main tip and a checkpoint commit on a work
        // branch, published through the same hydrate → CAS substrate the
        // canonicalizer uses (stands in for a member's real push).
        let (repo, parent) = hydrate_for_write(
            &state.git_store,
            &tenant,
            &relay_hex,
            &repo_name,
            HydrationOptions {
                pack_cache: &state.git_pack_cache,
                scratch_dir: &state.config.git_repo_path,
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("hydrate fresh repo");
        let repo_path = repo.path().to_path_buf();
        let git_dir = repo_path.to_string_lossy().into_owned();
        let work = tempfile::TempDir::new_in(&state.config.git_repo_path).expect("workdir");
        let index = work.path().join("seed-index");
        let index_str = index.to_string_lossy().into_owned();
        let work_str = work.path().to_string_lossy().into_owned();
        let idn: Vec<(&str, &str)> = vec![
            ("GIT_DIR", git_dir.as_str()),
            ("GIT_WORK_TREE", work_str.as_str()),
            ("GIT_INDEX_FILE", index_str.as_str()),
            ("GIT_AUTHOR_NAME", "seed"),
            ("GIT_AUTHOR_EMAIL", "seed@test"),
            ("GIT_COMMITTER_NAME", "seed"),
            ("GIT_COMMITTER_EMAIL", "seed@test"),
        ];
        let sh = |args: Vec<String>, stdin: Option<Vec<u8>>| {
            let repo_path = repo_path.clone();
            let idn: Vec<(String, String)> = idn
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            async move {
                let refs: Vec<(&str, &str)> =
                    idn.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                run_git_env(&repo_path, &arg_refs, &refs, stdin.as_deref())
                    .await
                    .expect("seed git op")
            }
        };
        let blob = sh(
            vec!["hash-object".into(), "-w".into(), "--stdin".into()],
            Some(b"root file\n".to_vec()),
        )
        .await;
        sh(vec!["read-tree".into(), "--empty".into()], None).await;
        sh(
            vec![
                "update-index".into(),
                "--add".into(),
                "--cacheinfo".into(),
                format!("100644,{blob},README.md"),
            ],
            None,
        )
        .await;
        let tree = sh(vec!["write-tree".into()], None).await;
        let tip = sh(
            vec!["commit-tree".into(), tree, "-m".into(), "root".into()],
            None,
        )
        .await;
        sh(
            vec!["update-ref".into(), "refs/heads/main".into(), tip.clone()],
            None,
        )
        .await;
        let blob2 = sh(
            vec!["hash-object".into(), "-w".into(), "--stdin".into()],
            Some(b"thread output\n".to_vec()),
        )
        .await;
        sh(vec!["read-tree".into(), "--empty".into()], None).await;
        sh(
            vec![
                "update-index".into(),
                "--add".into(),
                "--cacheinfo".into(),
                format!("100644,{blob2},result.txt"),
            ],
            None,
        )
        .await;
        let t2 = sh(vec!["write-tree".into()], None).await;
        let ckpt = sh(
            vec!["commit-tree".into(), t2, "-m".into(), "ckpt".into()],
            None,
        )
        .await;
        sh(
            vec![
                "update-ref".into(),
                "refs/heads/thread-work".into(),
                ckpt.clone(),
            ],
            None,
        )
        .await;
        cas_publish(
            &state.git_store,
            &tenant,
            &repo_path,
            &relay_hex,
            &repo_name,
            &parent,
            PublishLimits {
                parent_hydrated_bytes: repo.hydrated_bytes(),
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("seed publish");
        drop(repo);

        // Closed, flag-set thread + a member-signed kind:47010 checkpoint.
        let root_keys = nostr::Keys::generate();
        let thread_id = root_keys.public_key().to_bytes().to_vec();
        assert!(state
            .db
            .create_work_thread(buzz_db::work_thread::CreateWorkThreadParams {
                community_id: community,
                thread_id: &thread_id,
                channel_id: channel.id,
                goal: "canon me",
                deadline: None,
                dri_pubkey: None,
                created_by: &member.public_key().to_bytes(),
                forked_from: None,
                fork_commit: None,
            })
            .await
            .expect("create thread"));
        use buzz_db::work_thread::WorkThreadStatus as S;
        assert!(state
            .db
            .transition_work_thread(community, &thread_id, S::Open, S::Ready, None)
            .await
            .expect("open → ready"));
        assert!(state
            .db
            .transition_work_thread(community, &thread_id, S::Ready, S::Closed, Some(true))
            .await
            .expect("ready → closed"));
        let ckpt_event = nostr::EventBuilder::new(
            nostr::Kind::Custom(KIND_WORK_THREAD_CHECKPOINT as u16),
            "turn 1",
        )
        .tags(
            [
                vec!["e".to_owned(), hex::encode(&thread_id)],
                vec!["h".to_owned(), channel.id.to_string()],
                vec!["commit".to_owned(), ckpt.clone()],
            ]
            .iter()
            .map(|t| Tag::parse(t.iter().map(String::as_str)).expect("tag"))
            .collect::<Vec<_>>(),
        )
        .sign_with_keys(&member)
        .expect("sign checkpoint");
        state
            .db
            .insert_event(community, &ckpt_event, Some(channel.id))
            .await
            .expect("store checkpoint event");

        // Run the job; it must merge and record the outcome.
        canonicalize_thread(&state, &tenant, &thread_id).await;
        let thread = state
            .db
            .get_work_thread(community, &thread_id)
            .await
            .expect("get thread")
            .expect("exists");
        assert_eq!(thread.canonicalize_outcome.as_deref(), Some("merged"));

        // The published main tip now carries canon/<short>/result.txt plus
        // the original root file.
        let (repo2, _parent2) = hydrate_for_write(
            &state.git_store,
            &tenant,
            &relay_hex,
            &repo_name,
            HydrationOptions {
                pack_cache: &state.git_pack_cache,
                scratch_dir: &state.config.git_repo_path,
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("rehydrate");
        let listing = run_git_env(
            repo2.path(),
            &["ls-tree", "-r", "refs/heads/main", "--name-only"],
            &[],
            None,
        )
        .await
        .expect("ls-tree");
        let prefix = canon_prefix(&thread_id);
        assert!(listing.contains("README.md"), "root file kept: {listing}");
        assert!(
            listing.contains(&format!("{prefix}/result.txt")),
            "canon graft present: {listing}"
        );

        // Second run is a no-op (claim already taken).
        canonicalize_thread(&state, &tenant, &thread_id).await;
        let again = state
            .db
            .get_work_thread(community, &thread_id)
            .await
            .expect("get thread")
            .expect("exists");
        assert_eq!(again.canonicalize_outcome.as_deref(), Some("merged"));
    }
}
