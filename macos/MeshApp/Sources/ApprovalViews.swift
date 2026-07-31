import MeshProtocol
import SwiftUI

/// One agent permission request, with the two buttons that answer it.
///
/// This is the most consequential control in the app: the agent is blocked
/// on it, and granting runs something on a real machine. So the card leads
/// with *what* is being asked and *who* is asking, and the buttons sit under
/// that rather than beside a truncated summary — a member should not be able
/// to approve something they have not read.
struct ApprovalCard: View {
    let approval: MeshApproval
    let isBusy: Bool
    let decide: (Bool) -> Void

    private var icon: String {
        switch approval.kind {
        case .command: return "terminal"
        case .fileRead: return "doc.text.magnifyingglass"
        case .fileChange: return "square.and.pencil"
        case .other: return "questionmark.circle"
        }
    }

    private var kindLabel: String {
        switch approval.kind {
        case .command: return "run a command"
        case .fileRead: return "read a file"
        case .fileChange: return "change a file"
        // Deliberately not "unknown request": the harness could not classify
        // it, which is a statement about the harness, not about the risk.
        case .other: return "do something unclassified"
        }
    }

    /// What the decision is about, said once so both buttons agree.
    private var spokenSubject: String {
        approval.detail.isEmpty ? kindLabel : approval.detail
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: icon).foregroundStyle(.blue)
                Text("Agent wants to \(kindLabel)")
                    .font(.subheadline.weight(.semibold))
                if let tool = approval.toolName {
                    Text(tool)
                        .font(.caption2.monospaced())
                        .padding(.horizontal, 5).padding(.vertical, 1)
                        .background(Color.blue.opacity(0.12), in: Capsule())
                        .foregroundStyle(.blue)
                }
                Spacer()
                Text(String(approval.agentPubkey.prefix(8)))
                    .font(.caption2.monospaced()).foregroundStyle(.secondary)
                    .help("The agent that asked — \(approval.agentPubkey)")
            }

            Text(approval.detail.isEmpty ? "No detail was published." : approval.detail)
                .font(.callout)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .foregroundStyle(approval.detail.isEmpty ? .secondary : .primary)

            switch approval.outcome {
            case .pending:
                HStack(spacing: 8) {
                    // Labelled with what is being decided, not just the
                    // verb. These buttons publish no title of their own —
                    // an accessibility dump shows four anonymous "button"
                    // elements — so without this a screen-reader user is
                    // asked to approve something they were never told, and
                    // two cards on screen are indistinguishable.
                    Button("Approve") { decide(true) }
                        .buttonStyle(.borderedProminent)
                        .accessibilityLabel("Approve: \(spokenSubject)")
                    Button("Deny") { decide(false) }
                        .buttonStyle(.bordered)
                        .accessibilityLabel("Deny: \(spokenSubject)")
                    Spacer()
                    if let expires = approval.expiresAt {
                        // A deadline the member cannot see is a decision
                        // that expires under their hands.
                        Text("expires \(expires, style: .relative)")
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                }
                .disabled(isBusy)
            case .granted(let by):
                ResolvedLabel(
                    systemImage: "checkmark.seal.fill", tint: .green,
                    text: by.isEmpty ? "Approved" : "Approved by \(String(by.prefix(8)))")
            case .denied(let by):
                ResolvedLabel(
                    systemImage: "hand.raised.fill", tint: .red,
                    text: by.isEmpty ? "Denied" : "Denied by \(String(by.prefix(8)))")
            case .withdrawn(let status):
                ResolvedLabel(
                    systemImage: "arrow.uturn.backward", tint: .secondary,
                    text: status == "expired"
                        ? "Withdrawn — the agent's request timed out"
                        : "Withdrawn by the agent")
            case .expired:
                ResolvedLabel(
                    systemImage: "clock.badge.xmark", tint: .secondary,
                    text: "Expired before anyone answered")
            }
        }
        .padding(10)
        .background(
            (approval.isActionable ? Color.blue : Color.secondary).opacity(0.07),
            in: RoundedRectangle(cornerRadius: 8))
    }
}

private struct ResolvedLabel: View {
    let systemImage: String
    let tint: Color
    let text: String

    var body: some View {
        Label(text, systemImage: systemImage)
            .font(.caption)
            .foregroundStyle(tint)
    }
}

/// What an agent is doing right now, in one line per turn.
///
/// Only live turns are shown. A finished turn's answer is an ordinary
/// message and already appears in the timeline, and its reactions are
/// soft-deleted by the relay the moment it ends — so there is nothing to
/// show and nothing to show it from.
struct TurnStrip: View {
    @ObservedObject var model: WorkspaceModel

    private var live: [MeshTurn] { model.turns.filter(\.isLive) }
    /// Refusals are kept briefly even though the turn is over: on the
    /// tier-gate paths this is the only thing the turn ever emits, and it
    /// explains why nothing happened.
    private var refusals: [MeshTurn] {
        Array(
            model.turns.filter {
                if case .refused = $0.state { return true }
                return false
            }.prefix(2))
    }

    var body: some View {
        if !live.isEmpty || !refusals.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                ForEach(live) { turn in
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text(turn.state == .queued ? "Agent queued this" : "Agent is working")
                            .font(.caption)
                        Text(String(turn.agentPubkey.prefix(8)))
                            .font(.caption2.monospaced()).foregroundStyle(.secondary)
                        Spacer()
                    }
                }
                ForEach(refusals) { turn in
                    if case .refused(let reason) = turn.state {
                        Label {
                            Text(reason).font(.caption).textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                        } icon: {
                            Image(systemName: "hand.raised.slash").foregroundStyle(.orange)
                        }
                    }
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .background(Color.blue.opacity(0.05))
        }
    }
}

/// The channel's approval queue.
///
/// Pending requests are shown in full because they need a decision.
/// Recently-resolved ones stay for a few rows so that clicking Approve
/// visibly *did* something — a card that simply vanishes leaves a member
/// unsure whether the relay took it.
struct ApprovalQueue: View {
    @ObservedObject var model: WorkspaceModel

    private var pending: [MeshApproval] { model.approvals.filter(\.isActionable) }
    private var resolved: [MeshApproval] {
        Array(model.approvals.filter { !$0.isActionable }.prefix(3))
    }

    var body: some View {
        if !model.approvals.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                HStack(spacing: 6) {
                    Image(systemName: "person.badge.shield.checkmark")
                        .foregroundStyle(pending.isEmpty ? Color.secondary : Color.blue)
                    Text(
                        pending.isEmpty
                            ? "Approvals" : "\(pending.count) waiting on you"
                    )
                    .font(.subheadline.weight(.semibold))
                    Spacer()
                }
                ForEach(pending) { approval in
                    ApprovalCard(approval: approval, isBusy: model.isSending) { grant in
                        Task { await model.decide(approval, grant: grant) }
                    }
                }
                ForEach(resolved) { approval in
                    ApprovalCard(approval: approval, isBusy: true) { _ in }
                }
            }
            .padding(12)
        }
    }
}
