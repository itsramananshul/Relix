//! In-memory `SlackApi` implementation for tests and dev tools.
//!
//! Lets the controller code be exercised without talking to Slack.
//! Tests push synthetic inbounds via `push_message` and observe
//! replies via `sent_messages`. The live HTTPS implementation is a
//! drop-in for this trait surface.

use std::sync::Mutex;

use async_trait::async_trait;

use crate::{BotIdentity, IncomingMessage, OutgoingMessage, SlackApi, SlackApiError};

#[derive(Default)]
pub struct MockSlackApi {
    inbound: Mutex<Vec<IncomingMessage>>,
    sent: Mutex<Vec<OutgoingMessage>>,
    updates: Mutex<Vec<(String, String, String)>>,
    bot_identity: Mutex<BotIdentity>,
    fail_next_send: Mutex<Option<SlackApiError>>,
}

impl MockSlackApi {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue a message the next `conversations_history` call
    /// will return. Tests build a script of incoming traffic this
    /// way.
    pub fn push_message(&self, m: IncomingMessage) {
        self.inbound.lock().unwrap().push(m);
    }

    pub fn sent_messages(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().unwrap().clone()
    }

    /// Inspect chat.update calls — `(channel, ts, text)` tuples.
    pub fn updates(&self) -> Vec<(String, String, String)> {
        self.updates.lock().unwrap().clone()
    }

    pub fn set_identity(&self, id: BotIdentity) {
        *self.bot_identity.lock().unwrap() = id;
    }

    pub fn fail_next_send(&self, err: SlackApiError) {
        *self.fail_next_send.lock().unwrap() = Some(err);
    }
}

/// Compare two Slack `ts` strings as lexicographic-on-numeric
/// strings: split on `.`, compare seconds first then fraction.
/// Slack `ts` values always have the same shape (integer + dot +
/// 6-digit fraction) so a plain string compare also works, but
/// this helper handles legacy / weird inputs more forgivingly.
fn ts_gt(a: &str, b: &str) -> bool {
    let (a_sec, a_frac) = a.split_once('.').unwrap_or((a, ""));
    let (b_sec, b_frac) = b.split_once('.').unwrap_or((b, ""));
    let a_sec_n: u128 = a_sec.parse().unwrap_or(0);
    let b_sec_n: u128 = b_sec.parse().unwrap_or(0);
    if a_sec_n != b_sec_n {
        return a_sec_n > b_sec_n;
    }
    a_frac > b_frac
}

#[async_trait]
impl SlackApi for MockSlackApi {
    async fn auth_test(&self) -> Result<BotIdentity, SlackApiError> {
        Ok(self.bot_identity.lock().unwrap().clone())
    }

    async fn conversations_history(
        &self,
        _channel: &str,
        oldest: &str,
    ) -> Result<Vec<IncomingMessage>, SlackApiError> {
        let mut q = self.inbound.lock().unwrap();
        let mut take: Vec<IncomingMessage> = if oldest.is_empty() {
            q.clone()
        } else {
            q.iter().filter(|m| ts_gt(&m.ts, oldest)).cloned().collect()
        };
        // Oldest-first.
        take.sort_by(|a, b| {
            if ts_gt(&a.ts, &b.ts) {
                std::cmp::Ordering::Greater
            } else if ts_gt(&b.ts, &a.ts) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        let returned: std::collections::HashSet<String> =
            take.iter().map(|m| m.ts.clone()).collect();
        q.retain(|m| !returned.contains(&m.ts));
        Ok(take)
    }

    async fn chat_post_message(&self, out: &OutgoingMessage) -> Result<(), SlackApiError> {
        if let Some(err) = self.fail_next_send.lock().unwrap().take() {
            return Err(err);
        }
        self.sent.lock().unwrap().push(out.clone());
        Ok(())
    }

    async fn chat_update(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackApiError> {
        self.updates
            .lock()
            .unwrap()
            .push((channel.to_string(), ts.to_string(), text.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(ts: &str) -> IncomingMessage {
        IncomingMessage {
            ts: ts.to_string(),
            channel_id: "C0".into(),
            user_id: "U0".into(),
            username: "alice".into(),
            is_bot: false,
            text: format!("u{ts}"),
        }
    }

    #[tokio::test]
    async fn oldest_cursor_skips_already_seen_messages() {
        let m = MockSlackApi::new();
        m.push_message(mk("1700000001.000100"));
        m.push_message(mk("1700000002.000100"));
        m.push_message(mk("1700000003.000100"));
        let batch = m
            .conversations_history("C0", "1700000001.000100")
            .await
            .unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].ts, "1700000002.000100");
        assert_eq!(batch[1].ts, "1700000003.000100");
        let empty = m
            .conversations_history("C0", "1700000001.000100")
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn empty_cursor_returns_all_pending() {
        let m = MockSlackApi::new();
        m.push_message(mk("1700000007.000100"));
        m.push_message(mk("1700000009.000100"));
        let batch = m.conversations_history("C0", "").await.unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[tokio::test]
    async fn post_message_records_outbound() {
        let m = MockSlackApi::new();
        let out = OutgoingMessage {
            channel_id: "C0".into(),
            thread_ts: "1700000000.000100".into(),
            text: "hello back".into(),
            blocks: Vec::new(),
        };
        m.chat_post_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
        assert_eq!(m.sent_messages()[0].text, "hello back");
    }

    #[tokio::test]
    async fn fail_next_send_returns_error_once() {
        let m = MockSlackApi::new();
        m.fail_next_send(SlackApiError::Transient("blip".into()));
        let out = OutgoingMessage {
            channel_id: "C0".into(),
            thread_ts: String::new(),
            text: "x".into(),
            blocks: Vec::new(),
        };
        let r = m.chat_post_message(&out).await;
        assert!(matches!(r, Err(SlackApiError::Transient(_))));
        m.chat_post_message(&out).await.unwrap();
        assert_eq!(m.sent_messages().len(), 1);
    }

    #[tokio::test]
    async fn chat_update_records_call() {
        let m = MockSlackApi::new();
        m.chat_update("C0", "1700000000.000100", "edited")
            .await
            .unwrap();
        assert_eq!(m.updates().len(), 1);
        assert_eq!(m.updates()[0].2, "edited");
    }

    #[test]
    fn ts_gt_handles_fractional_part() {
        assert!(ts_gt("1.000200", "1.000100"));
        assert!(!ts_gt("1.000100", "1.000200"));
        assert!(ts_gt("2.0", "1.999999"));
    }
}
