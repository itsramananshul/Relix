//! Derived identity model for Slack users — namespaced under
//! `slack:` so it can't collide with Telegram or Discord
//! derivations on the same numeric / string id.
//!
//! Slack users do not have Relix IdentityBundles. The channel
//! mints a derived subject per `(channel_id, user_id)` pair by
//! hashing `"slack:" + user_id + ":" + channel_id` with blake3
//! and using the 32-byte result as the subject id. The
//! Coordinator stores this as `owner_subject_id` on tasks so
//! operators can query "all tasks for Slack user U0123" later.

use relix_core::types::NodeId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSubject {
    pub channel: &'static str,
    pub channel_id: String,
    pub user_id: String,
    pub subject_id: NodeId,
}

impl ChannelSubject {
    /// `slack:<user_id>@<channel_id>:<hex16>` — first 16 hex chars
    /// of the subject id are enough for at-a-glance
    /// disambiguation.
    pub fn display_handle(&self) -> String {
        let hex = self.subject_id.to_string();
        let prefix = &hex[..hex.len().min(16)];
        format!(
            "{}:{}@{}:{}",
            self.channel, self.user_id, self.channel_id, prefix
        )
    }
}

/// Derive the per-user `ChannelSubject` for a Slack message.
/// Deterministic: same `(channel_id, user_id)` always returns the
/// same subject_id, across restarts.
pub fn derive_channel_subject(channel_id: &str, user_id: &str) -> ChannelSubject {
    let input = format!("slack:{user_id}:{channel_id}");
    let hash = blake3::hash(input.as_bytes());
    let bytes = hash.as_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    ChannelSubject {
        channel: "slack",
        channel_id: channel_id.to_string(),
        user_id: user_id.to_string(),
        subject_id: NodeId(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let a = derive_channel_subject("C0", "U0");
        let b = derive_channel_subject("C0", "U0");
        assert_eq!(a.subject_id, b.subject_id);
    }

    #[test]
    fn different_channels_yield_different_subjects() {
        let a = derive_channel_subject("C1", "U0");
        let b = derive_channel_subject("C2", "U0");
        assert_ne!(a.subject_id, b.subject_id);
    }

    #[test]
    fn different_users_yield_different_subjects() {
        let a = derive_channel_subject("C0", "U1");
        let b = derive_channel_subject("C0", "U2");
        assert_ne!(a.subject_id, b.subject_id);
    }

    #[test]
    fn display_handle_includes_channel_and_short_hex() {
        let s = derive_channel_subject("C0", "U0");
        let h = s.display_handle();
        assert!(h.starts_with("slack:U0@C0:"));
        let prefix = h.rsplit(':').next().unwrap();
        assert_eq!(prefix.len(), 16);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn slack_namespace_disjoint_from_discord_and_telegram() {
        // Even with identical id strings, slack:/discord:/telegram:
        // hashes must not collide. Done inline (no cross-crate dep)
        // to keep the dependency graph clean.
        let ours = derive_channel_subject("100", "42");
        let dc_input = "discord:42:100";
        let tg_input = "telegram:42:100";
        let dc_hash = blake3::hash(dc_input.as_bytes());
        let tg_hash = blake3::hash(tg_input.as_bytes());
        let mut dc = [0u8; 32];
        dc.copy_from_slice(&dc_hash.as_bytes()[..32]);
        let mut tg = [0u8; 32];
        tg.copy_from_slice(&tg_hash.as_bytes()[..32]);
        assert_ne!(ours.subject_id, NodeId(dc));
        assert_ne!(ours.subject_id, NodeId(tg));
    }
}
