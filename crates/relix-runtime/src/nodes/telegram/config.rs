//! `[telegram]` config section for the `node_type = "telegram"`
//! controller. Distinct from `relix_telegram::TelegramConfig`
//! which scopes the *original* scaffold-era validation; this
//! type is what the runtime actually consumes when booting a
//! telegram controller.

use std::path::PathBuf;

use serde::Deserialize;

/// Per-node telegram controller configuration.
///
/// Wire shape (controller TOML):
///
/// ```toml
/// [telegram]
/// token_env = "TELEGRAM_BOT_TOKEN"
/// allowed_users = []           # empty == allow everyone
/// allowed_groups = ["chat-users"]
/// operator_chat_id = 0         # 0 == disabled
/// messages_ring_capacity = 200
/// flow_template = "flows/chat_template.sol"  # not yet wired — reserved
/// session_db_path = "dev-data/local/telegram_sessions.db"
/// poll_interval_secs = 1       # idle delay between empty polls
/// approval_poll_interval_secs = 15
///
/// [telegram.memory_peer]
/// addr = "/ip4/127.0.0.1/tcp/19711"
/// alias = "memory"
/// deadline_secs = 10
///
/// [telegram.ai_peer]
/// addr = "/ip4/127.0.0.1/tcp/19712"
/// alias = "ai"
/// deadline_secs = 60
///
/// [telegram.coord_peer]
/// addr = "/ip4/127.0.0.1/tcp/19714"
/// alias = "coordinator"
/// deadline_secs = 10
/// ```
#[derive(Clone, Debug, Deserialize)]
pub struct TelegramNodeConfig {
    /// Name of the env var holding the Bot API token. The
    /// raw token never appears in this struct — only the
    /// indirection.
    pub token_env: String,

    /// Permit-list of Telegram numeric user ids. Empty list
    /// means "allow everyone." Non-empty lists reject any
    /// caller whose user_id is not present.
    #[serde(default)]
    pub allowed_users: Vec<i64>,

    /// Names of identity groups the controller advertises in
    /// its policy `[admit]` block. Carried through to logs and
    /// dashboard for the operator's reference; the actual
    /// admission decision is made by the policy engine.
    #[serde(default = "default_allowed_groups")]
    pub allowed_groups: Vec<String>,

    /// Telegram chat_id of the operator. When non-zero, the
    /// controller's approval-notifier polls the coordinator
    /// and posts an "approval required" message here for
    /// every task that enters `awaiting_input`.
    #[serde(default)]
    pub operator_chat_id: i64,

    /// Inbound-message ring capacity. The dashboard's recent-
    /// messages widget reads from this ring.
    #[serde(default = "default_ring_capacity")]
    pub messages_ring_capacity: usize,

    /// Path to the canonical chat SOL flow. Reserved for the
    /// SOL-flow integration; the current controller runs the
    /// equivalent memory + ai dispatch sequence directly.
    /// Stored for forwards compatibility — not validated.
    #[serde(default)]
    pub flow_template: PathBuf,

    /// SQLite path for the persistent session store. When
    /// absent the controller falls back to the in-memory
    /// session map and loses in-flight mappings on restart.
    #[serde(default)]
    pub session_db_path: Option<PathBuf>,

    /// Delay between empty `getUpdates` polls. Telegram's
    /// long-poll already blocks server-side; this is the
    /// idle backoff for the rare case the server returns
    /// immediately without updates.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,

    /// Interval the approval-notifier polls the coordinator
    /// for tasks in `awaiting_input`. Default 15s — fast
    /// enough for human approval workflows, slow enough not
    /// to thrash the coordinator.
    #[serde(default = "default_approval_poll_interval")]
    pub approval_poll_interval_secs: u64,

    /// Memory peer — required.
    pub memory_peer: MemoryPeerConfig,

    /// AI peer — required.
    pub ai_peer: AiPeerConfig,

    /// Coordinator peer — required.
    pub coord_peer: CoordPeerConfig,

    /// Audio-tool peer — optional. When present, the
    /// controller will dial this peer and route voice messages
    /// through `tool.audio.transcribe`. When absent, voice
    /// messages get a static "voice transcription not
    /// configured" reply instead of being silently dropped.
    #[serde(default)]
    pub audio_peer: Option<AudioPeerConfig>,

