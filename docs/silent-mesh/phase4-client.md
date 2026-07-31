# Phase 4 — the Swift macOS client

Phase 4 is the native macOS client (roadmap "Phase 4 — Swift macOS client
MVP"). It reads the workspace, writes to it, and updates live, all against
the real relay over Tailscale. Nothing here is mocked: every claim below
was made by running it from the MacBook against the WSL relay.

## What shipped

`macos/` holds five pieces. **68 tests — 55 run anywhere, 13 more only
against a live relay.** (Swift Testing's "Test run with N tests" line counts
skipped ones, so a green run without a relay is not 68 executed.)

| Piece | Responsibility |
|---|---|
| `MeshProtocol` | Nostr events, keys, kind registry, reconnecting WebSocket client |
| `MeshVault` | Secure-Enclave-wrapped identity at rest |
| `ThreadFold` | D41 work-thread state rebuilt from signed events |
| `MeshApprovalFold` | The supervised-approval decision queue |
| `MeshTurnFold` | Agent turn lifecycle, folded from cleartext reactions |
| `MeshApp` | SwiftUI shell — channels, threads, timeline, approvals, composer, refusals |

Three decisions worth keeping:

**The id is computed, never trusted.** `canonicalSerialization` writes
NIP-01's array by hand rather than through `JSONEncoder`, because the id is
a hash — key order, whitespace, and slash escaping all change it, and an
encoder that "helpfully" sorts keys produces events that verify nowhere.
`isValid()` recomputes the id *and* checks the signature over it; a valid
signature covering a different id would otherwise let a relay swap content
while keeping real cryptography. The suite grafts one event's signature
onto another to prove that path is closed.

**The relay is a distribution point, not a trust anchor.** Every event is
verified before it reaches the UI — on queries, and on live pushes — and
`publish` waits for the matching `OK` rather than assuming success. The
relay enforces authority, the privacy tier, and the D30 gate at ingest, so
"sent" and "accepted" are different outcomes a member needs told apart.
When the relay refuses, the app shows the relay's own words rather than a
generic failure.

**The kind mirror is tested against its source.** `KindParityTests` reads
`crates/buzz-core/src/kind.rs` and compares the integers. Drift there never
fails loudly at runtime: the client simply stops matching those events,
which looks like an empty channel rather than a bug.

## The relay client: one reader, and no gap between register and send

The client began with serialized exchanges — one request at a time, each
owning the socket until its reply arrived. That cannot express a live
subscription, which never completes: any query queued behind an open stream
waits forever. So a single reader task now owns `receive()` and
demultiplexes every frame to its waiter — by subscription id (`EVENT`,
`EOSE`, `CLOSED`), by event id (`OK`), or to the singleton challenge waiter
(`AUTH`).

That design has one rule, and it is the whole lesson of the slice:

> **Register the waiter in the same actor turn as the send.**

Sending first and registering after leaves a window in which the reply is
unclaimed, and the reader must discard it. The window looks impossibly
small and a relay on a LAN wins it routinely. Because an actor cannot
dispatch an incoming frame in the middle of a turn, doing both in one turn
does not *shrink* the gap — it removes it:

```swift
return try await withCheckedThrowingContinuation { continuation in
    pendingQueries[sub] = PendingQuery(continuation: continuation)
    sendFireAndForget(["REQ", sub, filter.jsonObject()])
}
```

**The bug this cost.** The relay sends its NIP-42 challenge the instant the
socket opens — before `authenticate()` has registered anyone. The reader
found no waiter, dropped the frame, and `authenticate()` then waited for a
challenge that had already come and gone, until the relay's own timer
closed the connection. It surfaced as "NIP-42 auth timeout", which points
at the relay.

The app lost that race every time; the tests never did. SwiftUI puts
main-actor work between `connect()` and `authenticate()`, while the tests
call them back to back and beat the network. **A green suite and an app
that could not connect at all, from nothing but scheduling.** An unclaimed
challenge is now buffered, so arriving early is not the same as being lost.

**Two of my own fixes were wrong, and both are worth remembering:**

- *Ping-before-read deadlocked.* Waiting for a pong before reading looked
  tidier than retrying — but the relay does not answer client pings, so the
  wait never ended. A race traded for a deadlock. The warm-up retry is back,
  bounded to *before the first frame*: after that, an error means the
  connection really died and every waiter must be told (`failAllPending`),
  not left hoping.
- *The live-subscription test could hang instead of fail.* Its deadline was
  checked inside `for await`, so it could only fire when an event arrived —
  meaning "nothing ever arrives", the exact failure the test exists to
  catch, would block the suite rather than report it. The deadline is now a
  racing task. **A wait whose escape hatch depends on the thing being waited
  for is not a timeout.**

**What actually found it was instrumentation, not reasoning.** Three rounds
of theorising produced nothing; one log line — `reader stopped after 1
frame(s)` — said the socket opened, took one frame, and died. That is now
permanent behind `MESH_LOG=1`.

A related trap is worth stating once: an earlier concurrency test written
to prove the reentrancy fix **also passed without the fix**. A test whose
outcome does not depend on the behaviour it names proves only that the code
runs. Check that a new test fails against the old code before trusting it.

## Supervised approvals (2026-07-31)

The roadmap's Phase 4 exit criterion names this explicitly — "a two-person
team + one agent runs a real session entirely from the macOS app, **including
granting a supervised approval**" — so it is the one client surface the phase
cannot exit without.

The whole lifecycle is already on the wire, which is why the client needs no
new HTTP:

| Event | Author | Carries |
|---|---|---|
| kind:46010 request | relay | `d` = token hash, `h` = channel, `p` = agent; content is the summary |
| kind:46030/46031 decision | the member | `d` = token hash, content = optional note |
| kind:46011/46012 outcome | relay | `d` = token hash, `e` = the member's command, approver in content |

So `MeshApprovalFold` rebuilds the decision queue from signed events exactly
as `MeshFold` rebuilds work threads. Four things it has to get right, none of
which are obvious from the kind numbers:

**The `d` tag is a hash, not the token.** The member references a request
without ever holding the secret that authorizes it; the relay resolves the
hash and checks membership itself. (The older *workflow* approval path puts
the raw token in event content. The agent path deliberately does not, and
mixing them up would leak one.)

**Kind 46010 is shared between two domains.** Workflow gates and agent
permission requests use the same integer, discriminated only by a `domain`
field inside the JSON content. Folding on the kind alone renders a workflow
gate as an agent request with every field blank — and offers an Approve
button that cannot execute. The fold reads `domain` and drops the rest.

**Expiry is the client's job.** The relay emits nothing when a request
lapses on its own, so a client folding outcome events alone would show dead
requests as live decisions forever; the fold expires them against the
deadline the request carries. (Agent *withdrawal* used to be silent too —
that one is now announced as kind:46013, below.)

**Both RFC 3339 shapes must parse.** `chrono`'s `to_rfc3339()` emits
sub-second digits whenever the value has any — `Utc::now()` always does — and
omits them when it does not. `ISO8601DateFormatter` accepts one shape or the
other, never both, so a single formatter drops a deadline it cannot read, and
an unparsed deadline silently becomes *no* deadline. The mutation test for
this failed two other tests as a side effect, which is what that class of bug
looks like: the parse never complains, the *rule* downstream quietly stops
applying.

### Proven live, not just unit-tested

Three gated tests run the real round trip: the Swift test signs NIP-98 and
registers a request over `POST /api/approvals` exactly as the ACP harness
does, then a second identity decides it over the WebSocket.

```
✔ a member reads the relay's request and grants it
✔ a denial is recorded as a denial, not merely as 'not granted'
✔ an agent cannot grant its own request
```

The last is the rule the word "supervised" rests on, so it asserts the
refusal *and* that the relay's reason is a sentence a member can read
("an agent cannot decide its own permission request"). Cross-checked against
the relay's own rows: one `granted`, one `denied`, and the self-approval
attempt still `pending`.

Unit tests alone could not have caught the timestamp or the domain trap —
both are properties of what the *relay* emits, not of what the client
believes.

### Driving the real UI found two things tests did not

Approving from the app is the exit criterion, so the button was clicked
through the accessibility API against the live relay, and the relay's own
rows checked before and after: pending went 4 → 3 and that exact request
became `granted`. Two findings came out of doing it rather than reasoning
about it.

**The fold ordering was non-deterministic.** Two captures of an unchanged
channel, from two launches, listed the same four threads in two different
orders. Both folds build a dictionary and sort on a second-granularity
timestamp — and `Dictionary.values` has no defined order while Swift's
`sort` is not stable, so anything sharing a second is free to swap. On the
thread list that reads as the workspace rearranging itself. **On the
approval queue it is worse than cosmetic: a card can move between the moment
a member reads it and the moment they click**, and the three live tests
create their requests 150 ms apart — all in the same second. Both folds now
break ties on the id.

The first test written for it *passed with the bug in place*: re-folding the
same events inside one process gives the same dictionary order whether or
not the tie-break exists, so self-consistency proved nothing. The cross-
process variation that caused the symptom cannot be reproduced in a single
test process, so the tests now assert the **specified** order instead. Both
were then mutation-checked. Verified against the symptom too: three launches
now produce byte-identical windows apart from the live expiry countdown.

**The decision buttons publish no accessible name.** An accessibility dump
of the approval cards shows four buttons whose only description is
"button" — no `AXTitle`, no `AXDescription`. A screen-reader user is asked
to approve something they were never told, and two cards on screen are
indistinguishable. `.accessibilityLabel("Approve: <detail>")` is now set on
both buttons; it did not make a name readable through System Events, so
whether it reaches VoiceOver is **unverified** and worth checking with
Accessibility Inspector on the Mac.

### Withdrawal, now announced (kind:46013)

`POST /api/approvals/resolve` lets the requesting agent take its own request
back. It used to update the row and emit **nothing**, unlike the decision
path beside it which carefully writes a kind:46011/46012 record — so a
client folding from events kept showing a dead request as actionable, and a
member who clicked got a refusal instead of an action.

The relay now emits a relay-signed **kind:46013** carrying the same `d`
token-hash handle as the rest of the series, so the fold retires the card
without needing to know anything new. Its content names the relay's own
reason (`cancelled` / `expired`) and the client repeats it rather than
inventing one, because "the agent took it back" and "it lapsed" read
differently to whoever was about to approve it.

Adding to that numeric block has one non-obvious obligation:
`is_workflow_execution_kind` is a **range** ending at the last approval
kind, and its job is stopping execution events from triggering workflows.
Extend the range with the kind, or the newcomer becomes the one execution
event that *can* trigger a workflow — the exact loop the guard exists to
prevent.

Proven by the order of operations rather than by assertion alone: the new
live test was run first against the **old** relay still in memory, where the
withdrawn request stayed `pending` and actionable — the gap, reproduced —
and then against the rebuilt relay, where it retires.

## Agent turns, the cleartext half (2026-07-31)

The roadmap's remaining Phase 4 UI item is "agent interaction (streaming,
approvals, per-turn diffs)". Approvals shipped. This is the part of
*streaming* that needs no streaming — and, it turns out, the only part an
ordinary member can see at all.

