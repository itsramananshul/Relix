//! AI node — registers the `ai.chat` capability with a provider-agnostic
//! backend.
//!
//! Provider selection is in config; the SOL flow never changes. See
//! `provider/mod.rs` for the [`ChatProvider`] trait and the per-backend
//! implementations.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Arg:    `session_id|prompt|history`   (UTF-8; pipe-delimited; history may be empty)
//! Return: provider's reply text         (UTF-8)
//!
//! ## Config
//!
//! ```toml
//! [controller]
//! node_type = "ai"
//!
//! [ai]
//! # Active provider: `mock` | `openai` | `openrouter` | `xai` | `local`
//! #                | `anthropic` | `gemini`
//! provider = "mock"
//! # Optional default model id. ChatInput.model overrides; empty means
//! # provider-side default.
//! model = ""
//!
//! [ai.providers.openai]
//! base_url     = "https://api.openai.com/v1"
//! api_key_env  = "OPENAI_API_KEY"
//! default_model = "gpt-4o-mini"
//!
//! [ai.providers.openrouter]
//! base_url     = "https://openrouter.ai/api/v1"
//! api_key_env  = "OPENROUTER_API_KEY"
//!
//! [ai.providers.xai]
//! base_url     = "https://api.x.ai/v1"
//! api_key_env  = "XAI_API_KEY"
//!
//! [ai.providers.local]
//! base_url     = "http://localhost:11434/v1"
//! # api_key_env unset or empty == no auth (Ollama-style local server).
//!
//! [ai.providers.anthropic]
//! api_key_env  = "ANTHROPIC_API_KEY"
//! default_model = "claude-3-5-sonnet-latest"
//!
//! [ai.providers.gemini]
//! api_key_env  = "GEMINI_API_KEY"
//! ```
//!
//! Provider keys live ONLY here on the AI node — never in `relix-web-bridge`
//! or any presentation peer.

pub mod failover;
pub mod provider;

pub use failover::{
    FailoverCategory, FailoverReason, classify_http_failure, classify_transport_failure,
};

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use provider::{
    AnthropicProvider, ChatInput, ChatProvider, GeminiProvider, MockProvider,
    OpenAICompatibleProvider, ProviderEntries, ProviderEntry, ProviderError,
};

/// Per-node AI configuration parsed from controller TOML `[ai]`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AiConfig {
    /// Active provider name. See module docs for the supported set.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Default model id used when ChatInput.model is empty. The provider
    /// also has its own `default_model`; this field is `[ai] model`.
    #[serde(default)]
    pub model: String,
    /// Per-provider settings, keyed by provider name (e.g. `openrouter`).
    #[serde(default)]
    pub providers: ProviderEntries,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            providers: ProviderEntries::new(),
        }
    }
}

fn default_provider() -> String {
    "mock".to_string()
}

/// Build the configured provider. Returns an Arc-wrapped trait object so
/// the handler closure can clone it cheaply across concurrent requests.
pub fn build_provider(cfg: &AiConfig) -> Result<Arc<dyn ChatProvider>, Box<dyn std::error::Error>> {
    // Helper: get the per-provider entry; default-construct if absent so
    // providers that legitimately need no config (mock) still boot.
    let entry =
        |name: &str| -> ProviderEntry { cfg.providers.get(name).cloned().unwrap_or_default() };

    match cfg.provider.as_str() {
        "mock" => Ok(Arc::new(MockProvider) as Arc<dyn ChatProvider>),
        "openai" => {
            let e = entry_or_err(&cfg.providers, "openai")?;
            let p = OpenAICompatibleProvider::from_entry("openai", &e)?;
            Ok(Arc::new(p))
        }
        "openrouter" => {
            let e = entry_or_err(&cfg.providers, "openrouter")?;
            let p = OpenAICompatibleProvider::from_entry("openrouter", &e)?;
            Ok(Arc::new(p))
        }
        "xai" => {
            let e = entry_or_err(&cfg.providers, "xai")?;
            let p = OpenAICompatibleProvider::from_entry("xai", &e)?;
            Ok(Arc::new(p))
        }
        "local" => {
            let e = entry_or_err(&cfg.providers, "local")?;
            let p = OpenAICompatibleProvider::from_entry("local", &e)?;
            Ok(Arc::new(p))
        }
        "anthropic" => {
            let e = entry_or_err(&cfg.providers, "anthropic")?;
            let p = AnthropicProvider::from_entry(&e)?;
            Ok(Arc::new(p))
        }
        "gemini" => {
            let e = entry(&cfg.provider);
            let p = GeminiProvider::from_entry(&e)?;
            Ok(Arc::new(p))
        }
        other => Err(format!("ai: unknown provider '{other}'").into()),
    }
}

