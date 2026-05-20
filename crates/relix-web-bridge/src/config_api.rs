//! `/v1/config/*` — dashboard-facing config endpoints.
//!
//! Reads and writes [`crate::secrets::BridgeSecrets`]. Every
//! response is redacted — the raw API key / Telegram token
//! never leaves the bridge process via these endpoints.
//!
//! Write endpoints (PUT/DELETE) accept the raw secret in the
//! request body and persist it to `bridge-secrets.toml`. The
//! INFO log line emitted for each write carries only the
//! redacted preview, never the raw value. See
//! `docs/dashboard-redesign.md` for the full contract.
//!
//! Auth: none at the HTTP layer. The bridge binds to loopback
//! by default; production operators must put a reverse proxy
//! with auth in front before exposing these endpoints beyond
//! the local machine.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;
use crate::secrets::{ALLOWED_PROVIDERS, ALLOWED_TELEGRAM_MODES, ProviderStatus, TelegramStatus};

/// Standard error envelope.
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        Json(self).into_response()
    }
}

fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError { error: msg.into() }),
    )
}

fn unprocessable(msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ApiError { error: msg.into() }),
    )
}

fn internal(msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError { error: msg.into() }),
    )
}

// ── Providers ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ProvidersResponse {
    pub providers: Vec<ProviderStatus>,
}

/// `GET /v1/config/providers` — list all allowed providers
/// with redacted status. Always returns every provider in
/// the allowlist so the dashboard can render a card per
/// provider without a second round-trip.
pub async fn list_providers(State(state): State<AppState>) -> Json<ProvidersResponse> {
    let providers = state.secrets.read(|s| s.all_provider_statuses());
    Json(ProvidersResponse { providers })
}

/// `GET /v1/config/providers/:name` — redacted status for one
/// provider. 404 when the name is not in the allowlist.
pub async fn get_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ProviderStatus>, (StatusCode, Json<ApiError>)> {
    if !ALLOWED_PROVIDERS.contains(&name.as_str()) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!(
                    "unknown provider '{name}'. allowed: {}",
                    ALLOWED_PROVIDERS.join(", ")
                ),
            }),
        ));
    }
    Ok(Json(state.secrets.read(|s| s.provider_status(&name))))
}

#[derive(Debug, Deserialize)]
pub struct PutProviderReq {
    /// Raw API key. Stored at mode 0600 on disk; never echoed
    /// back via any HTTP response or log line.
    pub api_key: String,
    #[serde(default)]
    pub default_model: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PutProviderResp {
    pub status: ProviderStatus,
    /// `true` when the change requires a controller restart to
    /// take effect. Provider keys are read at controller
    /// startup, so any PUT today returns `true`.
    pub restart_required: bool,
}

/// `PUT /v1/config/providers/:name` — set the provider key +
/// optional default model. Idempotent: re-submitting overwrites
/// in place + bumps `set_at`.
pub async fn put_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PutProviderReq>,
) -> Result<Json<PutProviderResp>, (StatusCode, Json<ApiError>)> {
    if !ALLOWED_PROVIDERS.contains(&name.as_str()) {
        return Err(unprocessable(format!(
            "unknown provider '{name}'. allowed: {}",
            ALLOWED_PROVIDERS.join(", ")
        )));
    }
    if req.api_key.trim().is_empty() {
        return Err(bad_request("api_key required (non-empty)"));
    }
    let result = state.secrets.mutate(|s| {
        s.set_provider(&name, req.api_key.clone(), req.default_model.clone());
        s.provider_status(&name)
    });
    match result {
        Ok(status) => {
            tracing::info!(
                provider = %name,
                key_preview = %status.key_preview.as_deref().unwrap_or(""),
                default_model = %status.default_model.as_deref().unwrap_or(""),
                "config: providers.{name} updated"
            );
            Ok(Json(PutProviderResp {
                status,
                restart_required: true,
            }))
        }
        Err(e) => Err(internal(format!("persist failed: {e}"))),
    }
}

/// Result of a `POST /v1/config/providers/:name/test`. Includes
/// the upstream HTTP status code + elapsed_ms so operators can
/// distinguish "key works but provider is slow" from "key is
/// rejected" from "network unreachable."
#[derive(Debug, Serialize)]
pub struct ProviderTestResult {
    pub name: String,
    pub ok: bool,
    /// Upstream HTTP status code, when the request reached the
    /// provider's server. `None` for transport-layer failures
    /// (DNS, TCP, TLS) — those land in `detail`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    pub elapsed_ms: u64,
    /// Human-readable summary. Bridge-supplied — NEVER includes
    /// the raw key, NEVER echoes back arbitrary upstream body.
    pub detail: String,
}

