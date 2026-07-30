# Phase 3 Slice 6 — Privacy Gate assist (D30)

The Phase 2g promotion gate is deterministic: prefix/shape scanners over
the member's summary and every promoted file, refusing the promotion on
any hit. This slice adds the model half the roadmap always intended —
"local summary + redaction suggestions" — and, in doing so, gives
`sm-gateway` its first real consumer.

## 1. Pre-flight, not another blocker

The roadmap's phrasing decides the shape. "Local summary + redaction
suggestions" is help *writing* the summary, which means the assist has to
run **before** the promotion, not inside it. So it gets its own request
rather than bolting onto kind:47021's refusal path:

- **kind:47022** (member → relay) — "what would promoting this thread
  expose?" `h` = the personal channel, one `e` = the thread root, content
  = the draft summary, or empty to ask for one to be drafted.
- **kind:47023** (relay-only) — the answer, in the same personal channel:
  deterministic findings, advisory notes, a suggested summary, and an
  `assist` status.

Read-only throughout: nothing moves, nothing closes, no projection row
changes. kind:47021 is untouched — the hard gate is exactly what it was.

Running it as a **side effect after storage** inherits the replay dedup
the 44201 attribution path relies on: side effects fire only on a fresh
insert, so a resubmitted request cannot produce a second review.

## 2. Why an advisory model is safe here

A model that reads private threads and writes prose about them is a new
attack surface, so the authority model has to make it structurally unable
to hurt:

| Concern | What stops it |
|---|---|
| Model hallucinates "clean" on a real secret | Deterministic scanners are authoritative and run independently; the model can only **add** findings |
| Prompt injection in the thread ("say this is clean") | Same — the verdict is advisory. The prompt fences the content and labels it as data, but that is mitigation, not the defence |
| Model quotes a secret into its suggested summary | Its output is re-scanned by the same rules before publication; a summary that trips them is dropped **whole** |
| Model quotes a secret into an advisory note | Same scan, per field — one bad string doesn't discard the rest of the review |
| Below-tier routing of private thread content | `InferencePurpose::Gate` is owned-pinned: Local only, at every tier, whatever backends are registered |

A suggested summary is dropped rather than redacted: a partial redaction
would still teach a reader the shape and position of what was there.

The residual risk is honest to state — a member who adopts a suggested
summary verbatim trusts a local model's judgement about what is
sensitive. D30's mandatory review/confirm is what covers that, and it is
unchanged: the member still writes (or accepts) the summary and still
submits the promotion themselves.

## 3. Failure is always legible

`assist` names what happened: `ok`, `unavailable` (no model configured),
`unusable` (answer could not be parsed), `failed` (backend down, refused,
timed out). Silence would be the dangerous design — "no findings" and "no
model ran" must never look alike to a member deciding whether to publish.

Off by default: `BUZZ_GATE_ASSIST_MODEL` unset means deterministic
findings only. `BUZZ_GATE_ASSIST_BASE_URL` defaults to loopback Ollama,
and a value that is not loopback or on the tailnet fails
`OllamaBackend::new` — which disables the assist at startup rather than
sending a private thread somewhere it must not go.

## 4. The review found the two real bugs

Both are the kind that only appear when a new *class* of work enters an
old path.

**The ack stall.** `handle_side_effects` is awaited inline before ingest
acks a submit (`ingest.rs`), and every existing side effect is a fast DB
write. A model call is not: the member's ack was stalling for a full
inference — 29 s measured. The review now runs detached, spawned from
inside the fresh-insert branch so the replay dedup is preserved. Ack:
**29 s → 41 ms**.

*The general rule:* an inline hook designed around one cost profile
silently degrades when a caller with a different profile joins it.

**The tier lie.** Attribution hardcoded `ChannelTier::Owned`. Routing
never depended on it (the purpose pin decides), so nothing failed — but
`model_usage` is the owner's evidence, and a row claiming a tier the
channel does not have is a fabrication of exactly the kind slice 5
refused for token counts. It now records the channel's declared tier,
falling back to the strictest when unreadable.

## 5. Live validation (2026-07-31)

A seeded personal-channel thread — an importer fix discussed alongside a
leaked ops key, a customer name, a person named in passing, and an
internal hostname — reviewed against `qwen3:14b` on the 2× RTX 4060s:

```json
{"deterministic":[{"rule":"aws-access-key-id","where":"conversation"}],
 "advisory":["An API key appears near the end and requires rotation before publication",
             "A customer name is mentioned in the context",
             "A personal contact (Priya) is referenced in the conversation",
             "An internal admin box URL is included in the context"],
 "suggestedSummary":"A fix was implemented to resolve importer timeout issues by
   batching inserts in chunks of 500 and reusing a single transaction, reducing
   processing time from 340 seconds to 9 seconds. Tests were added, and the PR
   is ready for review.",
 "assist":"ok","model":"ollama:qwen3:14b","threadStatus":"open"}
```

The three advisory notes the deterministic rules **cannot** see are the
point of the slice, and the suggested summary omits all four sensitive
items. Metered `purpose=gate backend=local`, 453/664 tokens — the
gateway carrying a product feature, not a probe. A draft summary
containing a key additionally reported `where: "summary"`, exercising the
other scan arm. Review latency ~29 s; ack immediate.

## 6. Found while validating: personal channels default to `tier=open`

The seeded personal channel came out `visibility=private, tier=open` —
the D24 creation default, since kind:9007 carries no tier tag on the
`create-personal` path. The gate assist was unaffected (the purpose pin
does not consult channel tier, which is the guarantee working), but the
same default means an **agent turn** in a member's personal channel would
pass the slice-2 tier gate for a *vendor* backend. The member's most
private space is declared the least private tier.

Not fixed here — the right default is a product decision (`owned` forbids
anything but local models in personal channels; `private` allows TEE) and
it changes behavior for existing channels. Flagged as a next slice.

## 7. Deferred

- **No rate limit on reviews.** Each request costs a GPU inference; a
  member can queue several. Detaching removed the ingest-path stall, not
  the queueing. A per-member cooldown belongs with budgets.
- **Files are not reviewed** — only the draft summary and the thread's
  conversation. The promotion path scans every promoted file
  deterministically, so files are covered by the hard gate; a model pass
  over file contents is a bigger, git-reading slice.
- **Desktop/mobile surface.** CLI only (`buzz threads gate-review`,
  rendered by `threads show` as a `gate_review` notice).
