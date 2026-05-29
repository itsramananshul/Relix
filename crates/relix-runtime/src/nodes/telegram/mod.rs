//! Telegram channel node — turns inbound Telegram messages
//! into chat-flow runs and posts the agent's reply back to
//! the originating chat.
//!
//! ## Shape
//!
//! `node_type = "telegram"` registers two read capabilities
//! the bridge proxies for the dashboard:
//!
//! - `telegram.status`         — bot online state + username
//!   + own user_id.
//! - `telegram.messages_recent` — last 20 inbound messages
//!   from a bounded ring (capacity 200).
//!
//! Plus a long-polling background task that:
//!
//! 1. Calls `getUpdates(offset)` against the live Bot API.
//! 2. Filters out non-authorised callers
//!    (`[telegram] allowed_users`).
//! 3. Splits on slash commands (`/start`, `/help`, `/status`,
//!    `/memory`, `/forget`, `/approve <id>`, `/reject <id>`)
//!    and handles them locally without invoking the AI peer.
//! 4. For chat messages: sends a `typing` chat-action, reads
//!    recent history from memory, dispatches `ai.chat`,
//!    persists both halves of the turn via `memory.write_turn`,
//!    posts the result back via `sendMessage`.
//! 5. Optionally polls the coordinator for tasks in
//!    `awaiting_input` and posts an approval-required note
//!    to `operator_chat_id` when configured.
//!
//! All outbound RPCs reach memory / ai / coordinator via the
//! same `MeshClient` + `Bundle` pattern the AI node and the
//! memory curator already use.

pub mod client;
pub mod commands;
pub mod config;
pub mod controller;
pub mod ring;
pub mod state;

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use relix_telegram::{BotApi, OutgoingMessage};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub use client::{TelegramOutboundClient, TelegramOutboundClientCell};
pub use config::{
    AiPeerConfig, CoordPeerConfig, MemoryPeerConfig, TelegramNodeConfig, TelegramNodeError,
};
pub use controller::{run_telegram_controller, run_telegram_controller_with_api};
pub use ring::{MessageRing, RecordedInbound};
pub use state::{ChannelState, NotifierState};

/// Render the `telegram.status` body. Stable wire shape
/// consumed by the bridge proxy:
///
/// `online=<bool>|username=<str>|first_name=<str>|user_id=<i64>|messages_seen=<u64>|last_message_at=<i64>\n`
///
/// `last_message_at` is the unix-seconds timestamp of the
/// most-recently-recorded inbound message; `-1` when none.
pub fn render_status_body(state: &ChannelState) -> String {
    let online = state.online();
    let id = state.identity();
    let messages_seen = state.messages_seen();
    let last_at = state.last_message_at().unwrap_or(-1);
    format!(
        "online={online}|username={}|first_name={}|user_id={}|messages_seen={messages_seen}|last_message_at={last_at}\n",
        id.username, id.first_name, id.user_id
    )
}

/// Render the `telegram.messages_recent` body. One row per
/// recorded inbound message, newest-first, tab-separated:
///
/// `ts\tfrom_user_id\tfrom_username\tchat_id\ttext_preview\n`
///
/// `text_preview` is truncated to 100 chars and stripped of
/// tabs/newlines so the row stays parseable.
pub fn render_recent_body(ring: &MessageRing, limit: usize) -> String {
    let entries = ring.snapshot();
    let take = limit.min(entries.len());
    let mut out = String::new();
    for entry in entries.iter().rev().take(take) {
        let preview = truncate_preview(&entry.text, 100);
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            entry.ts, entry.user_id, entry.username, entry.chat_id, preview
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

/// Register `telegram.status`, `telegram.messages_recent`, and
/// `telegram.send` on a controller with `node_type = "telegram"`.
///
/// The first two are read-only and project state the long-poll
/// loop already maintains.
///
/// `telegram.send` is the RELIX-7.11/§7.7 outbound capability the
/// coordinator (and the alert-fan-out sink) call when they
/// need to push a message to a Telegram chat from outside the
/// long-poll loop. It accepts JSON `{ "chat_id": "<id>", "text": "..." }`
/// (`chat_id` is a string so callers can pass either Telegram's
/// numeric ids or `@channelusername` forms; the handler parses
/// integers itself). On success it returns the JSON body
/// `{ "ok": true }`; on failure it returns a structured
/// `ErrorEnvelope` with the right kind.
pub fn register(
    bridge: &mut DispatchBridge,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    api: Arc<dyn BotApi>,
) {
    {
        let state = state.clone();
        bridge.register(
            "telegram.status",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let body = render_status_body(&state);
                    HandlerOutcome::Ok(body.into_bytes())
                }
            })),
        );
    }
    {
        let ring = ring.clone();
        bridge.register(
            "telegram.messages_recent",
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
            "telegram.send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_send(api, ctx.args).await }
            })),
        );
    }
}

#[derive(Debug, serde::Deserialize)]
struct SendArgs {
    /// Telegram chat id as a string. Numeric ids parse to
    /// `i64`; `@channelusername` ids fail with INVALID_ARGS
    /// (the Bot API accepts those but our wire shape is
    /// numeric for now to keep the handler small).
    chat_id: String,
    /// Message body.
    text: String,
}

