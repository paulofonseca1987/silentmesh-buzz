import MeshProtocol
import SwiftUI

/// A work thread's D41 state, as a badge.
///
/// Status is the question a member actually has about a thread — is anyone
/// waiting on me, is this finished, is it gone — so it is on the row rather
/// than one click away.
struct ThreadStatusBadge: View {
    let status: MeshThreadStatus

    private var color: Color {
        switch status {
        case .open: return .blue
        case .snoozed: return .gray
        case .ready: return .orange
        case .closed: return .green
        case .archived: return .secondary
        }
    }

    private var help: String {
        switch status {
        case .open: return "Being worked."
        case .snoozed: return "Parked; wakes back to open."
        case .ready: return "Done proposed — awaiting a Channel Admin's decision."
        case .closed: return "Closed by a Channel Admin or the workspace Owner."
        case .archived: return "Archived; storage state."
        }
    }

    var body: some View {
        Text(status.rawValue)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color)
            .help(help)
    }
}

struct ThreadRow: View {
    let thread: MeshThread

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(thread.goal.isEmpty ? "(no goal)" : thread.goal)
                    .lineLimit(2)
                    .font(.callout)
                Spacer(minLength: 6)
                ThreadStatusBadge(status: thread.status)
            }
            HStack(spacing: 8) {
                if thread.forkedFrom != nil {
                    Label("fork", systemImage: "arrow.triangle.branch")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                if thread.promotedFrom != nil {
                    Label("promoted", systemImage: "arrow.up.forward.square")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                if !thread.checkpoints.isEmpty {
                    Label("\(thread.checkpoints.count)", systemImage: "checkmark.seal")
                        .font(.caption2).foregroundStyle(.secondary)
                        .help("\(thread.checkpoints.count) checkpoint(s) recorded")
                }
                if let deadline = thread.deadline {
                    Label(
                        Date(timeIntervalSince1970: TimeInterval(deadline))
                            .formatted(date: .abbreviated, time: .omitted),
                        systemImage: "clock")
                        .font(.caption2)
                        // An overdue deadline is the one piece of thread
                        // metadata that is actively asking for attention.
                        .foregroundStyle(
                            TimeInterval(deadline) < Date().timeIntervalSince1970
                                ? Color.red : Color.secondary)
                }
            }
        }
        .padding(.vertical, 3)
    }
}

/// A thread's full state: framing, provenance, the checkpoint trail, and
/// everything the relay said about it.
/// Talk inside a work thread — and the only way to summon an agent into
/// one.
///
/// The channel composer is hidden while a thread is open, so without this a
/// thread is read-only: a member can create it and watch it, but cannot
/// address anyone in it. That is not a cosmetic gap — an `@mention` in the
/// thread's *goal* is a `p` tag on a kind:47000, and the harness does not
/// subscribe to that kind.
struct ThreadComposer: View {
    let threadRoot: String
    @ObservedObject var model: WorkspaceModel
    @State private var draft = ""

    var body: some View {
        Divider()
        HStack(spacing: 8) {
            TextField("Message this thread — @mention to bring in an agent", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .lineLimit(1...5)
                .onSubmit(send)
            Button(action: send) {
                Image(systemName: "arrow.up.circle.fill").font(.title3)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Send to this thread")
            .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.isSending)
        }
        .padding(10)
    }

    private func send() {
        let content = draft
        guard !content.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        draft = ""
        Task { await model.sendToThread(threadRoot, content: content) }
    }
}

/// One turn's changes, as published by the harness (kind:40008).
///
/// This is where a member decides whether to trust what an agent did, so
/// the card leans the same way the fold does: strict about attribution,
/// explicit about omission. Every line is shown under the file it belongs
/// to with its real line number, and a truncated diff says so — a cut diff
/// that presents as complete invites the reader to conclude the agent
/// changed less than it did.
struct TurnDiffCard: View {
    let diff: MeshTurnDiff

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Image(systemName: "plus.forwardslash.minus")
                    .foregroundStyle(.blue)
                Text(diff.note ?? "Turn changes")
                    .font(.caption.weight(.semibold))
                Text(String(diff.commit.prefix(8)))
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .help("The commit this turn pushed — the same oid its checkpoint records.")
                if diff.truncated {
                    Label("truncated", systemImage: "exclamationmark.triangle")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                        .help(
                            "The turn changed more than fits in one event. "
                                + "The full change is on the thread branch.")
                }
                Spacer(minLength: 6)
                Text("+\(diff.additions)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.green)
                Text("−\(diff.deletions)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.red)
            }
            ForEach(Array(diff.files.enumerated()), id: \.offset) { _, file in
                DiffFileView(file: file)
            }
        }
    }
}

