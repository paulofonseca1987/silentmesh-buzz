//! Content Seals (Silent Mesh D31) — create and list.
//!
//! `create` is the one write, and it is the workspace Owner's alone: the
//! literal travels over NIP-98-authenticated HTTP into the relay's own
//! store, because it must never be an event — events fan out to every
//! member of every tier, which is exactly what a seal exists to prevent.
//!
//! `list` reads the relay-signed kind:47100 announcements, which carry the
//! id, label and minimum tier and **never the literal**. That asymmetry is
//! the design: any member can learn *that* something is sealed and what to
//! call it; only the relay can match text against the value.

use crate::client::BuzzClient;
use crate::error::CliError;

/// Create a seal. Owner-only; the relay refuses anyone else.
pub async fn cmd_create_seal(
    client: &BuzzClient,
    label: &str,
    literal: &str,
    min_tier: &str,
) -> Result<(), CliError> {
    let valid_tiers = ["owned", "private"];
    if !valid_tiers.contains(&min_tier) {
        // `open` is refused relay-side too, but saying it here saves a
        // round trip: a seal at the loosest tier would seal nothing.
        return Err(CliError::Usage(format!(
            "invalid min_tier {min_tier:?} — must be one of: {}",
            valid_tiers.join(", ")
        )));
    }
    let body = serde_json::json!({
        "label": label,
        "literal": literal,
        "min_tier": min_tier,
    });
    let resp = client.post_authed("/api/seals", &body).await?;
    println!("{resp}");

    // The sweep says where the value already is. It rides in the JSON above
    // for machine callers; a human sealing something needs the exposure
    // count in front of them, and stderr keeps stdout's contract intact.
    //
    // Silence here means "the relay could not sweep", not "nothing found" —
    // the two are distinguishable in the JSON (`sweep: null` vs
    // `occurrences: 0`), and neither is worth shouting about.
    if let Some(sweep) = serde_json::from_str::<serde_json::Value>(&resp)
        .ok()
        .and_then(|v| v.get("sweep").cloned())
        .filter(|s| !s.is_null())
    {
        let n = |key: &str| sweep.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let exposed = n("exposed");
        if exposed > 0 {
            let where_ = sweep
                .get("channels")
                .and_then(|c| c.as_array())
                .map(|chans| {
                    chans
                        .iter()
                        .filter(|c| c.get("exposed").and_then(|e| e.as_bool()) == Some(true))
                        .filter_map(|c| {
                            let name = c.get("name")?.as_str()?;
                            let tier = c.get("tier")?.as_str().unwrap_or("?");
                            Some(format!("{name} ({tier})"))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            eprintln!(
                "warning: this value already appears in {exposed} stored event(s) in channels \
                 looser than '{min_tier}': {where_}"
            );
            eprintln!(
                "         the seal binds what happens from now on; it does not rewrite history."
            );
            if sweep.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
                eprintln!("         (scan stopped at its limit — there may be more)");
            }
        }
    }
    Ok(())
}

/// Revoke a seal. Owner-only; the relay refuses anyone else.
pub async fn cmd_revoke_seal(client: &BuzzClient, seal_id: &str) -> Result<(), CliError> {
    let id = seal_id.trim().to_ascii_lowercase();
    if !buzz_core::seal::is_valid_id(&id) {
        return Err(CliError::Usage(format!(
            "invalid seal id {seal_id:?} — 16 lowercase hex chars (see `seals list`)"
        )));
    }
    let body = serde_json::json!({ "seal_id": id });
    let resp = client.post_authed("/api/seals/revoke", &body).await?;
    println!("{resp}");
    Ok(())
}

/// List seals from their kind:47100 announcements.
///
/// One seal can have several announcements — creation, then revocation
/// (47100 is not a replaceable kind) — so rows are deduped by seal id
/// keeping the **newest**, or a revoked seal would show twice and read as
/// both live and dead.
pub async fn cmd_list_seals(
    client: &BuzzClient,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    let filter = serde_json::json!({
        "kinds": [buzz_core::kind::KIND_SEAL_ANNOUNCE],
        "limit": 200,
    });
    let resp = client.query(&filter).await?;
    let events: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    let deduped = newest_announcement_per_seal(&events);

    let rows: Vec<serde_json::Value> = deduped
        .iter()
        .map(|event| {
            let tag = |name: &str| -> Option<String> {
                event
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .and_then(|tags| {
                        tags.iter().find_map(|t| {
                            let t = t.as_array()?;
                            (t.first()?.as_str()? == name)
                                .then(|| t.get(1)?.as_str().map(str::to_owned))?
                        })
                    })
            };
            let content: serde_json::Value = event
                .get("content")
                .and_then(|c| c.as_str())
                .and_then(|c| serde_json::from_str(c).ok())
                .unwrap_or(serde_json::Value::Null);
            let id = tag("d").unwrap_or_default();
            // Absent on pre-revocation announcements — read as live.
            let revoked = content
                .get("revoked")
                .and_then(|r| r.as_bool())
                .unwrap_or(false);
            match format {
                crate::OutputFormat::Compact => serde_json::json!({
                    "seal_id": id,
                    "label": content.get("label").cloned().unwrap_or_default(),
                    "min_tier": tag("tier"),
                    "revoked": revoked,
                }),
                crate::OutputFormat::Json => serde_json::json!({
                    "seal_id": id,
                    "token": buzz_core::seal::token(&id),
                    "label": content.get("label").cloned().unwrap_or_default(),
                    "min_tier": tag("tier"),
                    "revoked": revoked,
                    "created_by": tag("p"),
                    "created_at": event.get("created_at").cloned().unwrap_or_default(),
                }),
            }
        })
        .collect();
    println!("{}", serde_json::to_string(&rows).unwrap_or_default());
    Ok(())
}

/// One row per seal id, keeping the newest announcement by `created_at`.
///
/// kind:47100 is not replaceable, so one seal accumulates announcements —
/// creation, then revocation — and without this a revoked seal lists twice,
/// reading as both live and dead. Newest wins because the announcements are
/// a state history and the last state is the current one; output is newest
/// first, ties broken by seal id so the order is deterministic.
fn newest_announcement_per_seal(events: &[serde_json::Value]) -> Vec<&serde_json::Value> {
    let seal_id = |e: &serde_json::Value| -> String {
        e.get("tags")
            .and_then(|t| t.as_array())
            .and_then(|tags| {
                tags.iter().find_map(|t| {
                    let t = t.as_array()?;
                    (t.first()?.as_str()? == "d").then(|| t.get(1)?.as_str().map(str::to_owned))?
                })
            })
            .unwrap_or_default()
    };
    let at = |e: &serde_json::Value| e.get("created_at").and_then(|c| c.as_i64()).unwrap_or(0);

    let mut newest: std::collections::HashMap<String, &serde_json::Value> =
        std::collections::HashMap::new();
    for event in events {
        match newest.entry(seal_id(event)) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(event);
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                if at(event) > at(o.get()) {
                    o.insert(event);
                }
            }
        }
    }
    let mut rows: Vec<&serde_json::Value> = newest.into_values().collect();
    rows.sort_by(|a, b| at(b).cmp(&at(a)).then_with(|| seal_id(a).cmp(&seal_id(b))));
    rows
}

pub async fn dispatch(
    cmd: crate::SealsCmd,
    client: &BuzzClient,
    format: &crate::OutputFormat,
) -> Result<(), CliError> {
    use crate::SealsCmd;
    match cmd {
        SealsCmd::Create {
            label,
            literal,
            min_tier,
        } => cmd_create_seal(client, &label, &literal, &min_tier).await,
        SealsCmd::List => cmd_list_seals(client, format).await,
        SealsCmd::Revoke { seal_id } => cmd_revoke_seal(client, &seal_id).await,
    }
}

#[cfg(test)]
mod tests {
    use super::newest_announcement_per_seal;
    use serde_json::json;

    fn announce(id: &str, created_at: i64, revoked: bool) -> serde_json::Value {
        json!({
            "created_at": created_at,
            "tags": [["d", id], ["tier", "private"]],
            "content": json!({"label": "l", "min_tier": "private", "revoked": revoked}).to_string(),
        })
    }

    #[test]
    fn a_revoked_seal_lists_once_as_its_newest_state() {
        let events = vec![
            announce("aaaaaaaaaaaaaaaa", 100, false),
            announce("aaaaaaaaaaaaaaaa", 200, true), // revocation, newer
            announce("bbbbbbbbbbbbbbbb", 150, false),
        ];
        let rows = newest_announcement_per_seal(&events);
        assert_eq!(rows.len(), 2, "one row per seal, not per announcement");
        // Newest first: the revocation (200), then b's creation (150).
        assert_eq!(rows[0]["created_at"], 200);
        assert!(rows[0]["content"]
            .as_str()
            .unwrap()
            .contains("\"revoked\":true"));
        assert_eq!(rows[1]["created_at"], 150);
    }

    #[test]
    fn announcement_order_does_not_change_the_outcome() {
        // The relay returns newest-first by default; the dedup must not
        // depend on that. Same events, reversed feed, same winner.
        let forward = vec![
            announce("aaaaaaaaaaaaaaaa", 100, false),
            announce("aaaaaaaaaaaaaaaa", 200, true),
        ];
        let reversed: Vec<_> = forward.iter().cloned().rev().collect();
        let f = newest_announcement_per_seal(&forward);
        let r = newest_announcement_per_seal(&reversed);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0]["created_at"], 200);
        assert_eq!(r[0]["created_at"], 200);
    }
}
