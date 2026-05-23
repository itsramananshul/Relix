//! Operator-facing `[discord]` config — minimal validation shared
//! between dashboard editing and the live controller's startup
//! check. The richer `node_type = "discord"` runtime config lives
//! in `crates/relix-runtime/src/nodes/discord/config.rs` so peer
//! aliases (memory / ai / coordinator) stay with the runtime
//! plumbing.

use serde::Deserialize;

/// Minimal client-side Discord config — the subset the dashboard
/// and the scaffold-era validator need. The runtime crate has its
/// own richer `DiscordNodeConfig` with peer wiring.
#[derive(Clone, Debug, Deserialize)]
pub struct DiscordConfig {
    /// Env var holding the bot token. The raw token NEVER appears
    /// in this struct.
    pub bot_token_env: String,
    /// Channel id the bot polls for inbound messages. Snowflake
    /// IDs are 64-bit integers but Discord serialises them as
    /// strings — keep them strings here too.
    pub channel_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    #[error("discord config: {0}")]
    Config(String),
    #[error("discord config: bot token env '{0}' is not set")]
    MissingToken(String),
}

impl DiscordConfig {
    pub fn resolve_token(&self) -> Result<String, DiscordError> {
        std::env::var(&self.bot_token_env)
            .map_err(|_| DiscordError::MissingToken(self.bot_token_env.clone()))
    }

    pub fn validate(&self) -> Result<(), DiscordError> {
        if self.bot_token_env.trim().is_empty() {
            return Err(DiscordError::Config(
                "bot_token_env must be a non-empty env var name".into(),
            ));
        }
        if self.channel_id.trim().is_empty() {
            return Err(DiscordError::Config(
                "channel_id must be a non-empty Discord snowflake string".into(),
            ));
        }
        // Snowflake sanity: numeric-only, at least 10 digits.
        // Don't enforce the exact length (Discord widens this over
        // time) — just reject obvious typos.
        if !self.channel_id.chars().all(|c| c.is_ascii_digit()) || self.channel_id.len() < 10 {
            return Err(DiscordError::Config(
                "channel_id must be numeric (Discord snowflake)".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(toml: &str) -> DiscordConfig {
        toml::from_str(toml).expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg: DiscordConfig = mk(r#"
            bot_token_env = "RELIX_DISCORD_BOT_TOKEN"
            channel_id    = "12345678901234567"
        "#);
        assert_eq!(cfg.bot_token_env, "RELIX_DISCORD_BOT_TOKEN");
        assert_eq!(cfg.channel_id, "12345678901234567");
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_empty_token_env() {
        let cfg = mk(r#"
            bot_token_env = ""
            channel_id    = "12345678901234567"
        "#);
        assert!(matches!(cfg.validate(), Err(DiscordError::Config(_))));
    }

    #[test]
    fn rejects_non_numeric_channel_id() {
        let cfg = mk(r#"
            bot_token_env = "X"
            channel_id    = "abcdefghij"
        "#);
        assert!(matches!(cfg.validate(), Err(DiscordError::Config(_))));
    }

    #[test]
    fn rejects_too_short_channel_id() {
        let cfg = mk(r#"
            bot_token_env = "X"
            channel_id    = "123"
        "#);
        assert!(matches!(cfg.validate(), Err(DiscordError::Config(_))));
    }

    #[test]
    fn resolve_token_surfaces_missing_env() {
        let cfg = mk(r#"
            bot_token_env = "RELIX_TEST_NOPE_xyz_discord"
            channel_id    = "12345678901234567"
        "#);
        match cfg.resolve_token() {
            Err(DiscordError::MissingToken(name)) => {
                assert_eq!(name, "RELIX_TEST_NOPE_xyz_discord");
            }
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }
}
