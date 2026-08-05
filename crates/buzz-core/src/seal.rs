//! Content Seals (Silent Mesh, D31) — the pure half: token format, token
//! and literal detection, and redaction.
//!
//! A seal is the workspace Owner's declaration that one exact literal — a
//! customer name, a key, a codename — may only appear in channels at or
//! above a minimum tier. Everywhere looser, the text carries a **token**
//! that references the seal without containing the value.
//!
//! Two invariants shape everything here, both from the D-log:
//!
//! - **The literal is server-side only.** Events fan out to every member of
//!   every tier, so a literal in any event republishes the value the seal
//!   exists to contain. Matching against literals is therefore something
//!   only the relay does; this module's literal functions are called with
//!   values that came from the relay's own store, never from an event.
//!   Like [`crate::secret_scan`], findings carry offsets and ids — never
//!   the matched text.
//! - **Tokens must survive agent edits, diffs, and merges** (the risk table
//!   names this). So the token is plain ASCII, bracketed, prefix-anchored,
//!   fixed-length: trivial for an agent or a merge to carry verbatim, and
//!   unambiguous to find afterwards.
//!
//! Deterministic and dependency-free, like the secret scanners: the same
//! input always yields the same findings, so the enforcement built on top
//! is auditable.

use crate::channel::ChannelTier;

/// Marker prefix. The `sm-` namespace keeps a collision with organic text
/// implausible without resorting to non-ASCII, which agents and diff tools
/// mangle more readily than brackets.
pub const TOKEN_PREFIX: &str = "[sm-seal:";
/// Marker suffix.
pub const TOKEN_SUFFIX: char = ']';
/// Seal ids are 16 lowercase hex chars (64 bits): short enough to read,
/// wide enough that ids never collide in one workspace's lifetime.
pub const ID_LEN: usize = 16;

/// Render the token for a seal id, e.g. `[sm-seal:0123456789abcdef]`.
pub fn token(id: &str) -> String {
    format!("{TOKEN_PREFIX}{id}{TOKEN_SUFFIX}")
}

/// Is `id` a well-formed seal id (exactly [`ID_LEN`] lowercase hex chars)?
pub fn is_valid_id(id: &str) -> bool {
    id.len() == ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// A token found in text. Carries the id and where it starts — the id is a
/// reference, not a secret, so unlike literal hits it is safe to echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenHit {
    /// The referenced seal's 16-hex id.
    pub id: String,
    /// Byte offset of the token's `[`.
    pub offset: usize,
}

/// Find every well-formed seal token in `text`, in offset order.
///
/// Strict by construction: the id must be exactly [`ID_LEN`] lowercase hex
/// followed by the closing bracket. A malformed near-token (wrong length,
/// uppercase, unterminated) is plain text — rendering it as a seal would
/// invent a reference to nothing, and refusing text over it would let a
/// stray `[sm-seal:` in prose block a message.
pub fn find_tokens(text: &str) -> Vec<TokenHit> {
    let mut hits = Vec::new();
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = find_sub(&bytes[from..], TOKEN_PREFIX.as_bytes()) {
        let start = from + rel;
        let id_start = start + TOKEN_PREFIX.len();
        let id_end = id_start + ID_LEN;
        if id_end < bytes.len()
            && bytes[id_end] == TOKEN_SUFFIX as u8
            && text.get(id_start..id_end).is_some_and(is_valid_id)
        {
            hits.push(TokenHit {
                id: text[id_start..id_end].to_owned(),
                offset: start,
            });
            from = id_end + 1;
        } else {
            // Not a token; keep scanning after the `[` so an overlapping
            // real token is still found.
            from = start + 1;
        }
    }
    hits
}

/// One sealed literal, as loaded from the relay's own store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedLiteral {
    /// The seal's 16-hex id.
    pub id: String,
    /// What refusals call it. Every enforcement point refuses by label —
    /// the label is chosen to be sayable anywhere, the literal is not.
    pub label: String,
    /// The exact value the seal contains. Never from an event.
    pub literal: String,
    /// The loosest tier the literal may appear in.
    pub min_tier: ChannelTier,
}

