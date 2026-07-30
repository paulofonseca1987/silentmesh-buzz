//! The real `local` backend: an Ollama server (Silent Mesh Phase 3, D16).
//!
//! This is the first non-stub [`ModelBackend`]. It matters beyond "the
//! gateway can now talk to a model": every remaining Phase 3 capability —
//! Prompt Copilot (D25), Privacy Gate assist (D30), the embedding pipeline
//! (D37) — is **owned-pinned** by [`buzz_core::model_route`], meaning the
//! policy allows it on `Backend::Local` *and nowhere else*. A real local
//! backend is the single dependency they share.
//!
//! Two properties are load-bearing:
//!
//! - **Provable locality.** A backend that declares `Backend::Local` while
//!   pointing at a vendor endpoint would void the tier guarantee silently —
//!   the routing policy would wave it through for an `owned` channel and
//!   content would egress anyway. So locality is *checked*, not asserted:
//!   [`OllamaBackend::new`] refuses to build unless the base URL's host is
//!   loopback or on the tailnet ([`check_local_base_url`]). This is the
//!   gateway-layer form of the rule Phase 3 slice 3 established for the
//!   harness: a Local classification must never be inferred, only proven.
//!   Locality is a property of the *conversation*, not just the URL, so the
//!   HTTP client also refuses redirects and ignores proxy environment
//!   variables (see [`OllamaBackend::with_timeout`]).
//! - **Real token counts.** The stubs estimate tokens by counting words;
//!   this backend reports the server's own `usage` numbers, and treats a
//!   response it cannot meter as a failure rather than inventing a number
//!   (see [`parse_chat_response`]). Attribution that is quietly fictional
//!   is worse than attribution that is absent.
//!
//! The wire format is the OpenAI Chat Completions dialect Ollama exposes at
//! `{base_url}/chat/completions` — the same dialect `buzz-agent`'s
//! `Provider::Ollama` speaks, so both callers share one mental model.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use url::{Host, Url};

use buzz_core::model_route::Backend;

use crate::{InferenceRequest, ModelBackend, RawInference};

/// Default Ollama OpenAI-compatible base URL (loopback).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434/v1";

/// Environment variable overriding [`DEFAULT_BASE_URL`].
pub const BASE_URL_ENV: &str = "SM_GATEWAY_OLLAMA_BASE_URL";

/// Default per-request timeout. Generous: a 14B model on a cold cache can
/// spend minutes on the first token.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Why a base URL is not permissible for a `Backend::Local` backend.
///
/// Each variant is a refusal to *build* the backend — a misconfigured
/// gateway fails at startup rather than routing owned-tier content off the
/// machine at request time.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LocalHostError {
    /// The base URL did not parse as an absolute URL.
    #[error("base url '{0}' is not a valid absolute URL")]
    Unparseable(String),
    /// The scheme is not `http`/`https` (e.g. `file:`, `ftp:`).
    #[error("base url scheme '{0}' is not http or https")]
    UnsupportedScheme(String),
    /// Defensive: an http(s) URL with no host. Unreachable in practice —
    /// the URL parser rejects an empty host before this — but the check
    /// stays so the match is exhaustive by refusal rather than by luck.
    #[error("base url '{0}' has no host")]
    NoHost(String),
    /// The host is neither loopback nor on the tailnet.
    #[error(
        "host '{0}' is not local: a Backend::Local gateway may only reach \
         loopback or the tailnet (100.64.0.0/10, fd7a:115c:a1e0::/48, *.ts.net)"
    )]
    NotLocal(String),
}

/// Failure to construct an [`OllamaBackend`].
#[derive(Debug, thiserror::Error)]
pub enum OllamaInitError {
    /// The configured base URL is not a permissible local endpoint.
    #[error(transparent)]
    Host(#[from] LocalHostError),
    /// The HTTP client could not be built.
    #[error("http client: {0}")]
    Client(String),
}

/// Tailscale's CGNAT IPv4 range is `100.64.0.0/10` — second octet 64..=127.
fn is_tailscale_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    a == 100 && (64..=127).contains(&b)
}

/// Tailscale's IPv6 ULA prefix is `fd7a:115c:a1e0::/48`.
fn is_tailscale_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

