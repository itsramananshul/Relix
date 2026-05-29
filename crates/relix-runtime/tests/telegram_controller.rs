//! End-to-end controller integration tests.
//!
//! These exercise the full
//! `relix_runtime::nodes::telegram::controller::handle_one_update`
//! path through both a stub `BotApi` and a stub
//! `TelegramOutbound`, sharing a unified ordered event log so
//! the tests can assert the exact sequence the controller
//! follows — typing-indicator before send_message, memory
//! writes around the AI dispatch, task lifecycle, etc.
//!
//! Distinct from the unit tests inside the controller module
//! (which assert per-method behaviour with per-surface
//! stubs): this file's stubs share a single `OpLog` so we can
//! reason about cross-surface ordering.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use relix_telegram::{
    BotApi, BotApiError, BotIdentity, IncomingMessage, OutgoingMessage, ParseMode,
    derive_channel_subject,
};

use relix_runtime::nodes::telegram::client::TelegramOutbound;
use relix_runtime::nodes::telegram::controller::handle_one_update;
use relix_runtime::nodes::telegram::{ChannelState, MessageRing, TelegramNodeConfig};

// ── Unified ordered event log ─────────────────────────────

/// One observable operation the controller performed across
/// either surface (BotApi or TelegramOutbound).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    SendChatAction {
        chat_id: i64,
        action: String,
    },
    SendMessage {
        chat_id: i64,
        reply_to: i64,
        text: String,
    },
    MemoryRecent {
        session_id: String,
        n: usize,
    },
    MemoryWrite {
        session_id: String,
        role: String,
        text: String,
    },
    MemoryAgentRead {
        subject_id: String,
    },
    MemoryAgentClear {
        subject_id: String,
    },
    AiChat {
        session_id: String,
        prompt: String,
        history: String,
    },
    TaskCreate {
        title: String,
        owner_subject_id: String,
    },
    TaskEvent {
        task_id: String,
        event_type: String,
    },
    TaskUpdate {
        task_id: String,
        status: String,
    },
}

#[derive(Default)]
struct OpLog {
    ops: Mutex<Vec<Op>>,
}

impl OpLog {
    fn push(&self, op: Op) {
        self.ops.lock().unwrap().push(op);
    }
    fn snapshot(&self) -> Vec<Op> {
        self.ops.lock().unwrap().clone()
    }
    fn position<F: Fn(&Op) -> bool>(&self, f: F) -> Option<usize> {
        self.snapshot().iter().position(f)
    }
    fn count<F: Fn(&Op) -> bool>(&self, f: F) -> usize {
        self.snapshot().iter().filter(|o| f(o)).count()
    }
}

// ── Stub BotApi that logs to OpLog ─────────────────────────

struct StubApi {
    log: Arc<OpLog>,
}

#[async_trait]
impl BotApi for StubApi {
    async fn get_me(&self) -> Result<BotIdentity, BotApiError> {
        Ok(BotIdentity::default())
    }
    async fn get_updates(&self, _offset: i64) -> Result<Vec<IncomingMessage>, BotApiError> {
        Ok(Vec::new())
    }
    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), BotApiError> {
        self.log.push(Op::SendMessage {
            chat_id: out.chat_id,
            reply_to: out.reply_to_message_id,
            text: out.text.clone(),
        });
        Ok(())
    }
    async fn answer_callback_query(
        &self,
        _id: &str,
        _text: Option<&str>,
    ) -> Result<(), BotApiError> {
        Ok(())
    }
    async fn edit_message_text(
        &self,
        _chat_id: i64,
        _message_id: i64,
        _text: &str,
        _parse_mode: Option<ParseMode>,
    ) -> Result<(), BotApiError> {
        Ok(())
    }
    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), BotApiError> {
        self.log.push(Op::SendChatAction {
            chat_id,
            action: action.into(),
        });
        Ok(())
    }
    async fn get_file_bytes(&self, _file_id: &str) -> Result<Vec<u8>, BotApiError> {
        Err(BotApiError::ClientError("stub: no file storage".into()))
    }
}

// ── Stub TelegramOutbound that logs to OpLog ───────────────

#[derive(Clone, Default)]
struct StubOutboundCfg {
    /// `ai.chat` reply text. `None` simulates an unreachable
    /// brain.
    ai_chat_reply: Option<String>,
    /// What `task.create` returns. `None` simulates the
    /// coordinator being unreachable.
    task_create_id: Option<String>,
    /// Pre-recorded history `memory.recent_for_session`
    /// returns.
    history: Vec<(String, String)>,
    /// What `memory.agent_read` returns.
    agent_read_reply: (String, String),
}

