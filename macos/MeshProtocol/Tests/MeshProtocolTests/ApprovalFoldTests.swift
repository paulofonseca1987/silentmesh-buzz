import Foundation
import Testing

@testable import MeshProtocol

/// The approval queue is the one surface where a member's click has an
/// irreversible effect on someone else's machine, so the rules about what is
/// *actionable* matter more than the rendering.
@Suite("Approval fold")
struct ApprovalFoldTests {
    private let agent = String(repeating: "ab", count: 32)
    private let token = String(repeating: "cd", count: 32)

    private func event(
        id: String, kind: Int, at: Int64, tags: [[String]] = [], content: String = ""
    ) -> MeshEvent {
        MeshEvent(
            id: id, pubkey: String(repeating: "ff", count: 32), createdAt: at, kind: kind,
            tags: tags, content: content, sig: String(repeating: "00", count: 64))
    }

    private func requestEvent(
        token: String? = nil, at: Int64 = 100, kind: String = "command",
        tool: String? = "shell", detail: String = "rm -rf build/",
        expires: String = "2099-01-01T00:00:00+00:00", domain: String = "agent"
    ) -> MeshEvent {
        var body: [String: Any] = [
            "domain": domain,
            "request_id": "11111111-1111-1111-1111-111111111111",
            "request_kind": kind,
            "detail": detail,
            "expires_at": expires,
        ]
        body["tool_name"] = tool ?? NSNull()
        let json = String(
            data: try! JSONSerialization.data(withJSONObject: body), encoding: .utf8)!
        return event(
            id: "req-\(at)", kind: MeshKind.approvalRequested, at: at,
            tags: [["d", token ?? self.token], ["h", "chan"], ["p", agent]],
            content: json)
    }

    private func outcomeEvent(
        granted: Bool, token: String? = nil, at: Int64 = 200, approver: String = "deadbeef",
        domain: String = "agent"
    ) -> MeshEvent {
        let body: [String: Any] = [
            "domain": domain,
            "request_id": "11111111-1111-1111-1111-111111111111",
            "status": granted ? "granted" : "denied",
            "decision": granted ? "allow_once" : "reject_once",
            "approver": approver,
        ]
        let json = String(
            data: try! JSONSerialization.data(withJSONObject: body), encoding: .utf8)!
        return event(
            id: "out-\(at)",
            kind: granted ? MeshKind.approvalGranted : MeshKind.approvalDenied, at: at,
            tags: [["d", token ?? self.token], ["e", "cmd"], ["h", "chan"], ["p", agent]],
            content: json)
    }

    @Test("a request folds into an actionable approval")
    func pendingRequest() {
        let approvals = MeshApprovalFold.approvals(from: [requestEvent()])
        #expect(approvals.count == 1)
        let approval = approvals[0]
        #expect(approval.outcome == .pending)
        #expect(approval.isActionable)
        #expect(approval.kind == .command)
        #expect(approval.toolName == "shell")
        #expect(approval.detail == "rm -rf build/")
        #expect(approval.agentPubkey == agent)
        #expect(approval.channelID == "chan")
        #expect(approval.tokenHash == token)
    }

