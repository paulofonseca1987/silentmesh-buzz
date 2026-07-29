# Phase 1 Design Note — Governance: approvals wired end-to-end

Implements roadmap Phase 1 (architecture §8). Written from verified code study of
both codebases on 2026-07-29: this fork at `90e058e`, and T3 Code (the design
blueprint, decision D33) at `887dd6e4`. File:line references below are real;
re-verify line numbers after upstream merges.

> **Status (2026-07-29): P1–P6 landed** on this fork. P1 (WF-08 workflow
> gate), P2 (typed PermissionRequest + correlation), P4 (relay:
> `agent_permission_requests` migration/CRUD, `/api/approvals`
> create/list/resolve, 46030/46031 agent-domain dispatch, 46011/46012 +
> 24200 decision frame), P3 (runtime-mode policy, parking with
> poll-until-decision, fail-closed everywhere), P5 (`buzz approvals`
> family), P6 (supervised deploy defaults + urgent 46010). Deviations from
> §3: decision delivery is poll-first (the 24200 frame is emitted but the
> harness treats the authenticated read as the authority; the mpsc frame
> lane remains a fast-path follow-up), and the harness registers requests
> before its own turn-loop poller rather than a separate parking table.
> Remaining from §5: the stub-ACP-agent e2e and flipping the conformance
> pending lanes (needs the two-host docker harness).

**Goal.** Replace auto-approval of agent actions with a policy-gated,
human-in-the-loop flow whose grants are signed events — "who approved what" is
provable. Exit criterion (roadmap): a supervised claude-code agent blocks on a
shell command until a human grants via buzz-cli; grant is a signed event;
full-access proceeds ungated; a denied request returns a refusal to the agent;
all four paths visible in the audit chain.

## 1. The strategic finding

Buzz contains **two unrelated approval systems today**:

1. **Workflow approvals — 90% built, 0% reachable.** Kinds 46010/46011/46012
   (requested/granted/denied) + 46030/46031 (user-signed grant/deny commands);
   a `workflow_approvals` table with expiry and TOCTOU-safe CRUD
   (`crates/buzz-db/src/workflow.rs:925-1069`, `AND status='pending'` guard,
   doc says treat 0-rows as 409); relay grant/deny handlers with tenant
   scoping, expiry check, approver check, idempotent tx, and a working resume
   driver (`crates/buzz-relay/src/handlers/command_executor.rs:1029-1370`);
   CLI `buzz workflows approve` (`crates/buzz-cli/src/commands/workflows.rs:192`);
   SDK builder (`buzz-sdk/src/builders.rs:1522`). Dead because of exactly one
   gap: **nothing ever calls `create_approval` or emits 46010** — the executor
   returns `Suspended{approval_token}` and the driver marks the run Failed
   with "approval gates not yet implemented — see WF-08"
   (`crates/buzz-workflow/src/lib.rs:229-249`, `executor.rs:650-670`).
2. **ACP tool-call permissions — hardcoded allow.**
   `crates/buzz-acp/src/acp.rs:1864 handle_permission_request` reads only
   `msg.id` and `params.options[].{kind,optionId}`, picks `allow_once`
   (fallback `reject_once`), responds inline. It never reads
   `params.toolCall` (tool name/kind are parsed separately in
   `handle_session_update` acp.rs:1725-1743 and not correlated). No policy, no
   parking, no human anywhere.

**Phase 1 = finish system 1 (cheap, pure upstream value), then route system 2
through system 1's transport.** We reuse the kinds, the command path, the
CRUD patterns, the feed's "Needs Action" bucket (`buzz-db/src/feed.rs:168+`),
and 46010's existing push eligibility (`push_lease.rs:15`) — inheriting
loop-prevention too (`is_workflow_execution_kind` covers 46001..=46012 and is
excluded from workflow trigger matching, `buzz-workflow/src/lib.rs:1458`).

## 2. What we port from T3 (and what we deliberately change)

T3's model (verified in code, not its stale docs — the doc says two modes; the
code has four: `packages/contracts/src/orchestration.ts:117-124`):

