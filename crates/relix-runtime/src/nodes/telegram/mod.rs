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
    register_with_webhook(bridge, state, ring, api, None, None);
}

/// FIX 1 variant of [`register`] that ALSO wires the
/// `telegram.webhook_update` cap. When `out_cell` + `cfg` are
/// `Some`, the cap parses inbound Telegram Update bytes and
/// dispatches them through the same `handle_one_update`
/// pipeline the long-poll loop uses. When either is `None`,
/// the cap is omitted (existing behaviour — the long-poll
/// loop is the only inbound path).
pub fn register_with_webhook(
    bridge: &mut DispatchBridge,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    api: Arc<dyn BotApi>,
    out_cell: Option<crate::nodes::telegram::client::TelegramOutboundClientCell>,
    cfg: Option<Arc<crate::nodes::telegram::config::TelegramNodeConfig>>,
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
    // PART 8: rich approval dispatch. Hosts the operator-facing
    // interactive message (InlineKeyboardMarkup) so the
    // coordinator's `ApprovalDeliveryService` can route approval
    // requests through Telegram without rebuilding the wire-level
    // bot client. Args are the JSON shape of
    // `relix_runtime::approval::ApprovalSendArgs`.
    {
        let api = api.clone();
        bridge.register(
            "telegram.approval_send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_approval_send(api, ctx.args).await }
            })),
        );
    }
    // FIX 49: per-channel health cap. Returns the
    // `ChannelHealthSnapshot` JSON the bridge aggregates
    // into `/v1/health` under `channels.telegram`.
    {
        let state = state.clone();
        bridge.register(
            "telegram.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let snapshot = state.health().snapshot();
                    match serde_json::to_vec(&snapshot) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("telegram.health: serialise snapshot: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            })),
        );
    }
    // FIX 1: webhook-update dispatch cap. The bridge forwards
    // inbound Telegram Updates here when the bot is in webhook
    // mode. The cap parses the raw Update body, converts to
    // `IncomingMessage`, and runs the SAME `handle_one_update`
    // pipeline the long-poll loop uses.
    if let (Some(out_cell), Some(cfg)) = (out_cell, cfg) {
        let api = api.clone();
        let state = state.clone();
        let ring = ring.clone();
        bridge.register(
            "telegram.webhook_update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                let state = state.clone();
                let ring = ring.clone();
                let cfg = cfg.clone();
                let out_cell = out_cell.clone();
                async move {
                    handle_webhook_update(api, out_cell, state, ring, cfg, ctx.args).await
                }
            })),
        );
    }
}

/// FIX 1: parse one inbound Telegram Update + dispatch through
/// the same handler the long-poll loop uses. Returns
/// `HandlerOutcome::Ok(b"{\"ok\":true}")` on success, or an
/// `INVALID_ARGS` envelope on a malformed body. Telegram has
/// already been ACK'd by the bridge route (HTTP 200) — this
/// cap is the in-process dispatch leg.
async fn handle_webhook_update(
    api: Arc<dyn BotApi>,
    out_cell: crate::nodes::telegram::client::TelegramOutboundClientCell,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    cfg: Arc<crate::nodes::telegram::config::TelegramNodeConfig>,
    body: Vec<u8>,
) -> HandlerOutcome {
    // Telegram Update wire shape — `message` for text/voice,
    // `callback_query` for inline-button presses. Anything
    // else (edits, polls, channel posts) is silently ignored.
    let raw: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("telegram.webhook_update: body is not JSON: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let Some(msg) = parse_webhook_update_to_incoming(&raw) else {
        // Unknown update type — ack OK so the bridge does not
        // log spurious failures. Telegram already got its 200.
        return HandlerOutcome::Ok(b"{\"ok\":true,\"dispatched\":false}".to_vec());
    };
    let out = out_cell.get().cloned();
    let out_ref: Option<&dyn crate::nodes::telegram::client::TelegramOutbound> = out
        .as_ref()
        .map(|a| a.as_ref() as &dyn crate::nodes::telegram::client::TelegramOutbound);
    crate::nodes::telegram::controller::handle_one_update(
        api.as_ref(),
        out_ref,
        state.as_ref(),
        ring.as_ref(),
        cfg.as_ref(),
        &msg,
    )
    .await;
    let _ = msg; // suppress unused-warning lint
    HandlerOutcome::Ok(b"{\"ok\":true,\"dispatched\":true}".to_vec())
}

