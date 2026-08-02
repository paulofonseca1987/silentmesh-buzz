//! Work-thread git worktree engine (Silent Mesh Phase 2, final slice).
//!
//! The capability: an agent turn inside a **work thread** (a NIP-10 root
//! pointing at a kind:47000/47020 event, in a channel with a bound forge
//! repo) runs in a dedicated **git worktree** of that channel's repo on a
//! per-thread branch, and at the end of the turn the harness commits what
//! the agent changed, pushes the branch, and emits a kind:47010 checkpoint
//! — automating the per-turn trail the CLI's `threads checkpoint` produces
//! by hand.
//!
//! **Scope of this module.** It is the self-contained, tested engine —
//! provisioning ([`ThreadWorktrees::ensure_worktree`]), the end-of-turn
//! commit/push ([`ThreadWorktrees::checkpoint`]), the NIP-98 push-auth
//! config ([`push_auth_config`]), and the pure resolution helpers.
//!
//! The harness wiring that calls it lives in `pool.rs` and shipped
//! 2026-08-01: `resolve_turn_worktree` decides whether a turn earns a
//! worktree, `turn_session_key` gives a bound turn its own ACP session
//! (a session's cwd is fixed at `session/new`, and the worktree is the
//! different cwd), and `checkpoint_turn_worktree` publishes the kind:47010.
//! Opt-in via `BUZZ_ACP_WORKTREE_ROOT`.
//!
//! Everything here is **best-effort**, mirroring the relay's
//! canonicalize-on-close: any git failure returns `Err`/`None` and (once
//! wired) must be logged and skipped, never failing the agent's turn.
//! Every git subprocess has a wall-clock timeout and runs with system and
//! user gitconfig neutralized.
//!
//! Testability: the full git lifecycle (provision from a bound source repo
//! → worktree add → commit-or-skip → push → idempotent re-entry →
//! re-provision after external deletion → empty-repo soft-fail) is
//! exercised against local bare repos by the gated `probe_tests`; the pure
//! helpers have unit tests.
//!
//! **The auth path is covered by `forge_probe_tests`, and only there.**
//! Every `probe_tests` case passes a `repo_url_override`, which is exactly
//! the branch that skips `auth_cli_flags` and `apply_push_auth` — a local
//! bare repo needs no credentials — so those and `ensure_keyfile` have no
//! coverage from the local-repo probes at all, and only the pure
//! `push_auth_config` is unit-tested.
//!
//! That gap was expensive twice: the reverted wiring prototype cloned
//! unauthenticated and 401'd against every live relay, and the wiring that
//! replaced it hit three further failures in the same path, none a logic
//! error — git older than 2.46 cannot use the credential helper at all, a
//! relay bound to one address cannot reach its own policy endpoint over
//! loopback, and a shadowed `wc` made the fail-closed pre-receive hook
//! refuse every push. All three are invisible in production, because
//! worktree operations are best-effort so git can never fail an agent's
//! turn — which also means nothing announces when git cannot authenticate.
//! `forge_probe_tests` turns each into an assertion failure with git's own
//! error attached. It needs a live relay plus `git-credential-nostr` on
//! PATH, like the promote/canonicalize S3 probes.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

/// A `refs/heads/<branch>` name scoped to a work thread. Kept short and
/// filesystem/ref safe (`sm/thread/<first-12-hex>`), deterministic per
/// thread so re-entry reuses the same branch.
pub fn thread_branch(thread_root_hex: &str) -> String {
    let short: String = thread_root_hex
        .chars()
        .take(12)
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    format!("sm/thread/{short}")
}

/// Deterministic session-scope id for a `(channel, thread)` pair.
///
/// The pool keys ACP sessions by `Uuid`; when the wiring lands, a
/// work-thread turn needs its **own** session (its own cwd = its own
/// worktree), distinct from the channel's default session and from other
/// threads in the same channel. This gives a stable, collision-resistant
/// key without changing the map's type.
pub fn thread_scope_id(channel_id: Uuid, thread_root_hex: &str) -> Uuid {
    // SHA-256 over a fixed namespace + `channel:thread_root`, folded to 128
    // bits. Deterministic and collision-resistant; the namespace prefix
    // keeps it clear of any real channel UUID. (SHA-256 rather than
    // `Uuid::new_v5` to avoid pulling the uuid `v5` feature into the crate.)
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"silent-mesh/thread-scope/v1:");
    h.update(channel_id.as_bytes());
    h.update(b":");
    h.update(thread_root_hex.as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// The `(worktree path, branch)` an agent turn is bound to.
#[derive(Debug, Clone)]
pub struct WorktreeBinding {
    /// Absolute path the agent session's cwd is set to.
    pub path: PathBuf,
    /// The per-thread branch commits land on and are pushed to.
    pub branch: String,
}

/// Result of an end-of-turn checkpoint attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// A new commit was created and pushed. Carries the 40/64-hex oid.
    Committed { commit: String, branch: String },
    /// The worktree was clean — nothing to checkpoint this turn.
    NothingToCommit,
}

