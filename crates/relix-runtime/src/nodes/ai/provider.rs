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
pub use mock::{MOCK_EMBED_DIMS, MockProvider};
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
#[derive(Clone, Debug, Default)]
pub struct ChatOutput {
    /// Reply text.
    pub text: String,
    /// Provider name (`"mock"`, `"openai"`, …).
    pub provider: &'static str,
    /// Model identifier the provider actually used.
    pub model: String,
    /// Provider-supplied token usage, if known.
    pub usage: Option<TokenUsage>,
    /// RELIX-7.19 GAP 3: provider-reported finish reason
    /// normalised to a small vocabulary the ConfidenceScorer
    /// understands: `"stop"`, `"length"`, `"content_filter"`,
    /// `"tool_use"`, or `"other"`. `None` when the provider
    /// didn't report one. Streaming providers populate this
    /// from the final SSE frame.
    pub finish_reason: Option<String>,
    /// RELIX-7.19 GAP 3: average per-token log-probability of
    /// the response, when the provider reports it
    /// (OpenAI-compatible `logprobs.content[*].logprob`). The
    /// scorer maps `exp(logprob)` clamped to `[0, 1]` into the
    /// `provider_signal` sub-score.
    pub logprob: Option<f32>,
}

/// GAP 16 §7.29 Model Name Resolution — one row from a
/// provider's live model catalogue.
///
/// Fields beyond `id` are best-effort; not every provider's
/// `/models` endpoint surfaces pricing or context window, and
/// the spec calls for showing what's actually available rather
/// than synthesising values. `id` is required so a future tier
/// router config can validate against it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AvailableModel {
    /// Provider-canonical model id (e.g.
    /// `"anthropic/claude-opus-4"` for OpenRouter or
    /// `"gpt-4o-mini-2024-07-18"` for OpenAI direct).
    pub id: String,
    /// Optional human-friendly label when the provider ships
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Optional context window size (tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Optional price per million input tokens, in micro-USD.
    /// `1_000` = $0.001 per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_micros_per_mtoken: Option<u64>,
    /// Optional price per million output tokens, in micro-USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_micros_per_mtoken: Option<u64>,
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

    /// GAP 16 §7.29 Model Name Resolution — fetch the live
    /// model catalogue from the provider. Each entry carries
    /// the provider's canonical model id; future commits may
    /// extend [`AvailableModel`] with pricing + context-window
    /// metadata.
    ///
    /// The default impl returns `Ok(vec![])` so providers that
    /// have no `/models` endpoint (Anthropic without the new
    /// `/v1/models` route; mock) don't need to override.
    /// Operators reading `Ok(vec![])` should interpret it as
    /// "this provider doesn't expose its model list; configure
    /// model IDs manually" — distinct from `Err(_)` which
    /// signals an actual API failure they should retry or
    /// investigate.
    async fn list_available_models(&self) -> Result<Vec<AvailableModel>, ProviderError> {
        Ok(Vec::new())
    }

    /// Generate embeddings for a batch of texts. Default impl returns
    /// `Permanent("not supported")` so providers that have no
    /// embedding API (Anthropic, Gemini today) don't need to
    /// be touched. Mock + OpenAI-compatible override.
    async fn generate_embeddings(&self, input: EmbedInput) -> Result<EmbedOutput, ProviderError> {
        let _ = input;
        Err(ProviderError::Permanent(format!(
            "{} provider does not support embeddings",
            self.provider_name()
        )))
    }

    /// Generate a reply as a stream of [`StreamingChunk`] frames.
    /// The default implementation calls `generate_reply` and
    /// yields the full response as a single `Text` frame
    /// followed by an optional `Usage` frame when the
    /// non-streaming response carried usage metadata. Providers
    /// with native streaming APIs override and emit token-level
    /// `Text` chunks + a terminal `Usage` chunk extracted from
    /// the provider's final wire frame.
    async fn generate_reply_stream(&self, input: ChatInput) -> Result<ChatStream, ProviderError> {
        let out = self.generate_reply(input).await?;
        let text = out.text;
        let model = out.model;
        let usage = out.usage;
        let finish_reason = out.finish_reason;
        let s = async_stream::stream! {
            yield Ok(StreamingChunk::Text(text));
            if let Some(fr) = finish_reason {
                yield Ok(StreamingChunk::FinishReason(fr));
            }
            if let Some(u) = usage {
                yield Ok(StreamingChunk::Usage(StreamingUsage {
                    prompt_tokens: u.prompt_tokens,
                    completion_tokens: u.completion_tokens,
                    model,
                }));
            }
        };
        Ok(Box::pin(s))
    }
}

/// One frame of a streaming chat reply. Producers yield zero or
/// more [`StreamingChunk::Text`] items in order followed by an
/// optional single [`StreamingChunk::Usage`] frame after the
/// last text frame.
///
/// Consumers that only care about the assistant's words ignore
/// the `Usage` variant — `match` covers both, but in the wire-
/// path streaming bridge only `Text` frames are forwarded.
/// `Usage` frames feed the RELIX-7.11 metrics enrichment hook
/// (`MetricsSink::attach_ai_usage`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamingChunk {
    /// One incremental piece of assistant text. Producers
    /// preserve byte-level fidelity; consumers concatenate to
    /// reproduce the full reply.
    Text(String),
    /// Provider-reported token usage. Emitted exactly once at
    /// stream end when the provider's API surfaces a final
    /// usage payload. Providers that don't expose usage on
    /// their streaming API simply never yield this variant.
    Usage(StreamingUsage),
    /// RELIX-7.19 GAP 3: provider-reported finish reason from
    /// the final stream frame. Emitted at most once per
    /// stream, BEFORE the optional `Usage` chunk. Consumers
    /// that only forward `Text` frames to the wire (the AI
    /// handler does) should intercept this variant and feed
    /// it into the `AiProviderSignalsHint` side channel for
    /// the dispatch bridge to pick up.
    FinishReason(String),
}

/// Token usage carried by a [`StreamingChunk::Usage`] frame.
/// Same shape as [`TokenUsage`] but adds the resolved `model`
/// id so downstream pricing can look up the right per-1k rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamingUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Model identifier the provider actually used. Carries the
    /// upstream's value verbatim (e.g. `"gpt-4o-mini-2024-07-18"`)
    /// so the longest-prefix pricing lookup hits.
    pub model: String,
}

/// Streaming reply type alias. Producers yield one frame at a
/// time. A `ProviderError` on any item terminates the stream —
/// callers should treat the frames accumulated before the
/// error as a partial reply and surface the error to the
/// client.
pub type ChatStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamingChunk, ProviderError>> + Send>>;

/// Batch embedding request.
#[derive(Clone, Debug, Default)]
pub struct EmbedInput {
    /// Model id (provider-specific — `text-embedding-3-small`,
    /// `nomic-embed-text`, …). Empty means "use the provider's
    /// default embedding model" — for OpenAI-compatible this is
    /// `text-embedding-3-small`; for mock it's `mock-embed`.
    pub model: String,
    /// Inputs to embed. Order is preserved in the response.
    pub texts: Vec<String>,
}

/// Batch embedding response.
#[derive(Clone, Debug)]
pub struct EmbedOutput {
    /// Model id the provider actually used.
    pub model: String,
    /// One f32 vector per input text, in input order.
    pub vectors: Vec<Vec<f32>>,
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
