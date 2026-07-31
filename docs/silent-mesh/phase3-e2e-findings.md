# Phase 3 — end-to-end findings (2026-07-31, overnight run)

Everything shipped through slice 8, exercised against a live relay from
both machines: the WSL host (relay + Postgres + Redis + MinIO + Ollama on
2× RTX 4060) and the MacBook over Tailscale as a real remote client.

**Result: Phase 3 E2E 24/24, Phase 2 exit dry run 30/30.** Three defects
were found and fixed during the run; two decisions were taken under the
"assume the best default" instruction and are flagged for review.

## What ran

| Suite | Checks | Result |
|---|---|---|
| `phase3-e2e.sh` (new) | 24 | 24 PASS |
| `phase2-exit-dryrun.sh` (regression) | 30 | 30 PASS |
| Workspace unit tests | 786 | pass (2 known pre-existing failures) |
| PG-gated `buzz-db` | 150 | pass |
| Cross-machine (Mac client) | manual | pass |

The two pre-existing failures are documented in HANDOFF.md: the
`mesh_demo` UDP flake (passes on retry) and an upstream `git-sign-nostr`
BIP-340 test that fails identically on a clean tree.

## Findings

### F1 — the assist's summary leaked what its own advisory flagged (fixed)

The one that mattered. On a thread containing a production key and a
customer name, the model produced:

```
advisory:          "Do not disclose the deployment credentials"
suggestedSummary:  "Successfully deployed Northwind using a prod API key from my laptop"
```

The summary names the customer and describes the credential use — in the
field explicitly meant to be safe to publish. The deterministic vetting
missed it because `scan_text` matches credential *shapes*, and that
sentence contains no `AKIA…` literal. **Scanners are structurally blind
to the semantic class the model was asked to find.**

Two layers were wrong, and the second is the more interesting:

1. The prompt asked for advisory "notes", so the model wrote *policy*
   ("do not disclose…") instead of a *finding* ("the customer Northwind
   Trading is named"). Policy gives nothing concrete to check against.
2. The first self-check asked a **judgment** question — "does this summary
   reveal anything flagged?" — and *both* llama3.2:3b and qwen3:14b
   answered no, defensibly, since no literal credential appears.

Fixed by asking for findings at layer 1 and by replacing judgment with
**extraction** at layer 2: "list every customer name, person, hostname,
credential or use of one, and unreleased plan that appears in the summary;
do not judge whether it is acceptable." A non-empty list is unambiguous,
and both models enumerate correctly. The withheld summary is reported via
a new `summaryVetting` field rather than silently dropped.

*Generalizable:* when a model must check its own work, ask it to
**enumerate**, not to **judge**. A yes/no question about acceptability
invites a defensible no.

### F2 — a member could set a privacy tier but not read it (fixed)

Slice 8 made the personal-channel tier member-choosable, but
`buzz channels get`/`list` returned only name, description, and
timestamps. The privacy floor — the whole control — was visible only in
the database. Both commands now carry `tier` and `visibility`.

*Generalizable:* a new control needs a read surface in the same slice.
The E2E caught this instantly because a test has to *observe* an outcome;
the implementation never needed to.

### F3 — advisory quality is model-dependent, and silence is ambiguous (documented)

`llama3.2:3b` returned an empty advisory list on one run and four notes on
the next for the same thread; `qwen3:14b` is consistent. An empty advisory
list from a weak model is indistinguishable from a genuinely clean thread.

The E2E now **warns rather than fails** on an empty list, and the review
event always records which model ran. **Recommendation: run the gate
assist on qwen3:14b or better.** `llama3.2:3b` is fine for the E2E's speed
but should not be a deployment's gate model.

## Decisions taken (please review)

### D-1 — promotion must satisfy the destination, and evidence beats declaration

You said promoted content "needs to comply to the destination privacy
setting". Implemented as: **content may only move into a channel no
stricter than the space it came from.** Strict → loose (the ordinary
promotion) stays open; loose → strict is refused, because a stricter
channel's guarantee is about what has already been allowed to leave.

Comparing *declared* tiers alone would have been theatre — a member could
flip their personal channel to `owned` and promote a second later. So
promotion also consults `model_usage`: if the source channel has actually
run inference on a backend the destination's tier forbids, the promotion
is refused regardless of what the tiers now say. Re-tiering changes what
happens next; it does not un-send what a vendor already saw.

**This is evidence, not proof.** It sees metered inference, not text
pasted in from elsewhere. It can refuse wrongly-optimistic movement; it
can never certify a channel as clean.

*Alternative you may prefer:* allow the movement and record the
provenance (a "this thread passed through an `open` space" marker on the
promoted thread), leaving the judgment to the receiving channel's admin.
That trades a hard stop for an audit trail.

### D-2 — a member may loosen their personal channel, not just tighten it

Direction is unconstrained: `owned → open` is allowed. The reasoning is
that a personal channel has one member who is also its owner, so there is
nobody to surprise, and the promotion rule above is what protects
everyone else. Team channels keep the absolute D26 immutability.

*Alternative you may prefer:* allow tightening only, so a personal
channel's history can never span a weaker tier. That is simpler to reason
about but means a member who wants vendor models for one afternoon must
create a new space and lose their thread history.

### D-3 — the gate review has no rate limit

Each review costs a GPU inference (~30 s on qwen3:14b), and a member can
queue several. Detaching the review from the ingest ack removed the
request-path stall but not the queueing. Left for the budget slice, where
per-member ceilings belong.

## Not covered here

- **The pre-turn tier gate and live-turn attribution** (slices 2 and 4)
  need the ACP harness and a live agent; they are exercised by the
  two-machine agent demo, not by this CLI-driven script.
- **A successful promotion graft.** A CLI-only run never pushes a real
  commit, so the checkpoint is a synthetic id (as in the Phase 2 script)
  and promotion stops at the git layer. The script asserts it cleared both
  privacy gates; the full graft is covered by `promote::s3_probe_tests`
  against live MinIO.

## Runbook notes learned the hard way

- **The destructive migration tests reset the dev schema.** Running
  `cargo test -p buzz-db --lib -- --ignored` wipes channels, members, and
  agent-owner rows. Bootstrap the testbed *after* the last destructive
  run, not before.
- **`pgrep`/`pkill` self-match.** A pattern that appears anywhere in your
  own command line kills your shell. Match on the process *name*
  (`pgrep -x`) and filter by `/proc/<pid>/exe`.
- **`/proc/<pid>/exe` reads `…/buzz-relay (deleted)`** after `cargo build`
  replaces the binary, so an exact-suffix match misses a running stray —
  and the stray still holds the metrics port, which makes the next relay
  panic at startup with `Address already in use`.
