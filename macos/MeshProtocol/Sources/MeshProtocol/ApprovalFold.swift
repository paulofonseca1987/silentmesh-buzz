import Foundation

/// Where an approval request stands.
///
/// `.expired` is decided by the *client*, from the deadline the relay put in
/// the request. The relay does not emit anything when a request lapses, so a
/// client that only folded outcome events would show a dead request as
/// actionable forever.
public enum MeshApprovalOutcome: Sendable, Equatable {
    case pending
    case granted(by: String)
    case denied(by: String)
    case expired
}

/// What an agent is asking permission to do.
///
/// Mirrors `AgentPermissionRequestKind` in `buzz-db`. Unknown strings map to
/// `.other` rather than dropping the request: a member must still be able to
/// decide a request this client is too old to name, and the detail text
/// carries the substance either way.
public enum MeshApprovalKind: String, Sendable, Equatable, CaseIterable {
    case command
    case fileRead = "file-read"
    case fileChange = "file-change"
    case other

    init(wire: String?) {
        self = MeshApprovalKind(rawValue: wire ?? "") ?? .other
    }
}

/// An agent's pending permission request, as a member sees it.
///
/// The `d` tag — `tokenHash` here — is the only handle a decision needs, and
/// it is a *hash* of the approval token rather than the token itself. The
/// member therefore never holds the secret that authorizes the action; they
/// reference the request and the relay resolves it. (The older workflow
/// approval path puts the raw token in event content. This one does not, and
/// the difference is deliberate.)
public struct MeshApproval: Sendable, Equatable, Identifiable {
    /// Hex of the stored token hash — the `d` tag shared by the request and
    /// every event about it. Also this approval's identity.
    public var tokenHash: String
    /// Relay-minted request id, for correlating with the harness.
    public var requestID: String
    public var channelID: String
    /// The agent that asked. Never allowed to decide its own request.
    public var agentPubkey: String
    public var kind: MeshApprovalKind
    /// Tool the agent invoked, when the harness could name one.
    public var toolName: String?
    /// The summary the relay published. Never the tool payload.
    public var detail: String
    public var requestedAt: Int64
    /// When the relay stops accepting a decision. `nil` when the relay sent
    /// a timestamp this client could not parse — treated as no deadline
    /// rather than as already expired, since refusing to show a live request
    /// is worse than showing a dead one.
    public var expiresAt: Date?
    public var outcome: MeshApprovalOutcome

    public var id: String { tokenHash }

    /// Can a member still act on this? The relay decides for real; this only
    /// keeps the UI from offering a button that is certain to be refused.
    public var isActionable: Bool { outcome == .pending }
}

/// Folds approval events into the requests a member can act on.
///
/// The whole lifecycle is on the wire: the relay signs the request (46010)
/// and the outcome (46011/46012), and the member signs the decision
/// (46030/46031) that causes the outcome. So the client can rebuild the
/// queue from events alone, exactly as `MeshFold` rebuilds work threads.
///
/// One thing is *not* on the wire: an agent that withdraws its own request
/// (`POST /api/approvals/resolve`) updates the row and emits nothing. Such a
/// request keeps rendering as pending until it expires, and a member who
/// acts on it gets a refusal with the relay's reason. Documented rather than
/// papered over — the fix belongs on the relay, which is the only party that
/// knows.
public enum MeshApprovalFold {
    /// Every agent-domain approval visible in `events`, newest request first.
    ///
    /// - Parameter now: the instant to judge expiry against. Injected so the
    ///   expiry rule is testable without waiting for a clock.
    public static func approvals(from events: [MeshEvent], now: Date = Date()) -> [MeshApproval] {
        var byToken: [String: MeshApproval] = [:]

        // Requests first: an outcome for a request this page does not
        // contain describes something the member cannot see, and inventing a
        // row from it would render an approval nobody asked for.
        for event in events where event.kind == MeshKind.approvalRequested {
            guard let approval = request(from: event) else { continue }
            byToken[approval.tokenHash] = approval
        }

        for event in events {
            let granted = event.kind == MeshKind.approvalGranted
            let denied = event.kind == MeshKind.approvalDenied
            guard granted || denied,
                isAgentDomain(event),
                let token = event.tagValue("d"),
                var approval = byToken[token]
            else { continue }
            let approver = body(of: event)?["approver"] as? String ?? ""
            approval.outcome = granted ? .granted(by: approver) : .denied(by: approver)
            byToken[token] = approval
        }

        // Expiry last, and only against requests still pending: a request
        // decided a second before its deadline is decided, not expired.
        for (token, var approval) in byToken {
            if approval.outcome == .pending, let expires = approval.expiresAt, expires <= now {
                approval.outcome = .expired
                byToken[token] = approval
            }
        }

        // Newest first, ties broken by token hash.
        //
        // The tie-break is not tidiness. `Dictionary.values` has no defined
        // order and Swift's `sort` is not stable, so two requests made in
        // the same second come back in a different order on every fold —
        // and this list re-folds on every relay push. A card that moves
        // between the moment a member reads it and the moment they click
        // approves the wrong thing, which is the one failure this surface
        // must not have.
        return byToken.values.sorted {
            $0.requestedAt != $1.requestedAt
                ? $0.requestedAt > $1.requestedAt
                : $0.tokenHash < $1.tokenHash
        }
    }

    /// Decode a kind:46010 into an agent-domain request, or `nil`.
    ///
    /// Returns `nil` for the workflow approval gate, which shares this kind
    /// and carries an entirely different content shape (including, unlike
    /// this path, the raw token). Folding on the kind alone would render
    /// workflow gates as agent requests with every field empty.
    static func request(from event: MeshEvent) -> MeshApproval? {
        guard isAgentDomain(event),
            let token = event.tagValue("d"),
            let channel = event.tagValue("h"),
            let body = body(of: event)
        else { return nil }
        return MeshApproval(
            tokenHash: token,
            requestID: body["request_id"] as? String ?? "",
            channelID: channel,
            agentPubkey: event.tagValue("p") ?? "",
            kind: MeshApprovalKind(wire: body["request_kind"] as? String),
            toolName: body["tool_name"] as? String,
            detail: body["detail"] as? String ?? "",
            requestedAt: event.createdAt,
            expiresAt: (body["expires_at"] as? String).flatMap(parseTimestamp),
            outcome: .pending)
    }

    private static func isAgentDomain(_ event: MeshEvent) -> Bool {
        body(of: event)?["domain"] as? String == "agent"
    }

    private static func body(of event: MeshEvent) -> [String: Any]? {
        guard let data = event.content.data(using: .utf8) else { return nil }
        return try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    }

    /// Parse the relay's RFC 3339 timestamps.
    ///
    /// Both forms must be accepted. `chrono`'s `to_rfc3339()` emits
    /// sub-second precision whenever the value has any — which `Utc::now()`
    /// always does — but a timestamp that lands exactly on a second does
    /// not. `ISO8601DateFormatter` is strict about which it will take, so a
    /// single formatter silently fails on roughly one input in a billion,
    /// and on *every* input if the wrong one is chosen.
    static func parseTimestamp(_ value: String) -> Date? {
        let attempts: [ISO8601DateFormatter.Options] = [
            [.withInternetDateTime, .withFractionalSeconds],
            [.withInternetDateTime],
        ]
        for options in attempts {
            let formatter = ISO8601DateFormatter()
            formatter.formatOptions = options
            if let date = formatter.date(from: value) { return date }
        }
        return nil
    }
}
