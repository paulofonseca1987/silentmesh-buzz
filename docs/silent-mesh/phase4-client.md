# Phase 4 — the Swift macOS client

Phase 4 is the native macOS client (roadmap "Phase 4 — Swift macOS client
MVP"). It reads the workspace, writes to it, and updates live, all against
the real relay over Tailscale. Nothing here is mocked: every claim below
was made by running it from the MacBook against the WSL relay.

## What shipped

`macos/` holds four pieces. 33 tests — 27 pure, 6 gated on a live relay.

| Piece | Responsibility |
|---|---|
| `MeshProtocol` | Nostr events, keys, kind registry, WebSocket relay client |
| `MeshVault` | Secure-Enclave-wrapped identity at rest |
| `ThreadFold` | D41 work-thread state rebuilt from signed events |
| `MeshApp` | SwiftUI shell — channels, threads, timeline, composer, refusals |

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

- **Signed builds.** A keychain-held signing key is unreachable from an SSH
  session (`errSecInternalComponent`, and `security` reports "User
  interaction is not allowed") — unlock state is per-session, so unlocking
  in a console Terminal does not carry over. Each Swift change therefore
  needs one `xcodebuild` run from a Terminal on the Mac. Everything after
  that — running, testing, screenshotting, driving the UI — is headless,
  because Screen Recording *and* Accessibility are granted to sshd.
- **Nothing else.** `macos/scripts/ui-select.sh` selects rows and captures
  the window without a person present. Two traps it encodes: `click at`
  does not change a SwiftUI `List` selection (three byte-identical
  screenshots is how that was found — set `selected` via the accessibility
  API instead), and the window moves, so its position must be read on every
  call rather than cached.

## Suggested next slices

1. **Agent interaction** — approvals and per-turn diffs, where the client
   meets the ACP harness. The largest remaining gap between the Mac client
   and the desktop app, and the first place the client does more than
   observe.
2. **The channel repo browser** — file tree and blob view over git smart
   HTTP, so a work thread's checkpoints can be read where they happened.
3. **Reconnect** — the reader tells every waiter when the socket dies, but
   nothing yet re-establishes it. A dropped tailnet route currently means
   restarting the app.
