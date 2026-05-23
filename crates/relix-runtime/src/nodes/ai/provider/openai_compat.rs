//! OpenAI-compatible provider — works against any backend that speaks
//! `POST {base_url}/chat/completions` with the OpenAI message-list shape.
//!
//! Concrete deployments:
//!
//! | provider name | typical base_url                    | api_key_env example |
//! |---------------|-------------------------------------|---------------------|
//! | `openai`      | `https://api.openai.com/v1`         | `OPENAI_API_KEY`    |
//! | `openrouter`  | `https://openrouter.ai/api/v1`      | `OPENROUTER_API_KEY`|
//! | `xai`         | `https://api.x.ai/v1`               | `XAI_API_KEY`       |
//! | `local`       | `http://localhost:11434/v1` (Ollama) | (unset / empty)    |
//!
//! Bearer auth header is added iff a key was loaded.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::{
    ChatInput, ChatOutput, ChatProvider, ChatStream, EmbedInput, EmbedOutput, ProviderEntry,
    ProviderError, TokenUsage, load_api_key,
};

const DEFAULT_EMBED_MODEL: &str = "text-embedding-3-small";

/// One instance per active OpenAI-compatible provider name.
pub struct OpenAICompatibleProvider {
    name: &'static str,
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    http: reqwest::Client,
}

impl OpenAICompatibleProvider {
    /// Build from a `[ai.providers.<name>]` entry. `name` is the static
    /// label the trait reports back to the handler / audit.
    pub fn from_entry(name: &'static str, entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let base_url = entry
            .base_url
            .as_ref()
            .ok_or_else(|| {
                ProviderError::Permanent(format!("[ai.providers.{name}] missing base_url"))
            })?
            .trim_end_matches('/')
            .to_string();
        let api_key = load_api_key(entry)?;
        let default_model = entry
            .default_model
            .clone()
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(entry.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Permanent(format!("http client: {e}")))?;
        Ok(Self {
            name,
            base_url,
            api_key,
            default_model,
            http,
        })
    }
}

