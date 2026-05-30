//! Google Gemini provider.
//!
//! Talks to the public `generativelanguage.googleapis.com` REST API.
//! Distinct from the OpenAI-compatible path because Gemini uses:
//!
//! - a path-embedded model + action (`/v1beta/models/{model}:generateContent`),
//! - query-parameter / header API-key auth (no Bearer),
//! - a `contents[].role` of `user`/`model` (not `user`/`assistant`),
//! - a response shape of `candidates[0].content.parts[].text`,
//! - and an `alt=sse` streaming variant on the `:streamGenerateContent`
//!   endpoint that emits whole-JSON SSE events (each `data:` frame is a
//!   complete `GenerateContentResponse`, NOT a delta — we emit the
//!   incremental text since we last saw a frame).
//!
//! ## Multi-turn role mapping
//!
//! Relix's `ChatInput.history` is a free-form newline-separated blob the
//! memory peer renders as `role: body`. We try to parse that blob back
//! into structured turns and emit them as alternating `user`/`model`
//! entries so Gemini sees the conversation the way it expects. On a
//! parse failure we fall back to inlining the history block in the
//! user message (same as openai_compat / anthropic) so the response
//! quality only degrades — it never errors.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::{
    ChatInput, ChatOutput, ChatProvider, ChatStream, ProviderEntry, ProviderError, StreamingChunk,
    StreamingUsage, TokenUsage, load_api_key,
};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MODEL: &str = "gemini-2.0-flash";

pub struct GeminiProvider {
    base_url: String,
    /// SEC PART 2: Zeroizing wrapper — API key bytes are
    /// wiped from the heap when the provider is dropped.
    api_key: zeroize::Zeroizing<String>,
    default_model: String,
    http: reqwest::Client,
}

impl GeminiProvider {
    pub fn from_entry(entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let api_key = load_api_key(entry)?.ok_or_else(|| {
            ProviderError::Permanent(
                "[ai.providers.gemini] missing api_key_env — set api_key_env to the env var \
                 holding the key (e.g. \"GEMINI_API_KEY\")"
                    .into(),
            )
        })?;
        let base_url = entry
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let default_model = entry
            .default_model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(entry.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Permanent(format!("gemini: http client: {e}")))?;
        Ok(Self {
            base_url,
            api_key: zeroize::Zeroizing::new(api_key),
            default_model,
            http,
        })
    }
}

