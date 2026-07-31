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

**Phase 3 slice 7 — personal channels are the `owned` tier (2026-07-31).**
Closes the D24/D29 gap slice 6's live run surfaced: personal channels were
created at the `open` default, so an **agent turn** in a member's most
private space passed the tier gate for a vendor backend — while gate /
copilot / embedding reads of the same channel were owned-pinned to Local.
Same bytes, two egress floors. Forced at the **DB choke point** by removing
the `tier` parameter from `create_personal_channel` (the tier of a personal
channel is not a caller's choice, and a parameter invites some future call
site to declare it open again); the relay additionally refuses an explicit
weaker `tier` tag instead of silently upgrading it. **Migration 0034**
tightens existing rows — the only migration touching an immutable column,
justified in-file: the trigger guards against *weakening*, and
open/private → owned strictly reduces permitted egress. It **drops and
recreates** the trigger rather than disabling it (`ALTER TABLE channels …
DISABLE TRIGGER` is refused by the tenant-fence lint, correctly) and
restores it verbatim; verified live that a pre-fix row flips and that a
later tier change still raises. **Desktop** had to move with it: it wrote a
*bare* model id into `BUZZ_ACP_MODEL`, which fail-closes to Vendor, so
local desktop agents would have been refused in every personal channel —
new `buzz_core::model_route::qualify_model` joins launcher-known
model+provider into the `provider:model` form (never double-prefixes,
never overrides an explicit qualification, never qualifies an empty id).
The desktop call site is one line and **unverified locally** (the Tauri
crate needs WebKitGTK dev libs this box cannot install) — CI compiles it.

**Phase 3 slice 6 — Privacy Gate assist (D30), the gateway's first
consumer (2026-07-31).** New kind:47022 asks, before promoting, what a
promotion *would* expose; relay-only kind:47023 answers with deterministic
findings + advisory notes + a suggested summary. Pre-flight, not another
blocker (D30's wording is "local summary + redaction suggestions") —
kind:47021 is unchanged. Three rules make an advisory model safe here: the
**deterministic scanners stay authoritative** (the model may only ADD
findings, never clear one, so hallucination/injection cannot open the
gate); the **model's own output is re-scanned** before publication (a
suggested summary that trips the rules is dropped whole — the review can
never become a new leak path); and the request is pinned to
`InferencePurpose::Gate`, owned-pinned → Local at every tier. `assist`
status (`ok|unavailable|unusable|failed`) keeps "no findings" distinct
from "no model ran". Off by default (`BUZZ_GATE_ASSIST_MODEL`). Review
caught two: side effects are awaited **inline** before the ingest ack, so
the model call stalled the member's ack for a full inference (29 s → 41 ms
once detached, spawned inside the fresh-insert branch so replay dedup
survives); and attribution hardcoded tier Owned instead of the channel's
declared tier. Live on qwen3:14b: scanners caught the planted AWS key,
the model added four notes regexes cannot see (customer name, a person,
an internal host, the key needing rotation) and drafted a clean summary
omitting all four — metered `purpose=gate backend=local`, 453/664 tokens.
See phase3-gate-assist.md.

**Phase 3 slice 5 — the gateway's real `local` backend (2026-07-31).**
`sm-gateway` stops being a skeleton: `ollama::OllamaBackend` is the first
non-stub `ModelBackend`. Chosen first because the policy leaves no choice —
`Copilot`/`Gate`/`Embedding` are owned-pinned to `Backend::Local`, so D25 /
D30 / D37 all block on this one impl. **Locality is proven, not asserted**:
`check_local_base_url` (pure) admits only loopback and the tailnet
(`100.64.0.0/10`, `fd7a:115c:a1e0::/48`, `*.ts.net`; RFC1918 deliberately
excluded), applied at construction so a misconfiguration fails at startup,
not at request time. Review's finding, and the load-bearing half: the URL
check alone is insufficient — reqwest **follows redirects** (a loopback
`302 → api.openai.com` egresses the prompt from an endpoint that passed the
check) and **honors proxy env vars** with no loopback bypass; both pinned
off, with a raw one-shot-server test proving the 302 is surfaced rather than
followed. Real `usage` token counts replace the stub's word estimate, and an
unmeterable response is an error rather than a fabricated number. Validated
live on the 2× RTX 4060s: gateway-mediated owned-tier inference wrote a
`model_usage` row (36/3 tokens, owned/local). See phase3-gateway-local.md.