struct StubOutbound {
    log: Arc<OpLog>,
    cfg: StubOutboundCfg,
}

#[async_trait]
impl TelegramOutbound for StubOutbound {
    async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)> {
        self.log.push(Op::MemoryRecent {
            session_id: session_id.into(),
            n,
        });
        self.cfg.history.clone()
    }
    async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
        self.log.push(Op::MemoryWrite {
            session_id: session_id.into(),
            role: role.into(),
            text: text.into(),
        });
    }
    async fn memory_agent_read(&self, subject_id: &str) -> (String, String) {
        self.log.push(Op::MemoryAgentRead {
            subject_id: subject_id.into(),
        });
        self.cfg.agent_read_reply.clone()
    }
    async fn memory_agent_clear(&self, subject_id: &str) {
        self.log.push(Op::MemoryAgentClear {
            subject_id: subject_id.into(),
        });
    }
    async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        self.log.push(Op::AiChat {
            session_id: session_id.into(),
            prompt: prompt.into(),
            history: history.into(),
        });
        self.cfg.ai_chat_reply.clone()
    }
    async fn task_create(
        &self,
        title: &str,
        _flow_template: &str,
        _params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String> {
        self.log.push(Op::TaskCreate {
            title: title.into(),
            owner_subject_id: owner_subject_id.into(),
        });
        self.cfg.task_create_id.clone()
    }
    async fn task_update_status(&self, task_id: &str, status: &str, _result: &str) {
        self.log.push(Op::TaskUpdate {
            task_id: task_id.into(),
            status: status.into(),
        });
    }
    async fn task_event(&self, task_id: &str, event_type: &str, _payload: &str) {
        self.log.push(Op::TaskEvent {
            task_id: task_id.into(),
            event_type: event_type.into(),
        });
    }
    async fn task_list(
        &self,
        _status_filter: Option<&str>,
        _limit: usize,
    ) -> Vec<(String, String, String)> {
        Vec::new()
    }
    async fn approval_decide(
        &self,
        _approval_id: &str,
        decision: &str,
        _decided_by: &str,
        _note: &str,
    ) -> Option<String> {
        if decision == "approved" {
            Some("ok|cafebabecafebabecafebabecafebabe\n".to_string())
        } else {
            Some("ok\n".to_string())
        }
    }
    async fn tool_audio_transcribe(&self, _audio_bytes: Vec<u8>) -> Option<String> {
        None
    }
}

// ── Test helpers ──────────────────────────────────────────

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

fn msg_from(chat_id: i64, user_id: i64, text: &str) -> IncomingMessage {
    IncomingMessage {
        update_id: 1,
        chat_id,
        user_id,
        message_id: 7,
        username: "alice".into(),
        text: text.into(),
        voice_file_id: None,
        callback_query_id: None,
    }
}

fn online_state() -> ChannelState {
    let s = ChannelState::default();
    s.mark_online(BotIdentity {
        user_id: 1,
        username: "relixbot".into(),
        first_name: "Relix".into(),
    });
    s
}

fn build_harness(out_cfg: StubOutboundCfg) -> (Arc<OpLog>, StubApi, StubOutbound) {
    let log = Arc::new(OpLog::default());
    let api = StubApi { log: log.clone() };
    let out = StubOutbound {
        log: log.clone(),
        cfg: out_cfg,
    };
    (log, api, out)
}

// ── Tests ─────────────────────────────────────────────────