#[async_trait]
impl ChatProvider for GeminiProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
        let body = build_request_body(&input);
        let url = format!("{}/v1beta/models/{}:generateContent", self.base_url, model);

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("x-goog-api-key", self.api_key.as_str())
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| {
                let reason = crate::nodes::ai::classify_transport_failure(&e.to_string());
                tracing::warn!(
                    provider = "gemini",
                    failover.reason = %reason.label(),
                    "ai.provider: transport failure"
                );
                ProviderError::Transient(format!(
                    "gemini: http [{label}]: {e}",
                    label = reason.label(),
                ))
            })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transient(format!("gemini: read body: {e}")))?;
        if !status.is_success() {
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            tracing::warn!(
                provider = "gemini",
                http.status = status.as_u16(),
                failover.reason = %reason.label(),
                failover.category = ?reason.category(),
                "ai.provider: http failure"
            );
            let msg = format!(
                "gemini: HTTP {status} [{label}]: {text}",
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("gemini: parse: {e}")))?;
        let reply = extract_text(&parsed).ok_or_else(|| {
            ProviderError::Permanent(format!(
                "gemini: no candidates[0].content.parts[].text in: {text}"
            ))
        })?;
        let usage = extract_usage(&parsed);
        // RELIX-7.19 GAP 3: candidates[0].finishReason →
        // normalised finish_reason. Gemini doesn't expose
        // logprobs on the standard API.
        let finish_reason = extract_finish_reason(&parsed);
        Ok(ChatOutput {
            text: reply,
            provider: "gemini",
            model,
            usage,
            finish_reason,
            logprob: None,
        })
    }

    fn provider_name(&self) -> &'static str {
        "gemini"
    }

    /// Stream incremental text from `:streamGenerateContent?alt=sse`.
    /// Gemini's SSE frames are full `GenerateContentResponse` JSON
    /// objects (each subsequent frame's text typically grows past the
    /// previous one), so we extract whatever new text appeared since
    /// the last frame. Frames without a `candidates[0].content.parts`
    /// payload (e.g. final usage metadata) are skipped.
    async fn generate_reply_stream(&self, input: ChatInput) -> Result<ChatStream, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
        let body = build_request_body(&input);
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model
        );

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("x-goog-api-key", self.api_key.as_str())
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("gemini: stream http: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            let msg = format!(
                "gemini: HTTP {status} [{label}]: {text}",
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let model_for_usage = model.clone();
        let byte_stream = resp.bytes_stream();
        let s = async_stream::stream! {
            use futures::StreamExt;
            let mut byte_stream = std::pin::pin!(byte_stream);
            let mut buf = String::new();
            // Track the cumulative text seen so we can yield
            // incremental deltas even when the upstream emits
            // "running total" frames. Kept in the generator's
            // local frame (not a Mutex) so the stream future
            // stays `Send`.
            let mut last_emitted = String::new();
            // RELIX-7.11 GAP 2: Gemini emits `usageMetadata` on
            // every frame as a running total. We capture the
            // latest observed value and emit a single
            // `StreamingChunk::Usage` once the stream closes
            // (or `[DONE]` arrives).
            let mut pending_usage: Option<StreamingUsage> = None;
            // RELIX-7.19 GAP 3: capture the finishReason from
            // the LAST frame that carried one and emit it
            // BEFORE the Usage frame at stream end.
            let mut pending_finish: Option<String> = None;
            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(ProviderError::Transient(format!(
                            "gemini: stream read: {e}"
                        )));
                        return;
                    }
                };
                let chunk_str = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                buf.push_str(&chunk_str);
                while let Some(end) = buf.find("\n\n") {
                    let frame = buf[..end].to_string();
                    buf.drain(..end + 2);
                    for line in frame.lines() {
                        let payload = match line.strip_prefix("data:") {
                            Some(p) => p.trim(),
                            None => continue,
                        };
                        if payload.is_empty() {
                            continue;
                        }
                        if payload == "[DONE]" {
                            if let Some(fr) = pending_finish.take() {
                                yield Ok(StreamingChunk::FinishReason(fr));
                            }
                            if let Some(u) = pending_usage.take() {
                                yield Ok(StreamingChunk::Usage(u));
                            }
                            return;
                        }
                        let parsed: serde_json::Value =
                            match serde_json::from_str(payload) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        // Extract running-total usage on every frame.
                        if let Some(u) = extract_usage(&parsed) {
                            pending_usage = Some(StreamingUsage {
                                prompt_tokens: u.prompt_tokens,
                                completion_tokens: u.completion_tokens,
                                model: model_for_usage.clone(),
                            });
                        }
                        // RELIX-7.19 GAP 3: capture finish
                        // reason from any frame that has it.
                        if let Some(fr) = extract_finish_reason(&parsed) {
                            pending_finish = Some(fr);
                        }
                        if let Some(text) = extract_text(&parsed)
                            && !text.is_empty()
                        {
                            if let Some(suffix) =
                                text.strip_prefix(last_emitted.as_str())
                            {
                                if !suffix.is_empty() {
                                    last_emitted = text.clone();
                                    yield Ok(StreamingChunk::Text(suffix.to_string()));
                                }
                            } else {
                                last_emitted = text.clone();
                                yield Ok(StreamingChunk::Text(text));
                            }
                        }
                    }
                }
            }
            // Stream closed cleanly without `[DONE]` — flush
            // whatever finish reason + usage we observed.
            if let Some(fr) = pending_finish {
                yield Ok(StreamingChunk::FinishReason(fr));
            }
            if let Some(u) = pending_usage {
                yield Ok(StreamingChunk::Usage(u));
            }
        };
        Ok(Box::pin(s))
    }
}

/// Render `ChatInput` into the Gemini request body. Public so unit
/// tests can verify the request shape without standing up the HTTP
/// client.
pub(crate) fn build_request_body(input: &ChatInput) -> serde_json::Value {
    let contents = build_contents(input);
    let mut body = json!({ "contents": contents });
    let mut gen_config = serde_json::Map::new();
    gen_config.insert(
        "temperature".into(),
        json!(input.temperature.unwrap_or(0.7)),
    );
    gen_config.insert(
        "maxOutputTokens".into(),
        json!(input.max_tokens.unwrap_or(2048)),
    );
    body["generationConfig"] = serde_json::Value::Object(gen_config);
    if let Some(sys) = &input.system_prompt {
        body["systemInstruction"] = json!({
            "role": "system",
            "parts": [{ "text": sys }],
        });
    }
    body
}

