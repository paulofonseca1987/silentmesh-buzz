import MeshProtocol
import SwiftUI

/// The privacy tier, rendered as a badge.
///
/// Given prominence deliberately: the tier decides which models may touch
/// what is said in a channel, and it is the one property of a Silent Mesh
/// workspace a member cannot infer from anything else on screen. A client
/// that hides it asks people to remember where it is safe to speak.
struct TierBadge: View {
    let tier: MeshChannelTier

    private var label: String {
        switch tier {
        case .owned: return "owned"
        case .private: return "private"
        case .open: return "open"
        }
    }

    private var color: Color {
        switch tier {
        case .owned: return .green
        case .private: return .orange
        case .open: return .secondary
        }
    }

    private var help: String {
        switch tier {
        case .owned: return "Local models only — nothing here leaves this workspace."
        case .private: return "Local or TEE-attested providers."
        case .open: return "Members' own vendor subscriptions are allowed here."
        }
    }

    var body: some View {
        Text(label)
            .font(.caption2.weight(.medium))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(color.opacity(0.15), in: Capsule())
            .foregroundStyle(color)
            .help(help)
    }
}

struct ChannelRow: View {
    let channel: ChannelSummary

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: channel.isPrivate ? "lock.fill" : "number")
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(channel.name).lineLimit(1)
            Spacer(minLength: 8)
            TierBadge(tier: channel.tier)
        }
        .padding(.vertical, 2)
    }
}

/// A relay gate review (kind 47023), rendered as findings rather than JSON.
///
/// This is the member's decision surface before promoting: what the
/// deterministic scanners found, what the local model added, and — the part
/// that is easy to get wrong — whether the suggested summary was checked or
/// withheld. Dumping the raw payload would make a member parse JSON to
/// learn whether their thread is safe to publish.
struct GateReviewCard: View {
    let payload: [String: Any]

    private var deterministic: [(rule: String, place: String)] {
        (payload["deterministic"] as? [[String: Any]] ?? []).map {
            ($0["rule"] as? String ?? "?", $0["where"] as? String ?? "?")
        }
    }
    private var advisory: [String] { payload["advisory"] as? [String] ?? [] }
    private var summary: String? { payload["suggestedSummary"] as? String }
    private var vetting: String { payload["summaryVetting"] as? String ?? "unknown" }
    private var model: String { payload["model"] as? String ?? "—" }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: "shield.lefthalf.filled").foregroundStyle(.purple)
                Text("Privacy gate review").font(.subheadline.weight(.semibold))
                Spacer()
                Text(model).font(.caption2.monospaced()).foregroundStyle(.secondary)
            }

            if deterministic.isEmpty && advisory.isEmpty {
                Label("No findings", systemImage: "checkmark.circle")
                    .font(.callout).foregroundStyle(.green)
            }
            ForEach(deterministic, id: \.rule) { finding in
                Label {
                    Text("\(finding.rule) — in the \(finding.place)")
                } icon: {
                    Image(systemName: "exclamationmark.octagon.fill").foregroundStyle(.red)
                }
                .font(.callout)
            }
            ForEach(advisory, id: \.self) { note in
                Label {
                    Text(note)
                } icon: {
                    Image(systemName: "eye.trianglebadge.exclamationmark").foregroundStyle(.orange)
                }
                .font(.callout)
            }

            Divider()
            if let summary, !summary.isEmpty {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Suggested summary").font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(summary).font(.callout).textSelection(.enabled)
                    Text("checked: \(vetting)").font(.caption2).foregroundStyle(.secondary)
                }
            } else {
                Label {
                    VStack(alignment: .leading, spacing: 2) {
                        Text("Summary withheld").font(.callout.weight(.medium))
                        Text(withheldReason).font(.caption).foregroundStyle(.secondary)
                    }
                } icon: {
                    Image(systemName: "nosign").foregroundStyle(.orange)
                }
            }
        }
        .padding(10)
        .background(Color.purple.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
    }

    private var withheldReason: String {
        switch vetting {
        case "withheld-self-check":
            return "The draft named something the review flagged, so it was not offered."
        case "self-check-unverified":
            return "The check could not be read; treat the draft as unverified."
        case "none":
            return "No model assist ran — deterministic findings only."
        default:
            return "No summary was produced."
        }
    }
}

struct MessageRow: View {
    let message: TimelineMessage

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(message.isRelayNotice ? "relay" : message.author)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(message.isRelayNotice ? Color.purple : Color.primary)
                Text(message.createdAt, style: .time)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                if message.isRelayNotice {
                    Text("notice")
                        .font(.caption2)
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Color.purple.opacity(0.12), in: Capsule())
                        .foregroundStyle(.purple)
                }
            }
            if message.kind == MeshKind.workThreadGateReviewed,
                let data = message.content.data(using: .utf8),
                let payload = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
            {
                GateReviewCard(payload: payload)
            } else {
                Text(message.content)
                    .font(.callout)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.vertical, 3)
    }
}

