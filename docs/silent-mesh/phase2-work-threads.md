# Phase 2 Design Note — Work threads: roles, tiers, bindings, lifecycle

Implements roadmap Phase 2 (architecture §7) on the foundations Phase 1
landed. Written 2026-07-29 against this fork with the Phase 1 series merged.
Scope of the first implementation slices (2a/2b); worktree/checkpoint
mechanics, forking, promotion, and file ACLs follow in later slices.

## 1. Slices

- **2a-roles** — the D42 workspace layer: **reuses `relay_members`**
  (role `owner` = the workspace Owner, role `admin` = Workspace Admins,
  managed by the existing NIP-43 9030-series commands with owner-gated
  admin grants — no new table, no new kind), enforcement for team-channel
  creation and Channel-Admin appointment, Guest disabled.
- **2a-tiers** — immutable channel tier (D24/D26) declared at creation
  (`owned` | `private` | `open`), stored on the channel row, exposed in
  kind:39000 metadata, mutation rejected everywhere; git repo provisioned
  and bound at channel creation (D5/D6) with the binding recorded.
- **2b-threads** — the 47xxx work-thread kinds and the D41 state machine
  with authority enforcement: agents recommend, humans decide, terminal
  transitions have the narrowest authority.

## 2. Kind allocations (47000–47999)

Upstream reserves 47000–47999 for "User groups" but has no code in the
range; Silent Mesh takes the low block and leaves 47500+ untouched as a
courtesy split in case upstream lands something.

| Kind | Name | Shape | Purpose |
|---|---|---|---|
| 47000 | `KIND_WORK_THREAD_OPEN` | regular (append-only) | Thread root: launches a thread **as a task** — content = goal; tags `h` (channel), optional `deadline` (unix secs), optional `dri` (pubkey hex). The root event id is the thread id. |
| 47001 | `KIND_WORK_THREAD_METADATA` | command | Task metadata edit (goal / deadline / DRI) — any full member; agents only recommend. Tags `e` (thread root), `h`; content JSON with the changed fields. Relay validates authority and updates the projection. |
| 47002 | `KIND_WORK_THREAD_STATE` | command | State transition per §3 — relay validates the transition *and* the author's authority, updates the projection, rejects otherwise. Tags `e` (root), `h`, `state` (target), optional `canonicalize` flag on close. |
| 47003 | `KIND_WORK_THREAD_RECOMMEND` | regular (append-only) | Agent recommendation (open / metadata / done) — **inert until a human confirms** by sending the real 47001/47002 referencing it via an `e` tag. Never touches the projection. |
| 47010 | `KIND_WORK_THREAD_CHECKPOINT` | regular (append-only) | Per-turn checkpoint ref: tags `e` (root), `h`, `commit` (40/64-hex git id), optional `branch` + `turn`. Any channel participant (bots included); the thread must exist in the channel. |
| 47011 | `KIND_WORK_THREAD_OVERDUE` | **relay-only** | Overdue notice from the leader-elected deadline sweep — tags the DRI (`p`; fallback: opener), `e` (root), `h`. At-most-once per deadline (`overdue_notified_at` claim); a 47001 deadline edit re-arms it. Client submissions rejected. |
| 47012 | `KIND_WORK_THREAD_CANON` | **relay-only** | Canonicalization outcome from the close-with-canonicalize job — tags `e` (root), `h`, optional `commit` (new main tip); content JSON `{outcome, prefix}`. The job grafts the thread's latest 47010 checkpoint under `canon/<thread-short>/` on the default branch via hydrate → plumbing → pointer-CAS publish (bounded rehydrate-retry on conflict; `canonicalized_at` claim is once-only, re-armed by re-close; leader sweep recovers crashed jobs). Never fails the close. |
| 47013 | `KIND_WORK_THREAD_SIBLING_ARCHIVED` | **relay-only** | Sibling-archive notice — one per losing fork archived by a winner's close-with-`archive-siblings` (D28). Tags `e` (the archived thread's root), `h`, `winner` (winner root hex); content JSON `{winner}`. Lets event-folding clients see the batch archive; the projection batch is the authority. |
| 47014 | `KIND_WORK_THREAD_PROMOTED` | **relay-only** | Promotion notice in the **source** personal channel (D29) — records the relay-side close when a thread promotes. Tags `e` (source root), `h` (source channel), `to` (target channel UUID), `thread` (new target root hex); content JSON `{to, thread}`. |
| 47020 | `KIND_WORK_THREAD_FORK` | regular (append-only) | Thread fork (D27): the fork event **is the new thread's root** (its id = new thread id) — shipped as a root-like regular event, not a command, since the new root must be client-signed anyway. Content = the variation's goal; tags: exactly one **unmarked** `e` (parent root — `["e", <id>]` only, so a fork can't double-register as a NIP-10 reply), `h` (same channel), optional `commit` (fork point — must be a kind:47010 checkpoint the parent recorded; the check pages through the full checkpoint history; absent = head), optional `deadline`/`dri` as on 47000. All hex tag values lowercase (one canonical case for byte-exact `#e`/`commit` matching; 47010's `e`/`commit` tags share the rule). Any full member; parent forkable from any state. Conversation inherited by reference; projection row carries `forked_from`/`fork_commit`. |
| 47021 | `KIND_WORK_THREAD_PROMOTE` | command | Promotion out of a personal channel (D29/D30): stored in the **target** channel and **is the new thread's root** (its id = new thread id; goal = the member-written summary). Tags: **strict allowlist** — `h` (target), exactly one unmarked lowercase `e` (source root), `from` (source channel UUID), optional lowercase `commit` (a recorded source checkpoint; absent = latest); any other tag is rejected (extra tags would be an unscanned channel across the privacy boundary). Authority: author owns the source personal channel + full member of the live (non-archived) target — never a DM or another personal channel. Source may be open/snoozed/ready **or closed** (crash-retry convergence; re-promotion into another team channel is legitimate and each is gated + audited); only archived sources are refused. The Privacy Gate scaffold and the git graft run **before** the event persists — a refused promotion stores and transfers nothing. Files graft under `promoted/<new-short>/` on the target default branch (object transfer via a local push into a temp ref, deleted before CAS publish). **The graft commit parents only the target tip** — a parent edge to the source checkpoint would publish the personal repo's entire unscanned history (deleted secrets, commit messages, author identities); provenance lives in this event's tags, not the git graph. Source thread closes (the kind:47014 notice is emitted only when the close actually happened); conversation stays behind. |

