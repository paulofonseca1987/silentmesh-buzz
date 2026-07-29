# Phase 2 Design Note — Work threads: roles, tiers, bindings, lifecycle

Implements roadmap Phase 2 (architecture §7) on the foundations Phase 1
landed. Written 2026-07-29 against this fork with the Phase 1 series merged.
Scope of the first implementation slices (2a/2b); worktree/checkpoint
mechanics, forking, promotion, and file ACLs follow in later slices.

## 1. Slices

- **2a-roles** — the D42 workspace layer: `workspace_roles` (Owner is the
  community owner; Workspace Admins appointed by the Owner), enforcement for
  team-channel creation and Channel-Admin appointment, Guest disabled.
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
| 47010 | `KIND_WORK_THREAD_CHECKPOINT` | regular | Per-turn checkpoint ref (branch + commit) — later slice. |
| 47020 | `KIND_WORK_THREAD_FORK` | command | Thread fork at head/checkpoint — later slice. |
| 47021 | `KIND_WORK_THREAD_PROMOTE` | command | Personal-channel promotion through the gate — later slice. |

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
`admin`; "workspace Owner" = the community owner (communities.owner —
workspace_roles is the additive layer for Workspace Admins, who per D42 do
**not** get terminal thread transitions). Bots never transition — they emit
47003 recommendations. `ready → open` is the one addition to the D41
diagram: without it a mistaken done-proposal could only be closed or
archived; it is non-terminal, so it takes member authority (policy-knob
compatible).

## 4. Storage

Migration 0027:

- `channels.tier` — `channel_tier` enum (`owned`,`private`,`open`),
  NOT NULL DEFAULT `open` (backfills existing rows), **immutable**: no
  update path is added anywhere; the settings handler explicitly rejects
  tier tags.
- `channel_repos` — `(community_id, channel_id)` PK → `repo_name` (the
  forge name bound at creation), `created_at`. FK to channels, tenant-led.
- `workspace_roles` — `(community_id, pubkey)` PK → `role`
  (`workspace_admin`; the Owner needs no row — communities.owner is the
  authority), granted_by, granted_at.
- `work_threads` — `(community_id, thread_id)` PK (thread_id = root event
  id bytes) → channel_id (FK), goal TEXT, deadline TIMESTAMPTZ NULL,
  dri_pubkey BYTEA NULL, status `work_thread_status` enum
  (`open`,`snoozed`,`ready`,`closed`,`archived`), canonicalize_on_close
  BOOL, created_by BYTEA, created_at, updated_at, closed_at NULL.
  Projection only — the signed events are the truth; the row makes
  list/read cheap and transitions TOCTOU-safe
  (`UPDATE … WHERE status = $expected`).

## 5. Authority enforcement points (2a)

- **Team-channel creation** (kind 9007): requires community owner or
  `workspace_admin` for non-DM channels. DM channels (member-pair) stay
  member-creatable. The tier tag is parsed here; a repo is provisioned and
  bound in the same flow.
- **Channel-Admin appointment** (member role changes to owner/admin):
  requires community owner or workspace_admin (or an existing channel
  owner, preserving upstream semantics inside the channel).
- **Guest role**: add_member with role `guest` is rejected at the relay.
- **workspace grant/revoke**: new command kind 47100
  (`KIND_WORKSPACE_ROLE`) — Owner only; content grants/revokes
  `workspace_admin` for a pubkey. (47100 block for workspace governance.)

## 6. Deviations / deferred

- Repo provisioning reuses the forge's existing repo-creation seam (see
  the git API); if the forge is init-on-first-push, the binding row plus a
  server-side init is used instead — resolved during implementation
  against the mapped seam.
- Personal channels (D29), implicit Channel-Admin in them, forking,
  promotion, checkpoints, canonicalization mechanics, and file ACLs are
  later Phase 2 slices; nothing in this slice blocks them.
