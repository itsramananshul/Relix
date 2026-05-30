//! `[discord]` runtime config — what `node_type = "discord"`
//! consumes when booting a discord controller.

use std::path::PathBuf;

use serde::Deserialize;

/// Per-node discord controller configuration.
///
/// Wire shape (controller TOML):
///
/// ```toml
/// [discord]
/// token_env = "DISCORD_BOT_TOKEN"
/// channel_id = "12345678901234567"
/// allowed_users = []            # empty == allow everyone
/// allowed_groups = ["chat-users"]
/// operator_user_id = ""         # empty == disabled
/// messages_ring_capacity = 200
/// poll_interval_secs = 2
///
/// [discord.memory_peer]
/// addr = "/ip4/127.0.0.1/tcp/19711"
///
/// [discord.ai_peer]
/// addr = "/ip4/127.0.0.1/tcp/19712"
///
/// [discord.coord_peer]
/// addr = "/ip4/127.0.0.1/tcp/19714"
/// ```
#[derive(Clone, Debug, Deserialize)]
pub struct DiscordNodeConfig {
    /// Env var holding the bot token.
    pub token_env: String,

    /// Channel id the bot polls. Snowflake IDs are 64-bit
    /// integers but we keep them strings end-to-end.
    pub channel_id: String,

    /// Permit-list of Discord user ids (string snowflakes). Empty
    /// list means "allow everyone." Non-empty rejects anyone not
    /// present.
    #[serde(default)]
    pub allowed_users: Vec<String>,

    /// Names of identity groups the controller advertises in
    /// its policy `[admit]` block. Cosmetic — the actual
    /// admission decision is made by the policy engine.
    #[serde(default = "default_allowed_groups")]
    pub allowed_groups: Vec<String>,

    /// Discord user_id of the operator. When non-empty, the
    /// future approval-notifier surface will reach them here.
    /// (Today the field is reserved; no notifier loop runs.)
    #[serde(default)]
    pub operator_user_id: String,

    /// Inbound-message ring capacity. The dashboard's recent-
    /// messages widget reads from this ring.
    #[serde(default = "default_ring_capacity")]
    pub messages_ring_capacity: usize,

    /// Seconds between REST poll cycles. Default 2 — slow
    /// enough to stay well clear of Discord's per-route rate
    /// limit (50 requests / second / channel for GET messages,
    /// but we share the bucket with sendMessage), fast enough
    /// for a responsive chat surface.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Memory peer — required.
    pub memory_peer: MemoryPeerConfig,
    /// AI peer — required.
    pub ai_peer: AiPeerConfig,
    /// Coordinator peer — required.
    pub coord_peer: CoordPeerConfig,

    /// FIX 2 — path to the SQLite file holding the
    /// per-channel polling watermark. Absent ⇒ in-memory
    /// cursor only (existing behaviour: a bridge restart
    /// re-bootstraps from the channel tail). Set this to
    /// enable restart-safe watermark persistence.
    #[serde(default)]
    pub state_db_path: Option<PathBuf>,
}

fn default_allowed_groups() -> Vec<String> {
    vec!["chat-users".to_string()]
}

fn default_ring_capacity() -> usize {
    200
}

