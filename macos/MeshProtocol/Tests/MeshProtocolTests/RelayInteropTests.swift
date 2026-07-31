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

/// Live subscriptions — the reason the client needed a demultiplexing
/// reader rather than serialized exchanges.
@Suite("Interop live", .enabled(if: ProcessInfo.processInfo.environment["MESH_RELAY_URL"] != nil))
struct RelayLiveTests {
    private func env() throws -> (URL, String, String) {
        let e = ProcessInfo.processInfo.environment
        guard let urlString = e["MESH_RELAY_URL"], let url = URL(string: urlString),
            let key = e["MESH_PRIVATE_KEY"], let channel = e["MESH_CHANNEL"]
        else { throw MeshProtocolError.malformed("interop environment missing") }
        return (url, key, channel)
    }

    @Test("a live subscription delivers an event published after it started")
    func liveDelivery() async throws {
        let (url, keyHex, channel) = try env()
        let keys = try MeshKeys(privateKeyHex: keyHex)

        let watcher = MeshRelayClient(url: url, keys: keys)
        try await watcher.connect()
        try await watcher.authenticate()
        let stream = try await watcher.subscribe(
            MeshFilter(kinds: [MeshKind.chatMessage], limit: 0, tags: ["#h": [channel]]))

        // A second connection publishes, so the delivery path is genuinely
        // relay fan-out rather than the client hearing its own echo on the
        // socket it wrote to.
        let publisher = MeshRelayClient(url: url, keys: keys)
        try await publisher.connect()
        try await publisher.authenticate()
        let marker = "live subscription \(UUID().uuidString)"
        var event = try MeshEvent.chatMessage(
            channel: channel, content: marker, pubkey: keys.publicKeyHex)
        try event.sign(with: keys)
        try await publisher.publish(event)

        // The deadline must be independent of arrivals: checking it inside
        // the loop only fires when an event shows up, so "nothing ever
        // arrives" — the failure this test exists to catch — would hang the
        // suite instead of failing it.
        let seen: MeshEvent? = await withTaskGroup(of: MeshEvent?.self) { group in
            group.addTask {
                for await incoming in stream where incoming.content == marker {
                    return incoming
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(for: .seconds(20))
                return nil
            }
            let first = await group.next() ?? nil
            group.cancelAll()
            return first
        }
        #expect(seen != nil, "the subscription never delivered the published event")
        #expect(seen?.isValid() == true)

        await watcher.disconnect()
        await publisher.disconnect()
    }

    @Test("queries still work while a subscription is open")
    func queryDuringSubscription() async throws {
        let (url, keyHex, channel) = try env()
        let client = MeshRelayClient(url: url, keys: try MeshKeys(privateKeyHex: keyHex))
        try await client.connect()
        try await client.authenticate()

        // The case serialized exchanges could not express: a stream stays
        // open indefinitely, so a query that waits its turn would wait
        // forever.
        let stream = try await client.subscribe(
            MeshFilter(kinds: [MeshKind.chatMessage], limit: 0, tags: ["#h": [channel]]))
        let messages = try await client.query(
            MeshFilter(kinds: [MeshKind.chatMessage], limit: 20, tags: ["#h": [channel]]))
        #expect(!messages.isEmpty, "a query alongside a live subscription came back empty")
        #expect(messages.allSatisfy { $0.kind == MeshKind.chatMessage })

        let iterator = stream.makeAsyncIterator()
        _ = iterator  // the stream is closed by disconnect below
        await client.disconnect()
    }
}

/// Supervised approvals, end to end against a live relay.
///
/// This is the roadmap's Phase 4 exit criterion in miniature — "including
/// granting a supervised approval". Everything else about approvals is
/// provable from the event shapes; only this proves the client's reading of
/// a *relay-authored* kind:46010 is right, and that a Swift-signed
/// kind:46030 is one the relay will act on.
///
/// The test plays both parts. `MESH_AGENT_KEY` is a second identity that
/// registers the request over the relay's HTTP surface exactly as the ACP
/// harness does; `MESH_PRIVATE_KEY` is the member who decides it. They must
/// differ: an agent deciding its own request is refused, which the last test
/// here relies on.
///
///   MESH_RELAY_URL=… MESH_PRIVATE_KEY=<member> MESH_AGENT_KEY=<agent> \
///   MESH_CHANNEL=<uuid> swift test --filter RelayApprovalTests
@Suite(
    "Interop approvals",
    .enabled(if: ProcessInfo.processInfo.environment["MESH_AGENT_KEY"] != nil))
struct RelayApprovalTests {
    private struct Environment {
        var relay: URL
        var http: URL
        var member: MeshKeys
        var agent: MeshKeys
        var channel: String
    }