#[async_trait]
impl ChatProvider for OpenAICompatibleProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };

        // Build the OpenAI-style messages array. Keep it simple in the
        // alpha: optional system + a single user turn that wraps the
        // history block in front of the new prompt. Typed turns land at
        // Gate 2 with the CDDL stdlib.
        let mut messages = Vec::with_capacity(2);
        if let Some(sys) = &input.system_prompt {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        let user_content = if input.history.trim().is_empty() {
            input.prompt.clone()
        } else {
            format!(
                "Recent conversation (session={s}):\n{h}\n\nNew message: {p}",
                s = input.session_id,
                h = input.history,
                p = input.prompt,
            )
        };
        messages.push(json!({ "role": "user", "content": user_content }));

        let mut body = json!({
            "model": model,
            "messages": messages,
        });
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = input.max_tokens {
            body["max_tokens"] = json!(m);
        }

        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|e| {
            let reason = crate::nodes::ai::classify_transport_failure(&e.to_string());
            tracing::warn!(
                provider = %self.name,
                failover.reason = %reason.label(),
                "ai.provider: transport failure"
            );
            ProviderError::Transient(format!(
                "{provider}: http [{label}]: {e}",
                provider = self.name,
                label = reason.label(),
            ))
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            ProviderError::Transient(format!("{provider}: read body: {e}", provider = self.name))
        })?;

        if !status.is_success() {
            // H1: classify the failure into a typed FailoverReason
            // BEFORE collapsing into Transient/Permanent. The label
            // lands in both the structured tracing field and the
            // error message itself so the bridge / dashboard can
            // surface "rate-limit" vs "context-overflow" vs
            // "model-not-found" without parsing free-form text.
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            tracing::warn!(
                provider = %self.name,
                http.status = status.as_u16(),
                failover.reason = %reason.label(),
                failover.category = ?reason.category(),
                "ai.provider: http failure"
            );
            let msg = format!(
                "{}: HTTP {status} [{label}]: {text}",
                self.name,
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            ProviderError::Permanent(format!("{provider}: parse: {e}", provider = self.name))
        })?;
        let reply_text = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ProviderError::Permanent(format!(
                    "{provider}: no choices[0].message.content in: {text}",
                    provider = self.name
                ))
            })?;
        let usage = parsed.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        });
        Ok(ChatOutput {
            text: reply_text,
            provider: self.name,
            model,
            usage,
        })
    }

    fn provider_name(&self) -> &'static str {
        self.name
    }

    /// Stream `delta.content` chunks from the upstream OpenAI-style
    /// `/v1/chat/completions` SSE response. The request body adds
    /// `stream: true`; the response body is a sequence of
    /// `data: <json>\n\n` frames terminated by `data: [DONE]`.
    /// Each yielded chunk is the `choices[0].delta.content`
    /// string from one SSE frame. Frames without `delta.content`
    /// (role-only header frames, finish_reason frames) are
    /// skipped silently. Transport / decode errors map to
    /// `ProviderError::Transient` / `Permanent` the same way
    /// `generate_reply` already does.
    async fn generate_reply_stream(&self, input: ChatInput) -> Result<ChatStream, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
        let mut messages = Vec::with_capacity(2);
        if let Some(sys) = &input.system_prompt {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        let user_content = if input.history.trim().is_empty() {
            input.prompt.clone()
        } else {
            format!(
                "Recent conversation (session={s}):\n{h}\n\nNew message: {p}",
                s = input.session_id,
                h = input.history,
                p = input.prompt,
            )
        };
        messages.push(json!({ "role": "user", "content": user_content }));
        let mut body = json!({
            "model":    model,
            "messages": messages,
            "stream":   true,
        });
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = input.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        let resp = req.send().await.map_err(|e| {
            ProviderError::Transient(format!(
                "{provider}: stream http: {e}",
                provider = self.name
            ))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            let msg = format!(
                "{}: HTTP {status} [{label}]: {text}",
                self.name,
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let provider_name = self.name;
        let byte_stream = resp.bytes_stream();
        let s = async_stream::stream! {
            use futures::StreamExt;
            let mut byte_stream = std::pin::pin!(byte_stream);
            let mut buf = String::new();
            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(ProviderError::Transient(format!(
                            "{provider_name}: stream read: {e}"
                        )));
                        return;
                    }
                };
                let chunk_str = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                buf.push_str(&chunk_str);
                // Frames end on a blank line (\n\n). Drain
                // complete frames from the front; the partial
                // tail stays in `buf` for the next byte chunk.
                while let Some(end) = buf.find("\n\n") {
                    let frame = buf[..end].to_string();
                    buf.drain(..end + 2);
                    for line in frame.lines() {
                        match parse_sse_line(line) {
                            SseLine::Delta(text) => yield Ok(text),
                            SseLine::Done => return,
                            SseLine::Skip => {}
                        }
                    }
                }
            }
        };
        Ok(Box::pin(s))
    }

    async fn generate_embeddings(&self, input: EmbedInput) -> Result<EmbedOutput, ProviderError> {
        let model = if input.model.is_empty() {
            DEFAULT_EMBED_MODEL.to_string()
        } else {
            input.model.clone()
        };
        if input.texts.is_empty() {
            return Ok(EmbedOutput {
                model,
                vectors: Vec::new(),
            });
        }

        let body = json!({
            "model": model,
            "input": input.texts,
        });
        let url = format!("{}/embeddings", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        let resp = req.send().await.map_err(|e| {
            ProviderError::Transient(format!(
                "{provider}: http embeddings: {e}",
                provider = self.name
            ))
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            ProviderError::Transient(format!(
                "{provider}: read embeddings body: {e}",
                provider = self.name
            ))
        })?;
        if !status.is_success() {
            let msg = format!("{}: HTTP {status} embeddings: {text}", self.name);
            return Err(if status.as_u16() == 429 || status.is_server_error() {
                ProviderError::Transient(msg)
            } else {
                ProviderError::Permanent(msg)
            });
        }
        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            ProviderError::Permanent(format!(
                "{provider}: parse embeddings: {e}",
                provider = self.name
            ))
        })?;
        let data = parsed
            .get("data")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                ProviderError::Permanent(format!(
                    "{provider}: no data[] in embeddings response: {text}",
                    provider = self.name
                ))
            })?;
        if data.len() != input.texts.len() {
            return Err(ProviderError::Permanent(format!(
                "{provider}: embeddings response had {got} vectors for {want} inputs",
                provider = self.name,
                got = data.len(),
                want = input.texts.len(),
            )));
        }
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(data.len());
        for (idx, item) in data.iter().enumerate() {
            let arr = item
                .get("embedding")
                .and_then(|v| v.as_array())
                .ok_or_else(|| {
                    ProviderError::Permanent(format!(
                        "{provider}: no embedding[] at data[{idx}]",
                        provider = self.name
                    ))
                })?;
            let mut v: Vec<f32> = Vec::with_capacity(arr.len());
            for x in arr {
                let f = x.as_f64().ok_or_else(|| {
                    ProviderError::Permanent(format!(
                        "{provider}: non-numeric embedding component at data[{idx}]",
                        provider = self.name
                    ))
                })? as f32;
                v.push(f);
            }
            vectors.push(v);
        }
        Ok(EmbedOutput { model, vectors })
    }
}

