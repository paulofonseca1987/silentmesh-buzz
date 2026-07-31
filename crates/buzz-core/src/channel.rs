//! Channel and membership enums shared across crates.
//!
//! These live in `buzz-core` (zero I/O deps) so both the SDK (client-side)
//! and the DB layer (server-side) can use the same types without pulling in
//! sqlx/tokio.

use std::fmt;
use std::str::FromStr;

/// Returns the canonical display name for a channel.
///
/// Channel names are rendered with a leading `#` by clients, so surrounding
/// whitespace and user-supplied hash prefixes are removed here to keep the
/// stored name prefix-free.
pub fn canonical_channel_name(name: &str) -> &str {
    name.trim_start_matches(|c: char| c == '#' || c.is_whitespace())
        .trim_end()
}

/// Whether a channel is publicly visible or invite-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelVisibility {
    /// Searchable; anyone can join without an invite.
    Open,
    /// Hidden; requires an invite to join.
    Private,
}

impl ChannelVisibility {
    /// Canonical string representation (matches DB enum and Nostr tags).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Private => "private",
        }
    }
}

impl fmt::Display for ChannelVisibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChannelVisibility {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "private" => Ok(Self::Private),
            other => Err(format!("unknown channel visibility: {other:?}")),
        }
    }
}

/// The functional type of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelType {
    /// Linear message stream (the default).
    Stream,
    /// Threaded forum-style discussion.
    Forum,
    /// Direct message conversation.
    Dm,
    /// Internal workflow execution channel.
    Workflow,
}

impl ChannelType {
    /// Canonical string representation (matches DB enum and Nostr tags).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Forum => "forum",
            Self::Dm => "dm",
            Self::Workflow => "workflow",
        }
    }
}

impl fmt::Display for ChannelType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ChannelType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stream" => Ok(Self::Stream),
            "forum" => Ok(Self::Forum),
            "dm" => Ok(Self::Dm),
            "workflow" => Ok(Self::Workflow),
            other => Err(format!("unknown channel type: {other:?}")),
        }
    }
}

/// Channel privacy tier (Silent Mesh D21/D24/D26).
///
/// Declared at channel creation and immutable afterwards — the only
/// re-tiering path is an owner-only channel clone. `open` is the loosest
/// tier and the default for channels created without a tier tag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChannelTier {
    /// Client-local or server-GPU inference only; zero egress.
    Owned,
    /// Owned plus TEE-attested providers.
    Private,
    /// Members' own vendor subscriptions allowed (loosest).
    #[default]
    Open,
}

impl ChannelTier {
    /// Wire/DB string for this tier.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelTier::Owned => "owned",
            ChannelTier::Private => "private",
            ChannelTier::Open => "open",
        }
    }

    /// How strict this tier is, ascending: `open` (0) → `private` (1) →
    /// `owned` (2). Only meaningful relative to another tier; see
    /// [`ChannelTier::is_at_least_as_strict_as`].
    pub fn strictness(&self) -> u8 {
        match self {
            ChannelTier::Open => 0,
            ChannelTier::Private => 1,
            ChannelTier::Owned => 2,
        }
    }

    /// Does this tier permit **no more** egress than `other`?
    ///
    /// The comparison content movement has to satisfy (D29/D30): material
    /// may move into a channel only from a space at least as strict, because
    /// a stricter channel's promise is about what has *already* been allowed
    /// to leave. Promoting an `open` thread into an `owned` channel would
    /// import content that may have been sent to a vendor into a space whose
    /// whole guarantee is that nothing in it ever was.
    ///
    /// The reverse — strict into loose — is the ordinary promotion direction,
    /// a deliberate privacy weakening, which is exactly what the D30 gate
    /// review exists to make the member look at first.
    pub fn is_at_least_as_strict_as(&self, other: ChannelTier) -> bool {
        self.strictness() >= other.strictness()
    }
}

impl std::fmt::Display for ChannelTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ChannelTier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owned" => Ok(ChannelTier::Owned),
            "private" => Ok(ChannelTier::Private),
            "open" => Ok(ChannelTier::Open),
            other => Err(format!("invalid channel tier: {other}")),
        }
    }
}

/// A member's role within a channel.
///
/// The hierarchy for permission checks is: Owner > Admin > Member > Guest.
/// Bot is a **separate designation** — it is not part of the linear hierarchy.
/// Use [`MemberRole::permission_level`] for numeric comparisons in authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// Full control — can manage members and delete the channel.
    Owner,
    /// Can manage members and channel settings.
    Admin,
    /// Standard participant.
    Member,
    /// Read-only external participant.
    Guest,
    /// Automated agent or integration (not in the role hierarchy).
    Bot,
}

impl MemberRole {
    /// Canonical string representation (matches DB enum and Nostr tags).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
            Self::Guest => "guest",
            Self::Bot => "bot",
        }
    }

    /// Elevated roles that only existing owners/admins may grant.
    pub fn is_elevated(&self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// Numeric permission level for authorization comparisons.
    ///
    /// Higher = more privileged. Bot returns 0 (must use explicit grants).
    /// Use `role.permission_level() >= required.permission_level()` for checks.
    pub fn permission_level(self) -> u8 {
        match self {
            Self::Owner => 4,
            Self::Admin => 3,
            Self::Member => 2,
            Self::Guest => 1,
            Self::Bot => 0,
        }
    }

    /// Returns true if this role meets or exceeds the required role's permission level.
    ///
    /// Bot never meets any requirement (returns false for all non-Bot requirements).
    pub fn has_at_least(self, required: MemberRole) -> bool {
        self.permission_level() >= required.permission_level()
    }
}

impl fmt::Display for MemberRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemberRole {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Self::Owner),
            "admin" => Ok(Self::Admin),
            "member" => Ok(Self::Member),
            "guest" => Ok(Self::Guest),
            "bot" => Ok(Self::Bot),
            other => Err(format!("unknown member role: {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn tier_strictness_orders_by_permitted_egress() {
        use super::ChannelTier::*;
        assert!(Owned.strictness() > Private.strictness());
        assert!(Private.strictness() > Open.strictness());

        // Equal tiers satisfy each other (promotion within a tier is fine).
        for t in [Owned, Private, Open] {
            assert!(t.is_at_least_as_strict_as(t), "{t} vs itself");
        }
        // Strict → loose is the ordinary promotion direction.
        assert!(Owned.is_at_least_as_strict_as(Open));
        assert!(Owned.is_at_least_as_strict_as(Private));
        assert!(Private.is_at_least_as_strict_as(Open));
        // Loose → strict imports already-egressable content into a space
        // whose guarantee is that nothing in it ever egressed.
        assert!(!Open.is_at_least_as_strict_as(Owned));
        assert!(!Open.is_at_least_as_strict_as(Private));
        assert!(!Private.is_at_least_as_strict_as(Owned));
    }

    use super::canonical_channel_name;

    #[test]
    fn channel_names_trim_whitespace_and_drop_all_leading_hashes() {
        assert_eq!(canonical_channel_name("channel"), "channel");
        assert_eq!(canonical_channel_name("#channel"), "channel");
        assert_eq!(canonical_channel_name("###channel"), "channel");
        assert_eq!(canonical_channel_name("  ###channel  "), "channel");
        assert_eq!(canonical_channel_name("# channel"), "channel");
        assert_eq!(canonical_channel_name("### channel  "), "channel");
        assert_eq!(canonical_channel_name("  ###  "), "");
        assert_eq!(canonical_channel_name("# #"), "");
        assert_eq!(canonical_channel_name("### ###"), "");
        assert_eq!(canonical_channel_name("channel#topic"), "channel#topic");
    }
}
