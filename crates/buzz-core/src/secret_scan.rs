//! Deterministic secret scanners — the Privacy Gate scaffold (Silent Mesh
//! Phase 2g, D30).
//!
//! A small, dependency-free rule set that flags obviously-credential-shaped
//! strings in text before any privacy-weakening movement (thread promotion
//! out of a personal channel). Deterministic by design: the same input
//! always produces the same findings, so the gate is auditable and
//! testable. Model-assisted review (summaries, flagged spans, suggested
//! obfuscations) arrives in Phase 3 — these scanners remain the hard
//! backstop underneath it.
//!
//! Rules are prefix/shape matchers, not regexes, to keep buzz-core free of
//! new dependencies and the behavior easy to audit. False negatives are
//! accepted (the gate is a backstop, not a guarantee); rules are chosen to
//! keep false positives rare.

/// One secret-scan finding: the rule that matched and the byte offset of
/// the match start. The matched value itself is deliberately **not**
/// carried — findings travel in error messages and notices, which must
/// never echo the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretHit {
    /// Stable rule identifier (e.g. `aws-access-key-id`).
    pub rule: &'static str,
    /// Byte offset of the match in the scanned text.
    pub offset: usize,
}

/// Is `c` a character that can appear in a base62 token body?
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// Count consecutive chars from `text` satisfying `pred`.
fn run_len(text: &str, pred: fn(char) -> bool) -> usize {
    text.chars().take_while(|c| pred(*c)).count()
}

/// A prefix-anchored token rule: `prefix` followed by at least `min_body`
/// chars matching `body`.
struct PrefixRule {
    rule: &'static str,
    prefix: &'static str,
    min_body: usize,
    body: fn(char) -> bool,
}

const PREFIX_RULES: &[PrefixRule] = &[
    PrefixRule {
        rule: "aws-access-key-id",
        prefix: "AKIA",
        min_body: 16,
        body: |c| c.is_ascii_uppercase() || c.is_ascii_digit(),
    },
    PrefixRule {
        rule: "github-token",
        prefix: "ghp_",
        min_body: 36,
        body: is_token_char,
    },
    PrefixRule {
        rule: "github-token",
        prefix: "gho_",
        min_body: 36,
        body: is_token_char,
    },
    PrefixRule {
        rule: "github-token",
        prefix: "ghu_",
        min_body: 36,
        body: is_token_char,
    },
    PrefixRule {
        rule: "github-token",
        prefix: "ghs_",
        min_body: 36,
        body: is_token_char,
    },
    PrefixRule {
        rule: "github-token",
        prefix: "ghr_",
        min_body: 36,
        body: is_token_char,
    },
    PrefixRule {
        rule: "github-fine-grained-pat",
        prefix: "github_pat_",
        min_body: 22,
        body: |c| c.is_ascii_alphanumeric() || c == '_',
    },
    PrefixRule {
        rule: "slack-token",
        prefix: "xoxb-",
        min_body: 10,
        body: |c| c.is_ascii_alphanumeric() || c == '-',
    },
    PrefixRule {
        rule: "slack-token",
        prefix: "xoxp-",
        min_body: 10,
        body: |c| c.is_ascii_alphanumeric() || c == '-',
    },
    PrefixRule {
        rule: "slack-token",
        prefix: "xoxs-",
        min_body: 10,
        body: |c| c.is_ascii_alphanumeric() || c == '-',
    },
    PrefixRule {
        rule: "stripe-secret-key",
        prefix: "sk_live_",
        min_body: 16,
        body: is_token_char,
    },
    PrefixRule {
        rule: "stripe-restricted-key",
        prefix: "rk_live_",
        min_body: 16,
        body: is_token_char,
    },
    PrefixRule {
        rule: "google-api-key",
        prefix: "AIza",
        min_body: 35,
        body: |c| c.is_ascii_alphanumeric() || c == '_' || c == '-',
    },
    PrefixRule {
        rule: "anthropic-api-key",
        prefix: "sk-ant-",
        min_body: 20,
        body: |c| c.is_ascii_alphanumeric() || c == '_' || c == '-',
    },
    PrefixRule {
        rule: "openai-api-key",
        prefix: "sk-proj-",
        min_body: 20,
        body: |c| c.is_ascii_alphanumeric() || c == '_' || c == '-',
    },
    PrefixRule {
        rule: "npm-token",
        prefix: "npm_",
        min_body: 36,
        body: is_token_char,
    },
];