A turn broadcasts its lifecycle in cleartext, as reactions on the message
that woke it:

| Event | Meaning | Shape |
|---|---|---|
| kind:7 `👀` | queued — an agent took it | `e` = **the message**, content = emoji |
| kind:7 `💬` | the model is being prompted | same |
| kind:5 | that reaction withdrawn | `e` = **the reaction event** |
| kind:9 from the agent | the answer | NIP-10 `e` back at the trigger |
| kind:9 opening `⚠️` | the harness refused | replaces the answer entirely |

`MeshTurnFold` folds those into `queued → working → answered | refused |
ended`. Three things it must get right:

**Ending a turn is a two-hop correlation.** The kind:5 names the *reaction*,
not the message. A fold that matched deletions against the message id finds
nothing and shows every turn as permanently working. Both directions are
mutation-tested.

**The answer outranks the reactions.** `clear_reactions` is fire-and-forget
and may lag or be lost, so waiting for the 💬 to disappear before showing an
answer that has already arrived leaves the turn reading as "working" forever.

**A `⚠️` reply is a distinct state.** On the Silent Mesh tier-gate paths it is
the *only* thing a turn emits — no answer, no metrics — so rendering it as an
ordinary chat line buries the one message explaining why nothing happened.

### Turn state is live-only, and the relay forces that

