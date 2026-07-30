# Phase 3 Slice 5 — The gateway's real `local` backend (Ollama)

`sm-gateway` shipped in slice 1 as a skeleton: routing gate, metering
write, budget hook, and three **stub** backends. This slice makes the
`local` class real, so the gateway can carry a request instead of only
describing how one would be routed.

## 1. Why `local` first (and not TEE or vendor)

Not sequencing preference — the routing policy decides it. `Copilot`,
`Gate`, and `Embedding` are **owned-pinned** in
`buzz_core::model_route`: allowed on `Backend::Local` at *every* channel
tier and refused on every other backend, because each reads across
content that must never egress. So the three remaining Phase 3
capabilities — Prompt Copilot (D25), Privacy Gate assist (D30), the
embedding/retrieval pipeline (D37) — cannot be built on any backend but
this one. A real `Local` impl is their shared prerequisite; TEE and
vendor unblock nothing that is currently blocked.

## 2. Provable locality, not asserted locality

A `ModelBackend` whose `kind()` returns `Backend::Local` is *trusted by
the policy*: the router waves it through for an `owned` channel without
further question. A backend that returned `Local` while pointed at a
vendor endpoint would therefore void the tier guarantee **silently** —
the refusal path would never run, and content would egress with the
routing layer reporting success.

So locality is checked, not claimed. `check_local_base_url` (pure, no
DNS, no I/O) admits only:

| Form | Accepted |
|---|---|
| Loopback | `127.0.0.0/8`, `::1`, `localhost` |
| Tailscale IPv4 (CGNAT) | `100.64.0.0/10` |
| Tailscale IPv6 (ULA) | `fd7a:115c:a1e0::/48` |
| Tailscale MagicDNS | `<host>.<tailnet>.ts.net` |

`OllamaBackend::new` applies it at **construction**, so a
misconfiguration is a startup failure rather than a request-time leak.

RFC1918 is deliberately **not** local. A tailnet peer is an
owner-enrolled, WireGuard-authenticated machine; a `192.168.x.y` host is
whatever answers on the LAN. Excluding it also means a typo'd address
fails closed instead of quietly becoming a permitted route.

This is the gateway-layer form of the rule slice 3 established for the
harness: **a Local classification must never be inferred — only proven.**

## 3. The URL check is necessary but not sufficient

The adversarial review's finding, and the more interesting half of the
slice: two `reqwest` defaults route around a validated URL.

- **Redirects are followed** (up to 10 by default). A loopback service
  answering `302 Location: https://api.openai.com/v1/chat/completions`
  would egress the owned-tier prompt *from an endpoint that passed the
  host check*. The guard validates the URL we dial; only refusing to
  follow makes that the URL we actually talk to.
- **Proxy environment variables are honored**, with no automatic
  loopback bypass. `HTTP_PROXY` in the relay's environment would route
  "local" inference through a third party.

Both are pinned off on the client (`Policy::none()`, `.no_proxy()`). A
raw one-shot loopback server test asserts the 302 surfaces as a backend
error and that the vendor host never appears — a property test, not a
configuration assertion, because the next reqwest upgrade should fail
the test rather than the guarantee.

The general shape is worth keeping in mind for the TEE backend: **a
static check on configuration can be escaped by dynamic transport
behavior.** Locality is a property of the whole conversation.

## 4. Real token counts, or none

The stubs estimate tokens by counting whitespace words. This backend
reports the server's own `usage` numbers, and treats a response it
cannot meter as a **backend error** rather than substituting an estimate
or a zero. Attribution that is quietly fictional is worse than
attribution that is absent: `model_usage` is the owner's evidence, and a
plausible wrong number is unfalsifiable in a way a missing row is not.

Attribution records the model id the caller asked for — the
persona-prefixed self-declared local identity (`ollama:qwen3:14b`) that
slice 3 made load-bearing for classification. Only the wire call strips
the prefix, since the Ollama server knows just the bare tag. Same rule
as `buzz-agent`'s `strip_ollama_prefix`: the known provider prefix only,
case-insensitively, so colon-bearing tags survive intact.

## 5. Live validation (2026-07-31)

Ollama restarted on the WSL host (sudo-less user install at
`~/ollama/bin/ollama`, CUDA on the 2× RTX 4060s). Gated probes green:

```
live:    36 tokens in / 4 out
metered: 36/3 tokens as owned/local
```

The second is the one that matters — `Gateway::route_and_record` took an
owned-tier `AgentTurn` with no explicit backend, resolved `Local`,
dispatched to a real model, and wrote a `model_usage` row with real
counts. The first request the gateway itself has carried end to end.

```bash
SM_GATEWAY_OLLAMA_PROBE=1 DATABASE_URL=postgres://buzz:buzz_dev@localhost:5432/buzz \
  cargo test -p sm-gateway --lib -- --ignored --test-threads=1
```

`SM_GATEWAY_OLLAMA_PROBE_MODEL` overrides the model (default
`llama3.2:3b`, small enough to answer in ~2 s);
`SM_GATEWAY_OLLAMA_BASE_URL` overrides the endpoint.

## 6. Deferred

- **Consumers.** Nothing calls the gateway yet — the harness still runs
  its own turns and meters them via kind 44201 (slice 4). Copilot, gate
  assist, and embeddings are the callers this backend exists for.
- **Embeddings** need a different endpoint *and* a different return
  type; `RawInference { text, tokens }` cannot express a vector. That is
  a real seam in the trait, to be widened with D37 rather than distorted
  now.
- **Streaming.** The gateway is request/response; token streaming to a
  UI is a later shape.
- The **stub `local()`** remains for routing/metering tests that must run
  with no model server. It must never be registered in a deployment,
  where its canned completion would meter as real usage.
