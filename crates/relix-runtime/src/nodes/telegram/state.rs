//! Shared in-memory state for the telegram controller —
//! bot identity (online flag + username + user_id),
//! statistics (messages_seen + last_message_at), and a
//! `set` of already-notified approval task ids so the
//! notifier doesn't spam the operator for the same task on
//! every poll tick.

use std::collections::HashSet;
use std::sync::Mutex;

use relix_telegram::BotIdentity;

/// Shared mutable state for the telegram controller. All
/// fields are locked individually to keep the read paths
/// (status capability + recent-messages renderer) free of
/// contention with the long-poll loop.
#[derive(Default)]
pub struct ChannelState {
    /// `true` once `get_me` has succeeded. Used by the
    /// status capability + the dashboard.
    online: Mutex<bool>,
    /// The bot's own identity as returned by `get_me`.
    /// Empty before the first successful boot.
    identity: Mutex<BotIdentity>,
    /// Monotonic counter of inbound messages processed
    /// (including dropped slash commands).
    messages_seen: Mutex<u64>,
    /// Unix seconds of the most recent inbound message.
    last_message_at: Mutex<Option<i64>>,
}

impl ChannelState {
    pub fn online(&self) -> bool {
        *self.online.lock().expect("poisoned")
    }

    pub fn identity(&self) -> BotIdentity {
        self.identity.lock().expect("poisoned").clone()
    }

    pub fn messages_seen(&self) -> u64 {
        *self.messages_seen.lock().expect("poisoned")
    }

    pub fn last_message_at(&self) -> Option<i64> {
        *self.last_message_at.lock().expect("poisoned")
    }

    /// Stamp the identity returned by `get_me` and flip
    /// the `online` flag. Idempotent — restart loops can
    /// call this without resetting state.
    pub fn mark_online(&self, id: BotIdentity) {
        *self.identity.lock().expect("poisoned") = id;
        *self.online.lock().expect("poisoned") = true;
    }

    /// Record a new inbound message: bumps the counter and
    /// stamps the timestamp.
    pub fn record_inbound(&self, ts: i64) {
        *self.messages_seen.lock().expect("poisoned") += 1;
        *self.last_message_at.lock().expect("poisoned") = Some(ts);
    }
}

/// Tracker for tasks the approval-notifier has already
/// pinged the operator about. Lives only in memory —
/// restart re-notifies. That's intentional: the bridge
/// already persists task state; we'd rather double-notify
/// than miss a notification.
#[derive(Default)]
pub struct NotifierState {
    seen: Mutex<HashSet<String>>,
}

impl NotifierState {
    pub fn mark_notified(&self, task_id: &str) -> bool {
        let mut g = self.seen.lock().expect("poisoned");
        g.insert(task_id.to_string())
    }

    pub fn count(&self) -> usize {
        self.seen.lock().expect("poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_defaults_false_until_marked() {
        let s = ChannelState::default();
        assert!(!s.online());
        s.mark_online(BotIdentity {
            user_id: 7,
            username: "x".into(),
            first_name: "".into(),
        });
        assert!(s.online());
        assert_eq!(s.identity().user_id, 7);
    }

    #[test]
    fn record_inbound_advances_counter_and_timestamp() {
        let s = ChannelState::default();
        assert_eq!(s.messages_seen(), 0);
        assert_eq!(s.last_message_at(), None);
        s.record_inbound(123);
        s.record_inbound(456);
        assert_eq!(s.messages_seen(), 2);
        assert_eq!(s.last_message_at(), Some(456));
    }

    #[test]
    fn notifier_state_dedupes_by_task_id() {
        let n = NotifierState::default();
        assert!(n.mark_notified("t1"));
        // Second call returns false — already seen.
        assert!(!n.mark_notified("t1"));
        assert!(n.mark_notified("t2"));
        assert_eq!(n.count(), 2);
    }
}
