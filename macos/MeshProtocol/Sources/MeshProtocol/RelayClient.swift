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
/// **One task reads the socket.** Everything else waits to be handed the
/// frame it asked for. That is the whole design, and it is a correction:
/// an earlier version let each caller await `receive()` itself, and because
/// actors are reentrant, two overlapping calls both sat on the socket and
/// consumed each other's frames — the relay served everything correctly and
/// the client silently showed nothing. Serializing exchanges fixed that but
/// cannot carry a live subscription, which by definition stays open while
/// other work happens. A reader that demultiplexes by subscription id does
/// both.
///
/// The relay is the authority on access control; the client's job is to
/// present a signed identity, verify what comes back, and fold it.
public actor MeshRelayClient {
    private let url: URL
    private let keys: MeshKeys
    private let session: URLSession
    private var task: URLSessionWebSocketTask?
    private var readerTask: Task<Void, Never>?
    private var subscriptionCounter = 0

    /// Was this connection authenticated (NIP-42) since it opened?
    public private(set) var isAuthenticated = false

    // MARK: - What the reader is dispatching to

    /// A one-shot query, collecting until EOSE.
    private struct PendingQuery {
        var events: [MeshEvent] = []
        var continuation: CheckedContinuation<[MeshEvent], Error>
    }

    private var pendingQueries: [String: PendingQuery] = [:]
    /// Live subscriptions, which keep receiving after EOSE.
    private var liveSubscriptions: [String: AsyncStream<MeshEvent>.Continuation] = [:]
    /// The relay's verdict on a submitted event.
    ///
    /// A struct rather than the raw frame because a `[Any]` cannot cross
    /// actor boundaries under strict concurrency — and because "accepted,
    /// and here is why not" is the whole content of an OK.
    struct OKFrame: Sendable {
        let accepted: Bool
        let detail: String
    }

    /// `OK` waiters, keyed by the event id they are about.
    private var pendingOKs: [String: CheckedContinuation<OKFrame, Error>] = [:]
    /// The NIP-42 challenge waiter, if `authenticate()` is in flight.
    private var pendingChallenge: CheckedContinuation<String, Error>?
    /// A challenge that arrived before anyone asked for it.
    ///
    /// The relay sends AUTH the moment the socket opens, which is before
    /// `authenticate()` has had a turn to register. Dropping it meant
    /// waiting for a challenge that had already been and gone until the
    /// relay's own auth timer gave up and closed the connection — a
    /// failure that reads as "the network is broken".
    private var bufferedChallenge: String?

    public init(url: URL, keys: MeshKeys, session: URLSession = .shared) {
        self.url = url
        self.keys = keys
        self.session = session
    }

    public func connect() async throws {
        let task = session.webSocketTask(with: url)
        task.resume()
        self.task = task
        readerTask = Task { [weak self] in await self?.readLoop() }
    }

    public func disconnect() {
        readerTask?.cancel()
        readerTask = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        isAuthenticated = false
        failAllPending(with: MeshProtocolError.transport("disconnected"))
    }

    // MARK: - The reader

    private func readLoop() async {
        // `resume()` returns before the handshake completes, so the first
        // `receive()` can fail with "Socket is not connected" on a socket
        // that is about to work perfectly. Retry briefly — but ONLY before
        // the first frame arrives. After that an error means the
        // connection actually died, and every waiter must be told rather
        // than left hoping.
        //
        // (A ping would seem tidier, but the relay does not answer client
        // pings, so waiting for a pong hangs forever — a race traded for a
        // deadlock.)
        var warmupRetries = 20
        var framesSeen = 0
        while !Task.isCancelled {
            guard let task else { return }
            do {
                let message = try await task.receive()
                warmupRetries = 0
                framesSeen += 1
                let text: String
                switch message {
                case .string(let s): text = s
                case .data(let d): text = String(data: d, encoding: .utf8) ?? ""
                @unknown default: text = ""
                }
                guard let data = text.data(using: .utf8),
                    let frame = try? JSONSerialization.jsonObject(with: data) as? [Any]
                else { continue }
                dispatch(frame)
            } catch {
                if warmupRetries > 0 {
                    warmupRetries -= 1
                    try? await Task.sleep(for: .milliseconds(100))
                    continue
                }
                // Say where the connection died. A transport failure with
                // no context is the hardest thing to diagnose remotely —
                // it reads identically whether the relay is down, the
                // handshake never completed, or the socket dropped later.
                MeshLog.write(
                    "reader stopped after \(framesSeen) frame(s): \(error.localizedDescription)")
                // The socket is gone. Everything waiting on it must be told,
                // or every caller hangs forever on a connection that will
                // never answer — the same failure shape as a receive with no
                // timeout, one level up.
                failAllPending(with: MeshProtocolError.transport(error.localizedDescription))
                return
            }
        }
    }

    private func dispatch(_ frame: [Any]) {
        guard let verb = frame.first as? String else { return }
        switch verb {
        case "AUTH":
            guard let challenge = frame.count > 1 ? frame[1] as? String : nil else { return }
            if let waiter = pendingChallenge {
                pendingChallenge = nil
                waiter.resume(returning: challenge)
            } else {
                bufferedChallenge = challenge
            }

        case "OK":
            guard frame.count >= 3, let id = frame[1] as? String,
                let waiter = pendingOKs.removeValue(forKey: id)
            else { return }
            waiter.resume(
                returning: OKFrame(
                    accepted: (frame[2] as? Bool) ?? false,
                    detail: frame.count > 3 ? (frame[3] as? String ?? "") : ""))

        case "EVENT":
            guard frame.count >= 3, let sub = frame[1] as? String,
                let object = frame[2] as? [String: Any],
                let event = decodeEvent(object)
            else { return }
            // Verify before handing anything upward: a relay is a
            // distribution point, not a trust anchor, and rendering an
            // unverifiable event accepts its word for authorship.
            guard event.isValid() else { return }
            if pendingQueries[sub] != nil {
                pendingQueries[sub]?.events.append(event)
            } else if let live = liveSubscriptions[sub] {
                live.yield(event)
            }

        case "EOSE":
            guard frame.count >= 2, let sub = frame[1] as? String else { return }
            if let pending = pendingQueries.removeValue(forKey: sub) {
                sendFireAndForget(["CLOSE", sub])
                pending.continuation.resume(returning: pending.events)
            }
        // A live subscription's EOSE only means "history sent"; it keeps
        // streaming, so there is nothing to complete.

        case "CLOSED":
            guard frame.count >= 2, let sub = frame[1] as? String else { return }
            let reason = frame.count > 2 ? (frame[2] as? String ?? "") : ""
            if let pending = pendingQueries.removeValue(forKey: sub) {
                pending.continuation.resume(
                    throwing: MeshProtocolError.relay("subscription closed: \(reason)"))
            }
            liveSubscriptions.removeValue(forKey: sub)?.finish()

        default:
            return
        }
    }

    private func failAllPending(with error: Error) {
        for (_, pending) in pendingQueries { pending.continuation.resume(throwing: error) }
        pendingQueries.removeAll()
        for (_, waiter) in pendingOKs { waiter.resume(throwing: error) }
        pendingOKs.removeAll()
        for (_, live) in liveSubscriptions { live.finish() }
        liveSubscriptions.removeAll()
        pendingChallenge?.resume(throwing: error)
        pendingChallenge = nil
    }

    // MARK: - Operations

    /// Complete the NIP-42 challenge the relay sends on connect.
    ///
    /// Until this lands, a private channel's history is invisible — which
    /// is indistinguishable from an empty channel, so a client that skips
    /// it looks broken rather than unauthorized.
    public func authenticate(timeout: TimeInterval = 10) async throws {
        let challenge: String
        if let buffered = bufferedChallenge {
            bufferedChallenge = nil
            challenge = buffered
        } else {
            let deadline = Task { [weak self] in
                try? await Task.sleep(for: .seconds(timeout))
                await self?.timeoutChallenge(seconds: timeout)
            }
            challenge = try await withCheckedThrowingContinuation { continuation in
                pendingChallenge = continuation
            }
            deadline.cancel()
        }
        var event = MeshEvent(
            pubkey: keys.publicKeyHex,
            createdAt: Int64(Date().timeIntervalSince1970),
            kind: MeshKind.clientAuth,
            tags: [["relay", url.absoluteString], ["challenge", challenge]],
            content: ""
        )
        try event.sign(with: keys)
        let ok = try await sendAwaitingOK(event, timeout: timeout)
        guard ok.accepted else {
            throw MeshProtocolError.relay(
                "auth refused: \(ok.detail.isEmpty ? "refused" : ok.detail)")
        }
        isAuthenticated = true
    }

    /// Publish a signed event, returning once the relay accepts it.
    ///
    /// Waits for the matching `OK` rather than assuming success: the relay
    /// enforces authority, the privacy tier, and the D30 gate at ingest, so
    /// "sent" and "accepted" are different outcomes the member needs told
    /// apart.
    @discardableResult
    public func publish(_ event: MeshEvent, timeout: TimeInterval = 30) async throws -> String {
        guard event.isValid() else {
            throw MeshProtocolError.malformed(
                "refusing to publish an event that fails its own verification")
        }
        let ok = try await sendAwaitingOK(event, timeout: timeout)
        guard ok.accepted else {
            throw MeshProtocolError.relay(ok.detail.isEmpty ? "refused" : ok.detail)
        }
        return event.id
    }

    /// One-shot query: REQ, collect until EOSE, CLOSE.
    public func query(_ filter: MeshFilter, timeout: TimeInterval = 30) async throws -> [MeshEvent] {
        subscriptionCounter += 1
        let sub = "q-\(subscriptionCounter)"
        let deadline = Task { [weak self] in
            try? await Task.sleep(for: .seconds(timeout))
            await self?.timeoutQuery(sub, seconds: timeout)
        }
        defer { deadline.cancel() }
        // Register and send in ONE actor turn. Sending first and
        // registering after leaves a gap in which the relay's answer can
        // arrive and be discarded as unclaimed — and locally the relay is
        // fast enough that it usually does. The actor cannot dispatch a
        // frame while this body is running, so there is no gap to lose.
        return try await withCheckedThrowingContinuation { continuation in
            pendingQueries[sub] = PendingQuery(continuation: continuation)
            sendFireAndForget(["REQ", sub, filter.jsonObject()])
        }
    }

    /// A live subscription: history first, then everything new, until the
    /// stream is cancelled.
    ///
    /// Cancelling the stream sends `CLOSE`, so a relay is never left
    /// fanning out to a client that stopped listening.
    public func subscribe(_ filter: MeshFilter) async throws -> AsyncStream<MeshEvent> {
        subscriptionCounter += 1
        let sub = "s-\(subscriptionCounter)"
        let stream = AsyncStream<MeshEvent> { continuation in
            self.registerLive(sub: sub, continuation: continuation)
            continuation.onTermination = { _ in
                Task { await self.closeLive(sub) }
            }
        }
        // Registered above, so the REQ can go out safely now.
        try await send(["REQ", sub, filter.jsonObject()])
        return stream
    }

    // MARK: - Internals

    private func registerLive(sub: String, continuation: AsyncStream<MeshEvent>.Continuation) {
        liveSubscriptions[sub] = continuation
    }

    private func closeLive(_ sub: String) {
        guard liveSubscriptions.removeValue(forKey: sub) != nil else { return }
        sendFireAndForget(["CLOSE", sub])
    }

    private func sendAwaitingOK(_ event: MeshEvent, timeout: TimeInterval) async throws -> OKFrame {
        let verb = event.kind == MeshKind.clientAuth ? "AUTH" : "EVENT"
        let id = event.id
        let deadline = Task { [weak self] in
            try? await Task.sleep(for: .seconds(timeout))
            await self?.timeoutOK(id, seconds: timeout)
        }
        defer { deadline.cancel() }
        // Same ordering rule as `query`: the OK can come back before a
        // waiter registered after the send would exist.
        return try await withCheckedThrowingContinuation { continuation in
            pendingOKs[id] = continuation
            sendFireAndForget([verb, eventObject(event)])
        }
    }

    /// Fail a waiter that has been pending too long.
    ///
    /// A deadline is enforced by a companion task calling back into the
    /// actor rather than by racing the wait itself: the continuation must
    /// be registered from actor-isolated code, and — more importantly —
    /// whoever times out must also REMOVE the entry. A timed-out waiter
    /// left in the map is both a leak and a landmine, since a late frame
    /// would resume a continuation nobody is awaiting.
    private func timeoutQuery(_ sub: String, seconds: TimeInterval) {
        guard let pending = pendingQueries.removeValue(forKey: sub) else { return }
        sendFireAndForget(["CLOSE", sub])
        pending.continuation.resume(
            throwing: MeshProtocolError.timeout(
                "the relay did not finish the query within \(Int(seconds))s"))
    }

    private func timeoutOK(_ id: String, seconds: TimeInterval) {
        guard let waiter = pendingOKs.removeValue(forKey: id) else { return }
        waiter.resume(
            throwing: MeshProtocolError.timeout(
                "the relay did not acknowledge within \(Int(seconds))s"))
    }

    private func timeoutChallenge(seconds: TimeInterval) {
        guard let waiter = pendingChallenge else { return }
        pendingChallenge = nil
        waiter.resume(
            throwing: MeshProtocolError.timeout(
                "the relay sent no auth challenge within \(Int(seconds))s"))
    }

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

    private func decodeEvent(_ object: [String: Any]) -> MeshEvent? {
        guard let data = try? JSONSerialization.data(withJSONObject: object) else { return nil }
        return try? JSONDecoder().decode(MeshEvent.self, from: data)
    }

    private func send(_ message: [Any]) async throws {
        guard let task else { throw MeshProtocolError.transport("not connected") }
        let data = try JSONSerialization.data(
            withJSONObject: message, options: [.withoutEscapingSlashes])
        guard let text = String(data: data, encoding: .utf8) else {
            throw MeshProtocolError.malformed("message is not UTF-8")
        }
        try await task.send(.string(text))
    }

    /// `CLOSE` is housekeeping: if it fails the socket is already gone, and
    /// there is nothing useful to do about it.
    private func sendFireAndForget(_ message: [Any]) {
        guard let task,
            let data = try? JSONSerialization.data(
                withJSONObject: message, options: [.withoutEscapingSlashes]),
            let text = String(data: data, encoding: .utf8)
        else { return }
        Task { try? await task.send(.string(text)) }
    }
}


/// Diagnostics for a client that usually runs where nobody is watching.
///
/// Over SSH a window showing nothing and a window showing nothing for a
/// different reason are indistinguishable, so the transport says what it
/// did. Off by default: `MESH_LOG=1` turns it on.
enum MeshLog {
    private static let enabled = ProcessInfo.processInfo.environment["MESH_LOG"] == "1"

    static func write(_ message: String) {
        guard enabled else { return }
        FileHandle.standardError.write(Data("mesh-net: \(message)\n".utf8))
    }
}