async fn handle_send(api: Arc<dyn BotApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: SendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("telegram.send: args must be JSON {{chat_id, text}}: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.text.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "telegram.send: text must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let chat_id: i64 = match parsed.chat_id.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "telegram.send: chat_id must be a numeric Telegram chat id, got {:?}",
                    parsed.chat_id
                ),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let msg = OutgoingMessage {
        chat_id,
        // No threading context for outbound coordinator
        // messages — 0 means "top-level message".
        reply_to_message_id: 0,
        text: parsed.text,
        parse_mode: None,
        reply_markup: None,
    };
    match api.send_message(&msg).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(relix_telegram::BotApiError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("telegram.send: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(relix_telegram::BotApiError::ClientError(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("telegram.send: {c}"),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(relix_telegram::BotApiError::MissingCredentials) => {
            HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: "telegram.send: bot credentials missing".into(),
                retry_hint: 0,
                retry_after: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_telegram::BotIdentity;

    #[test]
    fn render_status_body_offline_default_shape() {
        let s = ChannelState::default();
        let body = render_status_body(&s);
        // Offline by default — never online without an
        // explicit `mark_online`.
        assert!(body.contains("online=false"));
        assert!(body.contains("username="));
        assert!(body.contains("messages_seen=0"));
        assert!(body.contains("last_message_at=-1"));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn render_status_body_after_online_includes_identity() {
        let s = ChannelState::default();
        s.mark_online(BotIdentity {
            user_id: 99,
            username: "relixbot".into(),
            first_name: "Relix".into(),
        });
        let body = render_status_body(&s);
        assert!(body.contains("online=true"));
        assert!(body.contains("username=relixbot"));
        assert!(body.contains("first_name=Relix"));
        assert!(body.contains("user_id=99"));
    }

    #[test]
    fn render_recent_body_returns_newest_first() {
        let ring = MessageRing::new(200);
        ring.record(RecordedInbound {
            ts: 100,
            user_id: 1,
            username: "alice".into(),
            chat_id: 10,
            text: "old".into(),
        });
        ring.record(RecordedInbound {
            ts: 200,
            user_id: 2,
            username: "bob".into(),
            chat_id: 20,
            text: "new".into(),
        });
        let body = render_recent_body(&ring, 20);
        let lines: Vec<&str> = body.trim_end().split('\n').collect();
        // newest first
        assert!(lines[0].contains("\tbob\t"));
        assert!(lines[1].contains("\talice\t"));
    }

    #[test]
    fn render_recent_body_truncates_text_preview() {
        let ring = MessageRing::new(200);
        let long_text: String = "a".repeat(250);
        ring.record(RecordedInbound {
            ts: 100,
            user_id: 1,
            username: "alice".into(),
            chat_id: 10,
            text: long_text,
        });
        let body = render_recent_body(&ring, 5);
        let preview = body.split('\t').nth(4).unwrap();
        // Trim trailing newline before counting.
        let preview = preview.trim_end_matches('\n');
        assert_eq!(preview.chars().count(), 100);
    }

    #[test]
    fn render_recent_body_drops_newlines_in_preview() {
        let ring = MessageRing::new(200);
        ring.record(RecordedInbound {
            ts: 100,
            user_id: 1,
            username: "alice".into(),
            chat_id: 10,
            text: "line1\nline2\tcol\rok".into(),
        });
        let body = render_recent_body(&ring, 5);
        // No raw newlines or tabs inside the preview column.
        let cols: Vec<&str> = body.trim_end_matches('\n').split('\t').collect();
        assert_eq!(cols.len(), 5);
        let preview = cols[4];
        assert!(!preview.contains('\n'));
        assert!(!preview.contains('\r'));
    }

    #[test]
    fn render_recent_body_returns_empty_when_ring_empty() {
        let ring = MessageRing::new(200);
        let body = render_recent_body(&ring, 20);
        assert!(body.is_empty());
    }

    // ── telegram.send capability tests ───────────────────

    #[tokio::test]
    async fn telegram_send_dispatches_to_mock_with_chat_id_and_text() {
        let api = std::sync::Arc::new(relix_telegram::mock::MockBotApi::new());
        let dyn_api: std::sync::Arc<dyn BotApi> = api.clone();
        let args = serde_json::json!({"chat_id": "12345", "text": "hello chat"})
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
        assert_eq!(sent[0].chat_id, 12345);
        assert_eq!(sent[0].text, "hello chat");
        assert_eq!(sent[0].reply_to_message_id, 0);
    }

    #[tokio::test]
    async fn telegram_send_rejects_non_numeric_chat_id() {
        let api: std::sync::Arc<dyn BotApi> =
            std::sync::Arc::new(relix_telegram::mock::MockBotApi::new());
        let args = serde_json::json!({"chat_id": "@channelusername", "text": "hi"})
            .to_string()
            .into_bytes();
        match handle_send(api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn telegram_send_rejects_empty_text() {
        let api: std::sync::Arc<dyn BotApi> =
            std::sync::Arc::new(relix_telegram::mock::MockBotApi::new());
        let args = serde_json::json!({"chat_id": "1", "text": ""})
            .to_string()
            .into_bytes();
        match handle_send(api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn telegram_send_returns_responder_internal_on_api_failure() {
        let api = std::sync::Arc::new(relix_telegram::mock::MockBotApi::new());
        api.fail_next_send(relix_telegram::BotApiError::ClientError("bad token".into()));
        let dyn_api: std::sync::Arc<dyn BotApi> = api;
        let args = serde_json::json!({"chat_id": "1", "text": "x"})
            .to_string()
            .into_bytes();
        match handle_send(dyn_api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::RESPONDER_INTERNAL),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }
}
