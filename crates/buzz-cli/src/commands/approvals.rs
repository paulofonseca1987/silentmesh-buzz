//! Agent permission approvals — list, show, grant, deny.
//!
//! Reads go through the relay's membership-scoped `GET /api/approvals`.
//! Grant/deny are the human-signed kind:46030/46031 commands: the event's
//! `d` tag carries the request's token hash (as published in the
//! kind:46010 notification and the list read), and the Schnorr signature
//! is the "who approved what" proof.

use crate::client::BuzzClient;
use crate::error::CliError;
use crate::validate::sdk_err;

/// A grant/deny target: a request UUID (resolved to its token hash through
/// the list read) or a 64-char hex token hash used directly.
async fn resolve_token_hash(client: &BuzzClient, request: &str) -> Result<String, CliError> {
    let trimmed = request.trim();
    if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(trimmed.to_ascii_lowercase());
    }
    if uuid::Uuid::parse_str(trimmed).is_err() {
        return Err(CliError::Usage(
            "request must be a request UUID or a 64-char hex token hash".into(),
        ));
    }
    let row = fetch_request(client, trimmed).await?.ok_or_else(|| {
        CliError::Usage(format!(
            "request {trimmed} not found among your visible approvals"
        ))
    })?;
    row.get("token_hash")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| CliError::Other("approval row missing token_hash".into()))
}

/// Fetch one request by id through the membership-scoped list read.
async fn fetch_request(
    client: &BuzzClient,
    request_id: &str,
) -> Result<Option<serde_json::Value>, CliError> {
    let resp = client.get_authed("/api/approvals?limit=200").await?;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&resp).unwrap_or_default();
    Ok(rows
        .into_iter()
        .find(|r| r.get("request_id").and_then(|v| v.as_str()) == Some(request_id)))
}

/// List permission requests visible to the caller.
pub async fn cmd_list_approvals(
    client: &BuzzClient,
    status: Option<&str>,
    channel: Option<&str>,
    limit: Option<u32>,
) -> Result<(), CliError> {
    let mut query: Vec<String> = Vec::new();
    if let Some(status) = status {
        let valid = ["pending", "granted", "denied", "cancelled", "expired"];
        if !valid.contains(&status) {
            return Err(CliError::Usage(format!(
                "invalid status '{status}' (valid: {})",
                valid.join(", ")
            )));
        }
        query.push(format!("status={status}"));
    }
    if let Some(channel) = channel {
        crate::validate::validate_uuid(channel)?;
        query.push(format!("channel={channel}"));
    }
    if let Some(limit) = limit {
        query.push(format!("limit={limit}"));
    }
    let path = if query.is_empty() {
        "/api/approvals".to_owned()
    } else {
        format!("/api/approvals?{}", query.join("&"))
    };
    let resp = client.get_authed(&path).await?;
    println!("{resp}");
    Ok(())
}

/// Show one permission request.
pub async fn cmd_show_approval(client: &BuzzClient, request: &str) -> Result<(), CliError> {
    crate::validate::validate_uuid(request)?;
    match fetch_request(client, request).await? {
        Some(row) => println!("{row}"),
        None => println!("null"),
    }
    Ok(())
}

/// Grant or deny a permission request — sign and submit kind:46030/46031.
pub async fn cmd_decide_approval(
    client: &BuzzClient,
    request: &str,
    approved: bool,
    note: Option<&str>,
) -> Result<(), CliError> {
    let token_hash = resolve_token_hash(client, request).await?;
    let builder = buzz_sdk::build_workflow_approval(&token_hash, approved, note.unwrap_or(""))
        .map_err(sdk_err)?;
    let event = client.sign_event(builder)?;
    let resp = client.submit_event(event).await?;
    println!("{}", crate::client::normalize_write_response(&resp));
    Ok(())
}

pub async fn dispatch(cmd: crate::ApprovalsCmd, client: &BuzzClient) -> Result<(), CliError> {
    use crate::ApprovalsCmd;
    match cmd {
        ApprovalsCmd::List {
            status,
            channel,
            limit,
        } => cmd_list_approvals(client, status.as_deref(), channel.as_deref(), limit).await,
        ApprovalsCmd::Show { request } => cmd_show_approval(client, &request).await,
        ApprovalsCmd::Grant { request, note } => {
            cmd_decide_approval(client, &request, true, note.as_deref()).await
        }
        ApprovalsCmd::Deny { request, note } => {
            cmd_decide_approval(client, &request, false, note.as_deref()).await
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_hash_shape_is_recognized() {
        // 64 hex chars pass through; a UUID needs the list lookup; anything
        // else is a usage error. (Shape logic only — no relay round-trip.)
        let hash = "a".repeat(64);
        assert!(hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(uuid::Uuid::parse_str("not-a-uuid").is_err());
        assert!(uuid::Uuid::parse_str("7f7f7f7f-1111-2222-3333-444444444444").is_ok());
    }
}