    /// FIX 1: explicit delivery-mode selector. Defaults to
    /// `long_poll` so pre-FIX-1 deployments behave unchanged.
    /// See [`Self::effective_mode`] for the arbitration with
    /// [`Self::webhook_url`].
    #[serde(default = "default_delivery_mode")]
    pub mode: relix_telegram::config::DeliveryMode,

    /// FIX 1: public HTTPS URL Telegram should POST updates to
    /// in webhook mode. Empty / unset disables webhook mode
    /// entirely (long-poll is forced). When set and `mode !=
    /// "long_poll"`, the controller calls `setWebhook` at
    /// startup and does NOT start the long-poll loop.
    #[serde(default)]
    pub webhook_url: Option<String>,
}

fn default_delivery_mode() -> relix_telegram::config::DeliveryMode {
    relix_telegram::config::DeliveryMode::LongPoll
}

fn default_allowed_groups() -> Vec<String> {
    vec!["chat-users".to_string()]
}

fn default_ring_capacity() -> usize {
    200
}

fn default_poll_interval() -> u64 {
    1
}

fn default_approval_poll_interval() -> u64 {
    15
}

/// Memory peer config — `memory.write_turn` /
/// `memory.recent_for_session` / `memory.agent_*` calls
/// land here.
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

/// AI peer config — `ai.chat` calls land here.
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

/// Coordinator peer config — `task.create` / `task.update` /
/// `task.event` / `task.list` calls land here.
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

/// Tool peer that hosts `tool.audio.transcribe`. When the
/// operator wires this section, voice messages get transcribed
/// before going through the chat flow; when absent, voice
/// messages get a static fallback reply.
#[derive(Clone, Debug, Deserialize)]
pub struct AudioPeerConfig {
    pub addr: String,
    #[serde(default = "default_audio_alias")]
    pub alias: String,
    /// Per-call deadline. Default 90s — Whisper transcription
    /// is CPU-bound and a longer voice clip can chew through
    /// most of that budget.
    #[serde(default = "default_audio_deadline")]
    pub deadline_secs: i64,
}

fn default_audio_alias() -> String {
    "tool".to_string()
}

fn default_audio_deadline() -> i64 {
    90
}

#[derive(Debug, thiserror::Error)]
pub enum TelegramNodeError {
    #[error("telegram node config: {0}")]
    Config(String),
    #[error("telegram node: token env '{0}' is not set")]
    MissingToken(String),
    #[error("telegram node: getMe failed: {0}")]
    GetMeFailed(String),
}