    private func environment() throws -> Environment {
        let e = ProcessInfo.processInfo.environment
        guard let urlString = e["MESH_RELAY_URL"], let relay = URL(string: urlString),
            let memberKey = e["MESH_PRIVATE_KEY"], let agentKey = e["MESH_AGENT_KEY"],
            let channel = e["MESH_CHANNEL"]
        else { throw MeshProtocolError.malformed("approval interop environment missing") }
        // The relay derives the NIP-98 URL it expects from the Host header,
        // so the HTTP origin must be the same authority as the WebSocket one.
        var components = URLComponents(url: relay, resolvingAgainstBaseURL: false)
        components?.scheme = relay.scheme == "wss" ? "https" : "http"
        guard let http = components?.url else {
            throw MeshProtocolError.malformed("cannot derive an HTTP origin from \(relay)")
        }
        return Environment(
            relay: relay, http: http,
            member: try MeshKeys(privateKeyHex: memberKey),
            agent: try MeshKeys(privateKeyHex: agentKey),
            channel: channel)
    }

    /// A NIP-98 `Authorization` header: a signed kind:27235 binding the
    /// method, the exact URL, and a digest of the body.
    ///
    /// The body digest is what stops a captured header from being replayed
    /// against a different request, so it is not optional for POSTs — the
    /// relay refuses body-bearing calls that omit it.
    private func nip98(keys: MeshKeys, method: String, url: String, body: Data?) throws -> String {
        var tags: [[String]] = [
            ["u", url], ["method", method], ["nonce", UUID().uuidString],
        ]
        if let body {
            tags.append(["payload", MeshHex.encode(SHA256Digest.hash(body))])
        }
        var event = MeshEvent(
            pubkey: keys.publicKeyHex, createdAt: Int64(Date().timeIntervalSince1970),
            kind: 27235, tags: tags, content: "")
        try event.sign(with: keys)
        let json = try JSONEncoder().encode(event)
        return "Nostr \(json.base64EncodedString())"
    }

    /// Register a pending permission request as the agent would.
    private func requestPermission(
        _ env: Environment, detail: String
    ) async throws -> (requestID: String, tokenHash: String) {
        let url = env.http.appendingPathComponent("api/approvals")
        let body = try JSONSerialization.data(withJSONObject: [
            "channel_id": env.channel,
            "request_kind": "command",
            "tool_name": "shell",
            "detail": detail,
            "options": [],
            "ttl_secs": 600,
        ])
        var request = URLRequest(url: url)
        request.httpMethod = "POST"
        request.httpBody = body
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue(
            try nip98(keys: env.agent, method: "POST", url: url.absoluteString, body: body),
            forHTTPHeaderField: "Authorization")

        let (data, response) = try await URLSession.shared.data(for: request)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        guard status == 200 else {
            throw MeshProtocolError.relay(
                "POST /api/approvals -> \(status): \(String(decoding: data, as: UTF8.self))")
        }
        let json = try JSONSerialization.jsonObject(with: data) as? [String: Any]
        guard let requestID = json?["request_id"] as? String,
            let tokenHash = json?["token_hash"] as? String
        else {
            throw MeshProtocolError.malformed(
                "approval response missing ids: \(String(decoding: data, as: UTF8.self))")
        }
        return (requestID, tokenHash)
    }

    private func connected(_ env: Environment, as keys: MeshKeys) async throws -> MeshRelayClient {
        let client = MeshRelayClient(url: env.relay, keys: keys)
        try await client.connect()
        try await client.authenticate()
        return client
    }

    private func approvals(_ client: MeshRelayClient, channel: String) async throws -> [MeshApproval] {
        let events = try await client.query(
            MeshFilter(
                kinds: [
                    MeshKind.approvalRequested, MeshKind.approvalGranted, MeshKind.approvalDenied,
                ],
                limit: 200, tags: ["#h": [channel]]))
        return MeshApprovalFold.approvals(from: events)
    }

