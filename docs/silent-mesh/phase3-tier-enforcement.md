# Phase 3 Slice 2 — buzz-acp pre-turn tier enforcement

Wires the Phase 3 routing policy (slice 1, `buzz_core::model_route`) into the
buzz-acp ACP harness as a **pre-turn gate**: before any user content reaches
an agent subprocess, a turn whose model backend would egress below the
channel's immutable privacy tier is refused. This makes the routing policy
load-bearing at the real turn boundary — an owned-channel turn can no longer
reach a vendor model.

## 1. Why the gate lives in the harness (the design fork)

The intuitive "record/enforce on the relay" path is **blocked by the privacy
model**, not merely unimplemented:

- The per-turn record is the `kind:44200` Agent Turn Metric, whose payload
  (model, channel, token counts) is **NIP-44 encrypted agent → owner**. The
  relay sees only the `p`(owner)/`agent` tags; it cannot read what a turn
  used. So the relay cannot classify or attribute a turn from ingest.
- buzz-acp is a **pool-less client** (no `buzz-db`). It cannot write the
  Phase-3 `model_usage` table directly.

What buzz-acp *does* have is (a) `buzz-core` — hence the pure `route()` policy
with no I/O — and (b) full plaintext at the turn boundary: the channel it is
about to serve and the model the agent is configured to use. And crucially,
**the harness is the only component that can actually _prevent_ egress** — it
launches the agent subprocess that would otherwise call the vendor. A
relay-side gate could not stop that call.

So the "wiring" splits in two, and this slice is the first half:

1. **Enforcement (this slice)** — pure policy at the harness turn boundary. No
   DB. Fully unit-testable.
2. **Attribution (deferred)** — recording live-turn usage into `model_usage`.
   Blocked for external-model turns by the 44200 encryption; it lands with the
   gateway-mediated real-backend slice (there the gateway sees plaintext *and*
   holds the pool) or an owner-side decrypt-and-record path.

## 2. The gate

At the top of `run_prompt_task` (before session creation, before any prompt is
sent to the subprocess):

```
tier     = channel_info.resolve(channel_id).tier      // from the 39000 `tier` tag
decision = tier_gate_decision(tier, agent.desired_model)
             = route(tier, provider_to_backend(split_model(model).provider), AgentTurn)
Allow          → run the turn unchanged
Refuse(reason) → post a kind:9 notice (post_failure_notice) + end the turn
                 (PromptOutcome::Cancelled, batch=None → agent released, no
                 requeue, no respawn). The agent never sees the content.
```

Heartbeats carry no channel and are never gated.

### The declared-vs-effective re-check

The pre-turn gate validates the *declared* model — but the subprocess can
silently fall back to its **built-in default** when that model isn't in its
catalog (`resolve_model_switch_method` → miss) or the switch fails at the
application level. Adversarial review confirmed this as an egress leak: an
owned-channel agent declared as `ollama:llama3` passes the gate as Local,
the switch fails (no local backend ships yet), and the turn would run on the
vendor default. The fix closes the gap end to end:

- `apply_model_switch` returns whether the switch was **confirmed**
  (application-level failures report `false`, not success).
- `create_session_and_apply_model` returns `(session_id,
  desired_model_verified)` — verified means "the model the gate classified is
  actually in effect" (`None` declared counts as verified: the gate already
  classified the default fail-closed).
- After session creation, an unverified fallback is **re-gated as
  `tier_gate_decision(tier, None)`** (⇒ Vendor, fail-closed): refused in
  owned/private with a dedicated notice, and the session is deliberately
  **not stored**, so a reused session in a restricted-tier channel is always
  a verified one. Refusal still precedes `session/prompt` — no user prompt
  reaches the subprocess.

Known residuals (assessed, non-blocking): the `session/new` request itself
carries the system prompt (persona + core + canvas) into subprocess *memory*
before the re-check can refuse — but inference only happens at
`session/prompt`, which never fires on the refused path; hardening would
pre-check the cached model catalog before `session/new` in restricted tiers.
Heartbeat turns (self-prompts, no channel content) are ungated by design —
tool-mediated reads during heartbeats are a separate work item.

### `provider_to_backend` (buzz-core, pure)

Maps a persona `"provider:model-id"` provider segment to a `Backend`:
`local`/`ollama`/`vllm`/`llamacpp` → `Local`; `tee` → `Tee`; **everything else,
including an absent or unrecognized provider → `Vendor`** (the most
egress-exposed class). No real Local/TEE providers ship yet — those arms are
the extension point for the real-backend slice.

## 3. Fail-closed, in both inputs

Enforcement under uncertainty always assumes the strictest interpretation:

- **Unresolvable tier ⇒ `Owned`** (`tier_from_tags`, and the resolver-miss
  default in the gate). This deliberately differs from the relay's channel
  *creation* default of `open`: creation decides what tier a *new* channel
  gets; the gate decides what to *assume* when it cannot read a tier, where the
  safe assumption is the opposite.
- **Absent/unclassifiable provider ⇒ `Vendor`**, so a model we cannot prove is
  local is refused in owned/private channels rather than waved through.

The relay already stamps every `kind:39000` channel-metadata event with a
`["tier", …]` tag, and buzz-acp already consumes those events for discovery —
so the tier reaches the client with **no relay changes**; the harness just
reads a tag it previously discarded, threading it through `ChannelInfo` →
`PromptChannelInfo` → the resolver cache.

## 4. Rollout: strict

Every model provider in the tree today (`anthropic`/`openai`/`databricks`) is
`Vendor`. Strict enforcement therefore means **owned and private channels
refuse all agent turns until a Local/TEE backend exists** — that is the
zero-egress guarantee of those tiers biting exactly as designed, not a
regression. (Warn-only and config-gated rollouts were considered and rejected:
a warn-only gate is not load-bearing.)

## 5. Testing

- `provider_to_backend` — exhaustive mapping + fail-closed (buzz-core unit).
- `tier_gate_decision` — the composed decision across tier × provider,
  including `None`/prefix-less fail-closed (buzz-acp unit).
- `format_tier_refusal_notice` — names the tier and the permitted backends.
- `tier_from_tags` + `merge_discovered_channels` — parse the tag; missing
  metadata fails closed to `Owned` (buzz-acp unit).

## 6. Deferred / next slices

- The attribution half (live-turn usage → `model_usage`) — see §1.
- Real `ModelBackend` impls (server-GPU `Local`, TEE) — which also *populate*
  the non-Vendor provider arms of `provider_to_backend` and unblock owned/
  private agent turns.
- Per-user budget enforcement at the gateway (slice 1 shipped the mechanism).
