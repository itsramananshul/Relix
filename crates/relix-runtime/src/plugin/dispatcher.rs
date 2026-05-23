//! HTTP/JSON dispatcher that calls a plugin subprocess's
//! `/invoke` endpoint. One `PluginDispatcher` per loaded plugin
//! — owns the plugin's port + the per-plugin invoke timeout.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// `/invoke` request body. Same shape relix-plugin-sdk decodes
/// on the plugin side.
#[derive(Clone, Debug, Serialize)]
pub struct InvokeRequest {
    pub method: String,
    pub args: String,
    pub trace_id: String,
    pub request_id: String,
    pub caller_subject_id: String,
    pub deadline_unix: i64,
}

/// `/invoke` response body.
#[derive(Clone, Debug, Deserialize)]
struct InvokeResponse {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    error_kind: Option<u32>,
    #[serde(default)]
    error_cause: Option<String>,
}

/// Errors the dispatcher returns. Each one maps cleanly to an
/// `ErrorEnvelope` at the host capability handler.
#[derive(Debug, thiserror::Error)]
pub enum PluginInvokeError {
    /// Connection refused / network issue / timeout.
    #[error("transport: {0}")]
    Transport(String),
    /// Body decode failure — the plugin sent something we can't
    /// understand.
    #[error("decode: {0}")]
    Decode(String),
    /// Plugin returned `ok: false` with a structured error.
    /// `kind` mirrors `relix_core::types::error_kinds`.
    #[error("plugin err kind={kind} {cause}")]
    Plugin { kind: u32, cause: String },
}

#[derive(Clone)]
pub struct PluginDispatcher {
    http: reqwest::Client,
    base: String,
    invoke_timeout: Duration,
}

impl PluginDispatcher {
    pub fn new(port: u16, invoke_timeout_secs: u64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(invoke_timeout_secs.max(1)))
            .build()
            .expect("reqwest::Client::builder succeeds");
        Self {
            http,
            base: format!("http://127.0.0.1:{port}"),
            invoke_timeout: Duration::from_secs(invoke_timeout_secs.max(1)),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// Hit `/health`. Returns `Ok(true)` if the server replied
    /// 200 within the per-call timeout, `Ok(false)` for any
    /// non-200, and `Err` for transport failures.
    pub async fn health(&self) -> Result<bool, PluginInvokeError> {
        let url = format!("{}/health", self.base);
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| PluginInvokeError::Transport(format!("health: {e}")))?;
        Ok(resp.status().is_success())
    }

    /// Hit `/invoke` with a typed request body. Returns the
    /// plugin's response body on `ok: true`; converts `ok: false`
    /// to [`PluginInvokeError::Plugin`].
    pub async fn invoke(&self, req: InvokeRequest) -> Result<String, PluginInvokeError> {
        let url = format!("{}/invoke", self.base);
        let resp = self
            .http
            .post(&url)
            .timeout(self.invoke_timeout)
            .json(&req)
            .send()
            .await
            .map_err(|e| PluginInvokeError::Transport(format!("invoke: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(PluginInvokeError::Transport(format!(
                "invoke: HTTP {status}"
            )));
        }
        let body: InvokeResponse = resp
            .json()
            .await
            .map_err(|e| PluginInvokeError::Decode(format!("invoke: {e}")))?;
        if body.ok {
            Ok(body.body.unwrap_or_default())
        } else {
            Err(PluginInvokeError::Plugin {
                kind: body.error_kind.unwrap_or(11),
                cause: body
                    .error_cause
                    .unwrap_or_else(|| "(no error_cause)".to_string()),
            })
        }
    }
}
