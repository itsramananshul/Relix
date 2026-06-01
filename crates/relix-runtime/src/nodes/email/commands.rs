//! Inbound command parsing + outbound templating helpers for the
//! email channel.
//!
//! Commands are detected on the first non-empty line of the
//! decoded plain-text body. The same `/help`, `/status`,
//! `/memory`, `/forget` set the other channels expose lives here
//! too — operators get a consistent surface regardless of which
//! channel they talk through.
//!
//! Templates are stored at `<data_dir>/email-templates/<name>.toml`
//! when present; the controller falls back to a small set of
//! built-ins (`welcome`, `reset_password`, `task_completed`,
//! `task_failed`) so a fresh install can send mail without
//! authoring any files first.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Status,
    Memory,
    Forget,
    Chat(String),
}

impl Command {
    pub fn parse(body: &str) -> Command {
        let first = body
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if !first.starts_with('/') {
            return Command::Chat(body.trim().to_string());
        }
        let head = first
            .split_whitespace()
            .next()
            .unwrap_or(first)
            .trim_start_matches('/');
        match head.to_ascii_lowercase().as_str() {
            "help" => Command::Help,
            "status" => Command::Status,
            "memory" => Command::Memory,
            "forget" => Command::Forget,
            _ => Command::Chat(body.trim().to_string()),
        }
    }
}

pub fn help_message() -> String {
    "Email Relix Commands:\n\n\
     /help   — this message.\n\
     /status — mesh health summary.\n\
     /memory — show your persistent agent + user memory.\n\
     /forget — wipe your persistent memory.\n\
     \nAnything else is treated as a chat message and routed to \
     the canonical chat flow."
        .to_string()
}

pub fn brain_unreachable_message() -> &'static str {
    "I'm having trouble reaching my brain right now. Please retry in a moment."
}

pub fn unauthorised_message() -> &'static str {
    "Your address is not on this Relix node's allow-list."
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

pub fn status_body(summary: &str) -> String {
    format!("Mesh health\n{summary}")
}

/// Render an oversized-bounce reply body.
pub fn oversize_message(bytes: u64, limit: u64) -> String {
    format!(
        "Your message was too large to process.\n\n\
         Received: {bytes} bytes\n\
         Limit:    {limit} bytes\n\n\
         Please attach files via a link service or split the \
         content across several smaller messages."
    )
}

// ── templates ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailTemplate {
    pub name: String,
    pub subject: String,
    pub body: String,
    pub html: Option<String>,
}

/// Resolve `template_name` to a concrete template. Checks the
/// operator's `<data_dir>/email-templates/<name>.toml` directory
/// first (if `RELIX_EMAIL_TEMPLATES_DIR` is set), then falls
/// back to the built-in registry.
pub fn find_template(name: &str) -> Option<EmailTemplate> {
    // SECTION 8 (sweep): `name` is the wire `template_name` from
    // `email.send`. Reject path-traversal names BEFORE joining to
    // the templates directory — `../../etc/passwd` must not read
    // an arbitrary host file. Only a plain filename component is
    // allowed; invalid names fall through to the built-in lookup
    // (which is a keyed map, not a filesystem read).
    let is_safe_filename = !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && !name.contains('\0')
        && matches!(
            {
                let mut c = std::path::Path::new(name).components();
                (c.next(), c.next())
            },
            (Some(std::path::Component::Normal(_)), None)
        );
    if is_safe_filename && let Ok(dir) = std::env::var("RELIX_EMAIL_TEMPLATES_DIR") {
        let path = PathBuf::from(dir).join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(parsed) = toml::from_str::<EmailTemplateToml>(&text)
        {
            return Some(EmailTemplate {
                name: name.to_string(),
                subject: parsed.subject,
                body: parsed.body,
                html: parsed.html,
            });
        }
    }
    BUILT_INS.get_or_init(built_in_templates).get(name).cloned()
}

#[derive(Debug, serde::Deserialize)]
struct EmailTemplateToml {
    subject: String,
    body: String,
    #[serde(default)]
    html: Option<String>,
}

static BUILT_INS: OnceLock<BTreeMap<String, EmailTemplate>> = OnceLock::new();

fn built_in_templates() -> BTreeMap<String, EmailTemplate> {
    let mut m = BTreeMap::new();
    m.insert(
        "welcome".to_string(),
        EmailTemplate {
            name: "welcome".into(),
            subject: "Welcome to Relix, {{name}}".into(),
            body: "Hi {{name}},\n\nYour Relix account is ready.\n\n— Relix".into(),
            html: Some(
                "<p>Hi {{name}},</p><p>Your Relix account is ready.</p><p>— Relix</p>".into(),
            ),
        },
    );
    m.insert(
        "reset_password".to_string(),
        EmailTemplate {
            name: "reset_password".into(),
            subject: "Reset your password".into(),
            body: "Hi {{name}},\n\nUse this link to reset your password: {{link}}\n\n— Relix"
                .into(),
            html: None,
        },
    );
    m.insert(
        "task_completed".to_string(),
        EmailTemplate {
            name: "task_completed".into(),
            subject: "Task complete: {{task_title}}".into(),
            body: "Your task '{{task_title}}' finished successfully.\n\nResult:\n{{result}}\n"
                .into(),
            html: None,
        },
    );
    m.insert(
        "task_failed".to_string(),
        EmailTemplate {
            name: "task_failed".into(),
            subject: "Task failed: {{task_title}}".into(),
            body: "Your task '{{task_title}}' failed.\n\nError:\n{{error}}\n".into(),
            html: None,
        },
    );
    m
}

