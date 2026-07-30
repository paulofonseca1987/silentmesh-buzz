# Phase 3 Slice 4 — Live-turn attribution (kind 44201) + the owner usage read

Closes the Phase 3 exit criterion: **"usage query shows per-user totals by
tier and backend"** — with real data from real zero-egress turns.

## 1. The bridge kind

The harness has the plaintext turn data (model, tokens, channel, triggering
user) but no database; the relay has the `model_usage` store (slice 1) but
cannot decrypt the owner-encrypted `kind:44200` turn metric. The bridge is
`kind:44201` — a **cleartext attribution sibling** published by the harness
per completed turn with reliable token counts:

```json
{ "model": "ollama:qwen3:14b", "promptTokens": 4100, "completionTokens": 1054,
  "purpose": "agent_turn", "channelId": "…", "userPubkey": "…",
  "threadRootId": "…" }
```

Same tag contract as the 44200 (one `p` = owner, one `agent` = event pubkey,
no `h` tag, global-only), and the same **owner-only read gating** on every
surface: P/RESULT-gated arrays, the reader gate, the `ids`-exemption
removal, live fan-out, and storage-level FTS exclusion (migration 0033 —
which *wraps* the live `search_tsv` expression per the 0014 pattern, so the
0008 fresh-install allowlist and brownfield denylists both survive).

## 2. The relay is the classification authority

The client's opinion of tier/backend is never consulted:

- **Tier** — resolved from the relay's own `channels` table by the payload's
  channel id (immutable per D26).
- **Backend** — derived from the model string via the shared
  `buzz_core::model_route::classify_model`: a known provider prefix
  classifies (e.g. `ollama:` → local), anything else — including bare
  colon-bearing Ollama tags — fails closed to vendor. This is the *same
  rule the tier gate applies*, so a turn is metered under exactly the
  classification it was gated by. The harness therefore publishes the
  **declared** model (the gate-classified `provider:model` form), falling
  back to the usage-reported model only when nothing was declared.
- **Ownership** — the `p` tag must be the agent's registered owner
  (`users.agent_owner_pubkey`, the NIP-OA `BUZZ_AUTH_TAG` path in
  production); ingest rejects otherwise.
- **Dedup** — side effects only run on fresh event inserts, so a replayed
  44201 can never double-record.
- Payload validation at ingest matches the `model_usage` CHECK constraints
  (purpose vocabulary, hex shapes, model ≤128 chars) so a bad payload fails
  *visibly* at submit rather than silently losing the row in the side
  effect.

Trust note: `userPubkey` (whose message triggered the turn) is
agent-claimed — the same trust class as everything else the owner-governed
agent signs.

## 3. Reliable first-turn deltas

The usage tracker conservatively marks a session's first turn
delta-unreliable (no baseline — correct for sessions first observed
mid-life, e.g. after a harness restart). But a session the harness **itself
just created** via `session/new` has a provably zero baseline —
`UsageTracker::seed_fresh_session` records that fact at both
session-creation sites, making first-turn deltas reliable. Without this,
`--max-turns-per-session 1` (every turn a first turn) never published
attribution at all.

## 4. The owner read

`buzz-admin usage [--since-hours N]` — direct-DB operator surface (needs
only `DATABASE_URL` + `RELAY_URL` for community resolution), printing
`user_usage_totals`: per-user rows by tier and backend with request and
token counts.

## 5. Live validation (2026-07-30, two-machine testbed)

Mac member's vault message → tier gate (local ✓) → confirmed switch →
qwen3:14b on the 2× RTX 4060 → tool-posted reply → 44201 → relay verified
ownership, resolved `owned`, classified `local` → `model_usage` row
(4100/1054 tokens) → `buzz-admin usage`:

```
user_pubkey      tier   backend  requests  prompt_tokens  completion_tokens
2bd2c851…(Mac)   owned  local           1           4100               1054
```

The debugging chain that got there was three *different guards working
correctly*: a silent skip (fixed with skip-reason logging), the
conservative first-turn delta rule (fixed with fresh-session seeding), and
403s from the ownership check against a wiped registration (fixed by
re-registering — the check held).

## 6. Deferred

- Member-facing usage reads (an event-kind surface; the operator CLI closes
  the exit criterion).
- Budget enforcement wiring (sm-gateway's `Budget` exists; enforcement at
  the harness/gateway is a later slice).
- The gateway-mediated inference path (sm-gateway `OllamaBackend`) — the
  44201 meters harness-run turns; gateway-run inference (copilot, gate,
  embedding) will record directly via `route_and_record`.
