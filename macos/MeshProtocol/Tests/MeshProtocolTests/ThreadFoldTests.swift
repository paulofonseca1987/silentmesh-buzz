import Foundation
import Testing

@testable import MeshProtocol

/// The fold is what makes the client able to disagree with a relay: it
/// reproduces the projection from signed events rather than trusting a
/// server's summary. These tests pin the rules that make the reproduction
/// faithful — especially the ordering one, which is where it broke in the
/// Rust CLI.
@Suite("Thread fold")
struct ThreadFoldTests {
    private func event(
        id: String, kind: Int, at: Int64, tags: [[String]] = [], content: String = "",
        pubkey: String = String(repeating: "aa", count: 32)
    ) -> MeshEvent {
        MeshEvent(
            id: id, pubkey: pubkey, createdAt: at, kind: kind, tags: tags, content: content,
            sig: String(repeating: "00", count: 64))
    }

    @Test("a root becomes an open thread carrying its task framing")
    func rootFold() {
        let root = event(
            id: "r1", kind: MeshKind.workThreadOpen, at: 100,
            tags: [["h", "chan"], ["deadline", "1900000000"], ["dri", "beef"]],
            content: "Fix the parser")
        let threads = MeshFold.threads(from: [root])
        #expect(threads.count == 1)
        let thread = threads[0]
        #expect(thread.goal == "Fix the parser")
        #expect(thread.status == .open)
        #expect(thread.deadline == 1_900_000_000)
        #expect(thread.dri == "beef")
        #expect(thread.channelID == "chan")
    }

    @Test("state commands move the thread through D41")
    func stateFold() {
        let root = event(id: "r1", kind: MeshKind.workThreadOpen, at: 100, tags: [["h", "c"]])
        let ready = event(
            id: "s1", kind: MeshKind.workThreadState, at: 110,
            tags: [["e", "r1"], ["state", "ready"]])
        let closed = event(
            id: "s2", kind: MeshKind.workThreadState, at: 120,
            tags: [["e", "r1"], ["state", "closed"]])
        let thread = MeshFold.threads(from: [closed, root, ready])[0]
        #expect(thread.status == .closed)
    }

    /// The bug this rule exists for: a relay notice is always written after
    /// the command it reports on, but timestamps have one-second
    /// granularity. Ordered naively, a same-second command wins and a
    /// thread the relay archived keeps rendering as open.
    @Test("a same-second relay notice beats a client command")
    func sameSecondTieGoesToTheNotice() {
        let root = event(id: "r1", kind: MeshKind.workThreadOpen, at: 100, tags: [["h", "c"]])
        let reopen = event(
            id: "aaa", kind: MeshKind.workThreadState, at: 150,
            tags: [["e", "r1"], ["state", "open"]])
        let archived = event(
            id: "000", kind: MeshKind.workThreadSiblingArchived, at: 150, tags: [["e", "r1"]])
        // Event id "000" sorts before "aaa", so a naive (time, id) sort
        // would apply the notice first and let the command win.
        let thread = MeshFold.threads(from: [root, archived, reopen])[0]
        #expect(thread.status == .archived)
    }

    @Test("metadata distinguishes 'clear this field' from 'leave it alone'")
    func metadataNullVersusAbsent() {
        let root = event(
            id: "r1", kind: MeshKind.workThreadOpen, at: 100,
            tags: [["h", "c"], ["deadline", "1900000000"], ["dri", "beef"]], content: "original")
        let editGoalOnly = event(
            id: "m1", kind: MeshKind.workThreadMetadata, at: 110, tags: [["e", "r1"]],
            content: #"{"goal":"revised"}"#)
        var thread = MeshFold.threads(from: [root, editGoalOnly])[0]
        #expect(thread.goal == "revised")
        #expect(thread.deadline == 1_900_000_000, "an absent field must not clear anything")
        #expect(thread.dri == "beef")

        let clearDeadline = event(
            id: "m2", kind: MeshKind.workThreadMetadata, at: 120, tags: [["e", "r1"]],
            content: #"{"deadline":null}"#)
        thread = MeshFold.threads(from: [root, editGoalOnly, clearDeadline])[0]
        #expect(thread.deadline == nil, "an explicit null must clear")
        #expect(thread.goal == "revised")
    }

    @Test("a fork root carries its provenance")
    func forkProvenance() {
        let parent = event(id: "p1", kind: MeshKind.workThreadOpen, at: 100, tags: [["h", "c"]])
        let fork = event(
            id: "f1", kind: MeshKind.workThreadFork, at: 200,
            tags: [["h", "c"], ["e", "p1"], ["commit", String(repeating: "d", count: 40)]],
            content: "Try approach B")
        let threads = MeshFold.threads(from: [parent, fork])
        let forked = threads.first { $0.id == "f1" }
        #expect(forked?.forkedFrom == "p1")
        #expect(forked?.forkCommit == String(repeating: "d", count: 40))
        // The fork's own `e` tag must not be mistaken for a command
        // against the parent — the parent stays open.
        #expect(threads.first { $0.id == "p1" }?.status == .open)
    }

    @Test("a promotion root records where it came from and closes the source")
    func promotionFold() {
        let source = event(id: "s1", kind: MeshKind.workThreadOpen, at: 100, tags: [["h", "personal"]])
        let promotion = event(
            id: "n1", kind: MeshKind.workThreadPromote, at: 300,
            tags: [["h", "team"], ["e", "s1"], ["from", "personal"]], content: "Importer fix")
        // The relay's own notice, in the SOURCE channel.
        let notice = event(
            id: "z1", kind: MeshKind.workThreadPromoted, at: 300,
            tags: [["e", "s1"], ["h", "personal"]])
        let threads = MeshFold.threads(from: [source, promotion, notice])
        let promoted = threads.first { $0.id == "n1" }
        #expect(promoted?.promotedFrom == "s1")
        #expect(promoted?.promotedChannel == "personal")
        #expect(promoted?.channelID == "team")
        #expect(threads.first { $0.id == "s1" }?.status == .closed)
    }

    @Test("checkpoints accumulate in order and notices are collected")
    func checkpointsAndNotices() {
        let root = event(id: "r1", kind: MeshKind.workThreadOpen, at: 100, tags: [["h", "c"]])
        let cp1 = event(
            id: "c1", kind: MeshKind.workThreadCheckpoint, at: 110,
            tags: [["e", "r1"], ["commit", "aaa"]])
        let cp2 = event(
            id: "c2", kind: MeshKind.workThreadCheckpoint, at: 120,
            tags: [["e", "r1"], ["commit", "bbb"]])
        let overdue = event(
            id: "o1", kind: MeshKind.workThreadOverdue, at: 130, tags: [["e", "r1"]])
        let review = event(
            id: "g1", kind: MeshKind.workThreadGateReviewed, at: 140, tags: [["e", "r1"]],
            content: #"{"assist":"ok"}"#)
        let thread = MeshFold.threads(from: [cp2, review, root, overdue, cp1])[0]
        #expect(thread.checkpoints == ["aaa", "bbb"])
        #expect(thread.notices.map(\.kind) == [.overdue, .gateReview])
        #expect(thread.status == .open, "notices that only inform must not change state")
    }

    @Test("commands for an unknown root are ignored, not invented into threads")
    func orphanCommandsIgnored() {
        let orphan = event(
            id: "s1", kind: MeshKind.workThreadState, at: 110,
            tags: [["e", "nope"], ["state", "closed"]])
        #expect(MeshFold.threads(from: [orphan]).isEmpty)
    }
}
