//! Work threads — open, list, show, set, state, fork, recommend (Silent
//! Mesh Phase 2, kinds 47000–47003, 47010, 47013, 47020).
//!
//! Writes are signed events on the generic submit path: 47000 roots, 47020
//! fork roots, and 47003 recommendations store append-only; 47001/47002 are
//! relay-validated commands (D41 authority + TOCTOU-safe projection
//! updates).
//!
//! Reads fold the signed events client-side — the events are the truth.
//! Every stored 47001/47002 was applied by the relay (rejected commands are
//! never stored), and the relay's own notices (47011–47014) record what its
//! sweeps and close flows did, so folding them in `created_at` order
//! reproduces the projection. Same-second ties resolve in favour of relay
//! notices (see [`fold_key`]) because a notice is only written after the
//! command it reports on; skewed client clocks can still differ from the
//! relay's row, which remains authoritative.

use crate::client::BuzzClient;
use crate::error::CliError;
use crate::validate::sdk_err;

/// Validate a 64-hex thread root id.
fn validate_thread_id(thread: &str) -> Result<String, CliError> {
    let trimmed = thread.trim();
    if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(CliError::Usage(
            "thread must be a 64-char hex event id (the kind:47000 root)".into(),
        ));
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// First value of a named tag on a raw event JSON object.
fn tag_value<'a>(event: &'a serde_json::Value, name: &str) -> Option<&'a str> {
    event.get("tags")?.as_array()?.iter().find_map(|t| {
        let t = t.as_array()?;
        (t.first()?.as_str()? == name).then(|| t.get(1)?.as_str())?
    })
}

/// Order key for folding: `(created_at, relay_notice, id)`.
///
/// The relay stores commands in validation order, so `created_at`
/// reproduces it. The middle term breaks same-second ties in favour of
/// relay-emitted notices (47011–47014): a notice is only ever written as
/// a consequence of a command the relay already applied, so it is
/// causally later even when the one-second wire clock cannot show it.
/// Without this, a losing thread's own `ready` command could sort after
/// the sibling-archive notice that superseded it and the fold would
/// permanently contradict the projection — observed in the Phase 2
/// exit-criterion dry run, where both events shared a second and the id
/// tie-break happened to favour the command.
///
/// Residual (documented, far rarer): two *relay* notices, or a client
/// command genuinely issued after a notice within the same second, still
/// fall back to id order. The relay's `work_threads` row stays
/// authoritative in every case.
fn fold_key(event: &serde_json::Value) -> (i64, u8, String) {
    let kind = event.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
    let relay_notice = u8::from(matches!(kind, 47011..=47014 | 47023));
    (
        event
            .get("created_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        relay_notice,
        event
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned(),
    )
}

/// A folded thread view assembled from the signed events.
struct FoldedThread {
    thread_id: String,
    channel_id: String,
    goal: String,
    deadline: Option<i64>,
    dri: Option<String>,
    status: String,
    created_by: String,
    created_at: i64,
    /// Parent thread root when the root is a kind:47020 fork.
    forked_from: Option<String>,
    /// Fork-point checkpoint commit (`None` = forked at head, or not a fork).
    fork_commit: Option<String>,
    /// Source thread root when the root is a kind:47021 promotion.
    promoted_from: Option<String>,
    /// Source (personal) channel when the root is a kind:47021 promotion.
    promoted_channel: Option<String>,
}

impl FoldedThread {
    fn from_root(root: &serde_json::Value) -> Self {
        let kind = root.get("kind").and_then(|v| v.as_u64());
        let is_fork = kind == Some(47020);
        let is_promotion = kind == Some(47021);
        FoldedThread {
            thread_id: root
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            channel_id: tag_value(root, "h").unwrap_or_default().to_owned(),
            goal: root
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            deadline: tag_value(root, "deadline").and_then(|v| v.parse().ok()),
            dri: tag_value(root, "dri").map(str::to_owned),
            status: "open".to_owned(),
            created_by: root
                .get("pubkey")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
            created_at: root.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            forked_from: is_fork
                .then(|| tag_value(root, "e").map(str::to_owned))
                .flatten(),
            fork_commit: is_fork
                .then(|| tag_value(root, "commit").map(str::to_owned))
                .flatten(),
            promoted_from: is_promotion
                .then(|| tag_value(root, "e").map(str::to_owned))
                .flatten(),
            promoted_channel: is_promotion
                .then(|| tag_value(root, "from").map(str::to_owned))
                .flatten(),
        }
    }

    /// Apply a kind:47001 metadata command body.
    fn apply_metadata(&mut self, event: &serde_json::Value) {
        let Some(body) = event
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        else {
            return;
        };
        if let Some(goal) = body.get("goal").and_then(|v| v.as_str()) {
            self.goal = goal.to_owned();
        }
        match body.get("deadline") {
            Some(serde_json::Value::Null) => self.deadline = None,
            Some(v) => {
                if let Some(secs) = v.as_i64() {
                    self.deadline = Some(secs);
                }
            }
            None => {}
        }
        match body.get("dri") {
            Some(serde_json::Value::Null) => self.dri = None,
            Some(v) => {
                if let Some(pk) = v.as_str() {
                    self.dri = Some(pk.to_owned());
                }
            }
            None => {}
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "thread_id": self.thread_id,
            "channel_id": self.channel_id,
            "goal": self.goal,
            "deadline": self.deadline,
            "dri": self.dri,
            "status": self.status,
            "created_by": self.created_by,
            "created_at": self.created_at,
            "forked_from": self.forked_from,
            "fork_commit": self.fork_commit,
            "promoted_from": self.promoted_from,
            "promoted_channel": self.promoted_channel,
        })
    }
}

