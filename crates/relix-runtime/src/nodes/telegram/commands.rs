//! Slash-command parsing + rendering for the telegram
//! controller.
//!
//! Slash commands branch the controller's normal flow:
//! they're handled locally and never spawn a chat turn or
//! hit the AI peer. The set is deliberately small —
//! anything more elaborate belongs in the SOL flow itself.
//!
//! Telegram's `BotCommand` menu is *not* wired here (that's
//! a one-shot `setMyCommands` call the operator runs out of
//! band); this module just recognises the on-wire `/cmd ...`
//! tokens.

/// Parsed slash-command intent. `Chat` is the
/// catch-all for ordinary messages.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `/start` — greet a new user.
    Start,
    /// `/help` — list available commands.
    Help,
    /// `/status` — mesh health summary.
    Status,
    /// `/memory` — show the caller's agent + user memory.
    Memory,
    /// `/forget` — clear the caller's agent + user memory.
    Forget,
    /// `/approve <task_id>` — operator-only approval.
    Approve(String),
    /// `/reject <task_id>` — operator-only rejection.
    Reject(String),
    /// Anything that isn't a recognised command. Carries
    /// the original (trimmed) text so the controller can
    /// pipe it straight into `ai.chat`.
    Chat(String),
}

impl Command {
    /// Parse the inbound text into a [`Command`]. Leading
    /// whitespace is trimmed; the `/cmd@bot_username`
    /// suffix that Telegram appends in group chats is
    /// stripped.
    pub fn parse(text: &str) -> Command {
        let trimmed = text.trim();
        if !trimmed.starts_with('/') {
            return Command::Chat(trimmed.to_string());
        }
        // Split into the command head and the rest.
        let (head_raw, rest) = match trimmed.split_once(' ') {
            Some((h, r)) => (h, r.trim()),
            None => (trimmed, ""),
        };
        // Drop the leading `/` and any `@bot_username` suffix.
        let head = head_raw.trim_start_matches('/');
        let head = match head.split_once('@') {
            Some((cmd, _bot)) => cmd,
            None => head,
        };
        match head.to_ascii_lowercase().as_str() {
            "start" => Command::Start,
            "help" => Command::Help,
            "status" => Command::Status,
            "memory" => Command::Memory,
            "forget" => Command::Forget,
            "approve" => Command::Approve(rest.to_string()),
            "reject" => Command::Reject(rest.to_string()),
            _ => Command::Chat(trimmed.to_string()),
        }
    }
}

/// Static welcome message returned by `/start`.
pub fn welcome_message() -> String {
    "👋 Welcome to Relix.\n\n\
     I'm your durable agent — every message you send becomes a Task in \
     the coordinator's ledger, runs through the canonical chat flow, and \
     leaves an audit trail you can inspect from the dashboard.\n\n\
     Send a message to start. Type /help for available commands."
        .to_string()
}

/// Static command listing returned by `/help`.
pub fn help_message() -> String {
    "Commands:\n\
     /start — show the welcome message.\n\
     /help — this message.\n\
     /status — mesh health summary.\n\
     /memory — show your persistent agent + user memory.\n\
     /forget — wipe your persistent memory.\n\
     /approve <task_id> — approve a pending approval (operator only).\n\
     /reject <task_id> — reject a pending approval (operator only).\n\
     Anything else is treated as a chat message and routed to the \
     canonical chat flow."
        .to_string()
}

/// User-friendly fallback when the AI peer is unreachable.
/// Same wording the spec requires.
pub fn brain_unreachable_message() -> &'static str {
    "I'm having trouble reaching my brain right now. Please try again in a moment."
}

/// User-facing "not authorised" message returned to callers
/// outside the permit list.
pub fn unauthorised_message() -> &'static str {
    "You are not authorized to use this bot."
}

/// Reply for a voice message when no audio peer is configured
/// (so `tool.audio.transcribe` can't be dispatched). Same
/// honest-scope posture as `brain_unreachable_message`: the
/// operator gets a clear, actionable line instead of silence.
pub fn voice_transcription_unavailable_message() -> &'static str {
    "Voice transcription isn't configured on this bot. Send your message as text, or ask the \
     operator to wire `[telegram.audio_peer]` and an audio tool node."
}

/// Reply for a voice message when transcription failed (engine
/// down, file fetch failed, empty transcript). Distinct from
/// the "unavailable" message above: this one tells the user
/// the path exists but didn't succeed for this message.
pub fn voice_transcription_failed_message() -> &'static str {
    "I couldn't transcribe that voice message. Please try again or send your message as text."
}

