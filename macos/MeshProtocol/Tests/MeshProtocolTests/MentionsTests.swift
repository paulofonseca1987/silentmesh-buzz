import Foundation
import Testing

@testable import MeshProtocol

/// Mention resolution decides whether an agent wakes at all, and the rule
/// is shared with the CLI, the desktop app and the workflow engine — an
/// agent that wakes for one client and not another is worse than one that
/// never wakes. These pin the parts of `extract_at_mentions_with_known`
/// that are easy to get subtly wrong.
@Suite("Mentions")
struct MentionsTests {
    private func profile(_ pubkey: String, displayName: String? = nil, name: String? = nil)
        -> MeshEvent
    {
        var body: [String: Any] = [:]
        if let displayName { body["display_name"] = displayName }
        if let name { body["name"] = name }
        let json = String(data: try! JSONSerialization.data(withJSONObject: body), encoding: .utf8)!
        return MeshEvent(
            id: "p-\(pubkey)", pubkey: pubkey, createdAt: 1, kind: MeshKind.profile,
            tags: [], content: json, sig: "")
    }

    @Test("a bare @name is picked up")
    func singleWord() {
        #expect(MeshMentions.extract(from: "@Mesh please look", known: []) == ["mesh"])
    }

    /// An `@` mid-word is an email address or a handle inside a URL, not a
    /// mention. Waking an agent because someone typed an address would be
    /// both wrong and hard to explain.
    @Test("an @ that is not at a word start is not a mention")
    func mustFollowWhitespaceOrStart() {
        #expect(MeshMentions.extract(from: "mail me at bob@mesh.example", known: []).isEmpty)
        #expect(MeshMentions.extract(from: "see docs/@mesh", known: []).isEmpty)
    }

    /// Known names are tried longest-first, so a multi-word display name
    /// binds as a whole and a bare first name does not steal it.
    @Test("a known multi-word name beats the single-word fallback")
    func longestKnownNameWins() {
        let known = ["Will Pfleger", "Will"]
        #expect(MeshMentions.extract(from: "@Will Pfleger ping", known: known) == ["will pfleger"])
        // With only the longer name known, a bare @Will falls back to the
        // single word rather than binding the member.
        #expect(MeshMentions.extract(from: "@Will ping", known: ["Will Pfleger"]) == ["will"])
    }

    /// Without the boundary check, longest-first is the only defence — and
    /// it fails as soon as the longer name is not a channel member.
    @Test("a known name must end on a word boundary")
    func wordBoundaryRequired() {
        // "Ann" must not match inside "Annabel".
        #expect(MeshMentions.extract(from: "@Annabel hi", known: ["Ann"]) == ["annabel"])
        // Punctuation is a boundary.
        #expect(MeshMentions.extract(from: "thanks @Ann!", known: ["Ann"]) == ["ann"])
        #expect(MeshMentions.extract(from: "(@Ann)", known: ["Ann"]).isEmpty)  // '(' is not whitespace
    }

    @Test("names come back lowercased, first-seen order, deduplicated")
    func normalisation() {
        #expect(
            MeshMentions.extract(from: "@Bob @alice @BOB", known: []) == ["bob", "alice"])
    }

    /// `display_name` wins; `name` is the fallback only when it is absent.
    @Test("display_name wins over name")
    func profileNamePreference() {
        let a = String(repeating: "aa", count: 32)
        let index = MeshMentions.nameIndex(profiles: [
            profile(a, displayName: "Mesh", name: "ignored")
        ])
        #expect(index["mesh"] == [a])
        #expect(index["ignored"] == nil)

        let b = String(repeating: "bb", count: 32)
        let fallback = MeshMentions.nameIndex(profiles: [profile(b, name: "OnlyName")])
        #expect(fallback["onlyname"] == [b])
    }

    /// Two members answering to one name both get tagged. Silently waking
    /// nobody would be the surprising behaviour, and it is not what the CLI
    /// does.
    @Test("an ambiguous name addresses everyone who answers to it")
    func ambiguityTagsAll() {
        let a = String(repeating: "aa", count: 32)
        let b = String(repeating: "bb", count: 32)
        let resolved = MeshMentions.resolve(
            content: "@Mesh hello",
            profiles: [profile(a, displayName: "Mesh"), profile(b, displayName: "mesh")])
        #expect(Set(resolved) == Set([a, b]))
    }

    @Test("a mention of nobody resolves to nobody")
    func unknownNameResolvesEmpty() {
        let a = String(repeating: "aa", count: 32)
        #expect(
            MeshMentions.resolve(
                content: "@nobody there", profiles: [profile(a, displayName: "Mesh")]
            ).isEmpty)
    }

    /// The whole point: the composer must emit a `p` tag, because that and
    /// nothing else is what wakes an agent.
    @Test("a resolved mention becomes a p tag on the message")
    func composerEmitsPTags() throws {
        let agent = String(repeating: "ab", count: 32)
        let event = try MeshEvent.chatMessage(
            channel: "chan", content: "@Mesh do the thing", mentions: [agent],
            pubkey: String(repeating: "cd", count: 32))
        #expect(event.tags.contains(["p", agent]))
        #expect(event.tags.contains(["h", "chan"]))
    }

    /// A malformed pubkey must not reach the relay as a tag it will refuse.
    @Test("a non-hex mention is dropped rather than published")
    func malformedMentionsDropped() throws {
        let event = try MeshEvent.chatMessage(
            channel: "chan", content: "hi", mentions: ["not-a-pubkey", "zz"],
            pubkey: String(repeating: "cd", count: 32))
        #expect(event.tags.allSatisfy { $0.first != "p" })
    }
}
