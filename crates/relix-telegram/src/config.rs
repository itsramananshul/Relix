//! `[telegram]` config section.

use std::path::PathBuf;

use serde::Deserialize;

/// Per-node Telegram channel configuration. Mirrors the shape
/// described in
/// [`docs/channel-node-architecture.md`](../../../docs/channel-node-architecture.md);
/// every operator-facing knob lives here.
#[derive(Clone, Debug, Deserialize)]
pub struct TelegramConfig {
    /// Environment variable the controller will read the Bot
    /// API token from. The token MUST NOT live in any
    /// checked-in config; this is the indirection point.
    pub bot_token_env: String,

    /// Delivery mode: `long_poll` (default — no public
    /// ingress required) or `webhook` (requires TLS
    /// termination + a separate handler).
    #[serde(default = "default_mode")]
    pub mode: DeliveryMode,

    /// Per-chat inbound rate cap. Messages above the cap
    /// receive a static "rate-limited, try again" reply
    /// without creating a Task.
    #[serde(default = "default_rate")]
    pub max_inbound_per_chat_per_minute: u32,

    /// SOL flow template the channel hands every inbound
    /// message to. Resolved relative to the controller's
    /// `flows/` directory.
    pub flow_template: PathBuf,

    /// Hard per-message runtime ceiling. The Coordinator's
    /// recovery scan flips overdue rows to `interrupted`.
    #[serde(default = "default_max_runtime")]
    pub max_runtime_secs: u32,

    /// Coordinator peer alias (matches a `[peers]` entry on
    /// the channel controller's TOML).
    pub coordinator_alias: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    LongPoll,
    Webhook,
}

fn default_mode() -> DeliveryMode {
    DeliveryMode::LongPoll
}

fn default_rate() -> u32 {
    6
}

fn default_max_runtime() -> u32 {
    60
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("telegram config: {0}")]
    Config(String),
    #[error("telegram config: bot_token_env '{0}' is not set in the environment")]
    MissingToken(String),
}

impl TelegramConfig {
    /// Resolve the bot token from the configured env var. Does
    /// NOT log the value. Returns `MissingToken` so the
    /// controller can fail loudly at startup instead of running
    /// without auth.
    pub fn resolve_token(&self) -> Result<String, TelegramError> {
        std::env::var(&self.bot_token_env)
            .map_err(|_| TelegramError::MissingToken(self.bot_token_env.clone()))
    }

    /// Validate the config without touching the network or the
    /// environment. Use at startup to fail-fast on obviously
    /// bad config.
    pub fn validate(&self) -> Result<(), TelegramError> {
        if self.bot_token_env.trim().is_empty() {
            return Err(TelegramError::Config(
                "bot_token_env must be a non-empty env var name".into(),
            ));
        }
        if self.coordinator_alias.trim().is_empty() {
            return Err(TelegramError::Config(
                "coordinator_alias must be a non-empty peer alias".into(),
            ));
        }
        if self.flow_template.as_os_str().is_empty() {
            return Err(TelegramError::Config(
                "flow_template must point at a SOL flow path".into(),
            ));
        }
        if self.max_runtime_secs == 0 {
            return Err(TelegramError::Config(
                "max_runtime_secs must be > 0 (recovery-scan deadline)".into(),
            ));
        }
        if self.max_inbound_per_chat_per_minute == 0 {
            return Err(TelegramError::Config(
                "max_inbound_per_chat_per_minute = 0 would block every \
                 message; set a non-zero cap"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(toml: &str) -> TelegramConfig {
        toml::from_str(toml).expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "RELIX_TELEGRAM_BOT_TOKEN"
            mode = "long_poll"
            max_inbound_per_chat_per_minute = 12
            flow_template = "flows/channel_telegram.sol"
            max_runtime_secs = 120
            coordinator_alias = "coordinator"
        "#);
        assert_eq!(cfg.bot_token_env, "RELIX_TELEGRAM_BOT_TOKEN");
        assert_eq!(cfg.mode, DeliveryMode::LongPoll);
        assert_eq!(cfg.max_inbound_per_chat_per_minute, 12);
        assert_eq!(cfg.max_runtime_secs, 120);
        assert_eq!(cfg.coordinator_alias, "coordinator");
        cfg.validate().unwrap();
    }

    #[test]
    fn defaults_applied() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.mode, DeliveryMode::LongPoll);
        assert_eq!(cfg.max_inbound_per_chat_per_minute, 6);
        assert_eq!(cfg.max_runtime_secs, 60);
    }

    #[test]
    fn webhook_mode_parses() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            mode = "webhook"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.mode, DeliveryMode::Webhook);
    }

    #[test]
    fn empty_token_env_rejected() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = ""
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert!(matches!(cfg.validate(), Err(TelegramError::Config(_))));
    }

    #[test]
    fn zero_rate_limit_rejected() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            max_inbound_per_chat_per_minute = 0
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert!(matches!(cfg.validate(), Err(TelegramError::Config(_))));
    }

    #[test]
    fn zero_max_runtime_rejected() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            max_runtime_secs = 0
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert!(matches!(cfg.validate(), Err(TelegramError::Config(_))));
    }

    #[test]
    fn resolve_token_surfaces_missing_env() {
        // Use a name that's almost certainly not in env. Don't
        // set anything — relies on the env var not existing.
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "RELIX_TEST_DEFINITELY_NOT_SET_xyz123"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        match cfg.resolve_token() {
            Err(TelegramError::MissingToken(name)) => {
                assert_eq!(name, "RELIX_TEST_DEFINITELY_NOT_SET_xyz123");
            }
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }
}
