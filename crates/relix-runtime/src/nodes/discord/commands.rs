//! Slash-command parsing + canned message strings for the
//! discord controller. Slash commands are detected by message
//! content starting with `/` (the spec deliberately avoids
//! Discord's formal slash-command registration system —
//! content-based detection works without an out-of-band setup
//! step).

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `/help` — list available commands.
    Help,
    /// `/status` — mesh health summary.
    Status,
    /// `/memory` — show the caller's agent + user memory.
    Memory,
    /// `/forget` — clear the caller's agent + user memory.
    Forget,
    /// Anything that isn't a recognised command. Carries the
    /// original (trimmed) text so the controller can pipe it
    /// into `ai.chat`.
    Chat(String),
}

impl Command {
    /// Parse the inbound content into a [`Command`]. Leading
    /// whitespace is trimmed.
    pub fn parse(text: &str) -> Command {
        let trimmed = text.trim();
        if !trimmed.starts_with('/') {
            return Command::Chat(trimmed.to_string());
        }
        let (head_raw, _rest) = match trimmed.split_once(char::is_whitespace) {
            Some((h, r)) => (h, r.trim()),
            None => (trimmed, ""),
        };
        let head = head_raw.trim_start_matches('/');
        match head.to_ascii_lowercase().as_str() {
            "help" => Command::Help,
            "status" => Command::Status,
            "memory" => Command::Memory,
            "forget" => Command::Forget,
            _ => Command::Chat(trimmed.to_string()),
        }
    }
}

/// `/help` body.
pub fn help_message() -> String {
    "Commands:\n\
     /help — this message.\n\
     /status — mesh health summary.\n\
     /memory — show your persistent agent + user memory.\n\
     /forget — wipe your persistent memory.\n\
     Anything else is treated as a chat message and routed to \
     the canonical chat flow."
        .to_string()
}

/// User-friendly fallback when the AI peer is unreachable.
pub fn brain_unreachable_message() -> &'static str {
    "I'm having trouble reaching my brain right now. Please try again in a moment."
}

/// User-facing "not authorised" message returned to callers
/// outside the permit list.
pub fn unauthorised_message() -> &'static str {
    "You are not authorized."
}

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
    format!("Your memory\n\n[agent]\n{agent_disp}\n\n[user]\n{user_disp}")
}

pub fn status_body(bridge_health: &str) -> String {
    format!("Mesh health\n{bridge_health}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_known_slash_commands() {
        assert_eq!(Command::parse("/help"), Command::Help);
        assert_eq!(Command::parse("/status"), Command::Status);
        assert_eq!(Command::parse("/memory"), Command::Memory);
        assert_eq!(Command::parse("/forget"), Command::Forget);
    }

    #[test]
    fn parse_plain_text_is_chat() {
        let c = Command::parse("hello there");
        assert_eq!(c, Command::Chat("hello there".to_string()));
    }

    #[test]
    fn parse_unknown_slash_falls_back_to_chat() {
        let c = Command::parse("/wat is going on");
        assert_eq!(c, Command::Chat("/wat is going on".to_string()));
    }

    #[test]
    fn parse_is_case_insensitive_on_command_head() {
        assert_eq!(Command::parse("/HELP"), Command::Help);
        assert_eq!(Command::parse("/Status"), Command::Status);
    }

    #[test]
    fn help_message_lists_every_supported_command() {
        let h = help_message();
        for cmd in ["/help", "/status", "/memory", "/forget"] {
            assert!(h.contains(cmd), "help message missing {cmd}");
        }
    }

    #[test]
    fn unauthorised_message_is_stable() {
        assert_eq!(unauthorised_message(), "You are not authorized.");
    }

    #[test]
    fn memory_body_renders_empty_targets_as_placeholder() {
        let m = memory_body("", "");
        assert!(m.contains("(empty)"));
    }
}
