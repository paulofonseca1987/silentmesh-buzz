# Phase 3 Design Note — Model plane: tier-aware routing + metering

Implements the opening of roadmap Phase 3 (architecture §11) on the
foundations Phase 2 landed. Written against this fork with all of Phase 2
merged. Scope of this first slice: the **routing policy**, the
**attribution store**, and the **`sm-gateway` skeleton** with stub
backends. The real backends (server-GPU serving, TEE, per-user vendor
CLIs), the copilot, the inference queue, seals, and retrieval are later
slices — the routing and metering *contracts* here do not change when they
land.

## 1. The privacy spine

Every model request must satisfy two invariants, decided in one place:

1. **Tier-aware routing (D24)** — a request may only reach a backend that
   satisfies its channel's minimum privacy tier. This is the guarantee
   that a value in an `owned` channel never egresses, and that a `private`
   channel never routes to a member's own vendor.
2. **Per-request attribution (D16)** — every routed request is recorded
   `(user, agent, channel, thread, model, tier, backend, purpose)` with
   token counts, the substrate for usage views and budgets.

## 2. Backends, tiers, and the routing rule

`buzz_core::model_route` is **pure policy** — no I/O, no backends — so the
whole decision surface is exhaustively unit-testable (like the D41
authority matrix).

A **backend** is ranked by egress exposure:

| Backend | Egress | Class |
|---|---|---|
| `Local` | none | client-local / workspace GPUs |
| `Tee` | attested | TEE confidential-compute provider |
| `Vendor` | cleartext to a third party | member's own subscription |

A **channel tier** declares the minimum privacy every request must meet:

| Tier | Permitted backends |
|---|---|
| `Owned` | `Local` only |
| `Private` | `Local`, `Tee` |
| `Open` | `Local`, `Tee`, `Vendor` |

`route(tier, backend, purpose) -> Allow | Refuse(reason)` applies two
independent gates, both of which must pass:

- **Purpose pin** — the Prompt Copilot (D25), the Privacy Gate's model
  assist (D30), and the retrieval embedding pipeline (D37) read across
  content that must never egress, so they are pinned to `Local` **at every
  channel tier**. Checked first, so a pinned-purpose request in an `open`
  channel gets the specific `OwnedPinnedPurpose` refusal.
- **Channel minimum (D24)** — otherwise the backend must satisfy the
  channel's declared minimum, per the table above.

Refusals carry a typed `RefuseReason` (`BelowChannelMinimum` /
`OwnedPinnedPurpose`) so callers match without string-parsing.

## 3. Attribution store

Migration 0032 adds `model_usage` (`buzz_db::model_usage`): one row per
routed request, community-scoped, community-led PK, reusing the
`channel_tier` enum from 0027; `backend`/`purpose` are CHECK-constrained to
the known sets so a typo can't create a phantom bucket. Reads:

- `user_usage_totals(community, since?)` — per-user totals grouped by
  `(tier, backend)`. This is the roadmap exit criterion's "usage query
  shows per-user totals by tier and backend".
- `user_token_spend(community, user, since?)` — one user's total spend, the
  input a budget check compares against.

The signed kind:44200 turn-metric events remain the per-turn *wire* record;
this table is the *queryable* metering substrate the gateway owns.

## 4. The gateway skeleton

`sm-gateway` (new additive crate, per D35) is a backend registry + the
metering store + an optional per-user token budget:

- **`ModelBackend`** (async trait) — real backends implement it; the
  gateway never calls one the policy hasn't allowed. This slice ships
  **stub** impls (`stub::local`/`tee`/`vendor`) returning canned
  completions + a word-count token estimate, so routing/metering are
  testable with no hardware.
- **`resolve_backend`** (pure) — the request's explicit backend, or the
  *loosest the tier/purpose permits* (the most capable route still within
  the privacy floor).
- **`route_and_record`** — strict order: **policy gate first** (a
  below-tier or owned-pinned request is refused before any backend runs and
  records nothing), then the budget gate, then dispatch, then attribution.
  Attribution is written only for requests that actually ran.

## 5. Deferred / next slices

- Real backends as `ModelBackend` impls: server-GPU serving spike
  (llama.cpp/vLLM/Ollama), the TEE provider (attestation-then-send), and
  per-user vendor CLIs with credential isolation + operated-for
  attribution.
- The harness/relay wiring that *calls* the gateway (buzz-acp turns, the
  copilot, the retrieval pipeline) — the gateway is the substrate; nothing
  calls it yet.
- The owner's usage read surface (CLI/relay) over `user_usage_totals`.
- Content Seals (D31), the Privacy Gate's model assist (D30 upgrade), the
  retrieval foundation (D37) — all pinned owned-tier in the router by
  construction.
- Budget policy detail (budgets vs reporting-only; what non-owners see) —
  the roadmap leaves this an open question; this slice ships a simple
  trailing-window token ceiling to prove the enforcement point.