    @Test("a member reads the relay's request and grants it")
    func grantRoundTrip() async throws {
        let env = try environment()
        let detail = "run `git status` in the checkout — \(UUID().uuidString)"
        let (requestID, tokenHash) = try await requestPermission(env, detail: detail)

        let member = try await connected(env, as: env.member)
        // The client's own decode of a relay-authored kind:46010 — the half
        // no unit test can reach, since it is the relay that chose the tags,
        // the content shape, and the timestamp format.
        let pending = try await approvals(member, channel: env.channel)
            .first { $0.tokenHash == tokenHash }
        #expect(pending != nil, "the relay's kind:46010 did not fold into an approval")
        #expect(pending?.outcome == .pending)
        #expect(pending?.isActionable == true)
        #expect(pending?.detail == detail)
        #expect(pending?.requestID == requestID)
        #expect(pending?.kind == .command)
        #expect(pending?.toolName == "shell")
        #expect(pending?.agentPubkey == env.agent.publicKeyHex)
        // A deadline the relay set and this client could not read would
        // silently become "no expiry" — the RFC 3339 shape is the trap.
        #expect(pending?.expiresAt != nil, "the relay's expires_at did not parse")

        var decision = try MeshEvent.approvalDecision(
            tokenHash: tokenHash, grant: true, note: "interop",
            pubkey: env.member.publicKeyHex)
        try decision.sign(with: env.member)
        _ = try await member.publish(decision)

        // The outcome record is emitted after the commit, so it may not be
        // queryable the instant the OK arrives.
        var resolved: MeshApproval?
        for _ in 0..<20 {
            resolved = try await approvals(member, channel: env.channel)
                .first { $0.tokenHash == tokenHash }
            if resolved?.outcome != .pending { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        #expect(
            resolved?.outcome == .granted(by: env.member.publicKeyHex),
            "the grant did not come back as the relay's own record")
        #expect(resolved?.isActionable == false)

        await member.disconnect()
    }

    @Test("a denial is recorded as a denial, not merely as 'not granted'")
    func denyRoundTrip() async throws {
        let env = try environment()
        let (_, tokenHash) = try await requestPermission(
            env, detail: "delete the build directory — \(UUID().uuidString)")
        let member = try await connected(env, as: env.member)

        var decision = try MeshEvent.approvalDecision(
            tokenHash: tokenHash, grant: false, note: "not from here",
            pubkey: env.member.publicKeyHex)
        try decision.sign(with: env.member)
        _ = try await member.publish(decision)

        var resolved: MeshApproval?
        for _ in 0..<20 {
            resolved = try await approvals(member, channel: env.channel)
                .first { $0.tokenHash == tokenHash }
            if resolved?.outcome != .pending { break }
            try await Task.sleep(for: .milliseconds(250))
        }
        #expect(resolved?.outcome == .denied(by: env.member.publicKeyHex))

        await member.disconnect()
    }

    /// The authority rule the whole surface rests on: the agent that asked
    /// may not answer. If this ever passes, "supervised" means nothing —
    /// so the test asserts the refusal, and that the relay says why.
    @Test("an agent cannot grant its own request")
    func agentCannotSelfApprove() async throws {
        let env = try environment()
        let (_, tokenHash) = try await requestPermission(
            env, detail: "self-approval attempt — \(UUID().uuidString)")
        let agent = try await connected(env, as: env.agent)

        var decision = try MeshEvent.approvalDecision(
            tokenHash: tokenHash, grant: true, pubkey: env.agent.publicKeyHex)
        try decision.sign(with: env.agent)

        var refusal: String?
        do {
            _ = try await agent.publish(decision)
        } catch MeshProtocolError.relay(let reason) {
            refusal = reason
        }
        #expect(refusal != nil, "the relay accepted an agent's grant of its own request")
        // The refusal is shown to the member verbatim, so it has to be a
        // sentence rather than a code.
        #expect(refusal?.contains("cannot decide its own") == true, "got: \(refusal ?? "nil")")

        // And it is still pending for a human.
        let member = try await connected(env, as: env.member)
        let still = try await approvals(member, channel: env.channel)
            .first { $0.tokenHash == tokenHash }
        #expect(still?.outcome == .pending)

        await agent.disconnect()
        await member.disconnect()
    }
}
