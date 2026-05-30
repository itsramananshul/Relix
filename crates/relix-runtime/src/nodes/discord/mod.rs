//! Discord channel node — turns inbound Discord messages into
//! chat-flow runs and posts the agent's reply back to the
//! originating channel.
//!
//! Mirrors `nodes/telegram/` but:
//! - Polls the REST surface (`GET /channels/:id/messages?after=:last_id`)
//!   instead of long-polling Telegram's `getUpdates`.
//! - Snowflake ids (user_id, channel_id, message_id) are strings
//!   throughout — they exceed JS's safe-int range.
//! - Has no approval-notifier loop and no webhook delivery mode;
//!   the spec deliberately keeps Discord's scope smaller than
//!   Telegram's first slice.
//!
//! Registers two read-only capabilities the bridge proxies for
//! the dashboard:
//!
//! - `discord.status` — bot online state + identity + counters.
//! - `discord.messages_recent` — last N inbound messages from a
//!   bounded ring (default capacity 200).

pub mod client;
pub mod commands;
pub mod config;
pub mod controller;
pub mod ring;
pub mod state;
pub mod watermark_store;

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use relix_discord::{DiscordApi, OutgoingMessage};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub use client::{DiscordOutboundClient, DiscordOutboundClientCell};
pub use config::{
    AiPeerConfig, CoordPeerConfig, DiscordNodeConfig, DiscordNodeError, MemoryPeerConfig,
};
pub use controller::{run_discord_controller, run_discord_controller_with_api};
pub use ring::{MessageRing, RecordedInbound};
pub use state::ChannelState;

/// Render the `discord.status` body. Pipe-delimited wire shape
/// (consumed by the bridge proxy):
///
/// `online=<bool>|username=<str>|user_id=<str>|channel_id=<str>|messages_seen=<u64>|last_message_at=<i64>\n`
///
/// `last_message_at` is unix-seconds of the most-recently-recorded
/// inbound message; `-1` when none.
pub fn render_status_body(state: &ChannelState, channel_id: &str) -> String {
    let online = state.online();
    let id = state.identity();
    let messages_seen = state.messages_seen();
    let last_at = state.last_message_at().unwrap_or(-1);
    format!(
        "online={online}|username={}|user_id={}|channel_id={channel_id}|messages_seen={messages_seen}|last_message_at={last_at}\n",
        id.username, id.user_id
    )
}

/// Render the `discord.messages_recent` body. One row per recorded
/// inbound, newest-first, tab-separated:
///
/// `ts\tuser_id\tusername\tchannel_id\ttext_preview\n`
///
/// `text_preview` is truncated to 100 chars and stripped of tabs/
/// newlines so each row stays parseable.
pub fn render_recent_body(ring: &MessageRing, limit: usize) -> String {
    let entries = ring.snapshot();
    let take = limit.min(entries.len());
    let mut out = String::new();
    for entry in entries.iter().rev().take(take) {
        let preview = truncate_preview(&entry.content, 100);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            entry.ts, entry.user_id, entry.username, entry.channel_id, preview
        ));
    }
    out
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    cleaned.chars().take(max_chars).collect()
}

/// Register `discord.status`, `discord.messages_recent`, and
/// `discord.send` on a controller with `node_type = "discord"`.
///
/// `discord.send` accepts JSON `{ "channel_id": "<snowflake>",
/// "text": "..." }` and posts the message via the bot API. Used
/// by the alert fan-out + any other coordinator code that needs
/// to push from outside the inbound polling loop. Returns
/// `{ "ok": true }` on success; structured `ErrorEnvelope` on
/// failure.
pub fn register(
    bridge: &mut DispatchBridge,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    channel_id: String,
    api: Arc<dyn DiscordApi>,
) {
    {
        let state = state.clone();
        let channel_id = channel_id.clone();
        bridge.register(
            "discord.status",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                let channel_id = channel_id.clone();
                async move {
                    let body = render_status_body(&state, &channel_id);
                    HandlerOutcome::Ok(body.into_bytes())
                }
            })),
        );
    }
    {
        let ring = ring.clone();
        bridge.register(
            "discord.messages_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let ring = ring.clone();
                async move {
                    let text = String::from_utf8_lossy(&ctx.args);
                    let limit = text
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .unwrap_or(20);
                    let body = render_recent_body(&ring, limit);
                    HandlerOutcome::Ok(body.into_bytes())
                }
            })),
        );
    }
    {
        let api = api.clone();
        bridge.register(
            "discord.send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_send(api, ctx.args).await }
            })),
        );
    }
    // PART 8: rich approval dispatch. Routes operator approval
    // requests through the component-rendering
    // `DiscordChannelDispatch` so buttons fire back to the
    // bridge's interactions endpoint.
    {
        let api = api.clone();
        bridge.register(
            "discord.approval_send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_approval_send(api, ctx.args).await }
            })),
        );
    }
    // FIX 49: per-channel health cap — `discord.health`.
    {
        let state = state.clone();
        bridge.register(
            "discord.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let snapshot = state.health().snapshot();
                    match serde_json::to_vec(&snapshot) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("discord.health: serialise snapshot: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            })),
        );
    }
}

