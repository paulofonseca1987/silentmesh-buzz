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
    Ok(())
}

/// List seals from their kind:47100 announcements.
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

    let rows: Vec<serde_json::Value> = events
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
            match format {
                crate::OutputFormat::Compact => serde_json::json!({
                    "seal_id": id,
                    "label": content.get("label").cloned().unwrap_or_default(),
                    "min_tier": tag("tier"),
                }),
                crate::OutputFormat::Json => serde_json::json!({
                    "seal_id": id,
                    "token": buzz_core::seal::token(&id),
                    "label": content.get("label").cloned().unwrap_or_default(),
                    "min_tier": tag("tier"),
                    "created_by": tag("p"),
                    "created_at": event.get("created_at").cloned().unwrap_or_default(),
                }),
            }
        })
        .collect();
    println!("{}", serde_json::to_string(&rows).unwrap_or_default());
    Ok(())
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
    }
}