| Ported as-is | Where T3 does it |
|---|---|
| Four-decision vocabulary: accept / acceptForSession / decline / **cancel** (cancel = abandon turn ≠ decline = refuse tool, continue) | `orchestration.ts:131-137`; teardown resolves pending with *cancel*, not deny (`ClaudeAdapter.ts:3034-3052`) |
| Server-minted request IDs, provider-native IDs kept only for correlation | `ClaudeAdapter.ts:3378`, `CodexSessionRuntime.ts:969-994` |
| Human-facing payload = request kind taxonomy (`command` / `file-read` / `file-change`) + pre-rendered ≤400-char `detail` string + full tool input carried separately | `summarizeToolRequest` `ClaudeAdapter.ts:829-855`; `ProviderRuntimeIngestion.ts:292-345` |
| One-approval-at-a-time queue semantics; a thread/channel blocked on a human is a first-class badge state | `ComposerPendingApprovalPanel.tsx:31-33`; `hasPendingApprovals` shell flag |
| Policy ladder concept: `approval-required` → `auto-accept-edits` → `auto` → `full-access` | mode→provider mapping tables in each adapter |

T3's decision vocabulary maps **natively** onto ACP option kinds Buzz already
sees in tests (`acp.rs:2274-2297`): accept→`allow_once`,
acceptForSession→`allow_always`, decline→`reject_once`, and cancel→the
existing `permission_response_cancelled(id)` builder (`acp.rs:2022`). Nothing
new on the wire.

Deliberate divergences from T3 (its verified weaknesses):

1. **Approvals get expiry.** T3 has none — a pending approval blocks forever.
   Buzz's `workflow_approvals` already has `expires_at` + `'expired'` status.
   Agent permission requests default to a short TTL (proposed: 15 min,
   config), timeout ⇒ `cancelled` outcome to the agent + `expired` row.
2. **Typed staleness, no string sniffing.** T3 detects orphaned callbacks by
   substring-matching an error message in four places (reactor, decider, web).
   We persist the request row and make the stale path a typed state.
3. **Session-scoped `allow_always` is documented as session-scoped** — T3
   silently wipes those grants when a mode change restarts the session; we
   keep the restart invariant but say it out loud in the UX copy.

## 3. Target design

### 3.1 Policy: harness-side runtime mode

New config on buzz-acp (`crates/buzz-acp/src/config.rs`, sibling of
`respond_to` at :537, mirroring its `allowed_*` self-escalation guard):

```
runtime_mode: RuntimeMode        # supervised | auto-accept-edits | full-access
allowed_runtime_modes: Vec<...>  # ceiling the operator grants this harness
```

- `full-access` — current upstream behavior (auto `allow_once`), now an
  explicit policy branch. **Upstream default stays `full-access`** so the
  patch is behavior-preserving; Silent Mesh's deploy config flips the default
  to `supervised` (D-decisions; the deploy profile sets it).
- `auto-accept-edits` — auto-allow requests whose correlated
  `toolCall.kind` is an edit/file op; gate everything else. Requires §3.3's
  toolCall correlation; ships in the same patch or degrades to `supervised`.
- `supervised` — gate every permission request through the flow below.
- T3's fourth mode (`auto`) is reserved, not implemented — its Codex
  semantics (agent self-review) have no ACP equivalent.

Phase 1 scope is per-harness (flat `Config`, matching everything else there).
Per-channel and per-thread modes arrive with sm-work in Phase 2; the
`allowed_runtime_modes` ceiling is designed now so those can only narrow.

Note: buzz-acp's existing `PermissionMode` (`config.rs:123-139`) is an
*agent-side* knob (`session/set_config_option`, configId "mode") — unrelated.
Supervised operation should also stop passing `BypassPermissions` there, or
agents will never emit permission requests to gate.

### 3.2 Data: `agent_permission_requests` table (new migration)

The `workflow_approvals` table is workflow-keyed (composite FKs to
`workflows`/`workflow_runs`, `0001_initial_schema.sql:411-435`) — don't bend
it. New table copying its proven idioms (community-prefixed PK, hashed token,
`AND status='pending'` updates, expiry):

```
community_id, request_id (UUID, server-minted — PK with community),
token BYTEA (sha256, for 46030/46031 d-tag compatibility),
channel_id, agent_pubkey, harness_session/turn ref,
request_kind ('command'|'file-read'|'file-change'|'other'),
tool_name, detail TEXT (≤400 pre-rendered), payload JSONB (full tool input),
options_offered JSONB (the ACP option list),
status ('pending'|'granted'|'denied'|'cancelled'|'expired'),
decision ('allow_once'|'allow_always'|'reject_once'|'cancel') NULL,
decider_pubkey, note, created_at, expires_at, resolved_at
```