/// Render an approval notification body for the operator
/// chat. Matches the spec wording. `task_id` is rendered
/// only when non-empty (the agent-employee approval flow
/// stamps the calling task_id on the approval row; older
/// flows that have no associated task pass `""`).
pub fn approval_notification(
    task_id: &str,
    subject_short: &str,
    method: &str,
    reason: &str,
) -> String {
    let reason_clean = if reason.trim().is_empty() {
        "(no reason given)"
    } else {
        reason.trim()
    };
    let task_line = if task_id.trim().is_empty() {
        String::new()
    } else {
        format!("Task: {task_id}\n")
    };
    format!(
        "⏳ Approval required\n{task_line}Agent: {subject_short}\nAction: {method}\nReason: {reason_clean}\nReply /approve {task_id} or /reject {task_id}"
    )
}

/// Render the `/memory` reply body.
pub fn memory_body(agent: &str, user: &str) -> String {
    let agent_disp = if agent.trim().is_empty() {
        "(empty)"
    } else {
        agent
    };
    let user_disp = if user.trim().is_empty() {
        "(empty)"
    } else {
        user
    };
    format!("📒 Your memory\n\n[agent]\n{agent_disp}\n\n[user]\n{user_disp}")
}

/// Render the `/status` reply body. Uses pre-resolved
/// strings so the caller controls how to format mesh
/// health.
pub fn status_body(bridge_health: &str) -> String {
    format!("🩺 Mesh health\n{bridge_health}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_known_slash_commands() {
        assert_eq!(Command::parse("/start"), Command::Start);
        assert_eq!(Command::parse("/help"), Command::Help);
        assert_eq!(Command::parse("/status"), Command::Status);
        assert_eq!(Command::parse("/memory"), Command::Memory);
        assert_eq!(Command::parse("/forget"), Command::Forget);
    }

    #[test]
    fn parse_strips_bot_username_suffix() {
        // Telegram appends `@bot_username` to slash commands
        // sent in group chats.
        assert_eq!(Command::parse("/help@relixbot"), Command::Help);
        assert_eq!(
            Command::parse("/approve@relixbot task-7"),
            Command::Approve("task-7".to_string())
        );
    }

    #[test]
    fn parse_extracts_approve_argument() {
        assert_eq!(
            Command::parse("/approve abc-123"),
            Command::Approve("abc-123".into())
        );
        assert_eq!(
            Command::parse("/reject  xyz-9"),
            Command::Reject("xyz-9".into())
        );
    }

    #[test]
    fn parse_unknown_slash_command_falls_back_to_chat() {
        // Avoid accidentally swallowing future commands.
        let cmd = Command::parse("/wat is going on");
        assert_eq!(cmd, Command::Chat("/wat is going on".to_string()));
    }

    #[test]
    fn parse_plain_text_is_chat() {
        let cmd = Command::parse("hello there");
        assert_eq!(cmd, Command::Chat("hello there".to_string()));
    }

    #[test]
    fn parse_is_case_insensitive_on_command_head() {
        assert_eq!(Command::parse("/START"), Command::Start);
        assert_eq!(Command::parse("/Help"), Command::Help);
    }

    #[test]
    fn welcome_message_contains_relix_introduction() {
        let w = welcome_message();
        assert!(w.contains("Relix"));
        assert!(w.contains("/help"));
    }

    #[test]
    fn help_message_lists_every_supported_command() {
        let h = help_message();
        for cmd in [
            "/start", "/help", "/status", "/memory", "/forget", "/approve", "/reject",
        ] {
            assert!(h.contains(cmd), "help_message missing {cmd}");
        }
    }

    #[test]
    fn approval_notification_includes_all_fields() {
        let n = approval_notification("abc-123", "telegram:99@10:", "tool.web_fetch", "review");
        assert!(n.contains("abc-123"));
        assert!(n.contains("telegram:99@10:"));
        assert!(n.contains("tool.web_fetch"));
        assert!(n.contains("review"));
        // Both approve and reject suggestions are present.
        assert!(n.contains("/approve abc-123"));
        assert!(n.contains("/reject abc-123"));
    }

    #[test]
    fn approval_notification_handles_empty_reason() {
        let n = approval_notification("t1", "subj", "method", "");
        assert!(n.contains("(no reason given)"));
    }

    #[test]
    fn memory_body_renders_empty_targets_as_placeholder() {
        let m = memory_body("", "");
        assert!(m.contains("(empty)"));
    }

    #[test]
    fn memory_body_renders_non_empty_targets() {
        let m = memory_body("a-content", "u-content");
        assert!(m.contains("a-content"));
        assert!(m.contains("u-content"));
    }

    #[test]
    fn unauthorised_message_is_stable() {
        // Locked because the dashboard's user docs reference
        // this exact wording.
        assert_eq!(
            unauthorised_message(),
            "You are not authorized to use this bot."
        );
    }
}
