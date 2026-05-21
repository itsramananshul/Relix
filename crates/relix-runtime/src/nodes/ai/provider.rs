//! AI provider abstraction for the `ai.chat` capability.
//!
//! Relix is provider-agnostic by design: the AI node never assumes a single
//! backend. The handler calls `ChatProvider::generate_reply` and the active
//! implementation is chosen at controller startup from `[ai] provider = "…"`.
//!
//! Supported providers in this milestone:
//!
//! | name           | impl                          | wire family                    |
//! |----------------|-------------------------------|--------------------------------|
//! | `mock`         | [`MockProvider`]              | deterministic, no network      |
//! | `openai`       | [`OpenAICompatibleProvider`]  | OpenAI `/v1/chat/completions`  |
//! | `openrouter`   | [`OpenAICompatibleProvider`]  | OpenRouter (same wire)         |
//! | `xai`          | [`OpenAICompatibleProvider`]  | xAI / Grok (OpenAI-compatible) |
//! | `local`        | [`OpenAICompatibleProvider`]  | any local OpenAI-compatible    |
//! | `anthropic`    | [`AnthropicProvider`]         | Anthropic Messages API         |
//! | `gemini`       | [`GeminiProvider`]            | placeholder; `not_implemented` |
//!
//! Adding a new backend = a new file implementing [`ChatProvider`] + a
//! `build_provider` arm. The SOL flow surface (`ai.chat` arg shape) does
//! not change.
//!
//! ## Credentials
//!
//! Per-provider `api_key_env = "VAR_NAME"` in the AI-node config names the
//! env var the provider reads at startup. Keys are NEVER inline in TOML.
//! `api_key_env = ""` (or unset) means "no auth" (used by local
//! OpenAI-compatible servers, e.g. Ollama).
//!
//! The bridge / web layer is intentionally **not** allowed to hold any of
//! these keys (see SECURITY.md and docs/provider-configuration.md).

pub mod anthropic;
pub mod gemini;
pub mod mock;
pub mod openai_compat;

use async_trait::async_trait;
use std::collections::BTreeMap;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use mock::MockProvider;
pub use openai_compat::OpenAICompatibleProvider;

// ──────────────────────────── Trait ────────────────────────────────────────

/// Inputs to `ChatProvider::generate_reply`. Stable across new optional
/// fields (system prompt, temperature, …).
#[derive(Clone, Debug, Default)]
pub struct ChatInput {
    /// Session id from the SOL flow.
    pub session_id: String,
    /// The user's new message.
    pub prompt: String,
    /// Recent conversation history (the `role: body\n` blob from
    /// `memory.recent_for_session`). May be empty.
    pub history: String,
    /// Caller-requested model id (provider-specific). If empty, the provider
    /// falls back to its `default_model` config knob.
    pub model: String,
    /// Optional system prompt. None means provider default.
    pub system_prompt: Option<String>,
    /// Optional sampling temperature in `[0, 2]`.
    pub temperature: Option<f32>,
    /// Optional max tokens to generate.
    pub max_tokens: Option<u32>,
    /// PH-WAVE2F: opt-in budget for Anthropic-style extended
    /// thinking (o1/o3-style structured reasoning). When
    /// `Some(N)` AND the active provider is Anthropic, the
    /// request body adds `thinking: { type: "enabled",
    /// budget_tokens: N }`. Providers that don't support
    /// extended thinking (OpenAI-compat, Gemini placeholder,
    /// mock) ignore the field. Honest scope: extended-thinking
    /// output is emitted by Anthropic as separate `thinking`
    /// content blocks alongside the regular `text` block;
    /// today's AnthropicProvider returns only the `text`
    /// block, so callers get the *benefit* of extended
    /// reasoning without seeing the reasoning trace. A future
    /// milestone can surface the thinking text via a new
    /// ChatOutput field; the request-side knob ships now
    /// because it's pure additive and operators don't need
    /// the trace to want the better answer quality.
    pub thinking_budget_tokens: Option<u32>,
}

/// Structured response from a provider.
#[derive(Clone, Debug)]
pub struct ChatOutput {
    /// Reply text.
    pub text: String,
    /// Provider name (`"mock"`, `"openai"`, …).
    pub provider: &'static str,
    /// Model identifier the provider actually used.
    pub model: String,
    /// Provider-supplied token usage, if known.
    pub usage: Option<TokenUsage>,
}

/// Best-effort token accounting.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Provider-layer trait. `Send + Sync` because the instance lives behind
/// an `Arc` shared by the handler.
#[async_trait]
pub trait ChatProvider: Send + Sync {
    /// Generate a reply.
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError>;

    /// Short identifier shown in startup logs and audit metadata.
    fn provider_name(&self) -> &'static str;
}

/// Provider-layer error class. The handler maps:
/// `Transient → responder_overloaded`, `Permanent → responder_internal`.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Network / 5xx / 429 — caller may retry.
    #[error("transient: {0}")]
    Transient(String),
    /// Config / auth / parsing / not-yet-implemented — retry will not help.
    #[error("permanent: {0}")]
    Permanent(String),
}

// ──────────────────────────── Shared config helpers ────────────────────────

/// One entry under `[ai.providers.<name>]`. Per-provider settings:
/// - `base_url` — endpoint override (mandatory for OpenAI-compatible).
/// - `api_key_env` — env var the provider reads at startup. Empty string OR
///   unset means "no auth", used by `local` (Ollama-style) servers.
/// - `default_model` — model id used when `ChatInput.model` is empty.
/// - `timeout_secs` — request timeout.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct ProviderEntry {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

pub(crate) fn default_timeout_secs() -> u64 {
    60
}

/// Per-provider entries, keyed by provider name.
pub type ProviderEntries = BTreeMap<String, ProviderEntry>;

/// Read `entry.api_key_env`; return `Ok(None)` when `api_key_env` is unset
/// OR empty (the latter signals "no auth"). Return `Err(Permanent)` when
/// the env var is named but missing — that almost always means
/// misconfiguration and is worth surfacing loudly.
pub(crate) fn load_api_key(entry: &ProviderEntry) -> Result<Option<String>, ProviderError> {
    let Some(name) = entry.api_key_env.as_deref() else {
        return Ok(None);
    };
    if name.is_empty() {
        return Ok(None);
    }
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(Some(v.trim().to_string())),
        Ok(_) => Err(ProviderError::Permanent(format!(
            "env var '{name}' is set but empty"
        ))),
        Err(_) => Err(ProviderError::Permanent(format!(
            "missing provider key: ${name}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_api_key_returns_none_when_unset() {
        let entry = ProviderEntry::default();
        assert!(matches!(load_api_key(&entry), Ok(None)));
    }

    #[test]
    fn load_api_key_returns_none_when_explicitly_empty() {
        let entry = ProviderEntry {
            api_key_env: Some(String::new()),
            ..Default::default()
        };
        assert!(matches!(load_api_key(&entry), Ok(None)));
    }

    #[test]
    fn load_api_key_errors_when_named_var_missing() {
        let entry = ProviderEntry {
            api_key_env: Some("RELIX_TEST_ABSOLUTELY_MISSING_VAR_42".into()),
            ..Default::default()
        };
        match load_api_key(&entry) {
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing provider key")),
            other => panic!("expected permanent error, got {other:?}"),
        }
    }
}
