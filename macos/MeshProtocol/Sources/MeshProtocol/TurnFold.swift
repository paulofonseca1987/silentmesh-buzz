import Foundation

/// Where an agent turn has got to, as a channel member can see it.
///
/// Deliberately coarse. The fine-grained transcript (tool calls, streamed
/// text) rides on kind:24200, which is ephemeral and encrypted to the
/// agent's *owner* — an ordinary member cannot read it at all. These five
/// states are the part of a turn that is cleartext and visible to everyone
/// in the channel, and they answer the question a member actually has:
/// did it hear me, is it working, and what came of it.
public enum MeshTurnState: Sendable, Equatable {
    /// 👀 — queued; an agent has taken the message but is not prompting yet.
    case queued
    /// 💬 — the model is being prompted.
    case working
    /// The agent replied.
    case answered(eventID: String)
    /// The harness refused. On the Silent Mesh tier-gate paths this is the
    /// *only* thing a turn emits — no answer, no metrics — so it is a
    /// distinct state rather than an answer that happens to read badly.
    case refused(reason: String)
    /// Reactions cleared with nothing to show for it. The turn ended: it may
    /// have crashed, been cancelled, or answered somewhere this client is
    /// not looking.
    case ended
}

/// One agent turn, keyed by the message that woke it.
public struct MeshTurn: Sendable, Equatable, Identifiable {
    /// The event that triggered the turn — a member's message.
    public var triggerEventID: String
    /// The agent, learned from whoever reacted rather than from config.
    public var agentPubkey: String
    public var state: MeshTurnState
    /// When the agent first acknowledged the message.
    public var startedAt: Int64
    /// When the turn last changed state — what "3s ago" is measured from.
    public var updatedAt: Int64

    public var id: String { triggerEventID }

    /// Is something still expected to happen?
    public var isLive: Bool { state == .queued || state == .working }
}

/// Folds an agent turn out of the cleartext events any channel member sees.
///
/// The wire shape, verified against `buzz-acp` and `buzz-sdk` rather than
/// assumed:
///
/// - **kind:7** — `content` is the emoji, one `e` tag pointing at the
///   *triggering message*. `👀` on queue, `💬` immediately before the prompt.
/// - **kind:5** — one `e` tag pointing at the **reaction event**, not at the
///   message. Published by `ReactionGuard::drop`, so it fires on every exit
///   path including panic, which makes it the most reliable "this turn is
///   over" signal there is.
///
/// That indirection is the whole difficulty: ending a turn is a *two-hop*
/// correlation — message → reaction ids → deletions. A fold that matched
/// deletions against the message id directly would find nothing and show
/// every turn as permanently working.
///
/// Neither kind carries an `h` tag. They still match a channel-scoped
/// subscription, because the relay falls back to the stored `channel_id`
/// when an event has no `h` tag at all (`buzz-core/src/filter.rs`) — a
/// fallback whose comment names these two kinds. Historical backfill is a
/// different matter and needs `#e`.
///
/// ## Turn state is live-only, and this is forced by the relay
///
/// The relay honours NIP-09 by **soft-deleting** the reaction: it sets
/// `deleted_at` on the kind:7 row, and every query filters
/// `deleted_at IS NULL` (`buzz-db/src/event.rs`). So the instant a turn
/// ends, its 👀/💬 stop coming back from queries — and the kind:5 that
/// retired them names an event nobody can fetch any more.
///
/// A completed turn is therefore **not reconstructible from a query**. Only
/// a client that watched it happen holds the reaction ids needed to
/// correlate the ending. Feed this fold from the live stream and let it
/// accumulate; re-querying does not merely cost more, it returns a
/// different and emptier answer. That is asserted in the interop suite, not
/// assumed.
///
/// This is the right shape anyway: turn state is what is happening *now*.
/// The durable record of a turn is its answer, which is an ordinary message
/// and lives in the timeline like any other.
public enum MeshTurnFold {
    private struct Reaction {
        let id: String
        let emoji: String
        let author: String
        let at: Int64
    }

    /// The two lifecycle emoji. Any other reaction is a member reacting to a
    /// message and must not be read as agent activity.
    static let queuedEmoji = "👀"
    static let workingEmoji = "💬"

    /// Build every visible turn in `events`, newest first.
    public static func turns(from events: [MeshEvent]) -> [MeshTurn] {
        // Pass 1: deletions, by the reaction id they retire.
        var deleted: Set<String> = []
        for event in events where event.kind == MeshKind.deletion {
            for target in event.tagValues("e") { deleted.insert(target) }
        }

        // Pass 2: lifecycle reactions, grouped by the message they mark.
        var byTrigger: [String: [Reaction]] = [:]
        for event in events where event.kind == MeshKind.reaction {
            guard event.content == queuedEmoji || event.content == workingEmoji,
                let trigger = event.tagValue("e")
            else { continue }
            byTrigger[trigger, default: []].append(
                Reaction(
                    id: event.id, emoji: event.content, author: event.pubkey,
                    at: event.createdAt))
        }

        // Pass 3: replies, by the message they answer. Only the agent's own
        // replies count — another member answering in-thread is conversation,
        // not a turn result.
        var repliesByTrigger: [String: [MeshEvent]] = [:]
        for event in events
        where event.kind == MeshKind.chatMessage || event.kind == MeshKind.channelMessage {
            for target in event.tagValues("e") {
                repliesByTrigger[target, default: []].append(event)
            }
        }

        var turns: [MeshTurn] = []
        for (trigger, reactions) in byTrigger {
            guard let first = reactions.min(by: { ($0.at, $0.id) < ($1.at, $1.id) })
            else { continue }
            let agent = first.author
            let live = reactions.filter { !deleted.contains($0.id) }
            let lastActivity = reactions.map(\.at).max() ?? first.at

            // A reply from the agent is terminal and outranks the reactions:
            // the 💬 removal is fire-and-forget and may lag or be lost, so
            // waiting for it before showing an answer that has already
            // arrived would leave the turn reading as "working" forever.
            let answer = repliesByTrigger[trigger]?
                .filter { $0.pubkey == agent }
                .min(by: { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) })

            let state: MeshTurnState
            if let answer {
                state =
                    answer.content.hasPrefix("⚠️")
                    ? .refused(reason: answer.content)
                    : .answered(eventID: answer.id)
            } else if live.contains(where: { $0.emoji == workingEmoji }) {
                state = .working
            } else if live.contains(where: { $0.emoji == queuedEmoji }) {
                state = .queued
            } else {
                state = .ended
            }

            turns.append(
                MeshTurn(
                    triggerEventID: trigger,
                    agentPubkey: agent,
                    state: state,
                    startedAt: first.at,
                    updatedAt: max(lastActivity, answer?.createdAt ?? 0)))
        }

        // Newest first, ties by trigger id — same discipline as the other
        // folds, and for the same reason: a dictionary has no order and
        // Swift's sort is not stable, so same-second turns would otherwise
        // shuffle on every re-fold.
        return turns.sorted {
            $0.startedAt != $1.startedAt
                ? $0.startedAt > $1.startedAt
                : $0.triggerEventID < $1.triggerEventID
        }
    }
}
