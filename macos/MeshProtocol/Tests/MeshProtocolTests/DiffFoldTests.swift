import Foundation
import Testing

@testable import MeshProtocol

/// The diff is what a reviewer reads to decide whether to trust an agent's
/// work, so these lean on attribution: which file a line belongs to, whether
/// it was added or removed, and whether anything was hidden. A diff rendered
/// a little plainly is a cosmetic loss; a diff rendered *wrongly* is not.
@Suite("Diff fold")
struct DiffFoldTests {
    /// Verbatim `git show` output from a real agent turn on the testbed —
    /// the same bytes the harness published as kind:40008 and the desktop
    /// rendered. Using the real thing rather than a hand-written sample is
    /// the point: a fixture I invent tests my idea of the format.
    static let realTurnDiff = """
        diff --git a/DIFF-PROBE.md b/DIFF-PROBE.md
        new file mode 100644
        index 0000000..7adb5db
        --- /dev/null
        +++ b/DIFF-PROBE.md
        @@ -0,0 +1,3 @@
        +first line
        +second line
        +accented: café ﬁ
        """

    private func diffEvent(
        content: String,
        tags: [[String]]
    ) -> MeshEvent {
        MeshEvent(
            id: String(repeating: "1", count: 64),
            pubkey: String(repeating: "ab", count: 32),
            createdAt: 1_700_000_000,
            kind: MeshKind.streamMessageDiff,
            tags: tags,
            content: content,
            sig: "")
    }

    @Test("a real turn's diff folds into a file, its status and its lines")
    func realTurn() throws {
        let event = diffEvent(
            content: Self.realTurnDiff,
            tags: [
                ["h", "ad7bb34f-ace8-4000-8e4a-87017af73cdb"],
                ["repo", "http://relay.example/git/owner/repo"],
                ["commit", "0d2398d940712ad9951ba1c154de2205e158d515"],
                ["parent-commit", "249eb383296f91806cfc664889be0c6269bcdc59"],
                ["branch", "sm/thread/4f0e11ddbc46", "sm/thread/4f0e11ddbc46"],
                ["description", "Agent turn checkpoint 0d2398d"],
                ["e", String(repeating: "cd", count: 32), "", "reply"],
            ])

        let diff = try #require(MeshTurnDiff.from(event: event))
        #expect(diff.commit == "0d2398d940712ad9951ba1c154de2205e158d515")
        #expect(diff.parentCommit == "249eb383296f91806cfc664889be0c6269bcdc59")
        #expect(diff.branch == "sm/thread/4f0e11ddbc46")
        #expect(diff.note == "Agent turn checkpoint 0d2398d")
        #expect(diff.truncated == false)
        #expect(diff.threadRoot == String(repeating: "cd", count: 32))

        #expect(diff.files.count == 1)
        let file = try #require(diff.files.first)
        #expect(file.path == "DIFF-PROBE.md")
        #expect(file.status == .added)
        #expect(file.additions == 3)
        #expect(file.deletions == 0)
        #expect(file.lines.map(\.text) == ["first line", "second line", "accented: café ﬁ"])
        // A new file starts at line 1, not 0.
        #expect(file.lines.map(\.newLine) == [1, 2, 3])
        #expect(file.lines.allSatisfy { $0.oldLine == nil })
    }

