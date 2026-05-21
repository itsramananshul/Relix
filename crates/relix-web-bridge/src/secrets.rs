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
    /// Operator-marked default provider name. Hint only — the
    /// AI controller still reads its provider config from its
    /// own TOML, so changing the default here doesn't switch
    /// the live runtime. The dashboard surfaces it as the
    /// "default" badge so operators can record their intended
    /// default without losing it across restarts.
    ///
    /// MUST be in [`ALLOWED_PROVIDERS`] when present; the
    /// config endpoints enforce this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
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
    /// Operator-marked enabled flag. Hint only — the AI
    /// controller reads its provider config from its own
    /// TOML at startup, so flipping this does NOT switch
    /// the live runtime. Defaults to `true` for back-compat
    /// with entries created before this field landed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    // ── M58: persistent last-test cache ────────────────────
    //
    // Every successful or failed call to
    // `POST /v1/config/providers/:name/test` writes these
    // fields. Operators glancing at the provider card see
    // "last tested ok 200 · 245ms · 3 min ago" without
    // re-running the test, and the value persists across
    // bridge restarts because it lives in the same
    // bridge-secrets.toml the keys do.
    /// Unix seconds of the most recent test call. `None`
    /// when the provider has never been tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_at: Option<i64>,
    /// `Some(true)` for success, `Some(false)` for failure,
    /// `None` when never tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_ok: Option<bool>,
    /// Upstream HTTP status code for the most recent test, when
    /// the request reached the provider's server. `None` for
    /// transport failures (DNS, TCP, TLS) — the failure mode
    /// surfaces in `last_test_detail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_status_code: Option<u16>,
    /// Elapsed milliseconds of the most recent test call.
    /// `None` when never tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_elapsed_ms: Option<u64>,
    /// Redaction-safe human summary from the most recent test.
    /// Same source as the live test response's `detail` field —
    /// NEVER includes the raw key or arbitrary upstream body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test_detail: Option<String>,
    // ── M69: operator-set quarantine ────────────────────────
    //
    // Real persistent state. The bridge enforces it at the
    // boundaries it controls (today: the test-provider
    // endpoint refuses live calls during a cooldown). The
    // AI controller does NOT live-read this flag yet — that
    // requires a runtime reload primitive (separate milestone).
    // Operators see the gap in the dashboard's quarantine
    // banner copy.
    /// Unix seconds at which the operator most recently set
    /// the quarantine. `None` when not quarantined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantined_at: Option<i64>,
    /// Operator-supplied short reason captured at quarantine
    /// time. Capped at `MAX_OPERATOR_NOTE_LEN` (validated
    /// by the bridge endpoint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    /// Unix seconds before which the bridge refuses to run
    /// `test_provider` against this entry. `None` = no
    /// active cooldown. Independent of `quarantined_at`:
    /// operators can quarantine without a cooldown (manual
    /// review) or set a cooldown without a permanent
    /// quarantine (auto-recovery window after a known
    /// flap).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    // ── M77: cumulative routing-trace counters ─────────────
    //
    // Counts every failed test against this provider's saved
    // key. Distinct from `last_test_ok` (snapshot of the
    // most recent call) — this is the long-running
    // reliability signal an operator uses to spot a degraded
    // provider that mostly works but flaps. Increments on
    // every fail; never decrements. Reset deliberately by
    // operator-clearing the field via a future endpoint (not
    // shipped — until then operators see the lifetime count).
    /// Total count of failed test calls since the entry was
    /// created OR since the operator last reset. Lifetime
    /// counter — never decrements automatically.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub failed_request_count: u64,
    /// Total count of successful test calls — same lifetime
    /// semantics as `failed_request_count`. The ratio gives
    /// operators a real health signal.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub success_request_count: u64,
    /// Unix seconds of the most recent failure observed by
    /// a test call. `None` when never failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<i64>,
    /// Upstream HTTP status code of the most recent failure
    /// (when the request reached the provider). None for
    /// transport-layer failures or when never failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_status_code: Option<u16>,
    /// H1: Hermes-style structured failover reason label
    /// (`rate-limit`, `context-overflow`, `auth-rejected`, …)
    /// captured from the most recent failure. Lets operators
    /// scan for "always failing the same way" vs "flapping
    /// across reasons" without parsing free-form bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<String>,
    /// PH-WAVE2G: bounded ring of recent rate-limit
    /// observations. Each entry is the unix-seconds timestamp
    /// of a test call that classified as `rate-limit`. Newest
    /// at the back. Capped at [`RATE_LIMIT_RING_CAP`] so a
    /// long-lived bridge can't grow the entry unboundedly.
    /// Distinct from `last_failure_*` (snapshot of newest
    /// failure regardless of class) — this ring is a
    /// time-decay signal for the specific rate-limit
    /// failure mode. Empty / absent means no rate-limit hits
    /// have been observed since the entry was created.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limit_recent_hits: Vec<i64>,
}