/// Is this IP one a `Backend::Local` request may reach?
///
/// Loopback (same machine — strictly stronger than the tailnet) or a
/// Tailscale address. Notably **not** RFC1918: a plain LAN host is not
/// owner-attested the way a tailnet peer is, and a stray `192.168.x.y`
/// typo should fail closed rather than become a permitted route.
fn is_local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || is_tailscale_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            // `::ffff:a.b.c.d` is an IPv4 host wearing an IPv6 hat; judge
            // it by the IPv4 rules so the mapping isn't a bypass.
            Some(v4) => v4.is_loopback() || is_tailscale_v4(v4),
            None => v6.is_loopback() || is_tailscale_v6(v6),
        },
    }
}

/// Is this hostname one a `Backend::Local` request may reach?
///
/// `localhost` and Tailscale MagicDNS names (`<host>.<tailnet>.ts.net`).
/// Hostname forms trust the local resolver; an IP literal is the stronger
/// configuration and is what the default uses.
fn is_local_domain(domain: &str) -> bool {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    if d == "localhost" {
        return true;
    }
    // A MagicDNS name, with at least one label in front of the suffix — so
    // a bare "ts.net" (or an attacker's "ts.net.evil.com", which does not
    // end with the suffix at all) does not qualify.
    match d.strip_suffix(".ts.net") {
        Some(prefix) => !prefix.is_empty(),
        None => false,
    }
}

/// Verify a base URL points at a machine a `Backend::Local` request may
/// reach: loopback, or a tailnet peer.
///
/// This is the gateway's egress guard, applied at construction so a
/// misconfiguration is a startup failure and never a silent leak. It is
/// pure over the URL string — no DNS, no I/O — so it is fully testable and
/// cannot be tricked by resolution timing.
pub fn check_local_base_url(base_url: &str) -> Result<(), LocalHostError> {
    let url = Url::parse(base_url).map_err(|_| LocalHostError::Unparseable(base_url.to_owned()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(LocalHostError::UnsupportedScheme(url.scheme().to_owned()));
    }
    match url.host() {
        None => Err(LocalHostError::NoHost(base_url.to_owned())),
        Some(Host::Ipv4(ip)) if is_local_ip(IpAddr::V4(ip)) => Ok(()),
        Some(Host::Ipv6(ip)) if is_local_ip(IpAddr::V6(ip)) => Ok(()),
        Some(Host::Domain(d)) if is_local_domain(d) => Ok(()),
        Some(host) => Err(LocalHostError::NotLocal(host.to_string())),
    }
}

/// Strip a leading `ollama:` persona-provider prefix for the wire.
///
/// Buzz personas name models `provider:model-id`, and Phase 3 slice 3 made
/// `ollama:<model>` the *self-declared* local identity — so that prefixed
/// form is what attribution records and what callers pass in. The Ollama
/// server itself knows only the bare tag. Ollama tags contain `:`
/// themselves (`qwen3:14b`), so only the known provider prefix is removed,
/// case-insensitively; everything else passes through untouched.
pub fn wire_model(model: &str) -> &str {
    let trimmed = model.trim();
    match trimmed.split_once(':') {
        Some((prefix, rest)) if prefix.eq_ignore_ascii_case("ollama") && !rest.is_empty() => rest,
        _ => trimmed,
    }
}

/// Parse an OpenAI-dialect chat completion into a [`RawInference`].
///
/// Fails when the response carries no usable `usage` counts. That is
/// deliberate: the gateway's whole contract is *metered* inference, and a
/// response we cannot attribute would either be recorded as a fabricated
/// estimate or as zero tokens — both of which corrupt `model_usage`
/// silently. An unmeterable response is a backend error.
pub fn parse_chat_response(body: &serde_json::Value) -> Result<RawInference, String> {
    let text = body
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_owned();

    let usage = body
        .get("usage")
        .ok_or_else(|| "response carries no usage counts (unmeterable)".to_owned())?;
    let count = |field: &str| -> Result<i64, String> {
        usage
            .get(field)
            .and_then(serde_json::Value::as_i64)
            .filter(|n| *n >= 0)
            .ok_or_else(|| format!("response usage.{field} is missing or invalid"))
    };

    Ok(RawInference {
        text,
        prompt_tokens: count("prompt_tokens")?,
        completion_tokens: count("completion_tokens")?,
    })
}

/// An Ollama server serving the `local` (zero-egress) routing class.
///
/// Construct with [`OllamaBackend::new`] (or [`OllamaBackend::from_env`])
/// and register it on the gateway:
///
/// ```no_run
/// # async fn demo(db: buzz_db::Db) -> Result<(), Box<dyn std::error::Error>> {
/// use sm_gateway::{ollama::OllamaBackend, Gateway};
///
/// let gateway = Gateway::new(db).with_backend(Box::new(OllamaBackend::from_env()?));
/// # let _ = gateway;
/// # Ok(())
/// # }
/// ```
pub struct OllamaBackend {
    client: reqwest::Client,
    /// Base URL, trailing slash trimmed.
    base_url: String,
    name: String,
}

impl OllamaBackend {
    /// Build a backend against `base_url`, with [`DEFAULT_TIMEOUT`].
    ///
    /// # Errors
    /// [`LocalHostError`] if the URL is not a permissible local endpoint —
    /// the egress guard — or [`OllamaInitError::Client`] if the HTTP client
    /// cannot be built.
    pub fn new(base_url: &str) -> Result<Self, OllamaInitError> {
        Self::with_timeout(base_url, DEFAULT_TIMEOUT)
    }

    /// Build a backend with an explicit per-request timeout.
    ///
    /// # Errors
    /// As [`OllamaBackend::new`].
    pub fn with_timeout(base_url: &str, timeout: Duration) -> Result<Self, OllamaInitError> {
        let trimmed = base_url.trim().trim_end_matches('/');
        check_local_base_url(trimmed)?;
        // The URL check alone does NOT make this backend zero-egress —
        // two transport behaviors would walk straight around it, so both
        // are pinned off:
        //
        // - **Redirects.** reqwest follows up to 10 by default. A loopback
        //   service answering `302 Location: https://api.openai.com/…`
        //   would egress owned-tier content from an endpoint that passed
        //   the host check. We validate the URL we dial; only refusing to
        //   follow makes that the URL we actually talk to.
        // - **Proxies.** reqwest honors `HTTP_PROXY`/`ALL_PROXY` from the
        //   environment and does not bypass loopback on its own, so a
        //   proxy var in the relay's environment would route "local"
        //   inference through a third party.
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| OllamaInitError::Client(e.to_string()))?;
        Ok(Self {
            client,
            name: format!("ollama@{trimmed}"),
            base_url: trimmed.to_owned(),
        })
    }

    /// Build from the environment: `SM_GATEWAY_OLLAMA_BASE_URL`, defaulting
    /// to [`DEFAULT_BASE_URL`].
    ///
    /// # Errors
    /// As [`OllamaBackend::new`].
    pub fn from_env() -> Result<Self, OllamaInitError> {
        let base = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        Self::new(&base)
    }

    /// The endpoint this backend posts completions to.
    pub fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }
}

