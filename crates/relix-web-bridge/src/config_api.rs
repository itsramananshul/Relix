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
use crate::intervention_audit::new_correlation_id;
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
    let corr = new_correlation_id();
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
            state.intervention_audit.record_with_id(
                "anon",
                "provider_save",
                &name,
                "ok",
                format!(
                    "key …{} · default_model={}",
                    status.key_preview.as_deref().unwrap_or("none"),
                    status.default_model.as_deref().unwrap_or("(none)")
                ),
                corr,
            );
            Ok(Json(PutProviderResp {
                status,
                restart_required: true,
            }))
        }
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "provider_save",
                &name,
                "error",
                format!("persist failed: {e}"),
                corr,
            );
            Err(internal(format!("persist failed: {e}")))
        }
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
    let corr = new_correlation_id();
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
    // M58: persist the test outcome so operators see a
    // last-tested badge on the provider card without re-running.
    // Best-effort — a persist failure logs at warn but the
    // live response still ships, so the operator isn't blocked.
    let cache_persist = state.secrets.mutate(|s| {
        s.record_provider_test(
            &name,
            result.ok,
            result.status_code,
            result.elapsed_ms,
            result.detail.clone(),
        )
    });
    if let Err(e) = cache_persist {
        tracing::warn!(
            provider = %name,
            error = %e,
            "config: providers.{name} test cache persist failed"
        );
    }
    state.intervention_audit.record_with_id(
        "anon",
        "provider_test",
        &name,
        if result.ok { "ok" } else { "error" },
        format!(
            "{}ms{}{}",
            result.elapsed_ms,
            result
                .status_code
                .map(|c| format!(" · HTTP {c}"))
                .unwrap_or_default(),
            if result.ok {
                String::new()
            } else {
                format!(" · {}", result.detail)
            }
        ),
        corr,
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

#[derive(Debug, Deserialize)]
pub struct PutEnabledReq {
    pub enabled: bool,
}

/// `PUT /v1/config/providers/:name/enabled` — flip the
/// operator-marked enabled flag. Hint only — the AI
/// controller reads its provider config from its own TOML
/// at startup, so flipping this here does NOT switch the
/// live runtime. Returns the new ProviderStatus.
pub async fn set_provider_enabled(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PutEnabledReq>,
) -> Result<Json<PutProviderResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    if !ALLOWED_PROVIDERS.contains(&name.as_str()) {
        return Err(unprocessable(format!(
            "unknown provider '{name}'. allowed: {}",
            ALLOWED_PROVIDERS.join(", ")
        )));
    }
    let result = state.secrets.mutate(|s| {
        let applied = s.set_provider_enabled(&name, req.enabled);
        (applied, s.provider_status(&name))
    });
    match result {
        Ok((applied, status)) => {
            if !applied {
                state.intervention_audit.record_with_id(
                    "anon",
                    "provider_enabled_set",
                    &name,
                    "refused",
                    "no entry yet — set api_key first",
                    corr,
                );
                return Err(unprocessable(format!(
                    "provider '{name}' has no entry; set an api_key first via PUT /v1/config/providers/{name}"
                )));
            }
            tracing::info!(
                provider = %name,
                enabled = req.enabled,
                "config: providers.{name} enabled flag updated"
            );
            state.intervention_audit.record_with_id(
                "anon",
                "provider_enabled_set",
                &name,
                "ok",
                if req.enabled { "enabled" } else { "disabled" },
                corr,
            );
            Ok(Json(PutProviderResp {
                status,
                restart_required: true,
            }))
        }
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "provider_enabled_set",
                &name,
                "error",
                format!("persist failed: {e}"),
                corr,
            );
            Err(internal(format!("persist failed: {e}")))
        }
    }
}

/// `DELETE /v1/config/providers/:name` — remove the provider
/// entry. Idempotent: deleting an absent entry is a no-op.
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<ProviderStatus>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
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
            state.intervention_audit.record_with_id(
                "anon",
                "provider_delete",
                &name,
                "ok",
                "",
                corr,
            );
            Ok(Json(s))
        }
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "provider_delete",
                &name,
                "error",
                format!("persist failed: {e}"),
                corr,
            );
            Err(internal(format!("persist failed: {e}")))
        }
    }
}

// ── Default provider marker ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PutDefaultProviderReq {
    /// One of [`crate::secrets::ALLOWED_PROVIDERS`], or `null`
    /// to clear the marker.
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DefaultProviderResp {
    /// `Some(name)` when a default is set, `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Always `true` — the AI controller reads its provider
    /// config from a separate TOML at startup, so changing the
    /// dashboard default is a hint, not a runtime switch.
    /// Operators must update their AI controller config + restart
    /// for the change to take effect.
    pub restart_required: bool,
}