Neither kind:7 nor kind:5 carries an `h` tag. They still reach a
channel-scoped subscription, because the relay falls back to the stored
`channel_id` when an event has no `h` tag at all
(`buzz-core/src/filter.rs`) — a fallback whose comment names these two kinds.

But the relay honours NIP-09 by **soft-deleting** the reaction: `deleted_at`
is set on the kind:7 row, and every query filters `deleted_at IS NULL`
(`buzz-db/src/event.rs`). So the instant a turn ends its 👀/💬 stop coming
back from queries, and the kind:5 that retired them names an event nobody can
fetch any more.

**A completed turn is therefore not reconstructible from a query.** Only a
client that watched it happen holds the reaction ids needed to correlate the
ending. The app accumulates turn events from the live stream and re-folds
locally; it never re-queries them. This was found by writing the live test
the obvious way — assert `.answered` from a fresh query — and watching it
fail against a relay that had done nothing wrong. The suite now asserts the
*absence*: a cold query yields no turn once it has finished.

That is the right shape anyway. Turn state is what is happening now; the
durable record of a turn is its answer, which is an ordinary message in the
timeline.

### What a member still cannot see

Real streaming — token-by-token text, thoughts, tool calls — is kind:24200,
which is ephemeral (never stored), NIP-44-encrypted to the agent's **owner**,
p-gated, and published with no channel scope. A Mac signed in as an ordinary
channel member sees none of it regardless of what it subscribes to. Reaching
it is a deployment decision (bind the agent's owner pubkey to this member's
key), not a client change. Same for kind:44200/44201 turn cost.

