//! Slash-command parsing for the slack controller. Same surface
//! as the discord controller: content-based detection of
//! `/<cmd>` at the start of the message body. No Slack slash-
//! command registration step — works without an out-of-band
//! setup.

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Memory,
    Forget,
    Chat(String),
}

impl Command {
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

pub fn brain_unreachable_message() -> &'static str {
    "I'm having trouble reaching my brain right now. Please try again in a moment."
}

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
        assert_eq!(Command::parse("hello"), Command::Chat("hello".to_string()));
    }

    #[test]
    fn parse_unknown_slash_falls_back_to_chat() {
        assert_eq!(
            Command::parse("/wat now"),
            Command::Chat("/wat now".to_string())
        );
    }

    #[test]
    fn parse_is_case_insensitive_on_head() {
        assert_eq!(Command::parse("/HELP"), Command::Help);
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
}
