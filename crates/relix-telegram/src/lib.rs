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

pub mod approval;
pub mod config;
pub mod identity;
pub mod live;
pub mod messages;
pub mod mock;
pub mod session_store;

pub use approval::TelegramChannelDispatch;
pub use config::{TelegramConfig, TelegramError};
pub use identity::{ChannelSubject, derive_channel_subject};
pub use live::{BotIdentity, LiveBotApi};
pub use messages::{
    IncomingMessage, InlineKeyboardButton, InlineKeyboardMarkup, OutgoingMessage, ParseMode,
};
pub use session_store::{
    DEFAULT_SESSION_TTL_HOURS, DEFAULT_SWEEP_INTERVAL, InMemorySessionStore, SessionStorage,
    SessionStore, SqliteSessionStore, spawn_session_sweeper,
};

use async_trait::async_trait;

/// Network surface a Telegram channel needs from a Bot API
/// client.
///
/// The trait is shaped to be small but cover every operation
/// the live channel controller actually performs: bootstrap
/// token verification (`get_me`), the long-poll receive loop
/// (`get_updates`), the standard text reply (`send_message`),
/// inline-button acknowledgement (`answer_callback_query`),
/// in-place edits (`edit_message_text` — used by approval
/// notifications to flip "pending" → "approved" after the
/// operator replies), and the typing-indicator hint
/// (`send_chat_action`).
///
/// Implemented by [`LiveBotApi`] (reqwest + rustls) and by
/// [`mock::MockBotApi`] for tests.
#[async_trait]
pub trait BotApi: Send + Sync + 'static {
    /// Verify the bot token at startup and return the bot's
    /// own identity (username + numeric user_id). Hit once
    /// during boot — when this fails the controller refuses
    /// to come up so a misconfigured token can't silently
    /// drop traffic.
    async fn get_me(&self) -> Result<BotIdentity, BotApiError>;

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

    /// Acknowledge an inline-button press. Telegram clients
    /// show a spinning indicator on the button until this is
    /// called; the optional text appears as a transient toast
    /// in the chat UI.
    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<(), BotApiError>;

    /// Edit an existing message's text in place. Used by the
    /// approval-notification flow to flip "⏳ pending" →
    /// "✅ approved" without sending a second message.
    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<(), BotApiError>;

    /// Send a chat action ("typing", "upload_photo", …). The
    /// indicator auto-expires after 5s on Telegram clients;
    /// callers re-send if the work takes longer.
    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), BotApiError>;

    /// Download a file by its Telegram `file_id`. Returns the
    /// raw bytes. Voice transcription uses this to pull the
    /// `.oga` audio attached to a voice message before
    /// dispatching `tool.audio.transcribe`.
    ///
    /// Implementations call Telegram's `getFile` to resolve
    /// `file_id → file_path`, then `GET <root>/file/bot<token>/<path>`
    /// to fetch the bytes.
    async fn get_file_bytes(&self, file_id: &str) -> Result<Vec<u8>, BotApiError>;

    /// FIX 1: register the webhook URL with Telegram. Called
    /// once at controller startup when
    /// `TelegramConfig::effective_mode()` resolves to
    /// `Webhook`. The body is:
    ///
    /// ```json
    /// {
    ///   "url": "<webhook_url>",
    ///   "allowed_updates": ["message", "callback_query"]
    /// }
    /// ```
    ///
    /// Default impl returns Ok(()) so the in-memory mock and
    /// any test stub keep compiling without overriding.
    async fn set_webhook(&self, _url: &str) -> Result<(), BotApiError> {
        Ok(())
    }
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

    #[tokio::test]
    async fn live_bot_api_is_also_trait_object_safe() {
        // We never call into the network here — just confirm
        // the live impl fits behind `dyn BotApi`.
        let live: Box<dyn BotApi> = Box::new(LiveBotApi::with_base_url(
            "test-token".into(),
            "http://127.0.0.1:1".into(),
        ));
        let _ = &live;
    }
}
