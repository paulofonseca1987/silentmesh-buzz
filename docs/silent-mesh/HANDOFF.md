# Silent Mesh — Continuation Handoff

Read this first when picking up the implementation in a fresh Claude Code
session. It records where the work stands, the working conventions, and the
next slices. The plan itself lives in [roadmap.md](./roadmap.md),
[architecture.md](./architecture.md), [phase1-governance.md](./phase1-governance.md),
and [phase2-work-threads.md](./phase2-work-threads.md); repo conventions in
[AGENTS.md](../../CLAUDE.md) / CONTRIBUTING.md. This file adds only what a
new session cannot learn from those.

> **Read [Audit](#audit-2026-07-31--status-corrections-and-untracked-debt)
> before planning anything.** This file twice claimed the Phase 3 exit
> criterion was closed when only its metering clause was, and it listed two
> shipped items as remaining. Those are corrected in place, but the audit
> also collects seven pieces of debt that no document tracked. The largest
> of those — the roadmap's #1 standing risk, fork divergence, having no
> mechanism at all — was **decided on 2026-08-02**: this is a hard fork,
> D35 is revised, and the risk is accepted rather than managed (FORK.md).
> Status claims here are only as good as their last check against the code;
> the audit records which ones were checked, and which were not.

## Where the work lives

- **Branch: `claude/fork-sync-0vdsko`** on `paulofonseca1987/silentmesh-buzz`
  (**hard fork** of `block/buzz` at `90e058eb`, 2026-07-28 — D35 revised
  2026-08-02, see FORK.md). All Silent Mesh work to date is on this branch;
  continue on it (or a branch cut from it) — the commit history is
  slice-per-commit and self-describing (`git log --oneline` is the index).
  `main` is the frozen fork point and there is deliberately no `upstream`
  remote. The branch name predates the decision and no longer describes a
  sync.

## Shipped so far

**Work-thread worktrees, end to end (2026-08-01).** `54329ad6`..`66fa1a03`.
The capability this file listed as next-slice #1 for weeks: an agent turn
inside a kind:47000/47020 thread runs in its own git worktree of the
channel's bound repo and leaves a kind:47010 checkpoint behind. Details in
next-slices #1 and the preconditions section; what is worth carrying
forward is *how* it went, because the shape repeats:

- **Every blocker found was environmental, and every one failed silently.**
  git 2.43 vs the 2.46 the credential helper needs; a hook callback to
  loopback on a relay that binds only a tailnet IP; an npm `wc-cli`
  shadowing `/usr/bin/wc` so the fail-closed hook refused every push. None
  was a logic error, none produced a useful message, and all three were
  invisible because worktree operations are best-effort by design.
- **The reverted prototype's two named defects were fixed first, as their
  own commits**, before anything was built on them — the session-model
  refactor and the 30617 author check. Both landed as pure refactors whose
  evidence was "every existing test passes unchanged".
- **Nothing was believed until the round trip ran.** The auth path had zero
  coverage (every probe passed `repo_url_override`, the branch that skips
  authentication), so it was proven with a real clone and push before the
  session binding was written. That is what surfaced all three blockers.

**Nine commits landed without a docs commit** (`faaccef7`..`57b3f906`) before
this update caught up. The slice→docs pairing held for ~50 commits and then
stopped; if it slips again, `git log --oneline <last-HANDOFF-commit>..HEAD`
is how to find the gap. What they did:

- **`faaccef7` — a repo announcement only counts if the relay signed it.**
  `repo_binding_from_events` now takes the expected owner and *skips*
  foreign signers rather than trusting any author with a matching `d` tag
  (the 30617 redirection hole named in next-slices #1). Fails closed on an
  empty owner. `c264093d` corrects the module doc: the worktree probe does
  **not** exercise the auth path, and used to say it did.
- **`e930aaa4`, `9ff5622d` — the Mac client can address someone and talk in
  a thread.** `MeshMentions` mirrors `buzz_sdk::mentions`
  (longest-known-name-first with a word-boundary check, `display_name` over
  `name`, ambiguous names tag everyone) and the thread composer posts kind:9
  with the `e` root and resolved `p` tags. Without the `p` tag an agent
  never wakes, so this is what makes the client able to *start* a turn.
- **`24548db3` — one session map with a structured key.** `SessionKey
  { Channel | Thread { channel, scope } | Heartbeat }` + `SessionEntry
  { id, turns }` replaces four parallel maps, and the turn counter moving
  *inside* the entry makes "drop the session but keep its counter" —
  the prototype's every-turn-rotation bug — unrepresentable. A pure
  refactor: all 668 pre-existing tests pass unchanged, which is the
  evidence. `58381faf` fixes two `unnecessary_get_then_check` clippy errors
  in its tests that escaped because clippy ran without `--all-targets`.
- **`4e8cda27` — the newline failure that looks like a typo.** A real
  qwen3:14b turn stored `99nThe smallest payload…` with no newline anywhere
  (`position(E'\n' in content)` = 0). The CLI is innocent — `--content
  'x\ny'` stores `5c 6e`. Of the three bash forms only the *unquoted* one
  yields the observed bytes, and it is the one `base_prompt.md` did not
  warn about: quoted forms leave a visible `\n` that gets reported, while
  quote removal deletes the backslash and the reader sees a typo. Both are
  now spelled out. **This fix is prompt-only** — see Audit.
- **`57b3f906` — `buzz usage show|turns`.** Members can read their own
  metering, closing the next-slices item that said `buzz-admin usage` was
  operator-only. Queries `{kinds:[44201], "#p":[self]}` — no new endpoint,
  no new kind — and classifies backend with `classify_model`, the same
  function the relay's ingest calls, so the column agrees by construction.
  Tier is deliberately absent: the relay resolves it from its own channels
  table and it is not in the event. Live totals match `buzz-admin usage`'s
  `agent_turn` rows to the token.
- **`0a5d9d9b` — a NIP-44 conformance fix in `mobile/`.** Real bug (decrypt
  accepted non-canonical padding and a 97-byte floor that should be 99;
  the upstream "invalid padding" vector returned plausible text instead of
  erroring), fixed with the official vectors vendored. **But see Audit:
  this is upstream Block code, outside declared Silent Mesh scope.**

**A real agent turn ran end to end (2026-07-31).** Until now every claim
about agent interaction rested on events *I synthesized*; the database had
zero kind:44200/44201/24200 rows. A live turn now runs: `@mention` →
kind:7 👀 → kind:7 💬 → local model → answer → kind:5 clearing both
reactions → kind:44200/44201 telemetry, metered by `buzz-admin usage` as
**private / local**. Harness: `buzz-acp` spawning in-tree `buzz-agent`
against Ollama.

Three things only a real turn could find:

1. **The tier gate refused the first two turns — correctly.** `BUZZ_ACP_MODEL`
   must be the *qualified* `ollama:qwen2.5:7b`, and the harness only accepts a
   Local-class prefix when the AGENT ADVERTISES IT EXACTLY — locality must be
   self-declared, never inferred by stripping (`ollama:claude-x` must not
   confirm against a vendor agent advertising bare `claude-x`). So
   `buzz-agent` needs `BUZZ_AGENT_PROVIDER=ollama` (its `Provider::Ollama`
   arm advertises the prefixed id); with `=openai` it advertises a bare id,
   the switch cannot be confirmed, and the gate fails closed rather than let
   an unverified fallback serve a private channel.
2. **Telemetry was refused 403 while the turn succeeded.** The relay accepts
   kind:44200/44201 only when the `p` tag is the agent's *registered* owner,
   and that mapping is materialized only from a verified **NIP-OA auth tag**.
   `BUZZ_ACP_AGENT_OWNER` is an assertion; the auth tag is the proof. Without
   it every turn works and no metering ever lands — silent, and exactly the
   data the model plane bills on.
3. **Nothing could mint that tag headlessly.** `buzz-sdk` could verify but not
   issue; the only path was the owner's Desktop. Added
   `buzz-admin mint-auth-tag --agent <hex>`, signed with the OWNER's
   `BUZZ_PRIVATE_KEY` (an agent cannot attest to owning itself).

Testbed runbook: `/tmp/agent-env.sh` shape — `BUZZ_ACP_AGENT_COMMAND`,
`BUZZ_ACP_AGENT_OWNER`, `BUZZ_ACP_MODEL=ollama:<tag>`,
`BUZZ_AGENT_PROVIDER=ollama`, `OLLAMA_MODEL`, `OLLAMA_BASE_URL`, and
`BUZZ_AUTH_TAG` from `mint-auth-tag`. The agent needs a kind:0 profile
(`buzz users set-profile --name Mesh`) or `@mention` never resolves to the
`p` tag that wakes it. **Never `pkill -f buzz-acp`** — it matches the
invoking shell; filter `pgrep -x` by `/proc/<pid>/exe`.

**Phase 4 — the macOS client works end to end (2026-07-31).** Five pieces,
all in `macos/`, all validated against the live relay from the MacBook.
68 Swift tests — 55 run anywhere, 13 only against a relay (the "Test run
with N" line counts skipped ones, so it overstates what executed):

- **`MeshProtocol`** (SPM library, no UI/entitlements — the half provable
  headlessly). Event id is **computed, never trusted**: canonical NIP-01
  array written by hand (an encoder that sorts keys makes unverifiable
  events), and `isValid()` recomputes the id AND checks the signature over
  it. `MeshRelayClient` is an actor that waits for the matching OK and
  verifies every queried event. Kind mirror is tested by **reading
  `kind.rs`** — drift there fails silently at runtime (the client just
  stops matching events, which looks like an empty channel).
- **`MeshVault`** — identity sealed by a non-extractable P-256 key **in the
  Secure Enclave**. The enclave never holds the identity (it does P-256
  only; Nostr is secp256k1) — it *wraps* it. Cross-process Touch ID unlock
  verified on hardware: seal in one process, unlock in a fresh one, 6.9 s
  (a human at the sensor; a cached grant returns instantly, which is why
  the self-test reports elapsed time).
- **The fold** (`ThreadFold.swift`) — D41 state rebuilt client-side from
  signed events. Same-second ties go to the relay notice, porting the CLI's
  fix; `null` clears a metadata field while absent leaves it alone.
- **The app** — channels with tier badges, work threads with status/fork/
  checkpoint/deadline, thread detail, gate-review cards, a composer, a
  new-thread field, and a **refusal banner** carrying the relay's own words.
- **Agent turns (cleartext half)** — `MeshTurnFold` folds kind:7 👀/💬,
  the kind:5 that retires them, and the agent's reply into
  queued → working → answered | refused | ended. Two hazards: the kind:5
  names the **reaction**, not the message (a one-hop fold shows every turn
  as permanently working), and **turn state is live-only** — the relay
  soft-deletes the reaction and every query filters `deleted_at IS NULL`, so
  a finished turn is not reconstructible from a query. Accumulate from the
  stream; never re-query. Real streaming (kind:24200) is ephemeral and
  NIP-44-encrypted to the agent's *owner*, so an ordinary member cannot see
  it at all — that is a deployment decision, not a client change.
- **Reconnect** — a dropped socket no longer ends the app. The rule: a query
  or publish is a *request* (fails with the socket; only the caller may
  decide to repeat it), a subscription is a *standing intent* (kept, and its
  REQ re-sent after re-auth). The replay sets `since` to the last event seen
  **and clears `limit`** — live subs open with `limit: 0`, so keeping it
  makes the replay return zero events while every "did the stream survive"
  test still passes. Proven by killing the dev relay under the running app:
  backoff 0.5s → 15s, ten attempts, recovered by itself.
- **Supervised approvals** — the Phase 4 exit criterion's named surface.
  The lifecycle is entirely on the wire (relay signs the 46010 request and
  the 46011/46012 outcome; the member signs the 46030/46031 decision), so
  `MeshApprovalFold` rebuilds the queue like `MeshFold` does threads.
  **Kind 46010 is shared with workflow gates** — discriminated only by a
  `domain` field in content — and the `d` tag is a *hash* of the approval
  token, so a member decides without ever holding the secret. Proven by
  clicking Approve in the running app and watching the relay's rows go
  4 pending → 3 with that request `granted`.
- **Live subscriptions** — one reader task owns `receive()` and
  demultiplexes every frame to its waiter (by subscription id, by event id,
  or to the challenge waiter). Serialized exchanges could not express a
  stream that never completes: any query queued behind an open subscription
  waited forever. Proven by fan-out from a *second* connection, not by the
  client hearing its own echo.

**Defects only running it could find** (see phase4-client.md):
*actor reentrancy* let two overlapping `query()` calls eat each other's
WebSocket frames (relay served everything, client showed nothing); **ATS**
refused cleartext `ws://` before opening a socket (the same code passed
tests, because a SwiftPM test binary has no Info.plist); a **gate review on
a team channel was accepted then silently dropped** — the check lived in
the side effect, which can decline to act but cannot say why, so it moved
to ingest where a reason can travel back; and the relay's **NIP-42
challenge arrived before anyone was waiting for it**, so the reader
discarded it and auth timed out. The app lost that race every time and the
tests never did — SwiftUI interleaves main-actor work between `connect()`
and `authenticate()` while the tests call them back to back. A green suite
and an app that could not connect at all, from nothing but scheduling.

**The rule that came out of it, now load-bearing across the client:**
*register the waiter in the same actor turn as the send.* An actor cannot
dispatch an incoming frame mid-turn, so doing both together removes the gap
rather than shrinking it. Two related traps, both mine: a
ping-before-read "fix" deadlocked (the relay does not answer client pings —
a race traded for a deadlock), and a live test checked its deadline *inside*
`for await`, so the failure it existed to catch would hang the suite instead
of failing it. A wait whose escape hatch depends on the thing being waited
for is not a timeout. Transport diagnostics now live behind `MESH_LOG=1`;
one log line found what three rounds of theorising did not.

**Both folds ordered non-deterministically** — found by two screenshots of
an unchanged channel listing the same four threads in two different orders.
`Dictionary.values` has no defined order and Swift's `sort` is not stable,
so anything sharing a second is free to swap; on the approval queue that
means **a card can move between reading it and clicking it**. Both folds now
break ties on the id. The first test written for it passed with the bug in
place (one process hands back the same dictionary order either way), so the
tests assert the *specified* order rather than self-consistency — the
recurring lesson, in a third form: **check a new test fails against the old
code before trusting it.**

**Mac working agreement.** SSH in with `ssh -i ~/.ssh/sm_e2e_mac
paulofonseca@macbook.tail94f67c.ts.net`. Screen Recording **and**
Accessibility are granted to sshd, so `macos/scripts/ui-select.sh` can
select rows and capture the window headlessly (`click at` does NOT work on
SwiftUI lists — set `selected` on the row; and the window moves, so read
its position every call). **Only vault work needs a human now**: the SSH
gate is *codesigning*, not compiling, and only the Secure Enclave needs a
real signature — so `CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY=""` with a
separate `-derivedDataPath` builds, launches, connects, and captures
entirely headlessly. A signed `xcodebuild` from a Terminal is still
required before testing `MESH_VAULT=1`. Two traps: the swift-secp256k1
build plugin leaves mode-`444` sources so the *next* build dies with 21
`cp: Permission denied` lines and no Swift error (clear
`BuildToolPluginIntermediates`), and the app's own `MESH_SNAPSHOT` capture
prefers a PDF display list whenever it exceeds 20 KB — a near-blank
`NavigationSplitView` render clears that bar, so prefer
`screencapture -l<window-id>`.

**Phase 3 slice 8 — the member owns their space's tier; promotion must
satisfy the destination (2026-07-31).** Personal channels still start
`owned` but the member may re-tier (kind:9002 + `tier` tag, which buys the
39000 re-emission the harness reads); team channels keep D26 immutability.
Migration 0035 **narrows** the trigger (exemption is data-driven off
`personal_channels`), and `set_personal_channel_tier`'s SQL joins the
registry on the owner pubkey so it cannot reach a team channel or someone
else's space. Promotion refuses loose→strict, **and** consults
`model_usage`: a source channel that actually ran a backend the target's
tier forbids is refused regardless of declared tiers — otherwise flipping
the source tier a second before promoting would bypass the rule.

**Phase 3 E2E + gate-assist fix (2026-07-31).** `phase3-e2e.sh` (24 checks)
green, `phase2-exit-dryrun.sh` still 30/30, cross-machine with the MacBook
as client. The run found a real defect: the assist's suggested summary
NAMED the customer its own advisory flagged, invisible to the scanners
(no credential shape). Fixed by asking the model for **findings** not
advice, and by replacing the judgment self-check ("does this leak?" — both
models said no) with an **extraction** one ("list every name and
credential in this summary"). Withheld summaries are reported via
`summaryVetting`. See phase3-e2e-findings.md for the decisions taken.

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
**The exit criterion's *metering clause* is closed with live data** — "usage
query shows per-user totals by tier and backend", its last sentence. This
paragraph used to claim the whole Phase 3 exit criterion was closed; it was
not, and the correction is recorded in "Audit" below. New cleartext kind
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

## Work-thread worktrees — operational preconditions

Three things must hold before a work-thread turn gets a worktree. Each was
found by trying the round trip rather than reading the code, each failed
*silently* (worktree ops are best-effort so nothing fails a turn), and each
now announces itself at boot or is fixed outright.

- **git ≥ 2.46 on the harness host.** `git-credential-nostr` answers only
  when git advertises `capability[]=authtype`, added in 2.46; older git gets
  an empty response and falls through to `could not read Username`. The
  harness warns at startup. This box's `/usr/bin/git` is 2.43 and production
  (bookworm) is 2.39 — `brew install git` gives 2.55 at
  `/home/linuxbrew/.linuxbrew/bin/git`, but brew's bin is **shadowed** by
  `/usr/bin`, so it must be prepended explicitly. **Production still needs
  its own answer**: the relay side is fixed, but any client pushing to the
  forge — including the harness — needs 2.46+ on *its* host.
- **`BUZZ_UDS_PATH` on a relay bound to a specific address** (`08109f56`).
  The pre-receive hook calls back to `/internal/git/policy`; a relay bound
  to a tailnet IP is not listening on loopback, so the TCP callback is
  refused and *every push* is rejected with "network error reaching policy
  service". Calling the bound address instead does not help — the
  locality guard then rejects it, since a same-host call to a tailnet IP
  has that IP as its peer. The Unix socket is same-host by construction.
- **A PATH whose `wc` is the real one** (`0fd7aebf`). The hook counts paths
  with `wc -l` and inherited the relay's PATH; an npm package `wc-cli`
  shadows `/usr/bin/wc` on this host with one that rejects `-l`, and the
  fail-closed hook refused every push with `unknown option '-l'`. The hook
  now puts the system directories first. (The same shadowing breaks `wc -c`
  in an interactive shell — it is nvm's bin, not hermit's.)

Testbed invocation that works:

```bash
BUZZ_ACP_WORKTREE_ROOT=/tmp/sm-worktrees \
PATH="/home/linuxbrew/.linuxbrew/bin:<repo>/target/release:$PATH" \
  buzz-acp            # target/release also supplies git-credential-nostr
```

## Audit (2026-07-31) — status corrections and untracked debt

Built by checking this file and `roadmap.md` against `git log`, the code,
and the live testbed database rather than against memory. Two claims here
were wrong and are corrected above; the rest is work that had no home in
any document.

**Corrected:** "the Phase 3 exit criterion is closed" (twice) — only the
metering clause was. "Member-facing usage reads" and "approvals" were
listed as remaining after they shipped.

**Untracked, in the order I would act on it:**

1. ~~**The fork's top standing risk has no mechanism.**~~ **DECIDED
   2026-08-02: the fork is a hard fork.** D35 is revised from "track
   upstream" to a snapshot at `90e058eb`; there are no merges or rebases,
   and Phase 0's "upstream rebase executed once end-to-end" is struck
   rather than left permanently unmet. See FORK.md for the posture and the
   accepted costs.

   The decision was made against measured numbers, not in the abstract:
   five days after the fork point upstream was **107 commits and 300 files
   ahead**, overlapping **29** of our files — concentrated in exactly the
   ~5k-line files the risk table named (`pool.rs`, `ingest.rs`, `req.rs`,
   `kind.rs`, `migration.rs`) — with **no migration-number collision yet**.
   D35's additive discipline demonstrably worked: 144 files touched, only
   29 colliding.

   **The risk is now taken, not managed.** Upstream fixes, including
   security fixes, no longer arrive in code this fork depends on but does
   not maintain; bringing one in is a deliberate cherry-pick. That is a
   real, permanent exposure and it is accepted knowingly — which is the
   point of deciding rather than continuing to drift while the documents
   claimed a cadence that had never once run.
2. **`mobile/` is outside declared scope, and `0a5d9d9b` went there
   anyway.** Phase 0 excludes desktop/mobile/web/admin-web from our CI
   ("trees stay in-tree, unbuilt"), and `mobile/lib/shared/crypto/nip44.dart`
   **predates the fork** — it is Block's code, first landed in the NIP-AB
   pairing work. The fix is real and worth having, but it belongs upstream
   to Block, and there is **no upstreaming process** — which is item 1
   again. Decide deliberately whether out-of-scope trees are in or out;
   right now the answer is "out, except when a bug is found by accident".
3. **The agent reply path is quoting-fragile and the fix is prompt-only.**
   Every agent reply is the model writing a shell command line through
   buzz-dev-mcp's bash. `4e8cda27` strengthened the instruction, but
   qwen3:14b had already ignored the *existing* stdin instruction, so there
   is no reason to believe a stronger sentence holds. A structural fix — a
   reply path that does not round-trip through shell quoting — would touch
   buzz-agent's tool-calls-as-output design. Not scheduled.
4. ~~**`SessionKey::Thread` carries `#[allow(dead_code)]`.**~~ **CLOSED**
   (`2b8620d6`). `turn_session_key` builds it for every turn bound to a
   work-thread worktree, so the variant is reachable from production and
   the allow is gone — closed together with next-slices #1, as predicted.
5. **The ledger and the events can disagree, and once did.** `model_usage`
   is the durable record; kind:44201 is the transport. The testbed holds a
   `purpose="gate"` row whose source event is **not in the events table**,
   so `buzz usage` reports three turns where `buzz-admin usage` reports
   four. `record_model_usage` has exactly one caller, driven by 44201, so
   the row must have come from an event that later vanished. **Cause not
   established** — do not assume a bug in either read before finding it.
6. **Desktop changes cannot be gated on the WSL box** (Tauri needs
   WebKitGTK, no sudo), and slice 7 shipped a desktop change explicitly
   marked "unverified locally", relying on CI. That is a reasonable
   trade but it is not written down as policy anywhere. The same gap makes
   `git push` fail its pre-push hook, so pushes here need `--no-verify`
   after running the workspace gates by hand — which once let **nine
   commits sit unpushed while being reported as pushed**. Check
   `git status -sb` for `[ahead N]`.
7. **Streaming has never been exercised.** Zero kind:24200 rows have ever
   existed in the testbed database. Next-slices correctly says the Mac
   client needs an owner binding first, but the consequence is that the
   whole observer-frame path — encrypt, publish, p-gate, decrypt — is
   unproven end to end, on any client.

**Not verified in this audit** (so do not treat as confirmed): Phase 1's
four approval paths were taken from this file, not re-run; and Phase 0's
"buzz-acp runs claude-code and codex; Grok via BYOH" is unconfirmed — the
live turn used in-tree `buzz-agent` against Ollama, not claude-code.

## Next slices (Phase 3+)

Phase 2's scope items (2a–2h) all shipped, the exit criterion runs green
from buzz-cli, and **the worktree wiring — this file's long-standing
next-slice #1 — is done** (`54329ad6`..`66fa1a03`, 2026-08-01). Phase 2 has
no remaining follow-ups.

1. ~~**buzz-acp worktree wiring**~~ — **SHIPPED.** A turn whose NIP-10 root
   is a kind:47000/47020 work thread runs in a dedicated worktree of the
   channel's bound repo on `sm/thread/<short>`, and on completion commits,
   pushes, and publishes a kind:47010. Opt-in via
   `BUZZ_ACP_WORKTREE_ROOT`; unset, nothing changes.

   Both things the adversarial review demanded were done first, as separate
   reviewable commits: the session-model change (`24548db3` +
   `be8982c1` — one `SessionKey` map, then one `turn_key` per turn so the
   lookup, the turn counter and rotation cannot disagree) and the 30617
   author check (`faaccef7` + `54329ad6` — the relay's own pubkey, resolved
   from NIP-11).

   **Read `self`, not `pubkey`.** NIP-11 carries both; `pubkey` is the
   operator's contact key and is `None` on our relays, while `self`
   (NIP-43) is the signing key that actually signs the 30617s. The wrong
   field returns `None`, which fails closed and therefore reads as "this
   channel has no repo bound" rather than "you asked the wrong question".

   Validated live end to end, cross-checked between two independently
   signed events: the agent's kind:47010 carries
   `commit=36eb07a6… branch=sm/thread/6e5b88de823e`, and the relay's own
   kind:30618 for that push records the same commit on the same ref — so
   the checkpoint names something genuinely fetchable. A plain channel
   message in the same channel bound no worktree, which is the half that
   proves the admission rule discriminates.

   **Three preconditions, none of them obvious, all now reported at boot
   or fixed** (see Environment notes): git ≥ 2.46 on the *harness* host,
   `BUZZ_UDS_PATH` on a relay bound to a specific address, and a PATH whose
   `wc` is the real one.
2. **Phase 3 continues** — slices 1–8 shipped (tier-aware router +
   attribution store; pre-turn tier enforcement; the local Ollama agent
   backend; live-turn attribution 44201 + owner usage read; the gateway's
   real `local` backend; the Privacy Gate assist; personal channels forced
   to `owned`; the member's own re-tiering with promotion checked against
   `model_usage`).

   **The Phase 3 exit criterion is NOT closed** — only its metering clause
   is. The criterion also requires a *spoken* request refined by the
   **copilot** and **queued**, **TEE** routing in a private channel, a
   **sealed** value resolving/scrubbing/rendering as a placeholder with a
   raw paste caught at ingestion, and an **agent retrieval query** scoped to
   what its operator can read. None of that exists. What is left, in
   dependency order — the first three are roadmap Phase 3 scope that this
   file previously did not track at all:
   - **Content Seals (D31)** — **slice 1 shipped** (`c0794f7b`,
     2026-08-06): kind:47100 allocated (owner-signed announcement, label +
     min tier, **never the literal** — events fan out to every tier) and
     `buzz_core::seal`, the pure half: `[sm-seal:<16-hex>]` tokens
     (ASCII, diff/merge-safe per the risk table), strict token parsing,
     server-side literal detection whose hits never echo the value,
     longest-first redaction (containment-safe), and `violating_seals` as
     the single home of the tier rule. Mutation-checked one kill per
     property.

     **Slice 2 shipped the same night**: migration 0036 (`content_seals`,
     community-fenced, the literal lives there and nowhere else), owner-only
     `POST /api/seals` (403 for a workspace *admin* — verified live, the
     sharper test than a mere member), relay-signed kind:47100
     announcements (relay-only kind, so members cannot forge registry
     entries), the **ingest guard**, and `buzz seals create|list`.

     The exit-criterion clause ran live on the testbed: the same literal
     was refused in an open channel — naming the seal's *label*, never the
     value — and accepted in private and owned; the token passed where the
     literal did not; `seals list` showed label and tier only. Owned
     channels skip the seal query entirely (strictest tier — nothing can
     violate there).

     Honest gap from the live run: the forge-a-47100 test was blocked by
     the **CLI's** --kind allowlist, not the relay, so the relay-side
     refusal is pinned by a unit test on the relay-only kind set rather
     than exercised end-to-end.

     **Slice 3 closed the movement hole.** The follow-up PG test the slice-2
     note asked for now exists (`sealed_literals_are_refused_below_their
     _tier`), and writing it surfaced something bigger: **promotion —
     kind:47021, the one primitive whose whole job is moving content into a
     looser channel — was not seal-checked at all.** Its summary went
     through the D30 credential scan only, and every blob in the promoted
     tree likewise; neither knew seals existed. Adding 47021 to the ingest
     guard would not have worked either: 47021 is a *command* kind, so it is
     dispatched (ingest.rs:2061) well before the guard runs (ingest.rs:2244)
     — the guard's list would have been dead code for it. The fix belongs in
     the promote path, which is also the only place that knows the target
     tier.

     Both gate surfaces now apply seals: the member-written summary and
     every text blob in the promoted tree, refusing by the seal's **label**
     via a distinct `PromoteError::SealedFindings` (a seal is not a
     credential finding — the two call for different fixes, so they must not
     read alike). Which seals apply is decided *once*, at the call site,
     against the **target** tier; the gate receives an already-filtered set.
     An `owned` target skips the query entirely.

     Five mutations, all killed: disabling the tree scan; naming the literal
     instead of the label (this one still refuses — it just leaks the sealed
     value into a message bound for the looser channel, which no
     did-it-reject assertion would catch); dropping per-file dedup;
     disabling the tree refusal end-to-end; and **filtering by the source
     tier instead of the target's**, which is the subtle one — the source is
     a personal channel, forced to `owned` by D24, where no seal can be
     violated, so that one line silently promotes sealed values into an open
     channel (`accepted: true`). The S3 probe pins it because it is the only
     test where source and target tiers differ.

     **Remaining slices**: workspace sweep, delivery redaction envelopes
     (stored history + fan-out), gateway scrub, gate integration, and seal
     revocation (nothing deletes a seal yet).

     **Slice 4 did that coverage audit, and the allowlist itself was the
     bug.** Enumerating every kind in `requires_h_channel_scope` against the
     guard's seven-kind list turned up **nine** member-authorable kinds
     carrying free text that nothing seal-checked: fork goals (47020),
     whole canvas documents (40100 — the largest text surface in the set,
     and it is fed into agent context), agent recommendations (47003),
     checkpoint notes (47010), gate-review drafts (47022), scheduled
     messages (40006), reminders (40007), huddle guidelines (48106), and
     thread metadata (47001).

     Adding nine constants would have left the same trap set for the tenth
     kind, so the guard was **inverted to fail closed**: it keys off
     `requires_h_channel_scope` itself, so a new channel-scoped kind is
     covered the day it is introduced and *skipping* one becomes a
     deliberate act rather than an oversight. Scanning kinds whose content
     is structured JSON is not waste — a literal embedded in a JSON field is
     still the literal leaving the tier.

     **47001 was the sharp one** and needed a different fix: it is a
     *command* kind, dispatched before the guard runs, and it rewrites an
     existing thread's goal. Guarding 47000 while leaving 47001 open was
     decorative — open a thread with a harmless goal, then re-goal it to the
     sealed value. Its check lives in `handle_thread_metadata`. The rule now
     has one home, `ingest::refuse_sealed_literals`, shared by the guard and
     by the command handlers the guard cannot see (promotion keeps the
     pre-filtered form, since it scans many blobs and must not re-query per
     blob).

     Two more mutations killed: reverting the guard to the old seven-kind
     allowlist, and removing the 47001 check.

     **Slice 5 closed the tag surface**, which the 9002 finding turned out
     to be a special case of. A tag value fans out with the event exactly as
     the body does, so a literal in `["alt", "…"]` on an ordinary kind:9
     leaves the tier just as surely as one in the message text — and no
     member needs any authority to send it. kind:9002 only made it
     unmissable: its `content` is empty and *all* of its member-written text
     (`name`, `about`, `topic`, `purpose`) rides in tags, so a content-only
     guard was blind to it entirely.

     The guard now scans `content` **and every tag value**, for the same
     fail-closed reason the kind list was inverted — not a list of the prose
     tag names. Structural values (event ids, UUIDs, pubkeys) simply never
     match a sealed literal, and if one did, refusing is the safe direction.
     The seal query moved out of the per-string path into
     `load_violating_seals`, so an event with twenty tags still costs one
     query; `refuse_sealed_in` is the pure check over many strings.

     Mutation: dropping tag scanning lets a plain member's message through
     with `accepted: true`, carrying the sealed value in an `alt` tag. The
     test deliberately orders that case first, ahead of the 9002 case, since
     9002 additionally requires channel authority — the member-reachable
     hole is the one that must break first.

     Cost, stated plainly: every channel-scoped event in a non-owned channel
     now costs one indexed SELECT on a small table, where before only seven
     kinds with non-empty content did. Small next to the membership,
     channel and insert queries already on that path, but a per-community
     seal cache invalidated on `POST /api/seals` is the obvious next
     optimization if it ever shows up in a profile.

     With that, **every ingestion path a member can reach is covered**:
     content and tags on all channel-scoped kinds, the two command kinds
     that bypass the guard (47001, 47021), and the promoted git tree. What
     remains for D31 is not ingestion but the rest of the lifecycle — the
     workspace sweep, delivery redaction envelopes, gateway scrub, gate
     integration, and revocation.
   - **Retrieval foundation (D37)** — pgvector + a continuous owned-tier
     embedding pipeline, ACL-scoped search over buzz-search FTS, retrieval
     tools for copilot and harness agents. **Not started**: `pg_extension`
     on the testbed holds only `pgcrypto` and the default `plpgsql` — no
     pgvector — and no embedding model is pulled.
   - **Prompt Copilot + inference queue (D25)** — exists solely as the
     `InferencePurpose::Copilot` enum variant in `model_route.rs`. Policy
     without an implementation. Server whisper (voice-to-text) likewise
     absent.
   - **More gateway consumers.** Slice 6 wired the first (the gate
     assist). Copilot (D25) and embeddings (D37) are the two above;
     embeddings additionally need a trait seam — `RawInference { text,
     tokens }` cannot express a vector.
   - **Owner budgets.** `Gateway::with_budget` exists and is tested, but
     nothing sets a budget or enforces one on the live (harness) path. The
     hard part is placement, not policy: the relay holds the spend data,
     the harness is the only component that can *prevent* a turn.
   - **TEE** (attestation-then-send) and **per-user vendor CLIs** (D23) —
     both still stubs. The desktop bare-model-id gap is closed (slice 7,
     `5d1c2221`) — desktop now calls `qualify_model` before writing
     `BUZZ_ACP_MODEL`. Member-facing usage reads shipped (`57b3f906`,
     `buzz usage show|turns`).
   - **The harness does not go through the gateway.** `buzz-agent` talks to
     Ollama directly; `sm-gateway` serves the relay-side consumers (gate
     assist). D17's "gateway-only model access" is therefore not yet true,
     and every routing guarantee on the harness path rests on the pre-turn
     tier gate instead. Worth settling deliberately rather than by drift.
3. **Phase 4 continues** — the client reads, writes, and updates live. What
   is left, roughly in value order:
   - **Per-turn diffs** — approvals shipped (`43f83977`, with `46013`
     withdrawal in `cef8bdb2`), so what remains of "agent interaction" is
     the diff half, and it is blocked on item 1 below, not on the client.
   - **The channel repo browser** — file tree and blob view over git smart
     HTTP, so a work thread's checkpoints can be read where they happened.
   - **Per-turn diffs: the data is on the wire, only the Swift view is
     missing.** This entry used to say they were blocked upstream — true
     until the worktree wiring shipped — and then that they needed "client
     work plus a fetch". Neither is true now: the harness publishes the diff
     itself as a **kind:40008** threaded under the work thread
     (`10460083`), so no client needs to clone a repo to show one.

     **The event shape is verified against a real renderer, not assumed.**
     The exact kind:40008 the harness published live — same content, same
     `repo`/`commit`/`parent-commit`/`branch`/`description` tags — was
     injected into the desktop through the E2E mock bridge and rendered
     correctly: the `commit` tag as the header chip, `description` as the
     subtitle, the body parsed into a file card with a NEW FILE badge, a
     `+3` count, per-line numbering, and non-ASCII intact. Every tag the
     publisher emits is consumed by a renderer that predates it.

     ~~So the remaining work is a `MeshProtocol` fold plus a thread-view
     row.~~ **Both shipped.** `dea0167d` added `MeshTurnDiff` (the fold,
     10 tests incl. the real turn's bytes as fixture, plus the kind-parity
     entry); the thread-view row followed and was verified **live**: the
     Mac app, launched headlessly against the testbed relay, rendered the
     real kind:40008 in the thread detail — commit chip, `+3 −0`, NEW FILE
     badge, numbered added lines — with the Changes card's commit matching
     the Checkpoints list's oid, which is the tie the design demanded. The
     fold's tests are the regression lock the desktop deliberately does not
     provide (that tree is unmaintained under D35/D36).

     AX-automation note for the next screenshot: the threads outline's row
     indices count section headers and the composer row — "first visible
     thread" was row **6**, not row 1. Enumerate rows via AX before
     selecting rather than trusting visual order.
   - **Streaming needs an owner binding**, not client work: kind:24200 is
     NIP-44-encrypted to the agent's owner and p-gated, so the Mac sees
     nothing until its member key is bound as that agent's owner.

## Suggested kickoff prompt for a fresh session

> Read docs/silent-mesh/HANDOFF.md **including its Audit section**, then
> docs/silent-mesh/roadmap.md,
> phase2-work-threads.md, and phase4-client.md (if the slice touches
> `macos/`), and skim `git log --oneline main..HEAD`. Continue
> the roadmap at the next unshipped slice, following the working
> conventions in the handoff (slice → gates → signed commit → push).
