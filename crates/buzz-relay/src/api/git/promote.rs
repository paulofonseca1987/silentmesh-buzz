//! Thread promotion out of a personal channel (Silent Mesh Phase 2g,
//! D29/D30/D31) — the git half of the kind:47021 command.
//!
//! Promotion moves a thread's **files** (the tree of one recorded
//! kind:47010 checkpoint) from the member's personal-channel repo into the
//! target channel repo, grafted under `promoted/<new-thread-short>/` on
//! the target's default branch. The conversation never moves — the new
//! target-channel root references the source thread only by id.
//!
//! The Privacy Gate scaffold (D30) runs **before** any write: the
//! member-written summary and every text blob in the promoted tree pass
//! the deterministic secret scan ([`buzz_core::secret_scan`]). Any finding
//! rejects the whole promotion — nothing is transferred, no event is
//! stored. Binary blobs (NUL-sniffed) are skipped by design; oversized
//! text blobs are refused as unscannable (fail closed) rather than passed
//! unread.
//!
//! Content Seals (D31) run over the same two surfaces and share those
//! limits. Where the secret scan asks "does this *look* like a credential",
//! a seal asks "is this exact registered value allowed to travel this far":
//! promotion is the movement primitive, so it is where a seal earns its
//! keep. Which seals apply is decided by the **target** channel's tier and
//! settled by the caller — this module receives an already-filtered set and
//! applies it, so the tier rule lives in one place. Refusals name the
//! seal's public label, never the sealed value, because the refusal travels
//! to a member standing in the looser channel.
//!
//! Mechanics mirror canonicalize-on-close: hydrate both repos, move the
//! checkpoint objects source → target with a local `git push` into a
//! temporary ref (deleted before publish; a path-remote *fetch* of a bare
//! sha would need `uploadpack.allowAnySHA1InWant`), graft with read-tree
//! plumbing, commit with the relay identity parenting **only the target
//! tip**, then pointer-CAS publish with bounded rehydrate-retry. The
//! source checkpoint is deliberately NOT a parent of the graft commit:
//! that edge would make the personal repo's entire unscanned history
//! (deleted secrets, commit messages, author identities) reachable — and
//! therefore published — in the target repo. Only the scanned tree
//! crosses; provenance lives in the signed kind:47021 event's
//! `e`/`from`/`commit` tags.

use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};
use uuid::Uuid;

use buzz_core::seal::SealedLiteral;
use buzz_core::secret_scan::{looks_binary, scan_text};
use buzz_core::tenant::TenantContext;
use buzz_db::work_thread::WorkThreadRecord;

use super::canonicalize::{latest_checkpoint_commit, publish_ref_state, run_git_env};
use super::cas_publish::{cas_publish, CasError, PublishLimits};
use super::hydrate::{hydrate_for_write, HydrationOptions};
use crate::state::AppState;

/// Bounded rehydrate-and-retry attempts on a lost pointer CAS.
const MAX_CAS_ATTEMPTS: u32 = 3;
/// Largest text blob the gate will scan; larger text is refused as
/// unscannable (fail closed). Binary blobs are skipped regardless of size.
const TEXT_SCAN_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Temporary ref used to move checkpoint objects into the target repo.
/// Deleted before publish so it never appears in the published manifest.
const PROMOTE_TMP_REF: &str = "refs/tmp/promote";

/// Why a promotion was refused or failed. Everything here maps to a
/// command rejection — the kind:47021 event is not stored and nothing has
/// been transferred (the graft publishes last).
#[derive(Debug)]
pub enum PromoteError {
    /// The source (personal) channel has no bound repo.
    NoSourceRepo,
    /// The target channel has no bound repo.
    NoTargetRepo,
    /// The source thread has no recorded checkpoint — nothing to promote.
    NoCheckpoint,
    /// The checkpoint commit is not present in the source repo.
    CommitMissing,
    /// The Privacy Gate found credential-shaped content. Entries are
    /// `"<rule> in <where>"` — rule names and paths only, never values.
    SecretFindings(Vec<String>),
    /// A Content Seal (D31) forbids one of the promoted literals in a
    /// channel this loose. Entries are `"<label> in <where>"` — the seal's
    /// public label and a path, never the sealed value itself.
    ///
    /// Separate from [`PromoteError::SecretFindings`] because the two say
    /// different things to the member: a credential finding means "this
    /// looks like a secret, take it out", while a seal finding means "this
    /// specific value is registered as too sensitive for where you are
    /// sending it" — actionable only if the refusal names the seal.
    SealedFindings(Vec<String>),
    /// A text blob was too large to scan; the gate fails closed.
    Unscannable(String),
    /// Pointer CAS lost against concurrent pushes, retries exhausted.
    Conflict,
    /// Any other failure.
    Error(String),
}

impl PromoteError {
    /// Human-facing rejection message (sanitized — no secret values).
    pub fn reject_message(&self) -> String {
        match self {
            PromoteError::NoSourceRepo => "invalid: source channel has no bound repo".into(),
            PromoteError::NoTargetRepo => "invalid: target channel has no bound repo".into(),
            PromoteError::NoCheckpoint => {
                "invalid: thread has no recorded checkpoint — record one before promoting".into()
            }
            PromoteError::CommitMissing => {
                "invalid: checkpoint commit is not present in the source repo (push first)".into()
            }
            PromoteError::SecretFindings(findings) => format!(
                "forbidden: privacy gate found credential-shaped content: {}",
                findings.join("; ")
            ),
            PromoteError::SealedFindings(findings) => format!(
                "forbidden: promotion would move a sealed value into a channel it is sealed \
                 against: {}. Remove it or promote into a channel at the seal's tier or stricter.",
                findings.join("; ")
            ),
            PromoteError::Unscannable(path) => format!(
                "invalid: {path} is too large for the privacy gate to scan — \
                 split or remove it before promoting"
            ),
            PromoteError::Conflict => {
                "error: target repo is being updated concurrently — retry".into()
            }
            PromoteError::Error(e) => format!("error: promotion failed: {e}"),
        }
    }
}

