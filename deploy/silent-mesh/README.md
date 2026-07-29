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
