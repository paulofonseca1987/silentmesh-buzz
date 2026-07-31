import Foundation
import Testing

@testable import MeshProtocol

/// The turn fold is the client's only cleartext window into what an agent is
/// doing. Everything richer (streamed text, tool calls) is encrypted to the
/// agent's owner, so if these five states are wrong a member has nothing.
@Suite("Turn fold")
struct TurnFoldTests {
    private let agent = String(repeating: "a9", count: 32)
    private let member = String(repeating: "b7", count: 32)
    private let trigger = String(repeating: "c1", count: 32)

    private func event(
        id: String, kind: Int, at: Int64, tags: [[String]] = [], content: String = "",
        pubkey: String? = nil
    ) -> MeshEvent {
        MeshEvent(
            id: id, pubkey: pubkey ?? agent, createdAt: at, kind: kind, tags: tags,
            content: content, sig: String(repeating: "00", count: 64))
    }

    /// A lifecycle reaction: kind:7, one `e` tag at the *message*.
    private func reaction(_ emoji: String, id: String, at: Int64, on target: String? = nil)
        -> MeshEvent
    {
        event(id: id, kind: MeshKind.reaction, at: at, tags: [["e", target ?? trigger]], content: emoji)
    }

    /// A removal: kind:5, one `e` tag at the *reaction event* — not at the
    /// message. This indirection is the fold's main hazard.
    private func removal(id: String, at: Int64, reaction reactionID: String) -> MeshEvent {
        event(id: id, kind: MeshKind.deletion, at: at, tags: [["e", reactionID]])
    }

    private func reply(id: String, at: Int64, content: String, from pubkey: String? = nil)
        -> MeshEvent
    {
        event(
            id: id, kind: MeshKind.chatMessage, at: at, tags: [["e", trigger], ["h", "chan"]],
            content: content, pubkey: pubkey)
    }

    @Test("👀 alone means queued")
    func queued() {
        let turns = MeshTurnFold.turns(from: [reaction("👀", id: "r1", at: 100)])
        #expect(turns.count == 1)
        #expect(turns[0].state == .queued)
        #expect(turns[0].isLive)
        // The agent is learned from who reacted, not from configuration.
        #expect(turns[0].agentPubkey == agent)
        #expect(turns[0].triggerEventID == trigger)
    }

    @Test("💬 means the model is being prompted")
    func working() {
        let turns = MeshTurnFold.turns(from: [
            reaction("👀", id: "r1", at: 100), reaction("💬", id: "r2", at: 101),
        ])
        #expect(turns[0].state == .working)
    }

    /// The removal points at the reaction, not at the message. A fold that
    /// matched deletions against the message id would find nothing and leave
    /// every turn reading as permanently working.
    @Test("a turn ends when its reactions are deleted by reaction id")
    func endsOnReactionDeletion() {
        let turns = MeshTurnFold.turns(from: [
            reaction("👀", id: "r1", at: 100),
            reaction("💬", id: "r2", at: 101),
            removal(id: "d1", at: 110, reaction: "r1"),
            removal(id: "d2", at: 110, reaction: "r2"),
        ])
        #expect(turns[0].state == .ended)
        #expect(turns[0].isLive == false)
    }

    /// The trap the two-hop correlation exists to avoid: a deletion aimed at
    /// the *message* must not retire the turn.
    @Test("a deletion aimed at the message does not end the turn")
    func deletionOfTheMessageIsNotTheTurnEnding() {
        let turns = MeshTurnFold.turns(from: [
            reaction("💬", id: "r2", at: 101),
            removal(id: "d1", at: 110, reaction: trigger),
        ])
        #expect(turns[0].state == .working)
    }

    @Test("the agent's reply is the answer")
    func answered() {
        let turns = MeshTurnFold.turns(from: [
            reaction("👀", id: "r1", at: 100),
            reaction("💬", id: "r2", at: 101),
            reply(id: "a1", at: 120, content: "Done — the parser now batches."),
        ])
        #expect(turns[0].state == .answered(eventID: "a1"))
        #expect(turns[0].isLive == false)
    }

    /// The reply is terminal and outranks the reactions. `clear_reactions` is
    /// fire-and-forget and may lag or be lost entirely, so waiting for it
    /// before showing an answer that already arrived leaves the turn reading
    /// as "working" forever.
    @Test("an answer outranks a 💬 that was never cleared")
    func answerOutranksStaleWorking() {
        let turns = MeshTurnFold.turns(from: [
            reaction("💬", id: "r2", at: 101),
            reply(id: "a1", at: 120, content: "here you go"),
        ])
        #expect(turns[0].state == .answered(eventID: "a1"))
    }

    /// On the tier-gate paths a refusal is the ONLY thing a turn emits — no
    /// answer, no metrics. Rendering it as an ordinary reply would bury the
    /// one message explaining why nothing happened.
    @Test("a ⚠️ reply is a refusal, not an answer")
    func refused() {
        let notice =
            "⚠️ This turn was blocked by privacy-tier enforcement: a `vendor` model is not allowed in an owned channel."
        let turns = MeshTurnFold.turns(from: [
            reaction("👀", id: "r1", at: 100),
            reply(id: "a1", at: 120, content: notice),
        ])
        #expect(turns[0].state == .refused(reason: notice))
        #expect(turns[0].isLive == false)
    }

    /// A member replying in-thread is conversation, not a turn result.
    @Test("only the agent's own reply resolves its turn")
    func otherMembersDoNotResolveTheTurn() {
        let turns = MeshTurnFold.turns(from: [
            reaction("💬", id: "r2", at: 101),
            reply(id: "m1", at: 120, content: "any luck?", from: member),
        ])
        #expect(turns[0].state == .working)
    }

    /// Members react to messages all the time. Reading a 🎉 as agent activity
    /// would invent turns that never happened.
    @Test("an ordinary emoji reaction is not a turn")
    func onlyLifecycleEmojiCount() {
        let turns = MeshTurnFold.turns(from: [
            event(
                id: "r9", kind: MeshKind.reaction, at: 100, tags: [["e", trigger]],
                content: "🎉", pubkey: member)
        ])
        #expect(turns.isEmpty)
    }

    @Test("turns are newest first, with a deterministic tie-break")
    func ordering() {
        let other = String(repeating: "d2", count: 32)
        let turns = MeshTurnFold.turns(from: [
            reaction("👀", id: "r1", at: 100),
            reaction("👀", id: "r2", at: 100, on: other),
        ])
        // Same second: order must come from the trigger id, not from
        // whatever the dictionary felt like.
        #expect(turns.map(\.triggerEventID) == [trigger, other].sorted())
    }
}