/// A successful promotion.
pub struct PromoteSuccess {
    /// The target default-branch commit carrying the promoted tree (the
    /// pre-existing tip when the graft was a no-op re-promotion).
    pub commit: String,
    /// The prefix the files landed under.
    pub prefix: String,
}

/// The `promoted/<short>/` prefix for a new target-channel thread.
fn promoted_prefix(new_thread_id: &[u8]) -> String {
    let hex = hex::encode(new_thread_id);
    format!("promoted/{}", &hex[..hex.len().min(12)])
}

/// Resolve the checkpoint to promote: the explicit request (already
/// verified against the thread's recorded checkpoints by the caller) or
/// the thread's latest.
pub async fn resolve_promote_checkpoint(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    source_thread: &WorkThreadRecord,
    requested: Option<&str>,
) -> Result<String, PromoteError> {
    match requested {
        Some(c) => Ok(c.to_ascii_lowercase()),
        None => latest_checkpoint_commit(state, tenant, source_thread)
            .await
            .map_err(PromoteError::Error)?
            .ok_or(PromoteError::NoCheckpoint),
    }
}

/// What the gate found in a promoted tree. Both lists are sanitized:
/// rule names, seal labels and paths only, never matched values.
#[derive(Default)]
struct GateFindings {
    /// Credential-shaped content (D30).
    secrets: Vec<String>,
    /// Sealed literals barred from the target's tier (D31).
    seals: Vec<String>,
}

/// Run the Privacy Gate over every blob in `commit`'s tree (source repo
/// already hydrated at `repo_path`). Returns sanitized findings.
///
/// `sealed` is the set of seals the **target** tier violates, pre-filtered
/// by the caller — this function applies seals, it does not decide which
/// ones apply. An empty slice makes the seal half a no-op, which is what
/// promoting into an `owned` channel gets.
///
/// Both halves inherit the same blind spot: a binary blob is skipped and an
/// oversized text blob fails closed. A sealed value inside a binary file
/// therefore passes here, exactly as a credential would.
async fn scan_checkpoint_tree(
    repo_path: &Path,
    commit: &str,
    sealed: &[SealedLiteral],
) -> Result<GateFindings, PromoteError> {
    let listing = run_git_env(
        repo_path,
        &["ls-tree", "-r", "-l", "-z", &format!("{commit}^{{tree}}")],
        &[],
        None,
    )
    .await
    .map_err(PromoteError::Error)?;

    let mut findings = GateFindings::default();
    for entry in listing.split('\0').filter(|e| !e.is_empty()) {
        // `<mode> <type> <oid> <size>\t<path>`
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut cols = meta.split_whitespace();
        let _mode = cols.next();
        let obj_type = cols.next().unwrap_or_default();
        let oid = cols.next().unwrap_or_default();
        let size: u64 = cols.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        if obj_type != "blob" || oid.is_empty() {
            continue;
        }
        if size > TEXT_SCAN_MAX_BYTES {
            // A large BINARY blob is skipped like any other binary (sniff
            // the head with a bounded read — never materialize the whole
            // blob); large TEXT fails closed as unscannable.
            let head = run_git_env_bytes_head(repo_path, &["cat-file", "blob", oid], 8192).await?;
            if looks_binary(&head) {
                continue;
            }
            return Err(PromoteError::Unscannable(path.to_owned()));
        }
        let bytes = run_git_env_bytes(repo_path, &["cat-file", "blob", oid]).await?;
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for hit in scan_text(&text) {
            findings.secrets.push(format!("{} in {path}", hit.rule));
        }
        // One finding per (seal, file), not per occurrence: a value pasted
        // forty times in one file is one thing to fix, and forty copies of
        // the same line would push the real list off the end of the
        // refusal message.
        let mut named = Vec::new();
        for hit in buzz_core::seal::find_literals(&text, sealed) {
            if named.contains(&hit.id) {
                continue;
            }
            let label = sealed
                .iter()
                .find(|s| s.id == hit.id)
                .map(|s| s.label.as_str())
                .unwrap_or("a sealed value");
            findings.seals.push(format!("{label} in {path}"));
            named.push(hit.id);
        }
    }
    Ok(findings)
}

