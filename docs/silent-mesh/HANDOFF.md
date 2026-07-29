# Silent Mesh — Continuation Handoff

Read this first when picking up the implementation in a fresh Claude Code
session. It records where the work stands, the working conventions, and the
next slices. The plan itself lives in [roadmap.md](./roadmap.md),
[architecture.md](./architecture.md), [phase1-governance.md](./phase1-governance.md),
and [phase2-work-threads.md](./phase2-work-threads.md); repo conventions in
[AGENTS.md](../../CLAUDE.md) / CONTRIBUTING.md. This file adds only what a
new session cannot learn from those.

## Where the work lives

- **Branch: `claude/fork-sync-0vdsko`** on `paulofonseca1987/silentmesh-buzz`
  (fork of `block/buzz`, in sync with upstream main at fork time). All Silent
  Mesh work to date is on this branch; continue on it (or a branch cut from
  it) — the commit history is slice-per-commit and self-describing
  (`git log --oneline` is the index).

## Shipped so far

**Phase 1 — governance (complete).** Workflow approval gate (WF-08) with
kinds 46010/46011/46012; agent permission requests (relay HTTP create/list +
hashed-token store); grant/deny via kind 46030/46031 commands (workflow and
agent domains share the path); runtime modes (full-access / auto-accept-edits
/ supervised) in buzz-acp with request parking and the kind-24200 decision
frame; `buzz approvals` CLI; supervised deploy defaults.

**Phase 2a — roles, tiers, bindings.** Workspace layer reuses
`relay_members` (owner/admin; NIP-43 9030-series manages it — no new table,
no new kind). Immutable `channels.tier` (owned|private|open; BEFORE UPDATE
trigger). `channel_repos` binding; channel creation provisions a relay-owned
repo (relay-signed kind:30617). Guest role disabled in depth.
`BUZZ_WORKSPACE_CHANNEL_GATE` (default **off**) gates non-DM channel
creation to workspace owner/admin when enabled.

**Phase 2b–2e — work threads.** Kinds 47000 (root/task), 47001 (metadata
cmd), 47002 (state cmd), 47003 (agent recommendation, inert), 47010
(checkpoint), 47011 (overdue notice, relay-only), 47012 (canonicalization
outcome, relay-only). D41 state machine enforced relay-side
(`handlers/work_thread.rs`; exhaustive matrix tests); `work_threads`
projection (migrations 0027–0029) with TOCTOU-safe transitions and
idempotent command replay. Overdue sweep (leader-only, at-most-once per
deadline, deadline-edit re-arms). Canonicalize-on-close
(`api/git/canonicalize.rs`): grafts the latest checkpoint under
`canon/<thread-short>/` on the default branch via hydrate → pre-2.0 git
plumbing → pointer-CAS publish (+ kind:30618), with claim/re-arm
bookkeeping, crash-recovery sweep, and a kind:47012 outcome notice that
never fails the close. `buzz threads` CLI family
(open/list/show/set/state/recommend/checkpoint) + buzz-sdk builders.

## Working conventions (as practiced)

- One roadmap slice at a time: implement → `cargo fmt --all` +
  `cargo clippy --workspace --all-targets` (zero warnings) + unit tests +
  PG-gated tests → `git commit -s` (DCO required) → push. Never batch
  unrelated slices into one commit.
- Fork discipline (FORK.md D35): additive modules over invasive edits;
  invasive edits marked `// silent-mesh:`; no renames; kinds registered in
  `buzz-core/src/kind.rs` first, with const-asserts; migration pin tests in
  `buzz-db/src/migration.rs` updated with every migration (count, per-DDL
  asserts, schema.sql mirror asserts).
- No `unwrap()`/`expect()` in production paths; doc comments on new public
  API; relay-only kinds go in `is_relay_only_kind`.

## Environment notes / gotchas (learned the hard way)

- PG-gated tests assume `postgres://buzz:buzz_dev@localhost:5432/buzz`
  (override `DATABASE_URL`/`BUZZ_TEST_DATABASE_URL`). The three destructive
  migration tests drop/recreate the schema — run them **serialized**:
  `cargo test -p buzz-db --lib migration -- --ignored --test-threads=1`.
- `api::mesh_demo::tests::demo_join_forwarded_arm_round_trips_echo` is
  flaky in sandboxes without UDP; it is pre-existing, not caused by this
  branch.
- The forge S3 probes (and the Phase 2e full-path canonicalize test) need
  live MinIO: `docker compose up -d minio`, then
  `BUZZ_GIT_S3_PROBE=1 cargo test -p buzz-relay --lib canonicalize::s3_probe_tests -- --ignored`.
  **This has not yet run anywhere** — the dev sandbox had no MinIO. Run it
  before trusting the merged-path behavior end-to-end.
- Don't start `redis-server` with the repo as cwd (it drops `dump.rdb` into
  the working tree).
- Production git is 2.39 (bookworm): no `merge-tree --merge-base`, which is
  why canonicalize uses read-tree/commit-tree plumbing.

## Next slices (roadmap Phase 2 remainder, then Phase 3+)

1. **2f — thread forking (D27) + sibling archiving**: command kind 47020
   (fork at head or a named checkpoint → new 47000-rooted thread with
   provenance tags: `e` parent root, `commit` fork point; conversation
   inherited by reference). Batch sibling-fork archiving into the winner's
   close flow (47002 close handler).
2. **2g — personal channels + promotion (D29) with Privacy Gate scaffold
   (D30)**: command kind 47021; member-written summary + deterministic
   secret scan; files + summary transfer, conversation does not.
3. **2h — folder/file write ACLs (D4)**: per-path rules in the pre-receive
   policy hook (`api/git/hook.rs` / `policy.rs`) + agent tool layer. Note:
   the hook currently sees ref updates, not changed paths — it needs the
   pack/paths surfaced (see mapping notes in git history of this branch).
4. **buzz-acp worktree binding**: align agent workspaces onto thread
   worktrees; emit kind:47010 checkpoints automatically at turn end
   (today checkpoints are CLI/manual).
5. Phase 2 exit-criterion dry run from buzz-cli (roadmap lines 87–100),
   then Phase 3 (model plane) per roadmap.

## Suggested kickoff prompt for a fresh session

> Read docs/silent-mesh/HANDOFF.md, then docs/silent-mesh/roadmap.md and
> phase2-work-threads.md, and skim `git log --oneline main..HEAD`. Continue
> the roadmap at the next unshipped slice, following the working
> conventions in the handoff (slice → gates → signed commit → push).
