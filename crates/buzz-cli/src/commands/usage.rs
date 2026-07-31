//! Member-facing model-usage read (Silent Mesh Phase 3, model plane).
//!
//! Until now the only way to see what a member's agents had spent was
//! `buzz-admin usage`, which reads the relay's `model_usage` table and so
//! needs `DATABASE_URL` — an operator surface. A member could not answer
//! "what has my agent been running, and did any of it leave the box?" about
//! their own turns.
//!
//! This reads the same facts from the events themselves. Kind 44201 is the
//! cleartext attribution sibling of the encrypted 44200: one event per
//! completed turn, tagged with exactly one `p` (the owner). Those kinds are
//! result-gated, and `{kinds:[44201], "#p":[self]}` is the owner's canonical
//! subscription — so no new endpoint, no new kind, and no new authorization
//! path. A member reads their own and nobody else's because the relay
//! already enforces that.
//!
//! **What this cannot show, and why.** The relay never trusts a client's
//! tier/backend classification: on ingest it resolves tier from its own
//! channels table and derives backend itself. Those land in `model_usage`,
//! not in the event. So `tier` is deliberately absent here — reporting a
//! locally-guessed tier would invite exactly the confusion the gate exists
//! to prevent. `buzz-admin usage` remains the authoritative view.
//!
//! **This is a view of events, not of the ledger.** `model_usage` is the
//! relay's durable record, written once on ingest; the kind:44201 events are
//! the transport that carries a turn to it. The two are not guaranteed to
//! stay in step — an event can be deleted or otherwise stop being served
//! while its `model_usage` row persists, and this read would then show less
//! than the operator's. That is not hypothetical: on the Silent Mesh testbed
//! a `purpose="gate"` row exists whose source event is no longer in the
//! events table, so this read returns three turns where `buzz-admin usage`
//! reports four. Every surviving turn is reported exactly — the totals here
//! match the ledger's `agent_turn` rows to the token — but "exactly what the
//! relay still serves you" is the honest claim, not "everything that ever
//! happened".
//!
//! Backend *is* shown, because it is derivable from the model string by the
//! very function the relay applies — [`classify_model`], the shared rule the
//! tier gate and attribution ingest both use. Calling it here means this
//! column agrees with the relay by construction rather than by coincidence,
//! including its traps: an Ollama tag like `llama3.2:3b` must not classify by
//! its tag, and an unrecognized or absent prefix fails closed to `vendor`.

use std::collections::BTreeMap;

use buzz_core::model_route::classify_model;

use crate::client::BuzzClient;
use crate::error::CliError;

/// How to bucket turns in the aggregate read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Model,
    Channel,
    Agent,
}

impl GroupBy {
    fn parse(s: &str) -> Result<Self, CliError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "model" => Ok(GroupBy::Model),
            "channel" => Ok(GroupBy::Channel),
            "agent" => Ok(GroupBy::Agent),
            other => Err(CliError::Usage(format!(
                "invalid --by {other:?} — must be one of: model, channel, agent"
            ))),
        }
    }

    fn field(&self) -> &'static str {
        match self {
            GroupBy::Model => "model",
            GroupBy::Channel => "channel_id",
            GroupBy::Agent => "agent",
        }
    }
}

/// The facts one kind:44201 event carries, once read defensively.
///
/// `model` is `None` when the event's content is not the JSON this kind is
/// specified to carry. That is not dropped: an unreadable turn still
/// *happened*, and silently omitting it would under-report the total while
/// looking complete.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Turn {
    model: Option<String>,
    channel_id: Option<String>,
    agent: Option<String>,
    /// `agent_turn`, `gate`, … — not every metered call is a chat reply, and
    /// a member looking at a token bill needs to know which is which.
    purpose: Option<String>,
    prompt_tokens: u64,
    completion_tokens: u64,
}

/// One aggregated bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub key: String,
    pub backend: &'static str,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl UsageRow {
    fn to_json(&self, field: &str) -> serde_json::Value {
        serde_json::json!({
            field: self.key,
            "backend": self.backend,
            "requests": self.requests,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
        })
    }
}

