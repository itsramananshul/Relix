//! Task-native Discord channel for Relix.
//!
//! Mirrors `relix-telegram` but speaks Discord's REST surface:
//!
//! - `https://discord.com/api/v10` base URL.
//! - `Authorization: Bot <token>` header (not `Bearer`).
//! - Snowflake IDs are 64-bit but exceed JS's safe-integer range; Discord
//!   returns them as **strings** on the wire and we keep them strings
//!   end-to-end so we never round-trip through an unsafe number type.
//! - Inbound delivery is REST polling against
//!   `GET /channels/:id/messages?after=:last_id&limit=:n` (per the spec,
//!   not the Gateway WebSocket).
//!
//! See [`DiscordApi`] for the surface area + [`live::LiveDiscordApi`] for
//! the production reqwest+rustls implementation.

pub mod approval;
pub mod config;
pub mod identity;
pub mod live;
pub mod messages;
pub mod mock;

pub use approval::{
    APPROVE_CUSTOM_ID_PREFIX, BUTTON_STYLE_DANGER, BUTTON_STYLE_PRIMARY, COMPONENT_TYPE_ACTION_ROW,
    COMPONENT_TYPE_BUTTON, DENY_CUSTOM_ID_PREFIX, DiscordChannelDispatch,
    INTERACTION_TYPE_APPLICATION_COMMAND, INTERACTION_TYPE_MESSAGE_COMPONENT,
    INTERACTION_TYPE_PING, InteractionAction, InteractionKind, InteractionParseError,
    MESSAGE_FLAG_EPHEMERAL, RESPONSE_TYPE_CHANNEL_MESSAGE_WITH_SOURCE,
    RESPONSE_TYPE_DEFERRED_UPDATE_MESSAGE, RESPONSE_TYPE_PONG, SignatureCheck, ack_response,
    deferred_update_response, parse_interaction_payload, pong_response,
    verify_interaction_signature,
};
pub use config::{DiscordConfig, DiscordError};
pub use identity::{ChannelSubject, derive_channel_subject};
pub use live::{BotIdentity, LiveDiscordApi};
pub use messages::{IncomingMessage, OutgoingMessage};

use async_trait::async_trait;

/// Network surface a Discord channel needs from the Bot API.
///
/// The trait is small but covers every operation the live controller
/// actually performs: token verification (`get_me`), the first-boot
/// watermark seed (`bootstrap_watermark` — PART 3, prevents replaying
/// historical channel content on startup), the polling cursor
/// (`get_messages`), text reply (`send_message`), typing indicator
/// (`send_typing`), and the cleanup primitive (`delete_message`).
///
/// Implemented by [`LiveDiscordApi`] (reqwest + rustls) and by
/// [`mock::MockDiscordApi`] for tests.
#[async_trait]
pub trait DiscordApi: Send + Sync + 'static {
    /// Verify the bot token at startup and return the bot's own
    /// identity (username + numeric user_id as string). Hit once at
    /// boot — when this fails the controller logs and idles rather
    /// than crashing the process, so a misconfigured token shows up
    /// as `online=false` on the dashboard.
    async fn get_me(&self) -> Result<BotIdentity, DiscordApiError>;

    /// PART 3: fetch the snowflake of the channel's MOST RECENT
    /// message without surfacing the content. Used by the
    /// controller's first-boot path to seed the polling
    /// watermark so historical channel content is NOT replayed
    /// the first time the bot connects to a busy channel.
    ///
    /// Returns `None` when the channel is empty.
    async fn bootstrap_watermark(
        &self,
        channel_id: &str,
    ) -> Result<Option<String>, DiscordApiError>;

    /// Fetch messages newer than `after_message_id`. The empty string
    /// means "start from the most recent" — that's how an operator-
    /// onboarded bot avoids replaying historical channel content on
    /// first boot.
    ///
    /// Returns oldest-first. Empty Vec when no new messages.
    async fn get_messages(
        &self,
        channel_id: &str,
        after_message_id: &str,
    ) -> Result<Vec<IncomingMessage>, DiscordApiError>;

    /// Post a text reply to the channel. `reply_to_message_id` empty
    /// means "no reference"; non-empty produces a Discord reply
    /// reference, rendered inline by clients.
    ///
    /// Implementations MUST retry transient (5xx / network) failures
    /// with bounded backoff and honour 429 `retry_after` before
    /// returning Err.
    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), DiscordApiError>;

    /// Trigger the channel-level typing indicator. Discord auto-
    /// expires it after 10s on the client; callers re-send for
    /// longer work.
    async fn send_typing(&self, channel_id: &str) -> Result<(), DiscordApiError>;

    /// Delete a message. Used for cleanup (operator command, retracted
    /// reply). 4xx errors are surfaced verbatim — Discord returns
    /// 404 when a message is already gone, which the caller may
    /// choose to ignore.
    async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), DiscordApiError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordApiError {
    /// 4xx — usually a configuration problem (bad token, missing
    /// permission, channel removed). Not retryable.
    #[error("discord api: client error: {0}")]
    ClientError(String),
    /// 5xx / network — retryable; the impl already retried per its
    /// own backoff before surfacing.
    #[error("discord api: transient: {0}")]
    Transient(String),
    /// Token / config missing. Surfaced once at startup.
    #[error("discord api: missing credentials")]
    MissingCredentials,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_implements_trait_object_safe() {
        let mock: Box<dyn DiscordApi> = Box::new(mock::MockDiscordApi::new());
        let v = mock.get_messages("123", "").await.unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn live_implements_trait_object_safe() {
        let live: Box<dyn DiscordApi> = Box::new(LiveDiscordApi::with_base_url(
            "test-token".into(),
            "http://127.0.0.1:1".into(),
        ));
        let _ = &live;
    }
}
