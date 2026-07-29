//! Folder/file write ACLs (Silent Mesh Phase 2h, D4).
//!
//! Upstream's `buzz-protect` tags constrain *which refs* a role may
//! update. D4 adds the orthogonal axis: **which paths** inside the tree a
//! given member may write. Rules ride the same kind:30617 announcement as
//! `buzz-protect`, so a channel's repo carries its own ACLs and the
//! pre-receive hook (plus, later, the agent tool layer) enforces the same
//! parsed rules.
//!
//! Tag format:
//!
//! ```text
//! ["buzz-path-acl", "<path-pattern>", "<rule>", ...]
//! ```
//!
//! Rules:
//! - `write:<role>` — minimum channel role that may write these paths
//!   (`owner`, `admin`, `member`).
//! - `write:<64-hex pubkey>` — an explicit writer allowlist; repeatable.
//!   When any pubkey rule matches a path, only listed pubkeys (plus anyone
//!   satisfying a `write:<role>` rule on the same pattern) may write it.
//! - `readonly` — nobody may write these paths through a push; the only
//!   writer is the relay itself (canonicalization, promotion grafts).
//!
//! Matching is **most-specific-wins**: the matching pattern with the most
//! literal segments decides. Ties union their requirements (all must pass),
//! which keeps two equally-specific rules from silently disagreeing.
//!
//! The read boundary is unchanged and deliberately coarse (D4): read =
//! channel membership. These rules govern writes only.

use crate::channel::MemberRole;
use std::fmt;

/// Maximum number of `buzz-path-acl` tags per repo.
pub const MAX_PATH_ACL_RULES: usize = 50;
/// Maximum character length of a path pattern.
pub const MAX_PATH_PATTERN_LENGTH: usize = 256;
/// Maximum number of changed paths the hook may report before the policy
/// endpoint refuses to decide (fail-closed when ACLs exist).
pub const MAX_REPORTED_PATHS: usize = 5000;

/// A validated repository path pattern.
///
/// Grammar: `segment ("/" segment)*` where a segment is a literal
/// `[a-zA-Z0-9._-]+`, `*` (exactly one segment), or `**` (one or more
/// trailing segments, last position only). Unlike [`crate::git_perms`]
/// ref patterns there is no required prefix — these match worktree paths
/// like `canon/**` or `src/*/secrets.rs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    raw: String,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Wildcard,
    RecursiveWildcard,
}

/// Errors from parsing a path pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPatternError {
    /// Pattern is empty.
    Empty,
    /// Pattern exceeds the maximum length.
    TooLong,
    /// A segment is empty, a partial glob, or contains invalid characters.
    InvalidSegment(String),
    /// Pattern is absolute or contains a `..` traversal.
    Unsafe,
}

impl fmt::Display for PathPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "path pattern is empty"),
            Self::TooLong => write!(f, "path pattern exceeds {MAX_PATH_PATTERN_LENGTH} chars"),
            Self::InvalidSegment(s) => write!(f, "invalid path segment: {s:?}"),
            Self::Unsafe => write!(f, "path pattern must be relative and free of '..'"),
        }
    }
}

impl std::error::Error for PathPatternError {}