/// THE end-to-end ordering test. Covers every step the user
/// listed in their prompt:
/// 1. user sends "hello"
/// 2. controller derives session_id
/// 3. memory.recent_for_session
/// 4. typing indicator
/// 5. ai.chat
/// 6. memory.write_turn (user)
/// 7. send_message with the AI reply
/// 8. memory.write_turn (assistant)
/// 9. task.create
#[tokio::test]
async fn e2e_chat_flow_runs_exact_sequence() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("hi back!".into()),
        task_create_id: Some("task-1".into()),
        history: vec![("user".into(), "older".into())],
        agent_read_reply: Default::default(),
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_default();
    let m = msg_from(100, 42, "hello");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snapshot = log.snapshot();
    // Quick sanity: every requested operation showed up.
    assert!(
        snapshot
            .iter()
            .any(|o| matches!(o, Op::SendChatAction { action, .. } if action == "typing")),
        "expected typing indicator; got: {snapshot:?}"
    );
    assert!(
        snapshot
            .iter()
            .any(|o| matches!(o, Op::SendMessage { text, .. } if text == &relix_runtime::nodes::channels::format_for_telegram_markdown_v2("hi back!"))),
        "expected send_message with AI reply; got: {snapshot:?}"
    );

    // chat_action precedes send_message.
    let i_typing = log
        .position(|o| matches!(o, Op::SendChatAction { action, .. } if action == "typing"))
        .expect("typing op");
    let i_send = log
        .position(|o| matches!(o, Op::SendMessage { text, .. } if text == &relix_runtime::nodes::channels::format_for_telegram_markdown_v2("hi back!")))
        .expect("send_message op");
    assert!(
        i_typing < i_send,
        "send_chat_action must run before send_message ({i_typing} >= {i_send}); snapshot: {snapshot:?}"
    );

    // Reply went to the originating chat_id, threaded to the
    // inbound message_id.
    let send_op = &snapshot[i_send];
    if let Op::SendMessage {
        chat_id, reply_to, ..
    } = send_op
    {
        assert_eq!(*chat_id, 100);
        assert_eq!(*reply_to, 7);
    } else {
        unreachable!()
    }

    // Memory writes — exactly two, with roles user then
    // assistant.
    let memory_writes: Vec<_> = snapshot
        .iter()
        .filter_map(|o| match o {
            Op::MemoryWrite { role, text, .. } => Some((role.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(memory_writes.len(), 2, "got: {memory_writes:?}");
    assert_eq!(memory_writes[0].0, "user");
    assert_eq!(memory_writes[0].1, "hello");
    assert_eq!(memory_writes[1].0, "assistant");
    assert_eq!(memory_writes[1].1, "hi back!");

    // task.create called exactly once.
    assert_eq!(log.count(|o| matches!(o, Op::TaskCreate { .. })), 1);

    // ai.chat got the rendered history block from
    // memory_recent. The controller's renderer wraps each
    // turn as `[role] text`.
    let history_text = snapshot
        .iter()
        .find_map(|o| match o {
            Op::AiChat { history, .. } => Some(history.clone()),
            _ => None,
        })
        .expect("ai.chat called");
    assert!(
        history_text.contains("[user] older"),
        "got: {history_text:?}"
    );

    // Coordinator task transitioned to `completed` after a
    // successful reply path.
    assert!(
        snapshot
            .iter()
            .any(|o| matches!(o, Op::TaskUpdate { status, .. } if status == "completed")),
        "expected task.update completed; got: {snapshot:?}"
    );
}

/// The full chat flow also wires the session_id consistently
/// across `memory.recent_for_session`, `memory.write_turn`,
/// and `ai.chat`. Lock the invariant so a refactor that
/// changes how session_id is derived (or stops threading it
/// through every dispatch) fails loudly.
#[tokio::test]
async fn e2e_session_id_is_consistent_across_dispatches() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("ok".into()),
        task_create_id: Some("task-1".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_default();
    let m = msg_from(100, 42, "hi");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();
    let expected_session = derive_channel_subject(100, 42).subject_id.to_string();
    for op in &snap {
        match op {
            Op::MemoryRecent { session_id, .. }
            | Op::MemoryWrite { session_id, .. }
            | Op::AiChat { session_id, .. } => {
                assert_eq!(
                    session_id, &expected_session,
                    "session_id drift: {op:?} should have used {expected_session}"
                );
            }
            _ => {}
        }
    }
}

// ── subject_id derivation ─────────────────────────────────

#[test]
fn subject_id_is_stable_for_same_user_id_and_chat_id() {
    let a = derive_channel_subject(100, 42);
    let b = derive_channel_subject(100, 42);
    assert_eq!(a.subject_id, b.subject_id);
}

#[test]
fn subject_id_differs_for_different_user_ids() {
    let a = derive_channel_subject(100, 42);
    let b = derive_channel_subject(100, 43);
    assert_ne!(a.subject_id, b.subject_id);
}

#[test]
fn subject_id_differs_for_different_chat_ids() {
    let a = derive_channel_subject(100, 42);
    let b = derive_channel_subject(200, 42);
    assert_ne!(a.subject_id, b.subject_id);
}

// ── allowed_users gate ────────────────────────────────────

#[tokio::test]
async fn e2e_allowed_users_blocks_user_not_on_the_list() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("would-be-reply".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    // Permit list does NOT include user 42 (msg_from default).
    let cfg = cfg_with_allow_list(&[999]);
    let m = msg_from(100, 42, "hello");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();
    // The user gets the canonical unauthorized reply.
    let sent_text = snap
        .iter()
        .find_map(|o| match o {
            Op::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("send_message must have fired with the unauthorized message");
    // The Telegram controller now formats outbound messages as
    // MarkdownV2 (so code fences, multi-paragraph replies, etc.
    // render correctly). Reserved characters like `.` get
    // backslash-escaped — assert against the formatted form.
    assert_eq!(
        sent_text,
        relix_runtime::nodes::channels::format_for_telegram_markdown_v2(
            "You are not authorized to use this bot."
        )
    );

    // And NO downstream chat-flow ops ran.
    assert_eq!(log.count(|o| matches!(o, Op::AiChat { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::MemoryWrite { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::TaskCreate { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::SendChatAction { .. })), 0);
}

#[tokio::test]
async fn e2e_allowed_users_admits_user_on_the_list() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("authorised-reply".into()),
        task_create_id: Some("task-1".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_with_allow_list(&[42]);
    let m = msg_from(100, 42, "hello");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();
    // AI was invoked and the reply was sent.
    assert!(snap.iter().any(|o| matches!(o, Op::AiChat { .. })));
    let texts: Vec<_> = snap
        .iter()
        .filter_map(|o| match o {
            Op::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        vec![relix_runtime::nodes::channels::format_for_telegram_markdown_v2("authorised-reply")]
    );
}

// ── slash commands ────────────────────────────────────────

#[tokio::test]
async fn e2e_start_command_does_not_invoke_chat_flow() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("would-not-fire".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_default();
    let m = msg_from(100, 42, "/start");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();
    // /start sends a welcome message — and never reaches the
    // AI peer.
    let welcome_text = snap
        .iter()
        .find_map(|o| match o {
            Op::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("welcome message");
    assert!(
        welcome_text.contains("Welcome to Relix"),
        "got: {welcome_text:?}"
    );
    assert_eq!(log.count(|o| matches!(o, Op::AiChat { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::MemoryWrite { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::TaskCreate { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::SendChatAction { .. })), 0);
}

#[tokio::test]
async fn e2e_help_command_does_not_invoke_chat_flow() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("would-not-fire".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_default();
    let m = msg_from(100, 42, "/help");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();
    let help_text = snap
        .iter()
        .find_map(|o| match o {
            Op::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("help message");
    // The help body lists the slash commands.
    assert!(help_text.contains("/approve"), "got: {help_text:?}");
    assert!(help_text.contains("/reject"), "got: {help_text:?}");
    assert_eq!(log.count(|o| matches!(o, Op::AiChat { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::MemoryWrite { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::TaskCreate { .. })), 0);
}

#[tokio::test]
async fn e2e_forget_command_clears_memory_without_chat_flow() {
    let (log, api, out) = build_harness(StubOutboundCfg {
        ai_chat_reply: Some("would-not-fire".into()),
        ..Default::default()
    });
    let state = online_state();
    let ring = MessageRing::new(50);
    let cfg = cfg_default();
    let m = msg_from(100, 42, "/forget");

    handle_one_update(&api, Some(&out), &state, &ring, &cfg, &m).await;

    let snap = log.snapshot();

    // memory.agent_clear was invoked — exactly once, on the
    // derived subject_id.
    let clear_calls: Vec<_> = snap
        .iter()
        .filter_map(|o| match o {
            Op::MemoryAgentClear { subject_id } => Some(subject_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(clear_calls.len(), 1, "got: {clear_calls:?}");
    let expected_subject = derive_channel_subject(100, 42).subject_id.to_string();
    assert_eq!(clear_calls[0], expected_subject);

    // No chat flow ran.
    assert_eq!(log.count(|o| matches!(o, Op::AiChat { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::MemoryWrite { .. })), 0);
    assert_eq!(log.count(|o| matches!(o, Op::TaskCreate { .. })), 0);

    // The user got the "memory cleared" ack.
    let ack_text = snap
        .iter()
        .find_map(|o| match o {
            Op::SendMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("ack message");
    assert!(ack_text.contains("Memory cleared"), "got: {ack_text:?}");
}