/// `PUT /v1/config/providers/default` — set or clear the
/// operator-marked default provider. Hint-only metadata; the
/// AI controller still reads its provider config from its own
/// TOML. Body: `{ "name": "openai" }` or `{ "name": null }`
/// to clear.
pub async fn put_default_provider(
    State(state): State<AppState>,
    Json(req): Json<PutDefaultProviderReq>,
) -> Result<Json<DefaultProviderResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    if let Some(name) = req.name.as_ref()
        && !ALLOWED_PROVIDERS.contains(&name.as_str())
    {
        return Err(unprocessable(format!(
            "unknown provider '{name}'. allowed: {}",
            ALLOWED_PROVIDERS.join(", ")
        )));
    }
    let result = state.secrets.mutate(|s| {
        s.set_default_provider(req.name.clone());
        s.default_provider.clone()
    });
    match result {
        Ok(current) => {
            tracing::info!(
                default_provider = ?current,
                "config: default provider updated"
            );
            let target = current.clone().unwrap_or_else(|| "(cleared)".to_string());
            state.intervention_audit.record_with_id(
                "anon",
                "provider_make_default",
                target,
                "ok",
                "",
                corr,
            );
            Ok(Json(DefaultProviderResp {
                default_provider: current,
                restart_required: true,
            }))
        }
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "provider_make_default",
                req.name
                    .clone()
                    .unwrap_or_else(|| "(unspecified)".to_string()),
                "error",
                format!("persist failed: {e}"),
                corr,
            );
            Err(internal(format!("persist failed: {e}")))
        }
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
    /// `polling` or `webhook`. Webhook mode is persisted +
    /// the URL is stored — but the live HTTPS client wiring
    /// is pending, so the channel controller will still
    /// fall back to polling until that ships. The response
    /// `note` says so honestly.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Webhook URL; required when mode=webhook. URL is not
    /// a secret (not redacted in responses).
    #[serde(default)]
    pub webhook_url: Option<String>,
}

fn default_mode() -> String {
    "polling".to_string()
}

#[derive(Debug, Serialize)]
pub struct PutTelegramResp {
    pub status: TelegramStatus,
    pub restart_required: bool,
    /// Honest pending-implementation note when relevant
    /// (e.g. webhook mode persisted but live client not
    /// wired). None when everything's immediately
    /// actionable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Result of a `POST /v1/config/telegram/test`. Same shape
/// as [`ProviderTestResult`] plus an optional `bot_username`
/// when Telegram returned a usable identity.
#[derive(Debug, Serialize)]
pub struct TelegramTestResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    pub elapsed_ms: u64,
    pub detail: String,
    /// When the probe succeeds and Telegram returns a `result.username`
    /// in the getMe response, the bridge surfaces it here so operators
    /// can verify they wired the right bot. `None` on failure or
    /// when the response shape is unexpected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_username: Option<String>,
}

/// `POST /v1/config/telegram/test` — validate the saved bot
/// token by calling Telegram's `getMe`. Returns success/failure,
/// optional bot_username, elapsed_ms, and a redaction-safe
/// detail string.
///
/// SECURITY NOTE: the Bot API requires the token in the URL
/// path (`/bot<TOKEN>/getMe`). The constructed URL never reaches
/// any tracing event; only the redacted detail summary does.
pub async fn test_telegram(
    State(state): State<AppState>,
) -> Result<Json<TelegramTestResult>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
    let token = state.secrets.read(|s| {
        s.telegram
            .as_ref()
            .map(|t| t.bot_token.clone())
            .unwrap_or_default()
    });
    if token.is_empty() {
        return Err(unprocessable(
            "telegram is not configured. Set a bot_token via PUT /v1/config/telegram first.",
        ));
    }
    let started = std::time::Instant::now();
    let outcome = check_telegram_token(&token).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let result = match outcome {
        Ok((detail, username)) => TelegramTestResult {
            ok: true,
            status_code: Some(200),
            elapsed_ms,
            detail,
            bot_username: username,
        },
        Err((status_code, detail)) => TelegramTestResult {
            ok: false,
            status_code,
            elapsed_ms,
            detail,
            bot_username: None,
        },
    };
    tracing::info!(
        ok = result.ok,
        status_code = ?result.status_code,
        elapsed_ms = result.elapsed_ms,
        "config: telegram test"
    );
    state.intervention_audit.record_with_id(
        "anon",
        "telegram_test",
        "telegram",
        if result.ok { "ok" } else { "error" },
        format!(
            "{}ms{}{}{}",
            result.elapsed_ms,
            result
                .status_code
                .map(|c| format!(" · HTTP {c}"))
                .unwrap_or_default(),
            result
                .bot_username
                .as_ref()
                .map(|u| format!(" · @{u}"))
                .unwrap_or_default(),
            if result.ok {
                String::new()
            } else {
                format!(" · {}", result.detail)
            }
        ),
        corr,
    );
    Ok(Json(result))
}