impl PathPattern {
    /// Parse and validate a path pattern.
    pub fn parse(pattern: &str) -> Result<Self, PathPatternError> {
        if pattern.is_empty() {
            return Err(PathPatternError::Empty);
        }
        if pattern.len() > MAX_PATH_PATTERN_LENGTH {
            return Err(PathPatternError::TooLong);
        }
        if pattern.starts_with('/') || pattern.split('/').any(|s| s == "..") {
            return Err(PathPatternError::Unsafe);
        }

        let parts: Vec<&str> = pattern.split('/').collect();
        let mut segments = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            if *part == "**" {
                if i != parts.len() - 1 {
                    return Err(PathPatternError::InvalidSegment(
                        "** must be the last segment".to_owned(),
                    ));
                }
                segments.push(Segment::RecursiveWildcard);
            } else if *part == "*" {
                segments.push(Segment::Wildcard);
            } else if part.is_empty() {
                return Err(PathPatternError::InvalidSegment(String::new()));
            } else if part.contains('*')
                || part.contains('?')
                || part.contains('[')
                || !part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            {
                return Err(PathPatternError::InvalidSegment((*part).to_owned()));
            } else {
                segments.push(Segment::Literal((*part).to_owned()));
            }
        }
        Ok(Self {
            raw: pattern.to_owned(),
            segments,
        })
    }

    /// The original pattern string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Number of literal segments — the specificity score used to pick the
    /// governing rule when several patterns match.
    pub fn specificity(&self) -> usize {
        self.segments
            .iter()
            .filter(|s| matches!(s, Segment::Literal(_)))
            .count()
    }

    /// Does this pattern match `path` (a repo-relative path)?
    pub fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        let mut pi = 0usize;
        for (si, seg) in self.segments.iter().enumerate() {
            match seg {
                Segment::RecursiveWildcard => {
                    // Trailing `**` matches one or more remaining segments.
                    return parts.len() > pi || si == 0;
                }
                Segment::Wildcard => {
                    if pi >= parts.len() {
                        return false;
                    }
                    pi += 1;
                }
                Segment::Literal(lit) => {
                    if pi >= parts.len() || parts[pi] != lit {
                        return false;
                    }
                    pi += 1;
                }
            }
        }
        pi == parts.len()
    }
}

/// A single write ACL parsed from a `buzz-path-acl` tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAclRule {
    /// The paths this rule governs.
    pub pattern: PathPattern,
    /// Minimum channel role allowed to write (if specified).
    pub write_role: Option<MemberRole>,
    /// Explicit writer allowlist (lowercase 64-hex pubkeys).
    pub write_pubkeys: Vec<String>,
    /// No push may write these paths at all.
    pub readonly: bool,
}

/// Errors from parsing a `buzz-path-acl` tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathAclParseError {
    /// Tag has fewer than 2 values (pattern + at least one rule).
    TooFewValues,
    /// More than [`MAX_PATH_ACL_RULES`] rules on this repo.
    TooManyRules,
    /// The path pattern is invalid.
    InvalidPattern(PathPatternError),
    /// A `write:` value is neither a known role nor a 64-hex pubkey.
    InvalidWriteTarget(String),
    /// A rule string is not recognized.
    UnknownRule(String),
}

impl fmt::Display for PathAclParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooFewValues => {
                write!(f, "buzz-path-acl tag needs a pattern + at least one rule")
            }
            Self::TooManyRules => write!(f, "exceeds max {MAX_PATH_ACL_RULES} path ACLs per repo"),
            Self::InvalidPattern(e) => write!(f, "invalid path pattern: {e}"),
            Self::InvalidWriteTarget(v) => {
                write!(f, "write: must name a role or 64-hex pubkey (got {v:?})")
            }
            Self::UnknownRule(r) => write!(f, "unknown path-acl rule: {r:?}"),
        }
    }
}

impl std::error::Error for PathAclParseError {}

/// Result of parsing the `buzz-path-acl` tags on a repo announcement.
#[derive(Debug, Clone, Default)]
pub struct ParsedPathAcls {
    /// Successfully parsed rules.
    pub rules: Vec<PathAclRule>,
    /// Unknown rule strings, skipped but reported so callers can warn.
    pub unknown_rules: Vec<String>,
}