private struct DiffFileView: View {
    let file: MeshDiffFile

    private var statusBadge: (String, Color)? {
        switch file.status {
        case .added: return ("new file", .green)
        case .deleted: return ("deleted", .red)
        case .renamed(let from): return ("renamed from \(from)", .orange)
        case .modified: return nil
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 6) {
                Text(file.path)
                    .font(.caption.monospaced().weight(.medium))
                    .textSelection(.enabled)
                if let badge = statusBadge {
                    Text(badge.0)
                        .font(.caption2)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(badge.1.opacity(0.15), in: Capsule())
                        .foregroundStyle(badge.1)
                }
                Spacer()
            }
            .padding(.vertical, 4)
            // Lazy because a turn can legitimately touch hundreds of lines,
            // and the whole detail view is already inside a ScrollView.
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(Array(file.lines.enumerated()), id: \.offset) { _, line in
                    DiffLineView(line: line)
                }
            }
        }
    }
}

private struct DiffLineView: View {
    let line: MeshDiffLine

    private var marker: (String, Color, Color) {
        switch line.kind {
        case .added: return ("+", .green, Color.green.opacity(0.10))
        case .removed: return ("−", .red, Color.red.opacity(0.10))
        case .context: return (" ", .secondary, .clear)
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 6) {
            // One number per line, from the side it exists on. A removed
            // line has no new-side number and must not borrow one.
            Text((line.newLine ?? line.oldLine).map(String.init) ?? "")
                .font(.caption2.monospaced())
                .foregroundStyle(.tertiary)
                .frame(width: 34, alignment: .trailing)
            Text(marker.0)
                .font(.caption.monospaced())
                .foregroundStyle(marker.1)
            Text(line.text.isEmpty ? " " : line.text)
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 1)
        .background(marker.2)
    }
}

struct ThreadDetailView: View {
    let thread: MeshThread
    var diffs: [MeshTurnDiff] = []
    var model: WorkspaceModel?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                VStack(alignment: .leading, spacing: 6) {
                    HStack(spacing: 8) {
                        Text(thread.goal.isEmpty ? "(no goal)" : thread.goal)
                            .font(.title3.weight(.semibold))
                        ThreadStatusBadge(status: thread.status)
                    }
                    HStack(spacing: 12) {
                        Label(String(thread.createdBy.prefix(8)), systemImage: "person")
                        if let dri = thread.dri {
                            Label(String(dri.prefix(8)), systemImage: "flag")
                                .help("DRI — directly responsible individual")
                        }
                        if let deadline = thread.deadline {
                            Label(
                                Date(timeIntervalSince1970: TimeInterval(deadline))
                                    .formatted(date: .abbreviated, time: .shortened),
                                systemImage: "clock")
                        }
                    }
                    .font(.caption)
                    .foregroundStyle(.secondary)

                    if let model {
                        // The D30 pre-flight, where the member actually
                        // stands: about to share private work, before
                        // deciding what the summary says.
                        HStack(spacing: 8) {
                            Button("Check what promoting would expose") {
                                Task {
                                    await model.requestGateReview(
                                        thread: thread.id, draftSummary: "")
                                }
                            }
                            .controlSize(.small)
                            .disabled(model.isSending)
                            Button("Refresh") { Task { await model.refreshThreads() } }
                                .controlSize(.small)
                                .buttonStyle(.link)
                                .font(.caption)
                        }
                        .padding(.top, 2)
                    }
                }

