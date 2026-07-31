import Foundation

/// Event kinds, mirroring `crates/buzz-core/src/kind.rs`.
///
/// The Rust registry is the source of truth; this is a client-side mirror
/// of the kinds a Silent Mesh client actually handles. `KindParityTests`
/// reads `kind.rs` and fails if any value here has drifted, because a
/// client that disagrees with the relay about a kind integer does not fail
/// loudly — it silently ignores events, which looks like an empty channel.
public enum MeshKind {
    // Chat + channel surface
    public static let textNote = 1
    public static let chatMessage = 9
    public static let channelMessage = 40002
    public static let channelMetadata = 39000
    public static let channelMembers = 39002
    public static let clientAuth = 22242

    // Work threads (Silent Mesh Phase 2)
    public static let workThreadOpen = 47000
    public static let workThreadMetadata = 47001
    public static let workThreadState = 47002
    public static let workThreadRecommend = 47003
    public static let workThreadCheckpoint = 47010
    public static let workThreadOverdue = 47011
    public static let workThreadCanon = 47012
    public static let workThreadSiblingArchived = 47013
    public static let workThreadPromoted = 47014
    public static let workThreadFork = 47020
    public static let workThreadPromote = 47021
    public static let workThreadGateReview = 47022
    public static let workThreadGateReviewed = 47023

    // Model plane (Silent Mesh Phase 3)
    public static let agentTurnMetric = 44200
    public static let agentTurnAttribution = 44201

    // Approvals. 46010/46011/46012 are relay-signed records; 46030/46031
    // are the member's own signed decision. The request kinds are shared
    // with the workflow approval gate — see `MeshApproval`, which reads the
    // `domain` field rather than trusting the kind alone.
    public static let approvalRequested = 46010
    public static let approvalGranted = 46011
    public static let approvalDenied = 46012
    public static let approvalGrant = 46030
    public static let approvalDeny = 46031

    /// Kinds only the relay may author. A client that renders one of these
    /// as if a member wrote it is misattributing the relay's own actions.
    public static let relayOnly: Set<Int> = [
        workThreadOverdue,
        workThreadCanon,
        workThreadSiblingArchived,
        workThreadPromoted,
        workThreadGateReviewed,
        approvalRequested,
        approvalGranted,
        approvalDenied,
    ]

    /// Is this one of the work-thread kinds (47000–47023)?
    public static func isWorkThread(_ kind: Int) -> Bool {
        (47000...47003).contains(kind) || (47010...47014).contains(kind)
            || (47020...47023).contains(kind)
    }
}

/// A channel's privacy tier (D24) — the floor that model routing must
/// satisfy for work in that channel.
public enum MeshChannelTier: String, Codable, Sendable, CaseIterable {
    /// Local models only; zero egress.
    case owned
    /// Local or TEE-attested providers.
    case `private`
    /// Members' own vendor subscriptions allowed.
    case open

    /// Ascending strictness — `open` (0) → `private` (1) → `owned` (2).
    public var strictness: Int {
        switch self {
        case .open: return 0
        case .private: return 1
        case .owned: return 2
        }
    }

    /// Does this tier permit no more egress than `other`? Mirrors
    /// `ChannelTier::is_at_least_as_strict_as` — the rule promotion
    /// enforces, so the client can explain a refusal before making the
    /// member discover it.
    public func isAtLeastAsStrict(as other: MeshChannelTier) -> Bool {
        strictness >= other.strictness
    }
}