CRUD in `buzz-db` mirrors `workflow.rs:925-1069` including the 0-rows⇒409
contract.

### 3.3 buzz-acp: typed requests, parking, decision delivery

- **Typed parsing + correlation.** Introduce a `PermissionRequest` struct
  (id, options, toolCall{id,title,kind,input}) replacing the raw
  `serde_json::Value` poking; correlate with the `tool_call` session/update
  by `toolCallId` (today parsed only for logging, acp.rs:1725-1743). This is
  a pure refactor patch with no behavior change.
- **Park without blocking the read loop.** `handle_permission_request` is
  called inside the stdout read loop (`read_until_response`, acp.rs:1228,
  1672) — awaiting a human inline would freeze session updates and cancels.
  Instead: register the pending request (state already half-exists:
  `pending_permission_id` acp.rs:160) and **return without responding** —
  JSON-RPC allows deferred responses; the loop continues. A spawned timer
  enforces `expires_at` ⇒ `permission_response_cancelled` + row `expired`.
  `cancel_with_cleanup` (acp.rs:995-1005) already covers teardown: it
  responds `cancelled` to a stranded request — keep that, and mark the row.
- **Create the request.** On supervised gate: POST to a new relay endpoint
  (NIP-98-signed by the agent key, `invites.rs:230-261` authenticate pattern)
  that inserts the row and triggers 46010 emission. REST rather than a signed
  event because the payload (full tool input) shouldn't be a permanent public
  event in the channel — the 46010 notification carries the summary, the row
  carries the payload.
- **Decision delivery — reuse the encrypted control channel.** buzz-acp
  already receives out-of-band control frames: kind 24200 observer frames,
  NIP-44 encrypted, ±300s freshness, dispatched by payload type
  (`cancel_turn`, `switch_model`) into the in-flight turn
  (`lib.rs:837-939, 1882-1901, 2780-2798`). Add payload type
  `permission_decision {request_id, decision}`. **One structural fix
  required:** delivery uses a consumed-once `oneshot` (`ControlSignal`,
  `pool.rs:263-283`) keyed by channel — an approval loop needs repeatable
  delivery. Add an `mpsc` lane for permission decisions in the task metadata
  alongside `control_tx` (steering already added an mpsc precedent,
  `pool.rs:67`).

### 3.4 Relay: emission, decision handling, notification

- **Emit 46010** (relay-signed) into the channel on request creation —
  pattern: `emit_system_message`, `side_effects.rs:760-771`
  (`EventBuilder…sign_with_keys(&state.relay_keypair)`). Tags: `d` = token
  hash (46030/46031 compatibility), channel, `p` = agent pubkey; content =
  `{request_kind, tool_name, detail, expires_at}` — the summary only.
- **Extend 46030/46031 handling** (`handle_approval_grant`/`deny`,
  `command_executor.rs:1029-1278`): resolve the `d` token hash against
  `workflow_approvals` first (existing path), else `agent_permission_requests`
  → authz (§3.5) → TOCTOU update → dispatch a `permission_decision` control
  frame to the agent's harness → **emit 46011/46012** (declared today, used
  nowhere) referencing the 46010 and the human-signed 46030/46031 event id.
  The human's Schnorr signature on 46030/46031 is the "who approved what"
  proof; 46011/46012 is the outcome record; the audit hash chain covers both.
- **Read side (new):** `GET /api/approvals?status=pending` (NIP-98,
  membership-scoped) — nothing like it exists; needed by CLI and later
  clients. Push urgency: 46010 is push-eligible but `URGENT_KINDS` is empty
  and `class:"urgent"` leases are rejected today (`push_lease.rs:15-16,285` —
  note: the `urgent_kinds:[46010]` at :684 is test-fixture only). Flip =
  one-line addition; reusing 46010 avoids the migration-trigger allowlist
  churn a new kind would require (drift test at `push_lease.rs:697`).

### 3.5 Authorization (v1, ahead of D42's full hierarchy)

