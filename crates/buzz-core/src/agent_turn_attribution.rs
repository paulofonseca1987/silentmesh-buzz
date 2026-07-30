//! Agent Turn Attribution — cleartext payload for `kind:44201`.
//!
//! The metering sibling of the NIP-44-encrypted `kind:44200` turn metric:
//! one event per completed turn with reliable token counts, published by
//! the harness and signed by the agent. The relay ingests it into the
//! `model_usage` table — resolving the channel's tier from its own store
//! and deriving the backend from the model string via
//! [`crate::model_route::classify_model`]; the client's classification is
//! never trusted. Reads are owner-gated exactly like the 44200.
//!
//! Trust note: the `user_pubkey` attribution (which member's message
//! triggered the turn) is agent-claimed — the same trust class as every
//! other statement the owner-governed agent signs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model_route::InferencePurpose;

/// Cleartext content of a `kind:44201` Agent Turn Attribution event.
///
/// Consumers MUST ignore unknown fields (forward compatibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTurnAttributionPayload {
    /// The model the turn ran under, in the gate-classified form: the
    /// declared persona string (`"ollama:qwen3:14b"`) when one was set, else
    /// the harness-reported effective model (which classifies Vendor,
    /// matching the gate's fail-closed rule for unclassifiable models).
    pub model: String,

    /// Prompt (input) tokens for this turn.
    pub prompt_tokens: u64,

    /// Completion (output) tokens for this turn.
    pub completion_tokens: u64,

    /// What the inference was for (`"agent_turn"` for harness turns).
    pub purpose: String,

    /// The channel the turn served.
    pub channel_id: Uuid,

    /// The member whose message triggered the turn (64-char hex pubkey).
    pub user_pubkey: String,

    /// The work-thread root event id, when the turn was thread-scoped
    /// (64-char hex event id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_root_id: Option<String>,
}

impl AgentTurnAttributionPayload {
    /// Validate field shapes: purpose must be a known [`InferencePurpose`]
    /// string, pubkey/event ids must be 64-char lowercase hex, model must be
    /// non-empty. The relay rejects invalid payloads at ingest (the
    /// `model_usage` CHECK constraints would hard-error otherwise).
    pub fn validate(&self) -> Result<(), String> {
        if self.model.trim().is_empty() {
            return Err("model must be non-empty".into());
        }
        // Match the model_usage CHECK constraint (1..=128 chars) so a bad
        // payload is rejected VISIBLY at ingest rather than silently losing
        // the metering row when the side-effect INSERT hard-fails.
        if self.model.chars().count() > 128 {
            return Err("model must be at most 128 characters".into());
        }
        let known_purpose = matches!(
            self.purpose.as_str(),
            "agent_turn" | "copilot" | "gate" | "embedding"
        );
        if !known_purpose {
            return Err(format!("unknown purpose: {}", self.purpose));
        }
        if !is_hex64(&self.user_pubkey) {
            return Err("userPubkey must be 64-char lowercase hex".into());
        }
        if let Some(root) = &self.thread_root_id {
            if !is_hex64(root) {
                return Err("threadRootId must be 64-char lowercase hex".into());
            }
        }
        Ok(())
    }

    /// The purpose as a typed [`InferencePurpose`]. Only call after
    /// [`Self::validate`]; unknown strings map to `AgentTurn` defensively.
    pub fn purpose_typed(&self) -> InferencePurpose {
        match self.purpose.as_str() {
            "copilot" => InferencePurpose::Copilot,
            "gate" => InferencePurpose::Gate,
            "embedding" => InferencePurpose::Embedding,
            _ => InferencePurpose::AgentTurn,
        }
    }
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AgentTurnAttributionPayload {
        AgentTurnAttributionPayload {
            model: "ollama:qwen3:14b".into(),
            prompt_tokens: 2050,
            completion_tokens: 145,
            purpose: "agent_turn".into(),
            channel_id: Uuid::new_v4(),
            user_pubkey: "2b".repeat(32),
            thread_root_id: Some("aa".repeat(32)),
        }
    }

    #[test]
    fn serde_round_trip_camel_case_and_validate() {
        let p = sample();
        p.validate().expect("valid");
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("promptTokens"), "{json}");
        assert!(json.contains("threadRootId"), "{json}");
        let back: AgentTurnAttributionPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        // Unknown fields are ignored (forward compatibility).
        let extended = json.replace('}', ",\"futureField\":1}");
        let back: AgentTurnAttributionPayload = serde_json::from_str(&extended).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn validate_rejects_bad_shapes() {
        let mut p = sample();
        p.model = "  ".into();
        assert!(p.validate().is_err());

        let mut p = sample();
        p.model = "m".repeat(129);
        assert!(p.validate().is_err(), "over-length model rejected");
        let mut p = sample();
        p.model = "m".repeat(128);
        p.validate().expect("128-char model is the DB bound");

        let mut p = sample();
        p.purpose = "mining".into();
        assert!(p.validate().is_err());

        let mut p = sample();
        p.user_pubkey = "nothex".into();
        assert!(p.validate().is_err());

        let mut p = sample();
        p.user_pubkey = p.user_pubkey.to_uppercase();
        assert!(p.validate().is_err(), "uppercase hex rejected");

        let mut p = sample();
        p.thread_root_id = Some("short".into());
        assert!(p.validate().is_err());

        let mut p = sample();
        p.thread_root_id = None;
        p.validate().expect("absent thread root is fine");
    }
}
