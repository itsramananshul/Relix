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
    AvailableModel, ChatInput, ChatOutput, ChatProvider, ChatStream, EmbedInput, EmbedOutput,
    ProviderEntry, ProviderError, StreamingChunk, StreamingUsage, TokenUsage, load_api_key,
};

const DEFAULT_EMBED_MODEL: &str = "text-embedding-3-small";

/// One instance per active OpenAI-compatible provider name.
pub struct OpenAICompatibleProvider {
    name: &'static str,
    base_url: String,
    /// SEC PART 2: Zeroizing wrapper; API key bytes wiped on
    /// drop. `None` means the provider was configured without
    /// a key (rare — typically a local Ollama).
    api_key: Option<zeroize::Zeroizing<String>>,
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
        let api_key = load_api_key(entry)?.map(zeroize::Zeroizing::new);
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
    /// GAP 16 §7.29 Model Name Resolution — hit the provider's
    /// `GET /models` endpoint and return the live catalogue.
    /// OpenAI / OpenRouter / xAI all expose this; Ollama does
    /// too (since v0.1.30). The bearer is sent when configured.
    async fn list_available_models(&self) -> Result<Vec<AvailableModel>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let mut req = self.http.get(&url);
        if let Some(key) = self.api_key.as_ref() {
            req = req.bearer_auth(key.as_str());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("{}: list_models: {e}", self.name)))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ProviderError::Permanent(format!(
                "{}: list_models HTTP {status}: {body}",
                self.name
            )));
        }
        parse_models_body(&body)
            .map_err(|e| ProviderError::Permanent(format!("{}: list_models: {e}", self.name)))
    }

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
            req = req.header("authorization", format!("Bearer {}", key.as_str()));
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
        // RELIX-7.19 GAP 3: extract finish_reason +
        // logprobs.content[*].logprob average from the parsed
        // response so the ConfidenceScorer's provider_signal
        // sub-score has real data to work with.
        let finish_reason = parsed
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
            .map(normalise_openai_finish_reason);
        let logprob = extract_openai_logprob(&parsed);
        Ok(ChatOutput {
            text: reply_text,
            provider: self.name,
            model,
            usage,
            finish_reason,
            logprob,
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
            // RELIX-7.11 GAP 2 — every OpenAI-compatible
            // streaming response now requests an extra final
            // frame carrying `usage` so the per-call token
            // counts can land on the metric row.
            "stream_options": { "include_usage": true },
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
            req = req.header("authorization", format!("Bearer {}", key.as_str()));
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
            // Track the resolved model id reported by the
            // upstream so the terminal `Usage` frame carries
            // the same string downstream pricing looks up.
            // OpenAI repeats the `model` field on every
            // streaming frame; we capture the latest.
            let mut observed_model: Option<String> = None;
            // The final `Usage` frame (RELIX-7.11 GAP 2) is
            // emitted once we see `[DONE]` OR a frame whose
            // `usage` field is populated (OpenAI sends the
            // usage on the second-to-last frame when
            // `stream_options.include_usage = true`).
            let mut pending_usage: Option<StreamingUsage> = None;
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
                            SseLine::Delta { text, model } => {
                                if let Some(m) = model
                                    && observed_model.as_deref() != Some(&m)
                                {
                                    observed_model = Some(m);
                                }
                                yield Ok(StreamingChunk::Text(text));
                            }
                            SseLine::Usage { prompt, completion, model } => {
                                if let Some(m) = model {
                                    observed_model = Some(m);
                                }
                                pending_usage = Some(StreamingUsage {
                                    prompt_tokens: prompt,
                                    completion_tokens: completion,
                                    model: observed_model.clone().unwrap_or_default(),
                                });
                            }
                            SseLine::FinishReason(fr) => {
                                // RELIX-7.19 GAP 3: emit
                                // BEFORE the Usage frame so
                                // downstream consumers see the
                                // provider signal first.
                                yield Ok(StreamingChunk::FinishReason(fr));
                            }
                            SseLine::Done => {
                                if let Some(u) = pending_usage.take() {
                                    yield Ok(StreamingChunk::Usage(u));
                                }
                                return;
                            }
                            SseLine::Skip => {}
                        }
                    }
                }
            }
            // Stream closed without an explicit `[DONE]` — emit
            // any pending usage payload we already saw.
            if let Some(u) = pending_usage {
                yield Ok(StreamingChunk::Usage(u));
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
            req = req.header("authorization", format!("Bearer {}", key.as_str()));
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

/// What a single SSE line means after parsing.
/// - `Delta` carries one `choices[0].delta.content` token and
///   (when present) the upstream's `model` field.
/// - `Usage` carries the final `usage` payload OpenAI emits
///   when `stream_options.include_usage = true`. The
///   `choices` array on this frame is empty, so it does NOT
///   produce a `Delta`.
/// - `Done` is the upstream `[DONE]` terminator.
/// - `FinishReason` carries the upstream
///   `choices[0].finish_reason` from the last delta frame.
///   RELIX-7.19 GAP 3 added this so streaming producers can
///   forward the same provider signal the non-streaming path
///   already does.
/// - `Skip` is everything else (event lines, comments,
///   role-only header frames, malformed JSON).
enum SseLine {
    Delta {
        text: String,
        model: Option<String>,
    },
    Usage {
        prompt: u32,
        completion: u32,
        model: Option<String>,
    },
    FinishReason(String),
    Done,
    Skip,
}

/// GAP 16 §7.29: parse the upstream `/models` JSON body into a
/// list of [`AvailableModel`]. Pulled out as a free function so
/// tests can exercise the shape variants without spinning up an
/// HTTP server.
fn parse_models_body(body: &str) -> Result<Vec<AvailableModel>, String> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let entries = parsed
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| parsed.as_array().cloned())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Some(id) = e.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        out.push(AvailableModel {
            id: id.to_string(),
            label: e.get("name").and_then(|x| x.as_str()).map(str::to_string),
            context_window: e
                .get("context_length")
                .and_then(|x| x.as_u64())
                .map(|n| n.min(u32::MAX as u64) as u32),
            input_price_micros_per_mtoken: e
                .pointer("/pricing/prompt")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * 1_000_000.0 * 1_000_000.0) as u64),
            output_price_micros_per_mtoken: e
                .pointer("/pricing/completion")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .map(|p| (p * 1_000_000.0 * 1_000_000.0) as u64),
        });
    }
    Ok(out)
}

