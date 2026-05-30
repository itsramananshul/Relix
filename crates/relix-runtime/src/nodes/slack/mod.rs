//! Slack channel node — turns inbound Slack messages into chat-
//! flow runs and posts the agent's reply back to the originating
//! channel.
//!
//! Mirrors `nodes/discord/` but:
//! - Uses Slack's Web API (`auth.test`, `conversations.history`,
//!   `chat.postMessage`) over `slack.com/api/`.
//! - Polling cursor is the per-message `ts` string
//!   (`"1700000000.000200"`) — Slack's `oldest` parameter — not a
//!   snowflake `after`.
//! - **No typing indicator.** Slack has no equivalent REST call
//!   for an ad-hoc typing ping (Socket Mode / Events API has a
//!   `user_typing` event but it's inbound only). The spec
//!   explicitly says do NOT invent an API; chat replies are posted
//!   directly.
//! - Bot self-loop filtering also happens at the SDK parse layer
//!   (subtype set OR bot_id set → dropped) before reaching this
//!   module.
//!
//! Registers two read-only capabilities the bridge proxies for
//! the dashboard:
//!
//! - `slack.status` — bot online state + identity + team.
//! - `slack.messages_recent` — last N inbound messages.

pub mod bot_start_store;
pub mod client;
pub mod commands;
pub mod config;
pub mod controller;
pub mod ring;
pub mod state;

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use relix_slack::{OutgoingMessage, SlackApi};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub use client::{SlackOutboundClient, SlackOutboundClientCell};
pub use config::{
    AiPeerConfig, CoordPeerConfig, MemoryPeerConfig, SlackNodeConfig, SlackNodeError,
};
pub use controller::{run_slack_controller, run_slack_controller_with_api};
pub use ring::{MessageRing, RecordedInbound};
pub use state::ChannelState;

/// Render the `slack.status` body. Pipe-delimited wire shape:
///
/// `online=<bool>|username=<str>|user_id=<str>|team_id=<str>|channel_id=<str>|messages_seen=<u64>|last_message_at=<i64>\n`
pub fn render_status_body(state: &ChannelState, channel_id: &str) -> String {
    let online = state.online();
    let id = state.identity();
    let messages_seen = state.messages_seen();
    let last_at = state.last_message_at().unwrap_or(-1);
    format!(
        "online={online}|username={}|user_id={}|team_id={}|channel_id={channel_id}|messages_seen={messages_seen}|last_message_at={last_at}\n",
        id.username, id.user_id, id.team_id
    )
}