All are channel-scoped (`h` tag) and ride the existing membership-checked
delivery. Threads thread: replies to the root use the ordinary kind-9 +
NIP-10 path, so `thread_metadata` counters and the 39005 summary overlay
work unchanged.

## 3. D41 state machine (as enforced relay-side)

```
open ⇄ snoozed            any full member
open → ready              any full member (done proposed; agents recommend)
ready → open              any full member (withdraw the done proposal)
ready → closed            Channel Admin (their channel) / workspace Owner
open|ready → archived     Channel Admin / Owner   (abandonment)
closed → archived         Channel Admin / Owner   (storage state)
archived → open           Owner only              (reopen, audit-logged)
```

Authority mapping: "Channel Admin" = Buzz per-channel role `owner` or
`admin`; "workspace Owner" = `relay_members` role `owner` (there is no
`communities.owner` column — the relay-membership roster **is** the
workspace layer). Workspace Admins (`relay_members` role `admin`) per D42
do **not** get terminal thread transitions; in a thread they carry only
whatever channel role they hold. Bots never transition — they emit 47003
recommendations. `ready → open` is the one addition to the D41 diagram:
without it a mistaken done-proposal could only be closed or archived; it
is non-terminal, so it takes member authority (policy-knob compatible).
A replayed (already-stored) 47001/47002 command short-circuits as an
idempotent duplicate *before* authority validation — otherwise a replayed
close would read as an illegal `closed → closed` transition.

**Sibling archiving (2f, D28)** is a second path into `archived` that is
*not* a client 47002 transition: closing a winner with the
`archive-siblings` tag archives every other thread in its fork family
(walk `forked_from` up to the original root, then the whole subtree) from
`open|snoozed|ready|closed`. The winner's TOCTOU close and the batch run
in **one DB transaction**, serialized per family by a Postgres advisory
transaction lock keyed on `(community, family root)` — so the close and
the batch commit or roll back together, and two concurrent family closes
cannot mutually archive each other's winner: the second serializes behind
the first, finds its own winner already archived, and loses its status
guard cleanly (exactly one winner survives). `snoozed` is deliberately
included even though the client matrix has no `snoozed → archived` edge —
the batch runs under the closer's admin authority as part of the close
flow, and a parked loser still loses. A sibling that is closed with a
**pending canonicalization** (flag set, unclaimed) is skipped — archiving
it would silently cancel an admin-authorized canon/ merge (claim and
recovery sweep both require `status = 'closed'`); it becomes archivable
once its kind:47012 outcome lands. Each archived sibling gets a
relay-signed kind:47013 notice, emitted best-effort after the commit; a
crash in between leaves the projection correct and the fold view stale
(the projection is authoritative). The batch never leaves the winner's
channel.