/// A literal match. Deliberately mirrors [`crate::secret_scan::SecretHit`]:
/// the matched value is **not** carried, because hits travel in refusals and
/// notices, which must never echo the thing being contained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralHit {
    /// The seal whose literal matched.
    pub id: String,
    /// Byte offset of the match.
    pub offset: usize,
}

/// Find every occurrence of every sealed literal in `text`, in offset order
/// (ties by seal id, so the result is deterministic whatever the input
/// order of `seals`).
///
/// Exact, case-sensitive substring matching — D31 says literal matching,
/// and a fuzzier rule would make "does this text leak?" a judgement call
/// instead of a fact. An empty literal never matches: it would otherwise
/// match at every offset, and an empty seal is a registry bug, not a text
/// property.
pub fn find_literals(text: &str, seals: &[SealedLiteral]) -> Vec<LiteralHit> {
    let mut hits = Vec::new();
    for seal in seals {
        if seal.literal.is_empty() {
            continue;
        }
        let needle = seal.literal.as_bytes();
        let bytes = text.as_bytes();
        let mut from = 0;
        while let Some(rel) = find_sub(&bytes[from..], needle) {
            let at = from + rel;
            hits.push(LiteralHit {
                id: seal.id.clone(),
                offset: at,
            });
            from = at + needle.len();
        }
    }
    hits.sort_by(|a, b| a.offset.cmp(&b.offset).then_with(|| a.id.cmp(&b.id)));
    hits
}

/// Which seals forbid their literal in a channel of `tier`?
///
/// A literal is permitted where the channel is at least as strict as the
/// seal's minimum, and contained everywhere looser. Split out so ingest and
/// the gate apply the same rule by calling it, not by re-deriving it.
pub fn violating_seals(seals: &[SealedLiteral], tier: ChannelTier) -> Vec<&SealedLiteral> {
    seals
        .iter()
        .filter(|s| !tier.is_at_least_as_strict_as(s.min_tier))
        .collect()
}

