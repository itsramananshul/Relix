//! Polling loop + per-message handler for the discord controller.
//! Mirrors nodes/telegram/controller.rs but uses Discord's REST
//! `after` cursor instead of Telegram's `offset`.
//!
//! FIX 2 — persistent watermark. The cursor on
//! [`ChannelState`] is in-memory only and resets across
//! restarts. When the operator configures
//! `[discord] state_db_path`, the controller hydrates the
//! per-channel watermark from a
//! [`super::watermark_store::DiscordWatermarkStore`] at startup
//! and persists every advance back to it so a bridge restart
//! does NOT replay the channel history Discord serves.

use std::sync::Arc;
use std::time::Duration;

use relix_core::clock::SystemClock;
use relix_discord::{DiscordApi, IncomingMessage, OutgoingMessage, derive_channel_subject};

use super::client::{DiscordOutbound, DiscordOutboundClientCell};
use super::commands::{
    Command, brain_unreachable_message, help_message, memory_body, status_body,
    unauthorised_message,
};
use super::config::DiscordNodeConfig;
use super::ring::{MessageRing, RecordedInbound};
use super::state::ChannelState;
use super::watermark_store::DiscordWatermarkStore;

const HISTORY_TURNS: usize = 10;

/// Run the discord controller forever. Re-fetches the outbound
/// cell on every poll so a late-startup wiring is picked up
/// without a restart.
pub async fn run_discord_controller(
    api: Arc<dyn DiscordApi>,
    out_cell: DiscordOutboundClientCell,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    cfg: Arc<DiscordNodeConfig>,
) {
    // Verify the token once. Failure keeps the controller running
    // so the bridge's status endpoint can still report
    // online=false — we just back off and retry.
    match api.get_me().await {
        Ok(id) => {
            tracing::info!(
                username = %id.username,
                user_id = %id.user_id,
                "Discord bot online: @{}",
                id.username
            );
            state.mark_online(id);
        }
        Err(e) => {
            tracing::error!(error = %e, "Discord bot get_me failed; controller will idle");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }

    // FIX 2: hydrate the persisted watermark, if configured.
    // When unset the controller falls back to its existing
    // in-memory cursor (empty ⇒ bootstrap from current tail).
    let watermark_store =
        cfg.state_db_path
            .as_deref()
            .and_then(|p| match DiscordWatermarkStore::open(p) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        path = %p.display(),
                        "discord: failed to open watermark store; persistence disabled"
                    );
                    None
                }
            });
    if let Some(s) = watermark_store.as_ref()
        && let Some(stored) = s.get(&cfg.channel_id)
    {
        tracing::info!(
            channel_id = %cfg.channel_id,
            last_message_id = %stored,
            "discord: hydrated watermark from store; resuming past historical messages"
        );
        state.set_cursor(&stored);
    }

    let poll_interval = Duration::from_secs(cfg.poll_interval_secs.max(1));
    let clock = SystemClock;
    loop {
        // Bootstrapping: an empty cursor means "start from the most
        // recent message". The very first poll returns up to 50
        // messages so the controller catches up to the channel
        // tail; subsequent polls only see new arrivals.
        let cursor = state.cursor();
        let updates = match api.get_messages(&cfg.channel_id, &cursor).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "discord: get_messages failed; backing off");
                tokio::time::sleep(poll_interval).await;
                continue;
            }
        };
        if updates.is_empty() {
            tokio::time::sleep(poll_interval).await;
            continue;
        }
        let bot_id = state.identity().user_id;
        for msg in &updates {
            // Advance the cursor monotonically, even when the
            // handler short-circuits (e.g. bot's own message),
            // so the next poll doesn't replay the same content.
            advance_cursor(&state, &msg.message_id);
            // FIX 2: persist the advanced watermark every time
            // the in-memory cursor moves. The store's
            // `INSERT … ON CONFLICT DO UPDATE` is idempotent
            // so a no-op advance is cheap.
            if let Some(s) = watermark_store.as_ref() {
                s.record(&cfg.channel_id, &state.cursor(), &clock);
            }
            if !bot_id.is_empty() && msg.user_id == bot_id {
                continue;
            }
            if msg.is_bot {
                // Other bots — log but don't process.
                continue;
            }
            let out = out_cell.get().cloned();
            let out_ref: Option<&dyn DiscordOutbound> =
                out.as_ref().map(|a| a.as_ref() as &dyn DiscordOutbound);
            handle_one_update(&*api, out_ref, &state, &ring, &cfg, msg).await;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Run the controller with a caller-provided `DiscordApi`. Wraps
/// the impl in `Arc<dyn>` so the runtime can pick an arbitrary
/// impl without pulling `LiveDiscordApi` into its public surface.
pub async fn run_discord_controller_with_api<D: DiscordApi + 'static>(
    api: D,
    out_cell: DiscordOutboundClientCell,
    state: Arc<ChannelState>,
    ring: Arc<MessageRing>,
    cfg: Arc<DiscordNodeConfig>,
) {
    let api: Arc<dyn DiscordApi> = Arc::new(api);
    run_discord_controller(api, out_cell, state, ring, cfg).await
}

