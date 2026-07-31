import Foundation

/// A REQ filter. Only the fields a Silent Mesh client actually sends.
public struct MeshFilter: Sendable {
    public var ids: [String]?
    public var authors: [String]?
    public var kinds: [Int]?
    public var since: Int64?
    public var until: Int64?
    public var limit: Int?
    /// Tag filters, e.g. `["#h": [channelUUID]]`.
    public var tags: [String: [String]]

    public init(
        ids: [String]? = nil,
        authors: [String]? = nil,
        kinds: [Int]? = nil,
        since: Int64? = nil,
        until: Int64? = nil,
        limit: Int? = nil,
        tags: [String: [String]] = [:]
    ) {
        self.ids = ids
        self.authors = authors
        self.kinds = kinds
        self.since = since
        self.until = until
        self.limit = limit
        self.tags = tags
    }

    func jsonObject() -> [String: Any] {
        var out: [String: Any] = [:]
        if let ids { out["ids"] = ids }
        if let authors { out["authors"] = authors }
        if let kinds { out["kinds"] = kinds }
        if let since { out["since"] = since }
        if let until { out["until"] = until }
        if let limit { out["limit"] = limit }
        for (k, v) in tags { out[k] = v }
        return out
    }
}

/// A live connection to a Silent Mesh relay.
///
/// Thin on purpose: connect, authenticate (NIP-42), publish, query. The
/// relay is the authority on access control, so the client's job is to
/// present a signed identity and fold what comes back — not to decide what
/// it is allowed to see.
///
/// An actor because a WebSocket is a single ordered stream: interleaved
/// sends from several tasks would corrupt the request/response pairing that
/// subscription ids exist to keep straight.
public actor MeshRelayClient {
    private let url: URL
    private let keys: MeshKeys
    private var task: URLSessionWebSocketTask?
    private let session: URLSession
    private var subscriptionCounter = 0

    /// One exchange at a time.
    ///
    /// An actor protects its state, not a *sequence* of awaits: actors are
    /// reentrant, so a second `query()` can begin while the first is
    /// suspended waiting for a frame. Both then await `receive()` on one
    /// WebSocket, and whichever await happens to be pending consumes the
    /// next frame no matter whose subscription it belongs to — each loop
    /// then discards what it does not recognise. The symptom is brutal to
    /// diagnose: the relay serves everything correctly and the client
    /// silently shows nothing.
    ///
    /// A demultiplexing reader (one task receiving, dispatching by
    /// subscription id) is the real answer and is what live subscriptions
    /// will need. Until then this makes request/response exchanges
    /// strictly sequential, which is honest about what the client
    /// currently supports.
    private var exchangeInFlight = false
    private var waiting: [CheckedContinuation<Void, Never>] = []

    private func beginExchange() async {
        while exchangeInFlight {
            await withCheckedContinuation { waiting.append($0) }
        }
        exchangeInFlight = true
    }

    private func endExchange() {
        exchangeInFlight = false
        if !waiting.isEmpty { waiting.removeFirst().resume() }
    }

    /// Was this connection authenticated (NIP-42) since it opened?
    public private(set) var isAuthenticated = false

    public init(url: URL, keys: MeshKeys, session: URLSession = .shared) {
        self.url = url
        self.keys = keys
        self.session = session
    }

    public func connect() async throws {
        let task = session.webSocketTask(with: url)
        task.resume()
        self.task = task
    }

    public func disconnect() {
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        isAuthenticated = false
    }

    /// Complete the NIP-42 challenge the relay sends on connect.
    ///
    /// The relay opens with `["AUTH", <challenge>]`; the client answers with
    /// a kind-22242 event carrying the challenge and the relay URL, signed
    /// by the member's key. Until that lands, a private channel's history is
    /// invisible — which is indistinguishable from an empty channel, so a
    /// client that skips this looks broken rather than unauthorized.
    public func authenticate(timeout: TimeInterval = 10) async throws {
        await beginExchange()
        defer { endExchange() }
        let challenge = try await waitForAuthChallenge(timeout: timeout)
        var event = MeshEvent(
            pubkey: keys.publicKeyHex,
            createdAt: Int64(Date().timeIntervalSince1970),
            kind: MeshKind.clientAuth,
            tags: [["relay", url.absoluteString], ["challenge", challenge]],
            content: ""
        )
        try event.sign(with: keys)
        try await send(["AUTH", eventObject(event)])
        // The relay answers with OK for the auth event id.
        let ok = try await waitFor(timeout: timeout) { message in
            guard message.count >= 3, message[0] as? String == "OK",
                  message[1] as? String == event.id
            else { return nil }
            return message
        }
        guard ok.count >= 3, (ok[2] as? Bool) == true else {
            let reason = ok.count > 3 ? (ok[3] as? String ?? "refused") : "refused"
            throw MeshProtocolError.relay("auth refused: \(reason)")
        }
        isAuthenticated = true
    }

    /// Publish a signed event, returning once the relay accepts it.
    ///
    /// Waits for the matching `OK` rather than assuming success: the relay
    /// enforces authority, the privacy tier, and the D30 gate at ingest, so
    /// "it was sent" and "it was accepted" are genuinely different outcomes
    /// and the member needs to know which one happened.
    @discardableResult
    public func publish(_ event: MeshEvent, timeout: TimeInterval = 30) async throws -> String {
        guard event.isValid() else {
            throw MeshProtocolError.malformed("refusing to publish an event that fails its own verification")
        }
        await beginExchange()
        defer { endExchange() }
        try await send(["EVENT", eventObject(event)])
        let ok = try await waitFor(timeout: timeout) { message in
            guard message.count >= 3, message[0] as? String == "OK",
                  message[1] as? String == event.id
            else { return nil }
            return message
        }
        let accepted = (ok[2] as? Bool) ?? false
        let detail = ok.count > 3 ? (ok[3] as? String ?? "") : ""
        guard accepted else { throw MeshProtocolError.relay(detail.isEmpty ? "refused" : detail) }
        return event.id
    }

    /// Run a one-shot query: REQ, collect until EOSE, CLOSE.
    public func query(_ filter: MeshFilter, timeout: TimeInterval = 30) async throws -> [MeshEvent] {
        await beginExchange()
        defer { endExchange() }
        subscriptionCounter += 1
        let sub = "mp-\(subscriptionCounter)"
        try await send(["REQ", sub, filter.jsonObject()])
        var collected: [MeshEvent] = []
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let message = try await receiveArray(timeout: deadline.timeIntervalSinceNow)
            guard let verb = message.first as? String else { continue }
            switch verb {
            case "EVENT":
                guard message.count >= 3, message[1] as? String == sub,
                      let object = message[2] as? [String: Any],
                      let event = try? decodeEvent(object)
                else { continue }
                // Verify every event: a relay is a distribution point, not
                // a trust anchor. An unverifiable event is dropped rather
                // than shown, since a client that renders it has silently
                // accepted the relay's word for authorship.
                if event.isValid() { collected.append(event) }
            case "EOSE" where message.count >= 2 && message[1] as? String == sub:
                try await send(["CLOSE", sub])
                return collected
            case "CLOSED" where message.count >= 2 && message[1] as? String == sub:
                let reason = message.count > 2 ? (message[2] as? String ?? "") : ""
                throw MeshProtocolError.relay("subscription closed: \(reason)")
            default:
                continue
            }
        }
        try? await send(["CLOSE", sub])
        throw MeshProtocolError.timeout("query did not reach EOSE")
    }

    // MARK: - Wire plumbing

    private func eventObject(_ event: MeshEvent) -> [String: Any] {
        [
            "id": event.id,
            "pubkey": event.pubkey,
            "created_at": event.createdAt,
            "kind": event.kind,
            "tags": event.tags,
            "content": event.content,
            "sig": event.sig,
        ]
    }

    private func decodeEvent(_ object: [String: Any]) throws -> MeshEvent {
        let data = try JSONSerialization.data(withJSONObject: object)
        return try JSONDecoder().decode(MeshEvent.self, from: data)
    }

    private func send(_ message: [Any]) async throws {
        guard let task else { throw MeshProtocolError.transport("not connected") }
        let data = try JSONSerialization.data(withJSONObject: message, options: [.withoutEscapingSlashes])
        guard let text = String(data: data, encoding: .utf8) else {
            throw MeshProtocolError.malformed("message is not UTF-8")
        }
        try await task.send(.string(text))
    }

    private func receiveArray(timeout: TimeInterval) async throws -> [Any] {
        guard let task else { throw MeshProtocolError.transport("not connected") }
        guard timeout > 0 else { throw MeshProtocolError.timeout("receive deadline passed") }
        let message = try await withTimeout(seconds: timeout) { try await task.receive() }
        let text: String
        switch message {
        case .string(let s): text = s
        case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
        @unknown default: text = ""
        }
        guard let data = text.data(using: .utf8),
              let array = try? JSONSerialization.jsonObject(with: data) as? [Any]
        else { throw MeshProtocolError.malformed("relay sent a non-array frame") }
        return array
    }

    private func waitForAuthChallenge(timeout: TimeInterval) async throws -> String {
        let message = try await waitFor(timeout: timeout) { message in
            guard message.count >= 2, message[0] as? String == "AUTH" else { return nil }
            return message
        }
        guard let challenge = message[1] as? String else {
            throw MeshProtocolError.malformed("AUTH without a challenge")
        }
        return challenge
    }

    /// Bound an await that has no timeout of its own.
    ///
    /// `URLSessionWebSocketTask.receive()` waits forever if the relay sends
    /// nothing. Checking a deadline *between* frames — which is all a loop
    /// can do — is not a timeout: one silent subscription hangs the caller
    /// permanently, with no error to log and nothing on screen to explain
    /// it. Racing the receive against a sleep makes the deadline real.
    private func withTimeout<T: Sendable>(
        seconds: TimeInterval,
        _ work: @escaping @Sendable () async throws -> T
    ) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask { try await work() }
            group.addTask {
                try await Task.sleep(for: .seconds(seconds))
                throw MeshProtocolError.timeout("relay sent nothing for \(Int(seconds))s")
            }
            guard let first = try await group.next() else {
                throw MeshProtocolError.transport("receive produced no result")
            }
            group.cancelAll()
            return first
        }
    }

    /// Receive frames until `match` returns one, or the deadline passes.
    /// Unmatched frames are dropped: this is a request/response helper on a
    /// stream that also carries subscription traffic.
    private func waitFor(
        timeout: TimeInterval,
        _ match: ([Any]) -> [Any]?
    ) async throws -> [Any] {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let message = try await receiveArray(timeout: deadline.timeIntervalSinceNow)
            if let matched = match(message) { return matched }
        }
        throw MeshProtocolError.timeout("no matching relay frame within \(timeout)s")
    }
}