    /// The `commit` tag ties a diff to the kind:47010 that anchors it. A
    /// diff without one is a claim about a repository state nobody can
    /// locate, so it is refused rather than rendered as if it were current.
    @Test("a diff with no commit tag is refused")
    func commitRequired() {
        #expect(MeshTurnDiff.from(event: diffEvent(content: "", tags: [["repo", "x"]])) == nil)
        #expect(
            MeshTurnDiff.from(event: diffEvent(content: "", tags: [["commit", ""]])) == nil)
    }

    @Test("only kind 40008 folds as a diff")
    func kindGate() {
        let wrongKind = MeshEvent(
            id: String(repeating: "1", count: 64), pubkey: String(repeating: "ab", count: 32),
            createdAt: 1, kind: MeshKind.chatMessage, tags: [["commit", "abc1234"]],
            content: Self.realTurnDiff, sig: "")
        #expect(MeshTurnDiff.from(event: wrongKind) == nil)
    }

    /// Truncation must survive into the model. A cut diff that presents as
    /// complete invites a reviewer to conclude the agent changed less than
    /// it did — the one misreading that matters here.
    @Test("truncation is carried, not dropped")
    func truncationSurfaces() throws {
        let event = diffEvent(
            content: Self.realTurnDiff,
            tags: [["commit", "abc1234"], ["truncated", "true"]])
        #expect(try #require(MeshTurnDiff.from(event: event)).truncated)
    }

    /// A removed line beginning with `---` is content, not a file header.
    /// Treating it as a header mid-hunk silently drops a deletion and, worse,
    /// leaves following lines attributed to a file that never started.
    @Test("diff-like text inside a hunk stays content")
    func headerLookalikesInsideHunks() throws {
        let text = """
            diff --git a/doc.md b/doc.md
            index 111..222 100644
            --- a/doc.md
            +++ b/doc.md
            @@ -1,3 +1,3 @@
             intro
            --- a/old/reference
            +++ b/new/reference
            """
        let files = parseUnifiedDiff(text)
        #expect(files.count == 1, "one file, not three")
        let file = try #require(files.first)
        #expect(file.path == "doc.md")
        #expect(file.deletions == 1)
        #expect(file.additions == 1)
        // Only the leading marker is stripped, so `--- a/x` is a removal of
        // the text `-- a/x`. Expecting `- a/x` here was my error, not the
        // parser's — the `+++` case in the same fixture already showed it.
        #expect(file.lines.map(\.text) == ["intro", "-- a/old/reference", "++ b/new/reference"])
    }

    @Test("a multi-file diff keeps each file's lines to itself")
    func multipleFiles() throws {
        let text = """
            diff --git a/a.txt b/a.txt
            index 1..2 100644
            --- a/a.txt
            +++ b/a.txt
            @@ -1,2 +1,2 @@
             kept
            -gone
            +fresh
            diff --git a/b.txt b/b.txt
            deleted file mode 100644
            index 3..0000000
            --- a/b.txt
            +++ /dev/null
            @@ -1,1 +0,0 @@
            -removed entirely
            """
        let files = parseUnifiedDiff(text)
        #expect(files.map(\.path) == ["a.txt", "b.txt"])
        #expect(files[0].status == .modified)
        #expect(files[0].additions == 1 && files[0].deletions == 1)
        #expect(files[1].status == .deleted)
        #expect(files[1].deletions == 1)
        #expect(files[1].lines.map(\.text) == ["removed entirely"])
    }

    /// Line numbers come from the hunk header, so a hunk that does not start
    /// at line 1 must not be numbered from 1.
    @Test("line numbers follow the hunk header")
    func hunkOffsets() throws {
        let text = """
            diff --git a/x.rs b/x.rs
            index 1..2 100644
            --- a/x.rs
            +++ b/x.rs
            @@ -40,3 +40,4 @@ fn context_header_text() {
             before
            +inserted
             after
            """
        let file = try #require(parseUnifiedDiff(text).first)
        #expect(file.lines.map(\.newLine) == [40, 41, 42])
        // The inserted line consumes a new-side number but no old-side one.
        #expect(file.lines.map(\.oldLine) == [40, nil, 41])
    }

    @Test("a rename records where the file came from")
    func renames() throws {
        let text = """
            diff --git a/old/name.md b/new/name.md
            similarity index 95%
            rename from old/name.md
            rename to new/name.md
            index 1..2 100644
            """
        let file = try #require(parseUnifiedDiff(text).first)
        // Filed under the name it ends up with — the old path no longer exists.
        #expect(file.path == "new/name.md")
        #expect(file.status == .renamed(from: "old/name.md"))
    }

    @Test("a no-newline marker annotates rather than counts")
    func noNewlineMarker() throws {
        let text = """
            diff --git a/t.txt b/t.txt
            index 1..2 100644
            --- a/t.txt
            +++ b/t.txt
            @@ -1 +1 @@
            -old
            \\ No newline at end of file
            +new
            \\ No newline at end of file
            """
        let file = try #require(parseUnifiedDiff(text).first)
        #expect(file.additions == 1 && file.deletions == 1)
        #expect(file.lines.map(\.text) == ["old", "new"])
    }

    @Test("empty or junk content yields no files rather than a phantom one")
    func emptyAndJunk() {
        #expect(parseUnifiedDiff("").isEmpty)
        #expect(parseUnifiedDiff("not a diff at all\njust prose\n").isEmpty)
        // Hunk lines with no preceding file header are not attributable.
        #expect(parseUnifiedDiff("@@ -1 +1 @@\n+orphan\n").isEmpty)
    }
}
