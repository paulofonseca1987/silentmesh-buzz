import Foundation

/// What one agent turn changed, folded from the kind:40008 the harness
/// publishes alongside each kind:47010 checkpoint.
///
/// The harness sends the diff as an event rather than leaving clients to
/// fetch it, because a client that must clone a repo over authenticated
/// smart HTTP to show a diff is a client that will not show one. So this is
/// a parser, not a git client: everything needed to render a turn's changes
/// arrives in the event.
///
/// Parsing is deliberately forgiving about *shape* and strict about
/// *attribution*. A hunk header we cannot read costs line numbers on that
/// hunk; it must never cause a line to be attributed to the wrong file or
/// counted as an addition when it was a removal. Showing a diff slightly
/// less richly is a cosmetic loss. Showing it wrongly is a correctness one,
/// because a reviewer decides whether to trust an agent's work from it.

/// One line of a unified diff.
public struct MeshDiffLine: Equatable, Sendable {
    public enum Kind: Equatable, Sendable {
        case context
        case added
        case removed
    }

    public let kind: Kind
    public let text: String
    /// Line number in the pre-image, absent for added lines.
    public let oldLine: Int?
    /// Line number in the post-image, absent for removed lines.
    public let newLine: Int?
}

/// One file's changes within a turn.
public struct MeshDiffFile: Equatable, Sendable {
    public enum Status: Equatable, Sendable {
        case added
        case deleted
        case modified
        case renamed(from: String)
    }

    public let path: String
    public let status: Status
    public let lines: [MeshDiffLine]

    public var additions: Int { lines.filter { $0.kind == .added }.count }
    public var deletions: Int { lines.filter { $0.kind == .removed }.count }
}

/// A turn's diff: the metadata from the event's tags, plus its parsed body.
public struct MeshTurnDiff: Equatable, Sendable {
    /// The commit this diff introduces — the same oid the turn's kind:47010
    /// checkpoint names, which is how the two are tied together.
    public let commit: String
    public let parentCommit: String?
    /// The thread branch, e.g. `sm/thread/6e5b88de823e`.
    public let branch: String?
    public let repoURL: String
    public let note: String?
    /// The publisher cut the diff to fit the event size budget. Surfacing
    /// this is not optional: a truncated diff that looks complete invites a
    /// reviewer to conclude the agent changed less than it did.
    public let truncated: Bool
    /// The work-thread root this diff belongs under.
    public let threadRoot: String?
    public let files: [MeshDiffFile]

    public var additions: Int { files.reduce(0) { $0 + $1.additions } }
    public var deletions: Int { files.reduce(0) { $0 + $1.deletions } }

    /// Fold a kind:40008 event, or `nil` if it is not one.
    ///
    /// A `commit` tag is required — without it the diff cannot be tied to a
    /// checkpoint, and an untethered diff is a claim about a repository
    /// state nobody can locate.
    public static func from(event: MeshEvent) -> MeshTurnDiff? {
        guard event.kind == MeshKind.streamMessageDiff else { return nil }
        guard let commit = event.tagValue("commit"), !commit.isEmpty else { return nil }

        return MeshTurnDiff(
            commit: commit,
            parentCommit: event.tagValue("parent-commit"),
            // The publisher writes `["branch", source, target]`; for a turn
            // both are the thread branch, so either position answers it.
            branch: event.tagValue("branch"),
            repoURL: event.tagValue("repo") ?? "",
            note: event.tagValue("description"),
            truncated: event.tagValue("truncated") == "true",
            threadRoot: event.tagValue("e"),
            files: parseUnifiedDiff(event.content))
    }
}

/// Parse a unified diff into per-file line lists.
///
/// Exposed for testing against real `git show` output; callers normally go
/// through `MeshTurnDiff.from(event:)`.
public func parseUnifiedDiff(_ text: String) -> [MeshDiffFile] {
    var files: [MeshDiffFile] = []

    var path: String?
    var status: MeshDiffFile.Status = .modified
    var lines: [MeshDiffLine] = []
    var oldLine = 0
    var newLine = 0
    var inHunk = false

    func flush() {
        guard let p = path else { return }
        files.append(MeshDiffFile(path: p, status: status, lines: lines))
        path = nil
        status = .modified
        lines = []
        inHunk = false
    }

    for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
        let line = String(raw)

        if line.hasPrefix("diff --git ") {
            flush()
            path = gitHeaderPath(line)
            continue
        }
        // Header lines precede the first hunk. Once inside a hunk a line
        // starting with "-" is a *removal*, not a header, so these are only
        // honoured before `@@` — otherwise a diff that removes a line
        // reading "--- something" would be misread as a file header.
        if !inHunk {
            if line.hasPrefix("new file mode") {
                status = .added
                continue
            }
            if line.hasPrefix("deleted file mode") {
                status = .deleted
                continue
            }
            if line.hasPrefix("rename from ") {
                status = .renamed(from: String(line.dropFirst("rename from ".count)))
                continue
            }
            if line.hasPrefix("--- ") || line.hasPrefix("+++ ") || line.hasPrefix("index ")
                || line.hasPrefix("similarity index") || line.hasPrefix("rename to ")
                || line.hasPrefix("old mode") || line.hasPrefix("new mode")
            {
                continue
            }
        }
        if line.hasPrefix("@@") {
            inHunk = true
            if let (o, n) = hunkStarts(line) {
                oldLine = o
                newLine = n
            }
            continue
        }
        guard inHunk, path != nil else { continue }

        // "\ No newline at end of file" annotates the previous line rather
        // than being one.
        if line.hasPrefix("\\") { continue }

        if line.hasPrefix("+") {
            lines.append(
                MeshDiffLine(
                    kind: .added, text: String(line.dropFirst()), oldLine: nil, newLine: newLine))
            newLine += 1
        } else if line.hasPrefix("-") {
            lines.append(
                MeshDiffLine(
                    kind: .removed, text: String(line.dropFirst()), oldLine: oldLine, newLine: nil))
            oldLine += 1
        } else if line.hasPrefix(" ") || line.isEmpty {
            lines.append(
                MeshDiffLine(
                    kind: .context, text: String(line.dropFirst()), oldLine: oldLine,
                    newLine: newLine))
            oldLine += 1
            newLine += 1
        }
    }
    flush()
    return files
}

/// The post-image path from a `diff --git a/X b/Y` header.
///
/// Takes the `b/` side because it is the name the change results in; for a
/// deletion git still emits both, and the `a/` side of a rename is the old
/// name, which would file the change under a path that no longer exists.
private func gitHeaderPath(_ line: String) -> String? {
    let rest = line.dropFirst("diff --git ".count)
    // Paths may contain spaces, so split on " b/" rather than whitespace.
    guard let sep = rest.range(of: " b/") else {
        return nil
    }
    let b = String(rest[sep.upperBound...])
    return b.isEmpty ? nil : b
}

/// Starting line numbers from `@@ -old,count +new,count @@`.
private func hunkStarts(_ header: String) -> (Int, Int)? {
    let parts = header.split(separator: " ")
    guard parts.count >= 3 else { return nil }
    func start(_ s: Substring) -> Int? {
        let body = s.dropFirst()  // strip - or +
        let digits = body.prefix { $0.isNumber }
        return Int(digits)
    }
    guard let o = start(parts[1]), let n = start(parts[2]) else { return nil }
    return (o, n)
}
