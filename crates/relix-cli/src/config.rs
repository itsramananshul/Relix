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
}

/// `[provider]` — picks the AI backend and carries its API key.
/// The `mock` provider needs no key.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_name")]
    pub name: String,
    #[serde(default)]
    pub api_key: String,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: default_provider_name(),
            api_key: String::new(),
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
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bridge_port: default_bridge_port(),
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
pub fn relix_home() -> PathBuf {
    if let Some(h) = std::env::var_os("RELIX_HOME") {
        return PathBuf::from(h);
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".relix");
    }
    PathBuf::from(".relix")
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
