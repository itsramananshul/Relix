//! Long-polling loop + per-update handler for the telegram
//! controller. Pure-orchestration code: it dispatches to a
//! `BotApi` (live or mock), a `TelegramOutbound` (live mesh
//! client or test stub), and the shared in-memory state.

use std::sync::Arc;
use std::time::Duration;

use relix_telegram::{BotApi, IncomingMessage, OutgoingMessage, ParseMode, derive_channel_subject};

use super::client::{TelegramOutbound, TelegramOutboundClientCell};
use super::commands::{
    Command, approval_notification, brain_unreachable_message, help_message, memory_body,
    status_body, unauthorised_message, voice_transcription_failed_message,
    voice_transcription_unavailable_message, welcome_message,
};
use super::config::TelegramNodeConfig;
use super::ring::{MessageRing, RecordedInbound};
use super::state::{ChannelState, NotifierState};

/// History budget passed to `ai.chat`. Keeps the prompt
/// payload bounded regardless of session age.
const HISTORY_TURNS: usize = 10;

/// Run the telegram controller forever. Re-fetches the
/// outbound cell on every poll so a late-startup wiring is
/// picked up without a restart.
pub async fn run_telegram_controller(
    api: Arc<dyn BotApi>,
    out_cell: TelegramOutboundClientCell,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    notifier: Arc<NotifierState>,
    cfg: Arc<TelegramNodeConfig>,
) {
    // Verify the token once. Failure here keeps the
    // controller running so the bridge's status endpoint
    // can still report online=false — we just back off and
    // retry rather than wedging the process.
    match api.get_me().await {
        Ok(id) => {
            tracing::info!(
                username = %id.username,
                user_id = id.user_id,
                "Telegram bot online: @{}",
                id.username
            );
            state.mark_online(id);
        }
        Err(e) => {
            tracing::error!(error = %e, "Telegram bot getMe failed; controller will idle");
            // Sleep so we don't hot-loop on a permanently
            // bad token. The dashboard will show offline.
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    // Spawn the approval-notifier loop in the background
    // when the operator has wired a chat id. Lives for the
    // lifetime of the process; loops independently of the
    // long-poll path.
    if cfg.approval_notifications_enabled() {
        let api2 = api.clone();
        let out_cell2 = out_cell.clone();
        let notifier2 = notifier.clone();
        let cfg2 = cfg.clone();
        tokio::spawn(async move {
            run_approval_notifier(api2, out_cell2, notifier2, cfg2).await;
        });
    }

    // Main long-poll loop. `offset` advances past the
    // last update we processed so Telegram doesn't replay
    // history on restart.
    let mut offset: i64 = 0;
    loop {
        let updates = match api.get_updates(offset).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: get_updates failed; backing off");
                tokio::time::sleep(Duration::from_secs(cfg.poll_interval_secs.max(1))).await;
                continue;
            }
        };
        if updates.is_empty() {
            tokio::time::sleep(Duration::from_secs(cfg.poll_interval_secs.max(1))).await;
            continue;
        }
        for upd in &updates {
            offset = offset.max(upd.update_id + 1);
            let out = out_cell.get().cloned();
            let out_ref: Option<&dyn TelegramOutbound> =
                out.as_ref().map(|a| a.as_ref() as &dyn TelegramOutbound);
            handle_one_update(&*api, out_ref, &state, &ring, &cfg, upd).await;
        }
    }
}

/// Run the controller with a caller-provided `BotApi` (the
/// production entrypoint wraps a `LiveBotApi`). Forwarding
/// shim so the runtime can pick an arbitrary impl without
/// pulling `LiveBotApi` into its public surface.
pub async fn run_telegram_controller_with_api<B: BotApi + 'static>(
    api: B,
    out_cell: TelegramOutboundClientCell,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    notifier: Arc<NotifierState>,
    cfg: Arc<TelegramNodeConfig>,
) {
    let api: Arc<dyn BotApi> = Arc::new(api);
    run_telegram_controller(api, out_cell, state, ring, notifier, cfg).await
}

/// Process one inbound update: authorise, route on the
/// parsed command, dispatch.
///
/// FIX 3: when `msg` originated from an inline-button press
/// (`msg.is_callback_query()`), we call `answerCallbackQuery`
/// AFTER the rest of the handler returns so Telegram clears
/// the operator's spinning button indicator. The ack call
/// happens unconditionally — Telegram drops the spinner only
/// when the ack reaches its servers, and a missing ack leaves
/// the operator's UI stuck for 15s. Failure to ack is logged
/// at WARN and otherwise ignored: a missed spinner is annoying
/// but the decision itself has already been recorded.
pub async fn handle_one_update(
    api: &dyn BotApi,
    out: Option<&dyn TelegramOutbound>,
    state: &ChannelState,
    ring: &MessageRing,
    cfg: &TelegramNodeConfig,
    msg: &IncomingMessage,
) {
    handle_one_update_inner(api, out, state, ring, cfg, msg).await;
    if let Some(cb_id) = msg.callback_query_id.as_deref()
        && let Err(e) = api
            .answer_callback_query(cb_id, Some("Decision recorded"))
            .await
    {
        tracing::warn!(
            callback_query_id = cb_id,
            error = %e,
            "telegram: answerCallbackQuery failed; operator UI spinner may stick \
             for ~15s but the decision has already been recorded"
        );
    }
}

