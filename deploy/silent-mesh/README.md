# Silent Mesh single-host deploy

VPN-only, one Linux host, built from this fork's source. See
[FORK.md](../../FORK.md) for the discipline and
[docs/silent-mesh/](../../docs/silent-mesh/architecture.md) for the plan.

```bash
cp .env.example .env        # fill in: VPN bind IP + generated secrets
docker compose -f ../compose/compose.yml -f compose.silent-mesh.yml up -d --build
curl http://$SM_VPN_BIND_ADDR:3000/_readiness    # from a VPN-connected machine
```

What this profile deliberately does NOT include:

- `compose.caddy.yml` / any public TLS or internet exposure (D11 — the public
  wiki, D43, is a separate static host fed by an export pipeline, Phase 7)
- Keycloak / SSO (dev-stack concern upstream; disabled for Silent Mesh)
- The GPU inference server — arrives in Phase 3 as an sm-gateway backend
  service in this file (2× RTX 4060 via device_requests)

Upstream's prod compose (`../compose/compose.yml`) provides Postgres 17,
Redis 7, and MinIO with healthchecks; this file only overrides the relay
(build-from-source, VPN-bound port).

## Supervised agents (Phase 1 governance)

Silent Mesh runs agent harnesses **supervised by default**: every gated
tool call parks as a pending approval (kind:46010 in the channel) until a
channel member decides:

```bash
buzz approvals list --status pending
buzz approvals grant --request <uuid> --note "looks safe"
buzz approvals deny  --request <uuid> --note "not like this"
```

The `.env.example` block sets `BUZZ_ACP_RUNTIME_MODE=supervised` with an
allowed-modes ceiling of `supervised,auto-accept-edits` — export those in
every harness environment. `full-access` is intentionally outside the
ceiling; granting it back is an explicit operator decision. This fork also
urgent-classifies kind:46010 for push leases so a blocked agent reaches
the approver's device promptly (see `push_lease.rs`, `silent-mesh:` marker).