Per-turn **diffs** are further off than they look, and not for client
reasons: `ThreadWorktrees`/`ensure_worktree` have **no callers outside
`worktree.rs`**, so there are no per-turn commits yet — `commit^..commit`
does not yet mean "what the agent changed this turn". (Kinds for carrying a
patch do already exist — `KIND_GIT_PATCH` 1617 and `KIND_STREAM_MESSAGE_DIFF`
40008 — so this needs wiring, not a new kind.)

## Reconnect (2026-07-31)

A dropped tailnet route used to be permanent: the reader told every waiter
the socket had died, ended each subscription's stream, and the app's
`for await` loop simply returned. Nothing ever asked again, so the window
kept showing a workspace that had stopped updating — the worst shape of
failure, because it looks exactly like a quiet channel.

The fix rests on one distinction:

> A query or a publish is a **request**. A subscription is a **standing
> intent**.

So one-shot waiters still fail with the socket — only the caller can decide
whether repeating a publish is safe, and a client that retried on its own
would be deciding to send a member's message twice. Live subscriptions are
kept, and the client re-opens, re-authenticates, and re-sends their REQs.

Three details that are easy to get wrong, and each of which silently loses
data rather than erroring:

- **Re-authenticate.** A new socket starts unauthenticated. Skip it and
  every private channel comes back empty, which reads as "nothing here"
  rather than "not allowed".
- **Replay from the gap, not from now.** The re-sent REQ sets `since` to the
  last event that subscription delivered. It is inclusive, so the last event
  usually arrives twice — deliberately, because a duplicate is visible to a
  client that dedups and a gap is visible to nobody.
