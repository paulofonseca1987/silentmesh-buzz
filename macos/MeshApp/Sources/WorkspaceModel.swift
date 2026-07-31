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

    private var client: MeshRelayClient?
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
            status = "connection failed: \(describe(error))"
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
            if let selected = selectedChannel { await loadMessages(channel: selected) }
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
