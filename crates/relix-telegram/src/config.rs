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

    /// FIX 6: TTL hours for the session-mapping store. Sessions
    /// (chat_id, message_id) -> task_id rows older than this
    /// are deleted by the background sweep. Default 24h matches
    /// Telegram's own conversation-staleness heuristic; raise
    /// for long-running batch flows; lower if the dashboard's
    /// session count is growing unbounded. The sweep runs every
    /// hour regardless of this value.
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: u32,

    /// FIX 1: public HTTPS URL Telegram should POST updates to
    /// when the controller is in webhook mode. Empty / unset
    /// disables webhook mode entirely. The URL MUST be the
    /// publicly-reachable URL of the bridge's
    /// `POST /v1/channels/telegram/webhook` route. Telegram
    /// requires TLS so this is always `https://…`.
    ///
    /// Mode arbitration is `effective_mode()`: when
    /// `webhook_url` is set AND `mode != "long_poll"`, the
    /// controller calls `setWebhook` at startup and does NOT
    /// start the long-poll loop. When `webhook_url` is absent,
    /// long-poll is forced regardless of `mode`. This keeps
    /// pre-FIX-1 deployments working unchanged.
    #[serde(default)]
    pub webhook_url: Option<String>,
}

impl TelegramConfig {
    /// FIX 1: mutually-exclusive resolution of `mode` +
    /// `webhook_url`. Returns the actual mode the controller
    /// should run in:
    ///
    /// - `webhook_url` set AND mode != long_poll ⇒ Webhook
    /// - `webhook_url` absent ⇒ LongPoll (forced)
    /// - mode == long_poll explicitly ⇒ LongPoll (operator
    ///   override; webhook_url is ignored)
    pub fn effective_mode(&self) -> DeliveryMode {
        let has_url = self
            .webhook_url
            .as_deref()
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false);
        match (has_url, self.mode) {
            (true, DeliveryMode::Webhook) => DeliveryMode::Webhook,
            // Operator explicitly chose long_poll → respect it
            // even when webhook_url is also set.
            (_, DeliveryMode::LongPoll) => DeliveryMode::LongPoll,
            // webhook_url absent + mode = webhook is a misconfig
            // (no URL means no Telegram pushes); fail safe by
            // long-polling.
            (false, DeliveryMode::Webhook) => DeliveryMode::LongPoll,
        }
    }
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

fn default_session_ttl_hours() -> u32 {
    crate::session_store::DEFAULT_SESSION_TTL_HOURS
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

    /// FIX 1: webhook_url + mode = "webhook" → effective Webhook.
    #[test]
    fn fix1_effective_mode_webhook_when_url_and_mode_match() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            mode = "webhook"
            webhook_url = "https://example.com/v1/channels/telegram/webhook"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.effective_mode(), DeliveryMode::Webhook);
    }

    /// FIX 1: webhook_url present but mode explicitly long_poll
    /// → operator override wins, effective LongPoll.
    #[test]
    fn fix1_effective_mode_respects_explicit_long_poll_override() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            mode = "long_poll"
            webhook_url = "https://example.com/webhook"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.effective_mode(), DeliveryMode::LongPoll);
    }

    /// FIX 1: mode = "webhook" but webhook_url absent →
    /// fail-safe LongPoll (no URL means no Telegram pushes;
    /// long-poll is the only viable receive path).
    #[test]
    fn fix1_effective_mode_fail_safes_to_long_poll_when_url_absent() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            mode = "webhook"
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.effective_mode(), DeliveryMode::LongPoll);
    }

    /// FIX 1: empty webhook_url string is treated as absent.
    #[test]
    fn fix1_effective_mode_treats_empty_webhook_url_as_absent() {
        let cfg: TelegramConfig = mk(r#"
            bot_token_env = "X"
            mode = "webhook"
            webhook_url = "   "
            flow_template = "f.sol"
            coordinator_alias = "c"
        "#);
        assert_eq!(cfg.effective_mode(), DeliveryMode::LongPoll);
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
