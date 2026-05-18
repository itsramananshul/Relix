//! AI node — registers the `ai.chat` capability (M7).
//!
//! Provider plumbing is behind a small [`ChatProvider`] trait so the handler
//! is agnostic to whether the responder is the deterministic mock or a real
//! Anthropic call. M7 ships two implementations:
//!
//! - [`provider::MockProvider`] — deterministic, network-free, used for local
//!   demos and tests. The reply echoes the prompt and reports the history
//!   character count.
//! - [`provider::AnthropicProvider`] — real `claude` call via `reqwest` against
//!   `https://api.anthropic.com/v1/messages`. The API key is loaded from
//!   `ANTHROPIC_API_KEY` (env) OR `[ai.anthropic] api_key_path` (file path).
//!   Never embedded in a config or committed to the repo.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Arg:    `session_id|prompt|history`   (UTF-8; pipe-delimited; history may be empty)
//! Return: provider's reply text         (UTF-8)
//!
//! `history` is the multi-line `role: body\n` blob produced by
//! `memory.recent_for_session`. The provider is responsible for turning it
//! into whatever shape its backing model expects; for Anthropic we currently
//! concatenate it into a single user message ("Recent conversation: ...\nNew
//! message: ...") because typed-message-arrays cross the SIMP-016 string-arg
//! line. Gate-2 CDDL types replace this when ready.
//!
//! ## Config
//!
//! ```toml
//! [controller]
//! node_type = "ai"
//!
//! [ai]
//! provider = "mock"          # or "anthropic"
//!
//! [ai.anthropic]
//! api_key_path = "dev-keys/anthropic.key"  # or unset to use $ANTHROPIC_API_KEY
//! model        = "claude-3-5-sonnet-latest"
//! max_tokens   = 1024
//! ```

pub mod provider;

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use provider::{AnthropicConfig, AnthropicProvider, ChatProvider, MockProvider, ProviderError};

/// Per-node AI configuration parsed from controller TOML `[ai]`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AiConfig {
    /// `"mock"` (default; deterministic) or `"anthropic"` (real provider).
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Provider-specific settings; required when `provider == "anthropic"`.
    #[serde(default)]
    pub anthropic: Option<AnthropicConfig>,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            anthropic: None,
        }
    }
}

fn default_provider() -> String {
    "mock".to_string()
}

/// Build the configured provider. Returns an Arc-wrapped trait object so the
/// handler closure can clone it cheaply across concurrent requests.
pub fn build_provider(cfg: &AiConfig) -> Result<Arc<dyn ChatProvider>, Box<dyn std::error::Error>> {
    match cfg.provider.as_str() {
        "mock" => Ok(Arc::new(MockProvider) as Arc<dyn ChatProvider>),
        "anthropic" => {
            let anth_cfg = cfg.anthropic.clone().ok_or_else(|| {
                "provider=anthropic requires an [ai.anthropic] config section".to_string()
            })?;
            let p = AnthropicProvider::from_config(anth_cfg)?;
            Ok(Arc::new(p) as Arc<dyn ChatProvider>)
        }
        other => Err(format!("ai: unknown provider '{other}'").into()),
    }
}

/// Register the `ai.chat` capability with the supplied provider.
pub fn register(bridge: &mut DispatchBridge, provider: Arc<dyn ChatProvider>) {
    let provider_for_handler = provider.clone();
    bridge.register(
        "ai.chat",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let p = provider_for_handler.clone();
            async move { handle_chat(p, ctx).await }
        })),
    );
}

async fn handle_chat(provider: Arc<dyn ChatProvider>, ctx: InvocationCtx) -> HandlerOutcome {
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
    let history = parts.next().unwrap_or(""); // optional; empty when absent
    let (Some(prompt),) = (prompt,) else {
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

    match provider.generate_reply(session_id, prompt, history).await {
        Ok(reply) => HandlerOutcome::Ok(reply.into_bytes()),
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
        let r1 = handle_chat(p.clone(), ctx(b"s1|hello|")).await;
        let r2 = handle_chat(p.clone(), ctx(b"s1|hello|")).await;
        match (r1, r2) {
            (HandlerOutcome::Ok(a), HandlerOutcome::Ok(b)) => assert_eq!(a, b),
            _ => panic!("expected both ok"),
        }
        let r3 = handle_chat(p, ctx(b"s1|hello|user: prior\n")).await;
        match r3 {
            HandlerOutcome::Ok(body) => {
                let t = String::from_utf8(body).unwrap();
                // "user: prior\n" = 12 bytes.
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
        let r = handle_chat(p, ctx(b"only-session-id")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[tokio::test]
    async fn empty_session_rejected() {
        let p: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        let r = handle_chat(p, ctx(b"|hello|")).await;
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }

    #[test]
    fn build_provider_defaults_to_mock() {
        let cfg = AiConfig::default();
        // Arc<dyn ChatProvider> does not impl Debug, so avoid expect().
        match build_provider(&cfg) {
            Ok(p) => assert_eq!(p.provider_name(), "mock"),
            Err(e) => panic!("default config should build: {e}"),
        }
    }

    #[test]
    fn build_provider_anthropic_requires_section() {
        let cfg = AiConfig {
            provider: "anthropic".into(),
            anthropic: None,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("[ai.anthropic]")),
        }
    }

    #[test]
    fn build_provider_rejects_unknown() {
        let cfg = AiConfig {
            provider: "unknown".into(),
            anthropic: None,
        };
        match build_provider(&cfg) {
            Ok(_) => panic!("expected error"),
            Err(e) => assert!(e.to_string().contains("unknown provider")),
        }
    }
}