/// FIX 1: parse a Telegram Update JSON shape into an
/// `IncomingMessage`. Mirrors the logic in
/// `relix_telegram::live::update_to_incoming` for the webhook
/// path (which the channel crate's private function does not
/// expose). Returns None for update types we don't model
/// (edits, polls, channel posts, …).
fn parse_webhook_update_to_incoming(
    raw: &serde_json::Value,
) -> Option<relix_telegram::IncomingMessage> {
    use relix_telegram::IncomingMessage;
    let update_id = raw.get("update_id").and_then(|v| v.as_i64()).unwrap_or(0);
    // Callback query path.
    if let Some(cb) = raw.get("callback_query") {
        let data = cb.get("data").and_then(|v| v.as_str()).unwrap_or("");
        if data.is_empty() {
            return None;
        }
        let id = cb
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let from = cb.get("from")?;
        let user_id = from.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        let username = from
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let (chat_id, message_id) = if let Some(m) = cb.get("message") {
            let chat_id = m
                .get("chat")
                .and_then(|c| c.get("id"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let message_id = m.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0);
            (chat_id, message_id)
        } else {
            (0, 0)
        };
        return Some(IncomingMessage {
            update_id,
            chat_id,
            user_id,
            message_id,
            username,
            text: data.to_string(),
            voice_file_id: None,
            callback_query_id: Some(id),
        });
    }
    // Message path.
    let m = raw.get("message")?;
    let from = m.get("from")?;
    let chat = m.get("chat")?;
    let chat_id = chat.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    let user_id = from.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    let username = from
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message_id = m.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0);
    let text = m
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let voice_file_id = m
        .get("voice")
        .and_then(|v| v.get("file_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if text.is_empty() && voice_file_id.is_none() {
        return None;
    }
    Some(IncomingMessage {
        update_id,
        chat_id,
        user_id,
        message_id,
        username,
        text,
        voice_file_id,
        callback_query_id: None,
    })
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

/// PART 8: dispatch one approval request via the rich
/// `TelegramChannelDispatch` (`InlineKeyboardMarkup` buttons,
/// callback_query reply path). Args are JSON shape of
/// [`crate::approval::ApprovalSendArgs`]; `target_id` is parsed
/// as the numeric chat id (Telegram's wire type).
async fn handle_approval_send(api: Arc<dyn BotApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: crate::approval::ApprovalSendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("telegram.approval_send: decode args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let chat_id: i64 = match parsed.target_id.trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "telegram.approval_send: target_id must be a numeric chat id, got {:?}",
                    parsed.target_id
                ),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let request = parsed.to_request();
    let is_escalation = parsed.is_escalation;
    let dispatch = relix_telegram::TelegramChannelDispatch::new(api, chat_id);
    use relix_core::approval::SingleChannelDispatch;
    match dispatch.send(&request, is_escalation).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("telegram.approval_send: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
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

    // ── FIX 1: webhook-update parsing ──────────────────────

    /// FIX 1: text-message Updates parse into `IncomingMessage`
    /// with the same fields the long-poll path produces.
    #[test]
    fn fix1_parse_webhook_text_message() {
        let raw = serde_json::json!({
            "update_id": 7,
            "message": {
                "message_id": 11,
                "from": { "id": 42, "username": "alice" },
                "chat": { "id": 100 },
                "text": "hello"
            }
        });
        let m = parse_webhook_update_to_incoming(&raw).expect("text message parses");
        assert_eq!(m.update_id, 7);
        assert_eq!(m.chat_id, 100);
        assert_eq!(m.user_id, 42);
        assert_eq!(m.message_id, 11);
        assert_eq!(m.username, "alice");
        assert_eq!(m.text, "hello");
        assert!(m.voice_file_id.is_none());
        assert!(m.callback_query_id.is_none());
    }

    /// FIX 1: callback_query Updates surface `data` as the
    /// text body and carry the `callback_query_id` so the
    /// downstream `answerCallbackQuery` (FIX 3) can clear
    /// the spinner.
    #[test]
    fn fix1_parse_webhook_callback_query() {
        let raw = serde_json::json!({
            "update_id": 3,
            "callback_query": {
                "id": "cb-1",
                "from": { "id": 42, "username": "alice" },
                "message": { "message_id": 99, "chat": { "id": 100 } },
                "data": "/approve apr-1"
            }
        });
        let m = parse_webhook_update_to_incoming(&raw).expect("callback_query parses");
        assert_eq!(m.callback_query_id.as_deref(), Some("cb-1"));
        assert_eq!(m.text, "/approve apr-1");
        assert_eq!(m.chat_id, 100);
        assert_eq!(m.message_id, 99);
    }

    /// FIX 1: voice-message Updates surface `voice.file_id` so
    /// the controller can route to `tool.audio.transcribe`.
    #[test]
    fn fix1_parse_webhook_voice_message() {
        let raw = serde_json::json!({
            "update_id": 9,
            "message": {
                "message_id": 21,
                "from": { "id": 42 },
                "chat": { "id": 100 },
                "voice": { "file_id": "AwACAg-fake" }
            }
        });
        let m = parse_webhook_update_to_incoming(&raw).expect("voice parses");
        assert_eq!(m.voice_file_id.as_deref(), Some("AwACAg-fake"));
        assert_eq!(m.text, "");
    }

    /// FIX 1: unknown Update types (e.g. `edited_message`) are
    /// silently skipped — the bridge already 200'd Telegram.
    #[test]
    fn fix1_parse_webhook_unknown_type_returns_none() {
        let raw = serde_json::json!({
            "update_id": 4,
            "edited_message": { "text": "edited" }
        });
        assert!(parse_webhook_update_to_incoming(&raw).is_none());
    }

    /// FIX 1: an empty body or one without `message`/`callback_query`
    /// is not a fatal error — returns None and the caller acks OK.
    #[test]
    fn fix1_parse_webhook_empty_body_returns_none() {
        let raw = serde_json::json!({ "update_id": 0 });
        assert!(parse_webhook_update_to_incoming(&raw).is_none());
    }
}
