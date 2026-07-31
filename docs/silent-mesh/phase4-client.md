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

## What is blocked, and on what

- **`MeshVault`** (SE-wrapped master key, biometric/PIN unlock) needs
  entitlements and a signed build. Xcode is now installed, so this is
  unblocked — but note it cannot be *proven* over SSH: biometric unlock
  needs a human at the machine.
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