/// Inner handler body. Split out from `handle_one_update` so
/// the outer wrapper can run a post-processing
/// `answerCallbackQuery` even when the inner body returns
/// early via an authorisation deny or transcription failure.
async fn handle_one_update_inner(
    api: &dyn BotApi,
    out: Option<&dyn TelegramOutbound>,
    state: &ChannelState,
    ring: &MessageRing,
    cfg: &TelegramNodeConfig,
    msg: &IncomingMessage,
) {
    let ts = unix_now();
    state.record_inbound(ts);
    ring.record(RecordedInbound {
        ts,
        user_id: msg.user_id,
        username: msg.username.clone(),
        chat_id: msg.chat_id,
        text: msg.text.clone(),
    });

    if !cfg.user_is_allowed(msg.user_id) {
        let _ = send_text(api, msg.chat_id, msg.message_id, unauthorised_message()).await;
        return;
    }

    // Voice messages: when the operator wired an audio peer,
    // download the file via Telegram's getFile + getFile-download
    // pair and route the bytes through `tool.audio.transcribe`.
    // The resulting transcript becomes the prompt and falls
    // through into the regular chat flow. Both the
    // "no audio peer configured" and "transcription failed"
    // branches surface a clear user-facing reply per
    // `commands.rs` so the user is never left in silence.
    let resolved_text: String = if let Some(file_id) = msg.voice_file_id.as_deref() {
        let Some(out) = out else {
            let _ = send_text(
                api,
                msg.chat_id,
                msg.message_id,
                brain_unreachable_message(),
            )
            .await;
            return;
        };
        // No audio peer ⇒ static fallback. Cheaper to check
        // here than dispatch into the client and rely on the
        // `None` return path.
        let bytes = match api.get_file_bytes(file_id).await {
            Ok(b) if !b.is_empty() => b,
            Ok(_) => {
                tracing::warn!(file_id, "telegram: getFile returned empty body");
                let _ = send_text(
                    api,
                    msg.chat_id,
                    msg.message_id,
                    voice_transcription_failed_message(),
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(error = %e, file_id, "telegram: getFile failed");
                let _ = send_text(
                    api,
                    msg.chat_id,
                    msg.message_id,
                    voice_transcription_failed_message(),
                )
                .await;
                return;
            }
        };
        match out.tool_audio_transcribe(bytes).await {
            Some(t) if !t.trim().is_empty() => t,
            Some(_) | None => {
                let body = if cfg.audio_peer.is_none() {
                    voice_transcription_unavailable_message()
                } else {
                    voice_transcription_failed_message()
                };
                let _ = send_text(api, msg.chat_id, msg.message_id, body).await;
                return;
            }
        }
    } else {
        msg.text.clone()
    };

    let cmd = Command::parse(&resolved_text);
    match cmd {
        Command::Start => {
            let _ = send_text(api, msg.chat_id, msg.message_id, &welcome_message()).await;
        }
        Command::Help => {
            let _ = send_text(api, msg.chat_id, msg.message_id, &help_message()).await;
        }
        Command::Status => {
            let body = status_body(&render_status_summary(state, cfg));
            let _ = send_text(api, msg.chat_id, msg.message_id, &body).await;
        }
        Command::Memory => {
            let subject = derive_channel_subject(msg.chat_id, msg.user_id);
            let (agent, user) = match out {
                Some(o) => o.memory_agent_read(&subject.subject_id.to_string()).await,
                None => (String::new(), String::new()),
            };
            let body = memory_body(&agent, &user);
            let _ = send_text(api, msg.chat_id, msg.message_id, &body).await;
        }
        Command::Forget => {
            let subject = derive_channel_subject(msg.chat_id, msg.user_id);
            if let Some(o) = out {
                o.memory_agent_clear(&subject.subject_id.to_string()).await;
            }
            let _ = send_text(api, msg.chat_id, msg.message_id, "🧹 Memory cleared.").await;
        }
        Command::Approve(approval_id) => {
            handle_approval_decision(api, out, cfg, msg, &approval_id, "approved", "/approve")
                .await;
        }
        Command::Reject(approval_id) => {
            handle_approval_decision(api, out, cfg, msg, &approval_id, "rejected", "/reject").await;
        }
        Command::Chat(text) => {
            run_chat_flow(api, out, state, cfg, msg, &text).await;
        }
    }
}

/// FIX 7 — shared approve/deny pipeline. The new contract:
///
/// 1. Fetch the approval row via `coord.approval.get` so the
///    controller can both verify it exists and read its
///    `authorized_approvers` allow-list.
/// 2. Verify the caller's Telegram chat_id (rendered as
///    `telegram:<id>`) is listed in `authorized_approvers`.
///    The legacy `operator_chat_id` config still admits when
///    it is set + matches — backwards-compatible escape hatch
///    so a deployment that hasn't yet populated
///    `authorized_approvers` keeps working.
/// 3. Call `approval.record_decision` (NOT the legacy
///    `coord.approval.decide` path) so the documented
///    coordinator cap flips the row + fires the escalation
///    cancel signal.
///
/// Unauthorised callers get a friendly reply and the decision
/// is NOT recorded. The function NEVER touches any store
/// directly — every state transition goes through a cap.
async fn handle_approval_decision(
    api: &dyn BotApi,
    out: Option<&dyn TelegramOutbound>,
    cfg: &TelegramNodeConfig,
    msg: &IncomingMessage,
    approval_id: &str,
    decision: &'static str, // "approved" | "rejected"
    cmd_tag: &'static str,  // "/approve" | "/reject"
) {
    let approval_id = approval_id.trim();
    if approval_id.is_empty() {
        let _ = send_text(
            api,
            msg.chat_id,
            msg.message_id,
            &format!("Usage: {cmd_tag} <approval_id>"),
        )
        .await;
        return;
    }
    let Some(o) = out else {
        let _ = send_text(
            api,
            msg.chat_id,
            msg.message_id,
            "⚠️ Coordinator unreachable.",
        )
        .await;
        return;
    };
    // Step 1: verify existence via coord.approval.get.
    let row = match o.approval_get(approval_id).await {
        Some(v) => v,
        None => {
            let _ = send_text(
                api,
                msg.chat_id,
                msg.message_id,
                &format!("⚠️ Approval {approval_id} not found."),
            )
            .await;
            return;
        }
    };
    // Step 2: chat_id authorisation. The legacy
    // `operator_chat_id` config still admits as a
    // backwards-compatible escape hatch (a deployment that
    // hasn't migrated to `authorized_approvers` keeps
    // working). The PRIMARY gate is the per-approval
    // `authorized_approvers` list — operators wire this when
    // creating approval requests destined for Telegram.
    let caller_token = format!("telegram:{}", msg.user_id);
    let operator_chat_admits = cfg.operator_chat_id != 0 && msg.chat_id == cfg.operator_chat_id;
    let listed = row
        .get("authorized_approvers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|x| x.as_str() == Some(caller_token.as_str()))
        })
        .unwrap_or(false);
    if !operator_chat_admits && !listed {
        let _ = send_text(
            api,
            msg.chat_id,
            msg.message_id,
            &format!(
                "🚫 You are not an authorised approver for {approval_id}. \
                 Ask an operator to add `telegram:{}` to the approval's \
                 `authorized_approvers` list.",
                msg.user_id
            ),
        )
        .await;
        return;
    }
    // Step 3: record the decision via the documented cap.
    let note = format!("via=telegram chat_id={}", msg.chat_id);
    let body = match o
        .approval_record_decision(approval_id, decision, &note)
        .await
    {
        Some(_) => match decision {
            "approved" => format!("✅ Approved {approval_id}."),
            "rejected" => format!("❌ Rejected {approval_id}."),
            other => format!("Recorded decision={other} for {approval_id}."),
        },
        None => format!("⚠️ Failed to record decision for {approval_id}."),
    };
    let _ = send_text(api, msg.chat_id, msg.message_id, &body).await;
}

