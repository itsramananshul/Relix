//! Deterministic mock provider. No network, no secrets; default for local
//! demos and tests. The reply shape exercises the SOL chat flow without
//! requiring any external credentials.

use async_trait::async_trait;

use super::{
    ChatInput, ChatOutput, ChatProvider, ChatStream, EmbedInput, EmbedOutput, ProviderError,
    StreamingChunk, StreamingUsage, TokenUsage,
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
            // RELIX-7.19 GAP 3: deterministic mock signal so
            // confidence-scoring tests against the mock
            // provider see a populated provider_signal field.
            finish_reason: Some("stop".to_string()),
            logprob: Some(-0.05_f32),
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

    /// Stream the deterministic reply word-by-word with a 20ms
    /// delay between yields, then emit a [`StreamingChunk::Usage`]
    /// frame derived from the same fake token accounting the
    /// unary path uses. Makes the RELIX-7.11 streaming-usage
    /// path testable end-to-end without a network provider.
    async fn generate_reply_stream(&self, input: ChatInput) -> Result<ChatStream, ProviderError> {
        let out = self.generate_reply(input).await?;
        let words: Vec<String> = mock_split_into_chunks(&out.text);
        let usage = out.usage;
        let model = out.model;
        let finish_reason = out.finish_reason;
        let s = async_stream::stream! {
            for (i, w) in words.into_iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                yield Ok(StreamingChunk::Text(w));
            }
            // RELIX-7.19 GAP 3: emit finish reason BEFORE
            // usage so consumers see the provider signal
            // first in the stream.
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

/// Split a string into emission chunks for mock streaming. We
/// preserve whitespace and newlines by attaching them to the
/// preceding word — so concatenating the yielded chunks
/// reproduces the original text byte-for-byte. Empty input yields
/// nothing.
fn mock_split_into_chunks(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in s.chars() {
        current.push(ch);
        if ch.is_whitespace() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
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
    async fn streaming_yields_word_by_word_and_assembles_to_full_reply() {
        use futures::StreamExt;
        let p = MockProvider;
        let stream = p
            .generate_reply_stream(ChatInput {
                session_id: "s1".into(),
                prompt: "hi".into(),
                ..ChatInput::default()
            })
            .await
            .expect("stream");
        let frames: Vec<StreamingChunk> = stream.map(|r| r.unwrap()).collect().await;
        // Pull text out for the lossless-reassembly check.
        let chunks: Vec<String> = frames
            .iter()
            .filter_map(|f| match f {
                StreamingChunk::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(chunks.len() >= 2, "expected multi-chunk reply: {chunks:?}");
        let assembled: String = chunks.join("");
        let full = p
            .generate_reply(ChatInput {
                session_id: "s1".into(),
                prompt: "hi".into(),
                ..ChatInput::default()
            })
            .await
            .unwrap()
            .text;
        assert_eq!(assembled, full);
        // RELIX-7.11 GAP 2: the mock provider emits a
        // terminal Usage frame after the last text chunk.
        let usage = frames
            .iter()
            .find_map(|f| match f {
                StreamingChunk::Usage(u) => Some(u.clone()),
                _ => None,
            })
            .expect("expected a terminal Usage frame from mock");
        assert!(usage.completion_tokens > 0, "mock usage must report tokens");
        assert_eq!(usage.model, "mock-1");
    }

    #[test]
    fn mock_split_preserves_whitespace_and_round_trips() {
        let s = "hello world\nthis is a test";
        let chunks = mock_split_into_chunks(s);
        assert_eq!(chunks.concat(), s);
        // First two chunks include their trailing whitespace.
        assert_eq!(chunks[0], "hello ");
        assert_eq!(chunks[1], "world\n");
        // Empty input yields nothing.
        assert!(mock_split_into_chunks("").is_empty());
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

    // ── RELIX-7.19 GAP 3: finish_reason + logprob populated

    #[tokio::test]
    async fn mock_provider_populates_finish_reason_and_logprob() {
        let p = MockProvider;
        let r = p
            .generate_reply(ChatInput {
                session_id: "s1".into(),
                prompt: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(r.finish_reason.as_deref(), Some("stop"));
        assert!(r.logprob.is_some());
    }

    #[tokio::test]
    async fn mock_provider_stream_emits_finish_reason_before_usage() {
        use futures::StreamExt;
        let p = MockProvider;
        let mut stream = p
            .generate_reply_stream(ChatInput {
                session_id: "s1".into(),
                prompt: "hi".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut chunks: Vec<StreamingChunk> = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.unwrap());
        }
        let last_text = chunks
            .iter()
            .rposition(|c| matches!(c, StreamingChunk::Text(_)))
            .expect("at least one text chunk");
        let fr_pos = chunks
            .iter()
            .position(|c| matches!(c, StreamingChunk::FinishReason(_)))
            .expect("FinishReason emitted");
        let usage_pos = chunks
            .iter()
            .position(|c| matches!(c, StreamingChunk::Usage(_)))
            .expect("Usage emitted");
        assert!(fr_pos > last_text, "FinishReason after last text");
        assert!(fr_pos < usage_pos, "FinishReason before Usage");
        match &chunks[fr_pos] {
            StreamingChunk::FinishReason(fr) => assert_eq!(fr, "stop"),
            _ => unreachable!(),
        }
    }
}
