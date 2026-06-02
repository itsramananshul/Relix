//! `~/.relix/config.toml` — the persistent operator config that the
//! setup wizard writes and `relix boot` reads.
//!
//! Layout:
//!
//! ```toml
//! [provider]
//! name    = "openrouter"   # mock | openai | openrouter | xai | anthropic | gemini | local
//! api_key = "sk-or-..."    # stored here, not in env var; chmod 600 on POSIX
//!
//! [channels]
//! telegram        = true
//! telegram_token  = "..."
//! discord         = false
//! discord_token   = ""
//! discord_channel = ""
//! slack           = false
//! slack_token     = ""
//! slack_channel   = ""
//!
//! [mesh]
//! data_dir    = "~/.relix/data"
//! bridge_port = 19791
//! ```
//!
//! Every channel-specific field has a default so partial configs
//! still deserialise.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level config struct mirroring `~/.relix/config.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelixConfig {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub mesh: MeshConfig,
    /// Coordinator-side chronicle retention. Operators control
    /// when (or whether) old `task_events` rows get compacted; the
    /// wizard preserves these values across re-runs without
    /// exposing a UI for them. See
    /// [`docs/chronicle-retention.md`](../../../docs/chronicle-retention.md).
    #[serde(default)]
    pub coordinator: CoordinatorBlock,
    /// RELIX-7.19 GAP 4: `[confidence]` master switch +
    /// rolling-window depth. Off by default — operators flip
    /// `enabled = true` to wire the per-call ConfidenceScorer +
    /// FallbackEngine into every node's dispatch bridge. The
    /// wizard exposes the on/off switch; per-cap policies stay
    /// edit-the-toml-yourself for now since the policy DSL is
    /// richer than the wizard's yes/no shape.
    #[serde(default)]
    pub confidence: ConfidenceBlock,
    /// `[credentials]` — opt-in credential vault. When `enabled`,
    /// `relix boot` forwards `master_key` to the coordinator (as the
    /// `RELIX_CREDENTIAL_KEY` env var) and the mesh-up script emits a
    /// `[credentials] enabled` section so the vault caps register. The
    /// master key is a USER SECRET: the wizard generates a strong one
    /// (and surfaces it to save) when the user opts in without
    /// supplying one. An empty `master_key` keeps the vault disabled —
    /// there is NO hardcoded default key.
    #[serde(default)]
    pub credentials: CredentialsBlock,
    /// `[approvals]` — opt-in approval delivery. When `enabled` the
    /// mesh-up script emits `[approval]` + `[approval.delivery]` so the
    /// approval caps register. `channel` is the default delivery
    /// channel; `"dashboard"` (the in-process operator console) needs
    /// no external secret.
    #[serde(default)]
    pub approvals: ApprovalsBlock,
}

/// `[credentials]` block in `~/.relix/config.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialsBlock {
    #[serde(default)]
    pub enabled: bool,
    /// Vault master key (argon2 KDF input). Stored so `relix boot`
    /// can forward it to the coordinator. Empty ⇒ vault stays off
    /// (never a hardcoded default).
    #[serde(default)]
    pub master_key: String,
}

/// `[approvals]` block in `~/.relix/config.toml`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalsBlock {
    #[serde(default)]
    pub enabled: bool,
    /// Default delivery channel. `"dashboard"` is the in-process
    /// operator console and needs no external secret.
    #[serde(default = "default_approval_channel")]
    pub channel: String,
}

impl Default for ApprovalsBlock {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: default_approval_channel(),
        }
    }
}

fn default_approval_channel() -> String {
    "dashboard".to_string()
}

/// `[confidence]` block in `~/.relix/config.toml`. Mirrors the
/// subset of [`relix_runtime::confidence::ConfidenceConfig`]
/// the setup wizard exposes: master `enabled` switch +
/// rolling-window depth. Operators editing the file by hand
/// add `[[confidence.policies]]` blocks per spec.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfidenceBlock {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_confidence_window_size")]
    pub window_size: usize,
    #[serde(default = "default_confidence_p95_baseline")]
    pub p95_latency_baseline_ms: u64,
}

impl Default for ConfidenceBlock {
    fn default() -> Self {
        Self {
            enabled: false,
            window_size: default_confidence_window_size(),
            p95_latency_baseline_ms: default_confidence_p95_baseline(),
        }
    }
}

fn default_confidence_window_size() -> usize {
    100
}

