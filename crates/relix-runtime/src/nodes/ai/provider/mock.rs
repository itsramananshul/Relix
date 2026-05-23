//! Deterministic mock provider. No network, no secrets; default for local
//! demos and tests. The reply shape exercises the SOL chat flow without
//! requiring any external credentials.

use async_trait::async_trait;

use super::{
    ChatInput, ChatOutput, ChatProvider, EmbedInput, EmbedOutput, ProviderError, TokenUsage,
};

/// Dimensionality the mock embedding generator returns. 8 is
/// enough to be non-degenerate (cosine sees meaningful distance)
/// while keeping test payloads tiny — 8 × 4 = 32 bytes per
/// vector. Real OpenAI embeddings are 1536 dims; nothing else in
/// the stack cares about the exact number.
pub const MOCK_EMBED_DIMS: usize = 8;

#[derive(Debug, Default)]
pub struct MockProvider;

/// Deterministic mock embedding: 8 f32 components derived from
/// blake3(text). Same text always returns the same vector;
/// different texts return different vectors. Vectors are roughly
/// unit length (each component is in `(-1, 1)`).
fn mock_embed_one(text: &str) -> Vec<f32> {
    let hash = blake3::hash(text.as_bytes());
    let bytes = hash.as_bytes();
    let mut out = Vec::with_capacity(MOCK_EMBED_DIMS);
    for i in 0..MOCK_EMBED_DIMS {
        // Two bytes per component → u16 → f32 in (-1, 1).
        let lo = bytes[i * 2] as u16;
        let hi = bytes[i * 2 + 1] as u16;
        let u = ((hi << 8) | lo) as f32;
        // Map u16 [0, 65535] to roughly (-1, 1).
        out.push((u - 32_768.0) / 32_768.0);
    }
    out
}

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

    async fn generate_embeddings(&self, input: EmbedInput) -> Result<EmbedOutput, ProviderError> {
        let model = if input.model.is_empty() {
            "mock-embed".to_string()
        } else {
            input.model.clone()
        };
        let vectors: Vec<Vec<f32>> = input.texts.iter().map(|t| mock_embed_one(t)).collect();
        Ok(EmbedOutput { model, vectors })
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
    async fn embeddings_deterministic_for_same_text() {
        let p = MockProvider;
        let a = p
            .generate_embeddings(EmbedInput {
                texts: vec!["hello".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        let b = p
            .generate_embeddings(EmbedInput {
                texts: vec!["hello".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(a.vectors, b.vectors);
        assert_eq!(a.vectors[0].len(), MOCK_EMBED_DIMS);
    }

    #[tokio::test]
    async fn embeddings_differ_for_different_text() {
        let p = MockProvider;
        let r = p
            .generate_embeddings(EmbedInput {
                texts: vec!["alpha".into(), "beta".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.vectors.len(), 2);
        assert_ne!(r.vectors[0], r.vectors[1]);
    }

    #[tokio::test]
    async fn embeddings_batch_returns_one_vec_per_input() {
        let p = MockProvider;
        let r = p
            .generate_embeddings(EmbedInput {
                texts: vec!["a".into(), "b".into(), "c".into(), "d".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.vectors.len(), 4);
        for v in &r.vectors {
            assert_eq!(v.len(), MOCK_EMBED_DIMS);
        }
    }

    #[tokio::test]
    async fn embeddings_use_default_model_when_unset() {
        let p = MockProvider;
        let r = p
            .generate_embeddings(EmbedInput {
                texts: vec!["x".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.model, "mock-embed");
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