#[async_trait]
impl ModelBackend for OllamaBackend {
    fn kind(&self) -> Backend {
        Backend::Local
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn infer(&self, req: &InferenceRequest) -> Result<RawInference, String> {
        let model = wire_model(&req.model);
        let body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": req.prompt }],
            "stream": false,
        });

        let resp = self
            .client
            .post(self.completions_url())
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("ollama request failed: {e}"))?;

        let status = resp.status();
        let payload = resp
            .text()
            .await
            .map_err(|e| format!("ollama response body: {e}"))?;
        if !status.is_success() {
            // Truncate: a server error page must not flood the log.
            let detail: String = payload.chars().take(400).collect();
            return Err(format!("ollama returned {status}: {detail}"));
        }

        let json: serde_json::Value =
            serde_json::from_str(&payload).map_err(|e| format!("ollama response json: {e}"))?;
        let raw = parse_chat_response(&json)?;
        tracing::debug!(
            backend = %self.name,
            model = %model,
            prompt_tokens = raw.prompt_tokens,
            completion_tokens = raw.completion_tokens,
            "local inference complete"
        );
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_tailnet_hosts_are_local() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://127.5.6.7:11434/v1",
            "http://localhost:11434/v1",
            "http://LocalHost:11434/v1",
            "http://[::1]:11434/v1",
            "http://[::ffff:127.0.0.1]:11434/v1",
            // Tailscale CGNAT 100.64.0.0/10, at both edges.
            "http://100.72.140.59:11434/v1",
            "http://100.64.0.0:11434/v1",
            "http://100.127.255.255:11434/v1",
            // Tailscale IPv6 ULA + MagicDNS.
            "http://[fd7a:115c:a1e0::1]:11434/v1",
            "http://gpubox.tail1a2b3c.ts.net:11434/v1",
            "https://gpubox.tail1a2b3c.TS.NET/v1",
        ] {
            assert_eq!(check_local_base_url(url), Ok(()), "expected local: {url}");
        }
    }

    #[test]
    fn public_and_lan_hosts_are_refused() {
        for url in [
            // The failure this guard exists to prevent.
            "https://api.openai.com/v1",
            "https://api.anthropic.com/v1",
            // RFC1918 is deliberately NOT local: a LAN host is not
            // owner-attested the way a tailnet peer is.
            "http://192.168.1.10:11434/v1",
            "http://10.0.0.5:11434/v1",
            "http://172.16.0.1:11434/v1",
            // Just outside the CGNAT range, either side.
            "http://100.63.255.255:11434/v1",
            "http://100.128.0.1:11434/v1",
            // Not the Tailscale ULA prefix.
            "http://[fd7a:115c:a1e1::1]:11434/v1",
            // Suffix games: neither is a MagicDNS name.
            "http://ts.net/v1",
            "http://gpubox.ts.net.attacker.example/v1",
            "http://evil-ts.net/v1",
        ] {
            assert!(
                matches!(check_local_base_url(url), Err(LocalHostError::NotLocal(_))),
                "expected refusal: {url}"
            );
        }
    }

    #[test]
    fn malformed_base_urls_are_refused() {
        assert!(matches!(
            check_local_base_url("127.0.0.1:11434"), // no scheme
            Err(LocalHostError::Unparseable(_))
        ));
        for url in ["file:///models", "ftp://127.0.0.1/models"] {
            assert!(
                matches!(
                    check_local_base_url(url),
                    Err(LocalHostError::UnsupportedScheme(_))
                ),
                "expected scheme refusal: {url}"
            );
        }
    }

    #[test]
    fn constructor_enforces_the_egress_guard() {
        assert!(OllamaBackend::new(DEFAULT_BASE_URL).is_ok());
        assert!(matches!(
            OllamaBackend::new("https://api.openai.com/v1"),
            Err(OllamaInitError::Host(LocalHostError::NotLocal(_)))
        ));
    }

    #[test]
    fn backend_declares_the_local_routing_class() {
        let backend = OllamaBackend::new(DEFAULT_BASE_URL).expect("loopback backend");
        assert_eq!(backend.kind(), Backend::Local);
        assert_eq!(
            backend.completions_url(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_does_not_double_up_in_the_endpoint() {
        let backend = OllamaBackend::new("http://127.0.0.1:11434/v1/").expect("backend");
        assert_eq!(
            backend.completions_url(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn only_the_provider_prefix_is_stripped_for_the_wire() {
        assert_eq!(wire_model("ollama:qwen3:14b"), "qwen3:14b");
        assert_eq!(wire_model("Ollama:llama3.2:3b"), "llama3.2:3b");
        assert_eq!(wire_model("  ollama:qwen2.5:7b  "), "qwen2.5:7b");
        // Bare ids — including the colon-bearing tags — pass through.
        assert_eq!(wire_model("qwen3:14b"), "qwen3:14b");
        assert_eq!(wire_model("llama3.2"), "llama3.2");
        // A vendor-prefixed id is NOT ours to rewrite.
        assert_eq!(wire_model("anthropic:claude-x"), "anthropic:claude-x");
        // Degenerate prefix stays as-is rather than becoming empty.
        assert_eq!(wire_model("ollama:"), "ollama:");
    }

    #[test]
    fn chat_response_yields_text_and_server_token_counts() {
        let body = json!({
            "choices": [{ "message": { "role": "assistant", "content": "mesh online" } }],
            "usage": { "prompt_tokens": 31, "completion_tokens": 3, "total_tokens": 34 }
        });
        let raw = parse_chat_response(&body).expect("parse");
        assert_eq!(raw.text, "mesh online");
        assert_eq!(raw.prompt_tokens, 31);
        assert_eq!(raw.completion_tokens, 3);
    }

    #[test]
    fn an_unmeterable_response_is_an_error_not_a_guess() {
        // No usage at all.
        let no_usage = json!({ "choices": [{ "message": { "content": "hi" } }] });
        assert!(parse_chat_response(&no_usage).is_err());
        // Usage present but a count missing.
        let partial = json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": 5 }
        });
        assert!(parse_chat_response(&partial).is_err());
        // Nonsense counts are refused rather than recorded.
        let negative = json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": -1, "completion_tokens": 3 }
        });
        assert!(parse_chat_response(&negative).is_err());
    }

    /// A one-shot loopback server that answers the first request with
    /// `resp` and hangs up. Returns the port it bound.
    fn one_shot_server(resp: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.flush();
            }
        });
        port
    }

    fn probe_request(model: &str) -> InferenceRequest {
        InferenceRequest {
            community_id: buzz_core::CommunityId::from_uuid(uuid::Uuid::from_u128(1)),
            user_pubkey: vec![0xaau8; 32],
            agent_pubkey: None,
            channel_id: None,
            thread_id: None,
            tier: buzz_core::channel::ChannelTier::Owned,
            purpose: buzz_core::model_route::InferencePurpose::AgentTurn,
            model: model.to_owned(),
            backend: None,
            prompt: "hello".to_owned(),
        }
    }

    #[tokio::test]
    async fn a_redirect_off_the_local_host_is_refused_not_followed() {
        // The hole this closes: reqwest follows redirects by default, so a
        // loopback endpoint that passed the URL check could bounce the
        // request — and the owned-tier prompt with it — to a vendor.
        let port = one_shot_server(
            "HTTP/1.1 302 Found\r\n\
             Location: https://api.openai.com/v1/chat/completions\r\n\
             Content-Length: 0\r\n\r\n",
        );
        let backend = OllamaBackend::with_timeout(
            &format!("http://127.0.0.1:{port}/v1"),
            Duration::from_secs(10),
        )
        .expect("loopback backend");

        let err = backend
            .infer(&probe_request("llama3.2:3b"))
            .await
            .expect_err("a redirect must not be followed");
        // We surface the redirect as the backend's own failure. If it had
        // been followed, the error would name the vendor host (or, worse,
        // there would be no error at all).
        assert!(err.contains("302"), "expected the 302 surfaced: {err}");
        assert!(
            !err.contains("openai"),
            "the request must never have gone to the redirect target: {err}"
        );
    }

    #[test]
    fn an_empty_completion_is_still_a_valid_metered_response() {
        // A model that answers with nothing (or whose content is absent)
        // still consumed prompt tokens — that must be attributed.
        let body = json!({
            "choices": [{ "message": { "role": "assistant" } }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 0 }
        });
        let raw = parse_chat_response(&body).expect("parse");
        assert_eq!(raw.text, "");
        assert_eq!(raw.prompt_tokens, 12);
    }
}

