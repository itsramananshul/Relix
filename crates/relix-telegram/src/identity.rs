//! Derived identity model for Telegram users.
//!
//! Telegram users do not have Relix IdentityBundles. The
//! channel mints a **derived subject** per `(chat_id, user_id)`
//! pair by hashing `"telegram:" + user_id + ":" + chat_id` with
//! blake3, then truncating to a `NodeId`. The Coordinator
//! stores this as `owner_subject_id` on tasks so operators can
//! query "all tasks for telegram user X."
//!
//! See [`docs/channel-node-architecture.md`](../../../docs/channel-node-architecture.md)
//! §Identity model for the full design rationale.

use relix_core::types::NodeId;

/// A derived channel subject. The wrapper exists so callers can
/// stop conflating "this is a Telegram user's subject_id" with
/// "this is a regular Relix-identity subject_id." Both serialise
/// the same way on the wire; the type discipline is local.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelSubject {
    pub channel: &'static str,
    pub chat_id: i64,
    pub user_id: i64,
    pub subject_id: NodeId,
}

impl ChannelSubject {
    /// Render as a stable display string for logs and audit
    /// records. Format: `telegram:<user_id>@<chat_id>:<hex16>`
    /// — the first 16 hex chars of the subject_id are enough
    /// for at-a-glance disambiguation; full id is in
    /// `subject_id`.
    pub fn display_handle(&self) -> String {
        let hex = self.subject_id.to_string();
        let prefix = &hex[..hex.len().min(16)];
        format!(
            "{}:{}@{}:{}",
            self.channel, self.user_id, self.chat_id, prefix
        )
    }
}

/// Derive the per-user `ChannelSubject` for a Telegram message.
/// Deterministic: same `(chat_id, user_id)` always returns the
/// same subject, across restarts and across channel instances
/// sharing a config.
pub fn derive_channel_subject(chat_id: i64, user_id: i64) -> ChannelSubject {
    let input = format!("telegram:{user_id}:{chat_id}");
    let hash = blake3::hash(input.as_bytes());
    let bytes = hash.as_bytes();
    // NodeId is 32 bytes; blake3 returns 32. Take the whole hash.
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes[..32]);
    ChannelSubject {
        channel: "telegram",
        chat_id,
        user_id,
        subject_id: NodeId(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let a = derive_channel_subject(100, 42);
        let b = derive_channel_subject(100, 42);
        assert_eq!(a, b);
        assert_eq!(a.subject_id, b.subject_id);
    }

    #[test]
    fn different_chats_yield_different_subjects() {
        let a = derive_channel_subject(100, 42);
        let b = derive_channel_subject(200, 42);
        assert_ne!(a.subject_id, b.subject_id);
        assert_eq!(a.user_id, b.user_id);
        assert_ne!(a.chat_id, b.chat_id);
    }

    #[test]
    fn different_users_yield_different_subjects() {
        let a = derive_channel_subject(100, 42);
        let b = derive_channel_subject(100, 43);
        assert_ne!(a.subject_id, b.subject_id);
    }

    #[test]
    fn display_handle_includes_channel_and_short_hex() {
        let s = derive_channel_subject(100, 42);
        let h = s.display_handle();
        assert!(h.starts_with("telegram:42@100:"));
        // 16-char hex prefix after the colon.
        let prefix = h.rsplit(':').next().unwrap();
        assert_eq!(prefix.len(), 16);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn negative_ids_are_handled() {
        // Telegram group chat ids are negative integers; the
        // derivation must tolerate them without panicking or
        // collapsing distinct values.
        let pos = derive_channel_subject(100, 42);
        let neg = derive_channel_subject(-100, 42);
        assert_ne!(pos.subject_id, neg.subject_id);
    }
}