fn default_confidence_p95_baseline() -> u64 {
    1500
}

/// `[coordinator]` block in `~/.relix/config.toml`. Only carries
/// retention today; future coordinator-side knobs (max_list,
/// recovery_scan, ...) live here too.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoordinatorBlock {
    #[serde(default)]
    pub retention: RetentionUserConfig,
}

/// `[coordinator.retention]` — operator-facing copy of
/// `relix_runtime::nodes::coordinator::RetentionConfig`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetentionUserConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_retention_max_age_days")]
    pub max_task_age_days: u32,
    #[serde(default = "default_retention_max_events_per_task")]
    pub max_events_per_task: u32,
    #[serde(default = "default_retention_compact_interval_h")]
    pub compact_interval_h: u32,
    #[serde(default = "default_retention_max_passes_per_run")]
    pub max_passes_per_run: u32,
}

impl Default for RetentionUserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_task_age_days: default_retention_max_age_days(),
            max_events_per_task: default_retention_max_events_per_task(),
            compact_interval_h: default_retention_compact_interval_h(),
            max_passes_per_run: default_retention_max_passes_per_run(),
        }
    }
}

fn default_retention_max_age_days() -> u32 {
    30
}
fn default_retention_max_events_per_task() -> u32 {
    500
}
fn default_retention_compact_interval_h() -> u32 {
    24
}
fn default_retention_max_passes_per_run() -> u32 {
    10
}

/// `[provider]` — picks the AI backend and carries its API key.
/// The `mock` provider needs no key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_name")]
    pub name: String,
    #[serde(default)]
    pub api_key: String,
    /// Optional model id (for example `openai/gpt-oss-120b:free`).
    /// Empty leaves the provider's baked-in `default_model` in place.
    /// On boot a non-empty value is forwarded as `RELIX_AI_MODEL`,
    /// which the mesh-up script writes as the provider's
    /// `default_model`. Lets a user pick a model (including a free one)
    /// without editing the AI-node TOML by hand.
    #[serde(default)]
    pub model: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: default_provider_name(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

fn default_provider_name() -> String {
    "mock".to_string()
}

/// `[channels]` — opt-in messaging adapters and their secrets.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: bool,
    #[serde(default)]
    pub telegram_token: String,

    #[serde(default)]
    pub discord: bool,
    #[serde(default)]
    pub discord_token: String,
    #[serde(default)]
    pub discord_channel: String,

    #[serde(default)]
    pub slack: bool,
    #[serde(default)]
    pub slack_token: String,
    #[serde(default)]
    pub slack_channel: String,
}

/// `[mesh]` — runtime parameters.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshConfig {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_bridge_port")]
    pub bridge_port: u16,
    /// Per-principal rate-limit budgets. Operators edit these
    /// directly in `~/.relix/config.toml`. The setup wizard
    /// preserves the existing values across a re-run but doesn't
    /// expose a UI for them yet — the defaults are sane for
    /// every local-deployment shape we ship.
    #[serde(default)]
    pub rate_limits: RateLimitsConfig,
}

/// `[mesh.rate_limits]` — copy of the bridge's
/// `crate::rate_limit::RateLimitConfig` shape so the wizard can
/// round-trip the section without depending on the bridge crate.
/// Field defaults match `crate::rate_limit::DEFAULT_*`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitsConfig {
    #[serde(default = "default_ai_per_min")]
    pub ai_calls_per_min: u32,
    #[serde(default = "default_dashboard_per_min")]
    pub dashboard_polls_per_min: u32,
    #[serde(default = "default_task_mut_per_min")]
    pub task_mutations_per_min: u32,
    #[serde(default = "default_ws_max_concurrent")]
    pub ws_max_concurrent: u32,
}

impl Default for RateLimitsConfig {
    fn default() -> Self {
        Self {
            ai_calls_per_min: default_ai_per_min(),
            dashboard_polls_per_min: default_dashboard_per_min(),
            task_mutations_per_min: default_task_mut_per_min(),
            ws_max_concurrent: default_ws_max_concurrent(),
        }
    }
}

fn default_ai_per_min() -> u32 {
    60
}
fn default_dashboard_per_min() -> u32 {
    120
}
fn default_task_mut_per_min() -> u32 {
    30
}
fn default_ws_max_concurrent() -> u32 {
    5
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bridge_port: default_bridge_port(),
            rate_limits: RateLimitsConfig::default(),
        }
    }
}