/// Per-git-subprocess wall-clock cap. A hung clone/fetch/push must not wedge
/// the caller (or, once wired, the agent slot) indefinitely — it times out,
/// the child is killed on drop, and the op becomes an ordinary soft failure.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Run a git subprocess in `cwd` with a hardened, minimal environment and a
/// wall-clock timeout. Returns trimmed stdout on success, an error string
/// otherwise.
///
/// The environment is fully neutralized against operator config that would
/// otherwise change behavior on the host: `GIT_CONFIG_NOSYSTEM` drops
/// `/etc/gitconfig`, and `GIT_CONFIG_GLOBAL=/dev/null` drops the user's
/// `~/.gitconfig` — without which a stray `commit.gpgsign=true`,
/// `core.hooksPath`, or a bespoke `credential.helper` would silently break
/// every checkpoint. Commit-shaped calls additionally pin signing/hooks off
/// via explicit `-c` flags at the call site.
async fn git(cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd).args(args).kill_on_drop(true);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
        Ok(r) => r.map_err(|e| format!("git {:?}: spawn: {e}", args.first().unwrap_or(&"")))?,
        Err(_) => {
            return Err(format!(
                "git {:?} timed out after {}s",
                args.first().unwrap_or(&""),
                GIT_TIMEOUT.as_secs()
            ))
        }
    };
    if !out.status.success() {
        return Err(format!(
            "git {:?} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The per-repo git config that authenticates pushes to the relay forge as
/// the agent, via the `git-credential-nostr` NIP-98 helper.
///
/// Returned as `(key, value)` pairs to apply with `git config`; kept a pure
/// function so the wiring is unit-testable without a live relay. Mirrors
/// the scheme `buzz-dev-mcp`'s shim builds, scoped to one repo:
/// - `credential.helper=nostr` + `credential.useHttpPath=true` — the relay
///   verifies NIP-98 against the full repo-root URL, so the path must ride
///   in the signed token.
/// - `nostr.keyfile` — the agent secret (hex), 0600, read by the helper.
/// - `nostr.authtag` — the NIP-OA owner attestation, which must sit inside
///   the signed event (git cannot add a separate header).
/// - identity for the commits the harness authors on the agent's behalf.
pub fn push_auth_config(
    keyfile: &str,
    agent_pubkey_hex: &str,
    relay_host: &str,
    auth_tag_json: Option<&str>,
) -> Vec<(String, String)> {
    let mut cfg = vec![
        ("credential.helper".into(), "nostr".into()),
        ("credential.useHttpPath".into(), "true".into()),
        ("nostr.keyfile".into(), keyfile.to_owned()),
        (
            "user.name".into(),
            format!(
                "agent-{}",
                &agent_pubkey_hex[..agent_pubkey_hex.len().min(8)]
            ),
        ),
        (
            "user.email".into(),
            format!("{agent_pubkey_hex}@{relay_host}"),
        ),
    ];
    if let Some(tag) = auth_tag_json {
        cfg.push(("nostr.authtag".into(), tag.to_owned()));
    }
    cfg
}

/// Derive the forge's HTTP(S) base from the harness's ws(s) relay URL.
/// `ws://h:p` → `http://h:p`, `wss://h:p` → `https://h:p`; anything else is
/// returned unchanged (already http-shaped).
pub fn forge_http_base(relay_url: &str) -> String {
    if let Some(rest) = relay_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = relay_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        relay_url.to_owned()
    }
    .trim_end_matches('/')
    .to_owned()
}

/// Host portion of a relay/forge URL, for the git author email. Falls back
/// to the whole string when there is no `//`.
pub fn url_host(url: &str) -> String {
    let after = url.split("://").nth(1).unwrap_or(url);
    after.split(['/', ':']).next().unwrap_or(after).to_owned()
}

/// Manages per-thread worktrees for one agent identity.
///
/// Holds only the pieces needed to provision and push: the config root,
/// the agent's own keys (for the push keyfile and commit identity), the
/// forge base URL, and the optional NIP-OA auth tag. Cheap to clone; the
/// filesystem is the source of truth for what is already provisioned, so
/// there is no in-memory cache to keep coherent across turns.
#[derive(Clone)]
pub struct ThreadWorktrees {
    root: PathBuf,
    agent_pubkey_hex: String,
    agent_secret_hex: String,
    forge_base: String,
    forge_host: String,
    auth_tag_json: Option<String>,
}

impl ThreadWorktrees {
    /// Build from resolved config + agent identity. `agent_secret_hex` is
    /// the 64-hex private key the harness already holds; it is written to a
    /// 0600 keyfile for the push credential helper and never logged.
    pub fn new(
        root: PathBuf,
        agent_pubkey_hex: String,
        agent_secret_hex: String,
        relay_url: &str,
        auth_tag_json: Option<String>,
    ) -> Self {
        Self {
            root,
            agent_pubkey_hex,
            agent_secret_hex,
            forge_base: forge_http_base(relay_url),
            forge_host: url_host(relay_url),
            auth_tag_json,
        }
    }

    fn channel_dir(&self, channel_id: Uuid) -> PathBuf {
        self.root.join(channel_id.to_string())
    }

    fn mirror_dir(&self, channel_id: Uuid) -> PathBuf {
        self.channel_dir(channel_id).join("repo.git")
    }

    fn worktree_dir(&self, channel_id: Uuid, thread_root_hex: &str) -> PathBuf {
        let short: String = thread_root_hex.chars().take(12).collect();
        self.channel_dir(channel_id).join("wt").join(short)
    }

    fn keyfile_path(&self) -> PathBuf {
        let short: String = self.agent_pubkey_hex.chars().take(16).collect();
        self.root.join(".keys").join(short)
    }

    /// The clone URL for a channel's bound forge repo.
    pub fn repo_url(&self, repo_owner_hex: &str, repo_name: &str) -> String {
        format!("{}/git/{repo_owner_hex}/{repo_name}", self.forge_base)
    }

    /// Write the agent secret to a 0600 keyfile the credential helper reads.
    /// Idempotent; returns the keyfile path as a string.
    ///
    /// The secret is a private key, so the file is created with 0600 from
    /// the outset (mode passed to `open`, not a chmod after `write`) — a
    /// write-then-chmod leaves a world-readable window, and a failed chmod
    /// would leave a 0644 secret behind. The containing `.keys` dir is
    /// likewise created 0700. Rare provisioning path (once per agent), so
    /// small blocking fs ops here are fine and avoid pulling tokio's `fs`
    /// feature into the crate.
    async fn ensure_keyfile(&self) -> Result<String, String> {
        use std::io::Write as _;
        let path = self.keyfile_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("keyfile dir: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path).map_err(|e| format!("open keyfile: {e}"))?;
        f.write_all(self.agent_secret_hex.as_bytes())
            .map_err(|e| format!("write keyfile: {e}"))?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Push-auth config as `-c key=value` CLI flags, for authenticating the
    /// initial clone (the clone itself hits the relay's NIP-98-gated forge,
    /// so auth cannot wait until after). The same values are persisted into
    /// the clone's config for later pushes.
    async fn auth_cli_flags(&self) -> Result<Vec<String>, String> {
        let keyfile = self.ensure_keyfile().await?;
        let mut flags = Vec::new();
        for (k, v) in push_auth_config(
            &keyfile,
            &self.agent_pubkey_hex,
            &self.forge_host,
            self.auth_tag_json.as_deref(),
        ) {
            flags.push("-c".to_owned());
            flags.push(format!("{k}={v}"));
        }
        Ok(flags)
    }

    /// Apply the push-auth git config to a mirror clone.
    async fn apply_push_auth(&self, mirror: &Path) -> Result<(), String> {
        let keyfile = self.ensure_keyfile().await?;
        for (k, v) in push_auth_config(
            &keyfile,
            &self.agent_pubkey_hex,
            &self.forge_host,
            self.auth_tag_json.as_deref(),
        ) {
            git(mirror, &["config", &k, &v], &[]).await?;
        }
        Ok(())
    }

    /// Ensure a worktree exists for `(channel, thread)` and return its
    /// binding. Provisions the channel mirror clone on first use, then adds
    /// (or reuses) the per-thread branch worktree. `repo_url_override` lets
    /// tests point at a local bare repo instead of the relay forge.
    pub async fn ensure_worktree(
        &self,
        channel_id: Uuid,
        repo_owner_hex: &str,
        repo_name: &str,
        thread_root_hex: &str,
        repo_url_override: Option<&str>,
    ) -> Result<WorktreeBinding, String> {
        let mirror = self.mirror_dir(channel_id);
        let branch = thread_branch(thread_root_hex);
        let worktree = self.worktree_dir(channel_id, thread_root_hex);

        // A non-bare clone keeps its metadata under `.git/`; that dir's
        // presence is the "already provisioned" marker. Because the clone
        // is run WITH the auth flags (below), a present `.git` means the
        // authenticated clone succeeded — the marker no longer conflates
        // "cloned" with "cloned-but-unauthenticated". A failed clone leaves
        // no `.git` (git cleans it up), so the next call retries.
        if !mirror.join(".git").exists() {
            if let Some(parent) = mirror.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("channel dir: {e}"))?;
            }
            let url = repo_url_override
                .map(str::to_owned)
                .unwrap_or_else(|| self.repo_url(repo_owner_hex, repo_name));
            let mirror_str = mirror.to_string_lossy().into_owned();
            // Authenticate the clone itself: the relay forge is NIP-98
            // gated, so a plain clone 401s. `-c` flags apply the credential
            // helper during the clone; a local `repo_url_override` (tests)
            // needs no auth.
            let mut args: Vec<String> = vec!["clone".into()];
            if repo_url_override.is_none() {
                args.extend(self.auth_cli_flags().await?);
            }
            args.push("--no-checkout".into());
            args.push(url);
            args.push(mirror_str);
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            git(&self.root, &arg_refs, &[]).await?;
            // Persist the same auth config for subsequent pushes.
            if repo_url_override.is_none() {
                self.apply_push_auth(&mirror).await?;
            }
            info!(target: "worktree", channel = %channel_id, "provisioned channel mirror clone");
        }

        // Prune any worktree entries whose dirs were removed out of band, so
        // a stale registration cannot block re-provisioning this thread.
        let _ = git(&mirror, &["worktree", "prune"], &[]).await;

        if worktree.join(".git").exists() {
            return Ok(WorktreeBinding {
                path: worktree,
                branch,
            });
        }

        // Fetch first so the branch add sees any commits pushed by other
        // turns/agents; ignore fetch errors (offline / empty remote).
        let _ = git(&mirror, &["fetch", "origin"], &[]).await;
        let worktree_str = worktree.to_string_lossy().into_owned();
        // Reuse the remote branch if it already exists, else base the new
        // branch on the default branch.
        let start_point = if git(
            &mirror,
            &["rev-parse", "--verify", &format!("origin/{branch}")],
            &[],
        )
        .await
        .is_ok()
        {
            format!("origin/{branch}")
        } else {
            match resolve_base(&mirror).await {
                Some(b) => b,
                // Empty repo (unborn HEAD, no default branch): there is no
                // commit to base a worktree on. Soft error → the caller
                // falls back to the ordinary channel session. A worktree
                // becomes possible once the channel repo has its first
                // commit.
                None => {
                    return Err(
                        "channel repo has no commits yet — nothing to base a worktree on".into(),
                    )
                }
            }
        };
        git(
            &mirror,
            &[
                "worktree",
                "add",
                "-B",
                &branch,
                &worktree_str,
                &start_point,
            ],
            &[],
        )
        .await?;
        info!(target: "worktree", channel = %channel_id, branch = %branch, "added thread worktree");
        Ok(WorktreeBinding {
            path: worktree,
            branch,
        })
    }

    /// Commit any agent changes in the worktree and push the branch.
    ///
    /// Returns [`CheckpointOutcome::NothingToCommit`] when the worktree is
    /// clean (a turn that changed no files), otherwise the new commit oid.
    /// Push failures are surfaced as `Err` so the caller can skip emitting a
    /// 47010 that points at an unfetchable commit.
    pub async fn checkpoint(&self, binding: &WorktreeBinding) -> Result<CheckpointOutcome, String> {
        let wt = &binding.path;
        git(wt, &["add", "-A"], &[]).await?;
        let status = git(wt, &["status", "--porcelain"], &[]).await?;
        if status.is_empty() {
            return Ok(CheckpointOutcome::NothingToCommit);
        }
        let msg = format!("checkpoint: {}", binding.branch);
        let email = format!("{}@{}", self.agent_pubkey_hex, self.forge_host);
        let name = format!(
            "agent-{}",
            &self.agent_pubkey_hex[..self.agent_pubkey_hex.len().min(8)]
        );
        // Identity is passed per-invocation so the commit is attributable
        // even when no repo-level user.* config was applied (local tests).
        let envs = [
            ("GIT_AUTHOR_NAME", name.as_str()),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_COMMITTER_NAME", name.as_str()),
            ("GIT_COMMITTER_EMAIL", email.as_str()),
        ];
        // Signing and hooks are pinned off explicitly (belt-and-suspenders
        // over `GIT_CONFIG_GLOBAL=/dev/null`): an auto-checkpoint must never
        // block on a passphrase prompt or a repo/user commit hook.
        git(
            wt,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--no-verify",
                "-m",
                &msg,
            ],
            &envs,
        )
        .await?;
        let oid = git(wt, &["rev-parse", "HEAD"], &[]).await?;
        git(
            wt,
            &[
                "push",
                "origin",
                &format!("HEAD:refs/heads/{}", binding.branch),
            ],
            &[],
        )
        .await
        .map_err(|e| format!("push: {e}"))?;
        Ok(CheckpointOutcome::Committed {
            commit: oid,
            branch: binding.branch.clone(),
        })
    }
}

/// A commit-ish to base a new thread branch on: the clone's recorded
/// default branch, else `origin/main`/`origin/master`. Returns `None` for
/// an empty repo (unborn HEAD, no remote branches) — there is nothing to
/// base a worktree on, and `"HEAD"` would be an invalid start point that
/// makes `worktree add` fail. Every returned value is a verified ref.
async fn resolve_base(mirror: &Path) -> Option<String> {
    if let Ok(sym) = git(
        mirror,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        &[],
    )
    .await
    {
        if let Some(b) = sym.strip_prefix("origin/") {
            let candidate = format!("origin/{b}");
            if git(mirror, &["rev-parse", "--verify", &candidate], &[])
                .await
                .is_ok()
            {
                return Some(candidate);
            }
        }
    }
    for candidate in ["origin/main", "origin/master"] {
        if git(mirror, &["rev-parse", "--verify", candidate], &[])
            .await
            .is_ok()
        {
            return Some(candidate.to_owned());
        }
    }
    None
}

/// What one turn changed, ready to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiff {
    /// Unified diff text, possibly cut short — see `truncated`.
    pub text: String,
    /// The commit's parent, absent for a root commit.
    pub parent: Option<String>,
    /// Whether `text` was cut to fit the size budget.
    pub truncated: bool,
}