## 4. Storage

Migration 0027:

- `channels.tier` — `channel_tier` enum (`owned`,`private`,`open`),
  NOT NULL DEFAULT `open` (backfills existing rows), **immutable**: no
  update path is added anywhere; the settings handler explicitly rejects
  tier tags.
- `channel_repos` — `(community_id, channel_id)` PK → `repo_name` (the
  forge name bound at creation, unique per community), `owner_pubkey`
  (announcement author, for audit), `created_at`. FK to channels,
  tenant-led.
- *(No `workspace_roles` table.)* The workspace layer reuses
  `relay_members` — the Owner is the `role = 'owner'` row minted at
  community creation; Workspace Admins are `role = 'admin'` rows managed
  by the NIP-43 9030-series commands.
- `work_threads` — `(community_id, thread_id)` PK (thread_id = root event
  id bytes) → channel_id (FK), goal TEXT, deadline TIMESTAMPTZ NULL,
  dri_pubkey BYTEA NULL, status `work_thread_status` enum
  (`open`,`snoozed`,`ready`,`closed`,`archived`), canonicalize_on_close
  BOOL, created_by BYTEA, created_at, updated_at, closed_at NULL.
  Projection only — the signed events are the truth; the row makes
  list/read cheap and transitions TOCTOU-safe
  (`UPDATE … WHERE status = $expected`).

Later additive migrations on `work_threads`: 0028 `overdue_notified_at`
(2d), 0029 `canonicalized_at` + `canonicalize_outcome` (2e), 0030
`forked_from` + `fork_commit` with a partial index on
`(community_id, forked_from)` for the sibling-family walk (2f).

**Personal channels (2g, D29)** live in the additive `personal_channels`
registry (migration 0031): `(community_id, owner_pubkey)` PK — one per
member — with a channel-side UNIQUE for the reverse lookup; rows are
written in the same transaction as the channel (`create_personal_channel`,
which forces visibility `private` and bootstraps the member as channel
owner = implicit Channel Admin). A `personal` tag on kind:9007 takes this
path — member-creatable even under `BUZZ_WORKSPACE_CHANNEL_GATE`.
Enforcement in depth: membership is restricted to the member plus bots
they own (`users.agent_owner_pubkey`), checked pre-storage in the 9000
validator and at the `add_member` choke point (covers invites, templates,
and workspace authority — even workspace admins cannot join); kind:9002
visibility edits are rejected on personal channels (privacy is permanent,
which also keeps kind:9021 self-joins closed); the Privacy-Gated
promotion path (47021) is the only sanctioned way work leaves.

**The Privacy Gate scaffold (2g, D30)** is `buzz_core::secret_scan`:
deterministic, dependency-free prefix/shape rules (AWS/GitHub/Slack/
Stripe/Google/Anthropic/OpenAI/npm token shapes, PEM private-key blocks,
JWTs) over the member-written summary and every text blob in the promoted
checkpoint tree. Findings carry rule name + path only — never the matched
value. Binary blobs (NUL sniff; oversized blobs are head-sniffed with a
bounded read) are skipped by design; text blobs over 5 MiB are refused as
unscannable (fail closed). The gate's blast radius is exactly the
promoted snapshot: the graft never links source history into the target
(no checkpoint parent edge), so ancestor commits the gate never saw can
never be fetched from the target repo. Model-assisted review arrives in
Phase 3; the scanners stay as the hard backstop underneath.

Registry hygiene: soft-deleting a personal channel frees the member's
slot — the stale registry row stops answering lookups and is reclaimed
on their next `personal` create.

## 5. Authority enforcement points (2a)

- **Team-channel creation** (kind 9007): behind `BUZZ_WORKSPACE_CHANNEL_GATE`
  (default **off**, preserving upstream behavior); when on, non-DM channel
  creation requires `relay_members` role `owner` or `admin`. DM channels
  (member-pair) stay member-creatable. The tier tag is parsed here; a repo
  is provisioned and bound in the same flow (relay-owned, relay-signed
  kind:30617 announcement).