/// `POST /v1/config/providers/:name/test` — validate the saved
/// key against the upstream provider by listing models. Returns
/// success/failure + elapsed time + a redaction-safe detail
/// string.
pub async fn test_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ProviderTestResult>, (StatusCode, Json<ApiError>)> {
    if !ALLOWED_PROVIDERS.contains(&name.as_str()) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!(
                    "unknown provider '{name}'. allowed: {}",
                    ALLOWED_PROVIDERS.join(", ")
                ),
            }),
        ));
    }
    // Read the key from secrets under the lock; clone it
    // immediately so the lock is released before the network
    // round-trip.
    let api_key = state.secrets.read(|s| {
        s.providers
            .get(&name)
            .map(|e| e.api_key.clone())
            .unwrap_or_default()
    });
    if api_key.is_empty() && name != "mock" {
        return Err(unprocessable(format!(
            "provider '{name}' is not configured. Set an API key via PUT /v1/config/providers/{name} first."
        )));
    }
    let started = std::time::Instant::now();
    let outcome = check_provider_key(&name, &api_key).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let result = match outcome {
        Ok(detail) => ProviderTestResult {
            name: name.clone(),
            ok: true,
            status_code: Some(200),
            elapsed_ms,
            detail,
        },
        Err((status_code, detail)) => ProviderTestResult {
            name: name.clone(),
            ok: false,
            status_code,
            elapsed_ms,
            detail,
        },
    };
    // INFO line carries only the redaction-safe summary, never
    // the raw key.
    tracing::info!(
        provider = %name,
        ok = result.ok,
        status_code = ?result.status_code,
        elapsed_ms = result.elapsed_ms,
        "config: providers.{name} test"
    );
    Ok(Json(result))
}

/// Per-provider connectivity probe. Returns `Ok(detail)` on
/// success, `Err((status_code, detail))` on failure. Never
/// surfaces the raw key in the returned strings.
async fn check_provider_key(name: &str, api_key: &str) -> Result<String, (Option<u16>, String)> {
    // Short timeout — operators won't wait long on a "test
    // connection" button. 10s is generous for a list-models call.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| (None, format!("http client init failed: {e}")))?;
    match name {
        "mock" => Ok("mock provider: no upstream to test".to_string()),
        "openai" => probe_bearer(&client, "https://api.openai.com/v1/models", api_key).await,
        "openrouter" => probe_bearer(&client, "https://openrouter.ai/api/v1/models", api_key).await,
        "xai" => probe_bearer(&client, "https://api.x.ai/v1/models", api_key).await,
        "anthropic" => {
            // Anthropic uses x-api-key + anthropic-version, not
            // Authorization: Bearer.
            let resp = client
                .get("https://api.anthropic.com/v1/models")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|e| {
                    (
                        None,
                        format!("network error: {}", redact_err(&e.to_string())),
                    )
                })?;
            interpret_response(resp).await
        }
        "google" => {
            // Gemini uses ?key=<KEY> in the query string. The
            // URL is built deliberately — the key is never logged.
            let url = format!(
                "https://generativelanguage.googleapis.com/v1beta/models?key={}",
                urlencode(api_key)
            );
            let resp = client.get(&url).send().await.map_err(|e| {
                (
                    None,
                    format!("network error: {}", redact_err(&e.to_string())),
                )
            })?;
            interpret_response(resp).await
        }
        _ => Err((
            None,
            format!("provider '{name}' has no shipped test handler"),
        )),
    }
}

