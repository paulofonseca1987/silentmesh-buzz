//! Work threads — open, list, show, set, state, recommend (Silent Mesh
//! Phase 2, kinds 47000–47003).
//!
//! Writes are signed events on the generic submit path: 47000 roots and
//! 47003 recommendations store append-only; 47001/47002 are relay-validated
//! commands (D41 authority + TOCTOU-safe projection updates).
//!
//! Reads fold the signed events client-side — the events are the truth.
//! Every stored 47001/47002 was applied by the relay (rejected commands are
//! never stored), so folding them in `created_at` order reproduces the
//! projection; ties or skewed client clocks can differ transiently from the
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

/// Order key for folding: `(created_at, id)` — the relay stores commands in
/// validation order, so this reproduces it except under client-clock skew.
fn fold_key(event: &serde_json::Value) -> (i64, String) {
    (
        event
            .get("created_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
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
}

impl FoldedThread {
    fn from_root(root: &serde_json::Value) -> Self {
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
        })
    }
}

/// Fold 47001/47002 command events (already sorted) over the root view.
fn fold_commands(thread: &mut FoldedThread, commands: &[serde_json::Value]) {
    for event in commands {
        match event.get("kind").and_then(|v| v.as_u64()) {
            Some(47001) => thread.apply_metadata(event),
            Some(47002) => {
                if let Some(state) = tag_value(event, "state") {
                    thread.status = state.to_owned();
                }
            }
            _ => {}
        }
    }
}

/// Fetch all 47001/47002 commands for a set of thread roots, sorted for
/// folding.
async fn fetch_commands(
    client: &BuzzClient,
    channel: &str,
    roots: &[String],
) -> Result<Vec<serde_json::Value>, CliError> {
    if roots.is_empty() {
        return Ok(Vec::new());
    }
    let filter = serde_json::json!({
        "kinds": [47001, 47002],
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
        "kinds": [47000],
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
        "kinds": [47000],
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

    let mut view = folded.to_json();
    view["recommendations"] = serde_json::Value::Array(recommendations);
    view["checkpoints"] = serde_json::Value::Array(checkpoints);
    println!("{view}");
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

/// Transition a thread's D41 state (kind 47002).
pub async fn cmd_thread_state(
    client: &BuzzClient,
    channel: &str,
    thread: &str,
    to: &str,
    canonicalize: bool,
) -> Result<(), CliError> {
    crate::validate::validate_uuid(channel)?;
    let channel_id = uuid::Uuid::parse_str(channel)
        .map_err(|_| CliError::Usage("channel must be a UUID".into()))?;
    let thread_id = validate_thread_id(thread)?;
    let canonicalize = canonicalize.then_some(true);
    let builder =
        buzz_sdk::build_thread_state(channel_id, &thread_id, to, canonicalize).map_err(sdk_err)?;
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
        } => cmd_thread_state(client, &channel, &thread, &to, canonicalize).await,
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
}
