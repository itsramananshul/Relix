//! Operator-facing `[slack]` config — minimal validation shared
//! between dashboard editing and the live controller's startup
//! check. The richer `node_type = "slack"` runtime config lives
//! in `crates/relix-runtime/src/nodes/slack/config.rs`.

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct SlackConfig {
    /// Env var holding the bot token. The raw token NEVER appears
    /// in this struct.
    pub bot_token_env: String,
    /// Slack channel ID — `C…` for public, `G…` for private,
    /// `D…` for IM. The controller only polls a single channel.
    pub channel_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("slack config: {0}")]
    Config(String),
    #[error("slack config: bot token env '{0}' is not set")]
    MissingToken(String),
}

impl SlackConfig {
    pub fn resolve_token(&self) -> Result<String, SlackError> {
        std::env::var(&self.bot_token_env)
            .map_err(|_| SlackError::MissingToken(self.bot_token_env.clone()))
    }

    pub fn validate(&self) -> Result<(), SlackError> {
        if self.bot_token_env.trim().is_empty() {
            return Err(SlackError::Config(
                "bot_token_env must be a non-empty env var name".into(),
            ));
        }
        if self.channel_id.trim().is_empty() {
            return Err(SlackError::Config(
                "channel_id must be a non-empty Slack channel id".into(),
            ));
        }
        let first = self.channel_id.chars().next().unwrap_or(' ');
        // Slack channel ids start with C (public), G (private), or
        // D (IM). Anything else is almost certainly a typo
        // (e.g. user pasted the channel name or a workspace url).
        if !matches!(first, 'C' | 'G' | 'D') {
            return Err(SlackError::Config(
                "channel_id must start with C, G, or D (Slack id prefix)".into(),
            ));
        }
        if self.channel_id.len() < 9 {
            return Err(SlackError::Config(
                "channel_id is too short (Slack ids are 9-12 chars)".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(toml: &str) -> SlackConfig {
        toml::from_str(toml).expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg = mk(r#"
            bot_token_env = "RELIX_SLACK_BOT_TOKEN"
            channel_id    = "C01234567"
        "#);
        assert_eq!(cfg.bot_token_env, "RELIX_SLACK_BOT_TOKEN");
        assert_eq!(cfg.channel_id, "C01234567");
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_empty_token_env() {
        let cfg = mk(r#"
            bot_token_env = ""
            channel_id    = "C01234567"
        "#);
        assert!(matches!(cfg.validate(), Err(SlackError::Config(_))));
    }

    #[test]
    fn rejects_channel_id_with_wrong_prefix() {
        let cfg = mk(r#"
            bot_token_env = "X"
            channel_id    = "Xabcdefgh"
        "#);
        assert!(matches!(cfg.validate(), Err(SlackError::Config(_))));
    }

    #[test]
    fn accepts_private_channel_id() {
        let cfg = mk(r#"
            bot_token_env = "X"
            channel_id    = "G01234567"
        "#);
        cfg.validate().unwrap();
    }

    #[test]
    fn accepts_dm_channel_id() {
        let cfg = mk(r#"
            bot_token_env = "X"
            channel_id    = "D01234567"
        "#);
        cfg.validate().unwrap();
    }

    #[test]
    fn resolve_token_surfaces_missing_env() {
        let cfg = mk(r#"
            bot_token_env = "RELIX_TEST_NOPE_xyz_slack"
            channel_id    = "C01234567"
        "#);
        match cfg.resolve_token() {
            Err(SlackError::MissingToken(name)) => {
                assert_eq!(name, "RELIX_TEST_NOPE_xyz_slack");
            }
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }
}