- **Channel-Admin appointment** (member role changes to owner/admin):
  workspace authority (`relay_members` owner/admin) substitutes for the
  usual inviter-elevation requirement; an existing channel owner keeps
  upstream semantics inside the channel.
- **Guest role**: add_member with role `guest` is rejected at the relay
  (pre-storage validator + DB layer), and migration 0027 retires legacy
  guest rows to `member`.
- **workspace grant/revoke**: **no new kind** — the existing NIP-43
  9030-series relay-admin commands manage the roster; admin grants are
  already owner-gated there.

## 6. Deviations / deferred

- Repo provisioning reuses the forge's existing repo-creation seam (see
  the git API); if the forge is init-on-first-push, the binding row plus a
  server-side init is used instead — resolved during implementation
  against the mapped seam.
- Everything in the Phase 2 scope list has shipped (2a–2h). The agent
  tool layer's half of D4 (binding the same path rules inside agent file
  tools) rides with the buzz-acp worktree work rather than this slice.

## 7. Folder/file write ACLs (2h, D4)

`buzz-protect` tags say *which refs* a role may update; `buzz-path-acl`
tags add the orthogonal axis — *which paths inside the tree* a member may
write. Both ride the same kind:30617 repo announcement, so a channel's
repo carries its own ACLs.

```text
["buzz-path-acl", "<path-pattern>", "write:<role>" | "write:<64-hex>" | "readonly", ...]
```

Patterns are the ref-pattern grammar without the `refs/` requirement
(literal segments, `*` = one segment, trailing `**` = one or more);
absolute paths and `..` are rejected. Matching is **most-specific-wins**
(most literal segments); equally-specific rules union, so two rules can
never silently disagree. `readonly` closes a path to every pusher
including the owner — that is how the relay-owned `canon/` and
`promoted/` layers are protected from hand-edits while the relay's own
grafts (which bypass the hook) keep writing them. Repos with no
`buzz-path-acl` tags behave exactly as upstream.

Enforcement needs the changed paths, which the hook did not previously
compute. The pre-receive script now collects them per ref update — `git
diff --name-only old new` for updates, `git rev-list new --not --all` +
`diff-tree` for creates (so a new branch reports only its own commits),
nothing for deletes — dedupes, and sends them **inside the HMAC**
(length-prefixed, matching `compute_hmac`), so a compromised hook cannot
strip paths to dodge ACLs. A bash/Rust parity test pins that agreement.
When a repo has ACLs and the path list is unusable — git failed
(`!PATHS_UNAVAILABLE`), the push exceeds the 5000-path cap
(`!PATHS_TRUNCATED`), or an older hook sent no list at all — the endpoint
**fails closed** rather than guessing.

## 8. Agent worktree engine + auto-checkpoints (buzz-acp)

The final Phase 2 slice closes the loop between an agent's turn and the
thread's git trail: a work-thread agent turn runs in a **per-thread git
worktree** of the channel's bound repo, and the harness commits + pushes +
emits a kind:47010 checkpoint at turn end (replacing the manual
`buzz threads checkpoint`).

**What shipped this slice — the engine.** `crates/buzz-acp/src/worktree.rs`
(`pub mod`) is the self-contained, tested git engine:

- **`ThreadWorktrees`**: clones the channel repo once under
  `<root>/<channel>/repo.git` (the clone is run **with** the NIP-98 auth
  `-c` flags so it authenticates against the relay's gated forge, not a
  plain clone that would 401), and `git worktree add`s a per-thread branch
  `sm/thread/<root-short>` at `<root>/<channel>/wt/<short>`. Idempotent:
  a present `.git` means the authenticated clone succeeded; `worktree
  prune` clears stale registrations so an externally-deleted worktree
  re-provisions; an empty channel repo (unborn HEAD) is a clean soft error,
  not a broken worktree.
- **`checkpoint`**: `git add -A`; a clean tree returns `NothingToCommit`;
  otherwise commit (signing/hooks pinned off; identity per-invocation),
  push the thread branch, return the pushed oid. A push failure is an
  `Err` so the caller suppresses the 47010 (a checkpoint must name a
  fetchable commit).