fn is_lower_hex64(v: &str) -> bool {
    v.len() == 64
        && v.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Parse one `buzz-path-acl` tag's values (the tag name already stripped).
pub fn parse_path_acl_tag(
    values: &[&str],
) -> Result<(PathAclRule, Vec<String>), PathAclParseError> {
    if values.len() < 2 {
        return Err(PathAclParseError::TooFewValues);
    }
    let pattern = PathPattern::parse(values[0]).map_err(PathAclParseError::InvalidPattern)?;
    let mut rule = PathAclRule {
        pattern,
        write_role: None,
        write_pubkeys: Vec::new(),
        readonly: false,
    };
    let mut unknown = Vec::new();
    for value in &values[1..] {
        if let Some(target) = value.strip_prefix("write:") {
            if is_lower_hex64(target) {
                rule.write_pubkeys.push(target.to_owned());
            } else {
                match target.parse::<MemberRole>() {
                    Ok(role) => {
                        // Keep the strictest role if repeated.
                        rule.write_role = Some(match rule.write_role {
                            Some(existing) if role_rank(existing) > role_rank(role) => existing,
                            _ => role,
                        });
                    }
                    Err(_) => {
                        return Err(PathAclParseError::InvalidWriteTarget(target.to_owned()));
                    }
                }
            }
        } else if *value == "readonly" {
            rule.readonly = true;
        } else {
            unknown.push((*value).to_owned());
        }
    }
    if rule.write_role.is_none() && rule.write_pubkeys.is_empty() && !rule.readonly {
        return Err(PathAclParseError::UnknownRule(
            values[1..].join(",").to_string(),
        ));
    }
    Ok((rule, unknown))
}

/// Rank roles for "at least this role" comparisons. Bot is deliberately
/// ranked with Member: it is a designation, not a permission tier (the
/// caller maps Bot → Member before evaluating, mirroring the git-role
/// mapping in the push policy).
fn role_rank(role: MemberRole) -> u8 {
    match role {
        MemberRole::Owner => 3,
        MemberRole::Admin => 2,
        MemberRole::Member | MemberRole::Bot => 1,
        MemberRole::Guest => 0,
    }
}

/// Parse every `buzz-path-acl` tag on a kind:30617 event.
pub fn parse_path_acl_tags(tags: &[Vec<String>]) -> Result<ParsedPathAcls, PathAclParseError> {
    let mut rules = Vec::new();
    let mut unknown_rules = Vec::new();
    for tag in tags {
        if tag.first().map(|s| s.as_str()) != Some("buzz-path-acl") {
            continue;
        }
        if rules.len() >= MAX_PATH_ACL_RULES {
            return Err(PathAclParseError::TooManyRules);
        }
        let values: Vec<&str> = tag[1..].iter().map(|s| s.as_str()).collect();
        let (rule, unknowns) = parse_path_acl_tag(&values)?;
        rules.push(rule);
        unknown_rules.extend(unknowns);
    }
    Ok(ParsedPathAcls {
        rules,
        unknown_rules,
    })
}

/// A refused path write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDenial {
    /// The path that was refused.
    pub path: String,
    /// Human-readable reason.
    pub reason: String,
}

impl fmt::Display for PathDenial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.reason)
    }
}

