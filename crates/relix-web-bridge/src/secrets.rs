//! Bridge-owned operator secrets: AI provider keys + Telegram
//! bot token.
//!
//! Persisted to a single TOML file (`bridge-secrets.toml`) at
//! mode 0600 on POSIX. The bridge is the only writer. The file
//! is local to one bridge process and gitignored — operators
//! supplying keys via the dashboard never expose them in version
//! control.
//!
//! The dashboard NEVER receives a raw secret back. The
//! [`status()`] helpers return only metadata (`configured`,
//! `key_preview`, `key_set_at`). The full value is read at AI
//! controller / channel startup time, NOT at every request.
//!
//! See `docs/dashboard-redesign.md` for the full security
//! contract.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Provider names the dashboard is allowed to configure. The
/// list is enforced by the config endpoints — submitting a
/// name outside this set returns 422. Keep in sync with the
/// dashboard's provider cards.
pub const ALLOWED_PROVIDERS: &[&str] =
    &["mock", "openai", "anthropic", "openrouter", "xai", "google"];

/// Telegram delivery modes the dashboard accepts. `polling` is
/// the only shipped mode today; `webhook` is in the schema for
/// forward-compat but submitting it returns 422.
pub const ALLOWED_TELEGRAM_MODES: &[&str] = &["polling", "webhook"];

/// On-disk shape — TOML. Both sections default to empty so
/// a fresh bridge that hasn't written this file yet loads as
/// `BridgeSecrets::default()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BridgeSecrets {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// The actual API key. Never exposed via any HTTP response.
    pub api_key: String,
    /// Operator-chosen default model id (e.g. `gpt-4o`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Wall-clock unix seconds at which the entry was last
    /// written. Surfaced by [`provider_status()`] so the
    /// dashboard can detect "key set after last controller
    /// restart, restart required."
    pub set_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramEntry {
    pub bot_token: String,
    /// `polling` or `webhook`. Only `polling` is functional
    /// today.
    #[serde(default = "default_telegram_mode")]
    pub mode: String,
    pub set_at: i64,
}

fn default_telegram_mode() -> String {
    "polling".to_string()
}

/// Per-provider redacted status returned by the config
/// endpoints. The dashboard renders this; the raw secret is
/// never echoed back.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderStatus {
    pub name: String,
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Last 4 chars of the key, prefixed with an ellipsis.
    /// Empty / unset secret returns `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_preview: Option<String>,
    /// Wall-clock unix seconds the key was last set. `None`
    /// when the provider is unconfigured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_set_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramStatus {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_preview: Option<String>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_set_at: Option<i64>,
}

