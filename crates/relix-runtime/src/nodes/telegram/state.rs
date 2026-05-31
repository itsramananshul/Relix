//! Shared in-memory state for the telegram controller —
//! bot identity (online flag + username + user_id),
//! statistics (messages_seen + last_message_at), and a
//! `set` of already-notified approval task ids so the
//! notifier doesn't spam the operator for the same task on
//! every poll tick.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use relix_core::channel_health::ChannelHealth;
use relix_core::channel_rate_limit::ChannelRateLimiter;
use relix_core::clock::{Clock, SystemClock};
use relix_telegram::BotIdentity;

/// Shared mutable state for the telegram controller. All
/// fields are locked individually to keep the read paths
/// (status capability + recent-messages renderer) free of
/// contention with the long-poll loop.
///
/// FIX 49: an embedded [`ChannelHealth`] surfaces the
/// per-channel snapshot via the `telegram.health` capability
/// (operators read it through the bridge's `/v1/health`
/// aggregation under `channels.telegram`).
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
    /// FIX 49: per-channel health tracker. Carries
    /// last_poll_success_ms, last_event_received_ms,
    /// last_send_success_ms, consecutive_failures,
    /// session_count, rate_limit_state.
    health: ChannelHealth,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemClock), None)
    }
}

impl ChannelState {
    /// FIX 49 / 50: construct with an explicit clock + optional
    /// rate-limiter so tests can drive the health snapshot
    /// deterministically.
    pub fn with_clock(clock: Arc<dyn Clock>, rate_limiter: Option<ChannelRateLimiter>) -> Self {
        Self {
            online: Mutex::new(false),
            identity: Mutex::new(BotIdentity::default()),
            messages_seen: Mutex::new(0),
            last_message_at: Mutex::new(None),
            health: ChannelHealth::new("long_poll", clock, rate_limiter),
        }
    }

    pub fn online(&self) -> bool {
        *self.online.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }

    pub fn identity(&self) -> BotIdentity {
        self.identity
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }

    pub fn messages_seen(&self) -> u64 {
        *self.messages_seen.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }

    pub fn last_message_at(&self) -> Option<i64> {
        *self.last_message_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }

    /// FIX 49: read access to the health tracker — used by
    /// the `telegram.health` capability.
    pub fn health(&self) -> &ChannelHealth {
        &self.health
    }

    /// Stamp the identity returned by `get_me` and flip
    /// the `online` flag. Idempotent — restart loops can
    /// call this without resetting state.
    pub fn mark_online(&self, id: BotIdentity) {
        *self.identity.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = id;
        *self.online.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = true;
        // FIX 49: a successful `get_me` enables the channel.
        self.health.mark_enabled();
    }

    /// Record a new inbound message: bumps the counter and
    /// stamps the timestamp.
    pub fn record_inbound(&self, ts: i64) {
        *self.messages_seen.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) += 1;
        *self.last_message_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(ts);
        // FIX 49: stamp the event-received timestamp on
        // every inbound. Both poll-success and
        // event-received fire on inbound traffic — the
        // controller's poll loop also stamps poll_success
        // independently when get_updates returns OK.
        self.health.record_event_received();
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
        let mut g = self.seen.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        g.insert(task_id.to_string())
    }

    pub fn count(&self) -> usize {
        self.seen
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .len()
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
