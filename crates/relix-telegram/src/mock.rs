//! In-memory `BotApi` implementation for tests and dev tools.
//!
//! `MockBotApi` lets the controller code be exercised without
//! talking to Telegram. Tests push synthetic updates via
//! [`MockBotApi::push_update`] and observe outgoing replies via
//! [`MockBotApi::sent_messages`]. The live HTTPS implementation
//! is a drop-in for this trait surface.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{BotApi, BotApiError, IncomingMessage, OutgoingMessage};

#[derive(Default)]
pub struct MockBotApi {
    inbound: Mutex<Vec<IncomingMessage>>,
    sent: Mutex<Vec<OutgoingMessage>>,
    /// When set, the next `send_message` call returns this
    /// error and clears the override. Used by tests covering
    /// the delivery retry path.
    fail_next_send: Mutex<Option<BotApiError>>,
}

impl MockBotApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a message the next `get_updates` call will
    /// return. Tests build a script of incoming traffic this
    /// way.
    pub fn push_update(&self, m: IncomingMessage) {
        self.inbound.lock().unwrap().push(m);
    }

    /// Inspect what the channel has sent in reply.
    pub fn sent_messages(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().unwrap().clone()
    }

    /// Set up the next `send_message` to fail. Cleared after
    /// one call.
    pub fn fail_next_send(&self, err: BotApiError) {
        *self.fail_next_send.lock().unwrap() = Some(err);
    }
}

#[async_trait]
impl BotApi for MockBotApi {
    async fn get_updates(&self, offset: i64) -> Result<Vec<IncomingMessage>, BotApiError> {
        let mut q = self.inbound.lock().unwrap();
        let take: Vec<IncomingMessage> = q
            .iter()
            .filter(|m| m.update_id >= offset)
            .cloned()
            .collect();
        // Mimic Telegram's "consume on read" model: drain
        // what we returned so a second poll doesn't see them.
        q.retain(|m| m.update_id < offset || !take.iter().any(|t| t.update_id == m.update_id));
        Ok(take)
    }

    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), BotApiError> {
        if let Some(err) = self.fail_next_send.lock().unwrap().take() {
            return Err(err);
        }
        self.sent.lock().unwrap().push(out.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(update_id: i64) -> IncomingMessage {
        IncomingMessage {
            update_id,
            chat_id: 100,
            user_id: 42,
            message_id: update_id, // ok for tests
            username: "alice".into(),
            text: format!("u{update_id}"),
        }
    }

    #[tokio::test]
    async fn updates_visible_only_above_offset() {
        let m = MockBotApi::new();
        m.push_update(mk(1));
        m.push_update(mk(2));
        m.push_update(mk(3));
        let batch = m.get_updates(2).await.unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch.iter().any(|u| u.update_id == 2));
        assert!(batch.iter().any(|u| u.update_id == 3));
        // Same offset again: empty (already consumed).
        let empty = m.get_updates(2).await.unwrap();
        assert!(empty.is_empty());
        // Updates below the cursor are NOT consumed.
        let still_there = m.get_updates(1).await.unwrap();
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].update_id, 1);
    }

    #[tokio::test]
    async fn send_message_records_outbound() {
        let m = MockBotApi::new();
        let out = OutgoingMessage {
            chat_id: 100,
            reply_to_message_id: 5,
            text: "hello back".into(),
        };
        m.send_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
        assert_eq!(m.sent_messages()[0].text, "hello back");
    }

    #[tokio::test]
    async fn fail_next_send_returns_error_once() {
        let m = MockBotApi::new();
        m.fail_next_send(BotApiError::Transient("network blip".into()));
        let out = OutgoingMessage {
            chat_id: 100,
            reply_to_message_id: 5,
            text: "x".into(),
        };
        let r = m.send_message(&out).await;
        assert!(matches!(r, Err(BotApiError::Transient(_))));
        // Second call goes through.
        m.send_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
    }
}
