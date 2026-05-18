//! AI provider abstraction for the M7 `ai.chat` capability.
//!
//! The handler calls `ChatProvider::generate_reply`; the concrete
//! implementation is chosen by `[ai] provider = "mock" | "anthropic"`.
//!
//! Adding a new provider:
//! 1. Implement [`ChatProvider`] in a new file.
//! 2. Add a `Config` struct under `AiConfig` and a `build_provider` arm.
//! 3. Document the env var / config knobs in the README.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

/// Provider trait — implemented per-backend. `Send + Sync` because the
/// instance is shared (Arc) across the handler's concurrent invocations.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Generate a reply for the given session, prompt, and recent history.
    /// `history` is the multi-line `role: body\n` blob produced by
    /// `memory.recent_for_session`, possibly empty.
    async fn generate_reply(
        &self,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Result<String, ProviderError>;

    /// Short identifier shown in startup logs and audit metadata.
    fn provider_name(&self) -> &'static str;
}

/// Provider-layer error class. The handler maps these to RELIX-1 error kinds:
/// `Transient → responder_overloaded`, `Permanent → responder_internal`.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Network / rate-limit / 5xx — caller may retry.
    #[error("transient: {0}")]
    Transient(String),
    /// Config / auth / parsing — retry will not help.
    #[error("permanent: {0}")]
    Permanent(String),
}

// ──────────────────────────── Mock provider ────────────────────────────────

/// Deterministic, network-free provider. Default and recommended for local
/// demos / tests; the reply shape exercises the SOL flow without requiring
/// any external credentials.
#[derive(Debug, Default)]
pub struct MockProvider;

#[async_trait]
impl ChatProvider for MockProvider {
    async fn generate_reply(
        &self,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Result<String, ProviderError> {
        Ok(format!(
            "mock: heard \"{prompt}\" in {session_id} (history={} chars)\n",
            history.len()
        ))
    }

    fn provider_name(&self) -> &'static str {
        "mock"
    }
}

// ──────────────────────────── Anthropic provider ───────────────────────────

/// Settings for the real Anthropic provider. The API key never lives in this
/// struct directly — it is loaded at construction time from either
/// `ANTHROPIC_API_KEY` (env) or the `api_key_path` file (gitignored).
#[derive(Clone, Debug, Deserialize)]
pub struct AnthropicConfig {
    /// File containing the API key (32+ bytes UTF-8, no trailing newline
    /// required — leading/trailing whitespace is stripped). Optional; the
    /// `ANTHROPIC_API_KEY` env var is consulted first.
    pub api_key_path: Option<PathBuf>,
    /// Anthropic model identifier. Default: `claude-3-5-sonnet-latest`.
    #[serde(default = "default_model")]
    pub model: String,
    /// Max tokens to request. Default 1024.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Request timeout in seconds. Default 60.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_model() -> String {
    "claude-3-5-sonnet-latest".to_string()
}
fn default_max_tokens() -> u32 {
    1024
}
fn default_timeout_secs() -> u64 {
    60
}

/// Real provider. Uses `reqwest` with rustls and the standard Anthropic
/// `messages` endpoint.
pub struct AnthropicProvider {
    api_key: String,
    model: String,
    max_tokens: u32,
    http: reqwest::Client,
}

impl AnthropicProvider {
    /// Construct from config. Loads the API key from env or file; errors if
    /// neither source is set.
    pub fn from_config(cfg: AnthropicConfig) -> Result<Self, Box<dyn std::error::Error>> {
        let api_key = load_api_key(&cfg)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .build()
            .map_err(|e| format!("anthropic: http client: {e}"))?;
        Ok(Self {
            api_key,
            model: cfg.model,
            max_tokens: cfg.max_tokens,
            http,
        })
    }
}

fn load_api_key(cfg: &AnthropicConfig) -> Result<String, Box<dyn std::error::Error>> {
    if let Ok(env) = std::env::var("ANTHROPIC_API_KEY") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(path) = &cfg.api_key_path {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("anthropic: read {}: {e}", path.display()))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("anthropic: api key file empty: {}", path.display()).into());
        }
        return Ok(trimmed.to_string());
    }
    Err("anthropic: no api key — set ANTHROPIC_API_KEY env or [ai.anthropic] api_key_path".into())
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn generate_reply(
        &self,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Result<String, ProviderError> {
        // SIMP: we ship a single user message that combines history + prompt
        // until typed-message-arrays cross the SIMP-016 string-arg boundary
        // (Gate 2). The wrapper sentence makes the layout obvious to the model.
        let user_content = if history.trim().is_empty() {
            prompt.to_string()
        } else {
            format!(
                "Recent conversation (session={session_id}):\n{history}\n\nNew message: {prompt}"
            )
        };
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": [
                { "role": "user", "content": user_content }
            ]
        });
        let resp = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("anthropic: http: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transient(format!("anthropic: read body: {e}")))?;

        if !status.is_success() {
            // 429 + 5xx → transient; other 4xx → permanent (auth, bad request)
            let perm = !(status.as_u16() == 429 || status.is_server_error());
            let msg = format!("anthropic: HTTP {status}: {text}");
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        // Extract `content[0].text`. Tolerant of additional content blocks.
        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("anthropic: parse: {e}")))?;
        let reply = parsed
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.iter().find_map(|b| b.get("text")?.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ProviderError::Permanent(format!("anthropic: no text content in: {text}"))
            })?;
        Ok(reply)
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: env-var precedence is verified by inspection of load_api_key
    // rather than by mutating the global env at test time. In Rust 2024
    // `std::env::set_var` is `unsafe`, and this crate's `forbid(unsafe_code)`
    // policy stays on; cross-thread test pollution outweighed the value of
    // exhaustive env-mutation tests here.

    #[test]
    fn load_api_key_reads_from_file_when_env_absent() {
        // We rely on the test environment not having ANTHROPIC_API_KEY set.
        // If it is set (CI secret leakage, dev shell), this test is skipped
        // to avoid a false negative — the env path is exercised by the
        // production code path at controller startup.
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            eprintln!("skipping load_api_key_reads_from_file: env var is set in this shell");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k.txt");
        std::fs::write(&path, " test-from-file\n").unwrap();
        let key = load_api_key(&AnthropicConfig {
            api_key_path: Some(path),
            model: default_model(),
            max_tokens: 1024,
            timeout_secs: 5,
        })
        .unwrap();
        assert_eq!(key, "test-from-file");
    }

    #[test]
    fn load_api_key_errors_when_neither_set() {
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            eprintln!("skipping: env var present in this shell");
            return;
        }
        match load_api_key(&AnthropicConfig {
            api_key_path: None,
            model: default_model(),
            max_tokens: 1024,
            timeout_secs: 5,
        }) {
            Ok(_) => panic!("expected error when no key source set"),
            Err(e) => assert!(e.to_string().contains("no api key")),
        }
    }

    #[tokio::test]
    async fn mock_provider_reply_includes_history_size() {
        let p = MockProvider;
        let r = p.generate_reply("s1", "hi", "user: prev\n").await.unwrap();
        assert!(r.contains("history=11 chars"));
        assert!(r.contains("\"hi\""));
        assert!(r.contains("in s1"));
    }
}