- **`push_auth_config`**: the `git-credential-nostr` NIP-98 scheme
  (`credential.useHttpPath=true`, a **0600-on-create** keyfile holding the
  agent secret, `nostr.authtag` for the NIP-OA owner attestation) — the
  same scheme `buzz-dev-mcp`'s shim builds, scoped to one repo. Pushes land
  on `sm/thread/*`, clear of the `refs/heads/main` protection rule the
  channel repo's 30617 declares.
- **Hardening**: every git subprocess has a 120 s wall-clock timeout
  (`kill_on_drop`), and the environment neutralizes both system
  (`GIT_CONFIG_NOSYSTEM`) and user (`GIT_CONFIG_GLOBAL=/dev/null`) config
  so a host `commit.gpgsign`/`core.hooksPath`/`credential.helper` can't
  silently break checkpoints. All best-effort, mirroring
  canonicalize-on-close: any git failure logs and skips.

The pure helpers (`thread_branch`, `thread_scope_id`, `forge_http_base`/
`url_host`, `repo_binding_from_events`/`event_kind_by_id`/
`is_work_thread_root_kind`, `push_auth_config`) have unit tests; the git
lifecycle (authenticated provision → worktree add → clean-skip →
dirty-commit-and-push → idempotent re-entry → re-provision after external
deletion → empty-repo soft-fail) is exercised end to end against a **local
bare remote** by the gated
`worktree::probe_tests::provision_commit_push_roundtrip`
(`BUZZ_ACP_WORKTREE_PROBE=1 cargo test -p buzz-acp --lib worktree::probe_tests -- --ignored`).

**The harness wiring shipped 2026-08-01** (`54329ad6`..`66fa1a03`). A turn
whose NIP-10 root is a kind:47000/47020 thread runs in a dedicated worktree
of the channel's bound repo and, on completion, commits, pushes and
publishes a kind:47010 — opt-in via `BUZZ_ACP_WORKTREE_ROOT`. Both defects
the review named below were fixed first as their own commits: the
session-model refactor (`24548db3`, `be8982c1`) and the relay-owner check
on the kind:30617 binding (`faaccef7`, `54329ad6`, resolving the relay
pubkey from NIP-11's `self` field — not `pubkey`, which is the operator's
contact key and is unset on our relays).

Validated live rather than in the sandbox, which is what surfaced three
silent environmental blockers — git 2.46+ for the credential helper, a
policy callback that cannot reach loopback on a tailnet-bound relay, and an
npm `wc-cli` shadowing `/usr/bin/wc` in the pre-receive hook. See
HANDOFF.md § Work-thread worktrees — operational preconditions.

The history below is kept because the review's findings are why the wiring
took the shape it did.

**Originally deferred to a dedicated follow-up — the harness wiring.**
Binding the agent's ACP session cwd to the worktree and firing `checkpoint`
at end-of-turn was prototyped and put through the adversarial review, which
surfaced ~20 confirmed defects concentrated in two layers that **cannot be
validated without a live agent + relay**: (a) the pool's session model —
work-thread turns need their own session (own cwd), which collides with
the channel-keyed `invalidate`/rotation/affinity/GC logic and needs a
session-model refactor, not a bolted-on second map; and (b) the resolution
path — trusting any author's kind:30617 for the repo binding is a
redirection hole that needs the relay-owner pubkey resolved first. Rather
than ship confirmed-broken wiring (the prototype's provisioning clone
was even unauthenticated → always 401 live), the wiring is a separate
slice built against a live harness. The tested engine is the durable
substrate it will call.

## 9. Known limitations

Known limitations shared with upstream patterns (recorded 2f review):

- **Projection-creation side effects are post-storage and best-effort**
  (47000 and 47020 alike, the upstream `handle_side_effects` model): if
  the row insert fails after the event stored, the failure is logged and
  the thread is invisible to 47001/47002/47010 (and the family walk)
  until an operator re-inserts the row. No 47xxx-specific regression;
  fixing it means moving projection creation pre-storage, upstream-wide.
- ~~Command kinds bypass the moderation timeout write-block~~ — **fixed**
  after the 2f review: command-executor routing now runs *after* the
  ban/timeout gate, so a restricted actor cannot issue 47001/47002/47021,
  DM, workflow, or approval commands. The two deliberate pre-gate
  exemptions (moderation commands 9040–9044, relay-admin 9030–9033)
  remain — both are the tooling used to lift restrictions, and neither is
  a command kind. A source-order regression test pins the ordering, which
  is otherwise invisible to unit-level calls.
