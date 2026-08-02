# Silent Mesh — Delivery Roadmap

Companion to [architecture.md](./architecture.md), re-cut for the **Buzz base**
(D33). Ordering principle: wire governance first (Buzz's one unfinished area and
Silent Mesh's thesis), then port T3's work-thread model, then the model plane and
seals, then the Swift client — with buzz-cli and scripted clients as the only
interim surface (D36). **D35 was revised to a hard fork on 2026-08-02** — the
upstream-tracking discipline this ordering assumed was never implemented and is
no longer intended; see FORK.md.

Each phase lists scope and an exit criterion — the observable thing that must work
before moving on.

## Phase 0 — Fork setup and slim-down

- Fork block/buzz at v0.5.x; stand up the single-host deploy (compose profile:
  Postgres, Redis, MinIO; Keycloak disabled; VPN-bound listeners) on the Linux
  server with the 2× RTX 4060 GPUs visible to it.
- Build discipline per D35: exclude desktop/mobile/web/admin-web from our CI
  (trees stay in-tree, unbuilt); pin the toolchain; establish the scheduled
  ~~upstream-rebase workflow~~ — dropped with the D35 revision; the fork is
  frozen at `90e058eb` and takes upstream commits only as deliberate
  cherry-picks.
- Community bootstrap: single community, owner enrolled, invites verified (wire
  the deferred invite side-effect handler if upstream hasn't by then).
- Migrate these plan docs into the fork; branding at config level only.
- Baseline agent check: buzz-acp runs claude-code and codex against a test
  channel; Grok CLI validated via the BYOH runtime path.

**Exit**: `just ci` (our slimmed profile) green on the fork; a scripted client
and buzz-cli exchange kind-9 messages in a members-only channel on the deployed
single-host stack; an @mentioned claude-code agent replies. ~~upstream rebase
executed once end-to-end~~ — **struck** with the D35 revision: this criterion
was never met and, under a hard fork, never will be. It is removed rather than
left permanently unmet. (The claude-code clause is also still unconfirmed —
the live turn used in-tree `buzz-agent` against Ollama.)

## Phase 1 — Governance: approvals wired end-to-end

The flagship early build (architecture §8), replacing upstream's auto-approve.

- Runtime-mode policy per thread/agent (full-access vs supervised — T3's model).
- buzz-acp `handle_permission_request` → policy check → pending-approval record
  (DB CRUD exists upstream) → **kind-46010 emission** into the channel/thread.
- Grant/deny: relay HTTP routes + buzz-cli commands; decision returns to the
  waiting agent; signed kind-46011 makes "who approved what" provable; audit
  chain entries throughout; timeouts and deny-by-default on disconnect.
- Structure the patches for upstreaming to Block (their ARCHITECTURE.md already
  claims these endpoints; the contribution has a natural home).

**Exit**: a supervised claude-code agent attempts a shell command; it blocks
until a human grants via buzz-cli; the grant is a signed 46011 event; a second
run in full-access mode proceeds without a gate; a denied request returns a
refusal to the agent; all four paths visible in the audit chain.

## Phase 2 — Work threads: T3's model on Buzz's forge

The `sm-work` crate (architecture §7) plus channel↔repo semantics.

- **Role hierarchy (D42)**: workspace Owner/Admin layer added over Buzz's
  per-channel roles (buzz-admin / moderation_authz extension points); Buzz's
  channel Owner/Admin remapped to Channel Admin; Guest disabled; authority
  enforcement for invites, team-channel creation, and Channel-Admin
  appointment (agents & budgets enforcement follows those features in
  Phase 3).
- Channel = folder = repo (D5/D6): channel creation (Workspace Admin+, D42)
  provisions and binds a forge repo; channel privacy tier declared at
  creation, **immutable** (D24/D26), with the owner-only clone operation (repo
  forked with history, membership copied, fresh stream with provenance link)
  as the only re-tiering path.
- Work threads as 47xxx kinds: thread bound to a worktree on the channel repo;
  per-turn checkpoints (hidden refs); diff/revert; plan-then-execute mode;
  buzz-acp agent workspaces aligned onto thread worktrees.
- **Task framing (D40)**: threads launch with goal, optional deadline, and DRI
  (human or agent); overdue notices tag the DRI in the stream.
- **Thread state machine (D41/D42)**: open ⇄ snoozed → ready → closed →
  archived, with reopen; agent recommendations (open / metadata / done) as
  first-class inert-until-confirmed events; authority enforcement — any human
  opens and edits metadata, **Channel Admins close/archive in their channel,
  the Owner anywhere** (policy knob; members implicitly Channel Admin of
  their personal channels); sibling-fork archiving batched into the winner's
  close flow.
- **Canonicalization (D38/D40)**: the `canon/` convention on every channel
  repo; closing a thread *with canonicalization* merges its output into the
  canonical layer and archives the thread (structural mechanics here;
  intelligent distillation arrives with sm-knowledge in Phase 7).
- **Thread forking (D27)**, **lifecycle incl. archive (D28)**, **personal
  channels + promotion (D29)** with the **Privacy Gate scaffold (D30)** —
  mandatory review/confirm with member-written summary + deterministic secret
  scanners (model assist arrives in Phase 3).
- **Folder/file write ACLs (D4)**: file-path rules in the pre-receive policy
  hook + agent tool layer.

**Exit**: from buzz-cli — a human launches a task thread (`goal: fix the
parser`, deadline, DRI = the claude agent); the agent works in a worktree, a
checkpoint event appears, a supervised tool call is approved, the push event
lands in the stream; the agent posts a done-recommendation, which changes
nothing until the **Channel Admin closes the thread choosing
canonicalization** — output merges into `canon/`, the thread archives. A plain
member's close attempt is refused; that member's metadata edit (new deadline)
succeeds; a Workspace Admin creates a channel and appoints its Channel Admin,
but their seal attempt is refused (owner-only). A second member
forks at an earlier checkpoint and drives a variation; closing the winner
offers sibling archiving. A member promotes (and closes) a personal-channel
thread through the gate scaffold — files and summary arrive, conversation
doesn't. An overdue deadline tags the DRI in the stream. A read-only-ACL
member's push is rejected; a non-member sees nothing.

## Phase 3 — Model plane: tiers, gateway, copilot, harness

`sm-gateway` + extensions to buzz-agent/buzz-acp (architecture §11).

- Tier-aware router (owned / private / open) with channel-minimum enforcement;
  per-request attribution `(user, agent, channel, thread, model, tier, backend)`
  extending buzz-acp's usage metering; owner budgets.
- Local serving on the 2× RTX 4060s (llama.cpp/vLLM/Ollama spike — Buzz's Ollama
  plumbing is the starting point); ~7–14B Q4–Q8 envelope + embeddings + server
  whisper.
- TEE provider spike + attestation-then-send integration.
- Per-user vendor subscriptions (D23): per-member credential isolation for the
  CLIs buzz-acp already spawns; operated-for attribution; self-reported metering.
- **Native harness (D17)**: buzz-agent + per-user profiles (persona, tier/model
  route, tool allowlist, limits; owner-bounded), gateway-only model access.
- **Prompt Copilot + inference queue (D25)** on server GPUs; **Privacy Gate
  assist (D30)** — local summary + redaction suggestions, pinned owned-tier in
  the router.
- **Content Seals (D31)**: registry, workspace sweep, ingestion guards
  (pre-receive + event ingest), delivery redaction envelopes, gateway
  resolution/scrub, mandatory-redaction integration with the gate.
- **Retrieval foundation (D37)**: pgvector + continuous local-embedding
  pipeline (owned-tier only, forbidden from remote routes in the router, like
  gate inference); ACL-scoped search API layered over buzz-search FTS; retrieval
  tools exposed to the copilot and harness agents; sealed content indexed in
  token form only.

**Exit**: in an `owned-only` channel with WAN cut, a member's spoken request is
refined by the copilot, queued, and completed by the harness on a local model
(thread, tools, approval, checkpoint, push all work). WAN restored: the same
request routes to TEE in a `private` channel and to the member's own Claude
subscription in an `open` one; below-tier routes refused. A value sealed at
`private` resolves in the TEE request, arrives scrubbed vendor-bound, renders as
a placeholder in `open`, and a raw-value paste is caught at ingestion. An
agent's retrieval query returns only content from channels its operator can
read. Usage query shows per-user totals by tier and backend.

## Phase 4 — Swift macOS client MVP

Starts once Phase 2's kinds have stabilized against scripted clients
(overlappable with Phase 3). macOS 14+ (D19); vault and app lock are foundations.

- `MeshProtocol` validated against Buzz's conformance suite and interop E2E;
  `MeshVault` (SE-wrapped master key, biometric/PIN unlock; sparse-bundle vs
  file-crypto spike); onboarding via invite + local keypair generation.
- UI: channel sidebar + personal space, streams + work threads, agent
  interaction (streaming, **approvals**, per-turn diffs), fork/archive/promote,
  file browser, markdown/image viewing, owner admin (members, roles, ACLs,
  agent profiles, usage dashboards, seal registry).

**Exit**: a two-person team + one agent runs a real session entirely from the
macOS app, including granting a supervised approval; the app relaunches locked;
the vault is unreadable outside the app.

## Phase 5 — Sync, offline, client local models

- `MeshSync`: libgit2 clone/pull/push of member channel repos into the vault via
  the inherited forge endpoints; background sync; conflicts as ordinary merges
  in UI.
- Offline outbox: signed events + local commits, replayed on reconnect.
- `MeshIntelligence` (D19/D25/D30): whisper.cpp voice-to-text; small local LLM
  for summarize/translate; client-side Prompt Copilot; **offline Privacy Gate**;
  weights fetched from the owner's MinIO, stored in the vault.

**Exit**: cable pulled — browse, edit, transcribe a voice note, summarize a doc,
speak a request the copilot refines into a queued job, run a gate review;
reconnect — commits push, events replay, the job dispatches to the right tier,
a seeded conflict resolves through the UI.

## Phase 6 — Artifact viewer and durable comments

- Sandboxed WKWebView rendering (strict CSP), selection → robust anchors,
  anchored comment events on `(channel, path, blob hash, anchor)` — extending
  Buzz's frame-anchored-comment pattern to HTML artifacts and files.
- Regenerate-with-AI from a selection/comment → work thread → proposal diff.
- **Seal-from-selection (D31)** for the owner; seal-aware rendering everywhere.
- Server-side headless Chromium so agents can verify their own artifacts.

**Exit**: agent produces an HTML report; a member selects a chart, comments,
requests regeneration; a second member sees the anchored comment exactly; the
new version lands with comment history intact. The owner seals a figure at
`private`; an `open`-channel clone renders it as a placeholder and vendor-bound
regeneration requests go out scrubbed.

## Phase 7 — Knowledge plane: wiki, reconciliation, disputes

`sm-knowledge` (architecture §12), building on Phase 3's retrieval foundation
and Phase 2's `canon/` + canonicalization mechanics. Runs after the artifact
phase because reconciliation consumes the markdown + artifact corpus those
phases produce.

- **Intelligent canonicalization**: distillation of completed-thread output
  into the channel's canonical docs with provenance links (upgrading Phase 2's
  structural merge); service-owned summary pages auto-maintained.
- **Cross-canon wiki view**: wiki-style navigation + search assembled
  per-viewer across every canon they can read, in the Swift client and
  buzz-cli.
- **Cross-channel reconciliation watchers**: new canonical information in one
  channel triggers update-proposal work threads in affected channels (never
  silent edits); proposals from stricter sources pass a batched Privacy Gate
  with seals enforced.
- **Dispute lifecycle (D39/D42)**: contradiction detection (extractor +
  human/agent flagging) → dispute event with both claims + provenance →
  escalation to the Channel Admin(s) of the affected channel, or a Workspace
  Admin / the Owner for cross-channel disputes (decider must read all
  sources) → signed decision event → propagation: the winning canon records
  the value, correction proposals open everywhere the losing value appears.
- **Public wiki (D43)**: the sm-publish pipeline — select canon pages →
  Workspace Admin/Owner approval → Privacy Gate + seals at maximum
  strictness → static-site export with its own public search index → push to
  the separate internet-facing host. Publish, update, and unpublish flows in
  the admin surfaces (unpublish removes from the site; the honest caveat that
  the internet may have cached it is stated in the UI).

**Exit**: two channels' canons carry contradictory facts; the service raises a
dispute; the designated steward (who can read both sources) decides B; the
winning canon records B with the dispute and decision linked; a correction
proposal appears in the channel whose canon still says A; the cross-canon wiki
view and search show the reconciled fact with provenance; a non-member of a
private source channel sees neither that canon's contribution nor its
existence. Separately: a completed task thread's output is distilled into its
channel's canon with a provenance link back to the thread. And: an approved
canon page publishes to the public static site with sealed values absent,
while the workspace server remains unreachable from the internet; a
Workspace Admin publishes, a Channel Admin's publish attempt is refused.

## Phase 8 — Hardening and iOS

- Key lifecycle (multi-device transfer, rotation/revocation), backup/restore
  drills, LUKS + deployment guide.
- **Deep seal (D32)**: history purge with SHA remap table, forced client
  re-sync, worktree rebase, blast-radius confirmation, rehearsed drill.
- Read-hiding within a channel via filtered mirror repos, if still wanted.
- Harness customization expansion (skills/plugins) informed by v1 usage; decide
  fate of dormant Buzz features (huddles, canvases, forum, workflows, mesh
  compute) — enable, adopt, or leave dormant.
- Knowledge-plane hardening: reconciliation confidence tuning, dispute-noise
  review, wiki backup/rebuild-from-provenance drill.
- iOS client reusing MeshProtocol/MeshVault/MeshSync/MeshIntelligence.

## Standing risks

| Risk | Mitigation |
|---|---|
| ~~Upstream velocity vs fork divergence~~ — **accepted, not mitigated** (D35 revised 2026-08-02) | The fork is frozen at `90e058eb`. Divergence is now permanent and grows: upstream was 107 commits / 300 files ahead within five days, overlapping 29 of our files. The risk is **taken**, not managed. Residual exposure: upstream fixes, including security fixes, no longer arrive in the ~5k-line files we depend on but do not maintain — a specific fix must be cherry-picked deliberately. Additive crates, no renames and unbuilt-not-deleted trees are retained as hygiene |
| Approval wiring may collide with upstream's own approval plans | engage Block early; our patches structured as the endpoints their docs already describe |
| sqlx runtime-only query validation (~27k lines of DB code) | our new code uses compile-checked queries where feasible; migration lint + integration tests on every rebase |
| Rust learning curve on a 233k-line codebase | Rust-first accepted (D34); T3 remains the design blueprint so the hard part is porting semantics, not inventing them |
| No GUI until Phase 4 (D36, accepted again) | buzz-cli + scripted clients keep every phase demonstrable; in-tree desktop app usable as a developer debugging tool |
| 16 GB total VRAM caps local model quality (~7–14B class) | owned-tier channels knowingly trade capability for privacy; TEE + vendor subscriptions cover the high end |
| TEE inference is a young market | attestation-then-send abstraction; if no provider passes the spike, private-tier complex tasks degrade to server GPUs |
| Per-user vendor OAuth lifecycle on the server | buzz-acp already runs these CLIs in production shape; we add per-member isolation; metering self-reported by design |
| Copilot/gate quality on small models | humans confirm before dispatch; routing and gating are policy code; deterministic scanners back the gate |
| Seal tokens must survive agent edits and merges | plain-text markers robust to diff/merge; pre-receive lint; looser contexts only ever hold placeholders |
| Deep-seal rewrites invalidate clones and SHA references | old→new SHA remap table; forced re-sync with vault purge; rare, owner-only, blast-radius-confirmed |
| Postgres/Redis/MinIO ops vs T3's single process | inherited compose/Helm deploys with healthchecks; one Linux host is the supported profile |
| Reconciliation noise (bad auto-updates, spurious disputes) | service auto-edits only pages it owns; everything else is a proposal; disputes require provenance on both claims; confidence thresholds tuned in Phase 8 |
| Public publication is practically irrevocable (caches, archives) | it is the most-gated action in the system: explicit selection + Workspace Admin/Owner approval + gate and seals at maximum strictness + a separate host holding only published content |
| The index as an ACL side-channel | retrieval scoping enforced server-side per query; embeddings owned-tier only; sealed content indexed as tokens; wiki inherits channel membership/tier like any content |

## Open questions (deliberately deferred)

1. TEE inference provider choice — Phase 3 spike.
2. Local serving stack (llama.cpp / vLLM / Ollama) and model lineup for 2× 8 GB —
   Phase 3 spike (start from Buzz's Ollama plumbing).
3. Copilot model choice and how much routing intelligence is model vs policy —
   Phase 3/5 spikes.
4. Inference-queue semantics: retention and cancellation UX.
5. Metering policy detail: budgets vs reporting-only; what non-owners see.
6. Harness customization surface for v1 (persona + route + tool allowlist
   baseline).
7. Vault implementation (sparse bundle vs file-level crypto) — Phase 4 spike.
8. Client local-model lineup — Phase 5 quality spike.
9. Privacy Gate obfuscation mechanics (placeholders vs generalized rewrites;
   merge-back behavior of redacted copies).
10. Deep-seal mechanics detail (rewrite tooling; remap-table retention;
    redaction envelopes for events quoting leaked values).
11. Which dormant Buzz features to eventually adopt (huddles, canvases, forum,
    workflows, mesh compute) — Phase 8 review.
12. Relationship with Block — **no longer a cadence question** after the D35
    revision. There is nothing to keep in sync, so the only remaining question
    is whether to offer the governance work to Block at all, once, as
    cherry-picks onto a fresh clone of upstream. Worth deciding on its own
    merits rather than as a by-product of merging.
13. Knowledge-plane detail: embedding model + chunking strategy (Phase 3 spike,
    alongside the serving-stack spike), claim-extraction approach for dispute
    detection, `canon/` structure and ownership conventions (service-owned vs
    human-owned pages). (Canonicalization trigger settled by D41; dispute
    routing settled by D42.)
14. Close-authority loosening path (D41/D42 policy knob): when and whether to
    extend close/archive from Channel Admins + Owner to DRIs.
15. Public host choice and publish mechanics (D43): static host (own VPS vs
    Pages-style vs CDN), domain, deploy transport from the VPN'd server to the
    public host, and publish cadence.