    /// Kind 46010 is shared with the workflow approval gate, which carries a
    /// different content shape — including the raw token. Folding on the
    /// kind alone would render those as agent requests with every field
    /// blank, and offer a decision the agent path cannot execute.
    @Test("a workflow-domain request on the same kind is not folded")
    func workflowDomainIgnored() {
        let workflow = event(
            id: "wf", kind: MeshKind.approvalRequested, at: 100,
            tags: [["d", token], ["h", "chan"]],
            content: #"{"message":"deploy?","token":"raw-secret","expires_at":"2099-01-01T00:00:00Z"}"#)
        #expect(MeshApprovalFold.approvals(from: [workflow]).isEmpty)
    }

    @Test("an outcome resolves its request and takes it out of play")
    func outcomeResolves() {
        let granted = MeshApprovalFold.approvals(from: [requestEvent(), outcomeEvent(granted: true)])
        #expect(granted[0].outcome == .granted(by: "deadbeef"))
        #expect(granted[0].isActionable == false)

        let denied = MeshApprovalFold.approvals(from: [requestEvent(), outcomeEvent(granted: false)])
        #expect(denied[0].outcome == .denied(by: "deadbeef"))
        #expect(denied[0].isActionable == false)
    }

    /// The gap that used to exist: an agent withdrew its request, the relay
    /// updated the row and said nothing, and the card stayed clickable until
    /// it expired. Now the relay emits kind:46013 and the queue retires it.
    @Test("a withdrawn request stops being actionable")
    func withdrawalRetiresTheRequest() {
        let withdrawal = event(
            id: "wd", kind: MeshKind.approvalWithdrawn, at: 200,
            tags: [["d", token], ["h", "chan"], ["p", agent]],
            content: #"{"domain":"agent","request_id":"1","status":"cancelled"}"#)
        let approvals = MeshApprovalFold.approvals(from: [requestEvent(), withdrawal])
        #expect(approvals[0].outcome == .withdrawn(status: "cancelled"))
        #expect(approvals[0].isActionable == false)
    }

    /// The relay distinguishes "the agent took it back" from "it lapsed", and
    /// a member reads those differently — so the client repeats the relay's
    /// word instead of collapsing both into one label.
    @Test("the relay's reason for withdrawal is carried, not flattened")
    func withdrawalCarriesItsReason() {
        let lapsed = event(
            id: "wd", kind: MeshKind.approvalWithdrawn, at: 200,
            tags: [["d", token], ["h", "chan"]],
            content: #"{"domain":"agent","request_id":"1","status":"expired"}"#)
        let approvals = MeshApprovalFold.approvals(from: [requestEvent(), lapsed])
        #expect(approvals[0].outcome == .withdrawn(status: "expired"))
    }

    /// An outcome whose request is not on this page describes something the
    /// member cannot see. Inventing a row from it would show an approval
    /// nobody asked for, with no detail to judge it by.
    @Test("an orphan outcome creates nothing")
    func orphanOutcome() {
        #expect(MeshApprovalFold.approvals(from: [outcomeEvent(granted: true)]).isEmpty)
    }

    @Test("an outcome only resolves the request it names")
    func outcomeMatchesOnToken() {
        let other = String(repeating: "ee", count: 32)
        let approvals = MeshApprovalFold.approvals(from: [
            requestEvent(), requestEvent(token: other, at: 110),
            outcomeEvent(granted: true, token: other),
        ])
        let mine = approvals.first { $0.tokenHash == token }
        let theirs = approvals.first { $0.tokenHash == other }
        #expect(mine?.outcome == .pending)
        #expect(theirs?.outcome == .granted(by: "deadbeef"))
    }

    /// The relay emits nothing when a request lapses, so expiry is the
    /// client's job. Without it a dead request stays on screen as a live
    /// decision, and the member finds out by being refused.
    @Test("a lapsed request stops being actionable without any event saying so")
    func expiryIsClientSide() {
        let request = requestEvent(expires: "2020-01-01T00:00:00+00:00")
        let approvals = MeshApprovalFold.approvals(from: [request])
        #expect(approvals[0].outcome == .expired)
        #expect(approvals[0].isActionable == false)
    }

    /// A decision that landed before the deadline is decided, not expired —
    /// otherwise the queue would relabel history every time it re-folded.
    @Test("a decided request is not relabelled expired afterwards")
    func decisionOutranksExpiry() {
        let approvals = MeshApprovalFold.approvals(from: [
            requestEvent(expires: "2020-01-01T00:00:00+00:00"),
            outcomeEvent(granted: true),
        ])
        #expect(approvals[0].outcome == .granted(by: "deadbeef"))
    }

    /// `chrono`'s `to_rfc3339()` emits sub-second digits whenever the value
    /// has any — which `Utc::now()` always does — and omits them when it
    /// does not. A formatter that accepts only one shape rejects the other,
    /// and a rejected deadline silently becomes "no deadline".
    @Test("both RFC 3339 shapes the relay can emit are parsed")
    func timestampShapes() {
        let withFraction = MeshApprovalFold.parseTimestamp("2026-07-31T15:04:05.123456789+00:00")
        let withoutFraction = MeshApprovalFold.parseTimestamp("2026-07-31T15:04:05+00:00")
        let zulu = MeshApprovalFold.parseTimestamp("2026-07-31T15:04:05Z")
        #expect(withFraction != nil)
        #expect(withoutFraction != nil)
        #expect(zulu != nil)
        #expect(MeshApprovalFold.parseTimestamp("not a date") == nil)
    }

    /// An unparseable deadline must not expire the request: refusing to show
    /// a live decision is worse than showing a dead one, and the relay is
    /// the authority either way.
    @Test("an unreadable deadline leaves the request actionable")
    func unparseableDeadlineIsNotExpiry() {
        let approvals = MeshApprovalFold.approvals(from: [requestEvent(expires: "soon")])
        #expect(approvals[0].expiresAt == nil)
        #expect(approvals[0].isActionable)
    }

    @Test("an unknown request kind is carried, not dropped")
    func unknownKindSurvives() {
        let approvals = MeshApprovalFold.approvals(from: [requestEvent(kind: "teleport")])
        #expect(approvals.count == 1)
        #expect(approvals[0].kind == .other)
        #expect(approvals[0].detail == "rm -rf build/")
    }

    @Test("a missing tool name is absent rather than empty")
    func nullToolName() {
        let approvals = MeshApprovalFold.approvals(from: [requestEvent(tool: nil)])
        #expect(approvals[0].toolName == nil)
    }

    @Test("newest request first")
    func ordering() {
        let old = requestEvent(token: String(repeating: "11", count: 32), at: 100)
        let new = requestEvent(token: String(repeating: "22", count: 32), at: 300)
        let approvals = MeshApprovalFold.approvals(from: [old, new])
        #expect(approvals.map(\.requestedAt) == [300, 100])
    }

    /// Requests made in the same second must not shuffle between folds.
    ///
    /// The queue re-folds on every relay push, and it is built through a
    /// dictionary — whose `values` have no defined order — then sorted with
    /// a sort that is not stable. Without a tie-break a card can move
    /// between the moment a member reads it and the moment they click, which
    /// on this surface means approving the wrong thing. Three requests 150 ms
    /// apart, as the live tests create, all land in the same second.
    @Test("same-second requests are ordered by token hash, not by chance")
    func sameSecondOrderIsDeterministic() {
        let tokens = (0..<8).map { String(repeating: String(format: "%02x", $0 + 0x10), count: 32) }
        let events = tokens.shuffled().map { requestEvent(token: $0, at: 500) }
        let folded = MeshApprovalFold.approvals(from: events).map(\.tokenHash)
        // Assert the *specified* order, not merely that two folds agree.
        // Re-folding the same events in one process gives the same
        // dictionary order whether or not the tie-break exists, so a
        // self-consistency check passes with the bug in place — it did.
        // The cross-process reordering that motivated this cannot be
        // reproduced in a single test process, so the contract is what gets
        // asserted.
        #expect(folded == tokens.sorted(), "same-second requests came back in an unspecified order")
    }
}

@Suite("Approval decisions")
struct ApprovalBuilderTests {
    private let pubkey = String(repeating: "ab", count: 32)
    private let token = String(repeating: "cd", count: 32)

    @Test("a grant and a deny differ only in kind")
    func decisionShape() throws {
        let grant = try MeshEvent.approvalDecision(
            tokenHash: token, grant: true, pubkey: pubkey)
        let deny = try MeshEvent.approvalDecision(
            tokenHash: token, grant: false, note: "not on prod", pubkey: pubkey)
        #expect(grant.kind == 46030)
        #expect(deny.kind == 46031)
        // `d` and nothing else: the relay resolves the channel from the
        // stored request, and buzz-sdk's builder tags exactly this.
        #expect(grant.tags == [["d", token]])
        #expect(grant.content == "")
        #expect(deny.content == "not on prod")
    }

    /// The relay answers a bad hash with "approval not found", which reads
    /// like the request is gone rather than like the client sent nonsense.
    /// Catching it locally says the true thing.
    @Test("a token hash that is not 64 hex is refused before signing")
    func rejectsBadTokenHash() {
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.approvalDecision(tokenHash: "cafe", grant: true, pubkey: pubkey)
        }
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.approvalDecision(
                tokenHash: String(repeating: "zz", count: 32), grant: true, pubkey: pubkey)
        }
    }
}
