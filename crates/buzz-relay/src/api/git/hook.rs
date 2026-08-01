//! Pre-receive hook script generation and injection.
//!
//! The hook is a shell script that:
//! 1. Reads `old_oid new_oid ref_name` lines from stdin
//! 2. For each non-create/non-delete, runs `git merge-base --is-ancestor`
//!    (inheriting quarantine env vars)
//! 3. POSTs the payload to the relay's internal policy endpoint with HMAC
//! 4. Exits non-zero on ANY non-200 response (fail-closed)
//!
//! Security invariants:
//! - Fail-closed: curl failure, timeout, non-200 → exit 1
//! - Quarantine vars inherited for ancestry checks
//! - HMAC binds callback to specific push operation

use std::path::Path;

use tokio::fs;
use tracing::{error, info};

/// The pre-receive hook script content.
///
/// Environment variables set by the relay before spawning git receive-pack:
/// - `BUZZ_HOOK_URL` — internal policy endpoint (http://127.0.0.1:{port}/internal/git/policy)
/// - `BUZZ_HOOK_UDS` — optional Unix socket to reach that endpoint over
///   instead of TCP. Set whenever the relay has one, and **required** in
///   practice when the relay binds a specific non-loopback address: it is
///   then not listening on loopback at all, so `BUZZ_HOOK_URL` is refused
///   and every push fails. A Unix socket is same-host by construction, so
///   it needs no address reasoning and satisfies the internal endpoint's
///   locality requirement directly.
/// - `BUZZ_HOOK_SECRET` — per-push HMAC secret
/// - `BUZZ_REPO_ID` — repo identifier (d-tag)
/// - `BUZZ_COMMUNITY_ID` — server-resolved community UUID for the git HTTP request
/// - `BUZZ_PUSHER_PUBKEY` — authenticated pusher's hex pubkey
///
/// Git sets automatically (quarantine):
/// - `GIT_OBJECT_DIRECTORY` — quarantine object store
/// - `GIT_ALTERNATE_OBJECT_DIRECTORIES` — includes the real object store
const PRE_RECEIVE_HOOK: &str = r#"#!/usr/bin/env bash
# Buzz pre-receive hook — FAIL-CLOSED
# ANY error, timeout, or non-200 response → reject the push.
set -eo pipefail

# Force C locale for deterministic sort order and byte-accurate string lengths.
# Rust uses byte-order comparison and byte lengths — locale-aware sort/strlen would mismatch.
export LC_ALL=C

ZERO="0000000000000000000000000000000000000000"

# Fail-closed: required env vars must be set by the relay.
: "${BUZZ_REPO_ID:?error: BUZZ_REPO_ID not set}"
: "${BUZZ_REPO_OWNER:?error: BUZZ_REPO_OWNER not set}"
: "${BUZZ_COMMUNITY_ID:?error: BUZZ_COMMUNITY_ID not set}"
: "${BUZZ_PUSHER_PUBKEY:?error: BUZZ_PUSHER_PUBKEY not set}"
: "${BUZZ_HOOK_URL:?error: BUZZ_HOOK_URL not set}"
: "${BUZZ_HOOK_SECRET:?error: BUZZ_HOOK_SECRET not set}"

WORK_DIR=$(mktemp -d) || { echo "error: cannot create temp dir" >&2; exit 1; }
REFS_FILE="$WORK_DIR/refs"
HMAC_FILE="$WORK_DIR/hmac"
RESP_FILE="$WORK_DIR/resp"
trap 'rm -rf "$WORK_DIR"' EXIT

