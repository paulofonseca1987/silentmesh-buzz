//! Tier-aware model routing policy (Silent Mesh Phase 3, D16/D21/D24).
//!
//! The privacy spine of the model plane: given the channel's immutable
//! privacy [`ChannelTier`](crate::channel::ChannelTier), which inference
//! **backends** may serve a request, and which are refused as
//! privacy-weakening. This module is pure policy — no I/O, no backends —
//! so the whole decision surface is exhaustively unit-testable, the same
//! way the D41 thread-authority matrix is. The gateway
//! (`sm-gateway`) consults it before dispatching, and records attribution
//! after.
//!
//! # The ordering
//!
//! A **backend** is ranked by how far content travels to reach it:
//!
//! - [`Backend::Local`] — client-local or the workspace's own GPUs. **Zero
//!   egress**: content never leaves the owner's infrastructure.
//! - [`Backend::Tee`] — a TEE-attested confidential-compute provider.
//!   Content egresses, but only after attestation and under confidential
//!   compute.
//! - [`Backend::Vendor`] — a member's own third-party subscription (e.g.
//!   their Claude account). Content egresses to a third party in the
//!   clear.
//!
//! A **channel tier** declares the *minimum* privacy every request in it
//! must meet (D24):
//!
//! - [`ChannelTier::Owned`] — zero egress. Only `Local`.
//! - [`ChannelTier::Private`] — owned **plus** attested providers. `Local`
//!   or `Tee`.
//! - [`ChannelTier::Open`] — the loosest; members' own vendors allowed.
//!   Any backend.
//!
//! # Purpose pins
//!
//! Some inference is pinned to the owned tier **regardless of channel
//! tier** (D25/D30/D37): the Prompt Copilot, the Privacy Gate's model
//! assist, and the retrieval embedding pipeline all read across content
//! that must never egress, so they may only ever use `Local`. See
//! [`InferencePurpose`].

use crate::channel::ChannelTier;
use std::fmt;

/// Where a model request is served, ordered by egress exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    /// Client-local or workspace-GPU inference. Zero egress.
    Local,
    /// TEE-attested confidential-compute provider. Attested egress.
    Tee,
    /// A member's own third-party vendor subscription. Cleartext egress.
    Vendor,
}

impl Backend {
    /// Wire/DB string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Local => "local",
            Backend::Tee => "tee",
            Backend::Vendor => "vendor",
        }
    }

    /// Parse from the wire/DB string.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "local" => Some(Backend::Local),
            "tee" => Some(Backend::Tee),
            "vendor" => Some(Backend::Vendor),
            _ => None,
        }
    }

    /// Every backend, strongest-privacy first.
    pub const ALL: [Backend; 3] = [Backend::Local, Backend::Tee, Backend::Vendor];

    /// The strongest channel tier this backend is permitted to serve — its
    /// egress class expressed as the tightest minimum it satisfies. `Local`
    /// satisfies `owned` (and everything looser); `Tee` satisfies `private`
    /// but not `owned`; `Vendor` satisfies only `open`. The routing rule is
    /// exactly "the backend must satisfy the channel's minimum".
    fn satisfies_minimum(&self, tier: ChannelTier) -> bool {
        match self {
            // Zero egress satisfies every tier.
            Backend::Local => true,
            // Attested egress satisfies private and open, never owned.
            Backend::Tee => matches!(tier, ChannelTier::Private | ChannelTier::Open),
            // Cleartext third-party egress satisfies only open.
            Backend::Vendor => matches!(tier, ChannelTier::Open),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a model request is *for*. Most inference is ordinary agent work
/// and routes by channel tier; three purposes are pinned to the owned tier
/// (Local only) no matter the channel, because they read across content
/// that must never egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InferencePurpose {
    /// An agent turn. Routes by the channel's tier.
    AgentTurn,
    /// Prompt Copilot refinement (D25). Owned-pinned.
    Copilot,
    /// Privacy Gate model assist (D30). Owned-pinned.
    Gate,
    /// Retrieval embedding pipeline (D37). Owned-pinned.
    Embedding,
}

impl InferencePurpose {
    /// Wire/DB string.
    pub fn as_str(&self) -> &'static str {
        match self {
            InferencePurpose::AgentTurn => "agent_turn",
            InferencePurpose::Copilot => "copilot",
            InferencePurpose::Gate => "gate",
            InferencePurpose::Embedding => "embedding",
        }
    }

    /// Whether this purpose is pinned to the owned tier (Local only)
    /// regardless of channel tier.
    pub fn is_owned_pinned(&self) -> bool {
        matches!(
            self,
            InferencePurpose::Copilot | InferencePurpose::Gate | InferencePurpose::Embedding
        )
    }
}

