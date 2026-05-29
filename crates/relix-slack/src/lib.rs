//! Task-native Slack channel for Relix.
//!
//! Mirrors `relix-discord` but speaks Slack's Web API:
//!
//! - `https://slack.com/api/` base URL.
//! - `Authorization: Bearer xoxb-...` header.
//! - One POST endpoint per method (auth.test, conversations.history,
//!   chat.postMessage, chat.update).
//! - **Slack returns HTTP 200 even on errors**, with `ok: false` and
//!   an `error` field in the body. The client treats `ok=false` as
//!   a `ClientError` rather than parsing the response normally.
//! - Inbound polling uses `oldest=<ts>` (Slack's timestamp strings,
//!   like `1700000000.000200`); the channel persists the most recent
//!   `ts` seen and feeds it back on the next poll.
//!
//! See [`SlackApi`] for the surface area + [`live::LiveSlackApi`] for
//! the production reqwest+rustls implementation.

pub mod approval;
pub mod config;
pub mod identity;
pub mod live;
pub mod messages;
pub mod mock;

pub use approval::{
    APPROVE_VALUE_PREFIX, DENY_VALUE_PREFIX, InteractionAction, InteractionParseError,
    MAX_SIGNATURE_AGE_SECS, SignatureCheck, SlackChannelDispatch, parse_interaction_payload,
    verify_request_signature,
};
pub use config::{SlackConfig, SlackError};
pub use identity::{ChannelSubject, derive_channel_subject};
pub use live::{BotIdentity, LiveSlackApi};
pub use messages::{IncomingMessage, OutgoingMessage};

use async_trait::async_trait;

/// Network surface a Slack channel needs from the Web API.
///
/// The trait is small but covers the operations the live controller
/// performs at boot + during steady state: token verification
/// (`auth_test`), the polling primitive (`conversations_history`),
/// text reply (`chat_post_message`), and an in-place edit
/// (`chat_update`) used by approval-style flows.
#[async_trait]
pub trait SlackApi: Send + Sync + 'static {
    /// Verify the bot token at startup and return the bot's own
    /// identity (user_id, team_id, bot_id, username). Hit once at
    /// boot — when this fails the controller logs and idles rather
    /// than crashing the process, so a misconfigured token shows up
    /// as `online=false` on the dashboard.
    async fn auth_test(&self) -> Result<BotIdentity, SlackApiError>;

    /// Fetch messages newer than `oldest`. The empty string means
    /// "start from the most recent" (Slack returns the latest
    /// window when `oldest` is absent). Returns chronological
    /// order — Slack's API gives newest-first, the live client
    /// reverses it before returning.
    async fn conversations_history(
        &self,
        channel: &str,
        oldest: &str,
    ) -> Result<Vec<IncomingMessage>, SlackApiError>;

    /// Post a text reply to the channel. `thread_ts` empty means a
    /// top-level message; non-empty produces a Slack threaded
    /// reply.
    ///
    /// Implementations MUST retry transient (5xx / network)
    /// failures with bounded backoff and honour 429 `Retry-After`
    /// before returning Err.
    async fn chat_post_message(&self, out: &OutgoingMessage) -> Result<(), SlackApiError>;

    /// Update an existing message in place. Slack identifies a
    /// message by `(channel, ts)`.
    async fn chat_update(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackApiError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SlackApiError {
    /// 4xx OR ok=false — usually a configuration problem (bad
    /// token, missing scope, channel not joined). Not retryable.
    #[error("slack api: client error: {0}")]
    ClientError(String),
    /// 5xx / network — retryable; the impl already retried per its
    /// own backoff before surfacing.
    #[error("slack api: transient: {0}")]
    Transient(String),
    /// Token / config missing. Surfaced once at startup.
    #[error("slack api: missing credentials")]
    MissingCredentials,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_implements_trait_object_safe() {
        let mock: Box<dyn SlackApi> = Box::new(mock::MockSlackApi::new());
        let v = mock.conversations_history("C0", "").await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn live_implements_trait_object_safe() {
        let live: Box<dyn SlackApi> = Box::new(LiveSlackApi::with_base_url(
            "xoxb-test".into(),
            "http://127.0.0.1:1".into(),
        ));
        let _ = &live;
    }
}
