# Phase 3 Slice 3 — Local backend (Ollama): owned channels serve turns

Slice 2 made owned/private channels refuse vendor-model turns; this slice
gives them something to serve instead. `buzz-agent` gains a `Provider::Ollama`
(local GPU inference over the OpenAI Chat dialect at a loopback base URL),
and the harness's model-switch confirm is hardened so a Local classification
can only come from the agent's **self-declared** identity. Validated live on
the two-machine testbed: the owned-tier vault that refused
`anthropic:claude-…` answered a Mac member's message with `qwen3:14b` running
on the workspace's own 2× RTX 4060 — zero egress, tool-executed threaded
reply.

## 1. The provider

`BUZZ_AGENT_PROVIDER=ollama`: no API key (Ollama ignores auth — a
placeholder bearer keeps the header well-formed), base URL
`OLLAMA_BASE_URL` defaulting to `http://127.0.0.1:11434/v1` (loopback ⇒ the
"local" claim is literal), Chat dialect pinned (no Responses API), model
from `BUZZ_AGENT_MODEL`/`OLLAMA_MODEL`. The persona `ollama:` prefix strips
for the wire — Ollama tags themselves contain `:` (`llama3.2:3b`), so only
the known provider prefix is stripped (`strip_ollama_prefix`).

## 2. Self-declared local identity (the load-bearing part)

The tier gate classifies a turn from the persona prefix of the harness's
`desired_model`, and slice 2's re-gate refuses turns whose declared model
was not **confirmed** applied. That confirm is only meaningful for a
Local-class prefix if the provider half is verified too:

- An Ollama agent advertises the **prefixed** id (`ollama:qwen3:14b`) as its
  `session/new` catalog identity — a self-declaration that this model is
  served by the local runtime. The harness's exact-match switch confirm then
  verifies provider and model as one unit, and the prefix strips again at
  the wire.
- Only the prefixed form is advertised (no bare alias): the desktop picker
  can then only select prefixed ids. Adversarial review found that a bare
  pick stores a bare `desired_model`, whose first-colon split yields a
  non-provider prefix ⇒ Vendor ⇒ the genuinely-local agent is locked out of
  owned/private until restart.
- The harness's persona-prefix matcher fallback (prefixed desired vs bare
  advertised id) applies to **Vendor-class prefixes only**
  (`is_known_provider_prefix` + `provider_to_backend` gate it). Review
  confirmed the unrestricted fallback re-opened the slice-2 leak:
  `ollama:claude-x` would confirm against a vendor-served agent advertising
  bare `claude-x`, skipping the re-gate while the gate classified the turn
  Local. Vendor prefixes can't relax anything, so `anthropic:X` matching
  advertised `X` remains safe convenience.

## 3. Reply contract for local models

Agents reply by **executing** `buzz messages send` through their shell tool
("tool-calls-as-output"); the harness discards plain text. Local models
read the old instruction ("use `--reply-to` on `buzz messages send`") as
output formatting — qwen2.5:14b echoed the flag as literal text. The
instructions now state the reply is an execution via the shell tool and
that plain text is discarded, with the full command template. That plus a
tool-capable model closes the loop.

**Model notes (empirical, ollama 0.32.5):** qwen2.5 (7b/14b) silently
swallows tool calls when 5+ tools are advertised — bisected against both
`/v1/chat/completions` and the native endpoint; pairs of tools work, the
full buzz-dev-mcp set fails deterministically (template-level). llama3.2:3b
answers in text. **qwen3:14b handles the full toolset and composed the
correct `buzz messages send` invocation** (channel UUID + `--reply-to` +
content) from the standard prompt.

## 3b. Text-fallback reply (shipped with this slice)

Even with the execution-explicit instructions, local models drop the
send-step after exploration rounds — the correct answer lands as final text
and the channel stays silent. The harness now catches it: a channel turn
ending `EndTurn` with un-sent final text posts that text as the threaded
reply.

- **Buffer**: session-scoped `agent_message_chunk` accumulator, reset at
  every boundary where earlier text becomes non-final (turn start, tool
  round starting, delivered mid-turn steer, `started_new_turn`). Only the
  post-final-boundary text can post — never preamble or superseded drafts.
- **Never double-post**: one relay probe (kinds 9/40002/45001/45003 by the
  agent's own key in the channel since turn start); any hit *or any probe
  error* skips — fail closed. Inert for capable models.
- **Only clean endings**: `MaxTokens`/`MaxTurnRequests`/`Refusal` turns and
  the completed-before-control-signal race never post (truncated/refused
  scratch is not an answer; a steering user must not get a stale one).
- **Envelope unwrap**: when the final text is a literal
  `buzz messages send --content "…"` command (the model composed the right
  command but emitted it as text — the most common shape observed), the
  content payload posts instead of the command string.
- Adversarially reviewed (6 confirmed findings, all fixed pre-commit).
  Validated live: vault question → clean threaded prose reply in 14 s via
  the fallback; the tool path stays preferred and suppresses it.
- Trust note: the probe's author filter is the harness key, which the MCP
  env injection forces into the CLI the agent uses — the two can only
  diverge for a custom agent bypassing the harness MCP (out of contract).

## 4. Deferred / next slices

- **Attribution from live turns**: with inference now local, the gateway-
  mediated path (sm-gateway `OllamaBackend` + `route_and_record`) can meter
  real turns into `model_usage` — next slice, closes the Phase 3 exit
  criterion together with the owner usage read surface.
- Desktop wiring: the desktop projects a **bare** model id into
  `BUZZ_ACP_MODEL`, so desktop-launched local agents don't yet pass the
  gate in owned/private; the desktop should write the prefixed form when
  the provider is a known local runtime.
- TEE backend, per-user vendor CLIs, seals/copilot/retrieval — per roadmap.
