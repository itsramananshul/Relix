//! Native Anthropic Messages API provider.
//!
//! Distinct from the OpenAI-compatible path because Anthropic uses different
//! headers (`x-api-key` + `anthropic-version`) and a different response shape
//! (`content[].text`, not `choices[].message.content`).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::{
    ChatInput, ChatOutput, ChatProvider, ChatStream, ProviderEntry, ProviderError, StreamingChunk,
    StreamingUsage, TokenUsage, load_api_key,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    base_url: String,
    /// SEC PART 2: wrapped in `Zeroizing` so the API key is
    /// wiped from the heap when the provider is dropped.
    api_key: zeroize::Zeroizing<String>,
    default_model: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn from_entry(entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let api_key = load_api_key(entry)?.ok_or_else(|| {
            ProviderError::Permanent(
                "[ai.providers.anthropic] missing api_key_env — set api_key_env to the env var \
                 holding the key (e.g. \"ANTHROPIC_API_KEY\")"
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
            .unwrap_or_else(|| "claude-3-5-sonnet-latest".to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(entry.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Permanent(format!("anthropic: http client: {e}")))?;
        Ok(Self {
            base_url,
            api_key: zeroize::Zeroizing::new(api_key),
            default_model,
            http,
        })
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
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
        let mut body = json!({
            "model": model,
            "max_tokens": input.max_tokens.unwrap_or(1024),
            "messages": [{ "role": "user", "content": user_content }],
        });
        if let Some(sys) = &input.system_prompt {
            // PH-WAVE2E: Anthropic prompt caching. When the system
            // block is sent as the structured `[{"type":"text",
            // "text":..., "cache_control":{"type":"ephemeral"}}]`
            // form Anthropic auto-caches it for ~5 minutes. Same
            // exact prompt within the window pays ~10% the cost
            // for the cached portion (currently 90% reduction on
            // input tokens). The marker is harmless when the
            // model doesn't support caching (Anthropic accepts +
            // ignores). Operators get the price reduction with
            // zero per-call code changes.
            body["system"] = json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }
        // PH-WAVE2F: extended thinking. Opt-in budget for
        // o1/o3-style structured reasoning. Anthropic accepts
        // `thinking: { type: "enabled", budget_tokens: N }`
        // and silently ignores it on models that don't
        // support extended thinking — same fail-soft posture
        // as the PH-WAVE2E cache_control hint.
        if let Some(budget) = input.thinking_budget_tokens {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": budget,
            });
        }

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| {
                let reason = crate::nodes::ai::classify_transport_failure(&e.to_string());
                tracing::warn!(
                    provider = "anthropic",
                    failover.reason = %reason.label(),
                    "ai.provider: transport failure"
                );
                ProviderError::Transient(format!(
                    "anthropic: http [{label}]: {e}",
                    label = reason.label(),
                ))
            })?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transient(format!("anthropic: read body: {e}")))?;

        if !status.is_success() {
            // H1: structured classification.
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            tracing::warn!(
                provider = "anthropic",
                http.status = status.as_u16(),
                failover.reason = %reason.label(),
                failover.category = ?reason.category(),
                "ai.provider: http failure"
            );
            let msg = format!(
                "anthropic: HTTP {status} [{label}]: {text}",
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("anthropic: parse: {e}")))?;
        let reply = parsed
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.iter().find_map(|b| b.get("text")?.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ProviderError::Permanent(format!("anthropic: no text content in: {text}"))
            })?;
        let usage = parsed.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            total_tokens: u
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .saturating_add(u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0))
                as u32,
        });
        // RELIX-7.19 GAP 3: extract stop_reason → finish_reason
        // mapping. Anthropic does not report logprobs.
        let finish_reason = parsed
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(normalise_anthropic_stop_reason);
        Ok(ChatOutput {
            text: reply,
            provider: "anthropic",
            model,
            usage,
            finish_reason,
            logprob: None,
        })
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }

    /// Stream Anthropic Messages API deltas. Sends the same body
    /// as `generate_reply` with `stream: true` and parses the
    /// SSE event stream. Anthropic's wire format is:
    ///
    /// ```text
    /// event: message_start
    /// data: {"type":"message_start","message":{...}}
    ///
    /// event: content_block_start
    /// data: {"type":"content_block_start","index":0,"content_block":{...}}
    ///
    /// event: content_block_delta
    /// data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}
    /// ...
    ///
    /// event: content_block_stop
    /// data: {"type":"content_block_stop","index":0}
    ///
    /// event: message_delta
    /// data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{...}}
    ///
    /// event: message_stop
    /// data: {"type":"message_stop"}
    /// ```
    ///
    /// We yield only the `content_block_delta.delta.text` payload
    /// of text-type deltas. `thinking` deltas (Anthropic's
    /// extended-thinking traces) are skipped — they're not part
    /// of the assistant-visible reply text. Errors during the
    /// stream surface as `ProviderError::Transient` items the
    /// caller can present and stop.
    async fn generate_reply_stream(&self, input: ChatInput) -> Result<ChatStream, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
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
        let mut body = json!({
            "model":      model,
            "max_tokens": input.max_tokens.unwrap_or(1024),
            "messages":   [{ "role": "user", "content": user_content }],
            "stream":     true,
        });
        if let Some(sys) = &input.system_prompt {
            body["system"] = json!([{
                "type": "text",
                "text": sys,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(budget) = input.thinking_budget_tokens {
            body["thinking"] = json!({
                "type":          "enabled",
                "budget_tokens": budget,
            });
        }

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", self.api_key.as_str())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::Transient(format!("anthropic: stream http: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            let msg = format!(
                "anthropic: HTTP {status} [{label}]: {text}",
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
            // RELIX-7.11 GAP 2: Anthropic emits `input_tokens`
            // on the `message_start` event and `output_tokens`
            // on the final `message_delta` event. Collect both
            // and emit a single `StreamingChunk::Usage` after
            // `message_stop`.
            let mut prompt_tokens: u32 = 0;
            let mut completion_tokens: u32 = 0;
            let mut saw_any_usage = false;
            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(ProviderError::Transient(format!(
                            "anthropic: stream read: {e}"
                        )));
                        return;
                    }
                };
                let s = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                buf.push_str(&s);
                while let Some(end) = buf.find("\n\n") {
                    let frame = buf[..end].to_string();
                    buf.drain(..end + 2);
                    for line in frame.lines() {
                        match parse_anthropic_sse_line(line) {
                            AnthropicEvent::TextDelta(t) => {
                                yield Ok(StreamingChunk::Text(t));
                            }
                            AnthropicEvent::InputTokens(n) => {
                                prompt_tokens = n;
                                saw_any_usage = true;
                            }
                            AnthropicEvent::MessageDelta { output_tokens, stop_reason } => {
                                if let Some(n) = output_tokens {
                                    completion_tokens = n;
                                    saw_any_usage = true;
                                }
                                if let Some(fr) = stop_reason {
                                    // RELIX-7.19 GAP 3: yield
                                    // the finish reason BEFORE
                                    // the Usage / Done frames
                                    // so downstream consumers
                                    // see the provider signal
                                    // first.
                                    yield Ok(StreamingChunk::FinishReason(fr));
                                }
                            }
                            AnthropicEvent::Done => {
                                if saw_any_usage {
                                    yield Ok(StreamingChunk::Usage(StreamingUsage {
                                        prompt_tokens,
                                        completion_tokens,
                                        model: model_for_usage.clone(),
                                    }));
                                }
                                return;
                            }
                            AnthropicEvent::Skip => {}
                        }
                    }
                }
            }
            // Stream ended without an explicit message_stop —
            // emit whatever usage we observed.
            if saw_any_usage {
                yield Ok(StreamingChunk::Usage(StreamingUsage {
                    prompt_tokens,
                    completion_tokens,
                    model: model_for_usage.clone(),
                }));
            }
        };
        Ok(Box::pin(s))
    }
}