**Phase 3 slice 1 — tier-aware router + metering.** `buzz_core::model_route`
(pure policy: Backend {local,tee,vendor} × ChannelTier {owned,private,open}
× InferencePurpose, owned-pinned copilot/gate/embedding; exhaustive matrix
test). `buzz_db::model_usage` + migration 0032 — per-request attribution
table (community-scoped, channel_tier enum reused, backend/purpose CHECKed)
with `user_usage_totals` (per-user by tier+backend) and `user_token_spend`.
New `sm-gateway` crate: `ModelBackend` trait + stub local/tee/vendor impls,
`Gateway::route_and_record` (policy gate → budget → dispatch → meter; a
refused request records nothing). Backends are stubs; the harness/relay
wiring that calls the gateway is a later slice. See phase3-model-plane.md.

**Phase 3 slice 2 — buzz-acp pre-turn tier enforcement.** The routing policy
made load-bearing at the turn boundary: a pre-turn gate in `run_prompt_task`
(before session creation / before any content reaches the agent subprocess)
refuses a turn whose model backend would egress below the channel's tier, and
posts a kind:9 notice. New pure `buzz_core::model_route::provider_to_backend`
(`provider:model-id` → Backend; unknown/absent → Vendor, fail-closed);
`buzz_acp` reads the `tier` tag the relay already stamps on kind:39000
(no relay change) and threads it ChannelInfo → PromptChannelInfo → resolver.
Fail-closed both ways (unresolvable tier → Owned; unclassifiable provider →
Vendor). Strict rollout — since all providers today are Vendor, owned/private
channels refuse all agent turns until a Local/TEE backend ships (the tier
guarantee, by design). Attribution-from-live-turns is blocked by the 44200
owner-encryption and deferred to the real-backend slice. See
phase3-tier-enforcement.md.

**Phase 3 slice 4 — live-turn attribution + owner usage read (2026-07-30).**
**The Phase 3 exit criterion is closed with live data.** New cleartext kind
44201 (attribution sibling of the encrypted 44200): harness publishes per
completed turn with reliable token counts; the relay is the classification
authority (tier from its own channels table, backend via the shared
`classify_model`, ownership via `users.agent_owner_pubkey`, dedup by event
id) and records into `model_usage`; owner-only read gating on every surface
incl. FTS exclusion (migration 0033, expression-wrapping pattern).
`UsageTracker::seed_fresh_session` makes first-turn deltas reliable for
harness-created sessions (without it, max-turns-per-session=1 never
attributed). `buzz-admin usage` prints per-user totals by tier/backend.
Validated live: Mac vault message → qwen3:14b on the 4060s → 44201 →
model_usage row (owned/local, 4100/1054) → usage table. See
phase3-attribution.md. Testbed gotcha: the users-table agent-owner
registration (NIP-OA BUZZ_AUTH_TAG in production) must exist or the relay
403s the 44200/44201 — correctly.

**Phase 3 slice 3 — local Ollama backend (2026-07-30).** Owned channels now
*serve* turns, not just refuse them: `buzz-agent` `Provider::Ollama` (OpenAI
Chat dialect at loopback, no key), self-declared prefixed catalog identity
(`ollama:<model>`) so the switch confirm verifies provider+model as one unit,
vendor-only persona-prefix matcher fallback (Local/TEE prefixes must be
self-declared — review-found regression of the slice-2 guarantee, closed),
and execution-explicit reply instructions. Validated live: the owned vault
answered a Mac member's message with qwen3:14b on the 2×4060s via
localhost-only Ollama — gate allowed, switch confirmed, model executed
`buzz messages send` through buzz-dev-mcp, threaded reply landed, zero
egress. Model notes + deferred desktop wiring in phase3-local-backend.md.

**Tier surface + live E2E (2026-07-30).** `buzz channels create --tier
<owned|private|open>` (SDK `build_create_channel` tier param) closed the gap
where no client could declare the D24 tier the relay already parsed. Proven
on a two-machine testbed: relay on the WSL host bound to its Tailscale IP
(`ws://100.72.140.59:3999`, dev docker infra), `buzz` CLI on a MacBook over
the tailnet. Verified live: 30/30 Phase 2 exit dry-run over the Tailscale
address; cross-machine messaging/threads; and the slice-2 tier gate —
a vendor-model agent turn in an `--tier owned` channel refused pre-turn with
the ⚠️ notice visible from the Mac (zero egress), while the same agent's
open-channel turn reached the provider (auth error on a canned key = egress
attempted where permitted). Testbed env: scratchpad `relay-e2e.env` +
`ids.env` (E2E identities, channels, Mac client key).

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