/// Replace `{{var}}` substrings with the matching value from
/// `vars`. Unknown variables are left as the literal `{{var}}`
/// so the operator can spot them in the rendered output. Pure
/// string substitution; no expression language, no escaping.
pub fn render_template(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len()
            && bytes[i] == b'{'
            && bytes[i + 1] == b'{'
            && let Some(close_rel) = template[i + 2..].find("}}")
        {
            let var_name = template[i + 2..i + 2 + close_rel].trim();
            match vars.get(var_name) {
                Some(v) => out.push_str(v),
                None => out.push_str(&template[i..i + 2 + close_rel + 2]),
            }
            i = i + 2 + close_rel + 2;
            continue;
        }
        out.push(template.as_bytes()[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognises_known_slash_commands() {
        assert_eq!(Command::parse("/help"), Command::Help);
        assert_eq!(Command::parse("/STATUS"), Command::Status);
        assert_eq!(Command::parse("/memory"), Command::Memory);
        assert_eq!(Command::parse("/forget"), Command::Forget);
    }

    #[test]
    fn parse_plain_text_is_chat() {
        assert_eq!(
            Command::parse("hello there"),
            Command::Chat("hello there".to_string())
        );
    }

    #[test]
    fn parse_unknown_slash_falls_back_to_chat() {
        assert_eq!(
            Command::parse("/foo bar"),
            Command::Chat("/foo bar".to_string())
        );
    }

    #[test]
    fn parse_uses_first_non_blank_line() {
        assert_eq!(Command::parse("\n\n  /help  \nignored"), Command::Help);
    }

    #[test]
    fn help_lists_every_supported_command() {
        let h = help_message();
        for cmd in ["/help", "/status", "/memory", "/forget"] {
            assert!(h.contains(cmd), "help body missing {cmd}");
        }
    }

    #[test]
    fn oversize_message_includes_both_numbers() {
        let m = oversize_message(20_000_000, 10_485_760);
        assert!(m.contains("20000000"));
        assert!(m.contains("10485760"));
    }

    #[test]
    fn render_template_substitutes_known_vars() {
        let mut vars = BTreeMap::new();
        vars.insert("name".to_string(), "Anshul".to_string());
        let out = render_template("Hi {{name}}", &vars);
        assert_eq!(out, "Hi Anshul");
    }

    #[test]
    fn render_template_leaves_unknown_vars_literal() {
        let vars = BTreeMap::new();
        let out = render_template("Hi {{missing}}", &vars);
        assert_eq!(out, "Hi {{missing}}");
    }

    #[test]
    fn render_template_handles_empty_template() {
        let vars = BTreeMap::new();
        assert_eq!(render_template("", &vars), "");
    }

    #[test]
    fn render_template_handles_no_substitutions() {
        let vars = BTreeMap::new();
        let out = render_template("plain text", &vars);
        assert_eq!(out, "plain text");
    }

    #[test]
    fn render_template_handles_multiple_substitutions() {
        let mut vars = BTreeMap::new();
        vars.insert("a".to_string(), "1".to_string());
        vars.insert("b".to_string(), "2".to_string());
        let out = render_template("{{a}} and {{b}}", &vars);
        assert_eq!(out, "1 and 2");
    }

    #[test]
    fn built_in_templates_include_welcome_and_reset_password() {
        let w = find_template("welcome").unwrap();
        assert!(w.subject.contains("{{name}}"));
        let r = find_template("reset_password").unwrap();
        assert!(r.body.contains("{{link}}"));
    }

    #[test]
    fn find_template_returns_none_for_unknown_name() {
        assert!(find_template("definitely-not-a-template").is_none());
    }

    /// The env-driven override is exercised at integration
    /// time — the unit test layer can't set / unset env vars
    /// because `#![forbid(unsafe_code)]` blocks
    /// `std::env::set_var` (which the 2024 edition flags
    /// unsafe). The built-in registry tests above prove the
    /// rest of the resolver path.
    #[test]
    fn find_template_returns_none_when_env_dir_not_set() {
        // No env wiring needed — RELIX_EMAIL_TEMPLATES_DIR
        // isn't set by the test harness.
        assert!(find_template("never-defined-template").is_none());
        // SECTION 8 sweep: a traversal template_name must not
        // read an arbitrary host file — it falls through to the
        // built-in (keyed) lookup, which has no such entry.
        assert!(find_template("../../etc/passwd").is_none());
        assert!(find_template("a/b").is_none());
        assert!(find_template("..").is_none());
    }
}