/// Cut `text` to at most `max_bytes`, on a line boundary.
///
/// Kept pure and separate from the git call so the boundary cases — a diff
/// exactly at the limit, one whose first line already exceeds it, multi-byte
/// characters straddling the cut — are testable without a repository.
///
/// The cut is at a **line** boundary, not just a character one, because a
/// unified diff severed mid-hunk is not a smaller diff, it is a corrupt one:
/// a renderer that parses hunk headers will mis-associate every line after
/// the wound. Losing the tail is honest; misattributing it is not.
pub fn truncate_diff(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    // Search the **bytes**, not a string slice. `text[..max_bytes]` panics
    // outright when the budget lands inside a multi-byte character, which a
    // diff of non-ASCII source hits routinely — a test that walks every
    // budget over an accented line found it immediately.
    //
    // Byte indexing is safe here and the result is still a valid boundary:
    // UTF-8 encodes `\n` as a single byte that can never appear inside a
    // multi-byte sequence, so a newline's index is always on a character
    // boundary even though the budget may not be.
    match text.as_bytes()[..max_bytes]
        .iter()
        .rposition(|&b| b == b'\n')
    {
        Some(cut) => (text[..=cut].to_owned(), true),
        // A first line longer than the whole budget: emit nothing rather
        // than a fragment of one line, which would render as a bogus hunk.
        None => (String::new(), true),
    }
}

