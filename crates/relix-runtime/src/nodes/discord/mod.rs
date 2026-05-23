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

use std::sync::Arc;

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

/// Register the read-only capabilities on a controller with
/// `node_type = "discord"`. Mirrors `nodes::telegram::register`.
pub fn register(
    bridge: &mut DispatchBridge,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    channel_id: String,
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
}