/// Process one inbound message: record → authorise → route on
/// the parsed command → dispatch.
pub async fn handle_one_update(
    api: &dyn DiscordApi,
    out: Option<&dyn DiscordOutbound>,
    state: &ChannelState,
    ring: &MessageRing,
    cfg: &DiscordNodeConfig,
    msg: &IncomingMessage,
) {
    let ts = unix_now();
    state.record_inbound(ts);
    ring.record(RecordedInbound {
        ts,
        user_id: msg.user_id.clone(),
        username: msg.username.clone(),
        channel_id: msg.channel_id.clone(),
        content: msg.content.clone(),
    });

    if !cfg.user_is_allowed(&msg.user_id) {
        let _ = send_text(
            api,
            &msg.channel_id,
            &msg.message_id,
            unauthorised_message(),
        )
        .await;
        return;
    }

    match Command::parse(&msg.content) {
        Command::Help => {
            let _ = send_text(api, &msg.channel_id, &msg.message_id, &help_message()).await;
        }
        Command::Status => {
            let body = status_body(&render_status_summary(state, cfg));
            let _ = send_text(api, &msg.channel_id, &msg.message_id, &body).await;
        }
        Command::Memory => {
            let subject = derive_channel_subject(&msg.channel_id, &msg.user_id);
            let (agent, user) = match out {
                Some(o) => o.memory_agent_read(&subject.subject_id.to_string()).await,
                None => (String::new(), String::new()),
            };
            let _ = send_text(
                api,
                &msg.channel_id,
                &msg.message_id,
                &memory_body(&agent, &user),
            )
            .await;
        }
        Command::Forget => {
            let subject = derive_channel_subject(&msg.channel_id, &msg.user_id);
            if let Some(o) = out {
                o.memory_agent_clear(&subject.subject_id.to_string()).await;
            }
            let _ = send_text(api, &msg.channel_id, &msg.message_id, "Memory cleared.").await;
        }
        Command::Chat(text) => {
            run_chat_flow(api, out, msg, &text).await;
        }
    }
}

fn render_status_summary(state: &ChannelState, cfg: &DiscordNodeConfig) -> String {
    let identity = state.identity();
    format!(
        "bot=@{}\nuser_id={}\nonline={}\nmessages_seen={}\nchannel_id={}\nallow_everyone={}",
        identity.username,
        identity.user_id,
        state.online(),
        state.messages_seen(),
        cfg.channel_id,
        cfg.allow_everyone()
    )
}