/// Fold 47001/47002 commands and relay-signed 47013/47014 notices
/// (already sorted) over the root view.
fn fold_commands(thread: &mut FoldedThread, commands: &[serde_json::Value]) {
    for event in commands {
        match event.get("kind").and_then(|v| v.as_u64()) {
            Some(47001) => thread.apply_metadata(event),
            Some(47002) => {
                if let Some(state) = tag_value(event, "state") {
                    thread.status = state.to_owned();
                }
            }
            // A winner's close archived this thread as a losing sibling.
            Some(47013) => thread.status = "archived".to_owned(),
            // This thread was promoted to a team channel — the relay
            // closed it in place; the files and summary moved.
            Some(47014) => thread.status = "closed".to_owned(),
            _ => {}
        }
    }
}

/// Fetch all 47001/47002 commands (plus 47013/47014 relay notices) for a
/// set of thread roots, sorted for folding.
async fn fetch_commands(
    client: &BuzzClient,
    channel: &str,
    roots: &[String],
) -> Result<Vec<serde_json::Value>, CliError> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let filter = serde_json::json!({
        "kinds": [47001, 47002, 47013, 47014],
        "#h": [channel],
        "#e": roots,
    });
    let mut commands = client.query_all(filter).await?;
    commands.sort_by_key(fold_key);
    Ok(commands)
}

/// Open a work thread (kind 47000). The response echoes the write result
/// with the new `thread_id` (the root event id).
pub async fn cmd_open_thread(
    client: &BuzzClient,
    channel: &str,
    goal: &str,
    deadline: Option<i64>,
    dri: Option<&str>,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let builder = buzz_sdk::build_thread_open(channel_id, goal, deadline, dri).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let thread_id = event.id.to_hex();
    let resp = client.submit_event(event).await?;
    let normalized = crate::client::normalize_write_response(&resp);
    match serde_json::from_str::<serde_json::Value>(&normalized) {
        Ok(mut v) if v.is_object() => {
            v["thread_id"] = serde_json::Value::String(thread_id);
            println!("{v}");
        }
        _ => println!("{normalized}"),
    }
    Ok(())
}

/// List threads in a channel with folded status.
pub async fn cmd_list_threads(
    client: &BuzzClient,
    channel: &str,
    status: Option<&str>,
    limit: Option<u32>,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    if let Some(status) = status {
        if !buzz_sdk::THREAD_STATES.contains(&status) {
            return Err(CliError::Usage(format!(
                "invalid status '{status}' (valid: {})",
                buzz_sdk::THREAD_STATES.join(", ")
            )));
        }
    }
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let filter = serde_json::json!({
        "kinds": [47000, 47020, 47021],
        "#h": [channel],
        "limit": limit,
    });
    let roots = client.query_paginated(filter, limit).await?;
    let root_ids: Vec<String> = roots
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let commands = fetch_commands(client, channel, &root_ids).await?;

    let mut out = Vec::with_capacity(roots.len());
    for root in &roots {
        let mut folded = FoldedThread::from_root(root);
        let own: Vec<serde_json::Value> = commands
            .iter()
            .filter(|c| tag_value(c, "e") == Some(folded.thread_id.as_str()))
            .cloned()
            .collect();
        fold_commands(&mut folded, &own);
        if status.is_none_or(|s| s == folded.status) {
            out.push(folded.to_json());
        }
    }
    println!("{}", serde_json::Value::Array(out));
    Ok(())
}