impl BridgeSecrets {
    /// Read the on-disk file if it exists; return an empty
    /// `BridgeSecrets` otherwise. Failure modes (file unreadable,
    /// not valid UTF-8, malformed TOML) return `Default::default()`
    /// and emit a warning — the bridge stays up but operators
    /// who configured providers see them as unconfigured. Better
    /// than refusing to boot.
    pub fn load_or_empty(path: &Path) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self::default();
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "bridge-secrets: read failed; treating as empty");
                return Self::default();
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(path = %path.display(),
                    "bridge-secrets: file is not valid UTF-8; treating as empty");
                return Self::default();
            }
        };
        match toml::from_str::<BridgeSecrets>(&text) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "bridge-secrets: TOML parse failed; treating as empty");
                Self::default()
            }
        }
    }

    /// Serialise + write the file. On POSIX, sets mode 0600 so
    /// only the bridge's user can read it. Atomic write via
    /// `.tmp` rename so a crashed write doesn't leave a partial
    /// file.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text =
            toml::to_string_pretty(self).map_err(|e| format!("bridge-secrets serialise: {e}"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("bridge-secrets mkdir {}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("bridge-secrets write {}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)
                .map_err(|e| format!("bridge-secrets stat tmp: {e}"))?
                .permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tmp, perms)
                .map_err(|e| format!("bridge-secrets chmod tmp: {e}"))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            format!(
                "bridge-secrets rename {} -> {}: {e}",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Build the redacted status for one provider name.
    /// `name` must be in [`ALLOWED_PROVIDERS`]; the caller
    /// validates that before calling.
    pub fn provider_status(&self, name: &str) -> ProviderStatus {
        match self.providers.get(name) {
            Some(e) => ProviderStatus {
                name: name.to_string(),
                configured: !e.api_key.is_empty(),
                default_model: e.default_model.clone(),
                key_preview: redact(&e.api_key),
                key_set_at: Some(e.set_at),
            },
            None => ProviderStatus {
                name: name.to_string(),
                configured: false,
                default_model: None,
                key_preview: None,
                key_set_at: None,
            },
        }
    }

    /// Redacted status for every allowed provider, sorted by
    /// the canonical allowlist order so the dashboard shows
    /// providers in a stable order regardless of which ones
    /// are configured.
    pub fn all_provider_statuses(&self) -> Vec<ProviderStatus> {
        ALLOWED_PROVIDERS
            .iter()
            .map(|p| self.provider_status(p))
            .collect()
    }

    /// Redacted Telegram status. `configured` is `false` and
    /// `mode` defaults to `polling` when the section is absent.
    pub fn telegram_status(&self) -> TelegramStatus {
        match &self.telegram {
            Some(t) => TelegramStatus {
                configured: !t.bot_token.is_empty(),
                token_preview: redact(&t.bot_token),
                mode: t.mode.clone(),
                token_set_at: Some(t.set_at),
            },
            None => TelegramStatus {
                configured: false,
                token_preview: None,
                mode: "polling".to_string(),
                token_set_at: None,
            },
        }
    }

    /// Insert or replace a provider entry. Stamps `set_at`
    /// with the current time. Caller is responsible for
    /// validating `name` against [`ALLOWED_PROVIDERS`] +
    /// rejecting empty `api_key`.
    pub fn set_provider(&mut self, name: &str, api_key: String, default_model: Option<String>) {
        self.providers.insert(
            name.to_string(),
            ProviderEntry {
                api_key,
                default_model,
                set_at: unix_secs(),
            },
        );
    }

    /// Remove a provider entry, if present. Idempotent.
    pub fn delete_provider(&mut self, name: &str) {
        self.providers.remove(name);
    }

    /// Insert or replace the Telegram entry. Caller is
    /// responsible for validating `mode` against
    /// [`ALLOWED_TELEGRAM_MODES`] + rejecting empty
    /// `bot_token` + rejecting `webhook` until the live
    /// client lands.
    pub fn set_telegram(&mut self, bot_token: String, mode: String) {
        self.telegram = Some(TelegramEntry {
            bot_token,
            mode,
            set_at: unix_secs(),
        });
    }
}

/// Shared read-write handle around a `BridgeSecrets`. Cloned
/// into every config endpoint via `AppState`.
#[derive(Clone)]
pub struct SecretsHandle {
    inner: Arc<RwLock<BridgeSecrets>>,
    path: Arc<PathBuf>,
}

impl SecretsHandle {
    pub fn new(initial: BridgeSecrets, path: PathBuf) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
            path: Arc::new(path),
        }
    }

    pub fn read<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&BridgeSecrets) -> T,
    {
        let g = self.inner.read().expect("secrets read lock");
        f(&g)
    }

    /// Apply a mutation + persist. Returns the error verbatim
    /// from `BridgeSecrets::save` on failure; on success
    /// returns whatever `f` produces. The lock is held for the
    /// full duration so concurrent writes serialise.
    pub fn mutate<F, T>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&mut BridgeSecrets) -> T,
    {
        let mut g = self.inner.write().expect("secrets write lock");
        let out = f(&mut g);
        g.save(&self.path)?;
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Return the last 4 characters of `secret`, prefixed with an
/// ellipsis. Empty / `None`-equivalent secrets return `None`.
/// Short secrets (≤4 chars) return `"…****"` so we never
/// leak a fingerprint of an obviously-too-short key.
///
/// Per the design doc, we deliberately take the TAIL not the
/// head — provider-prefix fingerprints (`sk-`, `xai-`, …)
/// would be too revealing.
pub fn redact(secret: &str) -> Option<String> {
    if secret.is_empty() {
        return None;
    }
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 4 {
        return Some("…****".to_string());
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    Some(format!("…{tail}"))
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_returns_none_for_empty_string() {
        assert!(redact("").is_none());
    }

    #[test]
    fn redact_masks_short_secret_without_leaking_bytes() {
        // Secrets ≤ 4 chars return the "…****" sentinel, not
        // the chars themselves. Otherwise a 4-char "test"
        // would leak entirely as "…test".
        assert_eq!(redact("a").as_deref(), Some("…****"));
        assert_eq!(redact("abcd").as_deref(), Some("…****"));
    }

    #[test]
    fn redact_returns_ellipsis_plus_last_four_chars_only() {
        assert_eq!(redact("sk-1234567890abcdef").as_deref(), Some("…cdef"));
    }

    #[test]
    fn redact_handles_multibyte_secrets() {
        // unicode-safe — uses character indexing, not byte
        // indexing. Last 4 chars by Unicode scalar value.
        let s = "aaaaλλλλ";
        let r = redact(s).unwrap();
        assert_eq!(r, "…λλλλ");
    }

    #[test]
    fn provider_status_unconfigured_omits_preview_and_set_at() {
        let s = BridgeSecrets::default();
        let p = s.provider_status("openai");
        assert!(!p.configured);
        assert!(p.key_preview.is_none());
        assert!(p.key_set_at.is_none());
        assert!(p.default_model.is_none());
    }

    #[test]
    fn provider_status_configured_reports_preview_and_default_model() {
        let mut s = BridgeSecrets::default();
        s.set_provider(
            "openai",
            "sk-test-1234567890abcdef".into(),
            Some("gpt-4o".into()),
        );
        let p = s.provider_status("openai");
        assert!(p.configured);
        assert_eq!(p.key_preview.as_deref(), Some("…cdef"));
        assert!(p.key_set_at.is_some());
        assert_eq!(p.default_model.as_deref(), Some("gpt-4o"));
        // Sanity: the API key itself is NOT present anywhere
        // in the serialised status — a future renamed-field
        // regression would catch a leak.
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("sk-test-1234567890abcdef"),
            "raw key leaked into ProviderStatus JSON: {json}"
        );
        assert!(
            !json.contains("1234567890"),
            "key body leaked into ProviderStatus JSON: {json}"
        );
    }

    #[test]
    fn all_provider_statuses_returns_one_per_allowlist_entry() {
        let s = BridgeSecrets::default();
        let v = s.all_provider_statuses();
        assert_eq!(v.len(), ALLOWED_PROVIDERS.len());
        // Order matches the allowlist for stable dashboard render.
        for (i, p) in v.iter().enumerate() {
            assert_eq!(p.name, ALLOWED_PROVIDERS[i]);
            assert!(!p.configured);
        }
    }

    #[test]
    fn telegram_status_unconfigured_defaults_to_polling_mode() {
        let s = BridgeSecrets::default();
        let t = s.telegram_status();
        assert!(!t.configured);
        assert_eq!(t.mode, "polling");
        assert!(t.token_preview.is_none());
    }

    #[test]
    fn telegram_status_configured_reports_redacted_token() {
        let mut s = BridgeSecrets::default();
        s.set_telegram("1234567:ABCDEFghijklmnop".into(), "polling".into());
        let t = s.telegram_status();
        assert!(t.configured);
        assert_eq!(t.token_preview.as_deref(), Some("…mnop"));
        let json = serde_json::to_string(&t).unwrap();
        assert!(
            !json.contains("1234567:ABCDEFghijklmnop"),
            "raw token leaked into TelegramStatus JSON: {json}"
        );
    }

    #[test]
    fn delete_provider_is_idempotent() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.delete_provider("openai");
        s.delete_provider("openai");
        s.delete_provider("never-set");
        assert!(s.providers.is_empty());
    }

    #[test]
    fn round_trip_through_disk_preserves_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-xyz".into(), Some("gpt-4o".into()));
        s.set_telegram("1234:abcdef".into(), "polling".into());
        s.save(&path).unwrap();
        let r = BridgeSecrets::load_or_empty(&path);
        assert_eq!(r.providers.get("openai").unwrap().api_key, "sk-xyz");
        assert_eq!(
            r.providers.get("openai").unwrap().default_model.as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(r.telegram.as_ref().unwrap().bot_token, "1234:abcdef");
        assert_eq!(r.telegram.as_ref().unwrap().mode, "polling");
    }

    #[test]
    fn load_or_empty_returns_default_when_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let s = BridgeSecrets::load_or_empty(&path);
        assert!(s.providers.is_empty());
        assert!(s.telegram.is_none());
    }

    #[test]
    fn load_or_empty_returns_default_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.toml");
        std::fs::write(&path, "this is = not = valid = toml [[").unwrap();
        let s = BridgeSecrets::load_or_empty(&path);
        assert!(s.providers.is_empty());
        assert!(s.telegram.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_mode_0600_on_posix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("secrets.toml");
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.save(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }
}