impl ThreadWorktrees {
    /// The remote a worktree pushes to, read back from the checkout itself.
    ///
    /// Read rather than reconstructed: the caller that needs it (publishing
    /// a diff) would otherwise have to carry the owner and repo name along
    /// beside the binding, and a rebuilt URL that drifts from the one git
    /// actually uses would point readers at a repo the commit is not in.
    /// Falls back to the forge base when the remote cannot be read, which is
    /// only reachable if the worktree is broken in ways that already
    /// prevented the push.
    pub async fn remote_url(&self, binding: &WorktreeBinding) -> String {
        git(&binding.path, &["remote", "get-url", "origin"], &[])
            .await
            .ok()
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .unwrap_or_else(|| self.forge_base.clone())
    }

    /// The unified diff a single commit introduced, bounded to `max_bytes`.
    ///
    /// `git show` rather than `<commit>^..<commit>` so a root commit — a
    /// thread branched from an empty repo — produces its contents instead of
    /// failing on a parent that does not exist.
    pub async fn diff_for_commit(
        &self,
        binding: &WorktreeBinding,
        commit: &str,
        max_bytes: usize,
    ) -> Result<TurnDiff, String> {
        let raw = git(
            &binding.path,
            &[
                "show",
                "--no-color",
                "--format=",
                "--unified=3",
                "--no-ext-diff",
                commit,
            ],
            &[],
        )
        .await?;
        // A missing parent is a root commit, not an error.
        let parent = git(&binding.path, &["rev-parse", &format!("{commit}^")], &[])
            .await
            .ok()
            .filter(|p| !p.is_empty());
        let (text, truncated) = truncate_diff(&raw, max_bytes);
        Ok(TurnDiff {
            text,
            parent,
            truncated,
        })
    }
}

/// Log a best-effort worktree failure without disturbing the turn.
pub fn log_soft_failure(context: &str, err: &str) {
    warn!(target: "worktree", "{context}: {err} (turn unaffected)");
}

/// The minimum git that can authenticate to the relay forge.
///
/// `git-credential-nostr` answers only when git advertises
/// `capability[]=authtype` in the credential protocol, which git added in
/// **2.46**. Older git never advertises it, the helper prints an empty
/// response and exits 0, and git falls through to prompting for a username —
/// which, with `GIT_TERMINAL_PROMPT=0`, is the terminal error
/// "could not read Username". See `crates/git-credential-nostr/README.md`.
const GIT_MIN_AUTHTYPE: (u32, u32) = (2, 46);

/// Whether `git --version` output describes a git that can do NIP-98 auth.
///
/// `None` means the version could not be parsed — reported as unknown rather
/// than guessed either way, since both a false "supported" (a doomed clone)
/// and a false "unsupported" (refusing a working setup) are worse than
/// saying so.
///
/// This exists because the failure it detects is **invisible**. Every
/// worktree operation is best-effort by design: a clone that cannot
/// authenticate is logged and skipped, the turn proceeds normally, and the
/// only symptom is that checkpoints never appear. On a host with old git
/// that looks exactly like "the feature isn't wired yet" — which is the
/// wrong conclusion, and an expensive one to reach twice.
pub fn git_supports_forge_auth(version_output: &str) -> Option<bool> {
    // "git version 2.43.0" / "git version 2.46.1.windows.1"
    let rest = version_output.split_whitespace().nth(2)?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor) >= GIT_MIN_AUTHTYPE)
}

/// Ask the `git` on PATH whether it can authenticate to the relay forge.
///
/// Returns `None` when git is absent or its version is unparseable.
pub async fn probe_git_forge_auth() -> Option<bool> {
    let out = Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    git_supports_forge_auth(&String::from_utf8_lossy(&out.stdout))
}

/// Is `kind` a work-thread root (kind:47000 open or kind:47020 fork)? Only
/// these anchor a worktree; an ordinary NIP-10 root (a kind:9 message) does
/// not.
pub fn is_work_thread_root_kind(kind: u64) -> bool {
    matches!(kind, 47000 | 47020)
}

/// Extract `(owner_hex, repo_name)` for a channel's bound forge repo from a
/// set of kind:30617 repo-announcement events.
///
/// The relay announces each channel repo as a kind:30617 whose author is
/// the repo owner and whose `d` tag is the repo name, carrying a
/// `["buzz-channel", "<uuid>"]` tag binding it to the channel — the same
/// shape the pre-receive policy reads. Pure over the parsed JSON so it is
/// testable without a relay.
///
/// # Why the author must be checked
///
/// `expected_owner_hex` is the **known relay owner** pubkey, and an
/// announcement signed by anyone else is ignored. Without that check this
/// resolves to the first announcement carrying a matching `buzz-channel`
/// tag *whoever signed it*, and returns that author as the repo owner — so
/// the caller clones from, and pushes to, `<forge>/<author>/<d>`.
///
/// Publishing a kind:30617 only needs `Scope::ReposWrite`, which the relay
/// grants to ordinary members; it does not restrict this kind to itself.
/// So any member could publish an announcement naming someone else's
/// channel and silently redirect that channel's agent worktrees into a repo
/// they own — turning private-channel agent work into a push to an
/// attacker-controlled remote. Every legitimate announcement on a live
/// relay is relay-signed, so requiring it costs nothing and closes that.
///
/// Fail-closed: an empty or malformed `expected_owner_hex` matches nothing.
pub fn repo_binding_from_events(
    events: &[serde_json::Value],
    channel_id: Uuid,
    expected_owner_hex: &str,
) -> Option<(String, String)> {
    if expected_owner_hex.is_empty() {
        return None;
    }
    let want = channel_id.to_string();
    for ev in events {
        if ev.get("kind").and_then(|v| v.as_u64()) != Some(30617) {
            continue;
        }
        // Author first: a mismatched signer is skipped rather than
        // returning `None`, so one forged announcement cannot hide the
        // genuine one behind it.
        if !ev
            .get("pubkey")
            .and_then(|v| v.as_str())
            .is_some_and(|a| a.eq_ignore_ascii_case(expected_owner_hex))
        {
            continue;
        }
        let Some(tags) = ev.get("tags").and_then(|v| v.as_array()) else {
            continue;
        };
        let tag_val = |name: &str| -> Option<String> {
            tags.iter().find_map(|t| {
                let t = t.as_array()?;
                (t.first()?.as_str()? == name).then(|| t.get(1)?.as_str().map(str::to_owned))?
            })
        };
        if tag_val("buzz-channel").as_deref() != Some(want.as_str()) {
            continue;
        }
        // `continue`, not `?`: bailing out of the whole scan on one
        // announcement that lacks a `d` tag would let a malformed event
        // suppress a valid one later in the set.
        let Some(name) = tag_val("d") else { continue };
        return Some((expected_owner_hex.to_owned(), name));
    }
    None
}

/// Per-channel repo bindings, resolved once and reused.
///
/// The binding is a kind:30617 announcement made when the channel was
/// created; it effectively never changes, while a turn happens constantly.
/// Re-querying it every turn would add a relay round trip to the hot path
/// for a fact that is stable.
///
/// Negative results are cached too. A channel with no bound repo is the
/// common case — most channels are conversations — and without caching the
/// miss, every turn in every such channel pays two queries to learn nothing.
#[derive(Debug, Default, Clone)]
pub struct RepoBindingCache {
    entries: std::collections::HashMap<Uuid, Option<(String, String)>>,
}

