import Foundation
import Testing

@testable import MeshProtocol

@Suite("Event builders")
struct EventBuilderTests {
    private let pubkey = String(repeating: "ab", count: 32)
    private let channel = "5b1e7c05-0000-4000-8000-000000000000"
    private let root = String(repeating: "cd", count: 32)

    @Test("a message carries its channel and signs into a verifiable event")
    func chatMessage() throws {
        let keys = try MeshKeys()
        var event = try MeshEvent.chatMessage(
            channel: channel, content: "  hello mesh  ", pubkey: keys.publicKeyHex)
        #expect(event.kind == MeshKind.chatMessage)
        #expect(event.tagValue("h") == channel)
        #expect(event.content == "hello mesh", "surrounding whitespace is trimmed")
        try event.sign(with: keys)
        #expect(event.isValid())
    }

    @Test("an empty message is refused locally, not by the relay")
    func emptyMessageRefused() {
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.chatMessage(channel: channel, content: "   ", pubkey: pubkey)
        }
    }

    @Test("a reply threads under a well-formed event id")
    func replyThreading() throws {
        let event = try MeshEvent.chatMessage(
            channel: channel, content: "reply", replyTo: root, pubkey: pubkey)
        #expect(event.tagValue("e") == root)
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.chatMessage(
                channel: channel, content: "reply", replyTo: "nope", pubkey: pubkey)
        }
    }

    @Test("a work thread carries goal, deadline and DRI")
    func openThread() throws {
        let deadline = Date(timeIntervalSince1970: 1_900_000_000)
        let event = try MeshEvent.openThread(
            channel: channel, goal: "Fix the parser", deadline: deadline, dri: root,
            pubkey: pubkey)
        #expect(event.kind == MeshKind.workThreadOpen)
        #expect(event.content == "Fix the parser")
        #expect(event.tagValue("deadline") == "1900000000")
        #expect(event.tagValue("dri") == root)

        // A thread with no goal is one nobody can act on.
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.openThread(channel: channel, goal: "", pubkey: pubkey)
        }
        // A malformed DRI is caught here rather than as a relay refusal.
        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.openThread(
                channel: channel, goal: "g", dri: "short", pubkey: pubkey)
        }
    }

    @Test("a gate review may carry an empty draft — that asks for one")
    func gateReview() throws {
        let asked = try MeshEvent.gateReview(
            channel: channel, threadRoot: root, pubkey: pubkey)
        #expect(asked.kind == MeshKind.workThreadGateReview)
        #expect(asked.content.isEmpty)
        #expect(asked.tagValue("e") == root)
        #expect(asked.tagValue("h") == channel)

        let drafted = try MeshEvent.gateReview(
            channel: channel, threadRoot: root, draftSummary: "Parser fix", pubkey: pubkey)
        #expect(drafted.content == "Parser fix")

        #expect(throws: MeshProtocolError.self) {
            try MeshEvent.gateReview(channel: channel, threadRoot: "bad", pubkey: pubkey)
        }
    }
}