/// What a single SSE line means after parsing. `Delta` carries
/// one `choices[0].delta.content` token, `Done` is the upstream
/// `[DONE]` terminator, `Skip` is everything else — keep-alive
/// comments, role-only header frames, finish_reason frames.
enum SseLine {
    Delta(String),
    Done,
    Skip,
}

/// Parse a single line from the upstream `/v1/chat/completions`
/// SSE stream. Returns `Done` on `data: [DONE]`, `Delta(text)`
/// when the line carries a non-empty `choices[0].delta.content`,
/// `Skip` for every other shape (event lines, comments, role-only
/// header frames, malformed JSON).
fn parse_sse_line(line: &str) -> SseLine {
    let Some(payload) = line.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let payload = payload.trim();
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    if payload.is_empty() {
        return SseLine::Skip;
    }
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return SseLine::Skip,
    };
    match parsed
        .pointer("/choices/0/delta/content")
        .and_then(|v| v.as_str())
    {
        Some(text) if !text.is_empty() => SseLine::Delta(text.to_string()),
        _ => SseLine::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_line_extracts_delta_content() {
        let line =
            r#"data: {"choices":[{"delta":{"content":"Hello"},"index":0,"finish_reason":null}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta(s) => assert_eq!(s, "Hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn parse_sse_line_handles_done_terminator() {
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
        assert!(matches!(parse_sse_line("data:[DONE]"), SseLine::Done));
    }

    #[test]
    fn parse_sse_line_skips_role_only_header_frame() {
        // OpenAI's first frame typically carries just the role —
        // no content delta yet. We must skip it without yielding
        // an empty string to the consumer.
        let line =
            r#"data: {"choices":[{"delta":{"role":"assistant"},"index":0,"finish_reason":null}]}"#;
        assert!(matches!(parse_sse_line(line), SseLine::Skip));
    }

    #[test]
    fn parse_sse_line_skips_finish_reason_only_frame() {
        let line = r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#;
        assert!(matches!(parse_sse_line(line), SseLine::Skip));
    }

    #[test]
    fn parse_sse_line_skips_event_lines_and_comments() {
        assert!(matches!(parse_sse_line("event: chunk"), SseLine::Skip));
        assert!(matches!(parse_sse_line(": keep-alive"), SseLine::Skip));
        assert!(matches!(parse_sse_line(""), SseLine::Skip));
        assert!(matches!(parse_sse_line("data:"), SseLine::Skip));
    }

    #[test]
    fn parse_sse_line_skips_malformed_json() {
        assert!(matches!(
            parse_sse_line("data: {not really json"),
            SseLine::Skip
        ));
    }

    #[test]
    fn parse_sse_line_skips_empty_content_string() {
        let line = r#"data: {"choices":[{"delta":{"content":""},"index":0,"finish_reason":null}]}"#;
        assert!(matches!(parse_sse_line(line), SseLine::Skip));
    }

    #[test]
    fn missing_base_url_errors_clearly() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: None,
            default_model: None,
            timeout_secs: 30,
        };
        // `OpenAICompatibleProvider` does not impl Debug (it holds an HTTP
        // client), so we cannot {:?} the Ok branch — explicit match instead.
        match OpenAICompatibleProvider::from_entry("openai", &entry) {
            Ok(_) => panic!("expected permanent error, got Ok"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing base_url")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    #[test]
    fn provider_name_passthrough() {
        let entry = ProviderEntry {
            base_url: Some("http://localhost:11434/v1".into()),
            api_key_env: None,
            default_model: Some("llama3:8b".into()),
            timeout_secs: 30,
        };
        let p = match OpenAICompatibleProvider::from_entry("local", &entry) {
            Ok(p) => p,
            Err(e) => panic!("local should build: {e}"),
        };
        assert_eq!(p.provider_name(), "local");
        assert_eq!(p.default_model, "llama3:8b");
        assert!(p.api_key.is_none());
    }
}