impl RepoBindingCache {
    pub fn get(&self, channel_id: &Uuid) -> Option<&Option<(String, String)>> {
        self.entries.get(channel_id)
    }

    pub fn insert(&mut self, channel_id: Uuid, binding: Option<(String, String)>) {
        self.entries.insert(channel_id, binding);
    }

    /// Forget a channel's binding — used when the harness leaves a channel,
    /// so a rejoin re-resolves rather than trusting a stale answer.
    pub fn forget(&mut self, channel_id: &Uuid) {
        self.entries.remove(channel_id);
    }
}

/// Decide whether a turn earns a worktree, and which repo it binds to.
///
/// This is the whole admission rule in one place, so the harness call site
/// cannot get the ordering wrong and so every clause is testable without a
/// relay. Both inputs are query results the caller already has to fetch:
/// `root_events` is whatever came back for the thread-root id, and
/// `repo_events` the channel's kind:30617 announcements.
///
/// Returns `Some((repo_owner_hex, repo_name))` only when **every** clause
/// holds; any doubt returns `None` and the turn runs in the harness cwd as it
/// does today. That is the safe direction: a turn that should have had a
/// worktree and didn't merely loses its per-turn checkpoint, while a turn
/// given the *wrong* worktree pushes a private channel's work to someone
/// else's repo.
///
/// The clauses, in order:
///
/// 1. **There is a thread root at all.** A bare channel message has none, and
///    an empty or non-hex id is treated as absent rather than looked up.
/// 2. **The root is a work-thread root** (kind:47000 or :47020). An ordinary
///    NIP-10 thread on a kind:9 message is a conversation, not a task, and
///    must not provision a branch. If the root event was not returned by the
///    query its kind is unknown — fail closed, because "unknown kind" and
///    "not a work thread" must not be distinguishable in the safe direction.
/// 3. **The channel has a repo bound by a relay-signed announcement**, via
///    [`repo_binding_from_events`], which is where the author check lives.
///
/// `relay_self_hex` is the relay's own signing pubkey — see
/// [`crate::relay::RestClient::relay_self_pubkey`]. `None` (undiscoverable)
/// fails closed here rather than at the git layer, so a relay whose NIP-11 is
/// unreachable simply never provisions worktrees.
pub fn worktree_target_for_turn(
    channel_id: Uuid,
    thread_root_hex: Option<&str>,
    root_events: &[serde_json::Value],
    repo_events: &[serde_json::Value],
    relay_self_hex: Option<&str>,
) -> Option<(String, String)> {
    let root = thread_root_hex?.trim();
    if root.len() != 64 || !root.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let kind = event_kind_by_id(root_events, root)?;
    if !is_work_thread_root_kind(kind) {
        return None;
    }
    repo_binding_from_events(repo_events, channel_id, relay_self_hex?)
}