/// Inline `/status` body rendered locally so the handler
/// stays self-contained.
fn render_status_summary(state: &ChannelState, cfg: &TelegramNodeConfig) -> String {
    let identity = state.identity();
    format!(
        "bot=@{}\nuser_id={}\nonline={}\nmessages_seen={}\noperator_chat_id={}\nallow_everyone={}",
        identity.username,
        identity.user_id,
        state.online(),
        state.messages_seen(),
        cfg.operator_chat_id,
        cfg.allow_everyone()
    )
}

/// The chat-flow path. Equivalent in shape to running
/// `flows/chat_template.sol`: read history, dispatch
/// `ai.chat`, persist both halves of the turn, post the
/// reply.
async fn run_chat_flow(
    api: &dyn BotApi,
    out: Option<&dyn TelegramOutbound>,
    _state: &ChannelState,
    cfg: &TelegramNodeConfig,
    msg: &IncomingMessage,
    text: &str,
) {
    let subject = derive_channel_subject(msg.chat_id, msg.user_id);
    let session_id = subject.subject_id.to_string();
    // Best-effort typing indicator — failures are silent.
    let _ = api.send_chat_action(msg.chat_id, "typing").await;

    // No outbound wiring yet (mesh discovery pending). Tell
    // the user honestly rather than spinning silently.
    let Some(out) = out else {
        let _ = send_text(
            api,
            msg.chat_id,
            msg.message_id,
            brain_unreachable_message(),
        )
        .await;
        return;
    };

    let history = out.memory_recent(&session_id, HISTORY_TURNS).await;
    let history_text = render_history(&history);

    // Best-effort durable record. Failure must NOT block
    // the reply path; chat is the operator's foreground UX.
    let task_id = out
        .task_create(
            &message_title(text),
            &flow_template_str(cfg),
            "",
            &session_id,
        )
        .await;
    if let Some(t) = task_id.as_deref() {
        out.task_event(
            t,
            "task.telegram.inbound",
            &format!("chat_id={}", msg.chat_id),
        )
        .await;
    }

    // RELIX-7.7 GAP 2: consult the coordinator's routing
    // rules. Telegram has no subject concept so we pass an
    // empty string; the router silently skips subject_match for
    // telegram (`ChannelType::has_subject` returns false).
    let preview: String = text.chars().take(200).collect();
    let routed = out
        .routing_resolve("telegram", &format!("@{}", msg.username), "", &preview)
        .await;
    let reply = match routed {
        Some((peer, capability)) => {
            tracing::info!(
                chat_id = msg.chat_id,
                from = %msg.username,
                target_peer = %peer,
                capability = %capability,
                "telegram: routed via ChannelRouter"
            );
            out.dispatch_chat(&peer, &capability, &session_id, text, &history_text)
                .await
        }
        None => out.ai_chat(&session_id, text, &history_text).await,
    };
    let reply = match reply {
        Some(r) if !r.trim().is_empty() => r,
        _ => {
            if let Some(t) = task_id.as_deref() {
                out.task_update_status(t, "failed", "ai_chat unreachable / empty")
                    .await;
            }
            let _ = send_text(
                api,
                msg.chat_id,
                msg.message_id,
                brain_unreachable_message(),
            )
            .await;
            return;
        }
    };

    // Persist both halves. Order matters: user turn first
    // so its update_id is older than the assistant's.
    out.memory_write(&session_id, "user", text).await;
    out.memory_write(&session_id, "assistant", &reply).await;

    let _ = send_text(api, msg.chat_id, msg.message_id, &reply).await;

    if let Some(t) = task_id.as_deref() {
        out.task_update_status(t, "completed", "ok").await;
    }
}

