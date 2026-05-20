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