A decision (46030/46031) for an agent permission request is accepted from:
any **full member of the channel** the request lives in (membership check in
the handler, as everywhere else), with relay owner/admin always allowed.
`check_approver_spec` (`command_executor.rs:1004-1027`) fails closed on
role specs — extend it with a `channel-member` spec used by agent requests;
role-based specs (`"@release-manager"`) stay unimplemented-and-fail-closed.
D42's Channel-Admin/Workspace-Admin distinctions arrive in Phase 2; nothing
here conflicts with them.

### 3.6 Close WF-08 while we're here

First patch of the series (also the most upstreamable): in the
`approval_token` branch (`buzz-workflow/src/lib.rs:229`), call
`create_approval` (expiry = step `timeout`, default 24h), set run
`WaitingApproval` at `result.step_index`, emit 46010. The entire downstream —
grant/deny handlers, resume driver (`resume_workflow_after_approval`,
`command_executor.rs:1279`), CLI — already works and is merely unreachable.
This validates every consumer surface (feed, push, CLI, audit) with zero ACP
coupling before we touch buzz-acp.

## 4. Patch series (upstream-shaped, D35)

| # | Patch | Behavior change upstream? |
|---|---|---|
| P1 | WF-08: create approval row + WaitingApproval + emit 46010; emit 46011/46012 from grant/deny; tests | Fixes their documented TODO — none beyond it |
| P2 | buzz-acp: typed PermissionRequest + toolCall correlation | None (refactor) |
| P3 | buzz-acp: runtime-mode policy + deferred-response parking + timeout + control-channel `permission_decision` (mpsc lane) | None by default (`full-access` default preserves auto-allow) |
| P4 | Relay: `agent_permission_requests` migration + CRUD + create/list endpoints + 46010 emission + 46030/46031 dispatch extension | Additive |
| P5 | CLI: `approvals` family — list / show / grant / deny (clap wiring per `lib.rs:174-240` + `commands/` pattern; grant/deny sign 46030/46031 like `workflows.rs:192-213`) | Additive |
| P6 | Silent Mesh deploy config: default `supervised`, stop passing agent-side `BypassPermissions`, flip `URGENT_KINDS` += 46010 | Ours (deploy profile), not upstreamed |

P1 and P2 can land in either order; P3 depends on P2 and P4; P5 on P4.

## 5. Test plan

- **Unit**: policy ladder (mode × toolCall.kind ⇒ gate/allow), typed request
  parsing incl. numeric and string JSON-RPC ids (acp.rs handles both today),
  expiry state machine, `check_approver_spec` `channel-member` spec, TOCTOU
  double-grant ⇒ second gets 409/race-reject (pattern exists,
  `command_executor.rs:1104`).
- **Integration** (isolated harness, `docker-compose.harness.yml`; style of
  `e2e_managed_agent.rs` — note it is event-round-trip style, no agent
  process): WF-08 flow — workflow with `request_approval` step suspends, row
  created, 46010 queryable, CLI grant resumes run, 46011 present; deny
  cancels run with 46012.
- **New e2e with a stub ACP agent** (nothing in-tree drives a real agent
  through a permission request — this is net-new): a tiny scripted binary
  speaking ACP over stdio that requests permission for a fake shell command.
  Supervised: harness parks, 46010 appears, `buzz approvals grant` →
  agent receives `selected/allow_once` → runs; deny → `reject_once`;
  timeout → `cancelled` + row `expired`; full-access: no 46010 at all.
  Non-member grant rejected. Conformance: the pending approval lanes in
  `conformance_multitenant.rs:1867,1934` (currently documenting the gap)
  flip to passing.

## 6. Open items

1. Timeout default (proposed 15 min) and whether `allow_always` should also
   write a durable per-agent allowlist row or stay session-scoped (T3:
   session-scoped; proposed: session-scoped v1).
2. Whether P1/P4 reuse kind 46010 for both domains permanently (proposed:
   yes — tag-discriminated; inherits push/feed/loop-prevention) or the agent
   domain later moves to a 47xxx kind.
3. Coordination with Block: their ARCHITECTURE.md already claims grant/deny
   endpoints exist — open an upstream issue referencing WF-08 before P1 lands
   so the series is expected (D35: upstream what belongs upstream).
4. Correction recorded during research: 46010 is **not** urgent-classified
   upstream (empty `URGENT_KINDS`); earlier session notes claiming otherwise
   were based on a test fixture.