/// Build the `contents` array. When the history blob is non-empty
/// we try to parse it back into alternating `role: body` turns and
/// emit them as Gemini-shaped contents. On parse failure we fall
/// back to a single-turn user message that inlines the history (the
/// same posture openai_compat + anthropic use), so a malformed
/// history degrades quality without breaking the call.
fn build_contents(input: &ChatInput) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    if let Some(turns) = parse_history_turns(&input.history) {
        for turn in turns {
            out.push(json!({
                "role":  turn.role,
                "parts": [{ "text": turn.text }],
            }));
        }
    } else if !input.history.trim().is_empty() {
        // History present but unparseable — inline as part of the
        // user message instead of dropping it.
        out.push(json!({
            "role":  "user",
            "parts": [{
                "text": format!(
                    "Recent conversation (session={s}):\n{h}",
                    s = input.session_id,
                    h = input.history,
                )
            }],
        }));
    }
    out.push(json!({
        "role":  "user",
        "parts": [{ "text": input.prompt }],
    }));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Turn {
    role: &'static str,
    text: String,
}

/// Parse the memory peer's `role: body` history blob into Gemini
/// turns. Returns `None` when the blob doesn't look like turn lines
/// at all so the caller can fall back to the inline form.
///
/// Recognised line shapes:
/// - `user: ...`        → role=user
/// - `assistant: ...`   → role=model
/// - `model: ...`       → role=model
/// - `system: ...`      → role=user  (Gemini has no system in contents[])
///
/// Multi-line bodies are joined; continuation lines (no `role:` prefix)
/// extend the most recent turn.
fn parse_history_turns(history: &str) -> Option<Vec<Turn>> {
    let h = history.trim();
    if h.is_empty() {
        return None;
    }
    let mut turns: Vec<Turn> = Vec::new();
    let mut saw_role_line = false;
    for line in h.lines() {
        if let Some((role_raw, body)) = split_role(line) {
            saw_role_line = true;
            let role = match role_raw.to_ascii_lowercase().as_str() {
                "user" => "user",
                "assistant" | "model" => "model",
                "system" => "user",
                _ => {
                    // Unknown role label — treat the whole blob as
                    // unparseable so we degrade to the inline form.
                    return None;
                }
            };
            turns.push(Turn {
                role,
                text: body.trim().to_string(),
            });
        } else if let Some(last) = turns.last_mut() {
            // Continuation line — append to the previous turn's
            // body so multi-paragraph turns survive the round trip.
            if !last.text.is_empty() {
                last.text.push('\n');
            }
            last.text.push_str(line);
        } else {
            // Continuation before any role line — bail out.
            return None;
        }
    }
    if !saw_role_line || turns.is_empty() {
        return None;
    }
    Some(turns)
}

fn split_role(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let idx = trimmed.find(':')?;
    let role = &trimmed[..idx];
    if role.is_empty() || role.chars().any(|c| !c.is_ascii_alphabetic()) {
        return None;
    }
    let body = &trimmed[idx + 1..];
    Some((role, body))
}

/// RELIX-7.19 GAP 3: extract Gemini's `finishReason` from a
/// parsed response (non-streaming OR per-frame for the
/// streaming path) and normalise it to the ConfidenceScorer
/// vocabulary. Mapping per spec:
///   STOP → "stop"
///   MAX_TOKENS → "length"
///   SAFETY → "content_filter"
///   RECITATION → "content_filter"
///   anything else → "other"
fn extract_finish_reason(parsed: &serde_json::Value) -> Option<String> {
    let raw = parsed
        .pointer("/candidates/0/finishReason")
        .and_then(|v| v.as_str())?;
    Some(normalise_gemini_finish_reason(raw))
}