impl fmt::Display for InferencePurpose {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// The request may proceed to `backend`.
    Allow,
    /// The request is refused; `reason` is a stable, sanitized explanation.
    Refuse(RefuseReason),
}

impl RouteDecision {
    /// Convenience: did routing allow the request?
    pub fn is_allowed(&self) -> bool {
        matches!(self, RouteDecision::Allow)
    }
}

/// Why a route was refused. Stable variants so callers (and tests) can
/// match without string-parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// The backend egresses below the channel's minimum tier.
    BelowChannelMinimum {
        /// The channel's declared minimum.
        tier: ChannelTier,
        /// The backend that was refused.
        backend: Backend,
    },
    /// The purpose is pinned to the owned tier (Local only) and a
    /// non-Local backend was requested.
    OwnedPinnedPurpose {
        /// The pinned purpose.
        purpose: InferencePurpose,
        /// The backend that was refused.
        backend: Backend,
    },
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefuseReason::BelowChannelMinimum { tier, backend } => write!(
                f,
                "backend '{backend}' egresses below the channel's minimum tier '{tier}'"
            ),
            RefuseReason::OwnedPinnedPurpose { purpose, backend } => write!(
                f,
                "'{purpose}' inference is pinned to the owned tier (local only); \
                 backend '{backend}' is not permitted"
            ),
        }
    }
}

/// Decide whether a request in a channel of `tier`, for `purpose`, may
/// route to `backend`.
///
/// Two independent gates, both of which must pass:
/// 1. **Purpose pin**: an owned-pinned purpose ([`InferencePurpose::is_owned_pinned`])
///    may only use `Local`.
/// 2. **Channel minimum** (D24): the backend must satisfy the channel's
///    declared minimum tier.
///
/// The purpose pin is checked first so a copilot/gate/embedding request
/// gets the more specific refusal even in an `open` channel.
pub fn route(tier: ChannelTier, backend: Backend, purpose: InferencePurpose) -> RouteDecision {
    if purpose.is_owned_pinned() && backend != Backend::Local {
        return RouteDecision::Refuse(RefuseReason::OwnedPinnedPurpose { purpose, backend });
    }
    if !backend.satisfies_minimum(tier) {
        return RouteDecision::Refuse(RefuseReason::BelowChannelMinimum { tier, backend });
    }
    RouteDecision::Allow
}

/// The backends permitted for `purpose` in a channel of `tier`, strongest
/// privacy first. Empty is impossible — `Local` is always permitted.
pub fn allowed_backends(tier: ChannelTier, purpose: InferencePurpose) -> Vec<Backend> {
    Backend::ALL
        .into_iter()
        .filter(|b| route(tier, *b, purpose).is_allowed())
        .collect()
}

/// Whether `prefix` is a recognized persona model-provider prefix (the
/// `provider` half of `"provider:model-id"`).
///
/// Needed because the persona split is ambiguous: model ids themselves may
/// contain `:` (an Ollama tag like `llama3.2:3b`), so a consumer must only
/// treat the pre-colon segment as a provider when it names one we know.
/// Covers the vendor providers seen in the ecosystem plus every
/// local/TEE runtime [`provider_to_backend`] classifies.
pub fn is_known_provider_prefix(prefix: &str) -> bool {
    matches!(
        prefix.trim().to_ascii_lowercase().as_str(),
        "anthropic"
            | "openai"
            | "openai-compat"
            | "databricks"
            | "databricks-v2"
            | "databricks_v2"
            | "local"
            | "ollama"
            | "vllm"
            | "llamacpp"
            | "llama-cpp"
            | "tee"
    )
}

/// Classify a full persona model string (`"provider:model-id"` or a bare
/// model id) into the [`Backend`] that serves it — the shared rule the tier
/// gate and the relay's attribution ingest both apply, so a turn is metered
/// under exactly the classification it was gated by.
///
/// The pre-colon segment counts as a provider only when
/// [`is_known_provider_prefix`] recognizes it (model ids themselves contain
/// `:` — an Ollama tag like `llama3.2:3b` must not classify by its tag).
/// A bare or unrecognized-prefix model fails closed to [`Backend::Vendor`],
/// matching [`provider_to_backend`]'s rule for absent providers.
pub fn classify_model(model: &str) -> Backend {
    match model.trim().split_once(':') {
        Some((prefix, _)) if is_known_provider_prefix(prefix) => provider_to_backend(Some(prefix)),
        _ => provider_to_backend(None),
    }
}

