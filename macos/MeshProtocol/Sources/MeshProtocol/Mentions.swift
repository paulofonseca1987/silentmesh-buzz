import Foundation

/// `@mention` resolution, mirroring `buzz_sdk::mentions`.
///
/// This is what makes the client able to *wake an agent*. An agent is woken
/// by a `p` tag naming it and by nothing else (`event_mentions_agent` in
/// `buzz-acp` checks exactly that), so a composer that publishes only an
/// `h` tag can talk in a channel but can never address anyone in it.
///
/// The matching rule is not ours to invent: the CLI, the desktop app and
/// the workflow engine all resolve mentions the same way, and an agent that
/// wakes for one client and not another is worse than one that never wakes.
/// So this follows `extract_at_mentions_with_known` exactly:
///
/// - an `@` counts only at the start of the content or after whitespace;
/// - known display names are tried **longest first**, case-insensitively,
///   and must end on a word boundary — so `@Will Pfleger` binds the member
///   of that name and a bare `@Will` does not;
/// - otherwise a single word is taken (`[A-Za-z0-9._-]+`);
/// - names come back lowercased, first-seen order, deduplicated.
///
/// One deliberate difference from the Rust, which is byte-oriented: this
/// works in `Character`s. The two agree for ASCII display names and can
/// differ for names outside it — a divergence worth knowing about rather
/// than one to discover from an agent that will not answer.
public enum MeshMentions {
    /// Extract `@name`s from `content`, preferring `known` display names.
    public static func extract(from content: String, known: [String]) -> [String] {
        guard content.contains("@") else { return [] }

        // Longest first, so a name that is a prefix of another cannot win.
        let candidates =
            known
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            .sorted { $0.count > $1.count }

        var names: [String] = []
        var seen: Set<String> = []
        let chars = Array(content)

        for (index, character) in chars.enumerated() where character == "@" {
            let precededProperly = index == 0 || chars[index - 1].isWhitespace
            guard precededProperly, index + 1 < chars.count else { continue }
            let rest = String(chars[(index + 1)...])

            let matched: String
            if let known = candidates.first(where: { matchesKnownName(rest, $0) }) {
                matched = known.lowercased()
            } else {
                let word = rest.prefix { $0.isASCII && ($0.isLetter || $0.isNumber || "._-".contains($0)) }
                guard !word.isEmpty else { continue }
                matched = word.lowercased()
            }

            if seen.insert(matched).inserted { names.append(matched) }
        }
        return names
    }

    /// Does `rest` begin with `name`, ending on a word boundary?
    ///
    /// The boundary check is what stops `@Ann` from matching a member
    /// called `Ann` inside `@Annabel` — without it the longest-first order
    /// is the only defence, and it fails whenever the longer name is not a
    /// member.
    private static func matchesKnownName(_ rest: String, _ name: String) -> Bool {
        guard rest.count >= name.count else { return false }
        let head = String(rest.prefix(name.count))
        guard head.lowercased() == name.lowercased() else { return false }
        let tail = rest.dropFirst(name.count)
        guard let next = tail.first else { return true }
        return next.isWhitespace || ",;.!?:)]}".contains(next)
    }

    /// Build `lowercased display name -> [pubkey]` from kind:0 profiles.
    ///
    /// `display_name` wins, falling back to `name` **only when
    /// `display_name` is absent** — the legacy order the other clients use.
    /// A name shared by two members maps to both: an ambiguous mention wakes
    /// everyone who answers to it rather than silently waking nobody, which
    /// is the behaviour the CLI already has.
    public static func nameIndex(profiles: [MeshEvent]) -> [String: [String]] {
        var index: [String: [String]] = [:]
        for profile in profiles where profile.kind == MeshKind.profile {
            guard let data = profile.content.data(using: .utf8),
                let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { continue }
            let display = (body["display_name"] as? String) ?? (body["name"] as? String)
            guard let name = display, !name.isEmpty else { continue }
            index[name.lowercased(), default: []].append(profile.pubkey)
        }
        return index
    }

    /// Every pubkey `content` addresses, given the channel's profiles.
    ///
    /// Best-effort by design, like the CLI's: if the profiles could not be
    /// loaded the result is empty and the message still sends. Failing to
    /// tag is a message nobody is woken by; refusing to send is a message
    /// nobody can write.
    public static func resolve(content: String, profiles: [MeshEvent]) -> [String] {
        let index = nameIndex(profiles: profiles)
        let known = index.values.flatMap { $0 }.isEmpty ? [] : Array(displayNames(profiles: profiles))
        return extract(from: content, known: known)
            .flatMap { index[$0] ?? [] }
    }

    /// The display names as written, for longest-first matching (the index
    /// is keyed lowercased, which would lose the original spelling).
    private static func displayNames(profiles: [MeshEvent]) -> [String] {
        profiles.compactMap { profile in
            guard profile.kind == MeshKind.profile,
                let data = profile.content.data(using: .utf8),
                let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            else { return nil }
            let display = (body["display_name"] as? String) ?? (body["name"] as? String)
            return (display?.isEmpty == false) ? display : nil
        }
    }
}