/// Returns `Ok((detail, bot_username))` on success;
/// `Err((status_code, detail))` on failure. Never includes
/// the raw token in any returned string.
async fn check_telegram_token(
    bot_token: &str,
) -> Result<(String, Option<String>), (Option<u16>, String)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| (None, format!("http client init failed: {e}")))?;
    // URL built locally; never logged. We pass the raw token
    // to reqwest; the token bytes never leave this scope as a
    // string outside the URL.
    let url = format!("https://api.telegram.org/bot{bot_token}/getMe");
    let resp = client.get(&url).send().await.map_err(|e| {
        // reqwest's Error display includes the URL on transport
        // failures — strip it before forwarding so the token
        // can't leak via an error message.
        (
            None,
            format!("network error: {}", scrub_telegram_url(&e.to_string())),
        )
    })?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status.is_success() {
        // Telegram's success shape: { "ok": true, "result": { "username": "...", ... } }
        let username = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("result")
                    .and_then(|r| r.get("username"))
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string())
            });
        let detail = match &username {
            Some(u) => format!("ok ({}) · bot @{u}", status.as_u16()),
            None => format!("ok ({})", status.as_u16()),
        };
        Ok((detail, username))
    } else {
        Err((
            Some(status.as_u16()),
            format!(
                "upstream returned {}: {}",
                status.as_u16(),
                truncate_for_op(&scrub_telegram_url(&body), 200)
            ),
        ))
    }
}