# Phase 1: Read ref updates from stdin, classify each, build JSON + HMAC lines.
# We write two files in parallel:
#   REFS_FILE: JSON entries (unsorted, for the request body)
#   HMAC_FILE: "ref_name old_oid new_oid" lines (for sorting → HMAC input)
REFS=""
PATHS_FILE="$WORK_DIR/paths"
: > "$PATHS_FILE"
while read -r old_oid new_oid ref_name; do
    # Ancestry check for FF detection.
    # CRITICAL: GIT_OBJECT_DIRECTORY and GIT_ALTERNATE_OBJECT_DIRECTORIES are
    # inherited from our environment (git sets them for quarantine). Any git
    # subprocess we call sees the quarantined objects automatically.
    IS_ANCESTOR="false"
    if [ "$old_oid" != "$ZERO" ] && [ "$new_oid" != "$ZERO" ]; then
        # Exit 0 = is ancestor (FF), exit 1 = not ancestor (NFF),
        # exit 128 = error → treat as NFF (fail-closed).
        if git merge-base --is-ancestor "$old_oid" "$new_oid" 2>/dev/null; then
            IS_ANCESTOR="true"
        fi
    fi

    # silent-mesh (D4): collect the worktree paths this update writes, so the
    # policy endpoint can enforce folder/file write ACLs. Deletes touch no
    # paths. For updates we diff old..new; for creates we diff every commit
    # reachable from new but not from any existing ref (`--not --all`), which
    # covers a brand-new branch without walking shared history. Any git
    # failure leaves the marker file, and the endpoint fails closed when the
    # repo has ACLs.
    if [ "$new_oid" != "$ZERO" ]; then
        if [ "$old_oid" != "$ZERO" ]; then
            git diff --name-only --no-renames "$old_oid" "$new_oid" >> "$PATHS_FILE" 2>/dev/null \
                || echo "!PATHS_UNAVAILABLE" >> "$PATHS_FILE"
        else
            git rev-list "$new_oid" --not --all 2>/dev/null \
                | while read -r commit; do
                    git diff-tree -r --no-commit-id --name-only --no-renames "$commit" 2>/dev/null \
                        || echo "!PATHS_UNAVAILABLE"
                done >> "$PATHS_FILE" \
                || echo "!PATHS_UNAVAILABLE" >> "$PATHS_FILE"
        fi
    fi

    # JSON entry for request body.
    # Escape any special JSON characters in ref_name (defense against injection).
    # Git ref names can't contain most special chars, but belt-and-suspenders.
    SAFE_REF=$(printf '%s' "$ref_name" | sed 's/\\/\\\\/g; s/"/\\"/g')

    if [ -n "$REFS" ]; then
        REFS="${REFS},"
    fi
    REFS="${REFS}{\"old_oid\":\"${old_oid}\",\"new_oid\":\"${new_oid}\",\"ref_name\":\"${SAFE_REF}\",\"is_ancestor\":${IS_ANCESTOR}}"

    # HMAC line: ref_name first (for sorting), then oids + is_ancestor.
    # is_ancestor as "1" or "0" to match Rust's b"1"/b"0".
    if [ "$IS_ANCESTOR" = "true" ]; then
        echo "${ref_name} ${old_oid} ${new_oid} 1" >> "$HMAC_FILE"
    else
        echo "${ref_name} ${old_oid} ${new_oid} 0" >> "$HMAC_FILE"
    fi
done

# Phase 1b: Deduplicate + bound the changed-path list (silent-mesh D4).
# Sorted-unique keeps the HMAC deterministic; the cap must match
# MAX_REPORTED_PATHS in buzz-core::path_acl. Over the cap we send the
# truncation marker instead of a partial list — the endpoint fails closed
# when the repo has path ACLs.
MAX_PATHS=5000
PATHS_JSON=""
PATHS_HMAC="$WORK_DIR/paths.hmac"
: > "$PATHS_HMAC"
if [ -s "$PATHS_FILE" ]; then
    sort -u "$PATHS_FILE" > "$PATHS_FILE.uniq"
    PATH_COUNT=$(wc -l < "$PATHS_FILE.uniq" | tr -d ' ')
    if [ "$PATH_COUNT" -gt "$MAX_PATHS" ]; then
        printf '!PATHS_TRUNCATED\n' > "$PATHS_FILE.uniq"
    fi
    while IFS= read -r p; do
        SAFE_PATH=$(printf '%s' "$p" | sed 's/\\/\\\\/g; s/"/\\"/g')
        if [ -n "$PATHS_JSON" ]; then
            PATHS_JSON="${PATHS_JSON},"
        fi
        PATHS_JSON="${PATHS_JSON}\"${SAFE_PATH}\""
        printf '%s' "$p" >> "$PATHS_HMAC"
        printf '\n' >> "$PATHS_HMAC"
    done < "$PATHS_FILE.uniq"
fi

# Phase 2: Compute HMAC-SHA256 signature.
# Payload format MUST match relay's compute_hmac() in policy.rs:
#   repo_id | repo_owner | community_id | pusher_pubkey | (old_oid + new_oid + ref_name + is_ancestor) per ref sorted by ref_name | sorted paths | timestamp
TIMESTAMP=$(date +%s)