struct ContentView: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        NavigationSplitView {
            List(model.channels, selection: $model.selectedChannel) { channel in
                ChannelRow(channel: channel).tag(channel.id)
            }
            .scrollContentBackground(.hidden)
            // An explicit opaque background: SwiftUI's default sidebar
            // material is drawn by the window server, so it renders as
            // blank white in a `cacheDisplay` bitmap — the screenshot
            // would show an empty sidebar the app does not actually have.
            .background(Color(nsColor: .controlBackgroundColor))
            .navigationSplitViewColumnWidth(min: 200, ideal: 230)
            .safeAreaInset(edge: .bottom) {
                HStack(spacing: 6) {
                    Circle()
                        .fill(model.status.contains("failed") ? Color.red : Color.green)
                        .frame(width: 7, height: 7)
                    Text(model.status).font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                }
                .padding(8)
            }
        } content: {
            // Work threads are the unit of work in a Silent Mesh channel,
            // so they get a column rather than being buried in the stream.
            // The channel's timeline is the first row, because a channel is
            // still a place people talk.
            List(selection: $model.selectedThread) {
                Section {
                    HStack(spacing: 6) {
                        Image(systemName: "bubble.left.and.bubble.right")
                            .foregroundStyle(.secondary)
                        Text("Channel timeline")
                        Spacer()
                        Text("\(model.messages.count)")
                            .font(.caption2).foregroundStyle(.secondary)
                    }
                    .tag(Optional<String>.none)
                }
                Section {
                    NewThreadField(model: model)
                } header: {
                    Text("Work threads")
                }
                Section {
                    if model.threads.isEmpty {
                        Text("No threads yet").font(.caption).foregroundStyle(.secondary)
                    }
                    ForEach(model.threads) { thread in
                        ThreadRow(thread: thread).tag(Optional(thread.id))
                    }
                }
            }
            .scrollContentBackground(.hidden)
            .background(Color(nsColor: .controlBackgroundColor))
            .navigationSplitViewColumnWidth(min: 260, ideal: 320)
        } detail: {
            VStack(alignment: .leading, spacing: 0) {
                if let selected = model.selectedChannel,
                    let channel = model.channels.first(where: { $0.id == selected })
                {
                    HStack(spacing: 8) {
                        Text(channel.name).font(.headline)
                        TierBadge(tier: channel.tier)
                        Spacer()
                    }
                    .padding(12)
                    Divider()
                }
                // Above everything the channel contains, and outside the
                // thread selection: an agent blocked on a decision is not a
                // property of whichever thread happens to be open, and
                // burying it one click deep means it waits until someone
                // goes looking. Bounded height so a busy queue cannot take
                // the whole pane.
                if !model.approvals.isEmpty {
                    ScrollView { ApprovalQueue(model: model) }
                        .frame(maxHeight: 320)
                    Divider()
                }
                if let selectedThread = model.selectedThread,
                    let thread = model.threads.first(where: { $0.id == selectedThread })
                {
                    ThreadDetailView(thread: thread, model: model)
                } else if model.messages.isEmpty {
                    ContentUnavailableView(
                        "No messages", systemImage: "text.bubble",
                        description: Text("Nothing in this channel yet."))
                } else {
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(model.messages) { MessageRow(message: $0) }
                        }
                        .padding(12)
                    }
                }
                if model.selectedThread == nil { Composer(model: model) }
            }
            .safeAreaInset(edge: .top) {
                // A refusal is the relay explaining a policy — a tier
                // mismatch, the privacy gate, missing authority. It is the
                // most useful thing on screen when it happens, so it sits
                // above the content rather than in a status line.
                if let refusal = model.lastRefusal {
                    HStack(alignment: .top, spacing: 8) {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                        Text(refusal).font(.callout).textSelection(.enabled)
                        Spacer()
                        Button("Dismiss") { model.lastRefusal = nil }
                            .buttonStyle(.link).font(.caption)
                    }
                    .padding(10)
                    .background(Color.orange.opacity(0.10))
                }
            }
        }
        .frame(minWidth: 1180, minHeight: 720)
        .task(id: model.selectedChannel) {
            if let selected = model.selectedChannel {
                model.selectedThread = nil
                await model.loadMessages(channel: selected)
                await model.loadThreads(channel: selected)
                await model.loadApprovals(channel: selected)
                model.startLive(channel: selected)
            }
        }
    }
}

/// Shown when the app has no identity to run as — better than an empty
/// window that looks like a connection failure.
struct NeedsConfigurationView: View {
    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "key.slash").font(.largeTitle).foregroundStyle(.secondary)
            Text("No identity configured").font(.headline)
            Text("Set MESH_RELAY_URL and MESH_PRIVATE_KEY.\nMeshVault will replace this with a real onboarding flow.")
                .font(.callout)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
        }
        .padding(40)
        .frame(minWidth: 520, minHeight: 300)
    }
}


/// Send a message to the selected channel.
struct Composer: View {
    @ObservedObject var model: WorkspaceModel
    @State private var draft = ""

    var body: some View {
        Divider()
        HStack(spacing: 8) {
            TextField("Message this channel", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .lineLimit(1...5)
                .onSubmit(send)
            Button(action: send) {
                Image(systemName: "arrow.up.circle.fill").font(.title3)
            }
            .buttonStyle(.plain)
            .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.isSending)
        }
        .padding(10)
    }

    private func send() {
        let content = draft
        guard !content.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        // Clear immediately: leaving the text in place after a send makes a
        // double-send one keystroke away, and the refusal banner is what
        // reports a rejection.
        draft = ""
        Task { await model.send(content: content) }
    }
}

/// Open a work thread without leaving the column it appears in.
struct NewThreadField: View {
    @ObservedObject var model: WorkspaceModel
    @State private var goal = ""

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: "plus.circle").foregroundStyle(.secondary).font(.caption)
            TextField("New thread — what needs doing?", text: $goal)
                .textFieldStyle(.plain)
                .font(.callout)
                .onSubmit(open)
        }
        .padding(.vertical, 2)
    }

    private func open() {
        let text = goal
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        goal = ""
        Task { await model.openThread(goal: text, deadline: nil) }
    }
}