fn default_data_dir() -> String {
    "~/.relix/data".to_string()
}

fn default_bridge_port() -> u16 {
    19791
}

impl RelixConfig {
    /// `~/.relix/config.toml` — the canonical persistent location.
    pub fn default_path() -> PathBuf {
        relix_home().join("config.toml")
    }

    /// Read + parse the config at `path`. Returns `Ok(None)` when the
    /// file simply doesn't exist (the wizard hasn't run yet); returns
    /// `Err` only on real I/O / parse problems.
    #[allow(dead_code)] // wired into `relix boot` in a follow-up commit
    pub fn load_from(path: &Path) -> Result<Option<Self>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let cfg: Self = toml::from_str(&s).map_err(ConfigError::Parse)?;
                Ok(Some(cfg))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Convenience: load from the default path.
    #[allow(dead_code)] // wired into `relix boot` in a follow-up commit
    pub fn load_default() -> Result<Option<Self>, ConfigError> {
        Self::load_from(&Self::default_path())
    }

    /// Atomically write the config to `path`. Parent dir is created if
    /// missing. On POSIX the file is chmod 600 because it holds API
    /// keys.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
        let body = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        // Tmp-write + rename so an interrupted save can't leave a
        // half-written config in place.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, body).map_err(ConfigError::Io)?;
        // Restrict permissions before rename. POSIX chmod 0600;
        // Windows shells out to icacls to strip inheritance and
        // grant Full only to the current user. See
        // `crate::os_secure`.
        let _ = crate::os_secure::restrict_to_current_user(&tmp);
        std::fs::rename(&tmp, path).map_err(ConfigError::Io)?;
        // Re-apply after rename: NTFS may inherit fresh ACEs on
        // rename in some configurations. POSIX preserves mode.
        let _ = crate::os_secure::restrict_to_current_user(path);
        Ok(())
    }

    /// Reject configs that can't actually boot a mesh — e.g. a
    /// non-mock provider with an empty API key, or Telegram enabled
    /// without a bot token. Returns the list of all problems so the
    /// caller can surface them at once.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let p = self.provider.name.to_ascii_lowercase();
        let supported = [
            "mock",
            "openai",
            "openrouter",
            "xai",
            "anthropic",
            "gemini",
            "local",
        ];
        if !supported.contains(&p.as_str()) {
            errs.push(format!(
                "provider.name '{}' is not one of: {}",
                self.provider.name,
                supported.join(", ")
            ));
        }
        if p != "mock" && p != "local" && self.provider.api_key.trim().is_empty() {
            errs.push(format!(
                "provider.api_key is required when provider.name = \"{p}\""
            ));
        }
        if self.channels.telegram && self.channels.telegram_token.trim().is_empty() {
            errs.push("channels.telegram = true but channels.telegram_token is empty".into());
        }
        if self.channels.discord
            && (self.channels.discord_token.trim().is_empty()
                || self.channels.discord_channel.trim().is_empty())
        {
            errs.push(
                "channels.discord = true requires channels.discord_token \
                 and channels.discord_channel"
                    .into(),
            );
        }
        if self.channels.slack
            && (self.channels.slack_token.trim().is_empty()
                || self.channels.slack_channel.trim().is_empty())
        {
            errs.push(
                "channels.slack = true requires channels.slack_token \
                 and channels.slack_channel"
                    .into(),
            );
        }
        errs
    }
}

/// `~/.relix/` — the operator data root. Honours `RELIX_HOME` first,
/// then the user's home dir, falling back to `.relix` in CWD on the
/// unusual systems where neither resolves.
///
/// We resolve the home dir from env vars directly rather than via
/// the `dirs` crate so the workspace doesn't pull in `option-ext`
/// (MPL-2.0), which trips `cargo deny`'s license allowlist. Every
/// platform Relix supports surfaces the home dir through one of
/// these standard variables.
pub fn relix_home() -> PathBuf {
    if let Some(h) = std::env::var_os("RELIX_HOME") {
        return PathBuf::from(h);
    }
    if let Some(home) = home_dir() {
        return home.join(".relix");
    }
    PathBuf::from(".relix")
}