#[derive(Debug, serde::Deserialize)]
struct SendArgs {
    /// Discord channel snowflake id.
    channel_id: String,
    /// Message body.
    text: String,
}

async fn handle_send(api: Arc<dyn DiscordApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: SendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("discord.send: args must be JSON {{channel_id, text}}: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.channel_id.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "discord.send: channel_id must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    if parsed.text.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "discord.send: text must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let msg = OutgoingMessage {
        channel_id: parsed.channel_id,
        // No reply reference for outbound coordinator messages.
        reply_to_message_id: String::new(),
        content: parsed.text,
        components: Vec::new(),
    };
    match api.send_message(&msg).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(relix_discord::DiscordApiError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("discord.send: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(relix_discord::DiscordApiError::ClientError(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("discord.send: {c}"),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(relix_discord::DiscordApiError::MissingCredentials) => {
            HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: "discord.send: bot credentials missing".into(),
                retry_hint: 0,
                retry_after: None,
            })
        }
    }
}

/// PART 8: dispatch one approval request via the component-rich
/// `DiscordChannelDispatch`. `target_id` is the channel snowflake
/// the operator configured in
/// `[approval.delivery.channels.discord] channel_id`.
async fn handle_approval_send(api: Arc<dyn DiscordApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: crate::approval::ApprovalSendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("discord.approval_send: decode args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.target_id.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "discord.approval_send: target_id (channel id) must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let request = parsed.to_request();
    let is_escalation = parsed.is_escalation;
    let dispatch = relix_discord::DiscordChannelDispatch::new(api, parsed.target_id);
    use relix_core::approval::SingleChannelDispatch;
    match dispatch.send(&request, is_escalation).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("discord.approval_send: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_discord::BotIdentity;

    #[test]
    fn render_status_body_offline_default_shape() {
        let s = ChannelState::default();
        let body = render_status_body(&s, "100");
        assert!(body.contains("online=false"));
        assert!(body.contains("channel_id=100"));
        assert!(body.contains("messages_seen=0"));
        assert!(body.contains("last_message_at=-1"));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn render_status_body_after_online_includes_identity() {
        let s = ChannelState::default();
        s.mark_online(BotIdentity {
            user_id: "999".into(),
            username: "relixbot".into(),
        });
        let body = render_status_body(&s, "100");
        assert!(body.contains("online=true"));
        assert!(body.contains("username=relixbot"));
        assert!(body.contains("user_id=999"));
    }

    #[test]
    fn render_recent_body_returns_newest_first() {
        let ring = MessageRing::new(200);
        ring.record(RecordedInbound {
            ts: 100,
            user_id: "1".into(),
            username: "alice".into(),
            channel_id: "10".into(),
            content: "old".into(),
        });
        ring.record(RecordedInbound {
            ts: 200,
            user_id: "2".into(),
            username: "bob".into(),
            channel_id: "20".into(),
            content: "new".into(),
        });
        let body = render_recent_body(&ring, 20);
        let lines: Vec<&str> = body.trim_end().split('\n').collect();
        assert!(lines[0].contains("\tbob\t"));
        assert!(lines[1].contains("\talice\t"));
    }

    #[test]
    fn render_recent_body_truncates_text_preview() {
        let ring = MessageRing::new(200);
        let long: String = "a".repeat(250);
        ring.record(RecordedInbound {
            ts: 100,
            user_id: "1".into(),
            username: "alice".into(),
            channel_id: "10".into(),
            content: long,
        });
        let body = render_recent_body(&ring, 5);
        let preview = body.split('\t').nth(4).unwrap().trim_end_matches('\n');
        assert_eq!(preview.chars().count(), 100);
    }

    // ── discord.send capability tests ────────────────────

    #[tokio::test]
    async fn discord_send_dispatches_to_mock_with_channel_id_and_text() {
        let api = std::sync::Arc::new(relix_discord::mock::MockDiscordApi::new());
        let dyn_api: std::sync::Arc<dyn DiscordApi> = api.clone();
        let args = serde_json::json!({"channel_id": "C99", "text": "hello"})
            .to_string()
            .into_bytes();
        match handle_send(dyn_api, args).await {
            HandlerOutcome::Ok(b) => {
                assert_eq!(b, b"{\"ok\":true}".to_vec());
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got Err: {e:?}"),
        }
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_id, "C99");
        assert_eq!(sent[0].content, "hello");
        assert!(sent[0].reply_to_message_id.is_empty());
    }

    #[tokio::test]
    async fn discord_send_rejects_empty_channel_id() {
        let api: std::sync::Arc<dyn DiscordApi> =
            std::sync::Arc::new(relix_discord::mock::MockDiscordApi::new());
        let args = serde_json::json!({"channel_id": "", "text": "hi"})
            .to_string()
            .into_bytes();
        match handle_send(api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn discord_send_returns_responder_internal_on_api_failure() {
        let api = std::sync::Arc::new(relix_discord::mock::MockDiscordApi::new());
        api.fail_next_send(relix_discord::DiscordApiError::ClientError("403".into()));
        let dyn_api: std::sync::Arc<dyn DiscordApi> = api;
        let args = serde_json::json!({"channel_id": "C1", "text": "x"})
            .to_string()
            .into_bytes();
        match handle_send(dyn_api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::RESPONDER_INTERNAL),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }
}
