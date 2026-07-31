import Foundation

/// Builders for the events a client authors.
///
/// Kept pure and separate from the transport so the shape of what we sign
/// is testable without a relay — and so a malformed event is caught here
/// rather than discovered as a refusal after a round trip.
///
/// These mirror `buzz-sdk`'s builders. Where the Rust side validates
/// (non-empty content, hex shapes, tag allowlists), so does this: the
/// relay will refuse the same things, and finding out locally is faster
/// and clearer than reading a rejection.
extension MeshEvent {
    /// A channel message (kind 9), optionally threading under `replyTo`.
    /// - Parameter mentions: pubkeys to address, as `p` tags. An agent is
    ///   woken by a `p` tag naming it and by nothing else, so a message
    ///   without these can be read by an agent but never addressed to one.
    public static func chatMessage(
        channel: String,
        content: String,
        replyTo: String? = nil,
        mentions: [String] = [],
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw MeshProtocolError.malformed("a message needs content")
        }
        var tags = [["h", channel]]
        if let replyTo {
            guard replyTo.count == 64, replyTo.allSatisfy(\.isHexDigit) else {
                throw MeshProtocolError.malformed("replyTo must be a 64-hex event id")
            }
            tags.append(["e", replyTo])
        }
        for mention in mentions where mention.count == 64 && mention.allSatisfy(\.isHexDigit) {
            tags.append(["p", mention])
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: MeshKind.chatMessage,
            tags: tags,
            content: trimmed)
    }

    /// A work-thread root (kind 47000): a task with a goal, and optionally
    /// a deadline and a DRI.
    ///
    /// The goal is the event's content and cannot be empty — a thread
    /// without one is a thread nobody can act on, and the relay refuses it
    /// too (D40).
    public static func openThread(
        channel: String,
        goal: String,
        deadline: Date? = nil,
        dri: String? = nil,
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        let trimmed = goal.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw MeshProtocolError.malformed("a work thread needs a goal")
        }
        var tags = [["h", channel]]
        if let deadline {
            tags.append(["deadline", String(Int64(deadline.timeIntervalSince1970))])
        }
        if let dri {
            guard dri.count == 64, dri.allSatisfy(\.isHexDigit) else {
                throw MeshProtocolError.malformed("dri must be a 64-hex pubkey")
            }
            tags.append(["dri", dri])
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: MeshKind.workThreadOpen,
            tags: tags,
            content: trimmed)
    }

    /// A Privacy Gate pre-flight review request (kind 47022, D30).
    ///
    /// Content is the draft summary and may be empty — that asks the gate
    /// to draft one. Read-only on the relay: nothing moves, nothing closes.
    public static func gateReview(
        channel: String,
        threadRoot: String,
        draftSummary: String = "",
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        guard threadRoot.count == 64, threadRoot.allSatisfy(\.isHexDigit) else {
            throw MeshProtocolError.malformed("threadRoot must be a 64-hex event id")
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: MeshKind.workThreadGateReview,
            tags: [["e", threadRoot], ["h", channel]],
            content: draftSummary)
    }

    /// A NIP-25 reaction (kind 7) to an event.
    ///
    /// Mirrors `buzz_sdk::build_reaction`: content is the emoji, and the only
    /// tag is `e` at the target. Note there is **no `h` tag** — the relay
    /// derives the channel from the target event, and a channel-scoped
    /// subscription still matches through its stored-`channel_id` fallback.
    public static func reaction(
        to eventID: String,
        emoji: String,
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        guard eventID.count == 64, eventID.allSatisfy(\.isHexDigit) else {
            throw MeshProtocolError.malformed("a reaction needs a 64-hex target event id")
        }
        guard !emoji.isEmpty, emoji.count <= 64 else {
            throw MeshProtocolError.malformed("a reaction's emoji must be 1...64 characters")
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: MeshKind.reaction,
            tags: [["e", eventID]],
            content: emoji)
    }

    /// A NIP-09 deletion (kind 5) retiring one's own earlier event.
    ///
    /// Mirrors `buzz_sdk::build_remove_reaction`. The `e` tag names the
    /// **reaction** being withdrawn, not whatever that reaction was about —
    /// the indirection `MeshTurnFold` has to follow to know a turn ended.
    public static func removeReaction(
        _ reactionEventID: String,
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        guard reactionEventID.count == 64, reactionEventID.allSatisfy(\.isHexDigit) else {
            throw MeshProtocolError.malformed("a deletion needs a 64-hex reaction event id")
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: MeshKind.deletion,
            tags: [["e", reactionEventID]],
            content: "")
    }

    /// A member's decision on a pending approval — kind 46030 (grant) or
    /// 46031 (deny).
    ///
    /// `tokenHash` is the request's `d` tag: a SHA-256 digest, so the member
    /// references the request without ever holding the token that authorizes
    /// it. The 64-hex check mirrors `build_workflow_approval` in `buzz-sdk`,
    /// which refuses anything else — and a wrong-length hash would otherwise
    /// come back as "approval not found", which reads like the request is
    /// gone rather than like the client sent nonsense.
    ///
    /// No `h` tag: the relay resolves the channel from the stored request,
    /// and the same command path serves both approval domains. `note` is the
    /// content and may be empty.
    ///
    /// Whether the relay accepts this is a separate question the caller must
    /// still ask — an agent may not decide its own request, only full
    /// members of the request's channel may decide, and an expired or
    /// already-decided request is refused with a reason worth showing.
    public static func approvalDecision(
        tokenHash: String,
        grant: Bool,
        note: String = "",
        pubkey: String,
        at: Date = Date()
    ) throws -> MeshEvent {
        guard tokenHash.count == 64, tokenHash.allSatisfy(\.isHexDigit) else {
            throw MeshProtocolError.malformed(
                "an approval decision needs the request's 64-hex token hash")
        }
        return MeshEvent(
            pubkey: pubkey,
            createdAt: Int64(at.timeIntervalSince1970),
            kind: grant ? MeshKind.approvalGrant : MeshKind.approvalDeny,
            tags: [["d", tokenHash]],
            content: note)
    }
}