/// Scan `text` for credential-shaped values. Returns every finding, in
/// offset order; an empty vec means the scan passed.
///
/// Deterministic and allocation-light: one pass per rule family over the
/// input. Intended for the Privacy Gate scaffold — the member-written
/// promotion summary and each text file in the promoted tree.
pub fn scan_text(text: &str) -> Vec<SecretHit> {
    let mut hits = Vec::new();

    for rule in PREFIX_RULES {
        let mut search_from = 0usize;
        while let Some(rel) = text[search_from..].find(rule.prefix) {
            let at = search_from + rel;
            let body = &text[at + rule.prefix.len()..];
            if run_len(body, rule.body) >= rule.min_body {
                hits.push(SecretHit {
                    rule: rule.rule,
                    offset: at,
                });
            }
            search_from = at + rule.prefix.len();
        }
    }

    // PEM private-key blocks: `-----BEGIN <anything> PRIVATE KEY-----`.
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("-----BEGIN ") {
        let at = search_from + rel;
        let line_end = text[at..].find('\n').map(|i| at + i).unwrap_or(text.len());
        if text[at..line_end].contains("PRIVATE KEY-----") {
            hits.push(SecretHit {
                rule: "private-key-block",
                offset: at,
            });
        }
        search_from = at + "-----BEGIN ".len();
    }

    // JWTs: three dot-separated base64url segments, the first two starting
    // with `eyJ` (`{"` base64-encoded) — the canonical JOSE shape.
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find("eyJ") {
        let at = search_from + rel;
        let seg = &text[at..];
        let b64 = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
        let first = run_len(seg, b64);
        let rest = &seg[first..];
        if first >= 8 && rest.starts_with('.') && rest[1..].starts_with("eyJ") {
            let second = run_len(&rest[1..], b64);
            let tail = &rest[1 + second..];
            if second >= 8 && tail.starts_with('.') && run_len(&tail[1..], b64) >= 8 {
                hits.push(SecretHit {
                    rule: "jwt",
                    offset: at,
                });
            }
        }
        search_from = at + 3;
    }

    hits.sort_by_key(|h| h.offset);
    hits
}

/// Does `bytes` look like binary content? (NUL byte in the sniff window —
/// the same heuristic git uses.) Binary blobs are skipped by the gate's
/// file scan and documented as out of the scaffold's scope.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Vec<&'static str> {
        scan_text(text).into_iter().map(|h| h.rule).collect()
    }

    #[test]
    fn flags_the_canonical_credential_shapes() {
        assert_eq!(
            rules("key = AKIAIOSFODNN7EXAMPLE"),
            vec!["aws-access-key-id"]
        );
        assert_eq!(
            rules(&format!("token: ghp_{}", "a1B2".repeat(9))),
            vec!["github-token"]
        );
        assert_eq!(
            rules(&format!("github_pat_{}", "x".repeat(30))),
            vec!["github-fine-grained-pat"]
        );
        assert_eq!(rules("xoxb-1234567890-abcdef"), vec!["slack-token"]);
        assert_eq!(
            rules(&format!("sk_live_{}", "a".repeat(24))),
            vec!["stripe-secret-key"]
        );
        assert_eq!(
            rules(&format!("AIza{}", "a".repeat(35))),
            vec!["google-api-key"]
        );
        assert_eq!(
            rules(&format!("sk-ant-api03-{}", "a".repeat(20))),
            vec!["anthropic-api-key"]
        );
        assert_eq!(
            rules("-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n"),
            vec!["private-key-block"]
        );
        assert_eq!(
            rules("-----BEGIN RSA PRIVATE KEY-----\nabc\n"),
            vec!["private-key-block"]
        );
        assert_eq!(
            rules("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.dBjftJeZ4CVP"),
            vec!["jwt"]
        );
    }

    #[test]
    fn clean_text_passes() {
        assert!(scan_text("ship the parser fix; see notes.md").is_empty());
        // Shapes that ALMOST match must not trip the gate.
        assert!(scan_text("AKIA-short").is_empty());
        assert!(scan_text("ghp_tooshort").is_empty());
        assert!(scan_text("-----BEGIN CERTIFICATE-----").is_empty());
        assert!(scan_text("eyJx.notajwt").is_empty());
        assert!(scan_text("sk_test_1234567890abcdef1234").is_empty());
        assert!(scan_text("the words skip_liveness_check pass").is_empty());
    }

    #[test]
    fn multiple_findings_report_in_offset_order() {
        let text = format!("a = AKIAIOSFODNN7EXAMPLE\nb = sk_live_{}\n", "a".repeat(20));
        let hits = scan_text(&text);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].offset < hits[1].offset);
        assert_eq!(hits[0].rule, "aws-access-key-id");
        assert_eq!(hits[1].rule, "stripe-secret-key");
    }

    #[test]
    fn binary_sniff() {
        assert!(looks_binary(b"\x89PNG\r\n\x1a\n\x00\x00"));
        assert!(!looks_binary(b"plain text file\nwith lines\n"));
    }
}