/// Parse a single line from the upstream `/v1/chat/completions`
/// SSE stream.
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
    let model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // RELIX-7.11 GAP 2: detect the include_usage final frame.
    // OpenAI sends a frame with `usage: {…}` and no choices on
    // stream close. Local OpenAI-compatible servers (Ollama,
    // vLLM) also emit this when configured.
    if let Some(usage) = parsed.get("usage")
        && !usage.is_null()
    {
        let prompt = usage
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let completion = usage
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        // Some upstreams send usage on a frame with a final
        // delta — return Usage if there's no text on it.
        let has_text = parsed
            .pointer("/choices/0/delta/content")
            .and_then(|v| v.as_str())
            .is_some_and(|t| !t.is_empty());
        if !has_text {
            return SseLine::Usage {
                prompt,
                completion,
                model,
            };
        }
    }
    match parsed
        .pointer("/choices/0/delta/content")
        .and_then(|v| v.as_str())
    {
        Some(text) if !text.is_empty() => SseLine::Delta {
            text: text.to_string(),
            model,
        },
        _ => {
            // RELIX-7.19 GAP 3: a frame with no delta.content
            // but a populated finish_reason is the terminal
            // "[finish_reason: ...]" marker from OpenAI. Emit
            // it so the streaming wrapper can pass it through
            // as `StreamingChunk::FinishReason`.
            if let Some(fr) = parsed
                .pointer("/choices/0/finish_reason")
                .and_then(|v| v.as_str())
                && !fr.is_empty()
            {
                return SseLine::FinishReason(normalise_openai_finish_reason(fr));
            }
            SseLine::Skip
        }
    }
}

