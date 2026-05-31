//! Shared in-memory state for the slack controller. Same shape
//! as the discord controller's state but carrying the Slack-
//! specific identity (team_id) and using a `ts` string for the
//! polling cursor.

use std::sync::{Arc, Mutex};

use relix_core::channel_health::ChannelHealth;
use relix_core::channel_rate_limit::ChannelRateLimiter;
use relix_core::clock::{Clock, SystemClock};
use relix_slack::BotIdentity;

pub struct ChannelState {
    online: Mutex<bool>,
    identity: Mutex<BotIdentity>,
    messages_seen: Mutex<u64>,
    last_message_at: Mutex<Option<i64>>,
    /// Slack `ts` of the most recent message processed. Doubles
    /// as the `oldest` parameter for the next
    /// `conversations.history` poll.
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

    pub fn set_cursor(&self, ts: &str) {
        *self.cursor.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = ts.to_string();
    }

    /// FIX 49: per-channel health accessor.
    pub fn health(&self) -> &ChannelHealth {
        &self.health
    }

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
            user_id: "U0".into(),
            team_id: "T0".into(),
            bot_id: "B0".into(),
            username: "bot".into(),
        });
        assert!(s.online());
        assert_eq!(s.identity().team_id, "T0");
    }

    #[test]
    fn record_inbound_advances_counter_and_timestamp() {
        let s = ChannelState::default();
        s.record_inbound(123);
        s.record_inbound(456);
        assert_eq!(s.messages_seen(), 2);
        assert_eq!(s.last_message_at(), Some(456));
    }

    #[test]
    fn cursor_defaults_empty_then_advances() {
        let s = ChannelState::default();
        assert_eq!(s.cursor(), "");
        s.set_cursor("1700000000.000200");
        assert_eq!(s.cursor(), "1700000000.000200");
    }
}