/// Chat-flow path. Same shape as nodes/telegram/controller.rs:
/// typing → memory_recent → ai.chat → memory_write × 2 →
/// send_message, plus best-effort task lifecycle records.
async fn run_chat_flow(
    api: &dyn DiscordApi,
    out: Option<&dyn DiscordOutbound>,
    msg: &IncomingMessage,
    text: &str,
) {
    let subject = derive_channel_subject(&msg.channel_id, &msg.user_id);
    let session_id = subject.subject_id.to_string();
    let _ = api.send_typing(&msg.channel_id).await;

    let Some(out) = out else {
        let _ = send_text(
            api,
            &msg.channel_id,
            &msg.message_id,
            brain_unreachable_message(),
        )
        .await;
        return;
    };

    let history = out.memory_recent(&session_id, HISTORY_TURNS).await;
    let history_text = render_history(&history);

    let task_id = out
        .task_create(
            &message_title(text),
            "flows/chat_template.sol",
            "",
            &session_id,
        )
        .await;
    if let Some(t) = task_id.as_deref() {
        out.task_event(
            t,
            "task.discord.inbound",
            &format!("channel_id={}", msg.channel_id),
        )
        .await;
    }

    // RELIX-7.7 GAP 2: consult the coordinator's routing
    // rules. Subject for Discord is the channel id (the
    // closest thing Discord has to a per-thread topic).
    let preview: String = text.chars().take(200).collect();
    let routed = out
        .routing_resolve("discord", &msg.username, &msg.channel_id, &preview)
        .await;
    let reply = match routed {
        Some((peer, capability)) => {
            tracing::info!(
                channel_id = %msg.channel_id,
                from = %msg.username,
                target_peer = %peer,
                capability = %capability,
                "discord: routed via ChannelRouter"
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
                &msg.channel_id,
                &msg.message_id,
                brain_unreachable_message(),
            )
            .await;
            return;
        }
    };

    out.memory_write(&session_id, "user", text).await;
    out.memory_write(&session_id, "assistant", &reply).await;

    let _ = send_text(api, &msg.channel_id, &msg.message_id, &reply).await;

    if let Some(t) = task_id.as_deref() {
        out.task_update_status(t, "completed", "ok").await;
    }
}

fn message_title(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return "discord-message".to_string();
    }
    let truncated: String = first_line.chars().take(80).collect();
    format!("discord: {truncated}")
}

fn render_history(history: &[(String, String)]) -> String {
    let mut out = String::new();
    for (role, text) in history {
        out.push_str(&format!("[{role}] {text}\n"));
    }
    out
}

async fn send_text(api: &dyn DiscordApi, channel_id: &str, reply_to: &str, text: &str) -> bool {
    // Discord caps a single message at ~2000 chars. The
    // formatter splits the assistant reply at paragraph >
    // sentence > line > space boundaries so a long answer
    // arrives as multiple consecutive messages — never
    // mid-word, never mid-code-block. See
    // `crate::nodes::channels::format_for_discord`.
    let chunks = crate::nodes::channels::format_for_discord(text);
    let mut ok = true;
    for (i, chunk) in chunks.iter().enumerate() {
        // Only the first chunk threads under the original
        // user message; subsequent chunks are standalone so
        // Discord doesn't pile up reply-references.
        let reply = if i == 0 { reply_to } else { "" };
        let msg = OutgoingMessage {
            channel_id: channel_id.to_string(),
            reply_to_message_id: reply.to_string(),
            content: chunk.clone(),
            components: Vec::new(),
        };
        if let Err(e) = api.send_message(&msg).await {
            tracing::warn!(error = %e, channel_id = channel_id, chunk_idx = i,
                "discord: send_message failed");
            ok = false;
            break;
        }
    }
    ok
}

/// Move the persistent cursor forward if the new id is
/// lexicographically greater (snowflakes are time-ordered, so
/// string compare on equal-length numeric strings is monotonic).
fn advance_cursor(state: &ChannelState, new_id: &str) {
    let cur = state.cursor();
    if cur.is_empty() {
        state.set_cursor(new_id);
        return;
    }
    if snowflake_lt(&cur, new_id) {
        state.set_cursor(new_id);
    }
}