/// Token counts are unsigned in the wire format; anything else (negative, a
/// string, a float, absent) reads as zero rather than aborting the whole
/// report for one malformed row.
fn u64_field(content: &serde_json::Value, key: &str) -> u64 {
    content
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn str_field(content: &serde_json::Value, key: &str) -> Option<String> {
    content
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Read the `agent` tag, falling back to the event's own pubkey.
///
/// The kind specifies `agent == event pubkey`, so the fallback should never
/// be needed — but reading the tag first keeps this honest about what the
/// event actually claims rather than assuming the invariant holds.
fn agent_of(event: &serde_json::Value) -> Option<String> {
    let tagged = event
        .get("tags")
        .and_then(serde_json::Value::as_array)
        .and_then(|tags| {
            tags.iter().find_map(|t| {
                let t = t.as_array()?;
                (t.first()?.as_str()? == "agent").then(|| t.get(1)?.as_str())?
            })
        })
        .map(str::to_owned);
    tagged.or_else(|| {
        event
            .get("pubkey")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn parse_turn(event: &serde_json::Value) -> Turn {
    let content = event
        .get("content")
        .and_then(serde_json::Value::as_str)
        .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        .unwrap_or(serde_json::Value::Null);

    Turn {
        model: str_field(&content, "model"),
        channel_id: str_field(&content, "channelId"),
        agent: agent_of(event),
        purpose: str_field(&content, "purpose"),
        prompt_tokens: u64_field(&content, "promptTokens"),
        completion_tokens: u64_field(&content, "completionTokens"),
    }
}

/// Bucket turns by `by` **and backend**, summing tokens and counting requests.
///
/// Backend is part of the key, not a property of the bucket: one channel can
/// serve both a local and a vendor turn, and collapsing those into a single
/// row would hide the one fact this product exists to show. Grouping by model
/// happens to make backend functionally dependent on the key; grouping by
/// channel or agent does not.
///
/// A turn whose model is unreadable is counted under `"unknown"` and
/// classified `vendor` — the same fail-closed answer `classify_model` gives
/// for an absent provider. It is counted rather than dropped so the request
/// total stays true.
pub fn aggregate(events: &[serde_json::Value], by: GroupBy) -> Vec<UsageRow> {
    let mut buckets: BTreeMap<(String, &'static str), UsageRow> = BTreeMap::new();

    for event in events {
        let turn = parse_turn(event);
        let backend = turn
            .model
            .as_deref()
            .map_or(classify_model(""), classify_model)
            .as_str();
        let key = match by {
            GroupBy::Model => turn.model.clone(),
            GroupBy::Channel => turn.channel_id.clone(),
            GroupBy::Agent => turn.agent.clone(),
        }
        .unwrap_or_else(|| "unknown".to_owned());

        let row = buckets
            .entry((key.clone(), backend))
            .or_insert_with(|| UsageRow {
                key,
                backend,
                requests: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
            });
        row.requests += 1;
        row.prompt_tokens += turn.prompt_tokens;
        row.completion_tokens += turn.completion_tokens;
    }

    let mut rows: Vec<UsageRow> = buckets.into_values().collect();
    // Heaviest first — the reason to read this is usually "what is costing
    // me", and BTreeMap order is alphabetical, which buries it.
    rows.sort_by(|a, b| {
        (b.prompt_tokens + b.completion_tokens)
            .cmp(&(a.prompt_tokens + a.completion_tokens))
            .then_with(|| a.key.cmp(&b.key))
            .then_with(|| a.backend.cmp(b.backend))
    });
    rows
}

/// Build the owner-scoped filter for this member's own turn attributions.
fn attribution_filter(my_pubkey: &str, since: Option<i64>) -> serde_json::Value {
    let mut filter = serde_json::json!({
        "kinds": [buzz_core::kind::KIND_AGENT_TURN_ATTRIBUTION],
        "#p": [my_pubkey],
    });
    if let Some(since) = since {
        filter["since"] = serde_json::json!(since);
    }
    filter
}

/// Convert `--since-hours` into a Unix timestamp, refusing values that would
/// silently produce a nonsense window.
fn since_from_hours(since_hours: Option<u32>) -> Result<Option<i64>, CliError> {
    let Some(hours) = since_hours else {
        return Ok(None);
    };
    if hours == 0 {
        return Err(CliError::Usage(
            "--since-hours must be at least 1 (omit it for all time)".into(),
        ));
    }
    let seconds = i64::from(hours) * 3600;
    Ok(Some(chrono::Utc::now().timestamp() - seconds))
}

/// Aggregate this member's own model usage.
pub async fn cmd_show_usage(
    client: &BuzzClient,
    since_hours: Option<u32>,
    by: Option<&str>,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    let by = by.map_or(Ok(GroupBy::Model), GroupBy::parse)?;
    let since = since_from_hours(since_hours)?;
    let my_pk = client.keys().public_key().to_hex();

    let events = client.query_all(attribution_filter(&my_pk, since)).await?;
    let rows = aggregate(&events, by);

    let out: Vec<serde_json::Value> = match format {
        crate::OutputFormat::Compact => rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    by.field(): r.key,
                    "backend": r.backend,
                    "requests": r.requests,
                    "total_tokens": r.prompt_tokens + r.completion_tokens,
                })
            })
            .collect(),
        crate::OutputFormat::Json => rows.iter().map(|r| r.to_json(by.field())).collect(),
    };
    println!("{}", serde_json::to_string(&out).unwrap_or_default());
    Ok(())
}

/// List this member's own turns, newest first, without aggregating.
pub async fn cmd_list_turns(
    client: &BuzzClient,
    since_hours: Option<u32>,
    limit: Option<u32>,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    let since = since_from_hours(since_hours)?;
    let my_pk = client.keys().public_key().to_hex();

    let filter = attribution_filter(&my_pk, since);
    let mut events = match limit {
        Some(limit) => client.query_paginated(filter, limit).await?,
        None => client.query_all(filter).await?,
    };
    events.sort_by_key(|e| {
        std::cmp::Reverse(
            e.get("created_at")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
    });

    let out: Vec<serde_json::Value> = events
        .iter()
        .map(|event| {
            let turn = parse_turn(event);
            let backend = turn
                .model
                .as_deref()
                .map_or(classify_model(""), classify_model)
                .as_str();
            let created_at = event
                .get("created_at")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match format {
                crate::OutputFormat::Compact => serde_json::json!({
                    "created_at": created_at,
                    "model": turn.model,
                    "backend": backend,
                    "purpose": turn.purpose,
                    "total_tokens": turn.prompt_tokens + turn.completion_tokens,
                }),
                crate::OutputFormat::Json => serde_json::json!({
                    "created_at": created_at,
                    "model": turn.model,
                    "backend": backend,
                    "purpose": turn.purpose,
                    "prompt_tokens": turn.prompt_tokens,
                    "completion_tokens": turn.completion_tokens,
                    "total_tokens": turn.prompt_tokens + turn.completion_tokens,
                    "channel_id": turn.channel_id,
                    "agent": turn.agent,
                }),
            }
        })
        .collect();
    println!("{}", serde_json::to_string(&out).unwrap_or_default());
    Ok(())
}

pub async fn dispatch(
    cmd: crate::UsageCmd,
    client: &BuzzClient,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    use crate::UsageCmd;
    match cmd {
        UsageCmd::Show { since_hours, by } => {
            cmd_show_usage(client, since_hours, by.as_deref(), format).await
        }
        UsageCmd::Turns { since_hours, limit } => {
            cmd_list_turns(client, since_hours, limit, format).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        model: Option<&str>,
        prompt: u64,
        completion: u64,
        channel: &str,
    ) -> serde_json::Value {
        let content = match model {
            Some(model) => serde_json::json!({
                "model": model,
                "promptTokens": prompt,
                "completionTokens": completion,
                "purpose": "agent_turn",
                "channelId": channel,
            })
            .to_string(),
            None => "not json at all".to_owned(),
        };
        serde_json::json!({
            "pubkey": "a".repeat(64),
            "created_at": 1_700_000_000u64,
            "kind": 44201,
            "tags": [["p", "b".repeat(64)], ["agent", "a".repeat(64)]],
            "content": content,
        })
    }

    #[test]
    fn totals_sum_per_model() {
        let rows = aggregate(
            &[
                event(Some("ollama:qwen3:14b"), 2050, 395, "chan-a"),
                event(Some("ollama:qwen3:14b"), 100, 5, "chan-a"),
            ],
            GroupBy::Model,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "ollama:qwen3:14b");
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].prompt_tokens, 2150);
        assert_eq!(rows[0].completion_tokens, 400);
    }

    /// The whole point of the column: a local turn and a vendor turn must not
    /// read the same, and the classification must be the relay's.
    #[test]
    fn backend_comes_from_the_shared_classifier() {
        let rows = aggregate(
            &[
                event(Some("ollama:qwen3:14b"), 10, 1, "chan-a"),
                event(Some("anthropic:claude-x"), 20, 2, "chan-a"),
            ],
            GroupBy::Model,
        );
        let local = rows.iter().find(|r| r.key == "ollama:qwen3:14b").unwrap();
        let vendor = rows.iter().find(|r| r.key == "anthropic:claude-x").unwrap();
        assert_eq!(local.backend, "local");
        assert_eq!(vendor.backend, "vendor");
    }

    /// A bare Ollama tag is not a provider. Classifying `llama3.2:3b` by its
    /// `:3b` tag would invent a provider out of a version number; the shared
    /// classifier fails closed instead, and this must inherit that.
    #[test]
    fn a_model_tag_is_not_a_provider() {
        let rows = aggregate(
            &[event(Some("llama3.2:3b"), 10, 1, "chan-a")],
            GroupBy::Model,
        );
        assert_eq!(rows[0].backend, "vendor");
    }

    /// An unreadable turn still happened. Dropping it would under-report the
    /// request count while the output still looked complete.
    #[test]
    fn an_unreadable_turn_is_counted_not_dropped() {
        let rows = aggregate(
            &[
                event(Some("ollama:qwen3:14b"), 10, 1, "chan-a"),
                event(None, 0, 0, "chan-a"),
            ],
            GroupBy::Model,
        );
        let total: u64 = rows.iter().map(|r| r.requests).sum();
        assert_eq!(total, 2, "the malformed turn must still be a request");
        let unknown = rows.iter().find(|r| r.key == "unknown").unwrap();
        assert_eq!(
            unknown.backend, "vendor",
            "an unclassifiable turn fails closed, like an absent provider"
        );
    }

    /// Backend is part of the key. One channel serving both a local and a
    /// vendor turn must produce two rows — collapsing them would hide the
    /// single fact a privacy-tiered product exists to surface.
    #[test]
    fn one_channel_with_two_backends_stays_two_rows() {
        let rows = aggregate(
            &[
                event(Some("ollama:qwen3:14b"), 10, 1, "chan-a"),
                event(Some("anthropic:claude-x"), 20, 2, "chan-a"),
            ],
            GroupBy::Channel,
        );
        assert_eq!(rows.len(), 2, "one row per (channel, backend)");
        assert!(rows.iter().all(|r| r.key == "chan-a"));
        let backends: Vec<&str> = rows.iter().map(|r| r.backend).collect();
        assert!(backends.contains(&"local") && backends.contains(&"vendor"));
    }

    #[test]
    fn heaviest_bucket_sorts_first() {
        let rows = aggregate(
            &[
                event(Some("ollama:small"), 1, 1, "chan-a"),
                event(Some("ollama:big"), 5000, 5000, "chan-a"),
            ],
            GroupBy::Model,
        );
        assert_eq!(rows[0].key, "ollama:big");
    }

    #[test]
    fn filter_is_the_owner_scoped_subscription() {
        let pk = "c".repeat(64);
        let filter = attribution_filter(&pk, Some(1_700_000_000));
        assert_eq!(filter["kinds"], serde_json::json!([44201]));
        assert_eq!(filter["#p"], serde_json::json!([pk]));
        assert_eq!(filter["since"], serde_json::json!(1_700_000_000));
    }

    /// Omitting `since` must mean "all time", not "since the epoch of a
    /// zero-hour window" — a `--since-hours 0` that quietly returned nothing
    /// would read as "you have no usage".
    #[test]
    fn a_zero_hour_window_is_refused_not_silently_empty() {
        assert!(since_from_hours(Some(0)).is_err());
        assert_eq!(since_from_hours(None).unwrap(), None);
        assert!(since_from_hours(Some(1)).unwrap().is_some());
    }

    /// Not every metered call is a chat reply. A `gate` review burns tokens
    /// under the same owner, and a member reading a bill needs to tell them
    /// apart — this is exactly the row that made the CLI's total differ from
    /// the operator's on the testbed.
    #[test]
    fn purpose_survives_parsing() {
        let ev = event(Some("ollama:qwen3:14b"), 309, 1082, "chan-a");
        assert_eq!(parse_turn(&ev).purpose.as_deref(), Some("agent_turn"));

        let mut gate = ev.clone();
        gate["content"] = serde_json::json!(serde_json::json!({
            "model": "ollama:qwen3:14b",
            "promptTokens": 309,
            "completionTokens": 1082,
            "purpose": "gate",
            "channelId": "chan-a",
        })
        .to_string());
        assert_eq!(parse_turn(&gate).purpose.as_deref(), Some("gate"));
    }

    #[test]
    fn group_by_rejects_nonsense() {
        assert!(GroupBy::parse("model").is_ok());
        assert!(GroupBy::parse("CHANNEL").is_ok());
        assert!(GroupBy::parse("tier").is_err());
    }
}