fn default_poll_interval() -> u64 {
    2
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemoryPeerConfig {
    pub addr: String,
    #[serde(default = "default_memory_alias")]
    pub alias: String,
    #[serde(default = "default_memory_deadline")]
    pub deadline_secs: i64,
}

fn default_memory_alias() -> String {
    "memory".to_string()
}
fn default_memory_deadline() -> i64 {
    10
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiPeerConfig {
    pub addr: String,
    #[serde(default = "default_ai_alias")]
    pub alias: String,
    #[serde(default = "default_ai_deadline")]
    pub deadline_secs: i64,
}

fn default_ai_alias() -> String {
    "ai".to_string()
}
fn default_ai_deadline() -> i64 {
    60
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoordPeerConfig {
    pub addr: String,
    #[serde(default = "default_coord_alias")]
    pub alias: String,
    #[serde(default = "default_coord_deadline")]
    pub deadline_secs: i64,
}

fn default_coord_alias() -> String {
    "coordinator".to_string()
}
fn default_coord_deadline() -> i64 {
    10
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordNodeError {
    #[error("discord node config: {0}")]
    Config(String),
    #[error("discord node: token env '{0}' is not set")]
    MissingToken(String),
}

impl DiscordNodeConfig {
    pub fn resolve_token(&self) -> Result<String, DiscordNodeError> {
        let name = self.token_env.trim();
        if name.is_empty() {
            return Err(DiscordNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
            _ => Err(DiscordNodeError::MissingToken(name.to_string())),
        }
    }

    pub fn validate(&self) -> Result<(), DiscordNodeError> {
        if self.token_env.trim().is_empty() {
            return Err(DiscordNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        if self.channel_id.trim().is_empty() {
            return Err(DiscordNodeError::Config(
                "channel_id is required (Discord snowflake string)".into(),
            ));
        }
        if !self.channel_id.chars().all(|c| c.is_ascii_digit()) || self.channel_id.len() < 10 {
            return Err(DiscordNodeError::Config(
                "channel_id must be a numeric snowflake (>= 10 digits)".into(),
            ));
        }
        if self.memory_peer.addr.trim().is_empty() {
            return Err(DiscordNodeError::Config(
                "[discord.memory_peer].addr is required".into(),
            ));
        }
        if self.ai_peer.addr.trim().is_empty() {
            return Err(DiscordNodeError::Config(
                "[discord.ai_peer].addr is required".into(),
            ));
        }
        if self.coord_peer.addr.trim().is_empty() {
            return Err(DiscordNodeError::Config(
                "[discord.coord_peer].addr is required".into(),
            ));
        }
        if self.messages_ring_capacity == 0 {
            return Err(DiscordNodeError::Config(
                "messages_ring_capacity must be > 0".into(),
            ));
        }
        Ok(())
    }

    pub fn allow_everyone(&self) -> bool {
        self.allowed_users.is_empty()
    }

    pub fn user_is_allowed(&self, user_id: &str) -> bool {
        self.allow_everyone() || self.allowed_users.iter().any(|u| u == user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> DiscordNodeConfig {
        let v: toml::Value = toml::from_str(toml_text).expect("toml");
        v.try_into().expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg = parse(
            r#"
                token_env = "DISCORD_BOT_TOKEN"
                channel_id = "12345678901234567"
                allowed_users = ["42", "1234"]
                operator_user_id = "99"
                messages_ring_capacity = 100
                [memory_peer]
                addr = "/ip4/127.0.0.1/tcp/19711"
                [ai_peer]
                addr = "/ip4/127.0.0.1/tcp/19712"
                [coord_peer]
                addr = "/ip4/127.0.0.1/tcp/19714"
            "#,
        );
        assert_eq!(cfg.token_env, "DISCORD_BOT_TOKEN");
        assert_eq!(cfg.channel_id, "12345678901234567");
        assert_eq!(
            cfg.allowed_users,
            vec!["42".to_string(), "1234".to_string()]
        );
        assert_eq!(cfg.operator_user_id, "99");
        cfg.validate().unwrap();
        assert_eq!(cfg.memory_peer.alias, "memory");
        assert_eq!(cfg.ai_peer.alias, "ai");
        assert_eq!(cfg.coord_peer.alias, "coordinator");
    }

    #[test]
    fn allow_everyone_when_list_empty() {
        let cfg = parse(
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
        );
        assert!(cfg.allow_everyone());
        assert!(cfg.user_is_allowed("999"));
    }

    #[test]
    fn permit_list_blocks_non_member() {
        let cfg = parse(
            r#"
                token_env = "X"
                channel_id = "12345678901234567"
                allowed_users = ["42"]
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(!cfg.allow_everyone());
        assert!(cfg.user_is_allowed("42"));
        assert!(!cfg.user_is_allowed("43"));
    }

    #[test]
    fn validate_rejects_empty_token_env() {
        let cfg = parse(
            r#"
                token_env = ""
                channel_id = "12345678901234567"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(DiscordNodeError::Config(_))));
    }

    #[test]
    fn validate_rejects_non_numeric_channel_id() {
        let cfg = parse(
            r#"
                token_env = "X"
                channel_id = "abcdefghij"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(DiscordNodeError::Config(_))));
    }
}
