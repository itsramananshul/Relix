//! `[slack]` runtime config consumed when `node_type = "slack"`.

use std::path::PathBuf;

use serde::Deserialize;

/// Per-node Slack controller configuration.
///
/// Wire shape (controller TOML):
///
/// ```toml
/// [slack]
/// token_env  = "SLACK_BOT_TOKEN"
/// channel_id = "C01234567"
/// allowed_users    = []          # empty == allow everyone
/// allowed_groups   = ["chat-users"]
/// operator_user_id = ""          # reserved
/// messages_ring_capacity = 200
/// poll_interval_secs     = 2
///
/// [slack.memory_peer]
/// addr = "/ip4/127.0.0.1/tcp/19711"
///
/// [slack.ai_peer]
/// addr = "/ip4/127.0.0.1/tcp/19712"
///
/// [slack.coord_peer]
/// addr = "/ip4/127.0.0.1/tcp/19714"
/// ```
#[derive(Clone, Debug, Deserialize)]
pub struct SlackNodeConfig {
    pub token_env: String,
    pub channel_id: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default = "default_allowed_groups")]
    pub allowed_groups: Vec<String>,
    #[serde(default)]
    pub operator_user_id: String,
    #[serde(default = "default_ring_capacity")]
    pub messages_ring_capacity: usize,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    pub memory_peer: MemoryPeerConfig,
    pub ai_peer: AiPeerConfig,
    pub coord_peer: CoordPeerConfig,
    /// FIX 4 — path to the SQLite file holding the
    /// historical-message filter state. `None` ⇒ no
    /// persistent filter (existing behaviour — the controller
    /// processes the first batch Slack returns, which can
    /// include pre-boot history on a freshly-joined channel).
    /// Set this to enable the FIX 4 historical filter.
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
pub enum SlackNodeError {
    #[error("slack node config: {0}")]
    Config(String),
    #[error("slack node: token env '{0}' is not set")]
    MissingToken(String),
}

impl SlackNodeConfig {
    pub fn resolve_token(&self) -> Result<String, SlackNodeError> {
        let name = self.token_env.trim();
        if name.is_empty() {
            return Err(SlackNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
            _ => Err(SlackNodeError::MissingToken(name.to_string())),
        }
    }

    pub fn validate(&self) -> Result<(), SlackNodeError> {
        if self.token_env.trim().is_empty() {
            return Err(SlackNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        if self.channel_id.trim().is_empty() {
            return Err(SlackNodeError::Config(
                "channel_id is required (Slack id starting with C/G/D)".into(),
            ));
        }
        let first = self.channel_id.chars().next().unwrap_or(' ');
        if !matches!(first, 'C' | 'G' | 'D') {
            return Err(SlackNodeError::Config(
                "channel_id must start with C, G, or D (Slack id prefix)".into(),
            ));
        }
        if self.channel_id.len() < 9 {
            return Err(SlackNodeError::Config(
                "channel_id is too short (Slack ids are 9-12 chars)".into(),
            ));
        }
        if self.memory_peer.addr.trim().is_empty() {
            return Err(SlackNodeError::Config(
                "[slack.memory_peer].addr is required".into(),
            ));
        }
        if self.ai_peer.addr.trim().is_empty() {
            return Err(SlackNodeError::Config(
                "[slack.ai_peer].addr is required".into(),
            ));
        }
        if self.coord_peer.addr.trim().is_empty() {
            return Err(SlackNodeError::Config(
                "[slack.coord_peer].addr is required".into(),
            ));
        }
        if self.messages_ring_capacity == 0 {
            return Err(SlackNodeError::Config(
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

    fn parse(toml_text: &str) -> SlackNodeConfig {
        let v: toml::Value = toml::from_str(toml_text).expect("toml");
        v.try_into().expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg = parse(
            r#"
                token_env = "SLACK_BOT_TOKEN"
                channel_id = "C01234567"
                allowed_users = ["U01", "U02"]
                operator_user_id = "U99"
                messages_ring_capacity = 100
                [memory_peer]
                addr = "/ip4/127.0.0.1/tcp/19711"
                [ai_peer]
                addr = "/ip4/127.0.0.1/tcp/19712"
                [coord_peer]
                addr = "/ip4/127.0.0.1/tcp/19714"
            "#,
        );
        assert_eq!(cfg.token_env, "SLACK_BOT_TOKEN");
        assert_eq!(cfg.channel_id, "C01234567");
        assert_eq!(
            cfg.allowed_users,
            vec!["U01".to_string(), "U02".to_string()]
        );
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
                channel_id = "C01234567"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.allow_everyone());
        assert!(cfg.user_is_allowed("U999"));
    }

    #[test]
    fn permit_list_blocks_non_member() {
        let cfg = parse(
            r#"
                token_env = "X"
                channel_id = "C01234567"
                allowed_users = ["U01"]
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.user_is_allowed("U01"));
        assert!(!cfg.user_is_allowed("U02"));
    }

    #[test]
    fn validate_rejects_channel_id_wrong_prefix() {
        let cfg = parse(
            r#"
                token_env = "X"
                channel_id = "X01234567"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(SlackNodeError::Config(_))));
    }

    #[test]
    fn validate_rejects_empty_token_env() {
        let cfg = parse(
            r#"
                token_env = ""
                channel_id = "C01234567"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(SlackNodeError::Config(_))));
    }
}
