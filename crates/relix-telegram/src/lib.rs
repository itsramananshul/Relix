//! Task-native Telegram channel for Relix.
//!
//! This is the **architecture scaffold** for the Telegram channel
//! per [`docs/channel-node-architecture.md`](../../../docs/channel-node-architecture.md).
//! It ships the testable pieces:
//!
//! - The `[telegram]` config section + validation.
//! - The derived-subject identity model (per-Telegram-user
//!   subject ids derived from chat + user via blake3).
//! - The `BotApi` trait — the Telegram Bot API client surface
//!   defined so the controller / ingest / delivery code is
//!   testable without HTTPS.
//! - A `MockBotApi` test double + tests for the mapping logic.
//!
//! The live HTTPS implementation (`reqwest`-backed `BotApi`) and
//! the controller binary are not yet wired. To enable Telegram,
//! the operator supplies a Bot API token via the dashboard's
//! Telegram settings page (see `docs/dashboard-redesign.md`).
//! The implementer adds a `live` module + a `main.rs` that
//! reads that config and wires this scaffold to the existing
//! controller startup path.

pub mod config;
pub mod identity;
pub mod messages;
pub mod mock;
pub mod session_store;

pub use config::{TelegramConfig, TelegramError};
pub use identity::{ChannelSubject, derive_channel_subject};
pub use messages::{IncomingMessage, OutgoingMessage};
pub use session_store::{InMemorySessionStore, SessionStorage, SessionStore, SqliteSessionStore};

use async_trait::async_trait;

/// Network surface a Telegram channel needs from a Bot API
/// client. Kept narrow — only the two operations the
/// task-native channel actually uses (long-poll and reply).
/// Webhook mode is a follow-up that adds a separate handler;
/// the trait stays the same shape from the channel's
/// perspective.
///
/// Implemented by the live HTTPS client and by [`mock::MockBotApi`]
/// for tests.
#[async_trait]
pub trait BotApi: Send + Sync + 'static {
    /// Fetch the next batch of inbound updates. The `offset`
    /// is Telegram's update_id pagination cursor; the channel
    /// passes back `max(update_id) + 1` from the previous
    /// batch (or 0 on first call).
    ///
    /// Returns updates in oldest-first order. Empty Vec when
    /// no updates are available within the configured long-poll
    /// timeout.
    async fn get_updates(&self, offset: i64) -> Result<Vec<IncomingMessage>, BotApiError>;

    /// Send a text reply to the originating chat. The
    /// `reply_to_message_id` ties the reply to the original
    /// message so Telegram clients render it as a thread.
    ///
    /// Implementations MUST retry transient (5xx / network)
    /// failures with bounded backoff before returning Err.
    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), BotApiError>;
}

#[derive(Debug, thiserror::Error)]
pub enum BotApiError {
    /// 4xx — usually a configuration problem (bad token,
    /// chat removed bot). Not retryable.
    #[error("telegram api: client error: {0}")]
    ClientError(String),
    /// 5xx / network — retryable; the impl already retried
    /// per its own backoff before surfacing.
    #[error("telegram api: transient: {0}")]
    Transient(String),
    /// Token / config missing. Surfaced once at startup.
    #[error("telegram api: missing credentials")]
    MissingCredentials,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_bot_api_implements_trait_object_safe() {
        // Quick sanity that the trait can be held behind dyn.
        // The live and mock impls both go through this path
        // inside the controller.
        let mock: Box<dyn BotApi> = Box::new(mock::MockBotApi::new());
        let v = mock.get_updates(0).await.unwrap();
        assert!(v.is_empty());
    }
}