/// Render the `slack.messages_recent` body. One row per recorded
/// inbound, newest-first, tab-separated:
///
/// `ts\tuser_id\tusername\tchannel_id\ttext_preview\n`
pub fn render_recent_body(ring: &MessageRing, limit: usize) -> String {
    let entries = ring.snapshot();
    let take = limit.min(entries.len());
    let mut out = String::new();
    for entry in entries.iter().rev().take(take) {
        let preview = truncate_preview(&entry.text, 100);
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

/// Register `slack.status`, `slack.messages_recent`, and
/// `slack.send` on a controller with `node_type = "slack"`.
///
/// `slack.send` accepts JSON `{ "channel": "<id-or-name>",
/// "text": "..." }` and posts via `chat.postMessage`. Slack
/// accepts both channel ids (`C…`) and channel names
/// (`#ops-alerts`) on the wire.
pub fn register(
    bridge: &mut DispatchBridge,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    channel_id: String,
    api: Arc<dyn SlackApi>,
) {
    {
        let state = state.clone();
        let channel_id = channel_id.clone();
        bridge.register(
            "slack.status",
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
            "slack.messages_recent",
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
            "slack.send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_send(api, ctx.args).await }
            })),
        );
    }
    // PART 8: rich approval dispatch. Routes operator approval
    // requests through the Block-Kit-rendering
    // `SlackChannelDispatch` so the dashboard's approve / deny
    // buttons show up in the workspace.
    {
        let api = api.clone();
        bridge.register(
            "slack.approval_send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let api = api.clone();
                async move { handle_approval_send(api, ctx.args).await }
            })),
        );
    }
    // FIX 49: per-channel health cap — `slack.health`.
    {
        let state = state.clone();
        bridge.register(
            "slack.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let snapshot = state.health().snapshot();
                    match serde_json::to_vec(&snapshot) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("slack.health: serialise snapshot: {e}"),
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
    /// Slack channel id (`C…`) or name (`#ops-alerts`).
    channel: String,
    /// Message body.
    text: String,
}

async fn handle_send(api: Arc<dyn SlackApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: SendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("slack.send: args must be JSON {{channel, text}}: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.channel.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "slack.send: channel must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    if parsed.text.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "slack.send: text must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    // Render CommonMark-ish text into Slack mrkdwn so a coordinator
    // alert / report that ships `**bold**` doesn't show literal
    // asterisks in the Slack client.
    let rendered = crate::nodes::channels::format_for_slack_mrkdwn(&parsed.text);
    let msg = OutgoingMessage {
        channel_id: parsed.channel,
        // No thread reference on outbound coordinator messages.
        thread_ts: String::new(),
        text: rendered,
        blocks: Vec::new(),
    };
    match api.chat_post_message(&msg).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(relix_slack::SlackApiError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("slack.send: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(relix_slack::SlackApiError::ClientError(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("slack.send: {c}"),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(relix_slack::SlackApiError::MissingCredentials) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: "slack.send: bot credentials missing".into(),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// PART 8: dispatch one approval request via the Block-Kit-rich
/// `SlackChannelDispatch`. `target_id` is the channel id
/// (`C…`) the operator configured in
/// `[approval.delivery.channels.slack] channel_id`.
async fn handle_approval_send(api: Arc<dyn SlackApi>, args: Vec<u8>) -> HandlerOutcome {
    let parsed: crate::approval::ApprovalSendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("slack.approval_send: decode args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.target_id.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "slack.approval_send: target_id (channel id) must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let request = parsed.to_request();
    let is_escalation = parsed.is_escalation;
    let dispatch = relix_slack::SlackChannelDispatch::new(api, parsed.target_id);
    use relix_core::approval::SingleChannelDispatch;
    match dispatch.send(&request, is_escalation).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("slack.approval_send: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_slack::BotIdentity;

    #[test]
    fn render_status_body_offline_default_shape() {
        let s = ChannelState::default();
        let body = render_status_body(&s, "C0");
        assert!(body.contains("online=false"));
        assert!(body.contains("channel_id=C0"));
        assert!(body.contains("messages_seen=0"));
        assert!(body.contains("last_message_at=-1"));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn render_status_body_after_online_includes_identity() {
        let s = ChannelState::default();
        s.mark_online(BotIdentity {
            user_id: "U999".into(),
            team_id: "T999".into(),
            bot_id: "B999".into(),
            username: "relixbot".into(),
        });
        let body = render_status_body(&s, "C0");
        assert!(body.contains("online=true"));
        assert!(body.contains("username=relixbot"));
        assert!(body.contains("user_id=U999"));
        assert!(body.contains("team_id=T999"));
    }

    #[test]
    fn render_recent_body_returns_newest_first() {
        let ring = MessageRing::new(200);
        ring.record(RecordedInbound {
            ts: "1700000001.000000".into(),
            user_id: "U1".into(),
            username: "alice".into(),
            channel_id: "C0".into(),
            text: "old".into(),
        });
        ring.record(RecordedInbound {
            ts: "1700000002.000000".into(),
            user_id: "U2".into(),
            username: "bob".into(),
            channel_id: "C0".into(),
            text: "new".into(),
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
            ts: "1.0".into(),
            user_id: "U0".into(),
            username: "alice".into(),
            channel_id: "C0".into(),
            text: long,
        });
        let body = render_recent_body(&ring, 5);
        let preview = body.split('\t').nth(4).unwrap().trim_end_matches('\n');
        assert_eq!(preview.chars().count(), 100);
    }

    // ── slack.send capability tests ──────────────────────

    #[tokio::test]
    async fn slack_send_dispatches_to_mock_with_channel_and_text() {
        let api = std::sync::Arc::new(relix_slack::mock::MockSlackApi::new());
        let dyn_api: std::sync::Arc<dyn SlackApi> = api.clone();
        let args = serde_json::json!({"channel": "#ops-alerts", "text": "hello"})
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
        assert_eq!(sent[0].channel_id, "#ops-alerts");
        assert_eq!(sent[0].text, "hello");
        assert!(sent[0].thread_ts.is_empty());
    }

    #[tokio::test]
    async fn slack_send_rejects_empty_channel() {
        let api: std::sync::Arc<dyn SlackApi> =
            std::sync::Arc::new(relix_slack::mock::MockSlackApi::new());
        let args = serde_json::json!({"channel": "", "text": "hi"})
            .to_string()
            .into_bytes();
        match handle_send(api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn slack_send_returns_responder_internal_on_api_failure() {
        let api = std::sync::Arc::new(relix_slack::mock::MockSlackApi::new());
        api.fail_next_send(relix_slack::SlackApiError::ClientError(
            "missing_scope".into(),
        ));
        let dyn_api: std::sync::Arc<dyn SlackApi> = api;
        let args = serde_json::json!({"channel": "C1", "text": "x"})
            .to_string()
            .into_bytes();
        match handle_send(dyn_api, args).await {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::RESPONDER_INTERNAL),
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }
}
