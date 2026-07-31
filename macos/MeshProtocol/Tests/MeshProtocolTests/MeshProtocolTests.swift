import Foundation
import Testing

@testable import MeshProtocol

// MARK: - Event identity and signatures

@Test("an event's id is the hash of its canonical form, not a field we trust")
func eventIDIsDerived() throws {
    let keys = try MeshKeys(
        privateKeyHex: "0000000000000000000000000000000000000000000000000000000000000001")
    var event = MeshEvent(
        pubkey: keys.publicKeyHex,
        createdAt: 1_700_000_000,
        kind: MeshKind.chatMessage,
        tags: [["h", "5b1e7c05-0000-4000-8000-000000000000"]],
        content: "hello mesh"
    )
    try event.sign(with: keys)

    #expect(event.id == (try event.computedID()))
    #expect(event.isValid())

    // Content tampering invalidates the id, so a relay cannot swap what an
    // event says while keeping its real signature.
    var tampered = event
    tampered.content = "hello mesh!"
    #expect(tampered.isValid() == false)

    // ...and neither can it keep the id while changing the author.
    var reauthored = event
    reauthored.pubkey = String(repeating: "ab", count: 32)
    #expect(reauthored.isValid() == false)
}

@Test("a valid signature over a different id is still rejected")
func signatureMustCoverThisEvent() throws {
    let keys = try MeshKeys()
    var first = MeshEvent(
        pubkey: keys.publicKeyHex, createdAt: 1_700_000_000, kind: MeshKind.chatMessage,
        tags: [], content: "one")
    var second = MeshEvent(
        pubkey: keys.publicKeyHex, createdAt: 1_700_000_000, kind: MeshKind.chatMessage,
        tags: [], content: "two")
    try first.sign(with: keys)
    try second.sign(with: keys)

    // Graft a genuine signature from one event onto the other: both halves
    // of `isValid` are needed to catch this.
    var spliced = second
    spliced.sig = first.sig
    #expect(spliced.isValid() == false)
}

@Test("canonical serialization is exactly NIP-01's array, unescaped")
func canonicalForm() throws {
    let event = MeshEvent(
        pubkey: String(repeating: "11", count: 32),
        createdAt: 1_700_000_000,
        kind: 1,
        tags: [["e", String(repeating: "22", count: 32)]],
        content: "a/b \"quoted\""
    )
    let text = String(data: try event.canonicalSerialization(), encoding: .utf8)!
    #expect(text.hasPrefix("[0,\"1111"))
    // Slashes must not be escaped — an escaped one changes the hash.
    #expect(text.contains("a/b"))
    #expect(text.contains("\\\"quoted\\\""))
}

@Test("hex round-trips and rejects malformed input")
func hexHelpers() {
    #expect(MeshHex.encode([0x00, 0x0f, 0xff]) == "000fff")
    #expect(MeshHex.decode("000fff") == [0x00, 0x0f, 0xff])
    #expect(MeshHex.decode("abc") == nil)  // odd length
    #expect(MeshHex.decode("zz") == nil)  // not hex
}

@Test("keys derive a stable x-only pubkey")
func keyDerivation() throws {
    let hex = "0000000000000000000000000000000000000000000000000000000000000001"
    let a = try MeshKeys(privateKeyHex: hex)
    let b = try MeshKeys(privateKeyHex: hex)
    #expect(a.publicKeyHex == b.publicKeyHex)
    #expect(a.publicKeyHex.count == 64)
    #expect(throws: MeshProtocolError.self) { try MeshKeys(privateKeyHex: "beef") }
}

// MARK: - Tier semantics

@Test("tier strictness matches the relay's promotion rule")
func tierStrictness() {
    #expect(MeshChannelTier.owned.isAtLeastAsStrict(as: .open))
    #expect(MeshChannelTier.owned.isAtLeastAsStrict(as: .private))
    #expect(MeshChannelTier.private.isAtLeastAsStrict(as: .open))
    for tier in MeshChannelTier.allCases {
        #expect(tier.isAtLeastAsStrict(as: tier))
    }
    // Loose into strict is what promotion refuses.
    #expect(MeshChannelTier.open.isAtLeastAsStrict(as: .owned) == false)
    #expect(MeshChannelTier.open.isAtLeastAsStrict(as: .private) == false)
    #expect(MeshChannelTier.private.isAtLeastAsStrict(as: .owned) == false)
}

