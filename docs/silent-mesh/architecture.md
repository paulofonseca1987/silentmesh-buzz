# Silent Mesh — Target Architecture

Silent Mesh is a **fork of Buzz (block/buzz)** that absorbs **T3 Code's governed
agent-work model**: a self-hosted, private workspace where a team of humans and AI
agents build together — Buzz's relay, channels, identity, and git forge underneath;
T3-style work threads with worktrees, checkpoints, and human-approved agent actions
on top; privacy-tiered model routing and a native Swift client as Silent Mesh's own
contribution.

This document records the agreed direction and the target architecture. The phased
delivery plan lives in [roadmap.md](./roadmap.md).

## 1. Decision log

Decisions taken with the owner (2026-07-28), in the order they were made. D1–D32
were taken against a T3-Code-base assumption; after a comparative analysis of both
codebases, D33 re-decided the foundation. **All D1–D32 product semantics survive**
— what changed is which half is inherited versus built.

| # | Decision | Choice |
|---|---|---|
| D1 | Base | ~~Fork of the T3 Code monorepo~~ *(superseded by D33)* |
| D2 | Identity | Nostr identity like Buzz: every human and agent has a keypair; every action is a signed event *(now inherited)* |
| D3 | Nostr depth | **Full relay wire compatibility** *(now inherited — this was the maximal-cost item on the T3 base and is free on the Buzz base)* |
| D4 | Team model | One community per server; channels with explicit membership; owner controls per-user permissions and per-folder/file access *(membership/tenancy inherited; folder ACLs new; role set finalized by D42 — Guest dropped)* |
| D5 | Sidebar mapping | **Channel = folder on the server; LLM thread = Slack-style thread inside the channel** |
| D6 | Repo topology | One git repository per channel *(implemented as a channel↔repo binding convention on Buzz's forge)* |
| D7 | Channel conversation | Main message stream (kind 9) + branching LLM work threads *(stream inherited; work threads new)* |
| D8 | Agent execution | Server-side only *(inherited — buzz-acp spawns agents as server-side child processes)* |
| D9 | File sync | Git-native: clients sync channel repos through ACL-enforcing git endpoints; offline edits are local commits *(transport inherited; client sync new)* |
| D10 | Clients | Native Swift/SwiftUI macOS app first, native iOS later; server is headless for us — inherited Buzz clients are **not shipped** (D36) |
| D11 | Remote access | VPN only (LAN / Tailscale / WireGuard / SSH); no third-party relay services. *The public wiki (D43) lives on a separate internet-facing host holding only already-published content — the workspace server stays dark* |
| D12 | Server platform | Linux |
| D13 | Client security | Files encrypted at rest on the client, accessible only inside the app; app lock via Face ID / Touch ID / PIN |
| D14 | Offline | Synced files remain editable offline; local on-device models handle summarize, translate, and voice-to-text |
| D15 | Vendor harnesses | **Claude, Codex, and Grok CLI** — via buzz-acp's existing `claude-code-acp` / `codex-acp` adapters and its BYOH generic-runtime support for Grok |
| D16 | Model plane | All model traffic routes through a **server-side model gateway with per-user token metering** *(builds on buzz-acp's usage metering and buzz-agent's provider layer)* |
| D17 | Native harness | Silent Mesh's own user-customizable agent harness *(realized by extending **buzz-agent** — Buzz's in-house 19k-line agent — with per-user profiles and gateway routing)* |
| D18 | Airgapped agent | V1 requirement — server-local models make zero-egress channels possible from launch |
| D19 | Client local models | Bundled open models (whisper.cpp + llama.cpp/MLX); macOS 14+ floor |
| D20 | Kind registry | Standard NIPs + Buzz's kinds *(now native — Silent Mesh's own 47xxx block rides alongside)* |
| D21 | Privacy tiers | **owned** (client-local — the only offline mode — or server GPUs), **private** (owned + TEE-attested provider), **open** (member's own vendor subscriptions) |
| D22 | Private complex tasks | TEE-based confidential inference provider, routed and metered by the gateway |
| D23 | Non-private inference | Each member's **own Claude Code / Codex / Grok subscription**, per-user credentials isolated server-side *(buzz-acp already runs these CLIs; per-user credential isolation is the addition)* |
| D24 | Channel privacy policy | Every channel declares a minimum tier; the gateway router refuses routes below it |
| D25 | Prompt Copilot | A private local model that sees task files, takes voice/text intent, refines prompts, asks clarifying questions, and queues jobs for the most adequate model within channel policy |
| D26 | Immutable channel tier | Tier fixed at creation; the only re-tiering path is an **owner-only channel clone** (repo forks, conversation stays behind) |
| D27 | Thread forking | Any member clones a thread at head or any checkpoint into a variation (branch + worktree) in the same channel; conversation inherited by reference |
| D28 | Thread archiving | Settled / snoozed / archived as core lifecycle states, signed events; losing forks archive |
| D29 | Personal channels + promotion | Every member gets a private personal channel (self + their agents); private thread work **promotes** into team channels — files transplant, conversation stays behind |
| D30 | Privacy Gate | Mandatory local-model review (summary, flagged spans, approved obfuscations) before any privacy-weakening movement |
| D31 | Content Seals | Owner seals any word/value/component at a minimum tier, **workspace-wide**: sweep, ingestion guards, delivery redaction, gateway scrub, token-by-reference |
| D32 | Deep seal | Optional retroactive git-history purge as the leak-correction mechanism; owner-only, blast-radius-confirmed, heavily audited |
| **D33** | **Base (final)** | **Fork block/buzz (v0.5.x)**. T3 Code becomes the **design blueprint** for the work-thread + approval layer, not the code base. Rationale: the two projects have complementary holes — Buzz's hardened relay/forge/tenancy (~110k lines, 3,717 tests) is the bigger, subtler half to rebuild; T3's work-thread + approval design is the smaller port and is exactly Buzz's one unfinished area |
| D34 | Stack | Rust-first backend accepted; TS only where inherited surfaces need it; Swift for clients |
| D35 | Fork posture | **Track upstream**: additive crates/middleware/new kinds over invasive edits; no mass renames (brand at config/deploy level); upstream-owned trees (desktop/mobile/web) stay in-tree **unbuilt** rather than deleted, so rebases stay clean; upstream our governance work where Block will take it |
| D36 | Interim client | **CLI-only until the Swift MVP** (buzz-cli + scripted/test clients + admin CLI). Inherited Buzz clients are not shipped or supported, though the in-tree desktop app remains available as a developer debugging tool |
| D37 | Indexing & retrieval | The server continuously runs an **indexing + embedding service** (FTS + vectors) over all workspace content so users and agents retrieve information fast. Embeddings are computed **exclusively on owned hardware** (embedding via remote backends is wholesale egress and is prohibited by design); every retrieval query is **ACL-scoped** to the requester's readable channels |
| D38 | Canonical docs per channel | **Every channel is a knowledge channel** — no dedicated wiki channels. Each channel carries its own **canonical docs**: the current truth about its topic, living alongside its chat in the same repo. The workspace wiki is the **ACL-scoped union of all canons**, browsable wiki-style and searchable, with provenance. The service auto-maintains distilled canon it owns and opens **update proposals** elsewhere — never silent edits |
| D39 | Dispute escalation | When sources disagree on a data point (A vs B), the service raises a **dispute** escalated to a member with the privilege to decide — *per D42: the Channel Admin(s) of the affected channel; cross-channel disputes go to a Workspace Admin or the Owner; the decider must be able to read all sources (the steward concept is folded into Channel Admin)*. The decision is a signed event, and the service **propagates the chosen value throughout** — canons updated, correction proposals opened wherever the losing value appears |
| D40 | Thread = task | A work thread is launched **as a task**: a goal, an optional **deadline**, and a **DRI** (directly responsible individual — human or agent). When the thread completes (closed/archived), its output is **canonicalized** into the channel's canonical docs; overdue deadlines notify the DRI in the stream |
| D41 | Thread states & authority | State machine: `open` (⇄ `snoozed`) → `ready` (done proposed) → `closed` (with or without canonicalization) → `archived`. **Any human** opens threads and edits task metadata — explicitly or by confirming an **agent recommendation**; agents only ever recommend. **Only a human closes/archives/distills** — *per D42: Channel Admins within their channel, the Owner anywhere* (the policy knob can later loosen to DRIs). In personal channels the member is implicitly the Channel Admin |
| D42 | Role hierarchy | Exactly five levels. **Owner** (one per workspace): everything — and **all privacy-weakening operations (tier-loosening clones, seals, deep seals) exclusively**. **Workspace Admins**: members & invites; team-channel creation (tier within owner-set bounds) and Channel-Admin appointment; agents & budgets; public-publishing approval. **Channel Admins**: manage everything in their channel — membership, terminal thread transitions (D41), knowledge-dispute decisions (absorbing D39's steward). **Members**: belong to channels, access their data, open threads, edit task metadata. **Public**: not a login — the published static wiki (D43). Guest is dropped; Bot remains the agent role; in personal channels the member is implicitly Channel Admin |
| D43 | Public wiki | Publishing is an **explicit act on selected canon pages**: Workspace Admin/Owner approval → Privacy Gate + seals at maximum strictness → **static-site export** (with its own public search index) pushed to a **separate internet-facing host**. Equivalent to the company's public website/datasheets. The workspace server remains VPN-only (D11); the public host holds only already-published content |

## 2. The core mapping

```
Buzz community          →  Silent Mesh server (one team per server)
Buzz Stream channel     →  Channel = folder = repo = chat + canonical docs on one topic
Buzz kind-9 stream      →  Channel conversation (humans + agents, @mentions)
Slack thread            →  Work thread (NEW: worktree, checkpoints, approvals — T3's design)
Linear-style task       →  The same work thread: goal, deadline, DRI; output
                           canonicalizes into the channel's docs on completion
Buzz Bot member         →  Agent (buzz-acp harness or buzz-agent instance, own keypair)
Buzz DM-with-self       →  Personal channel (membership of one + your agents, own repo)
Buzz desktop sidebar    →  Reference UX only — Silent Mesh ships the Swift client
```

The core product invariant survives the base flip: **channel membership _is_ the
file ACL**. A member sees the repos of the channels they belong to, and syncs
exactly those.

## 3. Provenance: inherited / ported / new / not shipped

### Inherited from Buzz (keep, configure, harden)

| Component | Where | Notes |
|---|---|---|
| Relay: Nostr WS (NIP-01/42), REST, subscription fan-out, membership-checked delivery | `crates/buzz-relay` (61.8k lines) | the foundation D2/D3/D4 wanted |
| Multi-tenant isolation + conformance suite | `docs/multi-tenant-*.md`, `conformance_multitenant.rs` | we run single-community but keep the fencing rigor |
| Git forge: smart-HTTP, CAS-on-object-storage, pre-receive policy hook, NIP-34 kinds, ref-pattern protections | `crates/buzz-relay/src/api/git/` (9.4k lines), `buzz-core/git_perms.rs` | the hardest-to-rebuild asset |
| ACP agent host: process pool, work queue, crash recovery, mid-turn steering, workspace layout (OUTBOX/REPOS), usage metering | `crates/buzz-acp` (34k lines) | runs claude-code / codex / goose / BYOH today |
| buzz-agent: in-house LLM agent (providers, MCP client, catalog, personas) | `crates/buzz-agent` (19k lines) | becomes the D17 native harness's chassis |
| Identity plumbing: keys, invites, pairing, `git-credential-nostr`, `git-sign-nostr` | `buzz-core`, `git-*-nostr` crates | agents sign commits as their Nostr identity — already real |
| Media (Blossom), search (Postgres FTS), audit hash chain, push gateway, workflows engine | respective crates | workflows kept but not a v1 focus |
| buzz-cli + test-client harness | `crates/buzz-cli`, `buzz-test-client` | the D36 interim surface and our conformance safety net |
| Ops: docker-compose, Helm charts, migrations, observability | `deploy/`, `migrations/` | single Linux server via the compose profile |

### Ported from T3 Code (design blueprint, reimplemented on Buzz)

| Capability | T3 reference | Silent Mesh home |
|---|---|---|
| Work threads: worktree per thread, per-turn **checkpoints** (hidden git refs), diff/revert, turn/activity model | `apps/server/src/{orchestration,checkpointing,vcs}/**` | new crate(s), e.g. `sm-work`, driving Buzz's forge storage |
| **Supervised approvals**: runtime modes (full-access vs supervised), per-tool-call pending approvals | `runtime-modes.md`, `projection_pending_approvals` | wired into buzz-acp's permission path + relay approval kinds (§8) |
| Thread fork / archive / promotion semantics (D27–D29) | T3 thread lifecycle + our D-decisions | `sm-work` + new 47xxx kinds |
| Plan-then-execute interaction mode, settled/snoozed inbox semantics | T3 threads | `sm-work` |
| Prompt-shaping UX detail (composer, per-turn diffs) | T3 web/mobile UI patterns | Swift client design reference |

### New (Silent Mesh's own)

1. **Governance completion** — end-to-end approvals (Buzz's one consistently
   unfinished area; §8). Candidate upstream contribution.
2. **Model gateway + privacy tiers + metering** (D16, D21–D24) and the
   **Prompt Copilot + inference queue** (D25) — §11.
3. **Folder/file write ACLs** inside a channel (D4) — extending the pre-receive
   policy hook from ref-patterns to file paths, plus agent tool-layer enforcement.
4. **Personal channels + promotion + Privacy Gate + Content Seals + deep seal**
   (D29–D32) — §7, §10.
5. **Channel↔repo binding + channel privacy tiers, immutable, with owner-only
   clone** (D5/D6/D24/D26).
6. **Swift macOS client** (vault, biometrics, git sync, offline local models,
   artifact viewer with durable comments) and later iOS — §12.
7. **Durable anchored comments on artifacts** (Buzz has frame-anchored media
   comments and canvases; we extend the pattern to HTML artifacts and files).
8. **Knowledge plane** (D37–D39) — continuous indexing/embedding, ACL-scoped
   retrieval for humans and agents, the reconciled wiki, and dispute
   escalation — §12.
9. **Public wiki pipeline** (D43) — approval-gated static export of selected
   canon to a separate internet-facing host, plus the workspace-level role
   hierarchy (D42) layered over Buzz's per-channel roles.

### Not shipped (kept in-tree, unbuilt — D35/D36)

Buzz's desktop (Tauri), mobile (Flutter), web repo-browser, and admin-web are
excluded from our build/CI but not deleted, keeping upstream rebases clean. The
desktop app doubles as a developer debugging tool. Keycloak and enterprise SSO are
disabled in our deploy profile. Huddles/audio, forum channels, canvases, and the
mesh-compute P2P feature stay upstream-maintained and dormant until wanted.

## 4. Server architecture (Linux)

The Buzz stack, single-host via compose, plus Silent Mesh's additive services:

```
                   ┌────────────────────────────────────────────────────────┐
                   │  silent-mesh server (Buzz fork, Rust, Linux host)      │
                   │                                                        │
  Swift client ────┤  WS: Nostr relay (NIP-01/42) ── buzz-relay             │
  (via VPN)        │  HTTP: git smart-HTTP (NIP-98) ── CAS on MinIO         │
  buzz-cli ────────┤  HTTP: Blossom media (NIP-98)                          │
  Nostr tooling ───┤                                                        │
                   │  ├─ membership/tenancy enforcement (inherited)         │
                   │  ├─ kind dispatch → side-effect handlers               │
                   │  │    ├─ sm-work: work threads, worktrees,             │
                   │  │    │   checkpoints, forks, promotion   (NEW)        │
                   │  │    ├─ approvals: wired end-to-end      (NEW §8)     │
                   │  │    ├─ buzz-acp: claude-code / codex / grok(BYOH)    │
                   │  │    ├─ buzz-agent: native harness, per-user profiles │
                   │  │    └─ audit hash chain (inherited)                  │
                   │  ├─ sm-gateway: privacy tiers · per-user metering (NEW)│
                   │  │    ├─ owned:   local models ── 2× RTX 4060 8 GB     │
                   │  │    ├─ private: TEE inference provider (attested)    │
                   │  │    └─ open:    per-user vendor subscriptions        │
                   │  ├─ sm-seals: sweep · ingestion guards · delivery      │
                   │  │    envelopes · gateway scrub               (NEW)    │
                   │  ├─ sm-knowledge: FTS+vector index · wiki              │
                   │  │    reconciliation · dispute escalation     (NEW)    │
                   │  ├─ sm-publish: gate-checked static export ──────────► │──► public host
                   │  │    (approved canon only, D43)              (NEW)    │    (separate, dark
                   │  └─ Prompt Copilot + inference job queue      (NEW)    │     to workspace)
                   │                                                        │
                   │  Postgres+pgvector (events, projections, index)        │
                   │  Redis (fan-out)                                       │
                   │  MinIO (git CAS, media, model weights)                 │
                   └────────────────────────────────────────────────────────┘
```

Everything reaches the server over the owner's VPN (D11). Silent Mesh services are
**additive crates and middleware** (D35): `sm-work`, `sm-gateway`, `sm-seals` hook
the relay's ingest/delivery/policy extension points rather than rewriting them.

## 5. Protocol: events and kinds

Inherited wire protocol: NIP-01 events, NIP-42 WS auth, NIP-98 HTTP auth, Buzz's
kind registry (kind 9 streams, NIP-34 git kinds, 45xxx forum, 46xxx workflow/
approval, kind 40100 canvases, Blossom media). Standard Nostr tooling connects,
subject to membership.

Silent Mesh adds its **47000–47999 block**: work-thread lifecycle (incl.
settled/snoozed/archived), thread fork, thread promotion, turn/activity, proposed
plan, checkpoint ref, artifact published, anchored comment, inference job
queued/dispatched/completed, thread canonicalized, **thread recommendation**
(agent-proposed open / metadata edit / done — inert until a human confirms),
content seal, canon updated, knowledge dispute raised, knowledge decision, sync
marker, ACL change notice, usage summary. Task metadata (goal, deadline, DRI,
overdue notice) and state transitions (D41) ride the thread-lifecycle kinds. Agent token-streaming uses ephemeral kinds (20000–29999); final messages
persist as one signed event. Approval kinds 46010/46011 are inherited names that
we actually wire (§8).

Offline writes remain a first-class property: client-signed events authored
offline are valid on replay — the Swift client's outbox is "signed events + local
git commits".

## 6. Team layer: identity, roles, ACLs

Inherited: keypair identity, invites (wiring the currently-deferred side-effect
handler if upstream hasn't), channel membership and role enforcement (Buzz's
per-channel roles, remapped per D42), TOCTOU-safe membership checks on REQ and
delivery, last-owner guards, moderation, audit chain, community isolation.

Silent Mesh additions:

- **Role hierarchy (D42)**: five levels. The **Owner** (one per workspace) can
  do everything, and holds all privacy-weakening operations exclusively
  (tier-loosening clones, seals, deep seals). **Workspace Admins** handle
  members & invites, team-channel creation (choosing the immutable tier within
  owner-set bounds) and Channel-Admin appointment, agents & budgets, and
  public-publishing approval. **Channel Admins** manage everything inside
  their channel — membership, terminal thread transitions (D41), dispute
  decisions (§12). **Members** belong to channels and work in them. **Public**
  is not a login: it is the published static wiki (D43). Guest is dropped
  (Buzz's Guest role is disabled); Bot remains the agent role. Workspace-level
  roles are an additive layer over Buzz's per-channel roles (buzz-admin /
  moderation_authz are the extension points); Buzz's per-channel Owner/Admin
  map to our Channel Admin.
- **Channel = folder = repo (D5/D6)**: channel creation provisions and binds a
  repo on the forge; the binding is the unit of sync and ACL.
- **Folder/file write ACLs (D4)**: the pre-receive policy hook gains file-path
  rules (member → paths → read-write | read-only) alongside upstream's
  ref-pattern protections; the same rules bind the agent tool layer. The honest
  v1 read boundary is unchanged: read = channel membership; need read-secrecy →
  separate channel.
- **Personal channels (D29)** and **promotion**, **Privacy Gate (D30)**,
  **Content Seals (D31/D32)** — semantics exactly as decided (see the D-log);
  seals are enforced at four points (render, inference, movement, sync) with
  workspace-wide sweep, ingestion guards, delivery redaction envelopes for signed
  events, and server-side-only literal matching. Deep seal purges history with an
  old→new SHA remap table and forced client re-sync.
- **Agents as members**: inherited outright — bots have keypairs, roles, channel
  memberships, sign their commits, and are auditable. Per-user operated-for
  tagging and per-user vendor credentials (D23) are the additions.

## 7. Work threads: T3's model on Buzz's forge

The `sm-work` layer gives Buzz what T3 had and Buzz lacks:

- A work thread binds a **worktree** on the channel's repo; every agent turn ends
  in a **checkpoint** (hidden ref); diffs/revert per turn and per thread.
- **Fork (D27)**: any member, at head or any checkpoint → sibling variation,
  conversation inherited by reference, merge-back via normal git flow.
- **Lifecycle (D28)**: settled / snoozed / archived as signed events; losing
  variations archive via the winner's close flow (D41).
- **Promotion (D29)** out of personal channels, through the Privacy Gate (D30).
- **Task framing (D40)**: a thread is launched as a task — a **goal** (the
  brief), an optional **deadline**, and a **DRI** (human or agent member)
  responsible for driving it to done. Overdue deadlines emit a notice into the
  channel stream tagging the DRI. An agent DRI drives the work itself; a human
  DRI drives the agents.
- **Thread states & authority (D41)**: the lifecycle is a small, explicit
  machine —

  ```
  open ⇄ snoozed
  open → ready      (done proposed — by the DRI, any member, or an agent
                     recommendation)
  ready → closed    (human decision; Channel Admin in their channel, Owner
                     anywhere. Choice at close: canonicalize or not)
  open/ready → archived   (abandonment path, same authority as close)
  closed → archived (automatic storage state after close)
  archived → open   (owner reopen, audit-logged; existing canon stays —
                     provenance is history, not state)
  ```

  `open` carries computed sub-signals (working / awaiting-approval / settled)
  rather than extra states. Authority follows one principle: **agents
  recommend, humans decide, and terminal transitions have the narrowest
  authority.**

  | Action | Agent | Member | Channel Admin | Owner |
  |---|---|---|---|---|
  | Open thread | recommend | ✓ | ✓ | ✓ |
  | Edit goal / deadline / DRI | recommend | ✓ | ✓ | ✓ |
  | Snooze / unsnooze | recommend | ✓ | ✓ | ✓ |
  | Propose done (`ready`) | recommend | ✓ | ✓ | ✓ |
  | Close (± canonicalize) | ✗ | ✗ | ✓ (their channel) | ✓ |
  | Archive / reopen | ✗ | ✗ | ✓ (their channel) | ✓ |

  Agent recommendations are first-class events a human confirms with one
  action; unconfirmed recommendations change nothing. Close authority is a
  **policy knob** shipped at Channel-Admin + Owner (D42) — loosening it later
  (to DRIs) is a config change, not a redesign. Two edge cases handled: in
  **personal channels** the member is implicitly the Channel Admin (the owner
  is not a member and cannot see them); and **losing fork variations** are
  archived through the close flow on the winning variation — closing the
  winner offers sibling archiving as a batch, keeping D28's "losers archive"
  inside human authority.
- **Canonicalization (D38/D40)**: every channel repo carries a **canonical docs
  layer** — a conventional `canon/` area on the main branch holding the current
  truth about the channel's topic. Thread work lives on the thread's branch;
  closing a thread **with canonicalization** merges its output into the
  canonical layer (the same merge-back flow, now with organizational meaning)
  and `sm-knowledge` distills the canonical doc updates with provenance links
  back to the thread; closing without it just archives. Chat and canon live in
  one place: joining a channel gives you the conversation *and* the
  accumulated truth of its topic.
- Buzz's existing agent workspace convention (OUTBOX/, REPOS/) is aligned with
  worktree binding so buzz-acp agents operate inside the thread's worktree.

## 8. Governance: wiring approvals end-to-end

The comparative analysis found Buzz's single biggest gap, and it is the center of
Silent Mesh's thesis, so it is the **first build** (roadmap Phase 1):

- Today upstream **auto-approves** agent permission requests (buzz-acp selects
  `allow_once` programmatically) and the workflow approval gate returns a token
  that is never persisted or emitted. DB CRUD for approvals exists; HTTP
  grant/deny routes and kind-46010 emission do not.
- Silent Mesh wires the full loop: ACP permission request → **runtime mode
  policy** (T3's full-access vs supervised, per thread) → pending-approval record
  → kind-46010 event to the channel/thread → human grants or denies (signed
  kind-46011, "who approved what" provable) → decision returned to the agent →
  audit chain.
- Grant/deny surfaces: buzz-cli (interim, D36), then the Swift client.
- This work is deliberately structured for **upstreaming** (D35): Block's own
  architecture docs claim the endpoints should exist, so the contribution has a
  natural home.

## 9. Files, git, and sync

- Forge storage, smart-HTTP transport, policy hook, NIP-34 review kinds:
  inherited. Push events already land in channels.
- **Client sync = git** (D9): the Swift client clones/pulls the member's channel
  repos into the encrypted vault via the inherited endpoints (NIP-98-authed),
  pushes local commits when online; conflicts are ordinary merges surfaced in UI.
- Large media and model weights ride Blossom/MinIO, referenced by hash.

## 10. Privacy posture

Unchanged from the D-decisions, restated on the new base: no third-party service
in the loop (Keycloak disabled; no telemetry we ship). Buzz's server-readable
design is **not a conflict for Silent Mesh** — the owner owns the relay; our
privacy model defends against third parties, looser inference backends, and lost
devices, via: privacy tiers with channel-immutable minimums (D21/D24/D26), the
Privacy Gate (D30), Content Seals + deep seal (D31/D32), the encrypted
biometric-gated client vault (D13), and VPN-only reachability (D11). The two
controlled egresses (member vendor subscriptions; the attested TEE provider) and
the `owned-only` zero-egress guarantee are exactly as decided.

## 11. Model plane: tiers, gateway, copilot, native harness

Semantics exactly as D16–D25; the base flip changes the substrate:

- **sm-gateway** fronts: server-local models (2× RTX 4060 8 GB — llama.cpp/vLLM/
  Ollama, spike; Buzz already has Ollama plumbing in buzz-agent), the TEE
  provider (attestation-then-send), and per-user vendor subscriptions (buzz-acp
  already spawns those CLIs; we add per-user credential isolation and
  operated-for attribution).
- **Metering** extends buzz-acp's existing usage accounting into per-request
  `(user, agent, channel, thread, model, tier, backend)` attribution with
  owner-set budgets.
- **Native harness (D17)** = buzz-agent + per-user profiles (persona, tier/model
  route, tool allowlist, limits, owner-bounded) + gateway-only model access.
  Buzz's agent catalog and persona system give this a head start.
- **Prompt Copilot (D25)** and the signed inference-job queue as decided; gate
  and copilot inference pinned to the owned tier in the router.

## 12. Knowledge plane: index, wiki, reconciliation, disputes

The `sm-knowledge` service (D37–D39) is the workspace's continuously-running
organizational memory:

- **Continuous indexing**: event-driven — every new message, thread turn,
  artifact, and git push triggers incremental indexing. Full-text search
  inherits `buzz-search` (Postgres FTS); semantic search adds **pgvector**
  embeddings computed **only on the server GPUs** — routing embedding through a
  remote backend would be wholesale content egress, so the tier router forbids
  it categorically, like gate/copilot inference.
- **ACL-scoped retrieval**: every query — from a human's search box, the Prompt
  Copilot's context gathering, or a harness agent's retrieval tool — is scoped
  server-side to channels the requester can read. Sealed content is indexed
  only in token form (post-sweep content is tokenized anyway), so the index
  never holds raw sealed values. Results carry provenance (channel, thread,
  file, commit).
- **Canon, not a wiki place (D38)**: there are no dedicated knowledge channels —
  **every channel is a knowledge channel**, carrying its own canonical docs
  (`canon/`, §7) as the current truth about its topic. Canon inherits the
  channel's membership and tier *by construction* — there is nothing to gate
  when a channel's own thread canonicalizes into its own docs. The
  **workspace wiki is a view**: wiki-style navigation and search assembled
  per-viewer across every canon they can read, with provenance on every
  statement. Distilled summary pages the service itself owns are maintained
  automatically; everything else is proposals.
- **Continuous reconciliation**: the service watches every channel. Inside a
  channel, canonicalized thread output updates that channel's canon (§7).
  **Across channels**, when new canonical information in channel X affects
  what channel Y's canon says, the service opens an **update proposal** in Y —
  an ordinary work thread with a diff — never a silent edit. Cross-channel
  proposals carry only content Y's members may see: if X is stricter than Y,
  the proposed content passes the **Privacy Gate** with seals enforced as
  always.
- **Disputes (D39/D42)**: when canons or sources disagree about a data point
  (A vs B) — the extractor detects a contradiction, or a human/agent flags
  one — the service raises a **knowledge dispute** event showing both claims
  with provenance. It escalates to the **Channel Admin(s)** of the affected
  channel; disputes spanning channels go to a **Workspace Admin or the
  Owner** — in every case the decider must have read access to all sources
  involved. The decision is a **signed event**; the service then propagates it
  throughout — the winning canon records the value (with the dispute +
  decision linked for the record), and correction proposals open in every
  channel still carrying the losing value. Dispute → decision → propagation is
  a fully auditable chain.
- **Public wiki (D43)**: publishing selected canon pages is an explicit,
  approval-gated act — a Workspace Admin or the Owner approves, the content
  passes the **Privacy Gate + seals at maximum strictness**, and `sm-publish`
  exports it as a **static site** (own public search index) pushed to a
  separate internet-facing host: the company's public documentation, website,
  datasheets. The workspace server never faces the internet (D11); the public
  host holds only already-published content, so compromising it reveals
  nothing that wasn't already public. Honest caveat: publication is
  practically irrevocable (caches, archives) — which is exactly why it is the
  most-gated action in the system.

## 13. Swift macOS client (and the iOS path)

Unchanged in design (macOS 14+, D19): `MeshProtocol` (NIP-01/10/42/98 — now
testable against Buzz's conformance suite and interop E2E, a real gift),
`MeshVault` (SE-wrapped master key, biometric/PIN unlock, encrypted store),
`MeshSync` (libgit2 against the inherited forge endpoints + signed-event outbox),
`MeshIntelligence` (whisper.cpp + llama.cpp/MLX; client-side copilot; offline
Privacy Gate). UI as decided: channels/streams/threads with fork/archive/promote, task
metadata (goal, deadline, DRI) with a task-board view over threads, per-channel
canon browsing, approvals, file browser, artifact viewer with seal-aware
rendering and durable comments, gate review flow, workspace search + the
cross-canon wiki view (§12), and role-scoped admin surfaces (D42): members &
invites, channels & channel admins, agents & budgets, usage dashboards,
publish-to-public approval, and the owner's seal registry.

Until it ships: **buzz-cli and scripted clients are the only supported surface**
(D36).

## 14. Fork discipline (D35)

- Silent Mesh changes live in **additive crates** (`sm-work`, `sm-gateway`,
  `sm-seals`) and registered extension points; invasive relay edits only when
  unavoidable, kept small and rebase-friendly.
- **No mass renames**: crate and binary names stay upstream; branding happens at
  the config/deploy/client layer.
- Upstream-owned trees we don't ship stay in-tree, unbuilt.
- Scheduled upstream rebases (e.g. biweekly); the conformance + E2E harness is
  the rebase safety net.
- Governance/approvals work is written to upstream-quality and offered to Block.