- **Clear `limit`.** Live subscriptions are opened with `limit: 0` ("no
  history, I already loaded it"). Carry that into the replay and the relay
  sends nothing stored — so the replay exists but returns exactly zero
  events. This one passes every test that only checks the stream survived.

`disconnect()` is final: `wantsConnection` goes false, and a cancelled
reader returns instead of reporting a transport failure. Without that check
the deliberate close was immediately overwritten by the socket error it
caused, so the state named a network fault for something the app asked for.

The app mirrors `MeshConnectionState` into its status line and the sidebar
dot — previously that dot was green unless the status *text* contained
"failed", which is green beside a stale workspace. On recovery it also
re-reads the channel list and folds: the subscription replays its own gap,
but the one-shot queries behind the channel list have nothing replaying them.

**Proven against a real outage, not a mock.** `simulateTransportFailure()`
drops the transport under a live subscription; an event published *during*
the outage still arrives. Three mutants confirm the test can fail: ending
subscriptions on failure, keeping `limit: 0`, and replaying from now. Then
the same thing end to end — the dev relay was killed under the running app,
which backed off 0.5s → 15s across ten attempts, reconnected by itself, and
re-read the workspace.

## Live interop (2026-07-31)

From the MacBook, over Tailscale, against the WSL relay:

```
✔ a Swift-signed event is accepted and read back by the relay
✔ work-thread events come back and fold into threads
✔ the relay's own tier stamp is readable from the channel metadata
✔ two overlapping queries each get their own results
✔ a live subscription delivers an event published after it started
✔ queries still work while a subscription is open
```

A Swift-signed `kind:9` was accepted by the Rust verifier, survived
Postgres and JSON, and still verified against its own id on the way back.
That is the strongest single statement that the two implementations agree,
and it is the roadmap's stated Phase 4 dependency ("MeshProtocol validated
against Buzz's conformance suite and interop E2E").

The live-delivery test publishes from a **second connection**, so what it
proves is relay fan-out rather than the client hearing its own echo on the
socket it wrote to. End to end, two messages published from the WSL host
appeared in an untouched Mac window.

```bash
MESH_RELAY_URL=ws://<relay> MESH_PRIVATE_KEY=<64-hex> MESH_CHANNEL=<uuid> \
  swift test            # gated; without MESH_RELAY_URL only the pure tests run
MESH_LOG=1 …            # transport diagnostics to stderr
```

## MeshVault — verified on hardware (2026-07-31)

`macos/MeshProtocol/Sources/MeshVault`. The identity is sealed at rest by
a **non-extractable P-256 key generated inside the Secure Enclave**; the
enclave never holds the identity itself (it does P-256 only, and a Nostr
identity is secp256k1), so it wraps rather than stores. Encryption uses
the enclave key's public half — saving needs no prompt — while decryption
uses the private half, so unlocking does.

Getting there took three separate gates, each of which reports the same
useless error (`-34018`) and so is easy to mistake for the previous one:

| Gate | Symptom | Fix |
|---|---|---|
| Ad-hoc signature | ephemeral enclave keys work, persisting fails | a real signing identity |
| No team identifier | still -34018 once signed | `DEVELOPMENT_TEAM` (pinned in `project.yml`, since `xcodegen` rewrites the project) |
| No keychain access group | still -34018 with a team | `keychain-access-groups` entitlement **plus** `kSecUseDataProtectionKeychain: true` |

The last two must land together: an enclave key lives in the
data-protection keychain and must belong to an access group the app is
entitled to. Without the flag the call targets the legacy file keychain,
which cannot hold one at all.

**The relaunch test.** Sealing and unlocking in one process proves the
wrapping works and nothing about being locked at rest — macOS can treat a
key's creator as already authenticated, so it may never prompt. The claim
the roadmap makes ("the app relaunches locked") is cross-process, so the
self-test runs as two launches:

```
$ MESH_VAULT_SELFTEST=presence-seal   …/MeshApp   # process A
vault: seal: ok
$ MESH_VAULT_SELFTEST=presence-unlock …/MeshApp   # process B, fresh
vault: unlock: ok — identity round-tripped in 6.9s
vault: result: PASS
```

Process B showed the system prompt — *"Silent Mesh is trying to Unlock
your Silent Mesh identity"* — and 6.9 s is a human reaching for the
sensor. The elapsed time is reported precisely because a cached grant
returns instantly, and pass/fail alone could not tell the two apart.

`.userPresence` rather than `.biometryCurrentSet`: Touch ID *or* the login
password. A member whose finger is not read should be inconvenienced, not
locked out of their workspace.

With `MESH_VAULT=1` the app imports an environment key into the vault on
first launch and never reads it again; later launches have no identity
until the member unlocks. That is the difference between an app handed a
secret and an app that keeps one.

## What still needs a human

- **Only vault work.** A keychain-held signing key is unreachable from an
  SSH session (`errSecInternalComponent`, and `security` reports "User
  interaction is not allowed") — unlock state is per-session, so unlocking
  in a console Terminal does not carry over.

  But that gate is *codesigning*, not compiling, and only the Secure Enclave
  needs a real signature. Building ad-hoc skips it entirely:

  ```bash
  xcodebuild -project MeshApp.xcodeproj -scheme MeshApp -configuration Debug \
    -derivedDataPath /tmp/meshapp-adhoc \
    CODE_SIGNING_ALLOWED=NO CODE_SIGN_IDENTITY="" build
  ```

  The result launches, connects, renders, and can be driven and captured
  headlessly. So everything except vault behaviour — which genuinely needs
  the enclave, hence a real identity — no longer needs a human at the Mac.
  A signed `xcodebuild` from a Terminal is still required before testing
  `MESH_VAULT=1`.

  One trap on repeat builds: the swift-secp256k1 build-tool plugin copies
  its sources in as mode `444`, and the next build's `cp` fails with
  `Permission denied` — 21 of them, and not one mentions Swift. Clear
  `DerivedData/…/BuildToolPluginIntermediates` rather than hunting a
  compile error that is not there.
- **Nothing else.** `macos/scripts/ui-select.sh` selects rows and captures
  the window without a person present. Two traps it encodes: `click at`
  does not change a SwiftUI `List` selection (three byte-identical
  screenshots is how that was found — set `selected` via the accessibility
  API instead), and the window moves, so its position must be read on every
  call rather than cached.

  Two more, learned driving the approval buttons. **Index by class, not by
  position**: `UI element 5` and `group 3` are the same node, because groups
  are interleaved with splitters — asking for `group 5` errors with "Invalid
  index" and reads like the pane is missing. And **`entire contents of
  window 1` finds nothing** in this app; navigate the explicit path
  (`scroll area 1 of group 3 of splitter group 1 of group 1`) instead.

- **The app's own `MESH_SNAPSHOT` self-capture is not trustworthy for this
  UI.** It prefers the PDF display list whenever that exceeds 20 KB, and a
  near-blank render of a `NavigationSplitView` clears that bar easily — it
  produced a 23 KB image containing two text fields and nothing else. Size
  is a poor proxy for "did it draw". Prefer `screencapture -l<window-id>`,
  which needs Screen Recording (granted to sshd) but shows what is actually
  on screen.

## Suggested next slices

1. **Per-turn agent output** — streaming turns and diffs. Approvals landed;
   what the agent *did* between them is still only visible as work-thread
   checkpoints.
2. **The channel repo browser** — file tree and blob view over git smart
   HTTP, so a work thread's checkpoints can be read where they happened.
3. **Agent turn liveness** — the harness already publishes 👀/💬 reactions
   and deletes them when a turn ends; adding kinds 7/5 to the live filter is
   a small change with a visible payoff.
