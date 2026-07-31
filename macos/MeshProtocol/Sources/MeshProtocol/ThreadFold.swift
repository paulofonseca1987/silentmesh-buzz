import Foundation

/// A work thread's D41 state.
public enum MeshThreadStatus: String, Sendable, Equatable, CaseIterable {
    case open, snoozed, ready, closed, archived
}

/// A work thread as the client understands it, assembled from signed
/// events rather than read from a server's summary.
public struct MeshThread: Sendable, Equatable, Identifiable {
    public var id: String
    public var channelID: String
    public var goal: String
    public var deadline: Int64?
    public var dri: String?
    public var status: MeshThreadStatus
    public var createdBy: String
    public var createdAt: Int64
    /// Parent thread when this root is a kind:47020 fork.
    public var forkedFrom: String?
    /// Fork-point checkpoint (`nil` = forked at head, or not a fork).
    public var forkCommit: String?
    /// Source thread when this root is a kind:47021 promotion.
    public var promotedFrom: String?
    /// Source (personal) channel when this root is a promotion.
    public var promotedChannel: String?
    /// Checkpoint commits in the order the thread recorded them.
    public var checkpoints: [String]
    /// Relay-authored notices about this thread, oldest first.
    public var notices: [MeshThreadNotice]
}

/// Something the relay did to a thread, on its own initiative.
public struct MeshThreadNotice: Sendable, Equatable {
    public enum Kind: Sendable, Equatable {
        case overdue, canonicalized, siblingArchived, promoted, gateReview
    }
    public var kind: Kind
    public var at: Int64
    public var body: String
}

/// Folds signed events into workspace state.
///
/// The events are the truth: every 47001/47002 command the relay stored was
/// one it applied (refusals are never stored), and its own notices record
/// what its sweeps and close flows did. Replaying them in order therefore
/// reproduces the relay's projection without asking the relay what it
/// thinks — which is what lets a client work from cache, verify what it
/// shows, and disagree loudly if a relay ever serves something inconsistent.
public enum MeshFold {
    /// Sort key mirroring `buzz-cli`'s `fold_key`.
    ///
    /// The middle term is load-bearing: a relay notice is only ever written
    /// *after* the command it reports on, but clock granularity is one
    /// second, so a same-second tie must resolve in the notice's favour.
    /// Without it a thread the relay archived can keep rendering as open —
    /// the exact bug the CLI hit in the Phase 2 dry run.
    static func foldKey(_ event: MeshEvent) -> (Int64, Int, String) {
        let isRelayNotice = MeshKind.relayOnly.contains(event.kind) ? 1 : 0
        return (event.createdAt, isRelayNotice, event.id)
    }

    static func sorted(_ events: [MeshEvent]) -> [MeshEvent] {
        events.sorted {
            let (a, b) = (foldKey($0), foldKey($1))
            if a.0 != b.0 { return a.0 < b.0 }
            if a.1 != b.1 { return a.1 < b.1 }
            return a.2 < b.2
        }
    }

    /// Build every thread visible in `events`.
    ///
    /// Roots are 47000 (opened), 47020 (forked), and 47021 (promoted — a
    /// promotion event *is* the new thread's root, so its id is the new
    /// thread id). Events referencing an unknown root are ignored rather
    /// than guessed at: a partial page is normal, and inventing a thread
    /// from a stray command would render state no one authored.
    public static func threads(from events: [MeshEvent]) -> [MeshThread] {
        var threads: [String: MeshThread] = [:]
        let ordered = sorted(events)

        for event in ordered {
            switch event.kind {
            case MeshKind.workThreadOpen, MeshKind.workThreadFork, MeshKind.workThreadPromote:
                threads[event.id] = root(from: event)
            default:
                continue
            }
        }

        for event in ordered {
            guard let root = event.tagValue("e"), var thread = threads[root] else { continue }
            switch event.kind {
            case MeshKind.workThreadMetadata:
                apply(metadata: event, to: &thread)
            case MeshKind.workThreadState:
                if let state = event.tagValue("state"),
                    let status = MeshThreadStatus(rawValue: state)
                {
                    thread.status = status
                }
            case MeshKind.workThreadCheckpoint:
                if let commit = event.tagValue("commit") { thread.checkpoints.append(commit) }
            case MeshKind.workThreadOverdue:
                thread.notices.append(
                    MeshThreadNotice(kind: .overdue, at: event.createdAt, body: event.content))
            case MeshKind.workThreadCanon:
                thread.notices.append(
                    MeshThreadNotice(kind: .canonicalized, at: event.createdAt, body: event.content))
            case MeshKind.workThreadSiblingArchived:
                // A winner's close archived this thread as a losing sibling.
                thread.status = .archived
                thread.notices.append(
                    MeshThreadNotice(
                        kind: .siblingArchived, at: event.createdAt, body: event.content))
            case MeshKind.workThreadPromoted:
                // Promoted away: the relay closed this thread in place,
                // and the files and summary moved to the target channel.
                thread.status = .closed
                thread.notices.append(
                    MeshThreadNotice(kind: .promoted, at: event.createdAt, body: event.content))
            case MeshKind.workThreadGateReviewed:
                thread.notices.append(
                    MeshThreadNotice(kind: .gateReview, at: event.createdAt, body: event.content))
            default:
                continue
            }
            threads[root] = thread
        }

        return threads.values.sorted { $0.createdAt < $1.createdAt }
    }

    private static func root(from event: MeshEvent) -> MeshThread {
        let isFork = event.kind == MeshKind.workThreadFork
        let isPromotion = event.kind == MeshKind.workThreadPromote
        return MeshThread(
            id: event.id,
            channelID: event.tagValue("h") ?? "",
            goal: event.content,
            deadline: event.tagValue("deadline").flatMap { Int64($0) },
            dri: event.tagValue("dri"),
            status: .open,
            createdBy: event.pubkey,
            createdAt: event.createdAt,
            forkedFrom: isFork ? event.tagValue("e") : nil,
            forkCommit: isFork ? event.tagValue("commit") : nil,
            promotedFrom: isPromotion ? event.tagValue("e") : nil,
            promotedChannel: isPromotion ? event.tagValue("from") : nil,
            checkpoints: [],
            notices: [])
    }

    /// Apply a kind:47001 metadata command.
    ///
    /// `null` clears a field and an absent field leaves it alone — the two
    /// are different intentions, and collapsing them would make "remove the
    /// deadline" indistinguishable from "change only the goal".
    private static func apply(metadata event: MeshEvent, to thread: inout MeshThread) {
        guard let data = event.content.data(using: .utf8),
            let body = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let goal = body["goal"] as? String { thread.goal = goal }
        if body.keys.contains("deadline") {
            thread.deadline = body["deadline"] is NSNull ? nil : (body["deadline"] as? NSNumber)?.int64Value
        }
        if body.keys.contains("dri") {
            thread.dri = body["dri"] is NSNull ? nil : body["dri"] as? String
        }
    }
}
