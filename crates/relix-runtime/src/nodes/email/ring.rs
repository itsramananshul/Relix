//! Bounded inbound-message ring for the email controller.
//!
//! Same shape as the telegram / slack / discord rings, with
//! email-specific columns (message_id, from, subject, session_id).

use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedInbound {
    /// Unix seconds when the controller recorded the message.
    pub ts: i64,
    /// RFC 5322 `Message-ID` header value (no angle brackets).
    pub message_id: String,
    /// Bare `from` address (no display name).
    pub from: String,
    /// Subject line (decoded).
    pub subject: String,
    /// Thread session_id derived from In-Reply-To / References,
    /// falling back to the message's own Message-ID.
    pub session_id: String,
    /// First ~200 chars of the plain-text body.
    pub preview: String,
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

    fn mk(secs: i64, mid: &str) -> RecordedInbound {
        RecordedInbound {
            ts: secs,
            message_id: mid.into(),
            from: "alice@example.com".into(),
            subject: format!("subj-{secs}"),
            session_id: format!("sess-{secs}"),
            preview: format!("hello {secs}"),
        }
    }

    #[test]
    fn ring_records_in_order() {
        let r = MessageRing::new(200);
        r.record(mk(1, "a"));
        r.record(mk(2, "b"));
        r.record(mk(3, "c"));
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].ts, 1);
        assert_eq!(snap[2].ts, 3);
    }

    #[test]
    fn ring_enforces_capacity_dropping_oldest() {
        let r = MessageRing::new(3);
        for i in 1..=5 {
            r.record(mk(i, &format!("m{i}")));
        }
        let snap = r.snapshot();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].ts, 3);
        assert_eq!(snap[2].ts, 5);
    }

    #[test]
    fn ring_min_capacity_is_one() {
        let r = MessageRing::new(0);
        assert_eq!(r.capacity(), 1);
    }
}
