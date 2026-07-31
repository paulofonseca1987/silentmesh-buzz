import Foundation
import MeshProtocol
import MeshVault
import SwiftUI

/// One channel as the client understands it, folded from the relay's
/// kind:39000 metadata event.
struct ChannelSummary: Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let tier: MeshChannelTier
    let isPrivate: Bool
}

/// One message in a channel's timeline.
struct TimelineMessage: Identifiable, Equatable, Sendable {
    let id: String
    let author: String
    let content: String
    let createdAt: Date
    let kind: Int

    /// Relay-authored notices (overdue, canonicalization, gate reviews) are
    /// not member speech and must not be rendered as if they were — showing
    /// the relay's own actions under a member's name is a misattribution
    /// the member cannot correct.
    var isRelayNotice: Bool { MeshKind.relayOnly.contains(kind) }
}

/// The app's view of the workspace: connect, authenticate, fold events.
///
/// Deliberately holds no business rules. The relay decides what this member
/// may see and whether a write is allowed; the client's job is to present a
/// signed identity, verify what comes back, and render it.
@MainActor
final class WorkspaceModel: ObservableObject {
    @Published private(set) var channels: [ChannelSummary] = []
    @Published private(set) var messages: [TimelineMessage] = []
    @Published private(set) var status: String = "starting"
    @Published private(set) var identity: String = ""
    @Published var selectedChannel: String?
    @Published private(set) var threads: [MeshThread] = []
    @Published var selectedThread: String?
    /// The relay's own words when it refuses something. Silent Mesh
    /// refuses for reasons a member needs to read — a tier mismatch, the
    /// privacy gate, missing authority — so a refusal is surfaced rather
    /// than logged.
    @Published var lastRefusal: String?
    @Published private(set) var isSending = false
    /// Agent permission requests in the selected channel — the surface the
    /// roadmap's Phase 4 exit criterion turns on ("including granting a
    /// supervised approval").
    @Published private(set) var approvals: [MeshApproval] = []

    /// Decisions this client has published and the relay has accepted, but
    /// whose public record has not arrived yet.
    ///
    /// The relay commits the decision *before* emitting the kind:46011/46012
    /// that records it, so between the OK and that event the fold still says
    /// pending. Re-showing an Approve button there invites a second click,
    /// which the relay refuses as "already granted" — a confusing way to
    /// learn the first click worked. This is not optimism: the relay has
    /// already accepted it. The record replaces it as soon as it lands.
    private var decidedLocally: [String: MeshApprovalOutcome] = [:]

    private var client: MeshRelayClient?
    /// The live subscription for the selected channel. One at a time: a
    /// stream left running for a channel nobody is looking at keeps the
    /// relay fanning out to a window that will never show it.
    private var liveTask: Task<Void, Never>?
    private let relayURL: URL
    private let keys: MeshKeys

    /// Configuration comes from the environment for now. `MeshVault` is the
    /// slice that replaces this with a Secure-Enclave-wrapped key and a
    /// real onboarding flow; until then the app is honest about running on
    /// a key handed to it.
    /// Is there an identity to run as at all?
    let isConfigured: Bool

    init?(environment: [String: String] = ProcessInfo.processInfo.environment) {
        guard let urlString = environment["MESH_RELAY_URL"], let url = URL(string: urlString)
        else { return nil }
        // With MESH_VAULT=1 the app holds its own identity instead of being
        // handed one: an env key is imported into the vault on first run and
        // never read again, so a later launch has nothing to run on until
        // the member unlocks. That is the difference between an app given a
        // secret and an app that keeps one.
        let keys: MeshKeys
        if environment["MESH_VAULT"] == "1" {
            let vault = MeshVault(protection: .secureEnclaveWithUserPresence)
            if !vault.vaultExists, let seed = environment["MESH_PRIVATE_KEY"] {
                try? vault.seal(identityHex: seed)
            }
            guard let unlocked = try? vault.unlock(reason: "Unlock your Silent Mesh workspace")
            else { return nil }
            keys = unlocked
        } else {
            guard let keyHex = environment["MESH_PRIVATE_KEY"],
                let fromEnvironment = try? MeshKeys(privateKeyHex: keyHex)
            else { return nil }
            keys = fromEnvironment
        }
        relayURL = url
        self.keys = keys
        identity = String(keys.publicKeyHex.prefix(8))
        isConfigured = true
    }