fn entry_or_err(
    map: &ProviderEntries,
    name: &str,
) -> Result<ProviderEntry, Box<dyn std::error::Error>> {
    map.get(name).cloned().ok_or_else(|| {
        format!("provider='{name}' requires an [ai.providers.{name}] config section").into()
    })
}

/// Register the `ai.chat` capability with the supplied provider.
pub fn register(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
) {
    let provider_for_handler = provider.clone();
    bridge.register(
        "ai.chat",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_handler.clone();
            let model = default_model.clone();
            async move { handle_chat(p, model, ctx).await }
        })),
    );
}

async fn handle_chat(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    ctx: InvocationCtx,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.chat arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // session_id | prompt | history. `history` may contain `|` so splitn(3).
    let mut parts = s.splitn(3, '|');
    let session_id = parts.next().unwrap_or("");
    let prompt = parts.next();
    let history = parts.next().unwrap_or("");
    let Some(prompt) = prompt else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat arg must be `session_id|prompt[|history]`".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    };
    if session_id.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "ai.chat: session_id required".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    }

    let input = ChatInput {
        session_id: session_id.to_string(),
        prompt: prompt.to_string(),
        history: history.to_string(),
        model: default_model,
        ..ChatInput::default()
    };
    match provider.generate_reply(input).await {
        Ok(output) => HandlerOutcome::Ok(output.text.into_bytes()),
        Err(ProviderError::Transient(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_OVERLOADED,
            cause: format!("ai.chat: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
        Err(ProviderError::Permanent(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("ai.chat: {c}"),
            retry_hint: 2,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        }
    }

    #[tokio::test]
    async fn mock_provider_is_deterministic_with_and_without_history() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r1 = handle_chat(p.clone(), String::new(), ctx(b"s1|hello|")).await;
        let r2 = handle_chat(p.clone(), String::new(), ctx(b"s1|hello|")).await;
        match (r1, r2) {
            (HandlerOutcome::Ok(a), HandlerOutcome::Ok(b)) => assert_eq!(a, b),
            _ => panic!("expected both ok"),
        }
        let r3 = handle_chat(p, String::new(), ctx(b"s1|hello|user: prior\n")).await;
        match r3 {
            HandlerOutcome::Ok(body) => {
                let t = String::from_utf8(body).unwrap();
                assert!(
                    t.contains("history=12 chars"),
                    "expected 'history=12 chars' in: {t}"
                );
            }
            HandlerOutcome::Err(e) => panic!("unexpected error: {}", e.cause),
        }
    }

    #[tokio::test]
    async fn missing_prompt_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(p, String::new(), ctx(b"only-session-id")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[tokio::test]
    async fn empty_session_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(p, String::new(), ctx(b"|hello|")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[test]
    fn build_provider_defaults_to_mock() {
        let cfg = AiConfig::default();
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "mock"),
            Err(e) => panic!("default config should build: {e}"),
        }
    }

    #[test]
    fn build_provider_requires_per_provider_section() {
        let cfg = AiConfig {
            provider: "openrouter".into(),
            model: String::new(),
            providers: ProviderEntries::new(),
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(
                e.to_string().contains("[ai.providers.openrouter]"),
                "msg: {e}"
            ),
        }
    }

    #[test]
    fn build_provider_rejects_unknown_provider() {
        let cfg = AiConfig {
            provider: "rumple".into(),
            model: String::new(),
            providers: ProviderEntries::new(),
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("unknown provider")),
        }
    }

    #[test]
    fn build_provider_anthropic_signals_missing_key_env() {
        let mut providers = ProviderEntries::new();
        providers.insert(
            "anthropic".into(),
            ProviderEntry {
                base_url: None,
                api_key_env: None, // no env var named at all → clear error
                default_model: None,
                timeout_secs: 30,
            },
        );
        let cfg = AiConfig {
            provider: "anthropic".into(),
            model: String::new(),
            providers,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("missing api_key_env"), "msg: {e}"),
        }
    }

    #[test]
    fn build_provider_local_no_key_ok() {
        let mut providers = ProviderEntries::new();
        providers.insert(
            "local".into(),
            ProviderEntry {
                base_url: Some("http://localhost:11434/v1".into()),
                api_key_env: None,
                default_model: Some("llama3:8b".into()),
                timeout_secs: 30,
            },
        );
        let cfg = AiConfig {
            provider: "local".into(),
            model: String::new(),
            providers,
        };
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "local"),
            Err(e) => panic!("local should build without key: {e}"),
        }
    }
}