/// Replace every occurrence of every sealed literal with its token.
/// Returns the redacted text and how many replacements were made.
///
/// Longest literal first: if one seal's literal contains another's
/// ("Acme Corp Zurich" and "Acme Corp"), replacing the shorter one first
/// would leave fragments of the longer one behind — redacted-looking text
/// still carrying half the value. Ties broken by id so the output is
/// deterministic.
pub fn redact(text: &str, seals: &[SealedLiteral]) -> (String, usize) {
    let mut ordered: Vec<&SealedLiteral> = seals.iter().filter(|s| !s.literal.is_empty()).collect();
    ordered.sort_by(|a, b| {
        b.literal
            .len()
            .cmp(&a.literal.len())
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut out = text.to_owned();
    let mut count = 0;
    for seal in ordered {
        let replaced = out.matches(&seal.literal).count();
        if replaced > 0 {
            out = out.replace(&seal.literal, &token(&seal.id));
            count += replaced;
        }
    }
    (out, count)
}

/// First occurrence of `needle` in `haystack`, byte-wise.
///
/// Byte search rather than `str::find` so offsets are byte offsets by
/// construction and slicing never lands inside a multi-byte character —
/// the same reasoning, and the same bug class, as the diff truncation in
/// `buzz-acp`, where a char-index assumption panicked on accented text.
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(id: &str, literal: &str, min_tier: ChannelTier) -> SealedLiteral {
        SealedLiteral {
            id: id.to_owned(),
            label: format!("label-{id}"),
            literal: literal.to_owned(),
            min_tier,
        }
    }

    const ID_A: &str = "0123456789abcdef";
    const ID_B: &str = "fedcba9876543210";

    #[test]
    fn token_round_trips_through_find() {
        let text = format!("before {} after", token(ID_A));
        let hits = find_tokens(&text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, ID_A);
        assert_eq!(hits[0].offset, "before ".len());
    }

    /// Malformed near-tokens are plain text. Rendering one as a seal would
    /// invent a reference to nothing; refusing text over one would let a
    /// stray `[sm-seal:` in prose block a message.
    #[test]
    fn malformed_tokens_are_not_tokens() {
        for bad in [
            "[sm-seal:short]",
            "[sm-seal:0123456789ABCDEF]",   // uppercase
            "[sm-seal:0123456789abcdef",    // unterminated
            "[sm-seal:0123456789abcdefff]", // too long
            "[sm-seal:0123456789abcdeg]",   // non-hex
            "sm-seal:0123456789abcdef]",    // no opening bracket
        ] {
            assert!(find_tokens(bad).is_empty(), "{bad:?} must not parse");
        }
        // And a malformed one must not eat a real one after it.
        let text = format!("[sm-seal:nope] then {}", token(ID_B));
        let hits = find_tokens(&text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, ID_B);
    }

    #[test]
    fn literals_are_found_at_every_occurrence_never_echoed() {
        let seals = [seal(ID_A, "AcmeCorp", ChannelTier::Private)];
        let hits = find_literals("AcmeCorp met AcmeCorp", &seals);
        assert_eq!(
            hits,
            vec![
                LiteralHit {
                    id: ID_A.into(),
                    offset: 0
                },
                LiteralHit {
                    id: ID_A.into(),
                    offset: 13
                },
            ]
        );
    }

    /// An empty literal is a registry bug, not a property of every text.
    #[test]
    fn an_empty_literal_never_matches() {
        let seals = [seal(ID_A, "", ChannelTier::Owned)];
        assert!(find_literals("anything at all", &seals).is_empty());
        let (out, n) = redact("anything", &seals);
        assert_eq!((out.as_str(), n), ("anything", 0));
    }

    /// Offsets are byte offsets, valid next to multi-byte characters — the
    /// bug class the diff truncation hit, pinned here on purpose.
    #[test]
    fn offsets_survive_multibyte_neighbours() {
        let seals = [seal(ID_A, "secret", ChannelTier::Owned)];
        let text = "café ﬁ secret café";
        let hits = find_literals(text, &seals);
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].offset..hits[0].offset + 6], "secret");
    }

    /// The containment case redaction exists to get right: a seal whose
    /// literal contains another's. Shorter-first would leave fragments of
    /// the longer literal — redacted-looking text still carrying half the
    /// value.
    #[test]
    fn redaction_replaces_longest_literals_first() {
        let seals = [
            seal(ID_A, "Acme Corp", ChannelTier::Private),
            seal(ID_B, "Acme Corp Zurich", ChannelTier::Owned),
        ];
        let (out, n) = redact("deal with Acme Corp Zurich signed", &seals);
        assert_eq!(n, 1, "one replacement, not a nested double");
        assert_eq!(out, format!("deal with {} signed", token(ID_B)));
        assert!(
            !out.contains("Zurich"),
            "no fragment of the longer literal may survive"
        );
    }

    #[test]
    fn redacted_text_contains_no_literals_and_scans_clean() {
        let seals = [
            seal(ID_A, "alpha-key-123", ChannelTier::Owned),
            seal(ID_B, "Beta Client", ChannelTier::Private),
        ];
        let (out, n) = redact("alpha-key-123 for Beta Client, again alpha-key-123", &seals);
        assert_eq!(n, 3);
        assert!(
            find_literals(&out, &seals).is_empty(),
            "redaction is complete"
        );
        assert_eq!(
            find_tokens(&out).len(),
            3,
            "every replacement is a real token"
        );
    }

    /// The tier rule, spelled out: permitted at-or-above the minimum,
    /// contained everywhere looser.
    #[test]
    fn violation_follows_tier_strictness() {
        let seals = [
            seal(ID_A, "x", ChannelTier::Owned),
            seal(ID_B, "y", ChannelTier::Private),
        ];
        let ids = |tier| -> Vec<String> {
            violating_seals(&seals, tier)
                .iter()
                .map(|s| s.id.clone())
                .collect()
        };
        // An owned channel is strict enough for everything.
        assert!(ids(ChannelTier::Owned).is_empty());
        // A private channel violates the owned-only seal.
        assert_eq!(ids(ChannelTier::Private), vec![ID_A.to_owned()]);
        // An open channel violates both.
        assert_eq!(
            ids(ChannelTier::Open),
            vec![ID_A.to_owned(), ID_B.to_owned()]
        );
    }

    #[test]
    fn id_validation_is_exact() {
        assert!(is_valid_id(ID_A));
        assert!(!is_valid_id("0123456789ABCDEF"));
        assert!(!is_valid_id("0123456789abcde"));
        assert!(!is_valid_id("0123456789abcdef0"));
        assert!(!is_valid_id(""));
    }
}