/// Extract the kind of the event with id `id_hex` from a query result set.
pub fn event_kind_by_id(events: &[serde_json::Value], id_hex: &str) -> Option<u64> {
    events.iter().find_map(|ev| {
        (ev.get("id").and_then(|v| v.as_str()) == Some(id_hex))
            .then(|| ev.get("kind").and_then(|v| v.as_u64()))
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_name_is_ref_safe_and_short() {
        assert_eq!(
            thread_branch("abcdef0123456789ffffffff"),
            "sm/thread/abcdef012345"
        );
        // Non-hex junk is dropped; short input is tolerated.
        assert_eq!(thread_branch("ab"), "sm/thread/ab");
    }

    #[test]
    fn scope_id_is_stable_and_distinct() {
        let ch = Uuid::from_u128(1);
        let a = thread_scope_id(ch, &"a".repeat(64));
        let b = thread_scope_id(ch, &"b".repeat(64));
        assert_eq!(a, thread_scope_id(ch, &"a".repeat(64)), "stable");
        assert_ne!(a, b, "distinct threads → distinct scopes");
        assert_ne!(a, ch, "never collides with the channel id");
        // Same thread in a different channel is a different scope.
        assert_ne!(a, thread_scope_id(Uuid::from_u128(2), &"a".repeat(64)));
    }

    #[test]
    fn forge_base_and_host_derivation() {
        assert_eq!(
            forge_http_base("ws://localhost:3999"),
            "http://localhost:3999"
        );
        assert_eq!(
            forge_http_base("wss://relay.example/"),
            "https://relay.example"
        );
        assert_eq!(forge_http_base("http://x:1"), "http://x:1");
        assert_eq!(url_host("ws://localhost:3999"), "localhost");
        assert_eq!(url_host("wss://relay.example/git"), "relay.example");
    }

    #[test]
    fn push_auth_config_carries_the_nip98_scheme() {
        let cfg = push_auth_config(
            "/k/agent",
            "ab".repeat(32).as_str(),
            "relay.example",
            Some(r#"["auth","owner","cond","sig"]"#),
        );
        let get = |k: &str| cfg.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
        assert_eq!(get("credential.helper"), Some("nostr"));
        assert_eq!(get("credential.useHttpPath"), Some("true"));
        assert_eq!(get("nostr.keyfile"), Some("/k/agent"));
        assert_eq!(
            get("nostr.authtag"),
            Some(r#"["auth","owner","cond","sig"]"#)
        );
        assert!(get("user.email").unwrap().ends_with("@relay.example"));
        // No auth tag → no authtag key.
        let cfg2 = push_auth_config("/k", &"c".repeat(64), "h", None);
        assert!(cfg2.iter().all(|(k, _)| k != "nostr.authtag"));
    }

    #[test]
    fn repo_binding_and_kind_parsing() {
        let channel = Uuid::from_u128(7);
        let events = serde_json::json!([
            { "id": "aa", "kind": 47000, "pubkey": "member", "tags": [["h", channel.to_string()]] },
            { "id": "bb", "kind": 30617, "pubkey": "relayowner", "tags": [
                ["d", "chan-repo"],
                ["buzz-channel", channel.to_string()]
            ]},
            { "id": "cc", "kind": 30617, "pubkey": "other", "tags": [
                ["d", "unrelated"],
                ["buzz-channel", Uuid::from_u128(99).to_string()]
            ]},
        ]);
        let evs = events.as_array().unwrap();
        assert_eq!(
            repo_binding_from_events(evs, channel, "relayowner"),
            Some(("relayowner".into(), "chan-repo".into()))
        );
        assert_eq!(
            repo_binding_from_events(evs, Uuid::from_u128(123), "relayowner"),
            None
        );
        assert_eq!(event_kind_by_id(evs, "aa"), Some(47000));
        assert_eq!(event_kind_by_id(evs, "zz"), None);
        assert!(is_work_thread_root_kind(47000) && is_work_thread_root_kind(47020));
        assert!(!is_work_thread_root_kind(9) && !is_work_thread_root_kind(47001));
    }

    // ── truncate_diff ───────────────────────────────────────────────────

    #[test]
    fn a_diff_within_budget_is_untouched() {
        let d = "diff --git a/x b/x\n+one\n";
        assert_eq!(truncate_diff(d, 1024), (d.to_owned(), false));
        // Exactly at the limit is still within it.
        assert_eq!(truncate_diff(d, d.len()), (d.to_owned(), false));
    }

    /// A unified diff cut mid-hunk is not a smaller diff, it is a corrupt
    /// one — a renderer parsing hunk headers mis-associates everything after
    /// the wound. Losing the tail is honest; misattributing it is not.
    #[test]
    fn truncation_lands_on_a_line_boundary() {
        let d = "aaaa\nbbbb\ncccc\n";
        let (out, truncated) = truncate_diff(d, 12);
        assert!(truncated);
        assert_eq!(out, "aaaa\nbbbb\n", "must not cut inside a line");
        assert!(out.ends_with('\n'));
    }

    /// Multi-byte characters must not be split. A newline is a single byte
    /// and can never sit inside one, so cutting at a newline is always a
    /// valid boundary — this pins that reasoning rather than assuming it.
    #[test]
    fn truncation_never_splits_a_multibyte_character() {
        let d = "+é ligature ﬁ\n+second line here\n";
        for budget in 1..d.len() {
            let (out, _) = truncate_diff(d, budget);
            assert!(d.starts_with(&out), "budget {budget} produced a non-prefix");
        }
    }

    /// A single line longer than the whole budget yields nothing rather than
    /// a fragment, which would render as a bogus hunk.
    #[test]
    fn a_first_line_over_budget_yields_nothing() {
        let (out, truncated) = truncate_diff("one enormous line with no newline", 8);
        assert_eq!(out, "");
        assert!(truncated, "and it must say it was truncated");
    }

    /// The boundary is exact and load-bearing: 2.45 cannot authenticate to
    /// the forge and 2.46 can, so an off-by-one here silently disables (or
    /// silently promises) the whole feature.
    #[test]
    fn git_forge_auth_support_is_detected_at_the_2_46_boundary() {
        for (v, want) in [
            ("git version 2.39.5", false),
            ("git version 2.43.0", false),
            ("git version 2.45.9", false),
            ("git version 2.46.0", true),
            ("git version 2.47.1", true),
            ("git version 3.0.0", true),
            ("git version 2.46.1.windows.1", true),
        ] {
            assert_eq!(git_supports_forge_auth(v), Some(want), "{v}");
        }
    }

    /// Unknown must stay unknown. Guessing "supported" schedules a doomed
    /// clone; guessing "unsupported" refuses a host that would have worked.
    #[test]
    fn an_unparseable_git_version_is_unknown_not_assumed() {
        for v in ["", "git", "git version", "git version x.y", "nonsense"] {
            assert_eq!(git_supports_forge_auth(v), None, "{v:?}");
        }
    }

    // ── worktree_target_for_turn: the admission rule ────────────────────
    //
    // Each test below kills exactly one clause. They are written so that
    // deleting any single check in `worktree_target_for_turn` turns one of
    // them red — a suite where the happy path alone passes would let the
    // whole rule be silently removed.

    fn relay_self() -> String {
        "6e32e3a8".repeat(8)
    }

    fn root_id() -> String {
        "ab".repeat(32)
    }

    fn root_event(kind: u64) -> serde_json::Value {
        serde_json::json!([{ "id": root_id(), "kind": kind }])
    }

    fn repo_event(channel: Uuid, signer: &str) -> serde_json::Value {
        serde_json::json!([{ "id": "repo", "kind": 30617, "pubkey": signer, "tags": [
            ["d", "chan-repo"], ["buzz-channel", channel.to_string()]
        ]}])
    }

    #[test]
    fn a_work_thread_root_with_a_relay_signed_repo_gets_a_worktree() {
        let channel = Uuid::from_u128(11);
        for kind in [47000, 47020] {
            assert_eq!(
                worktree_target_for_turn(
                    channel,
                    Some(&root_id()),
                    root_event(kind).as_array().unwrap(),
                    repo_event(channel, &relay_self()).as_array().unwrap(),
                    Some(&relay_self()),
                ),
                Some((relay_self(), "chan-repo".into())),
                "kind {kind} is a work-thread root"
            );
        }
    }

    /// An ordinary NIP-10 thread on a chat message is a conversation, not a
    /// task. Provisioning a branch for one would put a worktree behind every
    /// reply in the channel.
    #[test]
    fn an_ordinary_message_thread_gets_no_worktree() {
        let channel = Uuid::from_u128(11);
        assert_eq!(
            worktree_target_for_turn(
                channel,
                Some(&root_id()),
                root_event(9).as_array().unwrap(),
                repo_event(channel, &relay_self()).as_array().unwrap(),
                Some(&relay_self()),
            ),
            None
        );
    }

    /// "The root event wasn't in the query result" must be indistinguishable
    /// from "not a work thread" — both are absence of proof, and only proof
    /// may provision.
    #[test]
    fn an_unresolvable_root_kind_fails_closed() {
        let channel = Uuid::from_u128(11);
        let empty: Vec<serde_json::Value> = vec![];
        assert_eq!(
            worktree_target_for_turn(
                channel,
                Some(&root_id()),
                &empty,
                repo_event(channel, &relay_self()).as_array().unwrap(),
                Some(&relay_self()),
            ),
            None
        );
    }

    #[test]
    fn a_turn_with_no_thread_root_gets_no_worktree() {
        let channel = Uuid::from_u128(11);
        for root in [None, Some(""), Some("not-hex"), Some("abc")] {
            assert_eq!(
                worktree_target_for_turn(
                    channel,
                    root,
                    root_event(47000).as_array().unwrap(),
                    repo_event(channel, &relay_self()).as_array().unwrap(),
                    Some(&relay_self()),
                ),
                None,
                "root {root:?} must not provision"
            );
        }
    }

    /// If the relay's own key could not be discovered there is nothing to
    /// check an announcement's author against, so no binding may be trusted —
    /// the redirect hole is exactly what an unknown expected-owner reopens.
    ///
    /// This asserts the *behaviour*, not a specific layer, and deliberately
    /// so: the guard here is redundant with `repo_binding_from_events`'s own
    /// empty-`expected_owner_hex` check. A mutation run confirms it —
    /// replacing `relay_self_hex?` with `unwrap_or("")` leaves this test
    /// green, because the engine still refuses. Two independent refusals are
    /// worth keeping; claiming this test pins the outer one would not be.
    #[test]
    fn an_undiscoverable_relay_identity_fails_closed() {
        let channel = Uuid::from_u128(11);
        assert_eq!(
            worktree_target_for_turn(
                channel,
                Some(&root_id()),
                root_event(47000).as_array().unwrap(),
                repo_event(channel, &relay_self()).as_array().unwrap(),
                None,
            ),
            None
        );
    }

    /// The author check must still bite through this entry point, not only
    /// when `repo_binding_from_events` is called directly.
    #[test]
    fn a_forged_repo_announcement_is_refused_through_the_admission_rule() {
        let channel = Uuid::from_u128(11);
        let attacker = "deadbeef".repeat(8);
        assert_eq!(
            worktree_target_for_turn(
                channel,
                Some(&root_id()),
                root_event(47000).as_array().unwrap(),
                repo_event(channel, &attacker).as_array().unwrap(),
                Some(&relay_self()),
            ),
            None
        );
    }

    /// The redirection this resolver exists to refuse.
    ///
    /// Publishing a kind:30617 needs only `Scope::ReposWrite`, which the
    /// relay grants to ordinary members — it does not reserve this kind for
    /// itself. So a member can announce a repo they own, tagged with someone
    /// else's channel. Resolving that would clone from and push to
    /// `<forge>/<attacker>/<their-repo>`, sending a private channel's agent
    /// work to a remote they control.
    #[test]
    fn a_forged_announcement_cannot_redirect_a_channels_repo() {
        let channel = Uuid::from_u128(7);
        let relay_owner = "6e32e3a8".repeat(8);
        let attacker = "deadbeef".repeat(8);
        let events = serde_json::json!([
            // The attacker's announcement is FIRST, so a resolver that took
            // the first match would take this one.
            { "id": "evil", "kind": 30617, "pubkey": attacker, "tags": [
                ["d", "exfil"], ["buzz-channel", channel.to_string()]
            ]},
            { "id": "good", "kind": 30617, "pubkey": relay_owner, "tags": [
                ["d", "chan-repo"], ["buzz-channel", channel.to_string()]
            ]},
        ]);
        let evs = events.as_array().unwrap();

        // The genuine announcement is still found — the forged one is
        // skipped, not treated as a reason to give up.
        assert_eq!(
            repo_binding_from_events(evs, channel, &relay_owner),
            Some((relay_owner.clone(), "chan-repo".into()))
        );

        // And with only the forged announcement present, there is no binding
        // at all rather than a poisoned one.
        let evil_only = serde_json::json!([
            { "id": "evil", "kind": 30617, "pubkey": attacker, "tags": [
                ["d", "exfil"], ["buzz-channel", channel.to_string()]
            ]},
        ]);
        assert_eq!(
            repo_binding_from_events(evil_only.as_array().unwrap(), channel, &relay_owner),
            None
        );

        // Fail closed when the caller has no relay owner to compare against:
        // an unknown owner must not mean "trust anybody".
        assert_eq!(repo_binding_from_events(evs, channel, ""), None);
    }

    /// A malformed announcement must not hide a valid one behind it.
    #[test]
    fn a_malformed_announcement_does_not_suppress_a_later_valid_one() {
        let channel = Uuid::from_u128(7);
        let relay_owner = "ab".repeat(32);
        let events = serde_json::json!([
            // Right author, right channel, but no `d` tag — and tags absent
            // entirely on the one before it.
            { "id": "no-tags", "kind": 30617, "pubkey": relay_owner },
            { "id": "no-d", "kind": 30617, "pubkey": relay_owner, "tags": [
                ["buzz-channel", channel.to_string()]
            ]},
            { "id": "good", "kind": 30617, "pubkey": relay_owner, "tags": [
                ["d", "chan-repo"], ["buzz-channel", channel.to_string()]
            ]},
        ]);
        assert_eq!(
            repo_binding_from_events(events.as_array().unwrap(), channel, &relay_owner),
            Some((relay_owner, "chan-repo".into()))
        );
    }

    #[test]
    fn repo_url_targets_the_forge_smart_http_path() {
        let wt = ThreadWorktrees::new(
            PathBuf::from("/tmp/x"),
            "ab".repeat(32),
            "cd".repeat(32),
            "ws://localhost:3999",
            None,
        );
        assert_eq!(
            wt.repo_url("owner123", "chan-repo"),
            "http://localhost:3999/git/owner123/chan-repo"
        );
    }
}

/// Integration test for the full git lifecycle against a local bare remote
/// (no relay). Gated like the forge S3 probes because it shells out to real
/// git and writes temp repos. Run with:
///   `BUZZ_ACP_WORKTREE_PROBE=1 cargo test -p buzz-acp --lib \
///    worktree::probe_tests -- --ignored`
#[cfg(test)]
mod probe_tests {
    use super::*;

    fn probe_enabled() -> bool {
        std::env::var("BUZZ_ACP_WORKTREE_PROBE").as_deref() == Ok("1")
    }

    async fn sh(cwd: &Path, args: &[&str]) -> String {
        git(cwd, args, &[]).await.expect("git op")
    }

    #[tokio::test]
    #[ignore = "requires git; BUZZ_ACP_WORKTREE_PROBE=1"]
    async fn provision_commit_push_roundtrip() {
        if !probe_enabled() {
            eprintln!("skipping: set BUZZ_ACP_WORKTREE_PROBE=1");
            return;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let base = tmp.path();

        // A bare "forge" repo with one commit on main stands in for the
        // relay-served channel repo.
        let bare = base.join("forge.git");
        std::fs::create_dir_all(&bare).unwrap();
        sh(&bare, &["init", "--bare", "-b", "main", "."]).await;
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        sh(&seed, &["init", "-b", "main", "."]).await;
        std::fs::write(seed.join("README.md"), "channel repo\n").unwrap();
        sh(&seed, &["add", "-A"]).await;
        git(
            &seed,
            &["commit", "-m", "seed"],
            &[
                ("GIT_AUTHOR_NAME", "s"),
                ("GIT_AUTHOR_EMAIL", "s@t"),
                ("GIT_COMMITTER_NAME", "s"),
                ("GIT_COMMITTER_EMAIL", "s@t"),
            ],
        )
        .await
        .unwrap();
        sh(&seed, &["push", &bare.to_string_lossy(), "main"]).await;

        let root = base.join("wt-root");
        std::fs::create_dir_all(&root).unwrap();
        let wt = ThreadWorktrees::new(
            root,
            "ab".repeat(32),
            "cd".repeat(32),
            "ws://localhost:3999",
            None,
        );
        let channel = Uuid::from_u128(42);
        let thread = "ee".repeat(32);
        let bare_url = bare.to_string_lossy().into_owned();

        // Provision the worktree from the local bare "forge".
        let binding = wt
            .ensure_worktree(channel, "owner", "repo", &thread, Some(&bare_url))
            .await
            .expect("provision");
        assert_eq!(binding.branch, thread_branch(&thread));
        assert!(
            binding.path.join("README.md").exists(),
            "checkout populated"
        );

        // A clean turn checkpoints nothing.
        assert_eq!(
            wt.checkpoint(&binding).await.expect("clean checkpoint"),
            CheckpointOutcome::NothingToCommit
        );

        // The agent "works": a new file → the turn commits and pushes it.
        std::fs::write(binding.path.join("out.txt"), "agent output\n").unwrap();
        let outcome = wt.checkpoint(&binding).await.expect("dirty checkpoint");
        let commit = match outcome {
            CheckpointOutcome::Committed { commit, branch } => {
                assert_eq!(branch, binding.branch);
                commit
            }
            other => panic!("expected a commit, got {other:?}"),
        };
        assert_eq!(commit.len(), 40, "sha-1 oid");

        // The bare forge now carries the thread branch at that commit.
        let remote_oid = sh(
            &bare,
            &["rev-parse", &format!("refs/heads/{}", binding.branch)],
        )
        .await;
        assert_eq!(remote_oid, commit, "push landed on the forge");

        // Re-entering the same thread reuses the worktree (idempotent).
        let again = wt
            .ensure_worktree(channel, "owner", "repo", &thread, Some(&bare_url))
            .await
            .expect("re-provision");
        assert_eq!(again.path, binding.path);

        // A deleted-out-of-band worktree dir is re-provisioned (prune
        // clears the stale registration rather than wedging the thread).
        std::fs::remove_dir_all(&binding.path).expect("rm worktree");
        let reprovisioned = wt
            .ensure_worktree(channel, "owner", "repo", &thread, Some(&bare_url))
            .await
            .expect("re-provision after external deletion");
        assert!(reprovisioned.path.join("out.txt").exists() || reprovisioned.path.exists());

        // An EMPTY channel repo (no commits) is a clean soft error, not a
        // broken worktree — the caller falls back to the channel session.
        let empty_bare = base.join("empty.git");
        std::fs::create_dir_all(&empty_bare).unwrap();
        sh(&empty_bare, &["init", "--bare", "-b", "main", "."]).await;
        let empty_url = empty_bare.to_string_lossy().into_owned();
        let err = wt
            .ensure_worktree(
                Uuid::from_u128(43),
                "owner",
                "repo",
                &"ff".repeat(32),
                Some(&empty_url),
            )
            .await
            .expect_err("empty repo must soft-fail");
        assert!(err.contains("no commits"), "clear empty-repo error: {err}");
    }
}

/// The auth path, against a live relay forge.
///
/// This is the gap the module doc names: every `probe_tests` case passes a
/// `repo_url_override`, which is exactly the branch that skips
/// `auth_cli_flags` and `apply_push_auth`, so a local bare repo proves the
/// git mechanics and nothing about credentials. The reverted wiring
/// prototype cloned unauthenticated and 401'd against every live relay, and
/// this is the shape of test that would have caught it in seconds.
///
/// Deliberately end-to-end rather than mocked. Running it by hand is what
/// found all three blockers the unit tests could never see — none of them a
/// logic error:
///
/// - git older than **2.46** never answers the credential helper, so the
///   clone falls through to `could not read Username`;
/// - a relay bound to a specific address cannot reach its own policy
///   endpoint over loopback, so the pre-receive hook rejects every push;
/// - a shadowed `wc` makes that fail-closed hook reject every push with
///   `unknown option '-l'`.
///
/// Each failure surfaces here as a plain assertion failure with the git
/// error attached, which is the point: they are otherwise invisible,
/// because worktree operations are best-effort so git can never fail an
/// agent's turn.
///
/// ```text
/// BUZZ_ACP_FORGE_PROBE=1 \
/// BUZZ_RELAY_URL=ws://<host>:<port> \
/// BUZZ_ACP_PROBE_AGENT_SK=<64-hex agent secret> \
/// BUZZ_ACP_PROBE_CHANNEL=<channel uuid with a bound repo> \
/// BUZZ_AUTH_TAG='<NIP-OA auth tag json>' \
/// PATH="<brew-git-2.46+>:<repo>/target/release:$PATH" \
///   cargo test -p buzz-acp --lib worktree::forge_probe_tests -- --ignored --nocapture
/// ```
#[cfg(test)]
mod forge_probe_tests {
    use super::*;

    /// Fixed so repeated runs reuse one branch instead of littering the
    /// channel repo with a new `sm/thread/*` ref per run.
    const PROBE_ROOT: &str = "9704be00000000000000000000000000000000000000000000000000000000fe";

    #[tokio::test]
    #[ignore = "requires a live relay + git>=2.46 + git-credential-nostr; BUZZ_ACP_FORGE_PROBE=1"]
    async fn authenticated_clone_and_push_against_the_relay_forge() {
        if std::env::var("BUZZ_ACP_FORGE_PROBE").as_deref() != Ok("1") {
            eprintln!("skipping: set BUZZ_ACP_FORGE_PROBE=1");
            return;
        }
        let relay_url = std::env::var("BUZZ_RELAY_URL").expect("BUZZ_RELAY_URL");
        let sk_hex = std::env::var("BUZZ_ACP_PROBE_AGENT_SK").expect("BUZZ_ACP_PROBE_AGENT_SK");
        let channel: Uuid = std::env::var("BUZZ_ACP_PROBE_CHANNEL")
            .expect("BUZZ_ACP_PROBE_CHANNEL")
            .parse()
            .expect("channel uuid");
        let auth_tag = std::env::var("BUZZ_AUTH_TAG").ok();

        let keys = nostr::Keys::parse(&sk_hex).expect("agent key");
        let rest = crate::relay::RestClient {
            http: reqwest::Client::new(),
            base_url: crate::relay::relay_ws_to_http(&relay_url),
            keys: keys.clone(),
            auth_tag_json: auth_tag.clone(),
        };

        // Resolve the binding the way production does, so a probe failure
        // also covers "the relay stopped advertising what we depend on".
        let relay_self = rest
            .relay_self_pubkey()
            .await
            .expect("relay must advertise NIP-11 `self`");
        let filter = nostr::Filter::new().kind(nostr::Kind::Custom(30617));
        let events: Vec<serde_json::Value> = rest
            .query(std::slice::from_ref(&filter))
            .await
            .expect("30617 query")
            .as_array()
            .cloned()
            .unwrap_or_default();
        let (repo_owner, repo_name) = repo_binding_from_events(&events, channel, &relay_self)
            .expect("channel must have a relay-signed repo binding");

        let tmp = tempfile::tempdir().expect("tmp");
        let wt = ThreadWorktrees::new(
            tmp.path().to_path_buf(),
            keys.public_key().to_hex(),
            sk_hex.clone(),
            &relay_url,
            auth_tag,
        );

        // No `repo_url_override` — this is the whole point of the probe.
        let binding = wt
            .ensure_worktree(channel, &repo_owner, &repo_name, PROBE_ROOT, None)
            .await
            .expect("authenticated clone + worktree");
        assert_eq!(binding.branch, thread_branch(PROBE_ROOT));
        assert!(binding.path.join(".git").exists(), "worktree is a checkout");

        // A clean tree must not manufacture a commit.
        assert_eq!(
            wt.checkpoint(&binding).await.expect("clean checkpoint"),
            CheckpointOutcome::NothingToCommit,
            "an unchanged worktree must not produce a checkpoint"
        );

        // Now change something and push it for real.
        let stamp = nostr::Timestamp::now().as_secs();
        std::fs::write(
            binding.path.join("FORGE-PROBE.md"),
            format!("authenticated push probe {stamp}\n"),
        )
        .expect("write probe file");

        match wt.checkpoint(&binding).await.expect("authenticated push") {
            CheckpointOutcome::Committed { commit, branch } => {
                assert_eq!(branch, thread_branch(PROBE_ROOT));
                assert!(
                    (commit.len() == 40 || commit.len() == 64)
                        && commit.chars().all(|c| c.is_ascii_hexdigit()),
                    "checkpoint must name a full oid, got {commit:?}"
                );
                eprintln!("pushed {commit} to {branch}");
            }
            CheckpointOutcome::NothingToCommit => {
                panic!("a modified worktree must produce a commit")
            }
        }

        // Re-entry must reuse the same worktree rather than re-clone.
        let again = wt
            .ensure_worktree(channel, &repo_owner, &repo_name, PROBE_ROOT, None)
            .await
            .expect("idempotent re-entry");
        assert_eq!(again.path, binding.path);
    }
}
