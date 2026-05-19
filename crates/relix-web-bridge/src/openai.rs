//! OpenAI-compatible shim — `POST /v1/chat/completions` and `GET /v1/models`.
//!
//! The shim is a *thin translation layer*. It converts an OpenAI-style
//! request into the same SOL chat flow `POST /chat` uses, then projects the
//! flow result back into the OpenAI response shape (JSON for non-streaming,
//! OpenAI SSE chunks for `stream:true`).
//!
//! Architecturally:
//!
//!   * SOL remains the orchestration source of truth.
//!   * Bridge owns no AI provider key — provider selection happens on the
//!     AI node, advertised here only as cosmetic model ids.
//!   * Open WebUI and other OpenAI clients can talk to Relix unchanged.
//!
//! ## Session derivation (SIMP-020)
//!
//! OpenAI requests carry full message history every turn. The bridge derives
//! a *stable* session id from a hash of the first user message so subsequent
//! turns land in the same memory bucket on the memory node. The flow itself
//! re-reads history from Relix memory via `memory.recent_for_session`; the
//! client-supplied prior history is therefore acknowledged but ignored.
//!
//! ## Limitations (SIMP-020)
//!
//! * `system` messages and OpenAI tool-call payloads are dropped in the
//!   alpha — only the last `user` message becomes the prompt.
//! * `temperature` / `top_p` / `max_tokens` are accepted but ignored; those
//!   are provider-side concerns living on the AI node.
//! * Streaming is bridge-level (SIMP-019), not true token streaming.

use std::convert::Infallible;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::{ErrorResponse, exec_error_to_http};
use crate::config::AppState;
use crate::flow::{execute_chat_flow, execute_chat_with_tool_flow};
use crate::sse::split_utf8_into_chunks;
use crate::validate::{detect_url_in_message, sanitize_openai_message};

// ─────────────────────────── Request / response types ──────────────────────

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    #[serde(default)]
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(default)]
    pub stream: bool,
    /// Accepted but ignored — provider-side concern lives on the AI node.
    /// Held as a field (not flattened into `_extra`) so OpenAI clients that
    /// inspect their own outgoing request can confirm we parsed it.
    #[allow(dead_code)]
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Catch-all for unsupported fields (top_p, n, presence_penalty, …) so
    /// validation never rejects an OpenAI client over an inert parameter.
    #[serde(flatten)]
    pub _extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
    /// Non-OpenAI Relix extension so curl users see provenance.
    pub relix: RelixExtension,
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: OpenAiMessage,
    pub finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Serialize)]
pub struct RelixExtension {
    pub flow_id: String,
    pub trace_id: String,
    pub flow_log: String,
    pub session_id: String,
}

#[derive(Debug, Serialize)]
pub struct ModelsList {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub owned_by: &'static str,
    pub description: String,
}

// ─────────────────────────────── Handlers ──────────────────────────────────

pub async fn models(State(state): State<AppState>) -> impl IntoResponse {
    let now = unix_now();

    // 1) Static entries from `[openai_compat] models = [...]` (operator-curated).
    let mut data: Vec<ModelEntry> = state
        .cfg
        .openai_compat
        .as_ref()
        .map(|c| c.models.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|m| ModelEntry {
            id: m.id,
            object: "model",
            created: now,
            owned_by: "relix",
            description: m.description,
        })
        .collect();

    // 2) Dynamic entries derived from the M10 manifest cache. Any peer that
    //    advertises `ai.chat` becomes a model id of the form
    //    `relix-<provider>` (provider tag taken from the capability
    //    descriptor's sensitivity tags, with `unknown` as a fallback).
    //    Operator-curated entries are NOT overwritten — they appear first
    //    so an explicit alias wins over an auto-derived one.
    let mut have: std::collections::BTreeSet<String> = data.iter().map(|e| e.id.clone()).collect();
    for cached in state.manifest_cache.entries() {
        for cap in &cached.manifest.capabilities {
            if cap.method_name != "ai.chat" {
                continue;
            }
            let provider = cap
                .sensitivity_tags
                .iter()
                .find_map(|t| t.strip_prefix("provider:"))
                .unwrap_or("unknown");
            let id = format!("relix-{provider}");
            if have.insert(id.clone()) {
                data.push(ModelEntry {
                    id,
                    object: "model",
                    created: now,
                    owned_by: "relix",
                    description: format!(
                        "Discovered ai.chat on peer '{}' (node_type={})",
                        cached.alias.as_deref().unwrap_or("<unaliased>"),
                        cached.manifest.node_type,
                    ),
                });
            }
        }
    }

    // 3) Last-resort fallback: nothing static, nothing discovered.
    if data.is_empty() {
        data.push(ModelEntry {
            id: "relix".to_string(),
            object: "model",
            created: now,
            owned_by: "relix",
            description: "Default Relix mesh route (provider configured on AI node)".to_string(),
        });
    }

    Json(ModelsList {
        object: "list",
        data,
    })
}

