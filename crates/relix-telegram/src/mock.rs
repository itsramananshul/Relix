//! In-memory `BotApi` implementation for tests and dev tools.
//!
//! `MockBotApi` lets the controller code be exercised without
//! talking to Telegram. Tests push synthetic updates via
//! [`MockBotApi::push_update`] and observe outgoing replies via
//! [`MockBotApi::sent_messages`]. The live HTTPS implementation
//! is a drop-in for this trait surface.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{BotApi, BotApiError, BotIdentity, IncomingMessage, OutgoingMessage, ParseMode};

/// `(chat_id, message_id, new_text, parse_mode)` — one
/// recorded edit. Aliased to satisfy clippy's complex-type
/// lint without losing column readability.
pub type RecordedEdit = (i64, i64, String, Option<ParseMode>);

#[derive(Default)]
pub struct MockBotApi {
    inbound: Mutex<Vec<IncomingMessage>>,
    sent: Mutex<Vec<OutgoingMessage>>,
    chat_actions: Mutex<Vec<(i64, String)>>,
    callback_acks: Mutex<Vec<(String, Option<String>)>>,
    edits: Mutex<Vec<RecordedEdit>>,
    /// Override for `get_me`. Defaults to a mock identity so
    /// existing tests work; the controller-startup test sets
    /// this explicitly to assert format.
    bot_identity: Mutex<BotIdentity>,
    /// When set, the next `send_message` call returns this
    /// error and clears the override. Used by tests covering
    /// the delivery retry path.
    fail_next_send: Mutex<Option<BotApiError>>,
    /// Canned `file_id → bytes` map for `get_file_bytes`. Tests
    /// that exercise the voice path push a payload here keyed
    /// by file_id; everything else returns
    /// `BotApiError::ClientError`.
    file_bytes: Mutex<std::collections::HashMap<String, Vec<u8>>>,
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

    /// Inspect chat actions the channel has emitted (typing
    /// indicators). Each entry is `(chat_id, action)`.
    pub fn chat_actions(&self) -> Vec<(i64, String)> {
        self.chat_actions.lock().unwrap().clone()
    }

    /// Inspect callback-query acks the channel has emitted.
    pub fn callback_acks(&self) -> Vec<(String, Option<String>)> {
        self.callback_acks.lock().unwrap().clone()
    }

    /// Inspect edits the channel has emitted.
    pub fn edits(&self) -> Vec<RecordedEdit> {
        self.edits.lock().unwrap().clone()
    }

    /// Override the identity `get_me` returns. Tests use this
    /// to assert the controller logs the right `@username`.
    pub fn set_identity(&self, id: BotIdentity) {
        *self.bot_identity.lock().unwrap() = id;
    }

    /// Set up the next `send_message` to fail. Cleared after
    /// one call.
    pub fn fail_next_send(&self, err: BotApiError) {
        *self.fail_next_send.lock().unwrap() = Some(err);
    }

    /// Stage canned bytes the next `get_file_bytes(file_id)`
    /// call will return.
    pub fn stage_file_bytes(&self, file_id: impl Into<String>, bytes: Vec<u8>) {
        self.file_bytes
            .lock()
            .unwrap()
            .insert(file_id.into(), bytes);
    }
}

#[async_trait]
impl BotApi for MockBotApi {
    async fn get_me(&self) -> Result<BotIdentity, BotApiError> {
        Ok(self.bot_identity.lock().unwrap().clone())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<(), BotApiError> {
        self.callback_acks
            .lock()
            .unwrap()
            .push((callback_query_id.to_string(), text.map(|s| s.to_string())));
        Ok(())
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<(), BotApiError> {
        self.edits
            .lock()
            .unwrap()
            .push((chat_id, message_id, text.to_string(), parse_mode));
        Ok(())
    }

    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), BotApiError> {
        self.chat_actions
            .lock()
            .unwrap()
            .push((chat_id, action.to_string()));
        Ok(())
    }

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

    async fn get_file_bytes(&self, file_id: &str) -> Result<Vec<u8>, BotApiError> {
        match self.file_bytes.lock().unwrap().get(file_id).cloned() {
            Some(b) => Ok(b),
            None => Err(BotApiError::ClientError(format!(
                "mock: no staged bytes for file_id={file_id}"
            ))),
        }
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
            voice_file_id: None,
            callback_query_id: None,
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
            parse_mode: None,
            reply_markup: None,
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
            parse_mode: None,
            reply_markup: None,
        };
        let r = m.send_message(&out).await;
        assert!(matches!(r, Err(BotApiError::Transient(_))));
        // Second call goes through.
        m.send_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
    }
}
