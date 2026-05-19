//! Gemini provider — placeholder.
//!
//! Config and trait wiring exist so a controller declaring
//! `[ai] provider = "gemini"` boots cleanly; calls return a clear
//! `not_yet_implemented` permanent error until the real provider lands.
//! Tracked as future work in `docs/provider-configuration.md`.

use async_trait::async_trait;

use super::{ChatInput, ChatOutput, ChatProvider, ProviderEntry, ProviderError, load_api_key};

pub struct GeminiProvider {
    // Constructed at startup so misconfiguration (missing api_key_env)
    // surfaces before the first call.
    _api_key: Option<String>,
}

impl GeminiProvider {
    pub fn from_entry(entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let api_key = load_api_key(entry)?;
        Ok(Self { _api_key: api_key })
    }
}

#[async_trait]
impl ChatProvider for GeminiProvider {
    async fn generate_reply(&self, _input: ChatInput) -> Result<ChatOutput, ProviderError> {
        Err(ProviderError::Permanent(
            "gemini provider not yet implemented (M9+); use mock / openai / anthropic for now"
                .into(),
        ))
    }

    fn provider_name(&self) -> &'static str {
        "gemini"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn placeholder_returns_clear_not_implemented() {
        // No api key required — Gemini stub must boot regardless so the
        // operator sees a clean error at first call, not at startup.
        let p = GeminiProvider { _api_key: None };
        match p.generate_reply(ChatInput::default()).await {
            Err(ProviderError::Permanent(m)) => assert!(m.contains("not yet implemented")),
            other => panic!("expected permanent not-yet-implemented, got {other:?}"),
        }
    }
}