#[cfg(test)]
mod ollama_probe_tests {
    //! Live probes against a running Ollama server. Everything above this
    //! module is pure; these are the tests that prove the wire format and
    //! the metering are right against the real thing.
    //!
    //! Run with a server up (`ollama serve`) and the model pulled:
    //! ```text
    //! SM_GATEWAY_OLLAMA_PROBE=1 \
    //!   cargo test -p sm-gateway --lib ollama_probe_tests -- --ignored --test-threads=1
    //! ```
    //! `SM_GATEWAY_OLLAMA_PROBE_MODEL` overrides the model (default
    //! `llama3.2:3b` — small enough to answer in seconds);
    //! `SM_GATEWAY_OLLAMA_BASE_URL` overrides the endpoint. The second
    //! probe additionally needs Postgres and records real rows.
    use super::*;

    use buzz_core::channel::ChannelTier;
    use buzz_core::model_route::InferencePurpose;
    use buzz_core::CommunityId;
    use buzz_db::Db;
    use uuid::Uuid;

    use crate::{Gateway, InferenceRequest};

    const PROBE_ENV: &str = "SM_GATEWAY_OLLAMA_PROBE";
    const MODEL_ENV: &str = "SM_GATEWAY_OLLAMA_PROBE_MODEL";
    const DEFAULT_PROBE_MODEL: &str = "llama3.2:3b";
    const TEST_DB_URL: &str = "postgres://buzz:buzz_dev@localhost:5432/buzz";