fn flow_template_str(cfg: &TelegramNodeConfig) -> String {
    cfg.flow_template.as_path().display().to_string()
}

fn message_title(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return "telegram-message".to_string();
    }
    // Cap so the title column doesn't get a wall of text.
    let truncated: String = first_line.chars().take(80).collect();
    format!("telegram: {truncated}")
}

fn render_history(history: &[(String, String)]) -> String {
    // Use a Hermes-style `[role] text` form so the prompt
    // is unambiguous when the AI peer splits it back out.
    let mut out = String::new();
    for (role, text) in history {
        out.push_str(&format!("[{role}] {text}\n"));
    }
    out
}

async fn send_text(api: &dyn BotApi, chat_id: i64, reply_to: i64, text: &str) -> bool {
    // Format the assistant text as MarkdownV2 so code fences,
    // multi-paragraph replies, and reserved characters render
    // correctly in Telegram clients. The formatter escapes
    // everything outside code fences; inside a fence only the
    // backtick + backslash get escaped. See
    // `crate::nodes::channels::format_for_telegram_markdown_v2`.
    let formatted = crate::nodes::channels::format_for_telegram_markdown_v2(text);
    let msg = OutgoingMessage {
        chat_id,
        reply_to_message_id: reply_to,
        text: formatted,
        parse_mode: Some(ParseMode::MarkdownV2),
        reply_markup: None,
    };
    if let Err(e) = api.send_message(&msg).await {
        // Fallback: if the rich-text send failed (Bot API
        // rejected the markdown for any reason), try once more
        // as plain text so the operator sees the reply rather
        // than a silent drop.
        tracing::warn!(error = %e, chat_id = chat_id,
            "telegram: rich-text send_message failed; retrying as plain text");
        let fallback = OutgoingMessage {
            chat_id,
            reply_to_message_id: reply_to,
            text: text.to_string(),
            parse_mode: None,
            reply_markup: None,
        };
        if let Err(e2) = api.send_message(&fallback).await {
            tracing::warn!(error = %e2, chat_id = chat_id,
                "telegram: plain-text send_message also failed");
            return false;
        }
    }
    true
}

// ── Approval notifier ──────────────────────────────────────

