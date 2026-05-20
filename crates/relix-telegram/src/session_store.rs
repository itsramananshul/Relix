//! Channel-local mapping from `(chat_id, message_id)` to
//! `task_id`. Lets the async delivery path find the right
//! Telegram chat to reply to when a long-running flow finally
//! completes — without keeping the inbound handler blocked.
//!
//! Today this is an in-memory `BTreeMap` behind an
//! `RwLock`. A persistent backing store is a follow-up: if the
//! channel restarts mid-flow, the mapping for that in-flight
//! request is lost (the Task on the Coordinator survives, but
//! the channel can no longer route its reply). A sled / SQLite
//! backing store with the same trait shape lands when the
//! channel ships in production.

use std::collections::BTreeMap;
use std::sync::RwLock;

/// (chat_id, message_id) → task_id.
#[derive(Default)]
pub struct SessionStore {
    inner: RwLock<BTreeMap<(i64, i64), String>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the mapping. Called by the inbound handler right
    /// after `task.create` succeeds.
    pub fn record(&self, chat_id: i64, message_id: i64, task_id: String) {
        let mut g = self.inner.write().expect("poisoned");
        g.insert((chat_id, message_id), task_id);
    }

    /// Look up the task_id for a `(chat_id, message_id)`.
    /// Returns `None` when the mapping isn't present (channel
    /// restart, eviction, never recorded).
    pub fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let g = self.inner.read().expect("poisoned");
        g.get(&(chat_id, message_id)).cloned()
    }

    /// Drop the mapping after the reply is delivered.
    pub fn forget(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let mut g = self.inner.write().expect("poisoned");
        g.remove(&(chat_id, message_id))
    }

    /// Operator + test inspection: how many in-flight mappings?
    pub fn len(&self) -> usize {
        self.inner.read().expect("poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_lookup_forget_round_trip() {
        let s = SessionStore::new();
        assert!(s.lookup(1, 2).is_none());
        s.record(1, 2, "abc".into());
        assert_eq!(s.lookup(1, 2).as_deref(), Some("abc"));
        assert_eq!(s.len(), 1);
        let removed = s.forget(1, 2);
        assert_eq!(removed.as_deref(), Some("abc"));
        assert!(s.is_empty());
    }

    #[test]
    fn overwrite_on_duplicate_record() {
        // If a channel ever sees the same (chat, message) twice
        // — should never happen at the Telegram level but we
        // shouldn't double-store — last write wins.
        let s = SessionStore::new();
        s.record(1, 2, "first".into());
        s.record(1, 2, "second".into());
        assert_eq!(s.lookup(1, 2).as_deref(), Some("second"));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn distinct_keys_coexist() {
        let s = SessionStore::new();
        s.record(1, 2, "a".into());
        s.record(1, 3, "b".into());
        s.record(2, 2, "c".into());
        assert_eq!(s.len(), 3);
        assert_eq!(s.lookup(1, 2).as_deref(), Some("a"));
        assert_eq!(s.lookup(1, 3).as_deref(), Some("b"));
        assert_eq!(s.lookup(2, 2).as_deref(), Some("c"));
    }
}