    fn probe_enabled() -> bool {
        std::env::var(PROBE_ENV).is_ok_and(|v| v == "1")
    }

    /// The model id as a caller would pass it: the **persona-prefixed**
    /// local identity, so each probe also exercises `wire_model`.
    fn probe_model() -> String {
        let bare = std::env::var(MODEL_ENV).unwrap_or_else(|_| DEFAULT_PROBE_MODEL.to_owned());
        format!("ollama:{bare}")
    }

    fn backend() -> OllamaBackend {
        OllamaBackend::with_timeout(
            &std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned()),
            Duration::from_secs(180),
        )
        .expect("probe base url must be local")
    }

    fn request(model: String, prompt: &str) -> InferenceRequest {
        InferenceRequest {
            community_id: CommunityId::from_uuid(Uuid::from_u128(1)),
            user_pubkey: vec![0xaau8; 32],
            agent_pubkey: None,
            channel_id: Some(Uuid::from_u128(2)),
            thread_id: None,
            tier: ChannelTier::Owned,
            purpose: InferencePurpose::AgentTurn,
            model,
            backend: None,
            prompt: prompt.to_owned(),
        }
    }

    #[tokio::test]
    #[ignore = "requires a running Ollama server"]
    async fn live_local_inference_returns_text_and_server_token_counts() {
        if !probe_enabled() {
            eprintln!("skipping: set {PROBE_ENV}=1");
            return;
        }
        let backend = backend();
        assert_eq!(backend.kind(), Backend::Local);

        let raw = backend
            .infer(&request(
                probe_model(),
                "Reply with exactly two words and nothing else: mesh online",
            ))
            .await
            .expect("live inference");

        assert!(!raw.text.trim().is_empty(), "model returned no text");
        // The counts come from the server, not from an estimate — which is
        // the whole point of replacing the stub.
        assert!(raw.prompt_tokens > 0, "prompt tokens must be counted");
        assert!(
            raw.completion_tokens > 0,
            "completion tokens must be counted"
        );
        eprintln!(
            "live: {} tokens in / {} out — {:?}",
            raw.prompt_tokens,
            raw.completion_tokens,
            raw.text.chars().take(120).collect::<String>()
        );
    }

    #[tokio::test]
    #[ignore = "requires a running Ollama server and Postgres"]
    async fn gateway_mediated_inference_meters_a_local_row() {
        if !probe_enabled() {
            eprintln!("skipping: set {PROBE_ENV}=1");
            return;
        }
        let url = std::env::var("BUZZ_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .unwrap_or_else(|_| TEST_DB_URL.to_owned());
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");

        let community_uuid = Uuid::new_v4();
        sqlx::query("INSERT INTO communities (id, host) VALUES ($1, $2)")
            .bind(community_uuid)
            .bind(format!("gw-probe-{}.example", community_uuid.simple()))
            .execute(&pool)
            .await
            .expect("insert community");
        let community = CommunityId::from_uuid(community_uuid);

        let user = vec![0xcdu8; 32];
        let model = probe_model();
        let gateway = Gateway::new(Db::from_pool(pool.clone())).with_backend(Box::new(backend()));

        let mut req = request(model.clone(), "Answer in one word: what colour is the sky?");
        req.community_id = community;
        req.user_pubkey = user.clone();

        // An owned-tier agent turn with no explicit backend resolves to
        // Local, and Local is now a real model.
        let resp = gateway
            .route_and_record(&req)
            .await
            .expect("owned-tier local inference");
        assert_eq!(resp.backend, Backend::Local);
        assert!(resp.prompt_tokens > 0 && resp.completion_tokens > 0);
        // Attribution records the id the caller asked for (the prefixed
        // local identity), not the bare tag sent over the wire.
        assert_eq!(resp.model, model);

        let totals = buzz_db::model_usage::user_usage_totals(&pool, community, None)
            .await
            .expect("totals");
        let row = totals
            .iter()
            .find(|t| t.user_pubkey == user)
            .expect("a row for the probe user");
        assert_eq!(row.tier, "owned");
        assert_eq!(row.backend, "local");
        assert_eq!(row.prompt_tokens, resp.prompt_tokens);
        assert_eq!(row.completion_tokens, resp.completion_tokens);
        eprintln!(
            "metered: {}/{} tokens as {}/{}",
            row.prompt_tokens, row.completion_tokens, row.tier, row.backend
        );
    }
}