pub(crate) fn normalise_gemini_finish_reason(v: &str) -> String {
    match v {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" | "RECITATION" => "content_filter".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

/// Extract the concatenated text from a Gemini response. Joins
/// every `text` field in `candidates[0].content.parts[]` so a
/// reply split into multiple parts comes back as a single string.
fn extract_text(parsed: &serde_json::Value) -> Option<String> {
    let parts = parsed
        .pointer("/candidates/0/content/parts")
        .and_then(|v| v.as_array())?;
    let mut out = String::new();
    for part in parts {
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Pull `usageMetadata` into the shared `TokenUsage` shape. Gemini
/// names them `promptTokenCount` / `candidatesTokenCount` /
/// `totalTokenCount`. `None` when the field is absent (e.g. on a
/// streamed final-frame that doesn't carry usage).
fn extract_usage(parsed: &serde_json::Value) -> Option<TokenUsage> {
    let u = parsed.get("usageMetadata")?;
    Some(TokenUsage {
        prompt_tokens: u
            .get("promptTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: u
            .get("candidatesTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: u
            .get("totalTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    })
}

/// Used only by tests; the streaming hot path parses inline so
/// it can capture both text and usage on the same frame.
#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
enum SseLine {
    Chunk(String),
    Done,
    Skip,
}

/// Parse a single line from the upstream `streamGenerateContent`
/// SSE response. Each `data:` frame is a complete
/// `GenerateContentResponse` JSON object — we return whatever text
/// it carries; the caller diffs against the cumulative emitted
/// string to compute deltas.
#[cfg_attr(not(test), allow(dead_code))]
fn parse_sse_line(line: &str) -> SseLine {
    let Some(payload) = line.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return SseLine::Skip;
    }
    if payload == "[DONE]" {
        return SseLine::Done;
    }
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return SseLine::Skip,
    };
    match extract_text(&parsed) {
        Some(t) if !t.is_empty() => SseLine::Chunk(t),
        _ => SseLine::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_basic(prompt: &str) -> ChatInput {
        ChatInput {
            prompt: prompt.into(),
            ..ChatInput::default()
        }
    }

    // ── Request body shape ───────────────────────────────────

    #[test]
    fn body_basic_user_only_emits_single_user_content() {
        let b = build_request_body(&input_basic("Hello"));
        let contents = b.get("contents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(
            contents[0].pointer("/role").and_then(|v| v.as_str()),
            Some("user")
        );
        assert_eq!(
            contents[0]
                .pointer("/parts/0/text")
                .and_then(|v| v.as_str()),
            Some("Hello")
        );
    }

    #[test]
    fn body_has_generation_config_with_defaults() {
        let b = build_request_body(&input_basic("hi"));
        // f32 → f64 round-trip via serde_json widens the precision,
        // so compare with an epsilon. 0.7 ≈ 0.6999999880790710 in
        // f32; serde_json renders that as the widened f64.
        let temp = b
            .pointer("/generationConfig/temperature")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert!((temp - 0.7).abs() < 1e-5, "got {temp}");
        assert_eq!(
            b.pointer("/generationConfig/maxOutputTokens")
                .and_then(|v| v.as_u64()),
            Some(2048)
        );
    }

    #[test]
    fn body_respects_caller_temperature_and_max_tokens() {
        let mut input = input_basic("hi");
        input.temperature = Some(0.1);
        input.max_tokens = Some(64);
        let b = build_request_body(&input);
        let temp = b
            .pointer("/generationConfig/temperature")
            .and_then(|v| v.as_f64())
            .unwrap();
        assert!((temp - 0.1).abs() < 1e-5, "got {temp}");
        assert_eq!(
            b.pointer("/generationConfig/maxOutputTokens")
                .and_then(|v| v.as_u64()),
            Some(64)
        );
    }

    #[test]
    fn body_includes_system_instruction_when_present() {
        let mut input = input_basic("hi");
        input.system_prompt = Some("You are a pirate.".into());
        let b = build_request_body(&input);
        assert_eq!(
            b.pointer("/systemInstruction/parts/0/text")
                .and_then(|v| v.as_str()),
            Some("You are a pirate.")
        );
    }

    // ── Multi-turn history mapping ───────────────────────────

    #[test]
    fn history_maps_to_alternating_user_model_contents() {
        let mut input = input_basic("how are you?");
        input.history = "user: hi\nassistant: hello\nuser: ok\nassistant: cool".into();
        let b = build_request_body(&input);
        let contents = b.get("contents").and_then(|v| v.as_array()).unwrap();
        // 4 history turns + 1 new prompt
        assert_eq!(contents.len(), 5);
        // role alternation as Gemini expects
        let roles: Vec<&str> = contents
            .iter()
            .map(|c| c.get("role").and_then(|v| v.as_str()).unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["user", "model", "user", "model", "user"]);
        assert_eq!(
            contents[1]
                .pointer("/parts/0/text")
                .and_then(|v| v.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn history_model_role_label_maps_to_model() {
        // Some memory backends render the assistant turn as `model:`
        // already; we should accept either spelling.
        let mut input = input_basic("hi again");
        input.history = "user: hi\nmodel: hey".into();
        let b = build_request_body(&input);
        let contents = b.get("contents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            contents[1].get("role").and_then(|v| v.as_str()),
            Some("model")
        );
    }

    #[test]
    fn history_with_continuation_lines_joins_into_prior_turn() {
        let mut input = input_basic("now what?");
        input.history = "user: here is a long\nmulti-line question\nassistant: short answer".into();
        let b = build_request_body(&input);
        let contents = b.get("contents").and_then(|v| v.as_array()).unwrap();
        let user_text = contents[0]
            .pointer("/parts/0/text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(user_text.contains("here is a long"));
        assert!(user_text.contains("multi-line question"));
    }

    #[test]
    fn unparseable_history_falls_back_to_inline_user_message() {
        let mut input = input_basic("the question");
        input.session_id = "sess-1".into();
        input.history = "this isn't role-shaped at all".into();
        let b = build_request_body(&input);
        let contents = b.get("contents").and_then(|v| v.as_array()).unwrap();
        // Two messages: inline history + new prompt.
        assert_eq!(contents.len(), 2);
        let first = contents[0]
            .pointer("/parts/0/text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(first.contains("session=sess-1"));
        assert!(first.contains("this isn't role-shaped"));
    }

    // ── Response parsing ─────────────────────────────────────

    #[test]
    fn extract_text_joins_multiple_parts() {
        let body = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        { "text": "Hello " },
                        { "text": "world." },
                    ],
                    "role": "model",
                }
            }]
        });
        assert_eq!(extract_text(&body).as_deref(), Some("Hello world."));
    }

    #[test]
    fn extract_text_returns_none_when_parts_missing() {
        let body = serde_json::json!({ "candidates": [] });
        assert!(extract_text(&body).is_none());
    }

    #[test]
    fn extract_usage_reads_gemini_field_names() {
        let body = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 20,
                "totalTokenCount": 30,
            }
        });
        let u = extract_usage(&body).unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 20);
        assert_eq!(u.total_tokens, 30);
    }

    // ── Streaming chunk parsing ──────────────────────────────

    #[test]
    fn parse_sse_chunk_extracts_running_text() {
        let line = r#"data: {"candidates":[{"content":{"parts":[{"text":"Hi"}],"role":"model"}}]}"#;
        match parse_sse_line(line) {
            SseLine::Chunk(s) => assert_eq!(s, "Hi"),
            other => panic!("expected Chunk, got {other:?}"),
        }
    }

    #[test]
    fn parse_sse_skips_empty_and_non_data_lines() {
        assert!(matches!(parse_sse_line(""), SseLine::Skip));
        assert!(matches!(parse_sse_line(": keep-alive"), SseLine::Skip));
        assert!(matches!(parse_sse_line("event: msg"), SseLine::Skip));
        assert!(matches!(parse_sse_line("data:"), SseLine::Skip));
    }

    #[test]
    fn parse_sse_handles_done_terminator() {
        assert!(matches!(parse_sse_line("data: [DONE]"), SseLine::Done));
    }

    #[test]
    fn parse_sse_skips_malformed_json() {
        assert!(matches!(parse_sse_line("data: {not json"), SseLine::Skip));
    }

    #[test]
    fn parse_sse_skips_frame_without_text() {
        // Usage-metadata-only final frame.
        let line = r#"data: {"usageMetadata":{"promptTokenCount":5}}"#;
        assert!(matches!(parse_sse_line(line), SseLine::Skip));
    }

    // ── Misconfiguration ─────────────────────────────────────

    #[test]
    fn missing_api_key_env_errors_with_hint() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: None,
            default_model: None,
            timeout_secs: 30,
        };
        match GeminiProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing api_key_env")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    #[test]
    fn named_env_var_must_be_set() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: Some("RELIX_TEST_ABSOLUTELY_MISSING_GEMINI_42".into()),
            default_model: None,
            timeout_secs: 30,
        };
        match GeminiProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing provider key")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    // ── End-to-end with a mock HTTP server ───────────────────

    /// Spin up a one-shot TCP listener that returns a canned HTTP
    /// response. Used to verify the request shape + response
    /// parsing without depending on a third-party mock crate.
    async fn one_shot_server(canned_response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // Read the request — drain headers + body until we
            // either see the end of the body (Content-Length
            // satisfied) or 64 KB, whichever comes first.
            let mut buf = vec![0u8; 65536];
            let mut total = 0usize;
            loop {
                let n = sock.read(&mut buf[total..]).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                total += n;
                // Best-effort: stop once we've read the headers +
                // a presumably-small JSON body.
                if let Some(headers_end) = std::str::from_utf8(&buf[..total])
                    .ok()
                    .and_then(|s| s.find("\r\n\r\n"))
                {
                    // Pull Content-Length out and read at least
                    // that many body bytes.
                    let headers = std::str::from_utf8(&buf[..headers_end]).unwrap();
                    let cl = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .or_else(|| {
                            headers
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if total >= headers_end + 4 + cl {
                        break;
                    }
                }
                if total >= buf.len() {
                    break;
                }
            }
            sock.write_all(canned_response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&buf[..total]).to_string()
        });
        (base, handle)
    }

    /// Build a `GeminiProvider` directly without going through
    /// `from_entry`, so tests don't have to mutate `std::env`
    /// (which is unsafe in Rust 2024 and the runtime crate
    /// `forbid(unsafe_code)`s).
    fn test_provider(base_url: String, model: &str) -> GeminiProvider {
        GeminiProvider {
            base_url,
            api_key: zeroize::Zeroizing::new("fake-test-key".to_string()),
            default_model: model.into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("test http"),
        }
    }

    #[tokio::test]
    async fn end_to_end_request_shape_and_response_parsing() {
        let canned_json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{ "text": "Hello from mock." }],
                    "role": "model"
                }
            }],
            "usageMetadata": { "promptTokenCount": 4, "candidatesTokenCount": 5, "totalTokenCount": 9 }
        }"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            canned_json.len(),
            canned_json,
        );
        let (base, server) = one_shot_server(response).await;
        let provider = test_provider(base, "gemini-test-model");
        let out = provider
            .generate_reply(ChatInput {
                prompt: "say hi".into(),
                ..Default::default()
            })
            .await
            .expect("ok");
        assert_eq!(out.text, "Hello from mock.");
        assert_eq!(out.provider, "gemini");
        assert_eq!(out.model, "gemini-test-model");
        let usage = out.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 4);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 9);

        let raw_req = server.await.unwrap();
        assert!(
            raw_req.contains("/v1beta/models/gemini-test-model:generateContent"),
            "URL path didn't include model action; raw={raw_req}"
        );
        assert!(
            raw_req
                .to_lowercase()
                .contains("x-goog-api-key: fake-test-key"),
            "missing api-key header; raw={raw_req}"
        );
        let body_start = raw_req.find("\r\n\r\n").unwrap() + 4;
        let body_json: serde_json::Value =
            serde_json::from_str(&raw_req[body_start..]).expect("body parses");
        assert_eq!(
            body_json
                .pointer("/contents/0/parts/0/text")
                .and_then(|v| v.as_str()),
            Some("say hi")
        );
        assert_eq!(
            body_json
                .pointer("/generationConfig/maxOutputTokens")
                .and_then(|v| v.as_u64()),
            Some(2048)
        );
    }

    #[tokio::test]
    async fn end_to_end_http_400_returns_permanent() {
        let body = r#"{"error":{"code":400,"message":"bad model id"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        );
        let (base, server) = one_shot_server(response).await;
        let provider = test_provider(base, "nope");
        let err = provider
            .generate_reply(ChatInput {
                prompt: "x".into(),
                ..Default::default()
            })
            .await
            .expect_err("must be Err");
        assert!(matches!(err, ProviderError::Permanent(_)));
        let _ = server.await;
    }

    // ── RELIX-7.19 GAP 3: finishReason normalisation

    #[test]
    fn normalise_gemini_finish_reason_maps_documented_values() {
        assert_eq!(normalise_gemini_finish_reason("STOP"), "stop");
        assert_eq!(normalise_gemini_finish_reason("MAX_TOKENS"), "length");
        assert_eq!(normalise_gemini_finish_reason("SAFETY"), "content_filter");
        assert_eq!(
            normalise_gemini_finish_reason("RECITATION"),
            "content_filter"
        );
        // OTHER + any unknown value lowercases.
        assert_eq!(normalise_gemini_finish_reason("OTHER"), "other");
    }
}
