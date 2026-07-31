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

/// What the client is doing about its connection, for a UI that should not
/// have to guess whether silence means "nothing is happening" or "nothing
/// is getting through".
public enum MeshConnectionState: Sendable, Equatable {
    case connecting
    case connected
    /// The socket died and the client is waiting before trying again.
    case reconnecting(attempt: Int, retryIn: TimeInterval)
    /// Closed on purpose, or given up.
    case closed(String)
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

    /// A live subscription, which keeps receiving after EOSE — and which
    /// outlives the socket it was opened on.
    ///
    /// The filter is kept because a subscription is a standing intent, not
    /// a request: a relay that never heard the REQ (because this is a new
    /// connection) has to be told again. `lastSeen` is kept so the replay
    /// asks for the gap rather than starting from now.
    private struct LiveSubscription {
        let filter: MeshFilter
        let continuation: AsyncStream<MeshEvent>.Continuation
        /// `created_at` of the newest event delivered here.
        var lastSeen: Int64?
        /// When this subscription opened — the replay floor until something
        /// arrives, so a subscription that has seen nothing yet still does
        /// not silently skip the outage.
        let openedAt: Int64
    }

    private var liveSubscriptions: [String: LiveSubscription] = [:]
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

    // MARK: - Reconnection

    /// Should a dead socket be re-opened? False until the first successful
    /// `connect()`, and false again after a deliberate `disconnect()` — so
    /// closing on purpose is never mistaken for a failure worth retrying.
    private var wantsConnection = false
    /// Did `authenticate()` succeed on this client? A reconnect must repeat
    /// it, or the new socket comes back unauthenticated and every private
    /// channel silently reads as empty.
    private var wantsAuthentication = false
    private var reconnectTask: Task<Void, Never>?

    public private(set) var state: MeshConnectionState = .closed("not started")
    private var stateContinuation: AsyncStream<MeshConnectionState>.Continuation?

    /// Watch what the connection is doing.
    ///
    /// One consumer: a second call replaces the first, because this exists
    /// for a status line rather than as a general event bus.
    public func connectionStates() -> AsyncStream<MeshConnectionState> {
        AsyncStream { continuation in
            continuation.yield(state)
            stateContinuation?.finish()
            stateContinuation = continuation
        }
    }

    private func setState(_ next: MeshConnectionState) {
        guard next != state else { return }
        state = next
        stateContinuation?.yield(next)
        MeshLog.write("state: \(next)")
    }

    public init(url: URL, keys: MeshKeys, session: URLSession = .shared) {
        self.url = url
        self.keys = keys
        self.session = session
    }

    public func connect() async throws {
        wantsConnection = true
        setState(.connecting)
        openSocket()
        setState(.connected)
    }

    /// Open a socket and start a reader on it. Deliberately not `throws`:
    /// `resume()` returns before the handshake completes, so failure shows
    /// up in the reader, which is the one place equipped to react to it.
    private func openSocket() {
        readerTask?.cancel()
        let task = session.webSocketTask(with: url)
        task.resume()
        self.task = task
        isAuthenticated = false
        readerTask = Task { [weak self] in await self?.readLoop() }
    }

    public func disconnect() {
        wantsConnection = false
        wantsAuthentication = false
        reconnectTask?.cancel()
        reconnectTask = nil
        readerTask?.cancel()
        readerTask = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        isAuthenticated = false
        setState(.closed("disconnected"))
        failAllPending(with: MeshProtocolError.transport("disconnected"))
        stateContinuation?.finish()
        stateContinuation = nil
    }

