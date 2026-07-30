//! Stub backends (Silent Mesh Phase 3).
//!
//! Standing in for the real serving paths so the gateway's routing and
//! metering can be exercised with no hardware or provider: each returns a
//! canned completion and a whitespace-word token estimate. The TEE
//! provider and the per-user vendor CLIs replace their stubs as
//! `ModelBackend` impls in later slices; the trait contract does not
//! change.
//!
//! [`local`] is now only a **test double** — the real `local` backend is
//! [`crate::ollama::OllamaBackend`]. Keep the stub for routing/metering
//! tests that must run with no model server; never register it in a
//! deployment, where its canned "completion" would meter as real usage.

use async_trait::async_trait;

use buzz_core::model_route::Backend;

use crate::{InferenceRequest, ModelBackend, RawInference};

/// Rough token count: whitespace words, min 1 for non-empty text. Only a
/// stand-in — real backends report the provider's own counts.
fn estimate_tokens(text: &str) -> i64 {
    let words = text.split_whitespace().count() as i64;
    if text.is_empty() {
        0
    } else {
        words.max(1)
    }
}

/// One canned backend, parameterized by its routing class + label.
struct CannedBackend {
    kind: Backend,
    name: &'static str,
}

#[async_trait]
impl ModelBackend for CannedBackend {
    fn kind(&self) -> Backend {
        self.kind
    }

    fn name(&self) -> &str {
        self.name
    }

    async fn infer(&self, req: &InferenceRequest) -> Result<RawInference, String> {
        let prompt_tokens = estimate_tokens(&req.prompt);
        let text = format!(
            "[{} stub · model={}] echo: {}",
            self.name, req.model, req.prompt
        );
        Ok(RawInference {
            completion_tokens: estimate_tokens(&text),
            prompt_tokens,
            text,
        })
    }
}

/// The workspace-local (server-GPU / client-local) stub. Zero-egress class.
pub fn local() -> Box<dyn ModelBackend> {
    Box::new(CannedBackend {
        kind: Backend::Local,
        name: "local",
    })
}

/// The TEE-attested provider stub. Attested-egress class.
pub fn tee() -> Box<dyn ModelBackend> {
    Box::new(CannedBackend {
        kind: Backend::Tee,
        name: "tee",
    })
}

/// The per-user vendor-subscription stub. Cleartext-egress class.
pub fn vendor() -> Box<dyn ModelBackend> {
    Box::new(CannedBackend {
        kind: Backend::Vendor,
        name: "vendor",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_kinds_match_their_routing_class() {
        assert_eq!(local().kind(), Backend::Local);
        assert_eq!(tee().kind(), Backend::Tee);
        assert_eq!(vendor().kind(), Backend::Vendor);
    }

    #[test]
    fn token_estimate_is_word_count_with_empty_zero() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("one"), 1);
        assert_eq!(estimate_tokens("two words"), 2);
        assert_eq!(estimate_tokens("   "), 1); // whitespace-only is non-empty
    }
}