impl TelegramNodeConfig {
    /// Resolve the bot token from the configured env var.
    /// Never logged.
    pub fn resolve_token(&self) -> Result<String, TelegramNodeError> {
        let name = self.token_env.trim();
        if name.is_empty() {
            return Err(TelegramNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        match std::env::var(name) {
            Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
            _ => Err(TelegramNodeError::MissingToken(name.to_string())),
        }
    }

    /// Validate without touching env vars or the network.
    pub fn validate(&self) -> Result<(), TelegramNodeError> {
        if self.token_env.trim().is_empty() {
            return Err(TelegramNodeError::Config(
                "token_env must be a non-empty env var name".into(),
            ));
        }
        if self.memory_peer.addr.trim().is_empty() {
            return Err(TelegramNodeError::Config(
                "[telegram.memory_peer].addr is required".into(),
            ));
        }
        if self.ai_peer.addr.trim().is_empty() {
            return Err(TelegramNodeError::Config(
                "[telegram.ai_peer].addr is required".into(),
            ));
        }
        if self.coord_peer.addr.trim().is_empty() {
            return Err(TelegramNodeError::Config(
                "[telegram.coord_peer].addr is required".into(),
            ));
        }
        if self.messages_ring_capacity == 0 {
            return Err(TelegramNodeError::Config(
                "messages_ring_capacity must be > 0".into(),
            ));
        }
        Ok(())
    }

    /// `true` when the operator did not configure a permit
    /// list — all callers are allowed.
    pub fn allow_everyone(&self) -> bool {
        self.allowed_users.is_empty()
    }

    /// `true` when the caller's user_id passes the permit
    /// check. Empty allow-list ⇒ always true.
    pub fn user_is_allowed(&self, user_id: i64) -> bool {
        self.allow_everyone() || self.allowed_users.contains(&user_id)
    }

    /// `true` when the operator has wired an approval-notify
    /// chat. Zero == disabled.
    pub fn approval_notifications_enabled(&self) -> bool {
        self.operator_chat_id != 0
    }

    /// FIX 1: mutually-exclusive resolution of `mode` +
    /// `webhook_url`. See `relix_telegram::TelegramConfig::effective_mode`
    /// for the rule table; this runtime-side mirror applies the
    /// exact same arbitration so a single source-of-truth is
    /// observed across both config surfaces.
    pub fn effective_mode(&self) -> relix_telegram::config::DeliveryMode {
        use relix_telegram::config::DeliveryMode;
        let has_url = self
            .webhook_url
            .as_deref()
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false);
        match (has_url, self.mode) {
            (true, DeliveryMode::Webhook) => DeliveryMode::Webhook,
            (_, DeliveryMode::LongPoll) => DeliveryMode::LongPoll,
            (false, DeliveryMode::Webhook) => DeliveryMode::LongPoll,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> TelegramNodeConfig {
        let v: toml::Value = toml::from_str(toml_text).expect("toml");
        v.try_into().expect("parse")
    }

    #[test]
    fn parses_full_section() {
        let cfg = parse(
            r#"
                token_env = "TELEGRAM_BOT_TOKEN"
                allowed_users = [42, 1234]
                operator_chat_id = 7
                messages_ring_capacity = 100
                [memory_peer]
                addr = "/ip4/127.0.0.1/tcp/19711"
                [ai_peer]
                addr = "/ip4/127.0.0.1/tcp/19712"
                [coord_peer]
                addr = "/ip4/127.0.0.1/tcp/19714"
            "#,
        );
        assert_eq!(cfg.token_env, "TELEGRAM_BOT_TOKEN");
        assert_eq!(cfg.allowed_users, vec![42, 1234]);
        assert_eq!(cfg.operator_chat_id, 7);
        assert_eq!(cfg.messages_ring_capacity, 100);
        cfg.validate().unwrap();
        // defaulted aliases
        assert_eq!(cfg.memory_peer.alias, "memory");
        assert_eq!(cfg.ai_peer.alias, "ai");
        assert_eq!(cfg.coord_peer.alias, "coordinator");
    }

    #[test]
    fn allow_everyone_when_list_empty() {
        let cfg = parse(
            r#"
                token_env = "X"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.allow_everyone());
        assert!(cfg.user_is_allowed(999));
    }

    #[test]
    fn permit_list_blocks_non_member() {
        let cfg = parse(
            r#"
                token_env = "X"
                allowed_users = [42]
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(!cfg.allow_everyone());
        assert!(cfg.user_is_allowed(42));
        assert!(!cfg.user_is_allowed(43));
    }

    #[test]
    fn approval_notifications_disabled_when_zero() {
        let cfg = parse(
            r#"
                token_env = "X"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(!cfg.approval_notifications_enabled());
    }

    #[test]
    fn approval_notifications_enabled_when_chat_set() {
        let cfg = parse(
            r#"
                token_env = "X"
                operator_chat_id = -1001
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.approval_notifications_enabled());
    }

    #[test]
    fn validate_rejects_empty_token_env() {
        let cfg = parse(
            r#"
                token_env = ""
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(TelegramNodeError::Config(_))));
    }

    #[test]
    fn validate_rejects_empty_peer_addr() {
        let cfg = parse(
            r#"
                token_env = "X"
                [memory_peer]
                addr = ""
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(matches!(cfg.validate(), Err(TelegramNodeError::Config(_))));
    }

    #[test]
    fn resolve_token_surfaces_missing_env() {
        let cfg = parse(
            r#"
                token_env = "RELIX_TEST_NOPE_xyz_123_telegram"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        match cfg.resolve_token() {
            Err(TelegramNodeError::MissingToken(name)) => {
                assert_eq!(name, "RELIX_TEST_NOPE_xyz_123_telegram");
            }
            other => panic!("expected MissingToken, got {other:?}"),
        }
    }
}