/// Qualify a configured model id with its **declared** provider, producing
/// the `"provider:model-id"` form [`classify_model`] can route on.
///
/// A launcher (the desktop app, a persona projection) knows two separate
/// facts: which model the agent should use, and which provider serves it.
/// Passing only the model id downstream throws the second fact away, and
/// since an unqualified id fails closed to [`Backend::Vendor`], a perfectly
/// local agent then gets refused in an owned channel. This joins them back
/// together at the boundary.
///
/// Rules, in order:
/// - A model already carrying a known provider prefix is returned unchanged —
///   never double-prefixed, and a caller's explicit qualification wins.
/// - Otherwise a non-empty `provider` is prepended.
/// - Otherwise the bare id passes through, and downstream classification
///   fails closed as before.
///
/// **This does not weaken the self-declaration rule.** A launcher claiming
/// `ollama` only *proposes* a Local classification; the harness still
/// confirms the agent's own advertised catalog id before a turn runs, and
/// the prefixed-desired-vs-bare-advertised path is vendor-only, so a vendor
/// agent mislabeled `ollama:` fails the switch confirm rather than
/// smuggling a Local classification.
pub fn qualify_model(provider: Option<&str>, model: &str) -> String {
    let model = model.trim();
    // An empty model id must never be qualified: `"ollama:"` would classify
    // as Local off a known prefix with no model behind it, manufacturing a
    // zero-egress claim out of a missing configuration value.
    if model.is_empty() {
        return String::new();
    }
    if let Some((prefix, _)) = model.split_once(':') {
        if is_known_provider_prefix(prefix) {
            return model.to_owned();
        }
    }
    match provider.map(str::trim).filter(|p| !p.is_empty()) {
        Some(provider) => format!("{provider}:{model}"),
        None => model.to_owned(),
    }
}