/// Result of parsing one line from the Anthropic SSE stream.
#[derive(Debug)]
pub(crate) enum AnthropicEvent {
    /// One `content_block_delta` text chunk.
    TextDelta(String),
    /// Input token count from `message_start.message.usage.input_tokens`.
    InputTokens(u32),
    /// Output token count from `message_delta.usage.output_tokens`.
    /// RELIX-7.19 GAP 3: the `message_delta` event also
    /// carries `stop_reason`; emit it as a sibling field on
    /// the same event since the SSE wire frame is single-
    /// line and we only parse it once.
    MessageDelta {
        output_tokens: Option<u32>,
        stop_reason: Option<String>,
    },
    /// Terminal `message_stop` event — caller should stop reading.
    Done,
    /// Every other event type (block start/stop, ping,
    /// comments, extended-thinking deltas). The caller MUST
    /// ignore these silently — they're meta-events the client
    /// doesn't surface.
    Skip,
}

/// RELIX-7.19 GAP 3: map Anthropic's `stop_reason` enum to
/// the small ConfidenceScorer vocabulary. `end_turn` and
/// `stop_sequence` map to `"stop"`; `max_tokens` maps to
/// `"length"`; `tool_use` passes through; anything else
/// (future Anthropic additions) becomes `"other"`.
pub(crate) fn normalise_anthropic_stop_reason(v: &str) -> String {
    match v {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_use".to_string(),
        other => other.to_string(),
    }
}