/// Show one thread: folded view plus its recommendations.
pub async fn cmd_show_thread(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let thread_id = validate_thread_id(thread)?;

    let root_filter = serde_json::json!({
        "kinds": [47000, 47020, 47021],
        "#h": [channel],
        "ids": [thread_id],
    });
    let resp = client.query(&root_filter).await?;
    let roots: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    let Some(root) = roots.first() else {
        println!("null");
        return Ok(());
    };

    let mut folded = FoldedThread::from_root(root);
    let commands = fetch_commands(client, channel, std::slice::from_ref(&thread_id)).await?;
    fold_commands(&mut folded, &commands);

    let rec_filter = serde_json::json!({
        "kinds": [47003],
        "#h": [channel],
        "#e": [thread_id],
    });
    let rec_resp = client.query(&rec_filter).await?;
    let mut recommendations: Vec<serde_json::Value> =
        serde_json::from_str(&rec_resp).unwrap_or_default();
    recommendations.sort_by_key(fold_key);
    let recommendations: Vec<serde_json::Value> = recommendations
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                "author": r.get("pubkey").and_then(|v| v.as_str()).unwrap_or_default(),
                "note": r.get("content").and_then(|v| v.as_str()).unwrap_or_default(),
                "created_at": r.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let cp_filter = serde_json::json!({
        "kinds": [47010],
        "#h": [channel],
        "#e": [thread_id],
    });
    let cp_resp = client.query(&cp_filter).await?;
    let mut checkpoints: Vec<serde_json::Value> =
        serde_json::from_str(&cp_resp).unwrap_or_default();
    checkpoints.sort_by_key(fold_key);
    let checkpoints: Vec<serde_json::Value> = checkpoints
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                "author": c.get("pubkey").and_then(|v| v.as_str()).unwrap_or_default(),
                "commit": tag_value(c, "commit").unwrap_or_default(),
                "branch": tag_value(c, "branch"),
                "turn": tag_value(c, "turn").and_then(|v| v.parse::<u32>().ok()),
                "note": c.get("content").and_then(|v| v.as_str()).unwrap_or_default(),
                "created_at": c.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    // Relay-emitted notices about this thread: overdue (47011),
    // canonicalization outcome (47012), sibling-archive (47013),
    // promotion (47014), and gate review (47023). These are the relay's
    // own signals — an agent
    // driving a thread learns its deadline passed or its canonicalization
    // failed from here, so `show` surfaces them alongside the human and
    // agent events rather than leaving them invisible to the CLI.
    let notice_filter = serde_json::json!({
        "kinds": [47011, 47012, 47013, 47014, 47023],
        "#h": [channel],
        "#e": [thread_id],
    });
    let notice_resp = client.query(&notice_filter).await?;
    let mut notices: Vec<serde_json::Value> =
        serde_json::from_str(&notice_resp).unwrap_or_default();
    notices.sort_by_key(fold_key);
    let notices: Vec<serde_json::Value> = notices
        .iter()
        .map(|n| {
            let kind = n.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);
            let body: Option<serde_json::Value> = n
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .and_then(|s| serde_json::from_str(s).ok());
            serde_json::json!({
                "id": n.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                "kind": kind,
                "type": match kind {
                    47011 => "overdue",
                    47012 => "canonicalized",
                    47013 => "sibling_archived",
                    47014 => "promoted",
                    47023 => "gate_review",
                    _ => "unknown",
                },
                // The DRI the overdue notice tags, when present.
                "dri": tag_value(n, "p"),
                "commit": tag_value(n, "commit"),
                "winner": tag_value(n, "winner"),
                "to": tag_value(n, "to"),
                "thread": tag_value(n, "thread"),
                "body": body,
                "created_at": n.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    // Forks of this thread (kind:47020 roots referencing it as parent).
    let fork_filter = serde_json::json!({
        "kinds": [47020],
        "#h": [channel],
        "#e": [thread_id],
    });
    let fork_resp = client.query(&fork_filter).await?;
    let mut forks: Vec<serde_json::Value> = serde_json::from_str(&fork_resp).unwrap_or_default();
    forks.sort_by_key(fold_key);
    let forks: Vec<serde_json::Value> = forks
        .iter()
        .map(|f| {
            serde_json::json!({
                "thread_id": f.get("id").and_then(|v| v.as_str()).unwrap_or_default(),
                "author": f.get("pubkey").and_then(|v| v.as_str()).unwrap_or_default(),
                "goal": f.get("content").and_then(|v| v.as_str()).unwrap_or_default(),
                "fork_commit": tag_value(f, "commit"),
                "created_at": f.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let mut view = folded.to_json();
    view["recommendations"] = serde_json::Value::Array(recommendations);
    view["checkpoints"] = serde_json::Value::Array(checkpoints);
    view["forks"] = serde_json::Value::Array(forks);
    view["notices"] = serde_json::Value::Array(notices);
    println!("{view}");
    Ok(())
}

/// Fork a thread into a variation (kind 47020) at head or a checkpoint.
/// The response echoes the write result with the new `thread_id` (the fork
/// event id).
pub async fn cmd_fork_thread(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    goal: &str,
    commit: Option<&str>,
    deadline: Option<i64>,
    dri: Option<&str>,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let parent_id = validate_thread_id(thread)?;
    let builder = buzz_sdk::build_thread_fork(channel_id, &parent_id, goal, commit, deadline, dri)
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let thread_id = event.id.to_hex();
    let resp = client.submit_event(event).await?;
    let normalized = crate::client::normalize_write_response(&resp);
    match serde_json::from_str::<serde_json::Value>(&normalized) {
        Ok(mut v) if v.is_object() => {
            v["thread_id"] = serde_json::Value::String(thread_id);
            v["forked_from"] = serde_json::Value::String(parent_id);
            println!("{v}");
        }
        _ => println!("{normalized}"),
    }
    Ok(())
}

/// Edit task metadata (kind 47001).
#[allow(clippy::too_many_arguments)]
pub async fn cmd_set_thread(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    goal: Option<&str>,
    deadline: Option<i64>,
    clear_deadline: bool,
    dri: Option<&str>,
    clear_dri: bool,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let thread_id = validate_thread_id(thread)?;

    let deadline_arg = if clear_deadline {
        Some(None)
    } else {
        deadline.map(Some)
    };
    let dri_arg = if clear_dri { Some(None) } else { dri.map(Some) };
    if goal.is_none() && deadline_arg.is_none() && dri_arg.is_none() {
        return Err(CliError::Usage(
            "set requires at least one of --goal, --deadline/--clear-deadline, --dri/--clear-dri"
                .into(),
        ));
    }

    let builder =
        buzz_sdk::build_thread_metadata(channel_id, &thread_id, goal, deadline_arg, dri_arg)
            .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

/// Promote a personal-channel thread into a team channel (kind 47021).
/// The response echoes the write result with the new `thread_id` (the
/// promotion event id) plus the graft commit and source-close status.
pub async fn cmd_promote_thread(
    client: &BuzzClient,
    from: &str,
    thread: &str,
    to: &str,
    summary: &str,
    commit: Option<&str>,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(from)?;
    crate::validate::validate_uuid(to)?;
    let source_channel =
        uuid::Uuid::parse_str(from).map_err(|_| CliError::Usage("--from must be a UUID".into()))?;
    let target_channel =
        uuid::Uuid::parse_str(to).map_err(|_| CliError::Usage("--to must be a UUID".into()))?;
    let source_root = validate_thread_id(thread)?;
    let builder = buzz_sdk::build_thread_promote(
        target_channel,
        source_channel,
        &source_root,
        summary,
        commit,
    )
    .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let thread_id = event.id.to_hex();
    let resp = client.submit_event(event).await?;
    let normalized = crate::client::normalize_write_response(&resp);
    match serde_json::from_str::<serde_json::Value>(&normalized) {
        Ok(mut v) if v.is_object() => {
            v["thread_id"] = serde_json::Value::String(thread_id);
            println!("{v}");
        }
        _ => println!("{normalized}"),
    }
    Ok(())
}

/// Ask the Privacy Gate what promoting a thread would expose (kind 47022).
///
/// Publishes the request, then polls for the relay's kind:47023 answer,
/// which is signed by the relay and scoped to the same personal channel.
/// The wait is a poll rather than a live subscription because the review
/// is a one-shot answer, not a stream — and because a member who does not
/// want to wait (`--wait 0`) still gets the request receipt and can read
/// the result later with `buzz threads show`.
pub async fn cmd_gate_review(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    summary: Option<&str>,
    wait_secs: u64,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("--channel must be a UUID".into()))?;
    let thread_root = validate_thread_id(thread)?;
    let builder =
        buzz_sdk::build_thread_gate_review(channel_id, &thread_root, summary.unwrap_or_default())
            .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let request_id = event.id.to_hex();
    let resp = client.submit_event(event).await?;
    let normalized = crate::client::normalize_write_response(&resp);

    if wait_secs == 0 {
        println!("{normalized}");
        return Ok(());
    }
    // The relay accepted the request; a rejected one never produces a
    // review, so don't sit in a poll loop waiting for an answer that
    // cannot come.
    let accepted = serde_json::from_str::<serde_json::Value>(&normalized)
        .ok()
        .and_then(|v| v.get("accepted").and_then(serde_json::Value::as_bool))
        .unwrap_or(false);
    if !accepted {
        println!("{normalized}");
        return Ok(());
    }

    let filter = serde_json::json!({
        "kinds": [buzz_core::kind::KIND_WORK_THREAD_GATE_REVIEWED],
        "#h": [channel_id.to_string()],
        "#e": [thread_root],
        "limit": 20,
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    loop {
        if let Ok(raw) = client.query(&filter).await {
            if let Ok(events) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) {
                // Match on the `req` tag: a thread can be reviewed more
                // than once, and an older review must never be mistaken
                // for this one's answer.
                let mine = events.iter().find(|e| {
                    e.get("tags")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|tags| {
                            tags.iter().any(|t| {
                                let parts = t.as_array();
                                parts.is_some_and(|p| {
                                    p.first().and_then(serde_json::Value::as_str) == Some("req")
                                        && p.get(1).and_then(serde_json::Value::as_str)
                                            == Some(request_id.as_str())
                                })
                            })
                        })
                });
                if let Some(review) = mine {
                    let content = review
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("{}");
                    match serde_json::from_str::<serde_json::Value>(content) {
                        Ok(parsed) => println!("{parsed}"),
                        Err(_) => println!("{content}"),
                    }
                    return Ok(());
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            println!(
                "{}",
                serde_json::json!({
                    "event_id": request_id,
                    "accepted": true,
                    "message": format!(
                        "review not ready within {wait_secs}s — read it later with \
                         'buzz threads show'"
                    ),
                })
            );
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    }
}

/// Transition a thread's D41 state (kind 47002).
pub async fn cmd_thread_state(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    to: &str,
    canonicalize: bool,
    archive_siblings: bool,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let thread_id = validate_thread_id(thread)?;
    let canonicalize = canonicalize.then_some(true);
    let archive_siblings = archive_siblings.then_some(true);
    let builder =
        buzz_sdk::build_thread_state(channel_id, &thread_id, to, canonicalize, archive_siblings)
            .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

/// Post an agent recommendation (kind 47003) — inert until a human confirms.
pub async fn cmd_recommend(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    note: &str,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let thread_id = validate_thread_id(thread)?;
    let builder =
        buzz_sdk::build_thread_recommend(channel_id, &thread_id, note).map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

/// Record a per-turn checkpoint (kind 47010).
pub async fn cmd_checkpoint(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    commit: &str,
    branch: Option<&str>,
    turn: Option<u32>,
    note: Option<&str>,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let thread_id = validate_thread_id(thread)?;
    let builder = buzz_sdk::build_thread_checkpoint(
        channel_id,
        &thread_id,
        commit,
        branch,
        turn,
        note.unwrap_or(""),
    )
    .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

pub async fn dispatch(cmd: crate::ThreadsCmd, client: &BuzzClient) -> Result<(), CliError> {
    use crate::ThreadsCmd;
    match cmd {
        ThreadsCmd::Open {
            channel,
            goal,
            deadline,
            dri,
        } => cmd_open_thread(client, &channel, &goal, deadline, dri.as_deref()).await,
        ThreadsCmd::List {
            channel,
            status,
            limit,
        } => cmd_list_threads(client, &channel, status.as_deref(), limit).await,
        ThreadsCmd::Show { channel, thread } => cmd_show_thread(client, &channel, &thread).await,
        ThreadsCmd::Set {
            channel,
            thread,
            goal,
            deadline,
            clear_deadline,
            dri,
            clear_dri,
        } => {
            cmd_set_thread(
                client,
                &channel,
                &thread,
                goal.as_deref(),
                deadline,
                clear_deadline,
                dri.as_deref(),
                clear_dri,
            )
            .await
        }
        ThreadsCmd::State {
            channel,
            thread,
            to,
            canonicalize,
            archive_siblings,
        } => {
            cmd_thread_state(
                client,
                &channel,
                &thread,
                &to,
                canonicalize,
                archive_siblings,
            )
            .await
        }
        ThreadsCmd::Promote {
            from,
            thread,
            to,
            summary,
            commit,
        } => cmd_promote_thread(client, &from, &thread, &to, &summary, commit.as_deref()).await,
        ThreadsCmd::GateReview {
            channel,
            thread,
            summary,
            wait,
        } => cmd_gate_review(client, &channel, &thread, summary.as_deref(), wait).await,
        ThreadsCmd::Fork {
            channel,
            thread,
            goal,
            commit,
            deadline,
            dri,
        } => {
            cmd_fork_thread(
                client,
                &channel,
                &thread,
                &goal,
                commit.as_deref(),
                deadline,
                dri.as_deref(),
            )
            .await
        }
        ThreadsCmd::Recommend {
            channel,
            thread,
            note,
        } => cmd_recommend(client, &channel, &thread, &note).await,
        ThreadsCmd::Checkpoint {
            channel,
            thread,
            commit,
            branch,
            turn,
            note,
        } => {
            cmd_checkpoint(
                client,
                &channel,
                &thread,
                &commit,
                branch.as_deref(),
                turn,
                note.as_deref(),
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        kind: u64,
        content: &str,
        tags: &[&[&str]],
        created_at: i64,
        id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "kind": kind,
            "content": content,
            "pubkey": "aa".repeat(32),
            "created_at": created_at,
            "tags": tags
                .iter()
                .map(|t| t.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn folding_reproduces_lifecycle() {
        let root_id = "11".repeat(32);
        let channel = "7f7f7f7f-1111-2222-3333-444444444444";
        let root = event(
            47000,
            "ship the fix",
            &[&["h", channel], &["deadline", "1900000000"]],
            100,
            &root_id,
        );
        let mut folded = FoldedThread::from_root(&root);
        assert_eq!(folded.status, "open");
        assert_eq!(folded.deadline, Some(1_900_000_000));

        let mut commands = vec![
            event(
                47002,
                "",
                &[&["e", &root_id], &["h", channel], &["state", "ready"]],
                102,
                "cc01",
            ),
            event(
                47001,
                r#"{"goal":"ship it, tested","deadline":null}"#,
                &[&["e", &root_id], &["h", channel]],
                101,
                "cc00",
            ),
            event(
                47002,
                "",
                &[
                    &["e", &root_id],
                    &["h", channel],
                    &["state", "closed"],
                    &["canonicalize", "true"],
                ],
                103,
                "cc02",
            ),
        ];
        commands.sort_by_key(fold_key);
        fold_commands(&mut folded, &commands);
        assert_eq!(folded.goal, "ship it, tested");
        assert_eq!(folded.deadline, None);
        assert_eq!(folded.status, "closed");
    }

    #[test]
    fn thread_id_shape_is_enforced() {
        assert!(validate_thread_id(&"a".repeat(64)).is_ok());
        assert!(validate_thread_id("short").is_err());
        assert!(validate_thread_id(&"g".repeat(64)).is_err());
    }

    #[test]
    fn fork_root_folds_with_provenance() {
        let parent_id = "11".repeat(32);
        let fork_id = "22".repeat(32);
        let channel = "7f7f7f7f-1111-2222-3333-444444444444";
        let sha1 = "c".repeat(40);
        let fork_root = event(
            47020,
            "try approach B",
            &[&["e", &parent_id], &["h", channel], &["commit", &sha1]],
            200,
            &fork_id,
        );
        let folded = FoldedThread::from_root(&fork_root);
        assert_eq!(folded.thread_id, fork_id);
        assert_eq!(folded.goal, "try approach B");
        assert_eq!(folded.status, "open");
        assert_eq!(folded.forked_from.as_deref(), Some(parent_id.as_str()));
        assert_eq!(folded.fork_commit.as_deref(), Some(sha1.as_str()));

        // A 47000 root never carries fork provenance, even with an e tag.
        let plain = event(
            47000,
            "original",
            &[&["h", channel], &["e", &parent_id]],
            100,
            &"33".repeat(32),
        );
        let folded_plain = FoldedThread::from_root(&plain);
        assert!(folded_plain.forked_from.is_none() && folded_plain.fork_commit.is_none());
    }

    #[test]
    fn promotion_root_and_notice_fold() {
        let source_id = "66".repeat(32);
        let new_id = "77".repeat(32);
        let personal = "7f7f7f7f-1111-2222-3333-444444444444";
        let team = "7f7f7f7f-5555-6666-7777-888888888888";

        // The 47021 root in the TARGET channel folds as an open thread with
        // promotion provenance; its goal is the member-written summary.
        let promo_root = event(
            47021,
            "Parser fix, tested",
            &[&["e", &source_id], &["h", team], &["from", personal]],
            300,
            &new_id,
        );
        let folded = FoldedThread::from_root(&promo_root);
        assert_eq!(folded.goal, "Parser fix, tested");
        assert_eq!(folded.status, "open");
        assert_eq!(folded.promoted_from.as_deref(), Some(source_id.as_str()));
        assert_eq!(folded.promoted_channel.as_deref(), Some(personal));
        assert!(folded.forked_from.is_none());

        // The relay-signed 47014 notice in the SOURCE channel folds the
        // promoted thread to closed.
        let source_root = event(47000, "private work", &[&["h", personal]], 100, &source_id);
        let mut folded = FoldedThread::from_root(&source_root);
        let commands = vec![event(
            47014,
            &format!(r#"{{"to":"{team}","thread":"{new_id}"}}"#),
            &[
                &["e", &source_id],
                &["h", personal],
                &["to", team],
                &["thread", &new_id],
            ],
            301,
            "dd00",
        )];
        fold_commands(&mut folded, &commands);
        assert_eq!(folded.status, "closed");
    }

    /// Regression from the Phase 2 exit-criterion dry run: a relay notice
    /// and the command it supersedes can share a wire second, and the id
    /// tie-break used to let the command win — leaving the fold showing
    /// `ready` for a thread the projection had already archived.
    #[test]
    fn same_second_relay_notice_beats_the_command_it_supersedes() {
        let root_id = "88".repeat(32);
        let winner_id = "99".repeat(32);
        let channel = "7f7f7f7f-1111-2222-3333-444444444444";
        let root = event(47000, "losing variation", &[&["h", channel]], 100, &root_id);
        let mut folded = FoldedThread::from_root(&root);

        // Same created_at; the notice's id sorts BEFORE the command's, which
        // is exactly the case that used to invert the outcome.
        let mut commands = vec![
            event(
                47002,
                "",
                &[&["e", &root_id], &["h", channel], &["state", "ready"]],
                200,
                "ffff",
            ),
            event(
                47013,
                &format!(r#"{{"winner":"{winner_id}"}}"#),
                &[&["e", &root_id], &["h", channel], &["winner", &winner_id]],
                200,
                "0000",
            ),
        ];
        commands.sort_by_key(fold_key);
        fold_commands(&mut folded, &commands);
        assert_eq!(
            folded.status, "archived",
            "the relay's archive notice must win a same-second tie"
        );
    }

    #[test]
    fn sibling_archive_notice_folds_to_archived() {
        let root_id = "44".repeat(32);
        let winner_id = "55".repeat(32);
        let channel = "7f7f7f7f-1111-2222-3333-444444444444";
        let root = event(47000, "losing variation", &[&["h", channel]], 100, &root_id);
        let mut folded = FoldedThread::from_root(&root);

        let mut commands = vec![
            event(
                47002,
                "",
                &[&["e", &root_id], &["h", channel], &["state", "ready"]],
                101,
                "cc00",
            ),
            event(
                47013,
                &format!(r#"{{"winner":"{winner_id}"}}"#),
                &[&["e", &root_id], &["h", channel], &["winner", &winner_id]],
                102,
                "cc01",
            ),
        ];
        commands.sort_by_key(fold_key);
        fold_commands(&mut folded, &commands);
        assert_eq!(folded.status, "archived");
    }
}