**Phase 2 (final) — buzz-acp worktree engine.** Shipped the tested git
engine (`crates/buzz-acp/src/worktree.rs`, `pub mod`): `ThreadWorktrees`
(authenticated clone of the channel repo + per-thread `sm/thread/<short>`
worktree, idempotent with prune/empty-repo soft-fail), `checkpoint`
(add/commit/skip-if-clean/push → oid), `push_auth_config` (NIP-98
`git-credential-nostr` scheme, 0600-on-create keyfile), plus pure helpers.
Every git op has a 120 s timeout and runs with system+user gitconfig
neutralized; all best-effort. Covered by the gated
`worktree::probe_tests` against a local bare remote (authenticated
provision → worktree → clean-skip → commit+push → idempotent re-entry →
re-provision-after-deletion → empty-repo soft-fail) + unit tests on the
helpers. **The harness wiring is NOT shipped** — see next-slices #1.

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
- **Ollama** is a sudo-less user install at `~/ollama/bin/ollama` (not on
  `PATH`, nothing in `/usr/local`), run as the **systemd user service
  `ollama.service`** (`~/.config/systemd/user/ollama.service`, lingering
  enabled, `Restart=always`) — so it comes back on boot.
  `systemctl --user status|restart ollama.service`; verify with
  `curl -s localhost:11434/api/tags`. Bound **loopback-only**
  (`OLLAMA_HOST=127.0.0.1:11434`) to match the gateway's strictest local
  guard, with `OLLAMA_KEEP_ALIVE=30m` so a 14B stays resident between
  turns. Models present: `qwen3:14b` (the harness workhorse),
  `qwen2.5:14b`, `qwen2.5:7b`, `llama3.2:3b` (fast enough for probes,
  ~2 s). No embedding model is pulled yet — D37 will need one.
  Gotcha: don't stop it with `pkill -f "ollama/bin/ollama serve"` — the
  pattern matches your own shell and kills it; use
  `systemctl --user stop ollama.service`.
- `git_sign_nostr::tests::test_parse_envelope_rejects_invalid_oa_pubkey`
  fails on this branch **and on a stashed clean tree** — an upstream test
  expecting an all-zero pubkey to be refused as an invalid BIP-340 key,
  which the current secp/nostr version accepts. Pre-existing, not caused by
  Silent Mesh work; don't chase it when running the workspace unit gate.
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

## Next slices (Phase 3+)

Phase 2's scope items (2a–2h) all shipped and the exit criterion runs
green from buzz-cli. The buzz-acp **worktree engine** landed as the final
slice; its **harness wiring** is the one remaining Phase-2 follow-up.

1. **buzz-acp worktree wiring** (engine is done — `worktree.rs`, tested;
   see phase2-work-threads.md §8). Bind a work-thread turn's ACP session
   cwd to `ThreadWorktrees::ensure_worktree(...)` and call `checkpoint` +
   emit 47010 at end of turn. Two things the adversarial review proved
   this needs and that the deferred prototype got wrong:
   - **A session-model change, not a second map.** Work-thread turns need
     their own session (own cwd), but the pool's `invalidate`/rotation/
     `try_claim` affinity/channel-removal GC are all channel-keyed; a
     bolt-on `thread_sessions` map desynced from all of them (retained a
     broken session while dropping the healthy channel one; never reset the
     shared turn counter → every-turn rotation; leaked entries on channel
     removal). Do it as a proper effective-session-key refactor.
   - **Verify the 30617 author.** The repo binding must be resolved from
     the announcement signed by the *known relay owner*, not any author
     with a matching `d` tag (redirection hole). Resolve the relay pubkey
     first (NIP-11 or a startup fetch).
   Must be validated against a **live** claude-code turn in a 47000-rooted
   thread pushing to a running relay's forge — the whole value is that
   round trip, unexercisable in the dev sandbox.
2. **Phase 3 continues** — slices 1–5 shipped (tier-aware router +
   attribution store; pre-turn tier enforcement; the local Ollama agent
   backend; live-turn attribution 44201 + owner usage read; the gateway's
   real `local` backend). The Phase 3 exit criterion is closed. What is
   left, roughly in dependency order:
   - **More gateway consumers.** Slice 6 wired the first (the gate
     assist). Copilot (D25) and embeddings (D37) remain; embeddings
     additionally need a trait seam — `RawInference { text, tokens }`
     cannot express a vector.
   - **Owner budgets.** `Gateway::with_budget` exists and is tested, but
     nothing sets a budget or enforces one on the live (harness) path. The
     hard part is placement, not policy: the relay holds the spend data,
     the harness is the only component that can *prevent* a turn.
   - **Member-facing usage reads** (today `buzz-admin usage` is
     operator-only), the **desktop bare-model-id gap** (desktop writes bare
     ids into `BUZZ_ACP_MODEL`, so desktop-launched local agents classify
     as vendor and are refused in owned/private), then **TEE**
     (attestation-then-send) and **per-user vendor CLIs** (D23).

## Suggested kickoff prompt for a fresh session

> Read docs/silent-mesh/HANDOFF.md, then docs/silent-mesh/roadmap.md and
> phase2-work-threads.md, and skim `git log --oneline main..HEAD`. Continue
> the roadmap at the next unshipped slice, following the working
> conventions in the handoff (slice → gates → signed commit → push).
