# Phase 4 — Swift macOS client: foundation shipped, app blocked on tooling

Phase 4 is the native macOS client (roadmap "Phase 4 — Swift macOS client
MVP"). Its first piece — `MeshProtocol` — is shipped and validated against
a live relay. The rest is bounded by what the machine can build, not by
design questions.

## What shipped

`macos/MeshProtocol`, an SPM library with no UI and no entitlements. That
scope is deliberate: it is the half that can be built, tested, and proven
headlessly over SSH, and it is what every later surface (vault, UI, sync)
sits on. 11 tests, 9 pure and 2 live.

| Type | Responsibility |
|---|---|
| `MeshEvent` | Nostr event; canonical serialization, derived id, verification |
| `MeshKeys` | secp256k1 identity; BIP-340 signing (the vault will replace it) |
| `MeshKind` / `MeshChannelTier` | Registry mirror + the tier strictness rule |
| `MeshRelayClient` | WebSocket actor: NIP-42 auth, publish-with-OK, query-to-EOSE |

Three decisions worth keeping:

**The id is computed, never trusted.** `canonicalSerialization` writes
NIP-01's array by hand rather than through `JSONEncoder`, because the id is
a hash — key order, whitespace, and slash escaping all change it, and an
encoder that "helpfully" sorts keys produces events that verify nowhere.
`isValid()` recomputes the id *and* checks the signature over it; a valid
signature covering a different id would otherwise let a relay swap content
while keeping real cryptography. The suite grafts one event's signature
onto another to prove that path is closed.

**The relay is a distribution point, not a trust anchor.** Query results
are verified before they are returned, and `publish` waits for the matching
`OK` rather than assuming success — the relay enforces authority, the
privacy tier, and the D30 gate at ingest, so "sent" and "accepted" are
different outcomes a member needs told apart.

**The kind mirror is tested against its source.** `KindParityTests` reads
`crates/buzz-core/src/kind.rs` and compares the integers. Drift there never
fails loudly at runtime: the client simply stops matching those events,
which looks like an empty channel rather than a bug.

## Live interop (2026-07-31)

From the MacBook, over Tailscale, against the WSL relay:

```
✔ a Swift-signed event is accepted and read back by the relay
✔ the relay's own tier stamp is readable from the channel metadata
```

A Swift-signed `kind:9` was accepted by the Rust verifier, survived
Postgres and JSON, and still verified against its own id on the way back.
That is the strongest single statement that the two implementations agree,
and it is the roadmap's stated Phase 4 dependency ("MeshProtocol validated
against Buzz's conformance suite and interop E2E").

```bash
MESH_RELAY_URL=ws://<relay> MESH_PRIVATE_KEY=<64-hex> MESH_CHANNEL=<uuid> \
  swift test            # gated; without MESH_RELAY_URL only the 9 pure tests run
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

With `MESH_VAULT=1` the app now imports an environment key into the vault
on first launch and never reads it again; later launches have no identity
until the member unlocks. That is the difference between an app handed a
secret and an app that keeps one.

## What is blocked, and on what

- **Signed builds still need a human.** A keychain-held signing key is
  unreachable from an SSH session (`errSecInternalComponent`, and
  `security` reports "User interaction is not allowed") — unlock state is
  per-session, so unlocking in a console Terminal does not carry over.
  Swift changes therefore need one `xcodebuild` run from a Terminal on the
  Mac; everything after that (running, testing, screenshotting) works
  headlessly.
- **The SwiftUI app** needs an Xcode project. Recommend `xcodegen`
  (`brew install xcodegen`) so the project is a YAML file that can be
  edited and regenerated deterministically; hand-editing `project.pbxproj`
  over SSH is a silent-corruption risk.
- **Anything visual.** `screencapture` over SSH only works against an
  attached console session. The UI half needs either a human looking or a
  logged-in unlocked session.

## Suggested next slices

1. **`MeshVault`** — Secure Enclave key wrapping with a software fallback,
   so the protocol layer stops holding raw secrets. Testable except for the
   biometric prompt.
2. **The event fold** — channel list, message timeline, and work-thread
   state folded from signed events client-side, mirroring what
   `buzz threads show` does. Pure, fully testable, and the real content of
   "the client understands the workspace".
3. **The app shell** — only after 1 and 2, since both are provable without
   a window.