/// `a < b` where a/b are numeric snowflake strings. Length-first
/// then lexicographic — handles the eventual digit-count rollover
/// gracefully (Discord widens the snowflake range over the
/// decades).
fn snowflake_lt(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return a.len() < b.len();
    }
    a < b
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
    use relix_discord::{BotIdentity, DiscordApiError, mock::MockDiscordApi};
    use std::sync::Mutex;

    fn cfg_default() -> DiscordNodeConfig {
        toml::from_str(
            r#"
            token_env = "X"
            channel_id = "12345678901234567"
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

    fn cfg_with_allow_list(users: &[&str]) -> DiscordNodeConfig {
        let mut cfg = cfg_default();
        cfg.allowed_users = users.iter().map(|s| s.to_string()).collect();
        cfg
    }

    fn msg(content: &str) -> IncomingMessage {
        IncomingMessage {
            message_id: "9000".into(),
            channel_id: "100".into(),
            user_id: "42".into(),
            username: "alice".into(),
            is_bot: false,
            content: content.into(),
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
    }

    #[async_trait]
    impl DiscordOutbound for StubOutbound {
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
    }

    fn state_online() -> ChannelState {
        let s = ChannelState::default();
        s.mark_online(BotIdentity {
            user_id: "999".into(),
            username: "relixbot".into(),
        });
        s
    }

    #[tokio::test]
    async fn help_command_replies_without_hitting_ai() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/help")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].content.contains("/help"));
        assert!(out.ai_chat_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unauthorised_user_gets_static_message_no_dispatch() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&["999"]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hello")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, unauthorised_message());
        assert!(out.ai_chat_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn allowed_user_gets_chat_routed_to_ai() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("hello from ai".into());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&["42"]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi there")).await;
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "hello from ai");
        assert_eq!(out.memory_writes.lock().unwrap().len(), 2);
        // Typing indicator fired before chat.
        let typing = api.typing_pings();
        assert_eq!(typing, vec!["100"]);
        // Task created + completed.
        assert_eq!(out.task_creates.lock().unwrap().len(), 1);
        let updates = out.task_updates.lock().unwrap();
        assert!(updates.iter().any(|(_, s, _)| s == "completed"));
    }

    #[tokio::test]
    async fn chat_with_no_outbound_falls_back_to_brain_unreachable() {
        let api = MockDiscordApi::new();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, None, &state, &ring, &cfg, &msg("hi")).await;
        let sent = api.sent_messages();
        assert_eq!(sent[0].content, brain_unreachable_message());
    }

    #[tokio::test]
    async fn ai_chat_empty_reply_falls_back() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some(String::new());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        assert_eq!(api.sent_messages()[0].content, brain_unreachable_message());
        let updates = out.task_updates.lock().unwrap();
        assert!(updates.iter().any(|(_, s, _)| s == "failed"));
    }

    #[tokio::test]
    async fn memory_command_renders_blobs() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        *out.agent_read_reply.lock().unwrap() = ("agent-x".into(), "user-y".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/memory")).await;
        let sent = api.sent_messages();
        assert!(sent[0].content.contains("agent-x"));
        assert!(sent[0].content.contains("user-y"));
    }

    #[tokio::test]
    async fn forget_command_clears_memory_and_acks() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("/forget")).await;
        assert_eq!(*out.agent_clear_calls.lock().unwrap(), 1);
        assert!(api.sent_messages()[0].content.contains("Memory cleared"));
    }

    #[tokio::test]
    async fn inbound_recorded_in_ring_and_state() {
        let api = MockDiscordApi::new();
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
    async fn unauthorised_inbound_still_recorded_in_ring() {
        let api = MockDiscordApi::new();
        let out = StubOutbound::default();
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_with_allow_list(&["999"]);
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        assert_eq!(ring.len(), 1);
    }

    #[tokio::test]
    async fn subject_id_stable_for_same_user() {
        let s1 = derive_channel_subject("100", "42");
        let s2 = derive_channel_subject("100", "42");
        assert_eq!(s1.subject_id, s2.subject_id);
    }

    #[tokio::test]
    async fn subject_id_differs_per_user() {
        let s1 = derive_channel_subject("100", "42");
        let s2 = derive_channel_subject("100", "43");
        assert_ne!(s1.subject_id, s2.subject_id);
    }

    /// End-to-end: pre-load the mock with one inbound, run a
    /// single iteration of the polling loop's body manually, and
    /// confirm:
    /// - the typing indicator fired
    /// - ai.chat was called with the right session_id
    /// - both memory turns persisted
    /// - the agent's reply was sent back to the channel
    /// - the cursor advanced past the processed message
    /// - the bot's own message was filtered out (separate
    ///   sub-test below)
    #[tokio::test]
    async fn end_to_end_message_in_ai_reply_out() {
        let api = MockDiscordApi::new();
        let m = IncomingMessage {
            message_id: "9001".into(),
            channel_id: "100".into(),
            user_id: "42".into(),
            username: "alice".into(),
            is_bot: false,
            content: "hi there".into(),
        };
        api.push_message(m);
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("hello from ai".into());
        *out.task_create_id.lock().unwrap() = Some("task-1".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();

        // Drive one polling iteration manually.
        let updates = api
            .get_messages(&cfg.channel_id, &state.cursor())
            .await
            .unwrap();
        assert_eq!(updates.len(), 1);
        for inc in &updates {
            advance_cursor(&state, &inc.message_id);
            if inc.is_bot {
                continue;
            }
            handle_one_update(&api, Some(&out), &state, &ring, &cfg, inc).await;
        }

        // Cursor advanced.
        assert_eq!(state.cursor(), "9001");
        // Typing → AI reply.
        assert_eq!(api.typing_pings(), vec!["100"]);
        let sent = api.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].content, "hello from ai");
        // Two memory turns: user + assistant.
        assert_eq!(out.memory_writes.lock().unwrap().len(), 2);
        // ai.chat called with the derived session_id.
        let ai_calls = out.ai_chat_calls.lock().unwrap();
        assert_eq!(ai_calls.len(), 1);
        assert_eq!(ai_calls[0].1, "hi there");
        // Task lifecycle recorded.
        assert_eq!(out.task_creates.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bot_self_message_is_ignored_by_id_match() {
        // Build a controller that sees a message whose author_id
        // matches the bot's identity. The polling loop skips it
        // before the handler is invoked; we exercise that path
        // by simulating one loop iteration.
        //
        // PART 3: the live + mock `get_messages` now filter
        // bot-authored messages at the parse layer (primary
        // defence). The user_id-match guard below is
        // secondary defence — it kicks in if Discord ever
        // returns a message whose `author.bot` flag is
        // missing or false but whose user_id matches the
        // bot's identity. We simulate that case by pushing an
        // `is_bot = false` message with the matching user_id.
        let api = MockDiscordApi::new();
        let self_msg = IncomingMessage {
            message_id: "9999".into(),
            channel_id: "100".into(),
            user_id: "999".into(), // matches the bot identity below
            username: "relixbot".into(),
            is_bot: false, // simulate `bot` flag missing on the wire
            content: "reply from myself".into(),
        };
        api.push_message(self_msg);
        let out = StubOutbound::default();
        let state = state_online(); // identity user_id == "999"
        let ring = MessageRing::new(50);
        let cfg = cfg_default();

        let bot_id = state.identity().user_id;
        let updates = api
            .get_messages(&cfg.channel_id, &state.cursor())
            .await
            .unwrap();
        for inc in &updates {
            advance_cursor(&state, &inc.message_id);
            if !bot_id.is_empty() && inc.user_id == bot_id {
                continue;
            }
            handle_one_update(&api, Some(&out), &state, &ring, &cfg, inc).await;
        }
        // No reply sent; no ring entry; the controller treated
        // the message as a self-loop.
        assert!(api.sent_messages().is_empty());
        assert_eq!(ring.len(), 0);
        // Cursor still advanced so we don't replay it.
        assert_eq!(state.cursor(), "9999");
    }

    struct FailingApi {
        inner: MockDiscordApi,
    }
    #[async_trait]
    impl DiscordApi for FailingApi {
        async fn get_me(&self) -> Result<BotIdentity, DiscordApiError> {
            self.inner.get_me().await
        }
        async fn bootstrap_watermark(&self, c: &str) -> Result<Option<String>, DiscordApiError> {
            self.inner.bootstrap_watermark(c).await
        }
        async fn get_messages(
            &self,
            c: &str,
            a: &str,
        ) -> Result<Vec<IncomingMessage>, DiscordApiError> {
            self.inner.get_messages(c, a).await
        }
        async fn send_message(&self, m: &OutgoingMessage) -> Result<(), DiscordApiError> {
            self.inner.send_message(m).await
        }
        async fn send_typing(&self, c: &str) -> Result<(), DiscordApiError> {
            self.inner.send_typing(c).await
        }
        async fn delete_message(&self, c: &str, m: &str) -> Result<(), DiscordApiError> {
            self.inner.delete_message(c, m).await
        }
    }

    #[tokio::test]
    async fn send_failure_does_not_panic_handler() {
        let mock = MockDiscordApi::new();
        mock.fail_next_send(DiscordApiError::Transient("blip".into()));
        let api = FailingApi { inner: mock };
        let out = StubOutbound::default();
        *out.ai_chat_reply.lock().unwrap() = Some("ok".into());
        let state = state_online();
        let ring = MessageRing::new(50);
        let cfg = cfg_default();
        handle_one_update(&api, Some(&out), &state, &ring, &cfg, &msg("hi")).await;
        // Ring still recorded the inbound.
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn snowflake_lt_handles_same_and_different_lengths() {
        assert!(snowflake_lt("100", "200"));
        assert!(!snowflake_lt("200", "100"));
        assert!(snowflake_lt("99", "100")); // shorter < longer
        assert!(!snowflake_lt("100", "99"));
    }
}