                if thread.forkedFrom != nil || thread.promotedFrom != nil {
                    GroupBox("Provenance") {
                        VStack(alignment: .leading, spacing: 4) {
                            if let parent = thread.forkedFrom {
                                Label(
                                    "forked from \(String(parent.prefix(12)))…"
                                        + (thread.forkCommit.map { " at \(String($0.prefix(8)))" }
                                            ?? " at head"),
                                    systemImage: "arrow.triangle.branch")
                            }
                            if let source = thread.promotedFrom {
                                Label(
                                    "promoted from \(String(source.prefix(12)))…"
                                        + (thread.promotedChannel.map { " in \($0)" } ?? ""),
                                    systemImage: "arrow.up.forward.square")
                            }
                        }
                        .font(.caption)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }

                if !thread.checkpoints.isEmpty {
                    GroupBox("Checkpoints") {
                        VStack(alignment: .leading, spacing: 3) {
                            ForEach(Array(thread.checkpoints.enumerated()), id: \.offset) {
                                index, commit in
                                HStack(spacing: 6) {
                                    Text("\(index + 1)")
                                        .font(.caption2.monospaced())
                                        .foregroundStyle(.secondary)
                                    Text(String(commit.prefix(12)))
                                        .font(.caption.monospaced())
                                        .textSelection(.enabled)
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }

                if !diffs.isEmpty {
                    // What the agent actually changed, turn by turn — the
                    // evidence behind the checkpoint list above. Oldest
                    // first, matching the checkpoint order.
                    GroupBox("Changes") {
                        VStack(alignment: .leading, spacing: 12) {
                            ForEach(Array(diffs.enumerated()), id: \.offset) { _, diff in
                                TurnDiffCard(diff: diff)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }

                if !thread.notices.isEmpty {
                    // Relay notices are the thread's audit trail: how a
                    // deadline passed, whether canonicalization succeeded,
                    // what a gate review found. They are the relay's own
                    // words, so they are shown as such rather than folded
                    // into the member's narrative.
                    GroupBox("Relay notices") {
                        VStack(alignment: .leading, spacing: 8) {
                            ForEach(Array(thread.notices.enumerated()), id: \.offset) {
                                _, notice in
                                NoticeRow(notice: notice)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .padding(16)
        }
    }
}

struct NoticeRow: View {
    let notice: MeshThreadNotice

    private var label: (String, String, Color) {
        switch notice.kind {
        case .overdue: return ("Deadline passed", "clock.badge.exclamationmark", .red)
        case .canonicalized: return ("Canonicalization", "doc.on.doc", .blue)
        case .siblingArchived: return ("Archived as a losing fork", "archivebox", .secondary)
        case .promoted: return ("Promoted away", "arrow.up.forward.square", .green)
        case .gateReview: return ("Privacy gate review", "shield.lefthalf.filled", .purple)
        }
    }

    /// Relay notices carry a small JSON body — the winning thread of a
    /// fork family, the commit a canonicalization produced, where a
    /// promotion went. Rendering the raw payload makes a member read JSON
    /// to learn what happened to their own thread, so the known fields are
    /// named and anything unrecognised falls back to the raw text rather
    /// than being dropped.
    private var readableBody: String? {
        guard !notice.body.isEmpty else { return nil }
        guard let data = notice.body.data(using: .utf8),
            let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return notice.body }
        var parts: [String] = []
        // The overdue notice repeats the thread's goal and status, both
        // already on screen a few points above. Only the deadline itself
        // is news, so that is what is shown.
        if let deadline = (payload["deadline"] as? NSNumber)?.doubleValue {
            parts.append(
                "deadline was "
                    + Date(timeIntervalSince1970: deadline)
                        .formatted(date: .abbreviated, time: .shortened))
        }
        if let winner = payload["winner"] as? String {
            parts.append("winner: \(String(winner.prefix(12)))…")
        }
        if let commit = payload["commit"] as? String {
            parts.append("commit: \(String(commit.prefix(12)))")
        }
        if let outcome = payload["outcome"] as? String { parts.append(outcome) }
        if let to = payload["to"] as? String { parts.append("to: \(to)") }
        if let thread = payload["thread"] as? String {
            parts.append("new thread: \(String(thread.prefix(12)))…")
        }
        if parts.isEmpty {
            // Nothing recognised: show the payload rather than hiding it.
            // A notice whose meaning this client does not know yet is
            // still information the member is entitled to see.
            return notice.body
        }
        return parts.joined(separator: " · ")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Image(systemName: label.1).foregroundStyle(label.2)
                Text(label.0).font(.caption.weight(.semibold))
                Text(Date(timeIntervalSince1970: TimeInterval(notice.at)), style: .time)
                    .font(.caption2).foregroundStyle(.secondary)
            }
            if notice.kind == .gateReview,
                let data = notice.body.data(using: .utf8),
                let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            {
                GateReviewCard(payload: payload)
            } else if let summary = readableBody {
                Text(summary).font(.caption).foregroundStyle(.secondary)
                    .textSelection(.enabled)
            }
        }
    }
}