async fn run_approval_notifier(
    api: Arc<dyn BotApi>,
    out_cell: TelegramOutboundClientCell,
    notifier: Arc<NotifierState>,
    cfg: Arc<TelegramNodeConfig>,
) {
    let mut interval =
        tokio::time::interval(Duration::from_secs(cfg.approval_poll_interval_secs.max(5)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let Some(out) = out_cell.get().cloned() else {
            continue;
        };
        let pending = out.task_list(Some("awaiting_input"), 50).await;
        for (task_id, _status, title) in pending {
            if !notifier.mark_notified(&task_id) {
                continue;
            }
            // Render a generic notification — we don't know
            // the specific reason from `task.list`; this is
            // the alpha posture (richer detail can come from
            // a follow-up `task.get`).
            let body =
                approval_notification(&task_id, "(see dashboard)", "(awaiting_input)", &title);
            let out_msg = OutgoingMessage {
                chat_id: cfg.operator_chat_id,
                reply_to_message_id: 0,
                text: body,
                parse_mode: Some(ParseMode::MarkdownV2),
                reply_markup: None,
            };
            if let Err(e) = api.send_message(&out_msg).await {
                tracing::warn!(error = %e, "telegram: approval notify send failed");
            }
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use relix_telegram::{BotApiError, BotIdentity, mock::MockBotApi};
    use std::sync::Mutex;

    fn cfg_default() -> TelegramNodeConfig {
        toml::from_str(
            r#"
            token_env = "X"
            [memory_peer]
            addr = "a"
            [ai_peer]
            addr = "b"
            [coord_peer]
            addr = "c"
        "#,
        )
        .unwrap()
    }

    fn cfg_with_allow_list(users: &[i64]) -> TelegramNodeConfig {
        let mut cfg = cfg_default();
        cfg.allowed_users = users.to_vec();
        cfg
    }

    fn cfg_with_operator(chat: i64) -> TelegramNodeConfig {
        let mut cfg = cfg_default();
        cfg.operator_chat_id = chat;
        cfg
    }

    fn msg(text: &str) -> IncomingMessage {
        IncomingMessage {
            update_id: 1,
            chat_id: 100,
            user_id: 42,
            message_id: 7,
            username: "alice".into(),
            text: text.into(),
            voice_file_id: None,
            callback_query_id: None,
        }
    }

    fn voice_msg(file_id: &str) -> IncomingMessage {
        IncomingMessage {
            update_id: 1,
            chat_id: 100,
            user_id: 42,
            message_id: 7,
            username: "alice".into(),
            text: String::new(),
            voice_file_id: Some(file_id.into()),
            callback_query_id: None,
        }
    }

    #[derive(Default)]
    struct StubOutbound {
        memory_recent_replies: Mutex<Vec<(String, String)>>,
        memory_writes: Mutex<Vec<(String, String, String)>>,
        agent_clear_calls: Mutex<u32>,
        agent_read_reply: Mutex<(String, String)>,
        ai_chat_reply: Mutex<Option<String>>,
        ai_chat_calls: Mutex<Vec<(String, String, String)>>,
        task_creates: Mutex<Vec<(String, String, String, String)>>,
        task_create_id: Mutex<Option<String>>,
        task_updates: Mutex<Vec<(String, String, String)>>,
        task_events: Mutex<Vec<(String, String, String)>>,
        task_list_reply: Mutex<Vec<(String, String, String)>>,
        audio_reply: Mutex<Option<String>>,
        audio_calls: Mutex<Vec<Vec<u8>>>,
        // FIX 7: per-approval-id `approval.get` reply staging.
        // Empty ⇒ `approval_get` returns None (controller maps
        // to "approval not found").
        approval_get_replies: Mutex<std::collections::HashMap<String, serde_json::Value>>,
        // FIX 7: recorded `approval.record_decision` calls
        // — (approval_id, decision, note). Tests assert
        // exactly which calls landed.
        approval_record_calls: Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl TelegramOutbound for StubOutbound {
        async fn memory_recent(&self, _session_id: &str, _n: usize) -> Vec<(String, String)> {
            self.memory_recent_replies.lock().unwrap().clone()
        }
        async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
            self.memory_writes
                .lock()
                .unwrap()
                .push((session_id.into(), role.into(), text.into()));
        }
        async fn memory_agent_read(&self, _subject_id: &str) -> (String, String) {
            self.agent_read_reply.lock().unwrap().clone()
        }
        async fn memory_agent_clear(&self, _subject_id: &str) {
            *self.agent_clear_calls.lock().unwrap() += 1;
        }
        async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
            self.ai_chat_calls.lock().unwrap().push((
                session_id.into(),
                prompt.into(),
                history.into(),
            ));
            self.ai_chat_reply.lock().unwrap().clone()
        }
        async fn task_create(
            &self,
            title: &str,
            flow_template: &str,
            params_json: &str,
            owner_subject_id: &str,
        ) -> Option<String> {
            self.task_creates.lock().unwrap().push((
                title.into(),
                flow_template.into(),
                params_json.into(),
                owner_subject_id.into(),
            ));
            self.task_create_id.lock().unwrap().clone()
        }
        async fn task_update_status(&self, task_id: &str, status: &str, result: &str) {
            self.task_updates
                .lock()
                .unwrap()
                .push((task_id.into(), status.into(), result.into()));
        }
        async fn task_event(&self, task_id: &str, event_type: &str, payload: &str) {
            self.task_events.lock().unwrap().push((
                task_id.into(),
                event_type.into(),
                payload.into(),
            ));
        }
        async fn task_list(
            &self,
            _status_filter: Option<&str>,
            _limit: usize,
        ) -> Vec<(String, String, String)> {
            self.task_list_reply.lock().unwrap().clone()
        }
        async fn approval_decide(
            &self,
            _approval_id: &str,
            decision: &str,
            _decided_by: &str,
            _note: &str,
        ) -> Option<String> {
            if decision == "approved" {
                Some("ok|deadbeefdeadbeefdeadbeefdeadbeef\n".to_string())
            } else {
                Some("ok\n".to_string())
            }
        }
        async fn approval_get(&self, approval_id: &str) -> Option<serde_json::Value> {
            self.approval_get_replies
                .lock()
                .unwrap()
                .get(approval_id)
                .cloned()
        }
        async fn approval_record_decision(
            &self,
            approval_id: &str,
            decision: &str,
            note: &str,
        ) -> Option<String> {
            self.approval_record_calls.lock().unwrap().push((
                approval_id.into(),
                decision.into(),
                note.into(),
            ));
            Some("{\"ok\":true}".into())
        }
        async fn tool_audio_transcribe(&self, audio_bytes: Vec<u8>) -> Option<String> {
            self.audio_calls.lock().unwrap().push(audio_bytes);
            self.audio_reply.lock().unwrap().clone()
        }
    }

    fn state_online() -> ChannelState {
        let s = ChannelState::default();
        s.mark_online(BotIdentity {
            user_id: 1,
            username: "relixbot".into(),
            first_name: "Relix".into(),
        });
        s
    }

    #[tokio::test]
    async fn start_command_replies_with_welcome_message() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/start")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("Welcome"));
    }

    #[tokio::test]
    async fn help_command_lists_commands() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/help")).await;
        let sent = api.sent_messages();
        assert!(sent[0].text.contains("/approve"));
    }

    #[tokio::test]
    async fn unauthorised_user_gets_static_message_no_dispatch() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&[999]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hello")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].text,
            crate::nodes::channels::format_for_telegram_markdown_v2(unauthorised_message())
        );
        // AI was not invoked.
        assert!(out.ai_chat_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allowed_user_gets_chat_routed_to_ai() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("hello from ai".into());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&[42]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi there")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].text,
            crate::nodes::channels::format_for_telegram_markdown_v2("hello from ai")
        );
        // Both user + assistant turns persisted.
        assert_eq!(out.memory_writes.lock().unwrap().len(), 2);
        // Typing indicator fired before the chat dispatch.
        let actions = api.chat_actions();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].1, "typing");
        // Task created + completed.
        assert_eq!(out.task_creates.lock().unwrap().len(), 1);
        let updates = out.task_updates.lock().unwrap();
        assert!(updates.iter().any(|(_, s, _)| s == "completed"));
    }

    #[tokio::test]
    async fn chat_with_no_outbound_falls_back_to_brain_unreachable() {
        let api = MockBotApi::new();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, None, &state, &ring, &cfg, &msg("hi")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].text,
            crate::nodes::channels::format_for_telegram_markdown_v2(brain_unreachable_message())
        );
    }

    #[tokio::test]
    async fn voice_message_with_no_audio_peer_returns_unavailable_fallback() {
        let api = MockBotApi::new();
        api.stage_file_bytes("voice-1", vec![0xAA, 0xBB, 0xCC]);
        let out = StubOutbound::default();
        // audio_reply stays None and audio_peer is unset in
        // cfg_default — the transcribe call returns None and
        // we expect the "unavailable" reply (distinct from
        // the "failed" reply).
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        assert!(
            cfg.audio_peer.is_none(),
            "fixture should not configure audio"
        );
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &voice_msg("voice-1")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0].text,
            crate::nodes::channels::format_for_telegram_markdown_v2(
                voice_transcription_unavailable_message()
            )
        );
        // The audio bytes still got handed to the transcriber
        // stub (so the stub can record that the call was made),
        // but no chat happened.
        assert!(out.ai_chat_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn voice_message_with_successful_transcription_routes_to_ai_chat() {
        let api = MockBotApi::new();
        api.stage_file_bytes("voice-2", b"raw-audio-bytes".to_vec());
        let out = StubOutbound::default();
        *out.audio_reply.lock().unwrap() = Some("hello transcribed".into());
        *out.ai_chat_reply.lock().unwrap() = Some("AI reply".into());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let mut cfg = cfg_default();
        // Mark cfg.audio_peer as configured by parsing one.
        cfg.audio_peer = Some(
            toml::from_str(
                r#"
                    addr = "/ip4/127.0.0.1/tcp/19720"
                    alias = "tool"
                "#,
            )
            .unwrap(),
        );
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &voice_msg("voice-2")).await;
        // Transcriber saw the audio bytes.
        let calls = out.audio_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(&calls[0], b"raw-audio-bytes");
        // AI saw the transcript as the prompt.
        let chat_calls = out.ai_chat_calls.lock().unwrap();
        assert_eq!(chat_calls.len(), 1);
        assert_eq!(chat_calls[0].1, "hello transcribed");
        // User got the AI reply, not a voice fallback.
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].text.contains("AI reply"));
    }

    #[tokio::test]
    async fn ai_chat_empty_reply_falls_back_to_brain_unreachable() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some(String::new());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        let sent = api.sent_messages();
        assert_eq!(
            sent[0].text,
            crate::nodes::channels::format_for_telegram_markdown_v2(brain_unreachable_message())
        );
        // Task flipped to failed.
        let updates = out.task_updates.lock().unwrap();
        assert!(updates.iter().any(|(_, s, _)| s == "failed"));
    }

    #[tokio::test]
    async fn memory_command_renders_agent_user_blobs() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.agent_read_reply.lock().unwrap() = ("agent-x".into(), "user-y".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/memory")).await;
        let sent = api.sent_messages();
        // After the MarkdownV2 escape, `-` becomes `\-`. Check
        // the escaped substrings so the test matches the new
        // outbound contract.
        assert!(sent[0].text.contains("agent\\-x"));
        assert!(sent[0].text.contains("user\\-y"));
    }

    #[tokio::test]
    async fn forget_command_clears_memory_and_acks() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/forget")).await;
        assert_eq!(*out.agent_clear_calls.lock().unwrap(), 1);
        assert!(api.sent_messages()[0].text.contains("Memory cleared"));
    }

    /// FIX 7: a non-existent approval (no stub reply) yields
    /// "not found" — the get-first contract.
    #[tokio::test]
    async fn fix7_approve_command_unknown_approval_id_returns_not_found() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        // operator_chat matches so the legacy escape hatch is
        // open; the get-first contract still rejects because
        // the approval row doesn't exist.
        let cfg = cfg_with_operator(100);
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &msg("/approve apr-missing"),
        )
        .await;
        let sent = api.sent_messages();
        assert!(
            sent[0].text.contains("not found"),
            "expected `not found` in: {}",
            sent[0].text
        );
        assert!(
            out.approval_record_calls.lock().unwrap().is_empty(),
            "no record_decision call should fire for a missing approval"
        );
    }

    /// FIX 7: chat_id NOT listed in `authorized_approvers` is
    /// rejected with a friendly explanatory message AND no
    /// state mutation. This is the new security gate.
    #[tokio::test]
    async fn fix7_approve_command_unauthorized_chat_id_rejected() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        // Stage an approval that allow-lists a DIFFERENT chat.
        out.approval_get_replies.lock().unwrap().insert(
            "apr-1".into(),
            serde_json::json!({
                "approval_id": "apr-1",
                "status": "pending",
                "authorized_approvers": ["telegram:999"],
            }),
        );
        let state = state_online();
        let ring = MessageRing::new(50);
        // No operator chat configured; only the per-approval
        // allow-list applies. msg.user_id = 42; approval allows
        // 999 — must be denied.
        let cfg = cfg_default();
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &msg("/approve apr-1"),
        )
        .await;
        let sent = api.sent_messages();
        assert!(
            sent[0].text.contains("not an authorised approver"),
            "expected authorization deny copy in: {}",
            sent[0].text
        );
        assert!(
            out.approval_record_calls.lock().unwrap().is_empty(),
            "unauthorized caller must NEVER trigger record_decision"
        );
    }

    /// FIX 7: chat_id LISTED in `authorized_approvers` is
    /// admitted and calls `approval.record_decision`.
    #[tokio::test]
    async fn fix7_approve_command_authorized_chat_id_calls_record_decision() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        out.approval_get_replies.lock().unwrap().insert(
            "apr-1".into(),
            serde_json::json!({
                "approval_id": "apr-1",
                "status": "pending",
                "authorized_approvers": ["telegram:42"],
            }),
        );
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &msg("/approve apr-1"),
        )
        .await;
        let sent = api.sent_messages();
        assert!(
            sent[0].text.contains("Approved apr\\-1"),
            "expected approval ack in: {}",
            sent[0].text
        );
        let calls = out.approval_record_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one record_decision call");
        assert_eq!(calls[0].0, "apr-1");
        assert_eq!(calls[0].1, "approved");
        assert!(
            calls[0].2.contains("via=telegram"),
            "note must include via=telegram tag: {}",
            calls[0].2
        );
    }

    /// FIX 7: `/reject` end-to-end with an authorized chat.
    #[tokio::test]
    async fn fix7_reject_command_authorized_chat_id_calls_record_decision() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        out.approval_get_replies.lock().unwrap().insert(
            "apr-1".into(),
            serde_json::json!({
                "approval_id": "apr-1",
                "authorized_approvers": ["telegram:42"],
            }),
        );
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/reject apr-1")).await;
        let sent = api.sent_messages();
        assert!(sent[0].text.contains("Rejected apr\\-1"));
        let calls = out.approval_record_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "rejected");
    }

    /// FIX 7: the legacy `operator_chat_id` escape hatch
    /// still admits the operator chat even when no
    /// `authorized_approvers` entry matches — backwards
    /// compatibility for pre-FIX-7 deployments.
    #[tokio::test]
    async fn fix7_operator_chat_id_remains_a_backwards_compatible_escape_hatch() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        out.approval_get_replies.lock().unwrap().insert(
            "apr-1".into(),
            serde_json::json!({
                "approval_id": "apr-1",
                "authorized_approvers": [], // empty
            }),
        );
        let state = state_online();
        let ring = MessageRing::new(50);
        // msg.chat_id == 100; operator_chat_id matches.
        let cfg = cfg_with_operator(100);
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &msg("/approve apr-1"),
        )
        .await;
        let sent = api.sent_messages();
        assert!(sent[0].text.contains("Approved apr\\-1"));
        assert_eq!(out.approval_record_calls.lock().unwrap().len(), 1);
    }

    /// FIX 7: empty approval_id surfaces a usage hint.
    #[tokio::test]
    async fn fix7_approve_command_with_empty_id_surfaces_usage() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/approve")).await;
        let sent = api.sent_messages();
        assert!(sent[0].text.contains("Usage"));
        assert!(sent[0].text.contains("/approve"));
        assert!(out.approval_record_calls.lock().unwrap().is_empty());
    }

    /// FIX 7: a transport failure on `approval.record_decision`
    /// surfaces the friendly "Failed to record decision"
    /// message rather than crashing.
    #[tokio::test]
    async fn fix7_record_decision_failure_surfaces_friendly_error() {
        struct FailingRecord {
            inner: StubOutbound,
        }
        #[async_trait]
        impl TelegramOutbound for FailingRecord {
            async fn memory_recent(&self, s: &str, n: usize) -> Vec<(String, String)> {
                self.inner.memory_recent(s, n).await
            }
            async fn memory_write(&self, s: &str, r: &str, t: &str) {
                self.inner.memory_write(s, r, t).await
            }
            async fn memory_agent_read(&self, s: &str) -> (String, String) {
                self.inner.memory_agent_read(s).await
            }
            async fn memory_agent_clear(&self, s: &str) {
                self.inner.memory_agent_clear(s).await
            }
            async fn ai_chat(&self, s: &str, p: &str, h: &str) -> Option<String> {
                self.inner.ai_chat(s, p, h).await
            }
            async fn task_create(&self, t: &str, f: &str, p: &str, o: &str) -> Option<String> {
                self.inner.task_create(t, f, p, o).await
            }
            async fn task_update_status(&self, t: &str, s: &str, r: &str) {
                self.inner.task_update_status(t, s, r).await
            }
            async fn task_event(&self, t: &str, e: &str, p: &str) {
                self.inner.task_event(t, e, p).await
            }
            async fn task_list(&self, f: Option<&str>, l: usize) -> Vec<(String, String, String)> {
                self.inner.task_list(f, l).await
            }
            async fn approval_decide(&self, a: &str, d: &str, b: &str, n: &str) -> Option<String> {
                self.inner.approval_decide(a, d, b, n).await
            }
            async fn approval_get(&self, a: &str) -> Option<serde_json::Value> {
                self.inner.approval_get(a).await
            }
            async fn approval_record_decision(
                &self,
                _a: &str,
                _d: &str,
                _n: &str,
            ) -> Option<String> {
                // Simulate transport failure / responder
                // INTERNAL error.
                None
            }
            async fn tool_audio_transcribe(&self, b: Vec<u8>) -> Option<String> {
                self.inner.tool_audio_transcribe(b).await
            }
        }
        let api = MockBotApi::new();
        let stub = StubOutbound::default();
        stub.approval_get_replies.lock().unwrap().insert(
            "apr-1".into(),
            serde_json::json!({
                "approval_id": "apr-1",
                "authorized_approvers": ["telegram:42"],
            }),
        );
        let out = FailingRecord { inner: stub };
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &msg("/approve apr-1"),
        )
        .await;
        let sent = api.sent_messages();
        assert!(
            sent[0].text.contains("Failed to record decision"),
            "expected failure message in: {}",
            sent[0].text
        );
    }

    #[tokio::test]
    async fn typing_indicator_fires_before_chat_flow() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("reply".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        let actions = api.chat_actions();
        assert!(!actions.is_empty());
        assert_eq!(actions[0].1, "typing");
    }

    #[tokio::test]
    async fn inbound_recorded_in_ring_and_state() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("ok".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        assert_eq!(ring.len(), 1);
        assert_eq!(state.messages_seen(), 1);
        assert!(state.last_message_at().is_some());
    }

    #[tokio::test]
    async fn unauthorised_messages_still_recorded_in_ring() {
        // Ring is the audit feed; non-authorised inbound is
        // exactly what the operator needs to see, so don't
        // gate ring on the permit list.
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&[999]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        assert_eq!(ring.len(), 1);
    }

    /// A `BotApi` impl that returns an error on `send_message` once,
    /// then succeeds. Lets us check that a transient send-failure
    /// doesn't crash the handler.
    struct FailingApi {
        inner: MockBotApi,
    }
    #[async_trait]
    impl BotApi for FailingApi {
        async fn get_me(&self) -> Result<BotIdentity, BotApiError> {
            self.inner.get_me().await
        }
        async fn get_updates(&self, o: i64) -> Result<Vec<IncomingMessage>, BotApiError> {
            self.inner.get_updates(o).await
        }
        async fn send_message(&self, m: &OutgoingMessage) -> Result<(), BotApiError> {
            self.inner.send_message(m).await
        }
        async fn answer_callback_query(
            &self,
            id: &str,
            text: Option<&str>,
        ) -> Result<(), BotApiError> {
            self.inner.answer_callback_query(id, text).await
        }
        async fn edit_message_text(
            &self,
            chat_id: i64,
            message_id: i64,
            text: &str,
            parse_mode: Option<ParseMode>,
        ) -> Result<(), BotApiError> {
            self.inner
                .edit_message_text(chat_id, message_id, text, parse_mode)
                .await
        }
        async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), BotApiError> {
            self.inner.send_chat_action(chat_id, action).await
        }
        async fn get_file_bytes(&self, file_id: &str) -> Result<Vec<u8>, BotApiError> {
            self.inner.get_file_bytes(file_id).await
        }
    }

    #[tokio::test]
    async fn send_failure_does_not_panic_handler() {
        let mock = MockBotApi::new();
        mock.fail_next_send(BotApiError::Transient("blip".into()));
        let api = FailingApi { inner: mock };
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("ok".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        // Must not panic even though sendMessage returns Err.
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        // Ring still recorded the inbound.
        assert_eq!(ring.len(), 1);
    }

    fn callback_msg(cb_id: &str, text: &str) -> IncomingMessage {
        IncomingMessage {
            update_id: 1,
            chat_id: 100,
            user_id: 42,
            message_id: 7,
            username: "alice".into(),
            text: text.into(),
            voice_file_id: None,
            callback_query_id: Some(cb_id.into()),
        }
    }

    /// FIX 3: after processing a callback_query the controller
    /// MUST call `answerCallbackQuery` so the operator's
    /// in-Telegram button spinner clears. Without this call
    /// Telegram leaves the UI loading for ~15s before timing
    /// out — bad UX. We assert (a) the ack lands at all, and
    /// (b) the documented "Decision recorded" copy is the
    /// ack body.
    #[tokio::test]
    async fn callback_query_triggers_answer_callback_after_processing() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("ack body".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&[42]);
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &callback_msg("cb-xyz", "hi"),
        )
        .await;
        let acks = api.callback_acks();
        assert_eq!(acks.len(), 1, "exactly one ack should fire per callback");
        assert_eq!(acks[0].0, "cb-xyz");
        assert_eq!(
            acks[0].1.as_deref(),
            Some("Decision recorded"),
            "ack body must match the documented copy"
        );
    }

    /// FIX 3: regular text messages MUST NOT trigger an
    /// answerCallbackQuery — `callback_query_id` is None and
    /// `answer_callback_query` requires a non-empty id.
    #[tokio::test]
    async fn text_message_does_not_call_answer_callback() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("ack body".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&[42]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        assert!(
            api.callback_acks().is_empty(),
            "no callback ack should fire for a plain text message"
        );
    }

    /// FIX 3: the ack fires even when the inner handler exits
    /// early via the authorisation deny path. A blocked user
    /// pressing an inline button still needs the spinner to
    /// clear — Telegram doesn't care WHY we acked.
    #[tokio::test]
    async fn callback_ack_fires_even_when_user_is_denied() {
        let api = MockBotApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        // Allow-list excludes user 42 so handle_one_update's
        // inner body returns at the `user_is_allowed` check.
        let cfg = cfg_with_allow_list(&[999]);
        handle_one_update(
            &api,
            Some(&out),
            &state,
            &ring,
            &cfg,
            &callback_msg("cb-denied", "hi"),
        )
        .await;
        assert_eq!(api.callback_acks().len(), 1, "ack fires after deny too");
        assert_eq!(api.callback_acks()[0].0, "cb-denied");
    }
}