    /// A model that renders the "configure me" state instead of crashing on
    /// launch — an app that dies when a variable is unset is far harder to
    /// diagnose than one that says what is missing.
    private init(unconfigured: Bool) {
        relayURL = URL(string: "ws://unconfigured.invalid")!
        keys = (try? MeshKeys()) ?? (try! MeshKeys(privateKeyHex: String(repeating: "01", count: 32)))
        isConfigured = false
        status = "no identity configured"
    }

    static let unconfigured = WorkspaceModel(unconfigured: true)

    func connect() async {
        let client = MeshRelayClient(url: relayURL, keys: keys)
        self.client = client
        do {
            status = "connecting to \(relayURL.host ?? "relay")"
            try await client.connect()
            try await client.authenticate()
            status = "connected as \(identity)…"
            await loadChannels()
        } catch {
            // Also to stderr: the status line truncates, and a connection
            // failure is exactly the case where the detail matters and the
            // window may not be where anyone is looking.
            let detail = describe(error)
            FileHandle.standardError.write(Data("mesh: connect failed: \(detail)\n".utf8))
            status = "connection failed: \(detail)"
        }
    }

    func loadChannels() async {
        guard let client else { return }
        do {
            let events = try await client.query(
                MeshFilter(kinds: [MeshKind.channelMetadata], limit: 200))
            var summaries: [ChannelSummary] = []
            for event in events {
                guard let id = event.tagValue("d") else { continue }
                let tier = MeshChannelTier(rawValue: event.tagValue("tier") ?? "") ?? .open
                let isPrivate = !event.tags.contains { $0.first == "public" }
                summaries.append(
                    ChannelSummary(
                        id: id,
                        name: event.tagValue("name") ?? id,
                        tier: tier,
                        isPrivate: isPrivate))
            }
            channels = summaries.sorted { $0.name.localizedCompare($1.name) == .orderedAscending }
            status = "\(channels.count) channels"
            if selectedChannel == nil { selectedChannel = channels.first?.id }
            if let selected = selectedChannel {
                await loadMessages(channel: selected)
                await loadThreads(channel: selected)
                await loadApprovals(channel: selected)
                startLive(channel: selected)
            }
        } catch {
            status = "channel load failed: \(describe(error))"
        }
    }

    func loadMessages(channel: String) async {
        guard let client else { return }
        do {
            let kinds = [
                MeshKind.chatMessage, MeshKind.channelMessage,
                MeshKind.workThreadOpen, MeshKind.workThreadGateReviewed,
            ]
            let events = try await client.query(
                MeshFilter(kinds: kinds, limit: 100, tags: ["#h": [channel]]))
            FileHandle.standardError.write(
                Data("mesh: messages query -> \(events.count) events\n".utf8))
            messages =
                events
                .map {
                    TimelineMessage(
                        id: $0.id,
                        author: String($0.pubkey.prefix(8)),
                        content: $0.content,
                        createdAt: Date(timeIntervalSince1970: TimeInterval($0.createdAt)),
                        kind: $0.kind)
                }
                .sorted { $0.createdAt < $1.createdAt }
        } catch {
            status = "message load failed: \(describe(error))"
        }
    }

    /// Watch the selected channel for new events.
    ///
    /// The relay pushes; the client folds. Before this, the timeline only
    /// changed when something in the app happened to reload — so a message
    /// from another member simply did not appear, which reads as "nobody
    /// is talking" rather than "this client is not listening".
    func startLive(channel: String) {
        liveTask?.cancel()
        liveTask = Task { [weak self] in
            guard let self, let client = self.client else { return }
            let kinds = [
                MeshKind.chatMessage, MeshKind.channelMessage,
                MeshKind.workThreadOpen, MeshKind.workThreadFork,
                MeshKind.workThreadPromote, MeshKind.workThreadMetadata,
                MeshKind.workThreadState, MeshKind.workThreadCheckpoint,
                MeshKind.workThreadOverdue, MeshKind.workThreadCanon,
                MeshKind.workThreadSiblingArchived, MeshKind.workThreadPromoted,
                MeshKind.workThreadGateReviewed,
                // An agent asking permission is blocked until someone
                // answers, so this is the one push that must not wait for a
                // reload to be noticed.
                MeshKind.approvalRequested, MeshKind.approvalGranted,
                MeshKind.approvalDenied,
            ]
            do {
                // `limit: 0` asks for no history: the load already fetched
                // it, and replaying it here would duplicate every row.
                let stream = try await client.subscribe(
                    MeshFilter(kinds: kinds, limit: 0, tags: ["#h": [channel]]))
                for await event in stream {
                    if Task.isCancelled { break }
                    await self.absorb(event, channel: channel)
                }
            } catch {
                FileHandle.standardError.write(
                    Data("mesh: live subscription failed: \(self.describe(error))\n".utf8))
            }
        }
    }

