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
    public static func chatMessage(
        channel: String,
        content: String,
        replyTo: String? = nil,
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
}