/// Evaluate write ACLs for the paths a push touches.
///
/// `role` is the pusher's effective git role (Bot already mapped to
/// Member by the caller); `pusher_hex` is their lowercase hex pubkey.
/// Paths with no matching rule are unrestricted — ACLs are opt-in per
/// repo, matching `buzz-protect` semantics.
///
/// Returns every denial (bounded) so the pusher sees all offending paths
/// in one round trip rather than one per retry.
pub fn evaluate_path_writes(
    paths: &[String],
    role: MemberRole,
    pusher_hex: &str,
    rules: &[PathAclRule],
) -> Result<(), Vec<PathDenial>> {
    if rules.is_empty() {
        return Ok(());
    }
    let mut denials = Vec::new();
    for path in paths {
        // Most-specific-wins; ties union (all tied rules must pass).
        let mut best: Option<usize> = None;
        let mut governing: Vec<&PathAclRule> = Vec::new();
        for rule in rules {
            if !rule.pattern.matches(path) {
                continue;
            }
            let spec = rule.pattern.specificity();
            match best {
                Some(b) if spec < b => {}
                Some(b) if spec == b => governing.push(rule),
                _ => {
                    best = Some(spec);
                    governing.clear();
                    governing.push(rule);
                }
            }
        }
        for rule in governing {
            if rule.readonly {
                denials.push(PathDenial {
                    path: path.clone(),
                    reason: format!("path is read-only ({})", rule.pattern.as_str()),
                });
                continue;
            }
            let role_ok = rule
                .write_role
                .is_some_and(|required| role_rank(role) >= role_rank(required));
            let pubkey_ok = rule.write_pubkeys.iter().any(|pk| pk == pusher_hex);
            if !role_ok && !pubkey_ok {
                let mut needed: Vec<String> = Vec::new();
                if let Some(required) = rule.write_role {
                    needed.push(format!("role {}", required.as_str()));
                }
                if !rule.write_pubkeys.is_empty() {
                    needed.push(format!(
                        "{} allowlisted writer(s)",
                        rule.write_pubkeys.len()
                    ));
                }
                denials.push(PathDenial {
                    path: path.clone(),
                    reason: format!(
                        "write denied by path ACL {} (requires {})",
                        rule.pattern.as_str(),
                        needed.join(" or ")
                    ),
                });
            }
        }
    }
    if denials.is_empty() {
        Ok(())
    } else {
        // Bound the response: the first few denials tell the pusher what to
        // fix without shipping a wall of text.
        denials.truncate(20);
        Err(denials)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(values: &[&str]) -> PathAclRule {
        parse_path_acl_tag(values).expect("valid rule").0
    }

    #[test]
    fn pattern_matching() {
        let p = PathPattern::parse("canon/**").expect("valid");
        assert!(p.matches("canon/a.md"));
        assert!(p.matches("canon/deep/nested/file.txt"));
        assert!(!p.matches("canon"));
        assert!(!p.matches("other/a.md"));

        let p = PathPattern::parse("src/*/secrets.rs").expect("valid");
        assert!(p.matches("src/auth/secrets.rs"));
        assert!(!p.matches("src/secrets.rs"));
        assert!(!p.matches("src/a/b/secrets.rs"));

        let p = PathPattern::parse("deploy.yaml").expect("valid");
        assert!(p.matches("deploy.yaml"));
        assert!(!p.matches("infra/deploy.yaml"));
    }

    #[test]
    fn unsafe_and_malformed_patterns_are_rejected() {
        assert_eq!(PathPattern::parse(""), Err(PathPatternError::Empty));
        assert_eq!(
            PathPattern::parse("/etc/passwd"),
            Err(PathPatternError::Unsafe)
        );
        assert_eq!(
            PathPattern::parse("../escape"),
            Err(PathPatternError::Unsafe)
        );
        assert!(matches!(
            PathPattern::parse("src/**/deep"),
            Err(PathPatternError::InvalidSegment(_))
        ));
        assert!(matches!(
            PathPattern::parse("src/pre*fix"),
            Err(PathPatternError::InvalidSegment(_))
        ));
        assert!(matches!(
            PathPattern::parse(&"a".repeat(300)),
            Err(PathPatternError::TooLong)
        ));
    }

    #[test]
    fn rule_parsing() {
        let r = rule(&["canon/**", "readonly"]);
        assert!(r.readonly && r.write_role.is_none());

        let r = rule(&["infra/**", "write:admin"]);
        assert_eq!(r.write_role, Some(MemberRole::Admin));

        let pk = "a".repeat(64);
        let r = rule(&["infra/**", "write:admin", &format!("write:{pk}")]);
        assert_eq!(r.write_role, Some(MemberRole::Admin));
        assert_eq!(r.write_pubkeys, vec![pk]);

        // Repeated roles keep the strictest.
        let r = rule(&["x/**", "write:member", "write:owner"]);
        assert_eq!(r.write_role, Some(MemberRole::Owner));

        assert_eq!(
            parse_path_acl_tag(&["only-pattern"]),
            Err(PathAclParseError::TooFewValues)
        );
        assert!(matches!(
            parse_path_acl_tag(&["p/**", "write:nonsense"]),
            Err(PathAclParseError::InvalidWriteTarget(_))
        ));
        // A tag with only unknown rules is malformed (fail closed) rather
        // than silently permissive.
        assert!(matches!(
            parse_path_acl_tag(&["p/**", "someday-rule"]),
            Err(PathAclParseError::UnknownRule(_))
        ));
    }

    #[test]
    fn evaluation_role_and_allowlist() {
        let pk = "b".repeat(64);
        let other = "c".repeat(64);
        let rules = vec![
            rule(&["infra/**", "write:admin"]),
            rule(&["docs/**", &format!("write:{pk}")]),
            rule(&["canon/**", "readonly"]),
        ];

        // No matching rule → unrestricted.
        assert!(
            evaluate_path_writes(&["src/main.rs".into()], MemberRole::Member, &other, &rules)
                .is_ok()
        );

        // Role gate.
        assert!(evaluate_path_writes(
            &["infra/deploy.yaml".into()],
            MemberRole::Admin,
            &other,
            &rules
        )
        .is_ok());
        let denied = evaluate_path_writes(
            &["infra/deploy.yaml".into()],
            MemberRole::Member,
            &other,
            &rules,
        )
        .expect_err("member cannot write infra");
        assert_eq!(denied.len(), 1);
        assert!(denied[0].reason.contains("requires role admin"));

        // Explicit allowlist beats role.
        assert!(
            evaluate_path_writes(&["docs/x.md".into()], MemberRole::Member, &pk, &rules).is_ok()
        );
        assert!(
            evaluate_path_writes(&["docs/x.md".into()], MemberRole::Member, &other, &rules)
                .is_err()
        );

        // readonly denies everyone, including the owner.
        assert!(
            evaluate_path_writes(&["canon/a.md".into()], MemberRole::Owner, &other, &rules)
                .is_err()
        );

        // Empty rule set is a no-op.
        assert!(
            evaluate_path_writes(&["anything".into()], MemberRole::Member, &other, &[]).is_ok()
        );
    }

    #[test]
    fn most_specific_rule_wins_and_ties_union() {
        let rules = vec![
            rule(&["src/**", "write:member"]),
            rule(&["src/infra/**", "write:admin"]),
        ];
        // The deeper pattern governs: a member is refused there…
        assert!(evaluate_path_writes(
            &["src/infra/a.tf".into()],
            MemberRole::Member,
            "d".repeat(64).as_str(),
            &rules
        )
        .is_err());
        // …but allowed elsewhere under src/.
        assert!(evaluate_path_writes(
            &["src/app/main.rs".into()],
            MemberRole::Member,
            "d".repeat(64).as_str(),
            &rules
        )
        .is_ok());

        // Equally specific rules union: both must pass.
        let pk = "e".repeat(64);
        let tied = vec![
            rule(&["a/*", "write:member"]),
            rule(&["a/*", &format!("write:{pk}")]),
        ];
        assert!(evaluate_path_writes(&["a/x".into()], MemberRole::Member, &pk, &tied).is_ok());
        assert!(evaluate_path_writes(
            &["a/x".into()],
            MemberRole::Member,
            "f".repeat(64).as_str(),
            &tied
        )
        .is_err());
    }

    #[test]
    fn parse_tags_from_announcement() {
        let tags = vec![
            vec!["d".into(), "repo".into()],
            vec!["buzz-path-acl".into(), "canon/**".into(), "readonly".into()],
            vec![
                "buzz-path-acl".into(),
                "infra/**".into(),
                "write:admin".into(),
                "future-rule".into(),
            ],
        ];
        let parsed = parse_path_acl_tags(&tags).expect("parse");
        assert_eq!(parsed.rules.len(), 2);
        assert_eq!(parsed.unknown_rules, vec!["future-rule".to_owned()]);
    }
}