    /// Fold one pushed event into what is on screen.
    private func absorb(_ event: MeshEvent, channel: String) async {
        guard selectedChannel == channel else { return }
        switch event.kind {
        case MeshKind.approvalRequested, MeshKind.approvalGranted, MeshKind.approvalDenied:
            // Same reason as threads: the queue is a fold over the whole
            // trail, and patching one row in place is how a client ends up
            // showing a state the relay never had.
            await loadApprovals(channel: channel)
            return
        default:
            break
        }
        if MeshKind.isWorkThread(event.kind) {
            // Thread state is a fold over many events, so re-fold rather
            // than trying to patch it in place — a partial application is
            // how a client ends up showing a state the relay never had.
            await loadThreads(channel: channel)
            return
        }
        guard !messages.contains(where: { $0.id == event.id }) else { return }
        messages.append(
            TimelineMessage(
                id: event.id,
                author: String(event.pubkey.prefix(8)),
                content: event.content,
                createdAt: Date(timeIntervalSince1970: TimeInterval(event.createdAt)),
                kind: event.kind))
        messages.sort { $0.createdAt < $1.createdAt }
    }

    /// Post a message to the selected channel.
    ///
    /// Reloads afterwards rather than appending optimistically: the relay
    /// decides what is stored, and an optimistic row that the relay
    /// refused would be a lie the member acts on.
    func send(content: String) async {
        guard let client, let channel = selectedChannel else { return }
        isSending = true
        defer { isSending = false }
        do {
            var event = try MeshEvent.chatMessage(
                channel: channel, content: content, pubkey: keys.publicKeyHex)
            try event.sign(with: keys)
            try await client.publish(event)
            lastRefusal = nil
            await loadMessages(channel: channel)
        } catch {
            lastRefusal = describe(error)
        }
    }

    /// Open a work thread in the selected channel.
    func openThread(goal: String, deadline: Date?) async {
        guard let client, let channel = selectedChannel else { return }
        isSending = true
        defer { isSending = false }
        do {
            var event = try MeshEvent.openThread(
                channel: channel, goal: goal, deadline: deadline, pubkey: keys.publicKeyHex)
            try event.sign(with: keys)
            let id = try await client.publish(event)
            lastRefusal = nil
            await loadThreads(channel: channel)
            selectedThread = id
        } catch {
            lastRefusal = describe(error)
        }
    }

    /// Ask the Privacy Gate what promoting this thread would expose (D30).
    ///
    /// The answer arrives as a relay-signed kind:47023 attached to the
    /// thread, so this only submits the request and reloads; the review
    /// appears among the thread's notices when the local model is done.
    func requestGateReview(thread: String, draftSummary: String) async {
        guard let client, let channel = selectedChannel else { return }
        isSending = true
        defer { isSending = false }
        do {
            var event = try MeshEvent.gateReview(
                channel: channel, threadRoot: thread, draftSummary: draftSummary,
                pubkey: keys.publicKeyHex)
            try event.sign(with: keys)
            try await client.publish(event)
            lastRefusal = nil
        } catch {
            lastRefusal = describe(error)
        }
    }

    /// Poll for a thread's new notices — the gate review lands seconds
    /// later, since a local model is doing real work in between.
    func refreshThreads() async {
        guard let channel = selectedChannel else { return }
        await loadThreads(channel: channel)
    }