/// Parse one line of the Anthropic SSE stream. Returns
/// [`AnthropicEvent::Skip`] for everything that isn't a usable
/// text delta or a terminal event. Pure function — exported via
/// `pub(crate)` for unit tests.
pub(crate) fn parse_anthropic_sse_line(line: &str) -> AnthropicEvent {
    // Anthropic SSE alternates `event: <name>` and `data: <json>`
    // lines. We only need to inspect data lines — the JSON
    // payload has a `type` field that tells us what event it is.
    let Some(payload) = line.strip_prefix("data:") else {
        return AnthropicEvent::Skip;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return AnthropicEvent::Skip;
    }
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return AnthropicEvent::Skip,
    };
    let ty = parsed.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "message_stop" => AnthropicEvent::Done,
        "message_start" => {
            // RELIX-7.11 GAP 2: input_tokens lives at
            // `message_start.message.usage.input_tokens`.
            if let Some(n) = parsed
                .pointer("/message/usage/input_tokens")
                .and_then(|v| v.as_u64())
            {
                return AnthropicEvent::InputTokens(n as u32);
            }
            AnthropicEvent::Skip
        }
        "message_delta" => {
            // RELIX-7.11 GAP 2 + RELIX-7.19 GAP 3: this single
            // SSE frame carries BOTH the per-frame token
            // counter (`usage.output_tokens`) and the final
            // `delta.stop_reason`. Return a combined event so
            // the stream loop sees both at once.
            let output_tokens = parsed
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32);
            let stop_reason = parsed
                .pointer("/delta/stop_reason")
                .and_then(|v| v.as_str())
                .map(normalise_anthropic_stop_reason);
            if output_tokens.is_none() && stop_reason.is_none() {
                AnthropicEvent::Skip
            } else {
                AnthropicEvent::MessageDelta {
                    output_tokens,
                    stop_reason,
                }
            }
        }
        "content_block_delta" => {
            // Only `text_delta` carries assistant text. `thinking_delta`
            // belongs to Anthropic's extended-thinking trace and is
            // intentionally skipped (the consumer asked for the
            // reply text, not the model's chain of thought).
            let delta = parsed.get("delta");
            let delta_ty = delta
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if delta_ty != "text_delta" {
                return AnthropicEvent::Skip;
            }
            let text = delta
                .and_then(|d| d.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if text.is_empty() {
                AnthropicEvent::Skip
            } else {
                AnthropicEvent::TextDelta(text.to_string())
            }
        }
        _ => AnthropicEvent::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_api_key_env_errors() {
        // Use a deliberately-not-set env-var name.
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: Some("RELIX_TEST_ABSOLUTELY_MISSING_ANTH_42".into()),
            default_model: None,
            timeout_secs: 30,
        };
        // AnthropicProvider does not impl Debug (holds an HTTP client).
        match AnthropicProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing provider key")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_extracts_text_delta() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#;
        match parse_anthropic_sse_line(line) {
            AnthropicEvent::TextDelta(s) => assert_eq!(s, "Hello"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_handles_message_stop_as_done() {
        let line = r#"data: {"type":"message_stop"}"#;
        assert!(matches!(
            parse_anthropic_sse_line(line),
            AnthropicEvent::Done
        ));
    }

    #[test]
    fn parse_anthropic_sse_skips_event_lines_and_pings() {
        assert!(matches!(
            parse_anthropic_sse_line("event: content_block_delta"),
            AnthropicEvent::Skip
        ));
        assert!(matches!(
            parse_anthropic_sse_line("data: {\"type\":\"ping\"}"),
            AnthropicEvent::Skip
        ));
        assert!(matches!(parse_anthropic_sse_line(""), AnthropicEvent::Skip));
        assert!(matches!(
            parse_anthropic_sse_line(": keep-alive comment"),
            AnthropicEvent::Skip
        ));
    }

    #[test]
    fn parse_anthropic_sse_extracts_input_tokens_from_message_start() {
        let line = r#"data: {"type":"message_start","message":{"id":"x","model":"claude-sonnet-4","usage":{"input_tokens":42,"output_tokens":0}}}"#;
        match parse_anthropic_sse_line(line) {
            AnthropicEvent::InputTokens(n) => assert_eq!(n, 42),
            other => panic!("expected InputTokens, got {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_extracts_output_tokens_from_message_delta() {
        let line = r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":117}}"#;
        match parse_anthropic_sse_line(line) {
            AnthropicEvent::MessageDelta {
                output_tokens,
                stop_reason,
            } => {
                assert_eq!(output_tokens, Some(117));
                // RELIX-7.19 GAP 3: same frame carries the
                // stop_reason — normalised to "stop".
                assert_eq!(stop_reason.as_deref(), Some("stop"));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_skips_message_start_without_usage() {
        let line =
            r#"data: {"type":"message_start","message":{"id":"x","model":"claude-sonnet-4"}}"#;
        assert!(matches!(
            parse_anthropic_sse_line(line),
            AnthropicEvent::Skip
        ));
    }

    #[test]
    fn parse_anthropic_sse_skips_thinking_deltas() {
        // Extended-thinking traces aren't part of the assistant
        // reply text — the consumer asked for the answer, not
        // the chain of thought.
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me consider..."}}"#;
        assert!(matches!(
            parse_anthropic_sse_line(line),
            AnthropicEvent::Skip
        ));
    }

    #[test]
    fn parse_anthropic_sse_skips_malformed_json() {
        assert!(matches!(
            parse_anthropic_sse_line("data: {not really json"),
            AnthropicEvent::Skip
        ));
    }

    #[test]
    fn no_api_key_env_at_all_errors_with_hint() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: None,
            default_model: None,
            timeout_secs: 30,
        };
        match AnthropicProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing api_key_env")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    // ── RELIX-7.19 GAP 3: stop_reason normalisation

    #[test]
    fn normalise_anthropic_stop_reason_maps_documented_values() {
        assert_eq!(normalise_anthropic_stop_reason("end_turn"), "stop");
        assert_eq!(normalise_anthropic_stop_reason("stop_sequence"), "stop");
        assert_eq!(normalise_anthropic_stop_reason("max_tokens"), "length");
        assert_eq!(normalise_anthropic_stop_reason("tool_use"), "tool_use");
        // Unknown values pass through verbatim so future
        // Anthropic additions don't silently lose information.
        assert_eq!(normalise_anthropic_stop_reason("future"), "future");
    }

    #[test]
    fn parse_anthropic_sse_message_delta_carries_stop_reason_alone() {
        let line = r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null}}"#;
        match parse_anthropic_sse_line(line) {
            AnthropicEvent::MessageDelta {
                output_tokens,
                stop_reason,
            } => {
                assert!(output_tokens.is_none());
                assert_eq!(stop_reason.as_deref(), Some("length"));
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }
}