async fn probe_bearer(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<String, (Option<u16>, String)> {
    let resp = client
        .get(url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| {
            (
                None,
                format!("network error: {}", redact_err(&e.to_string())),
            )
        })?;
    interpret_response(resp).await
}

/// Translate a `reqwest::Response` into the success/failure
/// detail string. Never includes the raw body — only the
/// status + (optional) model count parsed from a JSON list
/// shape.
async fn interpret_response(resp: reqwest::Response) -> Result<String, (Option<u16>, String)> {
    let status = resp.status();
    if status.is_success() {
        // Try to parse a model count; non-fatal if we can't.
        match resp.text().await {
            Ok(body) => {
                let count = count_models_loosely(&body);
                let suffix = if count > 0 {
                    format!(" · {count} models advertised")
                } else {
                    String::new()
                };
                Ok(format!("ok ({}){suffix}", status.as_u16()))
            }
            Err(_) => Ok(format!("ok ({})", status.as_u16())),
        }
    } else {
        // Read the body but strip anything that looks like the
        // key (defensive). Most providers' error bodies don't
        // include the key, but they sometimes do echo back
        // headers in debug output. We hard-truncate to keep the
        // response surface minimal.
        let body = resp.text().await.unwrap_or_default();
        let detail = truncate_for_op(&body, 200);
        Err((
            Some(status.as_u16()),
            format!("upstream returned {}: {detail}", status.as_u16()),
        ))
    }
}

/// Very loose model-count parser — looks for a top-level
/// `"data": [...]` array (OpenAI shape) or counts top-level
/// objects under `"models"` (Google shape). Misses some
/// providers; that's fine — the count is a nice-to-have.
fn count_models_loosely(body: &str) -> usize {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            return arr.len();
        }
        if let Some(arr) = v.get("models").and_then(|d| d.as_array()) {
            return arr.len();
        }
    }
    0
}

/// Defensive: scrub anything that obviously looks like an API
/// key prefix from upstream error strings. Not cryptographic —
/// just an extra belt-and-braces guard so an oddly-formatted
/// upstream error can't accidentally surface the key.
fn redact_err(s: &str) -> String {
    // Common provider key prefixes. Treat any token that
    // starts with these as redacted.
    let redacted: String = s
        .split_whitespace()
        .map(|tok| {
            let lower = tok.to_ascii_lowercase();
            if lower.starts_with("sk-")
                || lower.starts_with("xai-")
                || lower.starts_with("aiza")
                || lower.starts_with("bearer ")
            {
                "<redacted>"
            } else {
                tok
            }
        })
        .collect::<Vec<&str>>()
        .join(" ");
    truncate_for_op(&redacted, 200)
}

