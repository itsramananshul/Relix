//! In-memory `DiscordApi` implementation for tests and dev tools.
//!
//! `MockDiscordApi` lets the controller code be exercised without
//! talking to Discord. Tests push synthetic inbounds via
//! [`MockDiscordApi::push_message`] and observe outgoing replies
//! via [`MockDiscordApi::sent_messages`]. The live HTTPS
//! implementation is a drop-in for this trait surface.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{BotIdentity, DiscordApi, DiscordApiError, IncomingMessage, OutgoingMessage};

#[derive(Default)]
pub struct MockDiscordApi {
    inbound: Mutex<Vec<IncomingMessage>>,
    sent: Mutex<Vec<OutgoingMessage>>,
    typing: Mutex<Vec<String>>,
    deletes: Mutex<Vec<(String, String)>>,
    bot_identity: Mutex<BotIdentity>,
    fail_next_send: Mutex<Option<DiscordApiError>>,
}

impl MockDiscordApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a message the next `get_messages` call will return.
    /// Tests build a script of incoming traffic this way.
    pub fn push_message(&self, m: IncomingMessage) {
        self.inbound.lock().unwrap().push(m);
    }

    /// Inspect what the channel has sent in reply.
    pub fn sent_messages(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().unwrap().clone()
    }

    /// Inspect typing pings (channel ids).
    pub fn typing_pings(&self) -> Vec<String> {
        self.typing.lock().unwrap().clone()
    }

    /// Inspect delete calls (channel_id, message_id).
    pub fn deletes(&self) -> Vec<(String, String)> {
        self.deletes.lock().unwrap().clone()
    }

    /// Override the identity returned by `get_me`.
    pub fn set_identity(&self, id: BotIdentity) {
        *self.bot_identity.lock().unwrap() = id;
    }

    /// Set up the next `send_message` to fail. Cleared after one
    /// call.
    pub fn fail_next_send(&self, err: DiscordApiError) {
        *self.fail_next_send.lock().unwrap() = Some(err);
    }
}

#[async_trait]
impl DiscordApi for MockDiscordApi {
    async fn get_me(&self) -> Result<BotIdentity, DiscordApiError> {
        Ok(self.bot_identity.lock().unwrap().clone())
    }

    async fn bootstrap_watermark(
        &self,
        _channel_id: &str,
    ) -> Result<Option<String>, DiscordApiError> {
        // PART 3: return the snowflake of the newest enqueued
        // message — same shape the real Discord
        // `GET /channels/:id/messages?limit=1` returns. Does
        // NOT drain the queue (the controller seeds the
        // watermark and continues to poll from the same set).
        let q = self.inbound.lock().unwrap();
        let max = q
            .iter()
            .map(|m| m.message_id.parse::<u128>().unwrap_or(0))
            .max();
        Ok(max.map(|n| n.to_string()))
    }

