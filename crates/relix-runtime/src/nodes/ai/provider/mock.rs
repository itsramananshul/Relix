//! Deterministic mock provider. No network, no secrets; default for local
//! demos and tests. The reply shape exercises the SOL chat flow without
//! requiring any external credentials.

use async_trait::async_trait;

use super::{ChatInput, ChatOutput, ChatProvider, ProviderError, TokenUsage};

#[derive(Debug, Default)]
pub struct MockProvider;

#[async_trait]
impl ChatProvider for MockProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            "mock-1".to_string()
        } else {
            input.model.clone()
        };
        let text = format!(
            "mock: heard \"{prompt}\" in {session} (history={chars} chars)\n",
            prompt = input.prompt,
            session = input.session_id,
            chars = input.history.len(),
        );
        let usage = TokenUsage {
            prompt_tokens: input.prompt.len() as u32 / 4,
            completion_tokens: text.len() as u32 / 4,
            total_tokens: (input.prompt.len() + text.len()) as u32 / 4,
        };
        Ok(ChatOutput {
            text,
            provider: "mock",
            model,
            usage: Some(usage),
        })
    }

    fn provider_name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_reply_includes_history_size() {
        let p = MockProvider;
        let r = p
            .generate_reply(ChatInput {
                session_id: "s1".into(),
                prompt: "hi".into(),
                history: "user: prev\n".into(),
                ..ChatInput::default()
            })
            .await
            .unwrap();
        assert_eq!(r.provider, "mock");
        assert!(r.text.contains("history=11 chars"));
        assert!(r.text.contains("\"hi\""));
        assert!(r.text.contains("in s1"));
    }

    #[tokio::test]
    async fn caller_model_passed_through() {
        let p = MockProvider;
        let r = p
            .generate_reply(ChatInput {
                session_id: "s1".into(),
                prompt: "x".into(),
                model: "custom-model".into(),
                ..ChatInput::default()
            })
            .await
            .unwrap();
        assert_eq!(r.model, "custom-model");
    }
}