/// `run_git_env` variant that preserves raw stdout bytes (blob contents).
async fn run_git_env_bytes(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, PromoteError> {
    use std::process::Stdio;
    use tokio::process::Command;
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args).kill_on_drop(true);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("HOME", cwd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = cmd
        .output()
        .await
        .map_err(|e| PromoteError::Error(format!("git spawn: {e}")))?;
    if !output.status.success() {
        return Err(PromoteError::Error(format!(
            "git {:?} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// Bounded head read of a git subprocess's stdout (binary sniffing for
/// blobs too large to materialize). Kills the child after `max` bytes.
async fn run_git_env_bytes_head(
    cwd: &Path,
    args: &[&str],
    max: usize,
) -> Result<Vec<u8>, PromoteError> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    use tokio::process::Command;
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args).kill_on_drop(true);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("HOME", cwd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| PromoteError::Error(format!("git spawn: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| PromoteError::Error("git stdout unavailable".into()))?;
    let mut buf = vec![0u8; max];
    let mut filled = 0usize;
    while filled < max {
        let n = stdout
            .read(&mut buf[filled..])
            .await
            .map_err(|e| PromoteError::Error(format!("git read: {e}")))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    // kill_on_drop reaps the child; we only needed the head.
    drop(stdout);
    let _ = child.kill().await;
    Ok(buf)
}

/// Result of one graft attempt.
enum GraftOutcome {
    /// The default branch advanced to this commit.
    Advanced(String, Box<super::cas_publish::CasSuccess>),
    /// The graft changed nothing (re-promotion of identical content); the
    /// existing default-branch tip already carries the promoted tree.
    Unchanged(String),
}

/// One graft attempt over a fresh target hydration.
#[allow(clippy::too_many_arguments)]
async fn attempt_graft(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    source_repo_path: &Path,
    target_owner: &str,
    target_repo_name: &str,
    ckpt_commit: &str,
    prefix: &str,
    summary_first_line: &str,
) -> Result<GraftOutcome, PromoteError> {
    let (repo, parent) = hydrate_for_write(
        &state.git_store,
        tenant,
        target_owner,
        target_repo_name,
        HydrationOptions {
            pack_cache: &state.git_pack_cache,
            scratch_dir: &state.config.git_repo_path,
            max_pack_bytes: state.config.git_max_pack_bytes,
            max_repo_bytes: state.config.git_max_repo_bytes,
        },
    )
    .await
    .map_err(|e| PromoteError::Error(format!("hydrate target: {e}")))?;
    let target_path = repo.path().to_path_buf();

    // Move the checkpoint objects across with a local push into a
    // temporary ref (deleted before publish).
    let target_str = target_path.to_string_lossy().into_owned();
    run_git_env(
        source_repo_path,
        &[
            "push",
            "--no-verify",
            &target_str,
            &format!("+{ckpt_commit}:{PROMOTE_TMP_REF}"),
        ],
        &[],
        None,
    )
    .await
    .map_err(|e| PromoteError::Error(format!("object transfer: {e}")))?;

    let head = parent.parent.head.clone();
    let main_tip = parent.parent.refs.get(&head).cloned();

    let work = tempfile::TempDir::new_in(&state.config.git_repo_path)
        .map_err(|e| PromoteError::Error(format!("workdir: {e}")))?;
    let index_path = work.path().join("promote-index");
    let index_str = index_path.to_string_lossy().into_owned();
    let git_dir = target_path.to_string_lossy().into_owned();
    let work_str = work.path().to_string_lossy().into_owned();
    let plumbing_env: Vec<(&str, &str)> = vec![
        ("GIT_DIR", git_dir.as_str()),
        ("GIT_WORK_TREE", work_str.as_str()),
        ("GIT_INDEX_FILE", index_str.as_str()),
    ];

    match &main_tip {
        Some(tip) => {
            run_git_env(
                &target_path,
                &["read-tree", &format!("{tip}^{{tree}}")],
                &plumbing_env,
                None,
            )
            .await
            .map_err(PromoteError::Error)?;
        }
        None => {
            run_git_env(&target_path, &["read-tree", "--empty"], &plumbing_env, None)
                .await
                .map_err(PromoteError::Error)?;
        }
    }

    let existing = run_git_env(
        &target_path,
        &["ls-files", "-z", "--", &format!("{prefix}/")],
        &plumbing_env,
        None,
    )
    .await
    .map_err(PromoteError::Error)?;
    if !existing.is_empty() {
        run_git_env(
            &target_path,
            &["update-index", "--force-remove", "-z", "--stdin"],
            &plumbing_env,
            Some(existing.as_bytes()),
        )
        .await
        .map_err(PromoteError::Error)?;
    }

    run_git_env(
        &target_path,
        &[
            "read-tree",
            &format!("--prefix={prefix}/"),
            &format!("{ckpt_commit}^{{tree}}"),
        ],
        &plumbing_env,
        None,
    )
    .await
    .map_err(PromoteError::Error)?;
    let new_tree = run_git_env(&target_path, &["write-tree"], &plumbing_env, None)
        .await
        .map_err(PromoteError::Error)?;

    // Drop the temporary transfer ref before anything is published.
    run_git_env(
        &target_path,
        &["update-ref", "-d", PROMOTE_TMP_REF],
        &[("GIT_DIR", git_dir.as_str())],
        None,
    )
    .await
    .map_err(PromoteError::Error)?;

    if let Some(tip) = &main_tip {
        let old_tree = run_git_env(
            &target_path,
            &["rev-parse", &format!("{tip}^{{tree}}")],
            &[],
            None,
        )
        .await
        .map_err(PromoteError::Error)?;
        if old_tree == new_tree {
            return Ok(GraftOutcome::Unchanged(tip.clone()));
        }
    }

    let relay_hex = state.relay_keypair.public_key().to_hex();
    let email = format!("{}@relay.silent-mesh", &relay_hex[..12]);
    let message = format!("promote: {summary_first_line} → {prefix}/");
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
    // Deliberately NO parent edge to the source checkpoint: a parent link
    // would make the personal repo's ENTIRE history (deleted secrets,
    // commit messages, author identities) reachable from the target's
    // default branch, and the reachability-based CAS pack would publish it
    // all — a wholesale Privacy Gate bypass. Only the scanned tree crosses;
    // provenance lives in the signed kind:47021 event (`e`/`from`/`commit`
    // tags), not in the git graph.
    let identity_env: Vec<(&str, &str)> = vec![
        ("GIT_DIR", git_dir.as_str()),
        ("GIT_AUTHOR_NAME", "Silent Mesh Relay"),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", "Silent Mesh Relay"),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let args_ref: Vec<&str> = commit_args.iter().map(String::as_str).collect();
    let new_commit = run_git_env(&target_path, &args_ref, &identity_env, None)
        .await
        .map_err(PromoteError::Error)?;

    let mut update_args: Vec<String> =
        vec!["update-ref".to_owned(), head.clone(), new_commit.clone()];
    if let Some(tip) = &main_tip {
        update_args.push(tip.clone());
    }
    let args_ref: Vec<&str> = update_args.iter().map(String::as_str).collect();
    run_git_env(
        &target_path,
        &args_ref,
        &[("GIT_DIR", git_dir.as_str())],
        None,
    )
    .await
    .map_err(PromoteError::Error)?;

    let success = cas_publish(
        &state.git_store,
        tenant,
        &target_path,
        target_owner,
        target_repo_name,
        &parent,
        PublishLimits {
            parent_hydrated_bytes: repo.hydrated_bytes(),
            max_pack_bytes: state.config.git_max_pack_bytes,
            max_repo_bytes: state.config.git_max_repo_bytes,
        },
    )
    .await
    .map_err(|e| match e {
        CasError::Conflict { .. } => PromoteError::Conflict,
        other => PromoteError::Error(format!("publish: {other}")),
    })?;

    Ok(GraftOutcome::Advanced(new_commit, Box::new(success)))
}

/// Move a source thread's checkpoint tree into the target channel repo
/// through the Privacy Gate. Pure git + gate — the caller owns every
/// authority check, the projection writes, and the event lifecycle.
#[allow(clippy::too_many_arguments)]
pub async fn promote_thread_files(
    state: &Arc<AppState>,
    tenant: &TenantContext,
    source_thread: &WorkThreadRecord,
    target_channel: Uuid,
    new_thread_id: &[u8],
    summary: &str,
    ckpt_commit: &str,
    sealed: &[SealedLiteral],
) -> Result<PromoteSuccess, PromoteError> {
    // Gate the summary first — cheapest check, no git work.
    let summary_hits: Vec<String> = scan_text(summary)
        .into_iter()
        .map(|h| format!("{} in summary", h.rule))
        .collect();
    if !summary_hits.is_empty() {
        return Err(PromoteError::SecretFindings(summary_hits));
    }
    let mut summary_seals: Vec<String> = Vec::new();
    for hit in buzz_core::seal::find_literals(summary, sealed) {
        let Some(seal) = sealed.iter().find(|s| s.id == hit.id) else {
            continue;
        };
        let finding = format!("{} in summary", seal.label);
        if !summary_seals.contains(&finding) {
            summary_seals.push(finding);
        }
    }
    if !summary_seals.is_empty() {
        return Err(PromoteError::SealedFindings(summary_seals));
    }

    let source_binding = state
        .db
        .get_channel_repo(tenant.community(), source_thread.channel_id)
        .await
        .map_err(|e| PromoteError::Error(format!("source repo binding: {e}")))?
        .ok_or(PromoteError::NoSourceRepo)?;
    let target_binding = state
        .db
        .get_channel_repo(tenant.community(), target_channel)
        .await
        .map_err(|e| PromoteError::Error(format!("target repo binding: {e}")))?
        .ok_or(PromoteError::NoTargetRepo)?;

    let _permit = match Arc::clone(&state.git_semaphore).acquire_owned().await {
        Ok(p) => p,
        Err(_) => return Err(PromoteError::Error("git semaphore closed".into())),
    };

    // Hydrate the source once; it is read-only for the whole promotion and
    // stays alive across target CAS retries.
    let (source_repo, _source_parent) = hydrate_for_write(
        &state.git_store,
        tenant,
        &source_binding.owner_pubkey,
        &source_binding.repo_name,
        HydrationOptions {
            pack_cache: &state.git_pack_cache,
            scratch_dir: &state.config.git_repo_path,
            max_pack_bytes: state.config.git_max_pack_bytes,
            max_repo_bytes: state.config.git_max_repo_bytes,
        },
    )
    .await
    .map_err(|e| PromoteError::Error(format!("hydrate source: {e}")))?;
    let source_path = source_repo.path().to_path_buf();

    let commit_exists = run_git_env(
        &source_path,
        &["cat-file", "-e", &format!("{ckpt_commit}^{{commit}}")],
        &[],
        None,
    )
    .await;
    if commit_exists.is_err() {
        return Err(PromoteError::CommitMissing);
    }

    // The Privacy Gate over the promoted tree (D30 scaffold) plus the
    // Content Seals barred from the target's tier (D31).
    let findings = scan_checkpoint_tree(&source_path, ckpt_commit, sealed).await?;
    // Seals first: the more specific refusal, and the one the member can
    // act on by name. A tree can trip both.
    if !findings.seals.is_empty() {
        return Err(PromoteError::SealedFindings(findings.seals));
    }
    if !findings.secrets.is_empty() {
        return Err(PromoteError::SecretFindings(findings.secrets));
    }

    let prefix = promoted_prefix(new_thread_id);
    let summary_first_line = summary.lines().next().unwrap_or("promoted thread");
    let summary_first_line: String = summary_first_line.chars().take(72).collect();

    let mut last = PromoteError::Conflict;
    for attempt in 1..=MAX_CAS_ATTEMPTS {
        match attempt_graft(
            state,
            tenant,
            &source_path,
            &target_binding.owner_pubkey,
            &target_binding.repo_name,
            ckpt_commit,
            &prefix,
            &summary_first_line,
        )
        .await
        {
            Ok(GraftOutcome::Advanced(new_commit, success)) => {
                publish_ref_state(state, tenant, &target_binding.repo_name, &success).await;
                info!(
                    thread = %hex::encode(new_thread_id),
                    commit = %new_commit,
                    prefix = %prefix,
                    "promoted thread files into target channel repo"
                );
                return Ok(PromoteSuccess {
                    commit: new_commit,
                    prefix,
                });
            }
            Ok(GraftOutcome::Unchanged(tip)) => {
                // Identical content already promoted (command replayed after
                // a crash) — success, pointing at the target branch commit
                // that already carries the promoted tree.
                return Ok(PromoteSuccess {
                    commit: tip,
                    prefix,
                });
            }
            Err(PromoteError::Conflict) => {
                warn!(attempt, "promotion lost pointer CAS; rehydrating target");
                last = PromoteError::Conflict;
            }
            Err(other) => return Err(other),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promoted_prefix_is_short_and_stable() {
        let id = [0xcd_u8; 32];
        assert_eq!(promoted_prefix(&id), "promoted/cdcdcdcdcdcd");
    }

    #[test]
    fn reject_messages_never_echo_values() {
        let e = PromoteError::SecretFindings(vec!["aws-access-key-id in notes.md".into()]);
        let msg = e.reject_message();
        assert!(msg.contains("privacy gate"));
        assert!(msg.contains("aws-access-key-id in notes.md"));
        assert!(PromoteError::NoCheckpoint
            .reject_message()
            .contains("checkpoint"));
        assert!(PromoteError::Unscannable("big.txt".into())
            .reject_message()
            .contains("big.txt"));
    }

    #[test]
    fn a_seal_refusal_reads_differently_from_a_credential_refusal() {
        // The two must not be confusable: "looks like a secret" and "is a
        // registered sensitive value" call for different fixes.
        let sealed = PromoteError::SealedFindings(vec!["the Zurich client in notes.md".into()])
            .reject_message();
        let secret = PromoteError::SecretFindings(vec!["aws-access-key-id in notes.md".into()])
            .reject_message();
        assert!(sealed.contains("sealed value"), "{sealed}");
        assert!(sealed.contains("the Zurich client in notes.md"), "{sealed}");
        assert!(
            !sealed.contains("credential-shaped"),
            "a seal is not a credential finding: {sealed}"
        );
        assert_ne!(sealed, secret);
    }

    /// One commit holding `files`. Returns the tempdir (kept alive by the
    /// caller) and the commit oid.
    async fn commit_tree(files: &[(&str, &[u8])]) -> (tempfile::TempDir, String) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path();
        for (name, body) in files {
            std::fs::write(path.join(name), body).expect("write blob");
        }
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["config", "user.email", "seal@test"],
            vec!["config", "user.name", "seal"],
            vec!["add", "-A"],
            vec!["commit", "-m", "seed", "--quiet"],
        ] {
            run_git_env(path, &args, &[], None)
                .await
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        }
        let head = run_git_env(path, &["rev-parse", "HEAD"], &[], None)
            .await
            .expect("rev-parse");
        (dir, head)
    }

    fn private_seal(id: &str, label: &str, literal: &str) -> SealedLiteral {
        SealedLiteral {
            id: id.into(),
            label: label.into(),
            literal: literal.into(),
            min_tier: buzz_core::channel::ChannelTier::Private,
        }
    }

    #[tokio::test]
    async fn tree_gate_names_the_seal_label_and_never_the_literal() {
        let (_dir, head) = commit_tree(&[(
            "notes.md",
            b"kickoff with Aurora Dynamics GmbH next week\n".as_slice(),
        )])
        .await;
        let seals = [private_seal(
            "00112233aabbccdd",
            "the Zurich client",
            "Aurora Dynamics GmbH",
        )];
        let found = scan_checkpoint_tree(_dir.path(), &head, &seals)
            .await
            .expect("scan");

        assert_eq!(found.seals, vec!["the Zurich client in notes.md"]);
        assert!(found.secrets.is_empty(), "{:?}", found.secrets);
        // The whole point of the label: the refusal travels to a member in
        // the looser channel, so it must not carry the value.
        let msg = PromoteError::SealedFindings(found.seals).reject_message();
        assert!(
            !msg.contains("Aurora Dynamics GmbH"),
            "refusal echoed the sealed literal: {msg}"
        );
    }

    #[tokio::test]
    async fn tree_gate_reports_one_finding_per_file_not_per_occurrence() {
        let (_dir, head) = commit_tree(&[
            ("a.md", b"Aurora Dynamics GmbH ".repeat(40).as_slice()),
            ("b.md", b"once: Aurora Dynamics GmbH\n".as_slice()),
            ("clean.md", b"nothing to see\n".as_slice()),
        ])
        .await;
        let seals = [private_seal(
            "00112233aabbccdd",
            "the Zurich client",
            "Aurora Dynamics GmbH",
        )];
        let mut found = scan_checkpoint_tree(_dir.path(), &head, &seals)
            .await
            .expect("scan")
            .seals;
        found.sort();
        assert_eq!(
            found,
            vec!["the Zurich client in a.md", "the Zurich client in b.md"]
        );
    }

    #[tokio::test]
    async fn tree_gate_is_a_no_op_when_the_target_tier_violates_no_seal() {
        // What an `owned` target gets: the caller filters to an empty set,
        // so the same tree that trips above passes untouched.
        let (_dir, head) = commit_tree(&[(
            "notes.md",
            b"kickoff with Aurora Dynamics GmbH next week\n".as_slice(),
        )])
        .await;
        let found = scan_checkpoint_tree(_dir.path(), &head, &[])
            .await
            .expect("scan");
        assert!(found.seals.is_empty(), "{:?}", found.seals);
    }

    #[tokio::test]
    async fn tree_gate_skips_binary_blobs_for_seals_as_it_does_for_secrets() {
        // Not a wish — a recorded blind spot. Binary blobs are skipped by
        // the D30 scan and the seal scan alike; if that ever changes, this
        // test should be inverted rather than deleted.
        let mut blob = b"\x00\x01\x02binary\x00".to_vec();
        blob.extend_from_slice(b"Aurora Dynamics GmbH");
        let (_dir, head) = commit_tree(&[("logo.bin", blob.as_slice())]).await;
        let seals = [private_seal(
            "00112233aabbccdd",
            "the Zurich client",
            "Aurora Dynamics GmbH",
        )];
        let found = scan_checkpoint_tree(_dir.path(), &head, &seals)
            .await
            .expect("scan");
        assert!(found.seals.is_empty(), "{:?}", found.seals);
    }
}

#[cfg(test)]
mod s3_probe_tests {
    //! Full-path probe: the D29/D30 exit criterion — a personal-channel
    //! thread promotes through the gate scaffold into a team channel; files
    //! and summary arrive, the conversation stays, the source closes. A
    //! secret-bearing checkpoint is refused with nothing transferred.
    //! Gated like the store probes — run with:
    //!   `BUZZ_GIT_S3_PROBE=1 cargo test -p buzz-relay --lib \
    //!    promote::s3_probe_tests -- --ignored`
    //! Pre-req: `docker compose up minio` (bucket `buzz-git`) + Postgres.
    use super::*;
    use buzz_core::kind::{
        KIND_WORK_THREAD_CHECKPOINT, KIND_WORK_THREAD_PROMOTE, KIND_WORK_THREAD_PROMOTED,
    };
    use buzz_db::work_thread::WorkThreadStatus;
    use buzz_db::CreateCommunityWithOwnerResult;
    use nostr::Tag;

    use crate::handlers::ingest::{HttpAuthMethod, IngestAuth};

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
        let (state, _audit_shutdown) = crate::state::AppState::new(
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

    /// Seed a relay-owned channel repo with one commit holding `files`
    /// (optionally parented on `parent_commit`), published through the CAS
    /// substrate. Returns the commit id.
    async fn seed_repo(
        state: &Arc<AppState>,
        tenant: &TenantContext,
        repo_name: &str,
        branch: &str,
        files: &[(&str, &[u8])],
        parent_commit: Option<&str>,
    ) -> String {
        let relay_hex = state.relay_keypair.public_key().to_hex();
        let (repo, parent) = hydrate_for_write(
            &state.git_store,
            tenant,
            &relay_hex,
            repo_name,
            HydrationOptions {
                pack_cache: &state.git_pack_cache,
                scratch_dir: &state.config.git_repo_path,
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("hydrate repo");
        let repo_path = repo.path().to_path_buf();
        let git_dir = repo_path.to_string_lossy().into_owned();
        let work = tempfile::TempDir::new_in(&state.config.git_repo_path).expect("workdir");
        let index = work.path().join("seed-index");
        let index_str = index.to_string_lossy().into_owned();
        let work_str = work.path().to_string_lossy().into_owned();
        let env: Vec<(&str, &str)> = vec![
            ("GIT_DIR", git_dir.as_str()),
            ("GIT_WORK_TREE", work_str.as_str()),
            ("GIT_INDEX_FILE", index_str.as_str()),
            ("GIT_AUTHOR_NAME", "seed"),
            ("GIT_AUTHOR_EMAIL", "seed@test"),
            ("GIT_COMMITTER_NAME", "seed"),
            ("GIT_COMMITTER_EMAIL", "seed@test"),
        ];
        run_git_env(&repo_path, &["read-tree", "--empty"], &env, None)
            .await
            .expect("empty index");
        for (path, content) in files {
            let blob = run_git_env(
                &repo_path,
                &["hash-object", "-w", "--stdin"],
                &env,
                Some(content),
            )
            .await
            .expect("hash blob");
            run_git_env(
                &repo_path,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                &env,
                None,
            )
            .await
            .expect("index add");
        }
        let tree = run_git_env(&repo_path, &["write-tree"], &env, None)
            .await
            .expect("write tree");
        let mut commit_args: Vec<String> = vec![
            "commit-tree".into(),
            tree.clone(),
            "-m".into(),
            "seed".into(),
        ];
        if let Some(parent) = parent_commit {
            commit_args.push("-p".into());
            commit_args.push(parent.to_owned());
        }
        let arg_refs: Vec<&str> = commit_args.iter().map(String::as_str).collect();
        let commit = run_git_env(&repo_path, &arg_refs, &env, None)
            .await
            .expect("commit");
        run_git_env(
            &repo_path,
            &["update-ref", &format!("refs/heads/{branch}"), &commit],
            &[("GIT_DIR", git_dir.as_str())],
            None,
        )
        .await
        .expect("update ref");
        cas_publish(
            &state.git_store,
            tenant,
            &repo_path,
            &relay_hex,
            repo_name,
            &parent,
            PublishLimits {
                parent_hydrated_bytes: repo.hydrated_bytes(),
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("seed publish");
        commit
    }

    fn http_auth(keys: &nostr::Keys) -> IngestAuth {
        IngestAuth::Http {
            pubkey: keys.public_key(),
            scopes: buzz_auth::Scope::all_known(),
            auth_method: HttpAuthMethod::DevPubkey,
        }
    }

    fn signed(keys: &nostr::Keys, kind: u32, content: &str, tags: &[Vec<String>]) -> nostr::Event {
        let tags: Vec<Tag> = tags
            .iter()
            .map(|t| Tag::parse(t.iter().map(String::as_str)).expect("tag"))
            .collect();
        nostr::EventBuilder::new(nostr::Kind::Custom(kind as u16), content)
            .tags(tags)
            .sign_with_keys(keys)
            .expect("sign")
    }

    fn tag(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[tokio::test]
    #[ignore = "requires Postgres + MinIO (BUZZ_GIT_S3_PROBE=1)"]
    async fn promotion_moves_files_and_summary_not_conversation() {
        if !probe_enabled() {
            eprintln!("skipping: set BUZZ_GIT_S3_PROBE=1 to run against live MinIO");
            return;
        }
        let state = test_state().await;
        let ws_owner = nostr::Keys::generate();
        let member = nostr::Keys::generate();

        let host = format!("promote-s3-{}.example", Uuid::new_v4().simple());
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
        for keys in [&ws_owner, &member] {
            state
                .db
                .ensure_user(community, keys.public_key().to_bytes().as_ref())
                .await
                .expect("ensure user");
        }

        // Personal channel + team channel, both with bound repos.
        let personal_id = Uuid::new_v4();
        assert!(matches!(
            state
                .db
                .create_personal_channel(
                    community,
                    personal_id,
                    "my-space",
                    buzz_core::channel::ChannelType::Stream,
                    None,
                    &member.public_key().to_bytes(),
                    None,
                )
                .await
                .expect("personal channel"),
            buzz_db::personal_channel::CreatePersonalChannelResult::Created(_)
        ));
        let team = state
            .db
            .create_channel(
                community,
                "team",
                buzz_core::channel::ChannelType::Stream,
                buzz_core::channel::ChannelVisibility::Open,
                None,
                &ws_owner.public_key().to_bytes(),
                None,
            )
            .await
            .expect("team channel");
        state
            .db
            .add_member(
                community,
                team.id,
                &member.public_key().to_bytes(),
                buzz_core::channel::MemberRole::Member,
                Some(&ws_owner.public_key().to_bytes()),
            )
            .await
            .expect("team membership");
        let relay_hex = state.relay_keypair.public_key().to_hex();
        let source_repo = format!("p{}", personal_id.simple());
        let target_repo = format!("t{}", team.id.simple());
        assert!(state
            .db
            .bind_channel_repo(community, personal_id, &source_repo, &relay_hex)
            .await
            .expect("bind source repo"));
        assert!(state
            .db
            .bind_channel_repo(community, team.id, &target_repo, &relay_hex)
            .await
            .expect("bind target repo"));

        // Source: an ANCESTOR commit whose tree carries a secret that was
        // later deleted, then the clean checkpoint on top of it — the
        // history-exposure case the graft must never publish. Plus a
        // separate secret-TREE commit for the gate-refusal case.
        let history_secret_commit = seed_repo(
            &state,
            &tenant,
            &source_repo,
            "thread-work",
            &[("oops.env", b"AWS_KEY=AKIAIOSFODNN7EXAMPLE\n")],
            None,
        )
        .await;
        let clean_commit = seed_repo(
            &state,
            &tenant,
            &source_repo,
            "thread-work",
            &[("result.txt", b"the finished work\n")],
            Some(&history_secret_commit),
        )
        .await;
        let secret_commit = seed_repo(
            &state,
            &tenant,
            &source_repo,
            "thread-secrets",
            &[("oops.env", b"AWS_KEY=AKIAIOSFODNN7EXAMPLE\n")],
            None,
        )
        .await;
        // Target: a pre-existing main tip that must survive the graft.
        let _team_tip = seed_repo(
            &state,
            &tenant,
            &target_repo,
            "main",
            &[("README.md", b"team repo\n")],
            None,
        )
        .await;

        // The personal thread + its checkpoints.
        let personal_hex = personal_id.to_string();
        let root = signed(
            &member,
            buzz_core::kind::KIND_WORK_THREAD_OPEN,
            "private exploration",
            &[tag(&["h", &personal_hex])],
        );
        crate::handlers::side_effects::handle_side_effects(
            &tenant,
            buzz_core::kind::KIND_WORK_THREAD_OPEN,
            &root,
            &state,
        )
        .await
        .expect("47000 side effect");
        let root_hex = root.id.to_hex();
        for commit in [&clean_commit, &secret_commit] {
            let ckpt = signed(
                &member,
                KIND_WORK_THREAD_CHECKPOINT,
                "turn",
                &[
                    tag(&["e", &root_hex]),
                    tag(&["h", &personal_hex]),
                    tag(&["commit", commit]),
                ],
            );
            state
                .db
                .insert_event(community, &ckpt, Some(personal_id))
                .await
                .expect("store checkpoint");
        }

        // Gate case first: promoting the secret-bearing checkpoint is
        // refused; nothing is stored, nothing lands in the target.
        let team_hex = team.id.to_string();
        let gated = signed(
            &member,
            KIND_WORK_THREAD_PROMOTE,
            "clean summary",
            &[
                tag(&["e", &root_hex]),
                tag(&["h", &team_hex]),
                tag(&["from", &personal_hex]),
                tag(&["commit", &secret_commit]),
            ],
        );
        let res = crate::handlers::work_thread::handle_thread_promote(
            &tenant,
            &state,
            &gated,
            &http_auth(&member),
        )
        .await;
        match res {
            Err(crate::handlers::ingest::IngestError::Rejected(msg)) => {
                assert!(msg.contains("privacy gate"), "gate must refuse: {msg}");
                assert!(msg.contains("aws-access-key-id in oops.env"));
                assert!(
                    !msg.contains("AKIA"),
                    "rejection must never echo the secret value"
                );
            }
            other => panic!("expected gate rejection, got {other:?}"),
        }
        assert!(
            state
                .db
                .get_event_by_id(community, gated.id.as_bytes())
                .await
                .expect("event lookup")
                .is_none(),
            "gated promotion must not store the event"
        );

        // silent-mesh (D31): the same promotion, blocked by a Content Seal
        // instead of a credential shape. The team channel is `open` and the
        // seal's floor is `private`, so the value may not travel there.
        //
        // This also pins the DIRECTION of the tier filter. The source is a
        // personal channel, which D24 forces to `owned`, and no seal can be
        // violated at `owned` — so a call site that filtered by the source
        // tier instead of the target's would produce an empty seal set and
        // let both cases below through.
        const SEALED: &str = "Aurora Dynamics GmbH";
        state
            .db
            .create_seal(buzz_db::seal::CreateSealParams {
                community_id: community,
                id: "00112233aabbccdd",
                label: "the Zurich client",
                literal: SEALED,
                min_tier: buzz_core::channel::ChannelTier::Private,
                created_by: &ws_owner.public_key().to_bytes(),
            })
            .await
            .expect("create seal");

        let sealed_tree_commit = seed_repo(
            &state,
            &tenant,
            &source_repo,
            "thread-sealed",
            &[("notes.md", format!("kickoff with {SEALED}\n").as_bytes())],
            None,
        )
        .await;
        let ckpt = signed(
            &member,
            KIND_WORK_THREAD_CHECKPOINT,
            "turn",
            &[
                tag(&["e", &root_hex]),
                tag(&["h", &personal_hex]),
                tag(&["commit", &sealed_tree_commit]),
            ],
        );
        state
            .db
            .insert_event(community, &ckpt, Some(personal_id))
            .await
            .expect("store sealed checkpoint");

        // Both halves of the gate: the member-written summary, and a file
        // in the promoted tree.
        for (case, summary, commit) in [
            (
                "summary",
                format!("work for {SEALED}, ready to share"),
                clean_commit.clone(),
            ),
            (
                "tree",
                "clean summary".to_owned(),
                sealed_tree_commit.clone(),
            ),
        ] {
            let sealed_promote = signed(
                &member,
                KIND_WORK_THREAD_PROMOTE,
                &summary,
                &[
                    tag(&["e", &root_hex]),
                    tag(&["h", &team_hex]),
                    tag(&["from", &personal_hex]),
                    tag(&["commit", &commit]),
                ],
            );
            let res = crate::handlers::work_thread::handle_thread_promote(
                &tenant,
                &state,
                &sealed_promote,
                &http_auth(&member),
            )
            .await;
            match res {
                Err(crate::handlers::ingest::IngestError::Rejected(msg)) => {
                    assert!(
                        msg.contains("sealed value"),
                        "{case}: seal must refuse: {msg}"
                    );
                    assert!(
                        msg.contains("the Zurich client"),
                        "{case}: refusal must name the seal's label: {msg}"
                    );
                    assert!(
                        !msg.contains(SEALED),
                        "{case}: refusal must never echo the sealed literal: {msg}"
                    );
                }
                other => panic!("{case}: expected seal rejection, got {other:?}"),
            }
            assert!(
                state
                    .db
                    .get_event_by_id(community, sealed_promote.id.as_bytes())
                    .await
                    .expect("event lookup")
                    .is_none(),
                "{case}: sealed promotion must not store the event"
            );
        }

        // The real promotion: files + summary move, conversation does not.
        // Reaching here with a seal registered also shows the guard does
        // not block clean content.
        let promote = signed(
            &member,
            KIND_WORK_THREAD_PROMOTE,
            "Parser fix, tested end to end",
            &[
                tag(&["e", &root_hex]),
                tag(&["h", &team_hex]),
                tag(&["from", &personal_hex]),
                tag(&["commit", &clean_commit]),
            ],
        );
        let result = crate::handlers::work_thread::handle_thread_promote(
            &tenant,
            &state,
            &promote,
            &http_auth(&member),
        )
        .await
        .expect("promotion succeeds");
        assert!(result.message.contains("\"source_closed\":true"));

        // New thread in the target channel, goal = the summary.
        let new_thread = state
            .db
            .get_work_thread(community, promote.id.as_bytes())
            .await
            .expect("get new thread")
            .expect("new thread projected");
        assert_eq!(new_thread.channel_id, team.id);
        assert_eq!(new_thread.goal, "Parser fix, tested end to end");
        assert_eq!(new_thread.status, WorkThreadStatus::Open);

        // Source thread closed; relay-signed 47014 notice in the source
        // channel records it for event-folding clients.
        let source_thread = state
            .db
            .get_work_thread(community, root.id.as_bytes())
            .await
            .expect("get source thread")
            .expect("source exists");
        assert_eq!(source_thread.status, WorkThreadStatus::Closed);
        let pool = sqlx::PgPool::connect(&state.config.database_url)
            .await
            .expect("pg pool");
        let notices: Vec<(Vec<u8>, serde_json::Value)> = sqlx::query_as(
            "SELECT pubkey, tags FROM events \
             WHERE community_id = $1 AND kind = $2 AND deleted_at IS NULL",
        )
        .bind(community.as_uuid())
        .bind(KIND_WORK_THREAD_PROMOTED as i32)
        .fetch_all(&pool)
        .await
        .expect("query notices");
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].0,
            state.relay_keypair.public_key().to_bytes().to_vec(),
            "notice must be relay-signed"
        );

        // The target main tip carries the promoted tree AND the original
        // team file; the temporary transfer ref is gone.
        let (repo, _parent) = hydrate_for_write(
            &state.git_store,
            &tenant,
            &relay_hex,
            &target_repo,
            HydrationOptions {
                pack_cache: &state.git_pack_cache,
                scratch_dir: &state.config.git_repo_path,
                max_pack_bytes: state.config.git_max_pack_bytes,
                max_repo_bytes: state.config.git_max_repo_bytes,
            },
        )
        .await
        .expect("rehydrate target");
        let listing = run_git_env(
            repo.path(),
            &["ls-tree", "-r", "refs/heads/main", "--name-only"],
            &[],
            None,
        )
        .await
        .expect("list target tree");
        let new_hex = promote.id.to_hex();
        let expected_path = format!("promoted/{}/result.txt", &new_hex[..12]);
        assert!(
            listing.lines().any(|l| l == expected_path),
            "promoted file must land under promoted/<short>/: {listing}"
        );
        assert!(listing.lines().any(|l| l == "README.md"));
        let refs = run_git_env(
            repo.path(),
            &["for-each-ref", "--format=%(refname)"],
            &[],
            None,
        )
        .await
        .expect("list refs");
        assert!(
            !refs.contains("refs/tmp/promote"),
            "transfer ref must not be published: {refs}"
        );

        // The source's git history must NOT cross the privacy boundary:
        // neither the checkpoint commit nor its secret-bearing ancestor
        // exists anywhere in the published target repo (the graft commit
        // has no source parent, so the reachability-based CAS pack never
        // ships them) — `git show <ancestor>:oops.env` is impossible for
        // target-channel members.
        let all_objects = run_git_env(
            repo.path(),
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
            &[],
            None,
        )
        .await
        .expect("list all target objects");
        for absent in [&clean_commit, &history_secret_commit, &secret_commit] {
            assert!(
                !all_objects.contains(absent.as_str()),
                "source commit {absent} must not be published into the target"
            );
        }
        let main_parents = run_git_env(
            repo.path(),
            &["rev-list", "--parents", "-n", "1", "refs/heads/main"],
            &[],
            None,
        )
        .await
        .expect("main parents");
        assert!(
            !main_parents.contains(clean_commit.as_str()),
            "graft commit must not parent the source checkpoint"
        );

        // Replaying the stored promotion is an idempotent duplicate.
        let replay = crate::handlers::work_thread::handle_thread_promote(
            &tenant,
            &state,
            &promote,
            &http_auth(&member),
        )
        .await
        .expect("replay");
        assert!(replay.message.contains("duplicate"));
    }
}