/// Classify an agent's configured model **provider** — the `provider` half of
/// a persona's `"provider:model-id"` string (see
/// `buzz_persona::persona::split_model`) — into the [`Backend`] that serves
/// it, so the harness can enforce the channel tier at the turn boundary.
///
/// Only providers we can *prove* keep content off third-party infrastructure
/// map to [`Backend::Local`] (client-local / workspace-GPU runtimes) or
/// [`Backend::Tee`]. **Everything else — including an absent (`None`) or
/// unrecognized provider — is [`Backend::Vendor`]**, the most egress-exposed
/// class. This is deliberately *fail-closed*: an unclassifiable model is
/// refused in owned/private channels rather than silently permitted to
/// egress. As real local/TEE backends land (Phase 3), their provider strings
/// join the match arms below.
///
/// Matching is case-insensitive and ignores surrounding whitespace.
pub fn provider_to_backend(provider: Option<&str>) -> Backend {
    let Some(provider) = provider else {
        // No provider prefix: the agent runs its built-in default, which we
        // cannot prove is local. Fail closed to the most egress-exposed class.
        return Backend::Vendor;
    };
    match provider.trim().to_ascii_lowercase().as_str() {
        // Zero-egress: client-local or workspace-GPU inference runtimes.
        "local" | "ollama" | "vllm" | "llamacpp" | "llama-cpp" => Backend::Local,
        // Attested confidential compute.
        "tee" => Backend::Tee,
        // Cleartext third-party egress: anthropic, openai, databricks, and any
        // other or unrecognized provider.
        _ => Backend::Vendor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChannelTier::*;

    const TIERS: [ChannelTier; 3] = [Owned, Private, Open];
    const PURPOSES: [InferencePurpose; 4] = [
        InferencePurpose::AgentTurn,
        InferencePurpose::Copilot,
        InferencePurpose::Gate,
        InferencePurpose::Embedding,
    ];

    /// The full tier × backend × purpose matrix, decided independently of
    /// the implementation so a policy change must update this table too.
    #[test]
    fn routing_matrix_is_exhaustive() {
        for tier in TIERS {
            for backend in Backend::ALL {
                for purpose in PURPOSES {
                    let decision = route(tier, backend, purpose);
                    // Expected table, restated independently of the impl —
                    // the owned-pinned set is written as a literal here (not
                    // via `is_owned_pinned()`) so this matrix genuinely
                    // double-entries that dimension, not just the
                    // channel-minimum arm.
                    let pinned = matches!(
                        purpose,
                        InferencePurpose::Copilot
                            | InferencePurpose::Gate
                            | InferencePurpose::Embedding
                    );
                    let expected = if pinned {
                        backend == Backend::Local
                    } else {
                        match (tier, backend) {
                            (Owned, Backend::Local) => true,
                            (Owned, _) => false,
                            (Private, Backend::Local | Backend::Tee) => true,
                            (Private, Backend::Vendor) => false,
                            (Open, _) => true,
                        }
                    };
                    assert_eq!(
                        decision.is_allowed(),
                        expected,
                        "tier={tier} backend={backend} purpose={purpose}: expected allowed={expected}"
                    );
                }
            }
        }
    }

    #[test]
    fn owned_channel_forbids_all_egress() {
        assert!(route(Owned, Backend::Local, InferencePurpose::AgentTurn).is_allowed());
        for egress in [Backend::Tee, Backend::Vendor] {
            let d = route(Owned, egress, InferencePurpose::AgentTurn);
            assert!(matches!(
                d,
                RouteDecision::Refuse(RefuseReason::BelowChannelMinimum { .. })
            ));
        }
    }

    #[test]
    fn private_channel_allows_tee_not_vendor() {
        assert!(route(Private, Backend::Tee, InferencePurpose::AgentTurn).is_allowed());
        assert!(matches!(
            route(Private, Backend::Vendor, InferencePurpose::AgentTurn),
            RouteDecision::Refuse(RefuseReason::BelowChannelMinimum { .. })
        ));
    }

    #[test]
    fn open_channel_allows_everything_for_agent_turns() {
        for backend in Backend::ALL {
            assert!(route(Open, backend, InferencePurpose::AgentTurn).is_allowed());
        }
    }

    #[test]
    fn owned_pinned_purposes_refuse_egress_even_in_open_channels() {
        for purpose in [
            InferencePurpose::Copilot,
            InferencePurpose::Gate,
            InferencePurpose::Embedding,
        ] {
            assert!(route(Open, Backend::Local, purpose).is_allowed());
            for egress in [Backend::Tee, Backend::Vendor] {
                let d = route(Open, egress, purpose);
                assert!(
                    matches!(
                        d,
                        RouteDecision::Refuse(RefuseReason::OwnedPinnedPurpose { .. })
                    ),
                    "owned-pinned {purpose} must refuse {egress} even in an open channel"
                );
            }
        }
    }

    #[test]
    fn allowed_backends_reflects_the_rules() {
        assert_eq!(
            allowed_backends(Owned, InferencePurpose::AgentTurn),
            vec![Backend::Local]
        );
        assert_eq!(
            allowed_backends(Private, InferencePurpose::AgentTurn),
            vec![Backend::Local, Backend::Tee]
        );
        assert_eq!(
            allowed_backends(Open, InferencePurpose::AgentTurn),
            vec![Backend::Local, Backend::Tee, Backend::Vendor]
        );
        // Owned-pinned collapses to Local at every tier.
        assert_eq!(
            allowed_backends(Open, InferencePurpose::Copilot),
            vec![Backend::Local]
        );
    }

    #[test]
    fn provider_to_backend_maps_known_and_fails_closed() {
        use InferencePurpose::AgentTurn;

        // Known third-party vendors → Vendor.
        for p in ["anthropic", "openai", "databricks"] {
            assert_eq!(provider_to_backend(Some(p)), Backend::Vendor, "{p}");
        }
        // Known zero-egress runtimes → Local.
        for p in ["local", "ollama", "vllm", "llamacpp", "llama-cpp"] {
            assert_eq!(provider_to_backend(Some(p)), Backend::Local, "{p}");
        }
        // TEE sentinel → Tee.
        assert_eq!(provider_to_backend(Some("tee")), Backend::Tee);
        // Case-insensitive + whitespace-trimmed. The trim/case probes must
        // target a NON-Vendor arm — an unmatched string falls through to
        // Vendor anyway, so only a Local-mapping probe can catch a dropped
        // `.trim()`/`to_ascii_lowercase()`.
        assert_eq!(provider_to_backend(Some("  ollama ")), Backend::Local);
        assert_eq!(provider_to_backend(Some("Ollama")), Backend::Local);
        assert_eq!(provider_to_backend(Some(" TEE ")), Backend::Tee);
        // Unknown provider and an absent provider both fail closed to Vendor.
        assert_eq!(provider_to_backend(Some("acme-cloud")), Backend::Vendor);
        assert_eq!(provider_to_backend(None), Backend::Vendor);

        // End-to-end at the policy layer: a vendor-backed agent turn is
        // refused in owned/private channels and allowed only in open.
        let vendor = provider_to_backend(Some("anthropic"));
        assert!(matches!(
            route(Owned, vendor, AgentTurn),
            RouteDecision::Refuse(RefuseReason::BelowChannelMinimum { .. })
        ));
        assert!(matches!(
            route(Private, vendor, AgentTurn),
            RouteDecision::Refuse(RefuseReason::BelowChannelMinimum { .. })
        ));
        assert!(route(Open, vendor, AgentTurn).is_allowed());
        // A local-backed turn is allowed at every tier.
        let local = provider_to_backend(Some("ollama"));
        for tier in TIERS {
            assert!(route(tier, local, AgentTurn).is_allowed(), "{tier}");
        }
    }

    #[test]
    fn qualify_model_joins_the_launcher_s_two_facts() {
        // The gap this closes: a desktop-launched local agent used to send a
        // bare id, which fails closed to Vendor and is refused in an owned
        // channel even though it never egresses.
        assert_eq!(
            qualify_model(Some("ollama"), "qwen3:14b"),
            "ollama:qwen3:14b"
        );
        assert_eq!(
            classify_model(&qualify_model(Some("ollama"), "qwen3:14b")),
            Backend::Local
        );

        // Already qualified: never double-prefixed, and the caller's own
        // qualification wins over the declared provider.
        assert_eq!(
            qualify_model(Some("ollama"), "ollama:qwen3:14b"),
            "ollama:qwen3:14b"
        );
        assert_eq!(
            qualify_model(Some("ollama"), "anthropic:claude-x"),
            "anthropic:claude-x",
            "an explicitly qualified model must not be re-labelled by the launcher"
        );

        // A colon-bearing tag is not a provider prefix — it gets qualified.
        assert_eq!(qualify_model(Some("openai"), "gpt-4o"), "openai:gpt-4o");

        // No provider declared → unchanged, so classification still fails
        // closed to Vendor exactly as before.
        for absent in [None, Some(""), Some("   ")] {
            assert_eq!(qualify_model(absent, "qwen3:14b"), "qwen3:14b");
            assert_eq!(
                classify_model(&qualify_model(absent, "qwen3:14b")),
                Backend::Vendor
            );
        }

        // An empty model id is never qualified — otherwise "ollama:" would
        // classify Local off the prefix alone, with no model behind it.
        assert_eq!(qualify_model(Some("ollama"), ""), "");
        assert_eq!(qualify_model(Some("ollama"), "   "), "");
        assert_eq!(
            classify_model(&qualify_model(Some("ollama"), "")),
            Backend::Vendor
        );

        // Whitespace around the pieces never produces a broken id.
        assert_eq!(
            qualify_model(Some(" ollama "), "  qwen3:14b "),
            "ollama:qwen3:14b"
        );
    }

    #[test]
    fn classify_model_matches_the_gate_rule() {
        assert_eq!(classify_model("ollama:qwen3:14b"), Backend::Local);
        assert_eq!(classify_model("vllm:llama3.1"), Backend::Local);
        assert_eq!(classify_model("tee:some-model"), Backend::Tee);
        assert_eq!(classify_model("anthropic:claude-sonnet-4"), Backend::Vendor);
        // Bare ids — including colon-bearing Ollama tags — fail closed.
        assert_eq!(classify_model("qwen3:14b"), Backend::Vendor);
        assert_eq!(classify_model("llama3.2:3b"), Backend::Vendor);
        assert_eq!(classify_model("claude-sonnet-4"), Backend::Vendor);
        assert_eq!(classify_model(""), Backend::Vendor);
    }

    #[test]
    fn known_provider_prefixes_cover_vendors_and_local_runtimes() {
        for p in [
            "anthropic",
            "openai",
            "databricks",
            "ollama",
            "local",
            "vllm",
            "llamacpp",
            "tee",
            " Ollama ", // trimmed + case-insensitive
        ] {
            assert!(is_known_provider_prefix(p), "{p}");
        }
        // Model-tag segments and arbitrary strings are NOT providers — this
        // is what stops "llama3.2:3b" being split as provider "llama3.2".
        for p in ["llama3.2", "3b", "claude", "gpt-4o", ""] {
            assert!(!is_known_provider_prefix(p), "{p}");
        }
    }

    #[test]
    fn backend_and_purpose_strings_round_trip() {
        for b in Backend::ALL {
            assert_eq!(Backend::from_str_opt(b.as_str()), Some(b));
        }
        assert_eq!(Backend::from_str_opt("nope"), None);
        for p in PURPOSES {
            // Purpose has no parser (relay never accepts it from the wire),
            // but the string must be stable/distinct.
            assert!(!p.as_str().is_empty());
        }
    }
}