    /// Drop the transport without telling the relay, as a dead tailnet route
    /// would. Test seam: reconnection is the one behaviour that cannot be
    /// exercised without a connection that fails, and asserting it against a
    /// mock would only prove the mock reconnects.
    func simulateTransportFailure() {
        task?.cancel(with: .abnormalClosure, reason: nil)
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
                // A cancelled reader is a teardown this client asked for,
                // not a connection that broke. Without this check
                // `disconnect()` cancels the socket, the reader reports the
                // resulting error as a transport failure, and the state ends
                // up naming a socket error for what was a deliberate close —
                // and on the warm-up path it would even retry.
                if Task.isCancelled { return }
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
                handleTransportFailure(error.localizedDescription)
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
                // Remember how far this subscription has got, so a reconnect
                // asks for the gap instead of resuming from now.
                liveSubscriptions[sub]?.lastSeen = max(live.lastSeen ?? 0, event.createdAt)
                live.continuation.yield(event)
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
            // A relay-sent CLOSED is a decision about this subscription (a
            // bad filter, a revoked permission), not a transport blip — so
            // it ends the stream rather than being retried forever.
            liveSubscriptions.removeValue(forKey: sub)?.continuation.finish()

        default:
            return
        }
    }

    private func failAllPending(with error: Error) {
        failOneShots(with: error)
        for (_, live) in liveSubscriptions { live.continuation.finish() }
        liveSubscriptions.removeAll()
    }

    /// Fail everything that was waiting for a single answer.
    ///
    /// One-shot work dies with the socket: a query or a publish belongs to
    /// whoever asked, and only they can decide whether repeating it is safe.
    /// Silently retrying a publish would be a client deciding on its own to
    /// send a member's message twice.
    private func failOneShots(with error: Error) {
        for (_, pending) in pendingQueries { pending.continuation.resume(throwing: error) }
        pendingQueries.removeAll()
        for (_, waiter) in pendingOKs { waiter.resume(throwing: error) }
        pendingOKs.removeAll()
        pendingChallenge?.resume(throwing: error)
        pendingChallenge = nil
        bufferedChallenge = nil
    }

    /// The socket died on its own.
    ///
    /// One-shot waiters are failed; live subscriptions are **kept**, because
    /// they are a standing intent rather than a request. Ending them here is
    /// what made a dropped tailnet route permanent: the app's `for await`
    /// loop returned, and nothing ever asked again.
    private func handleTransportFailure(_ reason: String) {
        let error = MeshProtocolError.transport(reason)
        failOneShots(with: error)
        isAuthenticated = false

        guard wantsConnection else {
            for (_, live) in liveSubscriptions { live.continuation.finish() }
            liveSubscriptions.removeAll()
            setState(.closed(reason))
            return
        }
        startReconnecting(after: reason)
    }

    private func startReconnecting(after reason: String) {
        guard reconnectTask == nil else { return }
        reconnectTask = Task { [weak self] in await self?.reconnectLoop(reason: reason) }
    }

    /// Re-open, re-authenticate, and re-subscribe, backing off between
    /// attempts.
    ///
    /// It does not give up. A Silent Mesh relay lives on a tailnet, so the
    /// usual reason for failure is a route that will come back — a laptop
    /// that slept, a network that changed. An app that stopped trying would
    /// have to be restarted to notice.
    private func reconnectLoop(reason: String) async {
        var attempt = 0
        while wantsConnection && !Task.isCancelled {
            attempt += 1
            // 0.5s doubling to a 15s ceiling: fast enough that a blip is
            // invisible, slow enough that an hour offline is not a spin.
            let delay = min(0.5 * pow(2, Double(attempt - 1)), 15)
            setState(.reconnecting(attempt: attempt, retryIn: delay))
            try? await Task.sleep(for: .seconds(delay))
            if Task.isCancelled || !wantsConnection { break }

            openSocket()
            do {
                if wantsAuthentication { try await authenticate() }
                try await resubscribeAll()
                setState(.connected)
                MeshLog.write("reconnected after \(attempt) attempt(s) (was: \(reason))")
                reconnectTask = nil
                return
            } catch {
                MeshLog.write("reconnect attempt \(attempt) failed: \(error)")
                // Leave the loop's own state alone and try again: a half-open
                // socket here is exactly the case the next attempt handles.
                failOneShots(with: MeshProtocolError.transport("reconnecting"))
            }
        }
        reconnectTask = nil
    }

    /// Re-send every live subscription's REQ on the new socket.
    ///
    /// Two changes to the original filter, both load-bearing:
    ///
    /// - `since` is set to the last event seen, so the relay replays what
    ///   was missed. Inclusive, so the last event usually arrives twice —
    ///   deliberately, because a duplicate is visible to a client that dedups
    ///   and a gap is not visible to anyone.
    /// - `limit` is cleared. Live subscriptions are opened with `limit: 0`
    ///   ("no history, I already loaded it"), and keeping that would make the
    ///   relay send nothing stored — so the replay this whole method exists
    ///   for would return exactly zero events.
    private func resubscribeAll() async throws {
        for (sub, live) in liveSubscriptions {
            var replay = live.filter
            replay.since = live.lastSeen ?? live.openedAt
            replay.limit = nil
            try await send(["REQ", sub, replay.jsonObject()])
        }
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
        wantsAuthentication = true
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
            self.registerLive(sub: sub, filter: filter, continuation: continuation)
            continuation.onTermination = { _ in
                Task { await self.closeLive(sub) }
            }
        }
        // Registered above, so the REQ can go out safely now.
        try await send(["REQ", sub, filter.jsonObject()])
        return stream
    }

    // MARK: - Internals

    private func registerLive(
        sub: String, filter: MeshFilter, continuation: AsyncStream<MeshEvent>.Continuation
    ) {
        liveSubscriptions[sub] = LiveSubscription(
            filter: filter,
            continuation: continuation,
            lastSeen: nil,
            openedAt: Int64(Date().timeIntervalSince1970))
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