/// PH-WAVE2G: hard cap on per-provider rate-limit ring size.
/// Operators can tell "we got rate-limited 32+ times in our
/// recorded window" from the ring being full at this size.
pub const RATE_LIMIT_RING_CAP: usize = 32;

/// PH-WAVE2I: rate-limit hits within a 5-minute window that
/// trigger the bridge to auto-set a cooldown. Higher than 1 so
/// a single transient flap doesn't slam the cooldown door;
/// low enough that a real rate-limit storm is caught quickly.
pub const ANTI_RATELIMIT_THRESHOLD_5MIN: u64 = 5;

/// PH-WAVE2I: length of the auto-set cooldown, in seconds.
/// Operators can extend / clear via the existing M69
/// quarantine surface; the bridge just refuses test calls
/// until cooldown_until passes.
pub const ANTI_RATELIMIT_COOLDOWN_SECS: i64 = 60;

/// PH-WAVE2G: count rate-limit observations whose timestamp is
/// within `window_secs` of `now`. Saturating arithmetic — a
/// wildly future-stamped entry doesn't wrap. Empty ring returns
/// 0. Public so callers (provider_status, future telemetry)
/// share one definition.
pub fn rate_limit_hits_in_window(hits: &[i64], now: i64, window_secs: i64) -> u64 {
    let lower = now.saturating_sub(window_secs);
    hits.iter().filter(|&&t| t >= lower && t <= now).count() as u64
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramEntry {
    pub bot_token: String,
    /// `polling` or `webhook`. Only `polling` is functional
    /// today; `webhook` persists the URL for future
    /// implementation but the live HTTPS client isn't
    /// wired yet (the channel controller falls back to
    /// polling).
    #[serde(default = "default_telegram_mode")]
    pub mode: String,
    /// Operator-supplied webhook URL. Required when
    /// `mode = "webhook"`; ignored when `mode = "polling"`.
    /// Not a secret — operators can see + edit it via the
    /// dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
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
    /// `true` when this provider is the operator-marked
    /// default. Hint only — see [`BridgeSecrets::default_provider`].
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_default: bool,
    /// Operator-marked enabled flag. Defaults to `true` for
    /// unconfigured providers (no entry → no opinion yet).
    /// Hint only — see `ProviderEntry::enabled`.
    pub enabled: bool,
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
    // ── M58: persistent last-test cache projection ─────────
    /// Unix seconds of the most recent operator-triggered
    /// test against this provider's saved key. `None` when
    /// the provider has never been tested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_at: Option<i64>,
    /// Outcome of the most recent test. `None` when never
    /// tested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_ok: Option<bool>,
    /// Upstream HTTP status code from the most recent test,
    /// when the request reached the provider's server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_status_code: Option<u16>,
    /// Elapsed milliseconds of the most recent test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_elapsed_ms: Option<u64>,
    /// Redaction-safe summary from the most recent test.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_test_detail: Option<String>,
    // ── M69: quarantine + cooldown projection ──────────────
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantined_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<i64>,
    // ── M77: routing-trace counters projection ─────────────
    /// Lifetime count of failed test calls. Zero suppressed
    /// from serialization so unused providers don't surface
    /// a noisy "0" in the dashboard.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub failed_request_count: u64,
    /// Lifetime count of successful test calls.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub success_request_count: u64,
    /// Most recent failure timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_at: Option<i64>,
    /// Most recent failure HTTP status code (when reached).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_status_code: Option<u16>,
    /// H1: Hermes-style failover reason label of the most
    /// recent failure (e.g. `rate-limit`, `context-overflow`).
    /// `None` when the provider has never failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure_reason: Option<String>,
    /// PH-WAVE2G: count of rate-limit observations in the last
    /// 5 minutes (computed from `rate_limit_recent_hits` at
    /// projection time). 0 → no recent hits → field suppressed
    /// from JSON output.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub rate_limit_hits_5min: u64,
    /// PH-WAVE2G: same for the last hour.
    #[serde(skip_serializing_if = "is_zero_u64")]
    pub rate_limit_hits_1h: u64,
    /// PH-WAVE2G: timestamp of the most recent rate-limit
    /// observation, if any. None when the ring is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_rate_limit_at: Option<i64>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
pub struct TelegramStatus {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_preview: Option<String>,
    pub mode: String,
    /// Persisted webhook URL when mode=webhook. Not a
    /// secret; operators see the URL directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
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
        let is_default = self.default_provider.as_deref() == Some(name);
        let now = unix_secs();
        match self.providers.get(name) {
            Some(e) => ProviderStatus {
                name: name.to_string(),
                configured: !e.api_key.is_empty(),
                is_default,
                enabled: e.enabled,
                default_model: e.default_model.clone(),
                key_preview: redact(&e.api_key),
                key_set_at: Some(e.set_at),
                last_test_at: e.last_test_at,
                last_test_ok: e.last_test_ok,
                last_test_status_code: e.last_test_status_code,
                last_test_elapsed_ms: e.last_test_elapsed_ms,
                last_test_detail: e.last_test_detail.clone(),
                quarantined_at: e.quarantined_at,
                quarantine_reason: e.quarantine_reason.clone(),
                cooldown_until: e.cooldown_until,
                failed_request_count: e.failed_request_count,
                success_request_count: e.success_request_count,
                last_failure_at: e.last_failure_at,
                last_failure_status_code: e.last_failure_status_code,
                last_failure_reason: e.last_failure_reason.clone(),
                rate_limit_hits_5min: rate_limit_hits_in_window(
                    &e.rate_limit_recent_hits,
                    now,
                    300,
                ),
                rate_limit_hits_1h: rate_limit_hits_in_window(&e.rate_limit_recent_hits, now, 3600),
                last_rate_limit_at: e.rate_limit_recent_hits.last().copied(),
            },
            None => ProviderStatus {
                name: name.to_string(),
                configured: false,
                is_default,
                enabled: true, // default for unconfigured: no opinion
                default_model: None,
                key_preview: None,
                key_set_at: None,
                last_test_at: None,
                last_test_ok: None,
                last_test_status_code: None,
                last_test_elapsed_ms: None,
                last_test_detail: None,
                quarantined_at: None,
                quarantine_reason: None,
                cooldown_until: None,
                failed_request_count: 0,
                success_request_count: 0,
                last_failure_at: None,
                last_failure_status_code: None,
                last_failure_reason: None,
                rate_limit_hits_5min: 0,
                rate_limit_hits_1h: 0,
                last_rate_limit_at: None,
            },
        }
    }

    /// Set or clear the operator-set quarantine flag on a
    /// provider entry (M69). Returns `false` when the
    /// provider has no entry (operator must set a key
    /// first); `true` on success.
    ///
    /// `cooldown_secs` is optional. When `Some(N)`, the
    /// resulting `cooldown_until` is set to `now + N` and
    /// the test-provider endpoint refuses to run live calls
    /// against this entry until that time passes.
    /// Independent of `quarantine` so operators can:
    /// - quarantine without cooldown (manual review)
    /// - set a cooldown without quarantining (auto-recovery
    ///   window after a known flap)
    /// - both
    ///
    /// `reason` is captured at set time; ignored on clear.
    pub fn set_provider_quarantine(
        &mut self,
        name: &str,
        quarantine: bool,
        reason: Option<String>,
        cooldown_secs: Option<i64>,
    ) -> bool {
        match self.providers.get_mut(name) {
            Some(e) => {
                let now = unix_secs();
                if quarantine {
                    e.quarantined_at = Some(now);
                    e.quarantine_reason = reason
                        .map(|r| r.trim().to_string())
                        .filter(|r| !r.is_empty());
                } else {
                    e.quarantined_at = None;
                    e.quarantine_reason = None;
                }
                e.cooldown_until = cooldown_secs.filter(|s| *s > 0).map(|s| now + s);
                e.set_at = now;
                true
            }
            None => false,
        }
    }

    /// Inspect the active cooldown for a provider name.
    /// Returns `Some(seconds_remaining)` when an active
    /// cooldown is in effect; `None` when no cooldown is
    /// set OR the cooldown has elapsed. Pure read — the
    /// bridge's test-provider handler uses this as a soft
    /// gate.
    pub fn cooldown_remaining_secs(&self, name: &str, now: i64) -> Option<i64> {
        self.providers
            .get(name)
            .and_then(|e| e.cooldown_until)
            .and_then(|until| {
                let rem = until - now;
                if rem > 0 { Some(rem) } else { None }
            })
    }

    /// Set the operator-marked enabled flag on a provider
    /// entry. Returns `false` when the provider has no entry
    /// (operator must set a key first); `true` on success.
    pub fn set_provider_enabled(&mut self, name: &str, enabled: bool) -> bool {
        match self.providers.get_mut(name) {
            Some(e) => {
                e.enabled = enabled;
                e.set_at = unix_secs();
                true
            }
            None => false,
        }
    }

    /// Set / clear the operator-marked default provider. The
    /// caller is responsible for validating `name` (when
    /// `Some(_)`) against [`ALLOWED_PROVIDERS`]; pass `None`
    /// to clear.
    pub fn set_default_provider(&mut self, name: Option<String>) {
        self.default_provider = name;
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
                webhook_url: t.webhook_url.clone(),
                token_set_at: Some(t.set_at),
            },
            None => TelegramStatus {
                configured: false,
                token_preview: None,
                mode: "polling".to_string(),
                webhook_url: None,
                token_set_at: None,
            },
        }
    }

    /// Insert or replace a provider entry. Stamps `set_at`
    /// with the current time. Preserves the operator-marked
    /// `enabled` flag from any prior entry — flipping a key
    /// shouldn't silently re-enable a disabled provider.
    /// Caller is responsible for validating `name` against
    /// [`ALLOWED_PROVIDERS`] + rejecting empty `api_key`.
    pub fn set_provider(&mut self, name: &str, api_key: String, default_model: Option<String>) {
        // Preserve operator-set flags across key overwrites.
        // The key may have changed underneath; the flags
        // (enabled, quarantine, cooldown) and the test cache
        // remain the operator's prior intent. They get cleared
        // explicitly via dedicated endpoints, not by a key swap.
        let prior = self.providers.get(name);
        let prior_enabled = prior.map(|e| e.enabled).unwrap_or(true);
        let prior_last_test_at = prior.and_then(|e| e.last_test_at);
        let prior_last_test_ok = prior.and_then(|e| e.last_test_ok);
        let prior_last_test_status_code = prior.and_then(|e| e.last_test_status_code);
        let prior_last_test_elapsed_ms = prior.and_then(|e| e.last_test_elapsed_ms);
        let prior_last_test_detail = prior.and_then(|e| e.last_test_detail.clone());
        let prior_quarantined_at = prior.and_then(|e| e.quarantined_at);
        let prior_quarantine_reason = prior.and_then(|e| e.quarantine_reason.clone());
        let prior_cooldown_until = prior.and_then(|e| e.cooldown_until);
        let prior_failed_count = prior.map(|e| e.failed_request_count).unwrap_or(0);
        let prior_success_count = prior.map(|e| e.success_request_count).unwrap_or(0);
        let prior_last_failure_at = prior.and_then(|e| e.last_failure_at);
        let prior_last_failure_status_code = prior.and_then(|e| e.last_failure_status_code);
        let prior_last_failure_reason = prior.and_then(|e| e.last_failure_reason.clone());
        let prior_rate_limit_ring = prior
            .map(|e| e.rate_limit_recent_hits.clone())
            .unwrap_or_default();
        self.providers.insert(
            name.to_string(),
            ProviderEntry {
                api_key,
                default_model,
                set_at: unix_secs(),
                enabled: prior_enabled,
                last_test_at: prior_last_test_at,
                last_test_ok: prior_last_test_ok,
                last_test_status_code: prior_last_test_status_code,
                last_test_elapsed_ms: prior_last_test_elapsed_ms,
                last_test_detail: prior_last_test_detail,
                quarantined_at: prior_quarantined_at,
                quarantine_reason: prior_quarantine_reason,
                cooldown_until: prior_cooldown_until,
                failed_request_count: prior_failed_count,
                success_request_count: prior_success_count,
                last_failure_at: prior_last_failure_at,
                last_failure_status_code: prior_last_failure_status_code,
                last_failure_reason: prior_last_failure_reason,
                rate_limit_recent_hits: prior_rate_limit_ring,
            },
        );
    }

    /// Remove a provider entry, if present. Idempotent.
    pub fn delete_provider(&mut self, name: &str) {
        self.providers.remove(name);
    }

    /// Record the outcome of a test-provider call against this
    /// provider's saved key. Stamps `last_test_at` to the
    /// current time. Returns `false` when there's no entry to
    /// stamp (defensive — the test handler validates this
    /// itself before calling).
    ///
    /// `failure_reason` is the H1 [`FailoverReason::label`] when
    /// `ok = false`; ignored on success. Pass `None` when the
    /// caller hasn't classified the failure (e.g. legacy paths
    /// that pre-date the H1 classifier).
    pub fn record_provider_test(
        &mut self,
        name: &str,
        ok: bool,
        status_code: Option<u16>,
        elapsed_ms: u64,
        detail: impl Into<String>,
        failure_reason: Option<&str>,
    ) -> bool {
        match self.providers.get_mut(name) {
            Some(e) => {
                let now = unix_secs();
                e.last_test_at = Some(now);
                e.last_test_ok = Some(ok);
                e.last_test_status_code = status_code;
                e.last_test_elapsed_ms = Some(elapsed_ms);
                e.last_test_detail = Some(detail.into());
                // M77: routing-trace counters. Lifetime
                // counts — never decrement automatically.
                // Saturating add so a long-lived bridge can't
                // wrap a u64 counter.
                if ok {
                    e.success_request_count = e.success_request_count.saturating_add(1);
                } else {
                    e.failed_request_count = e.failed_request_count.saturating_add(1);
                    e.last_failure_at = Some(now);
                    e.last_failure_status_code = status_code;
                    e.last_failure_reason = failure_reason.map(str::to_string);
                    // PH-WAVE2G: append to the rolling rate-limit ring
                    // ONLY for the rate-limit specific failure mode.
                    // Trim from the front when the cap is exceeded —
                    // we want the most recent observations.
                    if matches!(failure_reason, Some("rate-limit")) {
                        e.rate_limit_recent_hits.push(now);
                        if e.rate_limit_recent_hits.len() > RATE_LIMIT_RING_CAP {
                            let drop = e.rate_limit_recent_hits.len() - RATE_LIMIT_RING_CAP;
                            e.rate_limit_recent_hits.drain(..drop);
                        }
                        // PH-WAVE2I: auto-cooldown when the storm
                        // threshold is reached. Don't override an
                        // operator-set cooldown that's already
                        // longer (manual quarantine wins). We
                        // bump cooldown_until to the LATER of
                        // (existing, now+ANTI_RATELIMIT_COOLDOWN_SECS).
                        let recent = rate_limit_hits_in_window(&e.rate_limit_recent_hits, now, 300);
                        if recent >= ANTI_RATELIMIT_THRESHOLD_5MIN {
                            let proposed = now + ANTI_RATELIMIT_COOLDOWN_SECS;
                            e.cooldown_until = Some(
                                e.cooldown_until
                                    .map(|c| c.max(proposed))
                                    .unwrap_or(proposed),
                            );
                        }
                    }
                }
                true
            }
            None => false,
        }
    }

    /// Insert or replace the Telegram entry. Caller is
    /// responsible for validating `mode` against
    /// [`ALLOWED_TELEGRAM_MODES`] + rejecting empty
    /// `bot_token`. Webhook mode is now persisted (URL
    /// included) — the channel controller will still fall
    /// back to polling until the live HTTPS client wiring
    /// lands; the dashboard surfaces this honestly.
    pub fn set_telegram(&mut self, bot_token: String, mode: String, webhook_url: Option<String>) {
        self.telegram = Some(TelegramEntry {
            bot_token,
            mode,
            webhook_url,
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
    fn set_provider_quarantine_round_trips_with_reason_and_cooldown() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        let applied =
            s.set_provider_quarantine("openai", true, Some(" upstream flap ".into()), Some(120));
        assert!(applied);
        let p = s.provider_status("openai");
        assert!(p.quarantined_at.is_some());
        assert_eq!(p.quarantine_reason.as_deref(), Some("upstream flap"));
        assert!(p.cooldown_until.is_some());
    }

    #[test]
    fn set_provider_quarantine_refuses_when_no_entry() {
        let mut s = BridgeSecrets::default();
        let applied = s.set_provider_quarantine("openai", true, None, None);
        assert!(!applied);
    }

    #[test]
    fn cooldown_remaining_secs_is_none_when_elapsed_or_unset() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        // Unset
        assert!(s.cooldown_remaining_secs("openai", 1_700_000_000).is_none());
        // Set, but `now` is past the cooldown
        s.set_provider_quarantine("openai", false, None, Some(60));
        let entry = s.providers.get("openai").unwrap();
        let until = entry.cooldown_until.unwrap();
        assert!(s.cooldown_remaining_secs("openai", until + 1).is_none());
        // Set, `now` is mid-cooldown
        assert!(s.cooldown_remaining_secs("openai", until - 30).is_some());
    }

    #[test]
    fn set_provider_preserves_quarantine_across_key_overwrite() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-old".into(), None);
        s.set_provider_quarantine("openai", true, Some("flap".into()), Some(60));
        s.set_provider("openai", "sk-new".into(), None);
        let p = s.provider_status("openai");
        assert!(p.quarantined_at.is_some());
        assert!(p.cooldown_until.is_some());
        assert_eq!(p.quarantine_reason.as_deref(), Some("flap"));
    }

    // ── PH-WAVE2G: rolling rate-limit observation ring ───────────────

    #[test]
    fn rate_limit_ring_records_only_rate_limit_failures() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        s.record_provider_test("openai", false, Some(500), 80, "boom", Some("server-5xx"));
        s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        let p = s.provider_status("openai");
        // 2 rate-limit hits in the last hour; the server-5xx
        // doesn't count.
        assert_eq!(p.rate_limit_hits_1h, 2);
        assert!(p.last_rate_limit_at.is_some());
    }

    #[test]
    fn rate_limit_window_5min_vs_1h() {
        // Synthesise a provider entry directly so we control
        // the timestamps.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        // Drop two old hits and one recent one into the ring
        // by hand. Provider entry exists from set_provider.
        let now = unix_secs();
        if let Some(e) = s.providers.get_mut("openai") {
            e.rate_limit_recent_hits = vec![now - 7200, now - 3500, now - 60];
        }
        let p = s.provider_status("openai");
        // last 5min: only the now-60 hit.
        assert_eq!(p.rate_limit_hits_5min, 1);
        // last 1h: now-60 + now-3500 (just under 1h).
        assert_eq!(p.rate_limit_hits_1h, 2);
    }

    #[test]
    fn rate_limit_ring_caps_at_max() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        for _ in 0..(RATE_LIMIT_RING_CAP + 10) {
            s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        }
        let entry = s.providers.get("openai").unwrap();
        assert_eq!(
            entry.rate_limit_recent_hits.len(),
            RATE_LIMIT_RING_CAP,
            "ring should cap at RATE_LIMIT_RING_CAP"
        );
    }

    #[test]
    fn rate_limit_ring_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        s.save(&path).expect("save");
        let s2 = BridgeSecrets::load_or_empty(&path);
        let p = s2.provider_status("openai");
        assert_eq!(p.rate_limit_hits_1h, 1);
    }

    #[test]
    fn rate_limit_ring_preserved_across_key_overwrite() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-old".into(), None);
        s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        s.set_provider("openai", "sk-new".into(), Some("gpt-4o".into()));
        let p = s.provider_status("openai");
        assert_eq!(p.rate_limit_hits_1h, 1);
    }

    #[test]
    fn auto_cooldown_triggers_on_storm_threshold() {
        // PH-WAVE2I: 5 rate-limit hits in 5 minutes → auto
        // cooldown is set. Test fires 5 hits in a row; each
        // record_provider_test uses unix_secs() so they all
        // land in the same 5-min window.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        // First 4 hits: no cooldown yet.
        for _ in 0..(ANTI_RATELIMIT_THRESHOLD_5MIN - 1) {
            s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        }
        let p_pre = s.provider_status("openai");
        assert!(
            p_pre.cooldown_until.is_none(),
            "should not yet be in cooldown"
        );
        // 5th hit crosses the threshold.
        s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        let p_post = s.provider_status("openai");
        assert!(p_post.cooldown_until.is_some());
        let now = unix_secs();
        let cd = p_post.cooldown_until.unwrap();
        assert!(
            cd >= now,
            "cooldown_until should be in the future (got {cd}, now {now})"
        );
        assert!(
            cd <= now + ANTI_RATELIMIT_COOLDOWN_SECS + 1,
            "cooldown shouldn't be wildly extended"
        );
    }

    #[test]
    fn auto_cooldown_preserves_longer_operator_cooldown() {
        // Operator set a 1-hour cooldown manually. Auto-cooldown
        // (60s) must not shorten it.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        let now = unix_secs();
        let manual_cd = now + 3600;
        if let Some(e) = s.providers.get_mut("openai") {
            e.cooldown_until = Some(manual_cd);
        }
        // Fire enough to trigger.
        for _ in 0..ANTI_RATELIMIT_THRESHOLD_5MIN {
            s.record_provider_test("openai", false, Some(429), 50, "rate", Some("rate-limit"));
        }
        let p = s.provider_status("openai");
        assert_eq!(
            p.cooldown_until,
            Some(manual_cd),
            "operator-set cooldown must win when longer than auto"
        );
    }

    #[test]
    fn rate_limit_hits_in_window_basic() {
        let now = 1_000_000;
        let hits = vec![now - 7200, now - 600, now - 60];
        assert_eq!(rate_limit_hits_in_window(&hits, now, 300), 1);
        assert_eq!(rate_limit_hits_in_window(&hits, now, 3600), 2);
        assert_eq!(rate_limit_hits_in_window(&hits, now, 0), 0);
        assert_eq!(rate_limit_hits_in_window(&[], now, 3600), 0);
    }

    #[test]
    fn routing_trace_increments_lifetime_counters() {
        // M77: success/fail counters accumulate across many
        // test calls. Real lifetime signal — distinct from
        // the M58 snapshot.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.record_provider_test("openai", true, Some(200), 100, "ok", None);
        s.record_provider_test("openai", true, Some(200), 110, "ok", None);
        s.record_provider_test(
            "openai",
            false,
            Some(429),
            50,
            "rate limited",
            Some("rate-limit"),
        );
        s.record_provider_test(
            "openai",
            false,
            Some(500),
            80,
            "upstream 500",
            Some("server-5xx"),
        );
        s.record_provider_test("openai", true, Some(200), 120, "ok", None);
        let p = s.provider_status("openai");
        assert_eq!(p.success_request_count, 3);
        assert_eq!(p.failed_request_count, 2);
        assert!(p.last_failure_at.is_some());
        assert_eq!(p.last_failure_status_code, Some(500));
        // H1: most-recent failure reason label is what was passed in.
        assert_eq!(p.last_failure_reason.as_deref(), Some("server-5xx"));
    }

    #[test]
    fn routing_trace_counters_preserved_across_key_overwrite() {
        // M77: rotating a key must not zero the lifetime
        // counters — operators care about reliability over
        // the LIFE of the provider entry, not the LIFE of
        // any single API key.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-old".into(), None);
        s.record_provider_test("openai", false, Some(500), 100, "boom", Some("server-5xx"));
        s.record_provider_test("openai", true, Some(200), 90, "ok", None);
        s.set_provider("openai", "sk-new".into(), Some("gpt-4o".into()));
        let p = s.provider_status("openai");
        assert_eq!(p.failed_request_count, 1);
        assert_eq!(p.success_request_count, 1);
        assert!(p.last_failure_at.is_some());
        // H1: failure reason survives the key overwrite.
        assert_eq!(p.last_failure_reason.as_deref(), Some("server-5xx"));
    }

    #[test]
    fn routing_trace_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.record_provider_test(
            "openai",
            false,
            Some(429),
            88,
            "rate limited",
            Some("rate-limit"),
        );
        s.save(&path).expect("save");
        let s2 = BridgeSecrets::load_or_empty(&path);
        let p = s2.provider_status("openai");
        assert_eq!(p.failed_request_count, 1);
        assert_eq!(p.success_request_count, 0);
        assert_eq!(p.last_failure_status_code, Some(429));
        assert_eq!(p.last_failure_reason.as_deref(), Some("rate-limit"));
    }

    #[test]
    fn record_provider_test_writes_cache_fields() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        let p0 = s.provider_status("openai");
        assert!(p0.last_test_at.is_none());

        let applied = s.record_provider_test("openai", true, Some(200), 123, "ok", None);
        assert!(applied);
        let p1 = s.provider_status("openai");
        assert_eq!(p1.last_test_ok, Some(true));
        assert_eq!(p1.last_test_status_code, Some(200));
        assert_eq!(p1.last_test_elapsed_ms, Some(123));
        assert_eq!(p1.last_test_detail.as_deref(), Some("ok"));
        assert!(p1.last_test_at.is_some());
    }

    #[test]
    fn record_provider_test_refuses_when_no_entry() {
        let mut s = BridgeSecrets::default();
        // No prior set_provider — the entry doesn't exist, so
        // recording a test against it must noop.
        let applied = s.record_provider_test("openai", true, None, 0, "noop", None);
        assert!(!applied);
        let p = s.provider_status("openai");
        assert!(p.last_test_at.is_none());
    }

    #[test]
    fn set_provider_preserves_last_test_cache_across_key_overwrite() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-old".into(), Some("gpt-4o".into()));
        s.record_provider_test("openai", true, Some(200), 42, "ok ({200})", None);
        // Overwrite the key. The cache must survive — operators
        // can tell the cache is stale by comparing
        // `last_test_at` with `key_set_at`.
        s.set_provider("openai", "sk-new".into(), Some("gpt-4o".into()));
        let p = s.provider_status("openai");
        assert_eq!(p.last_test_ok, Some(true));
        assert_eq!(p.last_test_status_code, Some(200));
        assert_eq!(p.last_test_elapsed_ms, Some(42));
        // `key_set_at` was bumped by the second set_provider;
        // both timestamps are still present so the dashboard
        // can compare them.
        assert!(p.key_set_at.is_some());
        assert!(p.last_test_at.is_some());
    }

    #[test]
    fn provider_test_cache_round_trips_through_disk() {
        // Persisted cache survives a save + reload cycle. That's
        // the whole point of M58 — operators see the badge
        // after a bridge restart.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        s.record_provider_test(
            "openai",
            false,
            Some(401),
            88,
            "upstream returned 401",
            Some("auth-rejected"),
        );
        s.save(&path).expect("save");
        let s2 = BridgeSecrets::load_or_empty(&path);
        let p = s2.provider_status("openai");
        assert_eq!(p.last_test_ok, Some(false));
        assert_eq!(p.last_test_status_code, Some(401));
        assert_eq!(p.last_test_elapsed_ms, Some(88));
        assert_eq!(p.last_test_detail.as_deref(), Some("upstream returned 401"));
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
        s.set_telegram("1234567:ABCDEFghijklmnop".into(), "polling".into(), None);
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
    fn telegram_status_round_trips_webhook_url() {
        let mut s = BridgeSecrets::default();
        s.set_telegram(
            "1234:abcdef".into(),
            "webhook".into(),
            Some("https://relix.example.com/tg-hook".into()),
        );
        let t = s.telegram_status();
        assert_eq!(t.mode, "webhook");
        assert_eq!(
            t.webhook_url.as_deref(),
            Some("https://relix.example.com/tg-hook")
        );
        // Round-trip through disk + reload.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        s.save(&path).unwrap();
        let r = BridgeSecrets::load_or_empty(&path);
        let t2 = r.telegram_status();
        assert_eq!(t2.mode, "webhook");
        assert_eq!(
            t2.webhook_url.as_deref(),
            Some("https://relix.example.com/tg-hook")
        );
    }

    #[test]
    fn default_provider_marker_round_trips() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        // Initially no default.
        assert!(s.default_provider.is_none());
        assert!(!s.provider_status("openai").is_default);
        // Mark openai as default.
        s.set_default_provider(Some("openai".into()));
        assert!(s.provider_status("openai").is_default);
        assert!(!s.provider_status("anthropic").is_default);
        // Clearing.
        s.set_default_provider(None);
        assert!(!s.provider_status("openai").is_default);
    }

    #[test]
    fn set_provider_enabled_returns_false_for_unconfigured() {
        let mut s = BridgeSecrets::default();
        assert!(!s.set_provider_enabled("openai", false));
    }

    #[test]
    fn set_provider_enabled_round_trips_through_status() {
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-x".into(), None);
        // Default is true for newly-set providers.
        assert!(s.provider_status("openai").enabled);
        // Disable.
        assert!(s.set_provider_enabled("openai", false));
        assert!(!s.provider_status("openai").enabled);
        // Re-enable.
        assert!(s.set_provider_enabled("openai", true));
        assert!(s.provider_status("openai").enabled);
    }

    #[test]
    fn set_provider_preserves_enabled_when_overwriting_key() {
        // Operator's enabled=false intent must survive a
        // key rotation — set_provider() shouldn't silently
        // re-enable a disabled provider.
        let mut s = BridgeSecrets::default();
        s.set_provider("openai", "sk-old".into(), None);
        s.set_provider_enabled("openai", false);
        s.set_provider("openai", "sk-new".into(), None);
        assert!(!s.provider_status("openai").enabled);
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
        s.set_telegram("1234:abcdef".into(), "polling".into(), None);
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