/// Cross-platform stand-in for `dirs::home_dir`. Reads `$HOME` on
/// POSIX; on Windows tries `%USERPROFILE%` first and falls back to
/// `%HOMEDRIVE%%HOMEPATH%`. Returns `None` when nothing resolves —
/// the caller falls back to a CWD-relative path.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Some(up) = std::env::var_os("USERPROFILE") {
            return Some(PathBuf::from(up));
        }
        match (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH")) {
            (Some(d), Some(p)) => {
                let mut s: std::ffi::OsString = d;
                s.push(p);
                Some(PathBuf::from(s))
            }
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Render an API key for display: keep the first 8 characters, then
/// 8 bullets, suppressing the actual key. Empty / very short keys
/// just become a row of bullets so we never accidentally leak the
/// real value when it's almost-but-not-quite empty.
pub fn mask_api_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(8);
    }
    let prefix: String = chars[..8].iter().collect();
    format!("{prefix}{}", "•".repeat(8))
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("config parse: {0}")]
    Parse(toml::de::Error),
    #[error("config serialize: {0}")]
    Serialize(toml::ser::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_are_safe_to_serialise_and_round_trip() {
        let c = RelixConfig::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: RelixConfig = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
        assert_eq!(back.provider.name, "mock");
        assert_eq!(back.mesh.bridge_port, 19791);
    }

    #[test]
    fn partial_config_uses_field_defaults() {
        // Operator-edited file that omits half the channel fields —
        // every missing field should get its default rather than
        // failing to parse.
        let src = r#"
            [provider]
            name = "openrouter"
            api_key = "sk-or-test"

            [channels]
            telegram = true
            telegram_token = "tg-token"
        "#;
        let c: RelixConfig = toml::from_str(src).unwrap();
        assert_eq!(c.provider.name, "openrouter");
        assert!(c.channels.telegram);
        assert!(!c.channels.discord);
        assert!(!c.channels.slack);
        assert_eq!(c.mesh.bridge_port, 19791);
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let mut c = RelixConfig::default();
        c.provider.name = "openai".into();
        c.provider.api_key = "sk-abc123xyz0987654321".into();
        c.channels.telegram = true;
        c.channels.telegram_token = "tg-1234".into();
        c.save_to(&path).expect("save");
        let back = RelixConfig::load_from(&path).expect("load").expect("some");
        assert_eq!(c, back);
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nope.toml");
        let res = RelixConfig::load_from(&path).expect("ok");
        assert!(res.is_none());
    }

    #[test]
    fn save_creates_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested").join("dir").join("config.toml");
        RelixConfig::default().save_to(&path).expect("save");
        assert!(path.exists());
    }

    #[test]
    fn mask_api_key_keeps_first_eight_then_bullets() {
        assert_eq!(mask_api_key("sk-or-abc12345xyzMORE"), "sk-or-ab••••••••");
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("short"), "••••••••");
        // Trailing whitespace must not leak past the trim().
        assert_eq!(mask_api_key("sk-or-abc12345xyz  "), "sk-or-ab••••••••");
    }

    #[test]
    fn validate_rejects_non_mock_provider_with_empty_key() {
        let c = RelixConfig {
            provider: ProviderConfig {
                name: "openai".into(),
                api_key: String::new(),
                model: String::new(),
            },
            ..Default::default()
        };
        let errs = c.validate();
        assert!(
            errs.iter().any(|e| e.contains("api_key is required")),
            "expected api-key error, got: {errs:?}"
        );
    }

    #[test]
    fn validate_accepts_mock_provider_without_key() {
        let c = RelixConfig::default(); // mock + empty key
        assert_eq!(c.validate(), Vec::<String>::new());
    }

    #[test]
    fn validate_rejects_telegram_without_token() {
        let mut c = RelixConfig::default();
        c.channels.telegram = true;
        let errs = c.validate();
        assert!(errs.iter().any(|e| e.contains("telegram_token")));
    }

    #[test]
    fn validate_rejects_discord_without_channel() {
        let mut c = RelixConfig::default();
        c.channels.discord = true;
        c.channels.discord_token = "x".into();
        // Missing channel id
        let errs = c.validate();
        assert!(errs.iter().any(|e| e.contains("discord_channel")));
    }

    #[test]
    fn validate_rejects_unknown_provider_name() {
        let mut c = RelixConfig::default();
        c.provider.name = "rumple".into();
        let errs = c.validate();
        assert!(errs.iter().any(|e| e.contains("is not one of")));
    }

    #[test]
    fn local_provider_does_not_require_api_key() {
        // Ollama / vLLM / etc. — no auth.
        let mut c = RelixConfig::default();
        c.provider.name = "local".into();
        assert_eq!(c.validate(), Vec::<String>::new());
    }
}
