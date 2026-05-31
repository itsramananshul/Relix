//! Bounded inbound-message ring for the slack controller.
//! Mirror of nodes/discord/ring.rs but the message id is a Slack
//! `ts` string.

use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedInbound {
    pub ts: String,
    pub user_id: String,
    pub username: String,
    pub channel_id: String,
    pub text: String,
}

pub struct MessageRing {
    inner: Mutex<Vec<RecordedInbound>>,
    capacity: usize,
}

impl MessageRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Vec::with_capacity(capacity.min(1024))),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn record(&self, entry: RecordedInbound) {
        let mut g = self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        if g.len() >= self.capacity {
            g.remove(0);
        }
        g.push(entry);
    }

    pub fn snapshot(&self) -> Vec<RecordedInbound> {
        self.inner
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MessageRing {
    fn default() -> Self {
        Self::new(200)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(secs: i64) -> RecordedInbound {
        RecordedInbound {
            ts: format!("{secs}.000000"),
            user_id: "U0".into(),
            username: "alice".into(),
            channel_id: "C0".into(),
            text: format!("m{secs}"),
        }
    }

    #[test]
    fn ring_records_in_order() {
        let r = MessageRing::new(200);
        r.record(mk(1));
        r.record(mk(2));
        r.record(mk(3));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].ts, "1.000000");
        assert_eq!(snap[2].ts, "3.000000");
    }

    #[test]
    fn ring_enforces_capacity_dropping_oldest() {
        let r = MessageRing::new(3);
        for i in 1..=5 {
            r.record(mk(i));
        }
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].ts, "3.000000");
        assert_eq!(snap[2].ts, "5.000000");
    }

    #[test]
    fn ring_min_capacity_is_one() {
        let r = MessageRing::new(0);
        assert_eq!(r.capacity(), 1);
    }
}