pub async fn chat_completions(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let translated = translate_request(&req).map_err(invalid_input)?;
    let model_label = resolve_model_label(&state, &req.model);

    // Template selection — bridge does this and ONLY this. The decision is:
    // if the user message contains an http(s) URL AND the tool-flow template
    // is configured, use that template. Otherwise fall back to the regular
    // chat template. The tool node still runs its own admission pipeline
    // (identity → policy → SSRF check → fetch → audit) regardless of how
    // it got invoked.
    let tool_url = if state.tool_template.is_some() {
        detect_url_in_message(&translated.prompt)
    } else {
        None
    };

    let outcome = match tool_url.as_deref() {
        Some(url) => {
            execute_chat_with_tool_flow(&state, &translated.session_id, &translated.prompt, url)
                .await
                .map_err(exec_error_to_http)?
        }
        None => execute_chat_flow(&state, &translated.session_id, &translated.prompt)
            .await
            .map_err(exec_error_to_http)?,
    };

    if req.stream {
        let stream = build_openai_sse(
            outcome.reply.clone(),
            model_label.clone(),
            translated.session_id.clone(),
            outcome.flow_id.clone(),
            outcome.trace_id.clone(),
            outcome.flow_log_path.clone(),
            state.cfg.sse.chunk_bytes,
            Duration::from_millis(state.cfg.sse.chunk_delay_ms),
        );
        Ok(Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response())
    } else {
        let resp = ChatCompletionResponse {
            id: format!("chatcmpl-{}", outcome.flow_id),
            object: "chat.completion",
            created: unix_now(),
            model: model_label,
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: outcome.reply.clone(),
                },
                finish_reason: "stop",
            }],
            usage: Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            },
            relix: RelixExtension {
                flow_id: outcome.flow_id,
                trace_id: outcome.trace_id,
                flow_log: outcome.flow_log_path,
                session_id: translated.session_id,
            },
        };
        Ok(Json(resp).into_response())
    }
}

// ─────────────────────────── Translation logic ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedChatRequest {
    pub session_id: String,
    pub prompt: String,
}

/// Convert an OpenAI chat completion request into the (session_id, prompt)
/// pair the SOL chat flow consumes.
pub fn translate_request(req: &ChatCompletionRequest) -> Result<TranslatedChatRequest, String> {
    if req.messages.is_empty() {
        return Err("messages: empty".into());
    }

    // The prompt is the last `user` message; ignore trailing `assistant` /
    // `tool` / `system` messages with no later user turn (rare in practice).
    let last_user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role.eq_ignore_ascii_case("user"))
        .ok_or_else(|| "messages: no user message found".to_string())?;

    let prompt = sanitize_openai_message(&last_user.content)
        .map_err(|e| format!("messages[last user].content: {e}"))?;
    if prompt.is_empty() {
        return Err("messages[last user].content: empty after sanitisation".into());
    }

    // Session id = blake3 of (first system content + first user content).
    // Stable as conversation grows; bucketing in Relix memory just works.
    let first_system = req
        .messages
        .iter()
        .find(|m| m.role.eq_ignore_ascii_case("system"))
        .map(|m| m.content.as_str())
        .unwrap_or("");
    let first_user = req
        .messages
        .iter()
        .find(|m| m.role.eq_ignore_ascii_case("user"))
        .map(|m| m.content.as_str())
        .unwrap_or("");

    let mut hasher = blake3::Hasher::new();
    hasher.update(first_system.as_bytes());
    hasher.update(b"\x00");
    hasher.update(first_user.as_bytes());
    let digest = hasher.finalize();
    let session_id = format!("oa-{}", hex::encode(&digest.as_bytes()[..6]));

    Ok(TranslatedChatRequest { session_id, prompt })
}

fn resolve_model_label(state: &AppState, requested: &str) -> String {
    if !requested.is_empty() {
        return requested.to_string();
    }
    if let Some(cfg) = state.cfg.openai_compat.as_ref() {
        if !cfg.default_model.is_empty() {
            return cfg.default_model.clone();
        }
        if let Some(first) = cfg.models.first() {
            return first.id.clone();
        }
    }
    "relix".to_string()
}

fn invalid_input(msg: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: msg,
            flow_id: None,
            flow_log: None,
        }),
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ─────────────────────────── OpenAI SSE shape ──────────────────────────────