/// RELIX-7.19 GAP 3: normalise OpenAI `finish_reason` strings
/// into the small vocabulary the ConfidenceScorer uses.
/// `stop` and `length` pass through; `content_filter` keeps
/// its name; `tool_calls` maps to `tool_use` to match the
/// Anthropic normalisation; anything else becomes `"other"`.
pub(crate) fn normalise_openai_finish_reason(v: &str) -> String {
    match v {
        "stop" => "stop".to_string(),
        "length" => "length".to_string(),
        "content_filter" => "content_filter".to_string(),
        "tool_calls" | "function_call" => "tool_use".to_string(),
        other => other.to_string(),
    }
}

/// RELIX-7.19 GAP 3: average the per-token logprobs from an
/// OpenAI-compatible response. Returns `None` when no
/// `logprobs.content` array is present or every entry is
/// missing the field. The result is the raw average log-
/// probability — the scorer applies `exp(.)` clamped to
/// `[0, 1]` to convert it to a probability.
pub(crate) fn extract_openai_logprob(parsed: &serde_json::Value) -> Option<f32> {
    let arr = parsed
        .pointer("/choices/0/logprobs/content")
        .and_then(|v| v.as_array())?;
    let mut sum = 0.0_f64;
    let mut count = 0_usize;
    for entry in arr {
        if let Some(lp) = entry.get("logprob").and_then(|v| v.as_f64()) {
            sum += lp;
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    Some((sum / count as f64) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_models_body_handles_openai_wrapped_shape() {
        let body = r#"{
            "object": "list",
            "data": [
                { "id": "gpt-4o-mini" },
                { "id": "o1", "context_length": 128000 }
            ]
        }"#;
        let models = parse_models_body(body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o-mini");
        assert_eq!(models[1].id, "o1");
        assert_eq!(models[1].context_window, Some(128_000));
    }

    #[test]
    fn parse_models_body_handles_bare_array_shape() {
        let body = r#"[
            { "id": "claude-haiku-4-5" },
            { "id": "claude-opus-4" }
        ]"#;
        let models = parse_models_body(body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-haiku-4-5");
    }

    #[test]
    fn parse_models_body_drops_entries_without_an_id() {
        let body = r#"{ "data": [ {}, { "id": "good" }, { "name": "no id" } ] }"#;
        let models = parse_models_body(body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "good");
    }

    #[test]
    fn parse_models_body_parses_openrouter_pricing_shape() {
        let body = r#"{
            "data": [{
                "id": "openrouter/claude-sonnet-4",
                "context_length": 200000,
                "pricing": { "prompt": "0.000003", "completion": "0.000015" }
            }]
        }"#;
        let models = parse_models_body(body).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_window, Some(200_000));
        assert!(models[0].input_price_micros_per_mtoken.is_some());
        assert!(models[0].output_price_micros_per_mtoken.is_some());
    }

    #[test]
    fn parse_sse_line_extracts_delta_content() {
        let line =
            r#"data: {"choices":[{"delta":{"content":"Hello"},"index":0,"finish_reason":null}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta { text, .. } => assert_eq!(text, "Hello"),
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn parse_sse_line_extracts_model_from_delta_frame_when_present() {
        let line = r#"data: {"model":"gpt-4o-mini-2024-07-18","choices":[{"delta":{"content":"Hi"},"index":0,"finish_reason":null}]}"#;
        match parse_sse_line(line) {
            SseLine::Delta { text, model } => {
                assert_eq!(text, "Hi");
                assert_eq!(model.as_deref(), Some("gpt-4o-mini-2024-07-18"));
            }
            _ => panic!("expected Delta"),
        }
    }

    #[test]
    fn parse_sse_line_extracts_usage_from_final_frame() {
        // OpenAI's stream_options.include_usage tail frame:
        // empty `choices` (or no choices array) + populated
        // `usage`.
        let line = r#"data: {"model":"gpt-4o-mini","choices":[],"usage":{"prompt_tokens":42,"completion_tokens":17,"total_tokens":59}}"#;
        match parse_sse_line(line) {
            SseLine::Usage {
                prompt,
                completion,
                model,
            } => {
                assert_eq!(prompt, 42);
                assert_eq!(completion, 17);
                assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
            }
            _ => panic!("expected Usage"),
        }
    }

    #[test]
    fn parse_sse_line_emits_delta_when_text_and_usage_both_present_on_same_frame() {
        // Defence-in-depth: if a future upstream sends usage
        // on a frame that also carries text, we still yield the
        // text (no buffering regression). The usage frame
        // arrives separately at stream close.
        let line = r#"data: {"choices":[{"delta":{"content":"x"},"index":0}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        match parse_sse_line(line) {
            SseLine::Delta { text, .. } => assert_eq!(text, "x"),
            other => panic!("expected Delta, got {}", std::any::type_name_of_val(&other)),
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
    fn parse_sse_line_finish_reason_only_frame_emits_finish_reason_post_7_19() {
        // RELIX-7.19 GAP 3 changed the behaviour: a frame with
        // no `delta.content` but a populated `finish_reason`
        // now emits `SseLine::FinishReason(...)` so the
        // streaming wrapper can forward it as
        // `StreamingChunk::FinishReason`. Previously this was
        // `SseLine::Skip`.
        let line = r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#;
        match parse_sse_line(line) {
            SseLine::FinishReason(fr) => assert_eq!(fr, "stop"),
            _ => panic!("expected FinishReason"),
        }
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

    // ── RELIX-7.19 GAP 3: finish_reason + logprob extraction

    #[test]
    fn normalise_openai_finish_reason_maps_documented_values() {
        assert_eq!(normalise_openai_finish_reason("stop"), "stop");
        assert_eq!(normalise_openai_finish_reason("length"), "length");
        assert_eq!(
            normalise_openai_finish_reason("content_filter"),
            "content_filter"
        );
        assert_eq!(normalise_openai_finish_reason("tool_calls"), "tool_use");
        assert_eq!(normalise_openai_finish_reason("function_call"), "tool_use");
        assert_eq!(normalise_openai_finish_reason("unknown"), "unknown");
    }

    #[test]
    fn extract_openai_logprob_averages_content_logprobs() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"choices":[{"logprobs":{"content":[
                {"token":"hello","logprob":-0.10},
                {"token":"world","logprob":-0.20},
                {"token":"!","logprob":-0.30}
            ]}}]}"#,
        )
        .unwrap();
        let lp = extract_openai_logprob(&v).expect("logprob present");
        // Average of -0.10, -0.20, -0.30 = -0.20.
        assert!((lp - (-0.20_f32)).abs() < 1e-5, "got {lp}");
    }

    #[test]
    fn extract_openai_logprob_returns_none_when_no_content_array() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert!(extract_openai_logprob(&v).is_none());
    }

    #[test]
    fn parse_sse_line_emits_finish_reason_when_present_and_no_delta() {
        let line = r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}]}"#;
        match parse_sse_line(line) {
            SseLine::FinishReason(fr) => assert_eq!(fr, "stop"),
            other => panic!(
                "expected FinishReason, got {other:?}",
                other = match other {
                    SseLine::Skip => "Skip",
                    SseLine::Done => "Done",
                    SseLine::Delta { .. } => "Delta",
                    SseLine::Usage { .. } => "Usage",
                    SseLine::FinishReason(_) => "FinishReason",
                }
            ),
        }
    }

    #[test]
    fn parse_sse_line_emits_normalised_finish_reason_for_length() {
        let line = r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"length"}]}"#;
        match parse_sse_line(line) {
            SseLine::FinishReason(fr) => assert_eq!(fr, "length"),
            _ => panic!("expected FinishReason"),
        }
    }
}