# Structurally unambiguous HMAC format (matches Rust's compute_hmac):
# len(repo_id):repo_id | repo_owner | pusher | (old_oid + new_oid + len(ref):ref + is_anc)* | timestamp
REPO_ID_LEN=${#BUZZ_REPO_ID}
HMAC_INPUT="${REPO_ID_LEN}:${BUZZ_REPO_ID}|${BUZZ_REPO_OWNER}|${BUZZ_COMMUNITY_ID}|${BUZZ_PUSHER_PUBKEY}|"
# Sort by ref_name (field 1) — matches Rust's sort_by(|a, b| a.ref_name.cmp(&b.ref_name))
if [ -f "$HMAC_FILE" ]; then
    sort "$HMAC_FILE" | while IFS=' ' read ref_name old_oid new_oid is_anc; do
        REF_LEN=${#ref_name}
        printf '%s%s%s:%s%s' "$old_oid" "$new_oid" "$REF_LEN" "$ref_name" "$is_anc"
    done > "$HMAC_FILE.concat"
    HMAC_INPUT="${HMAC_INPUT}$(cat "$HMAC_FILE.concat")"
    rm -f "$HMAC_FILE.concat"
fi
# silent-mesh (D4): bind the changed-path list into the signature, so a
# compromised hook cannot strip paths to dodge ACLs. Each path is
# length-prefixed, matching Rust's compute_hmac.
HMAC_INPUT="${HMAC_INPUT}|"
if [ -s "$PATHS_HMAC" ]; then
    while IFS= read -r p; do
        P_LEN=${#p}
        printf '%s:%s' "$P_LEN" "$p"
    done < "$PATHS_HMAC" > "$PATHS_HMAC.concat"
    HMAC_INPUT="${HMAC_INPUT}$(cat "$PATHS_HMAC.concat")"
    rm -f "$PATHS_HMAC.concat"
fi
HMAC_INPUT="${HMAC_INPUT}|${TIMESTAMP}"

SIGNATURE=$(printf '%s' "$HMAC_INPUT" | openssl dgst -sha256 -hmac "$BUZZ_HOOK_SECRET" -hex 2>/dev/null | sed 's/.*= //')
if [ -z "$SIGNATURE" ]; then
    echo "error: failed to compute HMAC signature" >&2
    exit 1
fi

# Phase 3: POST to policy endpoint — FAIL-CLOSED.
# repo_id is free-form (user-chosen d-tag) — must be escaped for JSON safety.
# repo_owner, community_id, and pusher_pubkey are validated fixed-shape strings — no escaping needed.
SAFE_REPO_ID=$(printf '%s' "$BUZZ_REPO_ID" | sed 's/\\/\\\\/g; s/"/\\"/g')
BODY="{\"repo_id\":\"${SAFE_REPO_ID}\",\"repo_owner\":\"${BUZZ_REPO_OWNER}\",\"community_id\":\"${BUZZ_COMMUNITY_ID}\",\"pusher_pubkey\":\"${BUZZ_PUSHER_PUBKEY}\",\"ref_updates\":[${REFS}],\"changed_paths\":[${PATHS_JSON}],\"timestamp\":${TIMESTAMP},\"signature\":\"${SIGNATURE}\"}"

# Prefer the Unix socket when the relay provides one. A relay bound to a
# specific address (Silent Mesh's VPN-bound posture) is NOT listening on
# loopback, so the TCP URL is connection-refused and every push fails; the
# socket is same-host by construction and always reachable from the hook,
# which the relay spawned on its own machine.
if [ -n "${BUZZ_HOOK_UDS:-}" ]; then
    set -- --unix-socket "$BUZZ_HOOK_UDS" "http://localhost/internal/git/policy"
else
    set -- "$BUZZ_HOOK_URL"
fi

HTTP_CODE=$(curl --silent --max-time 10 \
    -o "$RESP_FILE" \
    -w "%{http_code}" \
    -X POST \
    -H "Content-Type: application/json" \
    -d "$BODY" \
    "$@" 2>/dev/null) || {
    echo "error: push authorization failed (network error reaching policy service)" >&2
    exit 1
}

if [ "$HTTP_CODE" != "200" ]; then
    echo "error: push denied by policy (HTTP $HTTP_CODE)" >&2
    cat "$RESP_FILE" >&2 2>/dev/null
    exit 1
fi

exit 0
"#;

/// Install the pre-receive hook into a bare repository.
///
/// Creates a `hooks/` directory and writes the hook script with execute permission.
/// Called during repo creation (kind:30617 handling) and can be called to
/// retrofit existing repos.
pub async fn install_hook(repo_path: &Path) -> anyhow::Result<()> {
    let hooks_dir = repo_path.join("hooks");
    fs::create_dir_all(&hooks_dir).await.map_err(|e| {
        error!(path = %hooks_dir.display(), error = %e, "failed to create hooks dir");
        anyhow::anyhow!("failed to create hooks directory: {e}")
    })?;

    let hook_path = hooks_dir.join("pre-receive");
    fs::write(&hook_path, PRE_RECEIVE_HOOK).await.map_err(|e| {
        error!(path = %hook_path.display(), error = %e, "failed to write hook");
        anyhow::anyhow!("failed to write pre-receive hook: {e}")
    })?;

    // Make executable (Unix only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&hook_path, perms).map_err(|e| {
            error!(path = %hook_path.display(), error = %e, "failed to chmod hook");
            anyhow::anyhow!("failed to set hook permissions: {e}")
        })?;
    }

    info!(repo = %repo_path.display(), "pre-receive hook installed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::PRE_RECEIVE_HOOK;

    /// silent-mesh (D4): the hook's path-collection phase, exercised against
    /// a real repository. Runs the extracted collection logic (the same
    /// commands the hook issues) for an update, a create, and a delete, and
    /// checks the sorted-unique list the policy endpoint will receive.
    #[test]
    fn hook_collects_changed_paths_for_updates_creates_and_deletes() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let repo = dir.path();
        let sh = |script: &str| -> String {
            let out = std::process::Command::new("bash")
                .arg("-c")
                .arg(script)
                .current_dir(repo)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@test")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@test")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("HOME", repo)
                .output()
                .expect("run bash");
            assert!(
                out.status.success(),
                "script failed: {script}\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };

        sh("git init -q -b main .");
        sh(
            "mkdir -p src infra && echo a > src/a.rs && echo b > infra/deploy.yaml \
            && git add -A && git commit -qm one",
        );
        let base = sh("git rev-parse HEAD");
        sh("echo a2 > src/a.rs && echo c > docs.md && git add -A && git commit -qm two");
        let head = sh("git rev-parse HEAD");

        // The hook's exact collection commands, extracted.
        let collect = |old: &str, new: &str| -> Vec<String> {
            let zero = "0".repeat(40);
            let script = if new == zero {
                String::from("true")
            } else if old == zero {
                format!(
                    "git rev-list {new} --not --all | while read -r c; do \
                     git diff-tree -r --no-commit-id --name-only --no-renames \"$c\"; done"
                )
            } else {
                format!("git diff --name-only --no-renames {old} {new}")
            };
            let out = sh(&format!("{script} | sort -u"));
            out.lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        };

        // Update: only what changed between the two commits.
        assert_eq!(
            collect(&base, &head),
            vec!["docs.md".to_owned(), "src/a.rs".to_owned()]
        );

        // Create of a branch whose commits are already reachable from an
        // existing ref contributes nothing new (`--not --all`).
        sh("git branch feature HEAD");
        let feature = sh("git rev-parse feature");
        assert!(collect(&"0".repeat(40), &feature).is_empty());

        // Create carrying a genuinely new commit reports its paths. The
        // incoming commit must be reachable from NO existing ref — that is
        // what a real pre-receive sees (objects in quarantine, ref not yet
        // created), so build it and then drop the branch that named it.
        sh("git checkout -q -b staging && echo x > newfile.txt \
            && git add -A && git commit -qm three");
        let incoming = sh("git rev-parse HEAD");
        sh("git checkout -q main && git branch -qD staging");
        assert_eq!(
            collect(&"0".repeat(40), &incoming),
            vec!["newfile.txt".to_owned()],
            "a create must report the paths of commits not reachable from any ref"
        );

        // Delete touches no paths.
        assert!(collect(&head, &"0".repeat(40)).is_empty());
    }

    #[test]
    fn runtime_image_installs_pre_receive_hook_tools() {
        let dockerfile = include_str!("../../../../../Dockerfile");
        let runtime_stage = dockerfile
            .split("FROM debian:${DEBIAN_VERSION}-slim AS runtime")
            .nth(1)
            .expect("Dockerfile should have a runtime stage");
        let runtime_setup = runtime_stage
            .split("COPY --from=builder")
            .next()
            .expect("runtime stage should copy built artifacts after package setup");

        for tool in ["curl", "openssl"] {
            assert!(
                PRE_RECEIVE_HOOK.contains(tool),
                "test setup expected the pre-receive hook to invoke {tool}"
            );
            assert!(
                runtime_setup.contains(&format!("\n        {tool} \\")),
                "relay runtime image must install {tool}; the git pre-receive hook uses it and fails closed without it"
            );
        }
    }
}