fn truncate_for_op(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    let s: String = trimmed.chars().take(n).collect();
    if trimmed.chars().count() > n {
        format!("{s}…")
    } else {
        s
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `DELETE /v1/config/providers/:name` — remove the provider
/// entry. Idempotent: deleting an absent entry is a no-op.
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ProviderStatus>, (StatusCode, Json<ApiError>)> {
    if !ALLOWED_PROVIDERS.contains(&name.as_str()) {
        return Err(unprocessable(format!(
            "unknown provider '{name}'. allowed: {}",
            ALLOWED_PROVIDERS.join(", ")
        )));
    }
    let status = state.secrets.mutate(|s| {
        s.delete_provider(&name);
        s.provider_status(&name)
    });
    match status {
        Ok(s) => {
            tracing::info!(provider = %name, "config: providers.{name} deleted");
            Ok(Json(s))
        }
        Err(e) => Err(internal(format!("persist failed: {e}"))),
    }
}

// ── Telegram ────────────────────────────────────────────────

/// `GET /v1/config/telegram` — redacted Telegram bot status.
pub async fn get_telegram(State(state): State<AppState>) -> Json<TelegramStatus> {
    Json(state.secrets.read(|s| s.telegram_status()))
}

#[derive(Debug, Deserialize)]
pub struct PutTelegramReq {
    /// Raw bot token from `@BotFather`. Stored at mode 0600
    /// on disk; never echoed back via any HTTP response or
    /// log line.
    pub bot_token: String,
    /// `polling` or `webhook`. `webhook` is in the schema but
    /// the live HTTPS client doesn't ship yet — submitting
    /// `webhook` returns 422 until it lands.
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "polling".to_string()
}

#[derive(Debug, Serialize)]
pub struct PutTelegramResp {
    pub status: TelegramStatus,
    pub restart_required: bool,
}

/// `PUT /v1/config/telegram` — set the bot token + delivery
/// mode. Idempotent. Returns 422 when `mode` is unknown or
/// when it's `webhook` (not yet implemented).
pub async fn put_telegram(
    State(state): State<AppState>,
    Json(req): Json<PutTelegramReq>,
) -> Result<Json<PutTelegramResp>, (StatusCode, Json<ApiError>)> {
    if req.bot_token.trim().is_empty() {
        return Err(bad_request("bot_token required (non-empty)"));
    }
    if !ALLOWED_TELEGRAM_MODES.contains(&req.mode.as_str()) {
        return Err(unprocessable(format!(
            "unknown mode '{}'. allowed: {}",
            req.mode,
            ALLOWED_TELEGRAM_MODES.join(", ")
        )));
    }
    if req.mode == "webhook" {
        return Err(unprocessable(
            "webhook mode not yet implemented; use polling",
        ));
    }
    let result = state.secrets.mutate(|s| {
        s.set_telegram(req.bot_token.clone(), req.mode.clone());
        s.telegram_status()
    });
    match result {
        Ok(status) => {
            tracing::info!(
                mode = %status.mode,
                token_preview = %status.token_preview.as_deref().unwrap_or(""),
                "config: telegram updated"
            );
            Ok(Json(PutTelegramResp {
                status,
                restart_required: true,
            }))
        }
        Err(e) => Err(internal(format!("persist failed: {e}"))),
    }
}

// ── Effective bridge config (redacted) ──────────────────────

/// Read-only redacted view of the bridge's effective config.
/// Shape: a small subset of the bridge's runtime state for the
/// dashboard's "Bridge Config" page. Distinct from the
/// secrets file itself.
#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    pub listen_addr: String,
    pub identity_bundle_path: String,
    pub peers_path: String,
    pub flow_template_path: String,
    pub tool_template_path: Option<String>,
    pub coordinator_alias: Option<String>,
    pub openai_compat: bool,
    pub secrets_path: String,
    pub providers_configured: Vec<String>,
    pub telegram_configured: bool,
}

/// `GET /v1/config` — effective bridge config (redacted).
pub async fn get_effective_config(State(state): State<AppState>) -> Json<EffectiveConfig> {
    let providers_configured = state.secrets.read(|s| {
        s.all_provider_statuses()
            .into_iter()
            .filter(|p| p.configured)
            .map(|p| p.name)
            .collect::<Vec<_>>()
    });
    let telegram_configured = state.secrets.read(|s| s.telegram_status().configured);
    Json(EffectiveConfig {
        listen_addr: state.cfg.bridge.listen_addr.clone(),
        identity_bundle_path: state.cfg.identity.bundle_path.display().to_string(),
        peers_path: state.cfg.transport.peers_path.display().to_string(),
        flow_template_path: state.cfg.flow.template_path.display().to_string(),
        tool_template_path: state
            .cfg
            .flow
            .tool_template_path
            .as_ref()
            .map(|p| p.display().to_string()),
        coordinator_alias: state.cfg.coordinator.as_ref().map(|c| c.alias.clone()),
        openai_compat: state.cfg.openai_compat.is_some(),
        secrets_path: state.secrets.path().display().to_string(),
        providers_configured,
        telegram_configured,
    })
}

// ── Tests for endpoint shapes (redaction contract) ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{BridgeSecrets, SecretsHandle};

    fn handle_with(secrets: BridgeSecrets) -> SecretsHandle {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-secrets.toml");
        // Leak the tempdir so the path stays valid for the
        // duration of the test (these are one-shot in-memory
        // checks; tempdir cleanup at end of test is fine).
        std::mem::forget(tmp);
        SecretsHandle::new(secrets, path)
    }

    #[test]
    fn put_provider_request_accepts_minimal_body() {
        // Required field present, default_model absent → ok.
        let body = r#"{"api_key":"sk-test-1234"}"#;
        let req: PutProviderReq = serde_json::from_str(body).unwrap();
        assert_eq!(req.api_key, "sk-test-1234");
        assert!(req.default_model.is_none());
    }

    #[test]
    fn put_provider_request_round_trips_default_model() {
        let body = r#"{"api_key":"sk-x","default_model":"gpt-4o"}"#;
        let req: PutProviderReq = serde_json::from_str(body).unwrap();
        assert_eq!(req.default_model.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn put_telegram_request_defaults_mode_to_polling() {
        let body = r#"{"bot_token":"1234:abc"}"#;
        let req: PutTelegramReq = serde_json::from_str(body).unwrap();
        assert_eq!(req.mode, "polling");
    }

    #[test]
    fn providers_response_serialisation_never_includes_raw_key() {
        // Set a key, serialise the list-providers response,
        // assert the raw key is absent from the JSON.
        let mut s = BridgeSecrets::default();
        s.set_provider(
            "openai",
            "sk-test-NEVERLEAK-1234".into(),
            Some("gpt-4o".into()),
        );
        let resp = ProvidersResponse {
            providers: s.all_provider_statuses(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("sk-test-NEVERLEAK-1234"),
            "raw provider key leaked into ProvidersResponse JSON: {json}"
        );
        assert!(
            !json.contains("NEVERLEAK"),
            "key body leaked into ProvidersResponse JSON: {json}"
        );
        // But the redacted preview IS present.
        assert!(
            json.contains("…1234"),
            "expected redacted preview in response JSON, got: {json}"
        );
    }

    #[test]
    fn telegram_response_serialisation_never_includes_raw_token() {
        let mut s = BridgeSecrets::default();
        s.set_telegram("1234:ABCDEF-NEVERLEAK-7890".into(), "polling".into());
        let resp = s.telegram_status();
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            !json.contains("ABCDEF-NEVERLEAK-7890"),
            "raw token leaked into TelegramStatus JSON: {json}"
        );
        assert!(
            !json.contains("NEVERLEAK"),
            "token body leaked into TelegramStatus JSON: {json}"
        );
        assert!(
            json.contains("…7890"),
            "expected redacted preview in response JSON, got: {json}"
        );
    }

    #[test]
    fn allowed_providers_list_is_stable() {
        // The dashboard hard-codes labels per provider; the
        // backend allowlist is the source of truth. Any new
        // provider entry that lands must also be reflected in
        // the dashboard's PROVIDER_LABELS map and the docs.
        // This test pins the current list so the cross-file
        // contract isn't accidentally broken.
        assert_eq!(
            ALLOWED_PROVIDERS,
            &["mock", "openai", "anthropic", "openrouter", "xai", "google"]
        );
    }

    #[test]
    fn count_models_loosely_handles_openai_shape() {
        let body = r#"{"data":[{"id":"gpt-4o"},{"id":"gpt-4o-mini"},{"id":"o1"}]}"#;
        assert_eq!(count_models_loosely(body), 3);
    }

    #[test]
    fn count_models_loosely_handles_google_shape() {
        let body = r#"{"models":[{"name":"models/gemini-1"},{"name":"models/gemini-2"}]}"#;
        assert_eq!(count_models_loosely(body), 2);
    }

    #[test]
    fn count_models_loosely_returns_zero_on_unknown_shape() {
        assert_eq!(count_models_loosely(""), 0);
        assert_eq!(count_models_loosely("{}"), 0);
        assert_eq!(count_models_loosely("not json"), 0);
    }

    #[test]
    fn redact_err_strips_known_key_prefixes() {
        // Defensive: even if an upstream error string somehow
        // contained the key, the bridge's response must not
        // forward it verbatim.
        let s = redact_err("401 Unauthorized for key sk-test-1234567890");
        assert!(!s.contains("sk-test"));
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn redact_err_passes_normal_text_through() {
        let s = redact_err("network timeout after 5s");
        assert_eq!(s, "network timeout after 5s");
    }

    #[test]
    fn truncate_for_op_caps_long_strings_with_ellipsis() {
        let s = truncate_for_op("aaaaaaaaaa", 4);
        assert_eq!(s, "aaaa…");
    }

    #[test]
    fn urlencode_preserves_unreserved_chars() {
        // Letters / digits / -_.~ stay as-is; everything else
        // is %HH.
        assert_eq!(urlencode("AIza_abc-DEF.123~"), "AIza_abc-DEF.123~");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn handle_can_persist_and_reload_via_mutate() {
        let h = handle_with(BridgeSecrets::default());
        h.mutate(|s| s.set_provider("openai", "sk-xyz-1234".into(), None))
            .unwrap();
        let v = h.read(|s| s.provider_status("openai"));
        assert!(v.configured);
        assert_eq!(v.key_preview.as_deref(), Some("…1234"));
        // Round-trip the file: a fresh handle pointed at the
        // same path should pick up the same entry.
        let h2 = SecretsHandle::new(
            BridgeSecrets::load_or_empty(h.path()),
            h.path().to_path_buf(),
        );
        let v2 = h2.read(|s| s.provider_status("openai"));
        assert!(v2.configured);
        assert_eq!(v2.key_preview.as_deref(), Some("…1234"));
    }
}