// MARK: - Registry parity with the Rust source of truth

/// The client's kind mirror must agree with `crates/buzz-core/src/kind.rs`.
///
/// Drift here does not fail loudly at runtime: the client simply never
/// matches those events, which looks like an empty channel rather than a
/// bug. So the test reads the Rust registry itself instead of restating the
/// numbers, and any renumbering upstream fails the Swift build.
@Test("kind constants match the Rust registry")
func kindParity() throws {
    let here = URL(fileURLWithPath: #filePath)
    let repoRoot = here
        .deletingLastPathComponent()  // MeshProtocolTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // MeshProtocol
        .deletingLastPathComponent()  // macos
        .deletingLastPathComponent()  // repo root
    let kindRS = repoRoot.appendingPathComponent("crates/buzz-core/src/kind.rs")
    guard let source = try? String(contentsOf: kindRS, encoding: .utf8) else {
        // Running outside a checkout (e.g. a packaged build) — nothing to
        // compare against, and inventing a pass would defeat the point.
        Issue.record("kind.rs not found at \(kindRS.path); parity unverified")
        return
    }

    func rustKind(_ name: String) -> Int? {
        guard let range = source.range(of: "pub const \(name): u32 = ") else { return nil }
        let tail = source[range.upperBound...]
        let digits = tail.prefix { $0.isNumber }
        return Int(digits)
    }

    let expected: [(String, Int)] = [
        ("KIND_WORK_THREAD_OPEN", MeshKind.workThreadOpen),
        ("KIND_WORK_THREAD_METADATA", MeshKind.workThreadMetadata),
        ("KIND_WORK_THREAD_STATE", MeshKind.workThreadState),
        ("KIND_WORK_THREAD_RECOMMEND", MeshKind.workThreadRecommend),
        ("KIND_WORK_THREAD_CHECKPOINT", MeshKind.workThreadCheckpoint),
        ("KIND_WORK_THREAD_OVERDUE", MeshKind.workThreadOverdue),
        ("KIND_WORK_THREAD_CANON", MeshKind.workThreadCanon),
        ("KIND_WORK_THREAD_SIBLING_ARCHIVED", MeshKind.workThreadSiblingArchived),
        ("KIND_WORK_THREAD_PROMOTED", MeshKind.workThreadPromoted),
        ("KIND_WORK_THREAD_FORK", MeshKind.workThreadFork),
        ("KIND_WORK_THREAD_PROMOTE", MeshKind.workThreadPromote),
        ("KIND_WORK_THREAD_GATE_REVIEW", MeshKind.workThreadGateReview),
        ("KIND_WORK_THREAD_GATE_REVIEWED", MeshKind.workThreadGateReviewed),
        ("KIND_AGENT_TURN_METRIC", MeshKind.agentTurnMetric),
        ("KIND_AGENT_TURN_ATTRIBUTION", MeshKind.agentTurnAttribution),
    ]
    for (rustName, swiftValue) in expected {
        let rustValue = rustKind(rustName)
        #expect(rustValue == swiftValue, "\(rustName): rust=\(rustValue as Int?) swift=\(swiftValue)")
    }
}

@Test("relay-only kinds are recognised as such")
func relayOnlyKinds() {
    #expect(MeshKind.relayOnly.contains(MeshKind.workThreadGateReviewed))
    #expect(MeshKind.relayOnly.contains(MeshKind.workThreadOverdue))
    #expect(MeshKind.relayOnly.contains(MeshKind.workThreadGateReview) == false)
    #expect(MeshKind.isWorkThread(MeshKind.workThreadGateReviewed))
    #expect(MeshKind.isWorkThread(MeshKind.chatMessage) == false)
}

// MARK: - Filters

@Test("filters serialize tag constraints in Nostr's '#tag' form")
func filterShape() {
    let filter = MeshFilter(
        kinds: [MeshKind.chatMessage], limit: 20, tags: ["#h": ["chan-uuid"]])
    let object = filter.jsonObject()
    #expect(object["kinds"] as? [Int] == [MeshKind.chatMessage])
    #expect(object["limit"] as? Int == 20)
    #expect(object["#h"] as? [String] == ["chan-uuid"])
    #expect(object["authors"] == nil)
}
