# Silent Mesh — Fork Discipline

This repository is a fork of [block/buzz](https://github.com/block/buzz) that
tracks upstream. Silent Mesh's architecture and roadmap live in
[docs/silent-mesh/](docs/silent-mesh/architecture.md) — decisions D33–D36 there
govern this file.

## Branches

- `main` — clean mirror of `upstream/main`. Never commit here.
- `silent-mesh` — the long-lived integration branch. All our work lands here.

## Rules (D35)

1. **Additive over invasive.** Silent Mesh code lives in new crates —
   `sm-work`, `sm-gateway`, `sm-seals`, `sm-knowledge`, `sm-publish` — and in
   registered extension points (kind handlers, policy hooks, middleware).
   Invasive edits to upstream crates only when unavoidable; keep them small,
   isolated, and commented `// silent-mesh:` so merges surface them.
2. **No renames.** Crate, binary, and env-var names stay upstream (`buzz-*`).
   Branding happens at the config/deploy/client layer only.
3. **Unbuilt, not deleted.** `desktop/`, `mobile/`, `web/`, `admin-web/` are
   upstream-owned trees we do not ship, build, or modify. They stay in-tree so
   upstream merges stay clean. The desktop app may be launched locally as a
   developer debugging tool; it is not a supported client (D36).
4. **Merge cadence.** `git merge upstream/main` into `silent-mesh` biweekly or
   on upstream releases — merge, not rebase (the branch is shared). Gate every
   merge on the backend test subset:
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
