//! Action sink trait — interface for workflow side-effects.
//!
//! The relay implements [`ActionSink`] to provide direct DB access to the
//! executor, replacing the HTTP loopback pattern.

use std::future::Future;
use std::pin::Pin;

use buzz_core::tenant::CommunityId;
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Errors from action sink operations.
#[derive(Debug, thiserror::Error)]
pub enum ActionSinkError {
    /// An input parameter is malformed (e.g. invalid UUID).
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The target channel does not exist.
    #[error("channel not found: {0}")]
    ChannelNotFound(String),
    /// The target channel is archived.
    #[error("channel is archived: {0}")]
    ChannelArchived(String),
    /// Nostr event construction or signing failed.
    #[error("event construction failed: {0}")]
    EventBuild(String),
    /// A database operation failed.
    #[error("database error: {0}")]
    Database(String),
    /// Message content is empty or whitespace-only.
    #[error("empty message content")]
    EmptyContent,
}

impl From<ActionSinkError> for crate::WorkflowError {
    fn from(e: ActionSinkError) -> Self {
        crate::WorkflowError::WebhookError(e.to_string())
    }
}

/// Payload for the kind:46010 approval-requested notification emitted when a
/// workflow run suspends at a `request_approval` step (WF-08).
#[derive(Debug, Clone)]
pub struct ApprovalRequestNotice {
    /// Workflow that owns the suspended run.
    pub workflow_id: Uuid,
    /// The suspended run.
    pub run_id: Uuid,
    /// ID of the `request_approval` step that suspended.
    pub step_id: String,
    /// Zero-based index of the suspended step.
    pub step_index: i32,
    /// Who may approve — the step's `from` field.
    pub approver_spec: String,
    /// Message shown to the approver.
    pub message: String,
    /// Raw approval token. Delivery of the event is membership-scoped, and
    /// grant/deny remain authorization-checked server-side (`approver_spec` +
    /// NIP-42 identity), so the token is a correlation handle, not a bearer
    /// credential — but it is what `buzz workflows approve <token>` needs.
    pub token: String,
    /// When the approval request expires.
    pub expires_at: DateTime<Utc>,
}

/// Interface for workflow actions that produce side effects.
///
/// Implemented by the relay to provide direct DB/event access to the executor.
/// This replaces the HTTP loopback where the executor POSTed to the relay's
/// REST API (which failed with 401 auth errors).
///
/// Returns `Pin<Box<dyn Future>>` for dyn-compatibility — required because
/// `WorkflowEngine` stores `Arc<dyn ActionSink>`.
pub trait ActionSink: Send + Sync {
    /// Post a message to a channel on behalf of a workflow owner.
    ///
    /// - `community_id`: the server-resolved community that owns the workflow
    ///   run driving this side effect. The relay-signed message is published
    ///   under *this* community, never the deployment/default tenant — the run
    ///   carries its owning community so a workflow in community B posts into B
    ///   even though the side effect has no inbound connection to bind.
    /// - `channel_id`: UUID string of the target channel
    /// - `text`: message body (must not be empty/whitespace-only)
    /// - `author_pubkey`: hex-encoded pubkey of the workflow owner (used for
    ///   the `p` attribution tag; the relay keypair signs the event)
    ///
    /// Returns the event ID hex string on success.
    fn send_message(
        &self,
        community_id: CommunityId,
        channel_id: &str,
        text: &str,
        author_pubkey: &str,
    ) -> Pin<Box<dyn Future<Output = Result<String, ActionSinkError>> + Send + '_>>;

    /// Emit a relay-signed kind:46010 approval-requested event into the
    /// channel so members (and the designated approver's "Needs Action" feed,
    /// via the `p` tag) learn a run is waiting on a human (WF-08).
    ///
    /// Tags: `d` = hex(SHA-256(token)) — the handle kind:46030/46031
    /// grant/deny commands reference; `h` = channel; `p` = approver pubkey
    /// when `approver_spec` is a concrete pubkey.
    ///
    /// Returns the event ID hex string on success.
    fn emit_approval_requested(
        &self,
        community_id: CommunityId,
        channel_id: &str,
        notice: ApprovalRequestNotice,
    ) -> Pin<Box<dyn Future<Output = Result<String, ActionSinkError>> + Send + '_>>;
}
