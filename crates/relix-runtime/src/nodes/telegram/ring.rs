//! Bounded inbound-message ring. Drives the dashboard's
//! "recent messages" widget without unbounded memory growth.

use std::sync::Mutex;

/// One recorded inbound. Kept tiny — only the columns the
/// dashboard's recent-messages table needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedInbound {
    pub ts: i64,
    pub user_id: i64,
    pub username: String,
    pub chat_id: i64,
    pub text: String,
}

/// Bounded FIFO. Pushes drop the oldest entry once
/// `capacity` is reached. Default capacity 200 per the spec.
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

    fn mk(ts: i64) -> RecordedInbound {
        RecordedInbound {
            ts,
            user_id: 1,
            username: "alice".into(),
            chat_id: 100,
            text: format!("m{ts}"),
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
        assert_eq!(snap[0].ts, 1);
        assert_eq!(snap[2].ts, 3);
    }

    #[test]
    fn ring_enforces_capacity_dropping_oldest() {
        let r = MessageRing::new(3);
        r.record(mk(1));
        r.record(mk(2));
        r.record(mk(3));
        r.record(mk(4));
        r.record(mk(5));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].ts, 3);
        assert_eq!(snap[2].ts, 5);
    }

    #[test]
    fn ring_capacity_of_200_holds_exactly_200() {
        let r = MessageRing::new(200);
        for i in 0..250 {
            r.record(mk(i));
        }
        assert_eq!(r.len(), 200);
        let snap = r.snapshot();
        // Oldest kept is i=50 (we dropped 0..50).
        assert_eq!(snap[0].ts, 50);
        assert_eq!(snap[199].ts, 249);
    }

    #[test]
    fn ring_min_capacity_is_one() {
        let r = MessageRing::new(0);
        assert_eq!(r.capacity(), 1);
    }
}
