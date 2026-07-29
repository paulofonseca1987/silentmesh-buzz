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

**Phase 2 exit-criterion dry run — 30/30 passing** (first green run
2026-07-29 against a scratch relay on :3999).
`docs/silent-mesh/phase2-exit-dryrun.sh`
drives the whole roadmap exit criterion through `buzz-cli` against a live
relay and prints a PASS/FAIL matrix (it mints its own participants per run
via `buzz-admin generate-key`, so repeat runs never collide on
once-per-member state). Run it against a scratch relay — **not** the
production one on :3000 — e.g. `BUZZ_BIND_ADDR=127.0.0.1:3999
RELAY_URL=ws://localhost:3999 BUZZ_HEALTH_PORT=8099 BUZZ_METRICS_PORT=9199
BUZZ_WORKSPACE_CHANNEL_GATE=true RELAY_OWNER_PUBKEY=<pk>
BUZZ_RELAY_PRIVATE_KEY=<sk>` (the last one is required or `buzz-admin
add-member` refuses to publish the roster event). Budget ~10 minutes: the
overdue notice waits on the leader-elected metrics tick.

Two CLI defects it caught, both fixed: `threads show` could not display
relay-emitted notices at all (47011/47012/47013/47014 — the events that
tell an agent its deadline passed or its canonicalization failed), and the
fold's `(created_at, id)` tie-break let a client command beat a
same-second relay notice, so a thread the projection had archived kept
rendering as `ready`. Notices are now surfaced and win same-second ties.

**Restriction-gate fix (post-2f review).** Command kinds
(47001/47002/47021, DM, workflow, approval) are routed *after* the
community ban/timeout write-block in ingest, not before — a timed-out
Channel Admin can no longer close threads or archive fork families. The
pre-gate exemptions stay limited to moderation commands (9040–9044) and
relay-admin (9030–9033), the tooling used to lift restrictions.

**Phase 2h — folder/file write ACLs (D4).** `buzz-path-acl` tags on
kind:30617 (`buzz_core::path_acl`): path patterns → `write:<role>` /
`write:<64-hex pubkey>` / `readonly`, most-specific-wins with ties
unioned; opt-in per repo (no tags = upstream behavior). The pre-receive
hook now collects changed paths (diff for updates, `rev-list --not --all`
+ diff-tree for creates, none for deletes), dedupes them, and signs them
into the callback HMAC — a bash/Rust parity test pins the format. The
policy endpoint enforces after `evaluate_push` and **fails closed** when
ACLs exist but the path list is unavailable, truncated (>5000), or
missing (older hook). `readonly` is what keeps relay-owned `canon/` and
`promoted/` layers safe from hand-edits.

**Phase 2g — personal channels + promotion.** `personal_channels`
registry (migration 0031; one per member, created atomically with the
channel via a `personal` tag on kind:9007 — member-creatable even under
the workspace gate, visibility forced private and locked, membership
restricted to the member + their own bots at the `add_member` choke
point). Kind 47021 promotion command (target-channel root; summary =
content; strict tag allowlist) runs the D30 Privacy Gate scaffold
(`buzz_core::secret_scan`, deterministic prefix/shape rules over summary
+ every text blob; fail-closed on unscannable text, bounded head-sniff
for big binaries) and a cross-repo graft (`api/git/promote.rs`: local
push into a temp ref → read-tree graft under `promoted/<short>/` → CAS
publish; the graft commit parents **only the target tip** — no source
parent edge, or the personal repo's entire unscanned history would
publish) **before** the event persists — a refused promotion stores and
transfers nothing. Closed (non-archived) sources stay promotable so
crash retries converge and re-promotion is possible; archived targets
are refused. Source thread closes with a relay-only
kind:47014 notice in the source channel. `buzz channels create-personal`
+ `buzz threads promote`. Exit-criterion behavior covered by the S3-gated
`promote::s3_probe_tests` (ran green on live MinIO 2026-07-29).

**Phase 2b–2f — work threads.** Kinds 47000 (root/task), 47001 (metadata
cmd), 47002 (state cmd), 47003 (agent recommendation, inert), 47010
(checkpoint), 47011 (overdue notice, relay-only), 47012 (canonicalization
outcome, relay-only), 47013 (sibling-archive notice, relay-only), 47020
(fork root). D41 state machine enforced relay-side
(`handlers/work_thread.rs`; exhaustive matrix tests); `work_threads`
projection (migrations 0027–0030) with TOCTOU-safe transitions and
idempotent command replay. Overdue sweep (leader-only, at-most-once per
deadline, deadline-edit re-arms). Canonicalize-on-close
(`api/git/canonicalize.rs`): grafts the latest checkpoint under
`canon/<thread-short>/` on the default branch via hydrate → pre-2.0 git
plumbing → pointer-CAS publish (+ kind:30618), with claim/re-arm
bookkeeping, crash-recovery sweep, and a kind:47012 outcome notice that
never fails the close. Thread forking (2f, D27/D28): kind 47020 is a
root-like regular event (its id = the new thread id, provenance
`e`/`commit` tags — lowercase, unmarked; fork point must be a recorded
parent checkpoint, checked over the full paged history; any full member,
any parent state); closing a winner with `archive-siblings` closes and
batch-archives its fork family in one family-advisory-locked transaction
(recursive walk; snoozed included under close authority;
pending-canonicalization siblings skipped; concurrent family closes
serialize so exactly one winner survives) and emits kind:47013 notices.
`buzz threads` CLI family
(open/list/show/set/state/fork/recommend/checkpoint) + buzz-sdk builders.

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
  First ran green 2026-07-29 on the WSL2 machine (live MinIO) during the
  2f session — the merged-path behavior is now covered end-to-end.
- Don't start `redis-server` with the repo as cwd (it drops `dump.rdb` into
  the working tree).
- Production git is 2.39 (bookworm): no `merge-tree --merge-base`, which is
  why canonicalize uses read-tree/commit-tree plumbing.
- `cargo clippy/check --workspace --all-targets` needs OpenSSL headers
  (openssl-sys via buzz-relay's mesh-llm dev-deps). On a machine without
  `libssl-dev` and without sudo: `apt-get download libssl-dev`, `dpkg -x`
  it somewhere, build a prefix with `include/openssl` (merge the
  `x86_64-linux-gnu/openssl` arch headers in — `opensslconf.h` lives
  there) and `lib/libssl.so`/`libcrypto.so` symlinks to the system
  `.so.3`, then run gates with `OPENSSL_DIR=<prefix>`. Beware masked
  pipeline exit codes (`cargo … | tail` reports tail's status).

## Next slices (roadmap Phase 2 remainder, then Phase 3+)

1. **buzz-acp worktree binding**: align agent workspaces onto thread
   worktrees; emit kind:47010 checkpoints automatically at turn end
   (today checkpoints are CLI/manual).
2. Phase 3 (model plane) per roadmap — Phase 2 is complete and its
   exit criterion runs green from buzz-cli.

## Suggested kickoff prompt for a fresh session

> Read docs/silent-mesh/HANDOFF.md, then docs/silent-mesh/roadmap.md and
> phase2-work-threads.md, and skim `git log --oneline main..HEAD`. Continue
> the roadmap at the next unshipped slice, following the working
> conventions in the handoff (slice → gates → signed commit → push).
