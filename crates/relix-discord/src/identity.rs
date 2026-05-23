//! Derived identity model for Discord users — mirror of the
//! Telegram derivation but namespaced under `discord:`.
//!
//! Discord users do not have Relix IdentityBundles. The channel
//! mints a derived subject per `(channel_id, user_id)` pair by
//! hashing `"discord:" + user_id + ":" + channel_id` with blake3.
//! The Coordinator stores this as `owner_subject_id` on tasks so
//! operators can query "all tasks for discord user X."

use relix_core::types::NodeId;

/// A derived channel subject. Mirrors `relix_telegram::ChannelSubject`
/// but is intentionally a distinct type so callers can't accidentally
/// swap a Telegram subject for a Discord one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSubject {
    pub channel: &'static str,
    pub channel_id: String,
    pub user_id: String,
    pub subject_id: NodeId,
}

impl ChannelSubject {
    /// Stable display string for logs and audit. Format:
    /// `discord:<user_id>@<channel_id>:<hex16>` — first 16 hex
    /// chars of the subject id are enough for at-a-glance
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

/// Derive the per-user `ChannelSubject` for a Discord message.
/// Deterministic: same `(channel_id, user_id)` always returns the
/// same subject_id, across restarts.
pub fn derive_channel_subject(channel_id: &str, user_id: &str) -> ChannelSubject {
    let input = format!("discord:{user_id}:{channel_id}");
    let hash = blake3::hash(input.as_bytes());
    let bytes = hash.as_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    ChannelSubject {
        channel: "discord",
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
        let a = derive_channel_subject("100", "42");
        let b = derive_channel_subject("100", "42");
        assert_eq!(a.subject_id, b.subject_id);
    }

    #[test]
    fn different_channels_yield_different_subjects() {
        let a = derive_channel_subject("100", "42");
        let b = derive_channel_subject("200", "42");
        assert_ne!(a.subject_id, b.subject_id);
    }

    #[test]
    fn different_users_yield_different_subjects() {
        let a = derive_channel_subject("100", "42");
        let b = derive_channel_subject("100", "43");
        assert_ne!(a.subject_id, b.subject_id);
    }

    #[test]
    fn display_handle_includes_channel_and_short_hex() {
        let s = derive_channel_subject("100", "42");
        let h = s.display_handle();
        assert!(h.starts_with("discord:42@100:"));
        let prefix = h.rsplit(':').next().unwrap();
        assert_eq!(prefix.len(), 16);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn discord_namespace_disjoint_from_telegram() {
        // Telegram derivation hashes `"telegram:" + user + ":" + chat`;
        // ours hashes `"discord:"...`. Even with identical numeric ids
        // the subjects must not collide. Done inline (no telegram dep
        // from the discord crate) to keep the dependency graph clean.
        let ours = derive_channel_subject("100", "42");
        let tg_input = format!("telegram:{}:{}", 42, 100);
        let tg_hash = blake3::hash(tg_input.as_bytes());
        let mut tg_bytes = [0u8; 32];
        tg_bytes.copy_from_slice(&tg_hash.as_bytes()[..32]);
        let tg_subject = NodeId(tg_bytes);
        assert_ne!(ours.subject_id, tg_subject);
    }
}
