//! Shared in-memory state for the discord controller — bot
//! identity (online flag + username + user_id), statistics
//! (messages_seen + last_message_at), and the last-seen message
//! id cursor used by the polling loop.

use std::sync::{Arc, Mutex};

use relix_core::channel_health::ChannelHealth;
use relix_core::channel_rate_limit::ChannelRateLimiter;
use relix_core::clock::{Clock, SystemClock};
use relix_discord::BotIdentity;

/// Shared mutable state for the discord controller. All fields
/// are locked individually so the status capability + recent-
/// messages renderer never block on the polling loop.
pub struct ChannelState {
    online: Mutex<bool>,
    identity: Mutex<BotIdentity>,
    messages_seen: Mutex<u64>,
    last_message_at: Mutex<Option<i64>>,
    /// Discord polling cursor. Empty string ⇒ start from the
    /// most recent (the controller initialises it to the first
    /// message it sees so it doesn't replay historical channel
    /// content on startup).
    cursor: Mutex<String>,
    /// FIX 49: per-channel health tracker.
    health: ChannelHealth,
}

impl Default for ChannelState {
    fn default() -> Self {
        Self::with_clock(Arc::new(SystemClock), None)
    }
}

impl ChannelState {
    pub fn with_clock(clock: Arc<dyn Clock>, rate_limiter: Option<ChannelRateLimiter>) -> Self {
        Self {
            online: Mutex::new(false),
            identity: Mutex::new(BotIdentity::default()),
            messages_seen: Mutex::new(0),
            last_message_at: Mutex::new(None),
            cursor: Mutex::new(String::new()),
            health: ChannelHealth::new("polling", clock, rate_limiter),
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

    pub fn cursor(&self) -> String {
        self.cursor
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }

    pub fn set_cursor(&self, id: &str) {
        *self.cursor.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = id.to_string();
    }

    /// FIX 49: per-channel health accessor.
    pub fn health(&self) -> &ChannelHealth {
        &self.health
    }

    /// Stamp the identity returned by `get_me` and flip the
    /// online flag. Idempotent.
    pub fn mark_online(&self, id: BotIdentity) {
        *self.identity.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = id;
        *self.online.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = true;
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
        self.health.record_event_received();
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
            user_id: "7".into(),
            username: "x".into(),
        });
        assert!(s.online());
        assert_eq!(s.identity().user_id, "7");
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
    fn cursor_defaults_empty_then_advances() {
        let s = ChannelState::default();
        assert_eq!(s.cursor(), "");
        s.set_cursor("9000");
        assert_eq!(s.cursor(), "9000");
    }
}