/// Emit OpenAI-style chat.completion.chunk SSE events, ending with the
/// `data: [DONE]` sentinel Open WebUI and the official `openai` clients
/// look for.
#[allow(clippy::too_many_arguments)]
fn build_openai_sse(
    reply: String,
    model: String,
    session_id: String,
    flow_id: String,
    trace_id: String,
    flow_log: String,
    chunk_bytes: usize,
    chunk_delay: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    use async_stream::stream;
    let id = format!("chatcmpl-{flow_id}");
    let created = unix_now();
    stream! {
        // Frame 1 — role marker.
        let role_chunk = serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant"},
                "finish_reason": null,
            }],
        });
        yield Ok(Event::default().data(role_chunk.to_string()));

        // Frames 2..N — content deltas.
        for slice in split_utf8_into_chunks(&reply, chunk_bytes) {
            let content_chunk = serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"content": slice},
                    "finish_reason": null,
                }],
            });
            yield Ok(Event::default().data(content_chunk.to_string()));
            if !chunk_delay.is_zero() {
                tokio::time::sleep(chunk_delay).await;
            }
        }

        // Frame N+1 — Relix provenance (non-standard but ignored by OpenAI clients).
        let relix_chunk = serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop",
            }],
            "relix": {
                "flow_id": flow_id,
                "trace_id": trace_id,
                "flow_log": flow_log,
                "session_id": session_id,
            },
        });
        yield Ok(Event::default().data(relix_chunk.to_string()));

        // OpenAI clients (and Open WebUI) look for the literal `[DONE]`.
        yield Ok(Event::default().data("[DONE]"));
    }
}

// ─────────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn req_json(s: &str) -> ChatCompletionRequest {
        serde_json::from_str(s).expect("parse openai request")
    }

    #[test]
    fn translate_extracts_last_user_message() {
        let req = req_json(
            r#"{
                "model":"relix-mock",
                "messages":[
                    {"role":"system","content":"be helpful"},
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":"hello!"},
                    {"role":"user","content":"how are you?"}
                ]
            }"#,
        );
        let t = translate_request(&req).expect("translate");
        assert_eq!(t.prompt, "how are you?");
        assert!(t.session_id.starts_with("oa-"));
    }

    #[test]
    fn translate_session_id_stable_as_conversation_grows() {
        let r1 = req_json(
            r#"{
                "model":"x",
                "messages":[
                    {"role":"system","content":"sysprompt"},
                    {"role":"user","content":"first turn"}
                ]
            }"#,
        );
        let r2 = req_json(
            r#"{
                "model":"x",
                "messages":[
                    {"role":"system","content":"sysprompt"},
                    {"role":"user","content":"first turn"},
                    {"role":"assistant","content":"prior reply"},
                    {"role":"user","content":"third turn"}
                ]
            }"#,
        );
        let t1 = translate_request(&r1).expect("t1");
        let t2 = translate_request(&r2).expect("t2");
        assert_eq!(t1.session_id, t2.session_id);
        assert_eq!(t1.prompt, "first turn");
        assert_eq!(t2.prompt, "third turn");
    }

    #[test]
    fn translate_rejects_empty_messages_and_no_user() {
        let r = req_json(r#"{"messages":[]}"#);
        assert!(translate_request(&r).is_err());
        let r = req_json(
            r#"{"messages":[{"role":"system","content":"x"},{"role":"assistant","content":"y"}]}"#,
        );
        assert!(translate_request(&r).is_err());
    }

    #[test]
    fn translate_sanitises_newlines_in_user_content() {
        let r =
            req_json(r#"{"messages":[{"role":"user","content":"line one\nline two\ttabbed"}]}"#);
        let t = translate_request(&r).expect("ok");
        assert!(!t.prompt.contains('\n'));
        assert!(!t.prompt.contains('\t'));
        assert_eq!(t.prompt, "line one line two tabbed");
    }

    #[test]
    fn translate_rejects_user_content_with_quote_or_pipe() {
        let r = req_json(r#"{"messages":[{"role":"user","content":"say \"hi\""}]}"#);
        assert!(translate_request(&r).is_err());
        let r = req_json(r#"{"messages":[{"role":"user","content":"a|b"}]}"#);
        assert!(translate_request(&r).is_err());
    }

    #[test]
    fn translate_ignores_unknown_fields_silently() {
        let r = req_json(
            r#"{
                "model":"x",
                "stream":false,
                "messages":[{"role":"user","content":"hi"}],
                "presence_penalty":0.1,
                "tool_choice":"auto",
                "logprobs":true
            }"#,
        );
        let t = translate_request(&r).expect("ok");
        assert_eq!(t.prompt, "hi");
    }

    #[test]
    fn translate_uses_first_user_for_session_not_last() {
        let a = req_json(
            r#"{"messages":[
                {"role":"user","content":"alpha"},
                {"role":"assistant","content":"x"},
                {"role":"user","content":"beta"}
            ]}"#,
        );
        let b = req_json(
            r#"{"messages":[
                {"role":"user","content":"alpha"},
                {"role":"assistant","content":"y"},
                {"role":"user","content":"gamma"}
            ]}"#,
        );
        assert_eq!(
            translate_request(&a).unwrap().session_id,
            translate_request(&b).unwrap().session_id
        );
    }
}