    /// Load the channel's approval trail and fold it into a decision queue.
    ///
    /// Only the relay-signed events are queried. The member's own
    /// kind:46030/46031 carries no `h` tag — it is addressed to a request,
    /// not to a channel — so a channel-scoped filter would never return it.
    /// That is the right shape anyway: what resolves a card is the relay's
    /// record of the outcome, not this client's memory of having clicked.
    func loadApprovals(channel: String) async {
        guard let client else { return }
        do {
            let events = try await client.query(
                MeshFilter(
                    kinds: [
                        MeshKind.approvalRequested, MeshKind.approvalGranted,
                        MeshKind.approvalDenied,
                    ],
                    limit: 200, tags: ["#h": [channel]]))
            var folded = MeshApprovalFold.approvals(from: events)
            for index in folded.indices {
                guard let mine = decidedLocally[folded[index].tokenHash] else { continue }
                if folded[index].outcome == .pending {
                    folded[index].outcome = mine
                } else {
                    // The relay's own record has arrived and outranks ours.
                    decidedLocally[folded[index].tokenHash] = nil
                }
            }
            approvals = folded
        } catch {
            if !(error is CancellationError) {
                status = "approval load failed: \(describe(error))"
            }
        }
    }

    /// Grant or deny a pending agent permission request.
    ///
    /// The relay is the authority on whether this is allowed at all: an
    /// agent may not decide its own request, only a full member of the
    /// request's channel may decide, and an expired or already-decided
    /// request is refused. Each of those comes back as a sentence worth
    /// reading, so a refusal goes to the banner rather than the log.
    func decide(_ approval: MeshApproval, grant: Bool, note: String = "") async {
        guard let client, let channel = selectedChannel else { return }
        isSending = true
        defer { isSending = false }
        do {
            var event = try MeshEvent.approvalDecision(
                tokenHash: approval.tokenHash, grant: grant, note: note,
                pubkey: keys.publicKeyHex)
            try event.sign(with: keys)
            try await client.publish(event)
            lastRefusal = nil
            decidedLocally[approval.tokenHash] =
                grant ? .granted(by: keys.publicKeyHex) : .denied(by: keys.publicKeyHex)
            await loadApprovals(channel: channel)
        } catch {
            lastRefusal = describe(error)
            // Re-fold either way: "already granted" usually means someone
            // else decided it, and the queue should show that rather than
            // keep offering a button that will be refused again.
            await loadApprovals(channel: channel)
        }
    }

    /// Load every work-thread event in the channel and fold them.
    ///
    /// One query for the whole family, then `MeshFold` rebuilds the D41
    /// projection locally. The relay is not asked what state a thread is
    /// in — the signed events say, and the client can therefore show a
    /// thread it has cached, and notice if a relay ever contradicts them.
    func loadThreads(channel: String) async {
        guard let client else { return }
        let kinds = [
            MeshKind.workThreadOpen, MeshKind.workThreadMetadata, MeshKind.workThreadState,
            MeshKind.workThreadCheckpoint, MeshKind.workThreadOverdue, MeshKind.workThreadCanon,
            MeshKind.workThreadSiblingArchived, MeshKind.workThreadPromoted,
            MeshKind.workThreadFork, MeshKind.workThreadPromote,
            MeshKind.workThreadGateReviewed,
        ]
        do {
            let events = try await client.query(
                MeshFilter(kinds: kinds, limit: 500, tags: ["#h": [channel]]))
            threads = MeshFold.threads(from: events).sorted { $0.createdAt > $1.createdAt }
            FileHandle.standardError.write(
                Data("mesh: threads query -> \(events.count) events, \(threads.count) threads\n".utf8))
            if let selected = selectedThread, !threads.contains(where: { $0.id == selected }) {
                selectedThread = nil
            }
        } catch {
            // Cancellation is not a failure worth shouting about: SwiftUI
            // cancels a `.task` whenever its id changes, which happens on
            // every channel switch.
            let detail = describe(error)
            FileHandle.standardError.write(Data("mesh: threads query failed: \(detail)\n".utf8))
            if !(error is CancellationError) { status = "thread load failed: \(detail)" }
        }
    }

    /// Surface the relay's own words. A refusal here is usually a policy
    /// decision the member needs to read (tier mismatch, privacy gate,
    /// missing authority), not a transport hiccup to be swallowed.
    private func describe(_ error: Error) -> String {
        switch error {
        case MeshProtocolError.relay(let reason): return reason
        case MeshProtocolError.timeout(let what): return "timed out: \(what)"
        case MeshProtocolError.transport(let what): return what
        case MeshProtocolError.malformed(let what): return what
        default: return String(describing: error)
        }
    }
}
