import Foundation
import Testing

@testable import MeshProtocol

/// Interop against a **live** Silent Mesh relay — the Phase 4 dependency
/// the roadmap names ("MeshProtocol validated against Buzz's conformance
/// suite and interop E2E").
///
/// Everything else in this package is pure and proves the client agrees
/// with itself. Only this proves it agrees with the relay: that the
/// canonical serialization produces ids the Rust verifier accepts, that
/// NIP-42 auth is answered correctly, and that a signed event survives the
/// round trip byte-for-byte.
///
/// Gated, because it needs infrastructure:
///   MESH_RELAY_URL=ws://100.72.140.59:3999 \
///   MESH_PRIVATE_KEY=<64-hex> \
///   MESH_CHANNEL=<uuid> \
///     swift test --filter Interop
@Suite("Interop", .enabled(if: ProcessInfo.processInfo.environment["MESH_RELAY_URL"] != nil))
struct RelayInteropTests {
    private func makeClient() throws -> (MeshRelayClient, MeshKeys, String) {
        let env = ProcessInfo.processInfo.environment
        guard let urlString = env["MESH_RELAY_URL"], let url = URL(string: urlString) else {
            throw MeshProtocolError.malformed("MESH_RELAY_URL missing")
        }
        guard let keyHex = env["MESH_PRIVATE_KEY"] else {
            throw MeshProtocolError.malformed("MESH_PRIVATE_KEY missing")
        }
        guard let channel = env["MESH_CHANNEL"] else {
            throw MeshProtocolError.malformed("MESH_CHANNEL missing")
        }
        let keys = try MeshKeys(privateKeyHex: keyHex)
        return (MeshRelayClient(url: url, keys: keys), keys, channel)
    }

    @Test("a Swift-signed event is accepted and read back by the relay")
    func roundTrip() async throws {
        let (client, keys, channel) = try makeClient()
        try await client.connect()
        try await client.authenticate()
        #expect(await client.isAuthenticated)

        let marker = "mesh-protocol interop \(UUID().uuidString)"
        var event = MeshEvent(
            pubkey: keys.publicKeyHex,
            createdAt: Int64(Date().timeIntervalSince1970),
            kind: MeshKind.chatMessage,
            tags: [["h", channel]],
            content: marker
        )
        try event.sign(with: keys)
        // If the Rust verifier disagrees with our id or signature, this is
        // where it shows — the relay refuses with a stated reason rather
        // than accepting a subtly wrong event.
        let id = try await client.publish(event)
        #expect(id == event.id)

        let found = try await client.query(
            MeshFilter(kinds: [MeshKind.chatMessage], limit: 20, tags: ["#h": [channel]])
        )
        let mine = found.first { $0.content == marker }
        #expect(mine != nil, "published event did not come back from the relay")
        // Round-tripped through Postgres and JSON, it must still verify
        // against its own id — the strongest single statement that the two
        // implementations agree.
        #expect(mine?.isValid() == true)
        #expect(mine?.id == event.id)
        #expect(mine?.pubkey == keys.publicKeyHex)

        await client.disconnect()
    }

    /// Diagnostic: what does the work-thread query actually return, and
    /// does each event survive verification? A client that silently drops
    /// unverifiable events (correctly) looks identical to a relay that
    /// returned nothing — this separates the two.
    @Test("work-thread events come back and fold into threads")
    func workThreadFold() async throws {
        let (client, _, channel) = try makeClient()
        try await client.connect()
        try await client.authenticate()

        let kinds = [
            MeshKind.workThreadOpen, MeshKind.workThreadMetadata, MeshKind.workThreadState,
            MeshKind.workThreadCheckpoint, MeshKind.workThreadOverdue, MeshKind.workThreadCanon,
            MeshKind.workThreadSiblingArchived, MeshKind.workThreadPromoted,
            MeshKind.workThreadFork, MeshKind.workThreadPromote,
            MeshKind.workThreadGateReviewed,
        ]
        let events = try await client.query(
            MeshFilter(kinds: kinds, limit: 500, tags: ["#h": [channel]]))
        let byKind = Dictionary(grouping: events, by: \.kind).mapValues(\.count)
        print("INTEROP: verified events by kind: \(byKind.sorted { $0.key < $1.key })")

        let threads = MeshFold.threads(from: events)
        print("INTEROP: folded \(threads.count) thread(s)")
        for thread in threads {
            print("INTEROP:   \(thread.status.rawValue) — \(thread.goal) — \(thread.notices.count) notice(s)")
        }
        #expect(!events.isEmpty, "the relay returned no work-thread events at all")
        #expect(!threads.isEmpty, "events came back but folded into no threads")

        await client.disconnect()
    }

    @Test("the relay's own tier stamp is readable from the channel metadata")
    func channelTierIsVisible() async throws {
        let (client, _, channel) = try makeClient()
        try await client.connect()
        try await client.authenticate()

        let metadata = try await client.query(
            MeshFilter(kinds: [MeshKind.channelMetadata], limit: 1, tags: ["#d": [channel]])
        )
        let tierTag = metadata.first?.tagValue("tier")
        #expect(tierTag != nil, "kind:39000 carried no tier tag")
        let tier = MeshChannelTier(rawValue: tierTag ?? "")
        #expect(tier != nil, "tier '\(tierTag ?? "")' is not one the client knows")

        await client.disconnect()
    }
}

/// Concurrency, against a live relay.
///
/// The app issues overlapping queries as a matter of course (a view's
/// `.task` and the connect path both load), and that is exactly what a
/// single-query test cannot see.
@Suite("Interop concurrency", .enabled(if: ProcessInfo.processInfo.environment["MESH_RELAY_URL"] != nil))
struct RelayConcurrencyTests {
    @Test("two overlapping queries each get their own results")
    func concurrentQueriesDoNotStealFrames() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let urlString = env["MESH_RELAY_URL"], let url = URL(string: urlString),
            let keyHex = env["MESH_PRIVATE_KEY"], let channel = env["MESH_CHANNEL"]
        else { throw MeshProtocolError.malformed("interop environment missing") }
        let client = MeshRelayClient(url: url, keys: try MeshKeys(privateKeyHex: keyHex))
        try await client.connect()
        try await client.authenticate()

        // Before serialization these raced on one socket: whichever await
        // was pending consumed the next frame regardless of subscription,
        // and the loser returned empty while the relay had served both.
        async let messages = client.query(
            MeshFilter(kinds: [MeshKind.chatMessage], limit: 100, tags: ["#h": [channel]]))
        async let threads = client.query(
            MeshFilter(
                kinds: [MeshKind.workThreadOpen, MeshKind.workThreadGateReviewed],
                limit: 100, tags: ["#h": [channel]]))
        let (gotMessages, gotThreads) = try await (messages, threads)

        #expect(!gotMessages.isEmpty, "the message query came back empty")
        #expect(!gotThreads.isEmpty, "the work-thread query came back empty")
        // Each result must contain only its own kinds — a stolen frame
        // would show up as a message in the thread result or vice versa.
        #expect(gotMessages.allSatisfy { $0.kind == MeshKind.chatMessage })
        #expect(
            gotThreads.allSatisfy {
                $0.kind == MeshKind.workThreadOpen || $0.kind == MeshKind.workThreadGateReviewed
            })
        await client.disconnect()
    }
}