    async fn get_messages(
        &self,
        _channel_id: &str,
        after_message_id: &str,
    ) -> Result<Vec<IncomingMessage>, DiscordApiError> {
        let mut q = self.inbound.lock().unwrap();
        let after_num: u128 = after_message_id.parse().unwrap_or(0);
        let mut take: Vec<IncomingMessage> = q
            .iter()
            .filter(|m| m.message_id.parse::<u128>().unwrap_or(0) > after_num)
            // PART 3: bot-authored messages MUST NOT surface,
            // mirroring the live parse-layer filter.
            .filter(|m| !m.is_bot)
            .cloned()
            .collect();
        // Oldest-first, mimicking Discord's `after` semantics with
        // `limit=50`.
        take.sort_by(|a, b| {
            a.message_id
                .parse::<u128>()
                .unwrap_or(0)
                .cmp(&b.message_id.parse::<u128>().unwrap_or(0))
        });
        // Drain returned messages so a second poll doesn't see them.
        let returned_ids: std::collections::HashSet<String> =
            take.iter().map(|m| m.message_id.clone()).collect();
        q.retain(|m| !returned_ids.contains(&m.message_id));
        Ok(take)
    }

    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), DiscordApiError> {
        if let Some(err) = self.fail_next_send.lock().unwrap().take() {
            return Err(err);
        }
        self.sent.lock().unwrap().push(out.clone());
        Ok(())
    }

    async fn send_typing(&self, channel_id: &str) -> Result<(), DiscordApiError> {
        self.typing.lock().unwrap().push(channel_id.to_string());
        Ok(())
    }

    async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), DiscordApiError> {
        self.deletes
            .lock()
            .unwrap()
            .push((channel_id.to_string(), message_id.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: u64) -> IncomingMessage {
        IncomingMessage {
            message_id: id.to_string(),
            channel_id: "100".into(),
            user_id: "42".into(),
            username: "alice".into(),
            is_bot: false,
            content: format!("u{id}"),
        }
    }

    #[tokio::test]
    async fn after_cursor_skips_already_seen_messages() {
        let m = MockDiscordApi::new();
        m.push_message(mk(1));
        m.push_message(mk(2));
        m.push_message(mk(3));
        let batch = m.get_messages("100", "1").await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].message_id, "2");
        assert_eq!(batch[1].message_id, "3");
        // Same call again returns nothing — drained.
        let empty = m.get_messages("100", "1").await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn empty_cursor_returns_all_pending() {
        let m = MockDiscordApi::new();
        m.push_message(mk(7));
        m.push_message(mk(9));
        let batch = m.get_messages("100", "").await.unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[tokio::test]
    async fn send_message_records_outbound() {
        let m = MockDiscordApi::new();
        let out = OutgoingMessage {
            channel_id: "100".into(),
            reply_to_message_id: "9000".into(),
            content: "hello back".into(),
            components: Vec::new(),
        };
        m.send_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
        assert_eq!(m.sent_messages()[0].content, "hello back");
    }

    #[tokio::test]
    async fn fail_next_send_returns_error_once() {
        let m = MockDiscordApi::new();
        m.fail_next_send(DiscordApiError::Transient("blip".into()));
        let out = OutgoingMessage {
            channel_id: "100".into(),
            reply_to_message_id: String::new(),
            content: "x".into(),
            components: Vec::new(),
        };
        let r = m.send_message(&out).await;
        assert!(matches!(r, Err(DiscordApiError::Transient(_))));
        m.send_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
    }

    #[tokio::test]
    async fn bootstrap_watermark_returns_newest_message_id() {
        let m = MockDiscordApi::new();
        assert_eq!(m.bootstrap_watermark("100").await.unwrap(), None);
        m.push_message(mk(7));
        m.push_message(mk(2));
        m.push_message(mk(9));
        let wm = m.bootstrap_watermark("100").await.unwrap();
        assert_eq!(wm.as_deref(), Some("9"));
        // bootstrap_watermark must NOT drain — a subsequent
        // get_messages with the watermark as `after` returns
        // nothing (already-seen messages).
        let after = m.get_messages("100", "9").await.unwrap();
        assert!(after.is_empty());
    }

    #[tokio::test]
    async fn bot_authored_messages_never_surface_through_mock() {
        let m = MockDiscordApi::new();
        let mut bot_msg = mk(1);
        bot_msg.is_bot = true;
        m.push_message(bot_msg);
        m.push_message(mk(2));
        let batch = m.get_messages("100", "").await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].message_id, "2");
    }

    #[tokio::test]
    async fn send_typing_records_channel_id() {
        let m = MockDiscordApi::new();
        m.send_typing("100").await.unwrap();
        m.send_typing("200").await.unwrap();
        assert_eq!(m.typing_pings(), vec!["100", "200"]);
    }

    #[tokio::test]
    async fn delete_message_records_call() {
        let m = MockDiscordApi::new();
        m.delete_message("100", "9000").await.unwrap();
        assert_eq!(m.deletes(), vec![("100".to_string(), "9000".to_string())]);
    }
}
