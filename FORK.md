# Silent Mesh — Fork Discipline

This repository is a **hard fork** of [block/buzz](https://github.com/block/buzz)
at `90e058eb` (2026-07-28). It does not track upstream. Silent Mesh's
architecture and roadmap live in
[docs/silent-mesh/](docs/silent-mesh/architecture.md) — decisions D33–D36 there
govern this file.

## Fork posture (D35, revised 2026-08-02)

**Snapshot, not stream.** D35 originally said "track upstream" and prescribed
scheduled merges. That was never implemented: there was no `upstream` remote,
`main` never moved off the fork point, and no merge or rebase ever ran. The
gap between the written intent and the practice was itself the risk, so the
intent was changed to match reality rather than the other way round.

What that costs, stated plainly so it is not rediscovered as a surprise:

- **Upstream fixes no longer arrive**, including security fixes, in the
  ~5k-line files this fork depends on but does not maintain
  (`handlers/ingest.rs`, `handlers/req.rs`, `pool.rs`, `kind.rs`). If a
  specific upstream fix matters, cherry-pick that commit deliberately — a
  hard fork makes that a decision rather than a side effect.
- **Contributing back is harder, not impossible.** The governance work still
  implements endpoints upstream's own ARCHITECTURE.md describes; offering it
  now means cherry-picking those commits onto a fresh clone of upstream in a
  separate branch, not merging.
- **Divergence is now permanent and grows.** At the moment of this decision
  upstream was 107 commits and 300 files ahead after five days, overlapping
  29 of our files. That overlap will only ever grow, and re-deciding later
  costs strictly more than deciding now would have.

Rules 1–3 below are **kept**, even though their original justification was
"so merges stay clean". Additive code, no renames, and unbuilt-not-deleted
trees remain good hygiene on their own: they keep this fork's own diff
legible, and they preserve the option of cherry-picking in either direction.

## Branches

- `main` — the fork point (`90e058eb`), frozen. Never commit here.
- `claude/fork-sync-*` — the working branch. All Silent Mesh work lands here.
  (The name predates this decision and no longer describes a sync.)

## Rules (D35)

1. **Additive over invasive.** Silent Mesh code lives in new crates —
   `sm-work`, `sm-gateway`, `sm-seals`, `sm-knowledge`, `sm-publish` — and in
   registered extension points (kind handlers, policy hooks, middleware).
   Invasive edits to upstream crates only when unavoidable; keep them small,
   isolated, and commented `// silent-mesh:` so merges surface them.
2. **No renames.** Crate, binary, and env-var names stay upstream (`buzz-*`).
   Branding happens at the config/deploy/client layer only.
3. **Unbuilt, not deleted.** `desktop/`, `mobile/`, `web/`, `admin-web/` are
   upstream-inherited trees we do not ship, build, or modify. They stay
   in-tree because deleting them buys nothing and they remain useful as
   reference and as a debugging surface. The desktop app may be launched
   locally as a developer debugging tool; it is not a supported client (D36).
   Note this makes them **unmaintained**, not merely unbuilt — a bug found in
   one of those trees is ours to fix or to leave, and no upstream fix will
   arrive for it. (One was already fixed by accident: `0a5d9d9b`, a NIP-44
   conformance bug in `mobile/`.)
4. ~~**Merge cadence.**~~ **No merges.** The fork is frozen at `90e058eb`; see
   the posture note above. Bringing in a specific upstream commit is a
   deliberate cherry-pick, gated like any other change on
   `cargo test -p buzz-relay -p buzz-core -p buzz-db -p buzz-acp` plus the
   isolated integration harness (`docker-compose.harness.yml`).
5. **CI posture.** Upstream's workflows stay disabled on this fork. Our CI (to
   be added under `.github/workflows/silent-mesh-*.yml`) builds and tests the
   backend crates + `sm-*` crates only.
6. **Upstream what belongs upstream.** The governance/approvals work (roadmap
   Phase 1) implements endpoints upstream's own ARCHITECTURE.md describes —
   structure those patches for contribution to Block. Anything
   privacy-plane-specific (tiers, seals, gate) stays ours.
7. **Disabled upstream features** (config, not code): Keycloak/SSO, public
   Caddy exposure. Dormant until wanted: huddles, canvases, forum channels,
   workflows, mesh compute (roadmap Phase 8 review).

## Deploy

Single Linux host, VPN-only, via [deploy/silent-mesh/](deploy/silent-mesh/README.md).
The workspace server never faces the internet; the public wiki (D43) is a
separate static host fed by an export pipeline — nothing on this box serves
public traffic.