/// Strip any substring that looks like a Telegram Bot API URL
/// fragment (`/bot<digits>:<chars>/`). Defensive guard so the
/// token can't leak via reqwest's error messages or an
/// unexpected upstream body that echoes the request URL.
fn scrub_telegram_url(s: &str) -> String {
    // Quick char-level scan: when we see "/bot" followed by a
    // digit, replace through to the next '/' (or end of string)
    // with "/bot<redacted>". Avoids pulling in a regex dep.
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for "/bot" prefix.
        if i + 5 <= bytes.len() && &bytes[i..i + 4] == b"/bot" && bytes[i + 4].is_ascii_digit() {
            out.push_str("/bot<redacted>");
            i += 4;
            // Skip until next '/' or end.
            while i < bytes.len() && bytes[i] != b'/' {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// `PUT /v1/config/telegram` — set the bot token + delivery
/// mode. Idempotent. Persists webhook mode + URL when
/// supplied, but the live HTTPS client wiring is pending —
/// the response `note` field surfaces this honestly so
/// operators don't expect immediate webhook activation.
pub async fn put_telegram(
    State(state): State<AppState>,
    Json(req): Json<PutTelegramReq>,
) -> Result<Json<PutTelegramResp>, (StatusCode, Json<ApiError>)> {
    let corr = new_correlation_id();
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
    // Webhook URL validation when in webhook mode.
    if req.mode == "webhook" {
        let url = req.webhook_url.as_deref().unwrap_or("");
        if url.trim().is_empty() {
            return Err(bad_request(
                "webhook_url required when mode='webhook' (https:// URL)",
            ));
        }
        if !url.starts_with("https://") {
            return Err(unprocessable(
                "webhook_url must be https:// (Telegram Bot API requires HTTPS)",
            ));
        }
    }
    let mode_for_log = req.mode.clone();
    let url_for_persist = if req.mode == "webhook" {
        req.webhook_url.clone()
    } else {
        // Drop any stale URL when switching to polling so
        // operators don't see a misleading URL on the
        // polling card.
        None
    };
    let result = state.secrets.mutate(|s| {
        s.set_telegram(req.bot_token.clone(), req.mode.clone(), url_for_persist);
        s.telegram_status()
    });
    match result {
        Ok(status) => {
            tracing::info!(
                mode = %status.mode,
                token_preview = %status.token_preview.as_deref().unwrap_or(""),
                webhook_url_set = status.webhook_url.is_some(),
                "config: telegram updated"
            );
            let note = if mode_for_log == "webhook" {
                Some(
                    "webhook mode persisted; live HTTPS client wiring is pending. \
                     The channel controller will continue using polling until \
                     the live webhook receiver ships."
                        .to_string(),
                )
            } else {
                None
            };
            state.intervention_audit.record_with_id(
                "anon",
                "telegram_save",
                "telegram",
                "ok",
                format!(
                    "mode={mode_for_log}{}",
                    if status.webhook_url.is_some() {
                        " · webhook_url set"
                    } else {
                        ""
                    }
                ),
                corr,
            );
            Ok(Json(PutTelegramResp {
                status,
                restart_required: true,
                note,
            }))
        }
        Err(e) => {
            state.intervention_audit.record_with_id(
                "anon",
                "telegram_save",
                "telegram",
                "error",
                format!("persist failed: {e}"),
                corr,
            );
            Err(internal(format!("persist failed: {e}")))
        }
    }
}

// ── Effective bridge config (redacted) ──────────────────────

/// Read-only redacted view of the bridge's effective config.
/// Shape: a small subset of the bridge's runtime state for the
/// dashboard's "Bridge Config" page. Distinct from the
/// secrets file itself.
#[derive(Debug, Serialize)]
pub struct EffectiveConfig {
    /// Bridge crate version from CARGO_PKG_VERSION. Lets
    /// operators verify they're talking to the build they
    /// just deployed (matters when restarting in place).
    pub bridge_version: &'static str,
    pub listen_addr: String,
    pub identity_bundle_path: String,
    pub peers_path: String,
    pub flow_template_path: String,
    pub tool_template_path: Option<String>,
    pub coordinator_alias: Option<String>,
    pub openai_compat: bool,
    pub secrets_path: String,
    pub providers_configured: Vec<String>,
    /// Subset of `providers_configured` whose `enabled` flag is
    /// true. Disabled providers stay in `providers_configured`
    /// so operators can see them but the AI controller treats
    /// them as routing-ineligible.
    pub providers_enabled: Vec<String>,
    pub telegram_configured: bool,
}

/// `GET /v1/config` — effective bridge config (redacted).
pub async fn get_effective_config(State(state): State<AppState>) -> Json<EffectiveConfig> {
    let (providers_configured, providers_enabled) = state.secrets.read(|s| {
        let statuses = s.all_provider_statuses();
        let configured: Vec<String> = statuses
            .iter()
            .filter(|p| p.configured)
            .map(|p| p.name.clone())
            .collect();
        let enabled: Vec<String> = statuses
            .iter()
            .filter(|p| p.configured && p.enabled)
            .map(|p| p.name.clone())
            .collect();
        (configured, enabled)
    });
    let telegram_configured = state.secrets.read(|s| s.telegram_status().configured);
    Json(EffectiveConfig {
        bridge_version: env!("CARGO_PKG_VERSION"),
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
        providers_enabled,
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
        s.set_telegram("1234:ABCDEF-NEVERLEAK-7890".into(), "polling".into(), None);
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
    fn scrub_telegram_url_redacts_token_in_url_fragment() {
        // The Telegram Bot API requires the token in the path.
        // If the URL leaks into an error string, the scrubber
        // must redact the token before the bridge forwards it.
        let s = scrub_telegram_url(
            "network error at https://api.telegram.org/bot1234567:ABCDEF-NEVERLEAK/getMe oops",
        );
        assert!(!s.contains("NEVERLEAK"), "token leaked: {s}");
        assert!(!s.contains("1234567:ABCDEF"), "token leaked: {s}");
        assert!(
            s.contains("/bot<redacted>/"),
            "expected redaction marker: {s}"
        );
    }

    #[test]
    fn scrub_telegram_url_handles_multiple_token_occurrences() {
        let s = scrub_telegram_url("/bot111:AAA/getMe and /bot222:BBB/sendMessage");
        assert!(!s.contains("AAA"));
        assert!(!s.contains("BBB"));
        assert_eq!(s.matches("/bot<redacted>/").count(), 2);
    }

    #[test]
    fn scrub_telegram_url_leaves_unrelated_text_alone() {
        let s = scrub_telegram_url("connection refused after 5s");
        assert_eq!(s, "connection refused after 5s");
        // Doesn't false-positive on "/bot" without a digit after it.
        let s2 = scrub_telegram_url("see /botanical for help");
        assert_eq!(s2, "see /botanical for help");
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
