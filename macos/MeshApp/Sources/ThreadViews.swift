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
struct ThreadDetailView: View {
    let thread: MeshThread
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
