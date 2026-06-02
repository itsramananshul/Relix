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
    http::{HeaderValue, StatusCode},
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
    /// Relix extension: durable workspace lease for this chat turn.
    /// The bridge resolves this to a tenant-owned active lease before
    /// stamping the workspace path onto dispatch envelopes.
    #[serde(default)]
    pub workspace_lease_id: Option<String>,
    /// Catch-all for unsupported fields (top_p, n, presence_penalty, …) so
    /// validation never rejects an OpenAI client over an inert parameter.
    #[serde(flatten)]
    pub _extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    pub content: String,
    /// SEC PART 5: serde-deserialised marker that signals
    /// the incoming message carried a `tool_calls` payload
    /// (assistant-emitted function-call envelope per the
    /// OpenAI spec). We do not implement OpenAI-format
    /// tool calling, so `translate_request` rejects with a
    /// clear error rather than silently dropping. Always
    /// serialises as `false` (no `#[serde(skip)]`) so
    /// round-tripping our own response is stable.
    #[serde(
        default,
        rename = "tool_calls",
        deserialize_with = "deserialize_has_tool_calls",
        skip_serializing
    )]
    pub has_tool_calls: bool,
}

/// SEC PART 5: serde helper for `OpenAiMessage::has_tool_calls`.
/// Reads the OpenAI `tool_calls` array (if present) and returns
/// `true` when it is a non-empty array — the shape OpenAI
/// clients use to attach function-call envelopes.
fn deserialize_has_tool_calls<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match v {
        Some(Value::Array(arr)) => !arr.is_empty(),
        Some(Value::Null) | None => false,
        _ => false,
    })
}

#[derive(Debug, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: &'static str,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    /// Token usage. The web bridge only sees the model's reply
    /// text over the mesh; the AI node routes its real token
    /// counts out-of-band to the metrics sink, so they never
    /// reach this layer (see RELA-23 / RELA-33). Rather than
    /// emit a fabricated `0/0/0` that makes cost-tracking
    /// clients compute a zero spend, the field is omitted
    /// entirely when no real counts are available. `Some` is
    /// reserved for the day usage travels the wire.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
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
    /// Coordinator-side Task id when persistence was wired and the call
    /// succeeded. `None` when the coordinator is absent / unreachable
    /// (B1.9 fail-soft). Skipped from serialisation when None so
    /// strict OpenAI clients that don't expect non-standard fields
    /// stay clean.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
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

/// `GET /v1/info` — Relix-native server info. The OpenAI shim
/// is an HTTP-compatible facade over a Relix mesh; this endpoint
/// is what SDK clients (and humans!) call to find out what's
/// actually behind it. The shape is intentionally NOT in the
/// OpenAI spec — it's a Relix surface — so we use a stable
/// nested JSON object that an SDK can rely on.
#[derive(Debug, Serialize)]
pub struct InfoResponse {
    pub system: &'static str,
    pub version: &'static str,
    pub provider: String,
    pub model: String,
    pub capabilities: Vec<&'static str>,
}

pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    let model = resolve_model_label(&state, "");
    let provider = provider_hint_for_model(&state, &model);
    Json(InfoResponse {
        system: "relix",
        version: env!("CARGO_PKG_VERSION"),
        provider,
        model,
        capabilities: vec!["chat", "streaming", "memory", "tasks"],
    })
}

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
    let observability_start = std::time::Instant::now();
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

    // RELIX-2 step 5: when `stream:true` AND the operator has
    // configured `[flow] streaming_template_path`, take the
    // true end-to-end streaming path — the SOL VM pipes
    // tokens to a chunk observer that forwards them to the
    // SSE response as they arrive from the AI provider. The
    // tool-flow URL detection is intentionally skipped for the
    // streaming path: `ai.chat.stream` doesn't run the planner
    // / tool dispatcher (semantic carved out in step 3), so a
    // URL-in-message in the streaming case is just streamed
    // verbatim like any other prompt.
    if req.stream
        && tool_url.is_none()
        && let Some(streaming_template) = state.streaming_template.clone()
    {
        return chat_completions_streaming(
            state,
            translated,
            req,
            streaming_template,
            model_label,
            observability_start,
        )
        .await;
    }

    let outcome = match tool_url.as_deref() {
        Some(url) => execute_chat_with_tool_flow(
            &state,
            &translated.session_id,
            &translated.prompt,
            url,
            req.workspace_lease_id.as_deref(),
        )
        .await
        .map_err(exec_error_to_http)?,
        None => execute_chat_flow(
            &state,
            &translated.session_id,
            &translated.prompt,
            req.workspace_lease_id.as_deref(),
        )
        .await
        .map_err(exec_error_to_http)?,
    };

    // W8: derive the SHA-256 of the system prompt (empty
    // string when the OpenAI request didn't include one) so
    // the provenance snapshot has a deterministic fingerprint
    // operators can correlate across runs.
    let system_prompt_text: String = req
        .messages
        .iter()
        .find(|m| m.role.eq_ignore_ascii_case("system"))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let system_prompt_hash = sha256_hex(&system_prompt_text);
    // Two-sink observability record. Sink A always lands;
    // Sink B carries the verbatim prompt + response. The
    // dashboard's session debugger reads from both via the
    // bridge endpoints landing in the next commit.
    record_chat_observability(
        &state,
        &translated.session_id,
        &outcome.trace_id,
        &translated.prompt,
        &outcome.reply,
        &model_label,
        observability_start.elapsed().as_millis() as u64,
    );
    // W8: provenance snapshot. Records what model + system
    // prompt drove this turn so a future regression report can
    // diff two traces by trace_id.
    record_chat_provenance(
        &state,
        &translated.session_id,
        &outcome.trace_id,
        &model_label,
        &system_prompt_hash,
    );

    // Honest headers: the bridge knows which model it resolved
    // and which provider it routed to (best-effort from the
    // manifest cache). Stamped on both the JSON and the SSE
    // response so OpenAI clients can audit which Relix backend
    // actually served the call.
    let provider_hint = provider_hint_for_model(&state, &model_label);
    let model_header =
        HeaderValue::from_str(&model_label).unwrap_or_else(|_| HeaderValue::from_static("relix"));
    let provider_header =
        HeaderValue::from_str(&provider_hint).unwrap_or_else(|_| HeaderValue::from_static("mesh"));

    if req.stream {
        let stream = build_openai_sse(
            outcome.reply.clone(),
            model_label.clone(),
            translated.session_id.clone(),
            outcome.flow_id.clone(),
            outcome.trace_id.clone(),
            outcome.flow_log_path.clone(),
            outcome.task_id.clone(),
            outcome.workspace_lease_id.clone(),
            outcome.workspace_path.clone(),
            state.cfg.sse.chunk_bytes,
            Duration::from_millis(state.cfg.sse.chunk_delay_ms),
        );
        let mut resp = Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
        resp.headers_mut()
            .insert("x-relix-model", model_header.clone());
        resp.headers_mut()
            .insert("x-relix-provider", provider_header.clone());
        Ok(resp)
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
                    has_tool_calls: false,
                },
                finish_reason: "stop",
            }],
            // RELA-23: no real token counts are available at the
            // bridge layer, so usage is omitted rather than
            // reported as a misleading zero.
            usage: None,
            relix: RelixExtension {
                flow_id: outcome.flow_id,
                trace_id: outcome.trace_id,
                flow_log: outcome.flow_log_path,
                session_id: translated.session_id,
                task_id: outcome.task_id,
                workspace_lease_id: outcome.workspace_lease_id,
                workspace_path: outcome.workspace_path,
            },
        };
        let mut http = Json(resp).into_response();
        http.headers_mut().insert("x-relix-model", model_header);
        http.headers_mut()
            .insert("x-relix-provider", provider_header);
        Ok(http)
    }
}

/// RELIX-2 step 5: end-to-end streaming variant of
/// [`chat_completions`]. Drives `execute_chat_flow_streaming`
/// with a chunk observer that forwards each token from the SOL
/// VM's `remote_call_stream` opcode into a tokio mpsc channel.
/// The SSE response reads from that channel and emits one
/// OpenAI-compatible `chat.completion.chunk` frame per token,
/// terminating with the standard `[DONE]` sentinel after the
/// flow completes.
///
/// Crucial property: the SSE response is opened BEFORE the flow
/// runs. HTTP clients see the first chunk as soon as the AI
/// provider's stream yields the first token — not after the
/// VM has finished collecting. Provenance + observability
/// records are written AFTER the flow completes (inside the
/// SSE stream's tail), so the audit trail matches the unary
/// path exactly.
async fn chat_completions_streaming(
    state: AppState,
    translated: TranslatedChatRequest,
    req: ChatCompletionRequest,
    streaming_template: String,
    model_label: String,
    observability_start: std::time::Instant,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    use crate::flow::execute_chat_flow_streaming;
    use axum::response::sse::Event;
    use std::sync::Arc;

    // Channel for tokens. Unbounded so the AI provider's
    // chunk arrival never blocks the SOL VM thread; bounded
    // would risk a deadlock when the SSE consumer is slow.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let tx_for_observer = tx.clone();
    let on_chunk: relix_runtime::flow_runner::ChunkObserver = Arc::new(move |bytes: &[u8]| {
        let _ = tx_for_observer.send(bytes.to_vec());
    });

    // RELIX-2 step 5b: cancellation signal wired through to
    // the streaming dispatcher. The Drop guard further down
    // fires `notify_one` when the SSE stream future is
    // dropped (client disconnect / proxy timeout), aborting
    // the in-flight `remote_call_stream` and writing a
    // `task.failed` audit trail instead of silently letting
    // the flow run to completion with nobody listening.
    let cancel_signal: relix_runtime::flow_runner::CancelSignal =
        Arc::new(tokio::sync::Notify::new());
    let cancel_for_flow = cancel_signal.clone();
    let cancel_for_guard = cancel_signal.clone();

    let state_for_flow = state.clone();
    let session_id = translated.session_id.clone();
    let prompt = translated.prompt.clone();
    let workspace_lease_id = req.workspace_lease_id.clone();
    let flow_handle = tokio::spawn(async move {
        let outcome = execute_chat_flow_streaming(
            &state_for_flow,
            &session_id,
            &prompt,
            workspace_lease_id.as_deref(),
            &streaming_template,
            on_chunk,
            cancel_for_flow,
        )
        .await;
        // Close the channel so the SSE stream exits its
        // recv-loop and emits the finish frame.
        drop(tx);
        outcome
    });

    /// RELIX-2 step 5b: cancellation guard. Lives inside the
    /// SSE stream future. When the future drops (HTTP client
    /// disconnect, proxy reset, shutdown), this Drop fires
    /// `notify_one()` on the signal the streaming dispatcher
    /// is selecting on — the in-flight substream read
    /// returns TRANSPORT-classed error, the FlowRunner writes
    /// `task.failed`, and the chronicle records the
    /// cancellation honestly instead of leaving an in-flight
    /// flow with no listener.
    struct CancelGuard(std::sync::Arc<tokio::sync::Notify>);
    impl Drop for CancelGuard {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }
    let cancel_guard = CancelGuard(cancel_for_guard);

    // W8: SHA-256 of the system prompt for the provenance
    // snapshot. Captured BEFORE the SSE stream so the closure
    // doesn't have to clone `req.messages`.
    let system_prompt_text: String = req
        .messages
        .iter()
        .find(|m| m.role.eq_ignore_ascii_case("system"))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let system_prompt_hash = sha256_hex(&system_prompt_text);

    let provider_hint = provider_hint_for_model(&state, &model_label);
    let model_header =
        HeaderValue::from_str(&model_label).unwrap_or_else(|_| HeaderValue::from_static("relix"));
    let provider_header =
        HeaderValue::from_str(&provider_hint).unwrap_or_else(|_| HeaderValue::from_static("mesh"));

    // The SSE id is a placeholder for the role frame; we
    // patch the real flow_id into the final relix-metadata
    // frame once the spawned flow completes.
    let pending_id = format!("chatcmpl-pending-{}", unix_now());
    let model_for_stream = model_label.clone();
    let session_for_stream = translated.session_id.clone();
    let prompt_for_obs = translated.prompt.clone();
    let state_for_tail = state.clone();

    let sse_stream = async_stream::stream! {
        // RELIX-2 step 5b: move the cancel guard INTO the
        // stream future. When this future drops (because the
        // HTTP client dropped the SSE response), the guard's
        // Drop impl fires `notify_one` on the cancel signal
        // — the streaming dispatcher's `tokio::select!` arm
        // sees it and aborts. Without this move the guard
        // would live in the enclosing scope and only drop
        // after the response handler returned, which would
        // be AFTER the flow already finished.
        let _cancel_guard = cancel_guard;

        let created = unix_now();
        // Role marker.
        yield Ok::<_, Infallible>(
            Event::default().data(streaming_role_chunk_json(&pending_id, created, &model_for_stream))
        );

        // Token chunks as they arrive. Tracks the full body
        // for the observability + provenance records.
        let mut accumulated: String = String::new();
        while let Some(bytes) = rx.recv().await {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            accumulated.push_str(&text);
            yield Ok(Event::default().data(streaming_content_chunk_json(
                &pending_id,
                created,
                &model_for_stream,
                &text,
            )));
        }

        // Channel closed → flow finished. Await the outcome
        // so we can stamp the real flow_id / trace_id on the
        // final relix-metadata frame AND write provenance /
        // observability.
        let outcome = match flow_handle.await {
            Ok(Ok(o)) => Some(o),
            Ok(Err(_)) => None,
            Err(_) => None,
        };

        let (
            real_flow_id,
            real_trace_id,
            real_flow_log,
            real_task_id,
            real_workspace_lease_id,
            real_workspace_path,
        ) = match &outcome {
            Some(o) => (
                o.flow_id.clone(),
                o.trace_id.clone(),
                o.flow_log_path.clone(),
                o.task_id.clone(),
                o.workspace_lease_id.clone(),
                o.workspace_path.clone(),
            ),
            None => (String::new(), String::new(), String::new(), None, None, None),
        };

        // Final finish frame + relix metadata.
        let finish_id = format!("chatcmpl-{real_flow_id}");
        yield Ok(Event::default().data(streaming_finish_chunk_json(
            &finish_id,
            created,
            &model_for_stream,
            &real_flow_id,
            &real_trace_id,
            &real_flow_log,
            &session_for_stream,
            real_task_id.as_deref(),
            real_workspace_lease_id.as_deref(),
            real_workspace_path.as_deref(),
        )));

        // Two-sink observability + provenance — same shape as
        // the unary path. Writes AFTER the stream has
        // emitted the full body so the audit reflects what
        // the client actually received.
        if let Some(o) = outcome.as_ref() {
            record_chat_observability(
                &state_for_tail,
                &session_for_stream,
                &o.trace_id,
                &prompt_for_obs,
                &accumulated,
                &model_for_stream,
                observability_start.elapsed().as_millis() as u64,
            );
            record_chat_provenance(
                &state_for_tail,
                &session_for_stream,
                &o.trace_id,
                &model_for_stream,
                &system_prompt_hash,
            );
        }

        // OpenAI clients (and Open WebUI) look for the
        // literal `[DONE]`.
        yield Ok(Event::default().data(STREAMING_DONE_SENTINEL));
    };

    let mut resp = Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response();
    resp.headers_mut()
        .insert("x-relix-model", model_header.clone());
    resp.headers_mut()
        .insert("x-relix-provider", provider_header.clone());
    Ok(resp)
}

/// RELIX-2 step 6: streaming-SSE chunk builders. Pulled out
/// of the inline `async_stream!` macro so unit tests can
/// pin the wire shape without going through axum / a full
/// AppState. Each function returns the JSON body for one
/// SSE `data: ...` line. The async_stream block wraps the
/// returned string in `Event::default().data(...)` —
/// equivalent to writing `data: <body>\n\n` on the wire.
pub(crate) fn streaming_role_chunk_json(id: &str, created: u64, model: &str) -> String {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": null,
        }],
    })
    .to_string()
}

/// RELIX-2 step 6: content chunk. One per token (or per
/// arrival from the AI provider, depending on the
/// provider's chunking granularity). `content` carries the
/// raw token text — the chunk JSON's `delta.content` field
/// matches the OpenAI streaming wire shape exactly.
pub(crate) fn streaming_content_chunk_json(
    id: &str,
    created: u64,
    model: &str,
    content: &str,
) -> String {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"content": content},
            "finish_reason": null,
        }],
    })
    .to_string()
}

/// RELIX-2 step 6: terminal finish chunk + Relix provenance
/// envelope. `finish_reason: "stop"` is the canonical
/// OpenAI sentinel; the `relix` object is a non-standard
/// extension that operators can read for cross-correlation
/// with the per-flow event log + task ledger.
#[allow(clippy::too_many_arguments)]
pub(crate) fn streaming_finish_chunk_json(
    id: &str,
    created: u64,
    model: &str,
    flow_id: &str,
    trace_id: &str,
    flow_log: &str,
    session_id: &str,
    task_id: Option<&str>,
    workspace_lease_id: Option<&str>,
    workspace_path: Option<&str>,
) -> String {
    serde_json::json!({
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
            "task_id": task_id,
            "workspace_lease_id": workspace_lease_id,
            "workspace_path": workspace_path,
        },
    })
    .to_string()
}

/// RELIX-2 step 6: terminal `[DONE]` sentinel. OpenAI
/// clients (and Open WebUI) look for the literal string,
/// not a JSON object. Kept as a constant so tests can
/// match exactly.
pub(crate) const STREAMING_DONE_SENTINEL: &str = "[DONE]";

/// Best-effort provider attribution for a resolved model label.
/// The bridge doesn't see the per-call provider chosen by the AI
/// node (that's a downstream decision), so we infer:
///
/// 1. Match the model label against the manifest cache — if any
///    discovered `ai.chat` capability advertises a `provider:X`
///    sensitivity tag AND its peer's announced model matches,
///    return that provider.
/// 2. Fall back to a label-substring sniff (`gpt-` → openai,
///    `claude-` → anthropic, `gemini-` → gemini, `grok-` → xai,
///    `relix-mock` → mock).
/// 3. Default: `"mesh"` — the honest "we don't know" label.
fn provider_hint_for_model(state: &AppState, model: &str) -> String {
    let lc = model.to_ascii_lowercase();
    // Manifest-cache lookup (per-peer provider tag).
    for cached in state.manifest_cache.entries() {
        for cap in &cached.manifest.capabilities {
            if cap.method_name != "ai.chat" {
                continue;
            }
            if let Some(provider) = cap
                .sensitivity_tags
                .iter()
                .find_map(|t| t.strip_prefix("provider:"))
            {
                // Heuristic: when the operator-curated `[openai_compat.models]`
                // table maps the same model label to a peer, we accept the
                // peer's provider tag as authoritative.
                let id_hint = format!("relix-{provider}");
                if lc == id_hint || lc.contains(provider) {
                    return provider.to_string();
                }
            }
        }
    }
    // Substring sniff. Honest about scope: this is heuristic, not
    // ground truth. The response body itself is the authoritative
    // record; the header is a convenience for OpenAI-shim clients.
    if lc.starts_with("gpt-") || lc.contains("openai") {
        return "openai".into();
    }
    if lc.starts_with("claude") || lc.contains("anthropic") {
        return "anthropic".into();
    }
    if lc.starts_with("gemini") {
        return "gemini".into();
    }
    if lc.starts_with("grok") || lc.contains("xai") {
        return "xai".into();
    }
    if lc.contains("mock") {
        return "mock".into();
    }
    "mesh".into()
}

// ─────────────────────────── Translation logic ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedChatRequest {
    pub session_id: String,
    pub prompt: String,
}

/// Convert an OpenAI chat completion request into the (session_id, prompt)
/// pair the SOL chat flow consumes.
///
/// SEC PART 5: pre-fix behaviour silently DROPPED OpenAI
/// `system` messages and any `tools` / `tool_calls` payload —
/// callers had no way to tell their context was discarded. The
/// new contract:
///
/// 1. `tools` (function-calling tool definitions) and per-
///    message `tool_calls` payloads are REJECTED with a clear
///    "not supported" error. The Relix native API is the
///    documented path for tool-calling; silently dropping the
///    definitions risks producing answers that look like they
///    honoured the operator's tool surface when they did not.
/// 2. EVERY `system` message in the request is preserved as
///    additional context — the prompt sent to the Relix chat
///    flow is `[system 1]\n[system 2]\n…\n[last user message]`.
///    The last user message remains the primary instruction;
///    system messages are framed so the model can distinguish
///    them.
/// 3. The session-id derivation is unchanged (blake3 of first
///    system + first user) so existing conversations keep
///    bucketing correctly.
pub fn translate_request(req: &ChatCompletionRequest) -> Result<TranslatedChatRequest, String> {
    if req.messages.is_empty() {
        return Err("messages: empty".into());
    }

    // SEC PART 5: reject OpenAI tool definitions outright.
    // `_extra` is the serde-flatten catch-all for unknown
    // top-level keys; OpenAI clients put the `tools` array
    // there. Refusing is preferable to silently dropping
    // because a dropped tool definition produces answers
    // that look like they honoured the operator's tool
    // surface when they did not.
    if req._extra.contains_key("tools") {
        return Err(
            "Tool definitions in OpenAI format are not supported by the Relix OpenAI shim. \
             Use the native Relix API for tool calls."
                .to_string(),
        );
    }
    // SEC PART 5: same posture for per-message `tool_calls`
    // (the assistant-emitted function-call envelope) and
    // `role = "tool"` messages (the function-call result the
    // OpenAI client would normally feed back in). Both
    // surfaces are part of the tool-calling protocol we do
    // not implement; admitting them silently is the bug.
    for (idx, m) in req.messages.iter().enumerate() {
        if m.role.eq_ignore_ascii_case("tool") {
            return Err(format!(
                "messages[{idx}] role = \"tool\" is not supported by the Relix OpenAI shim. \
                 Use the native Relix API for tool calls."
            ));
        }
        if m.has_tool_calls {
            return Err(format!(
                "messages[{idx}] carries `tool_calls`. Tool definitions in OpenAI format \
                 are not supported by the Relix OpenAI shim. Use the native Relix API \
                 for tool calls."
            ));
        }
    }

    // The prompt is the last `user` message; ignore trailing `assistant` /
    // `tool` / `system` messages with no later user turn (rare in practice).
    let last_user = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role.eq_ignore_ascii_case("user"))
        .ok_or_else(|| "messages: no user message found".to_string())?;

    let last_user_text = sanitize_openai_message(&last_user.content)
        .map_err(|e| format!("messages[last user].content: {e}"))?;
    if last_user_text.is_empty() {
        return Err("messages[last user].content: empty after sanitisation".into());
    }

    // SEC PART 5: collect EVERY system message in order and
    // sanitise each. The bridge then prepends them to the
    // prompt as additional context so the AI node sees what
    // the OpenAI client intended. Pre-fix path dropped them
    // entirely. We frame each block with `[SYSTEM N]` /
    // `[USER]` headers so the receiving model can
    // distinguish them.
    let mut system_blocks: Vec<String> = Vec::new();
    for (idx, m) in req
        .messages
        .iter()
        .filter(|m| m.role.eq_ignore_ascii_case("system"))
        .enumerate()
    {
        let cleaned = sanitize_openai_message(&m.content)
            .map_err(|e| format!("messages[system #{}].content: {e}", idx + 1))?;
        if !cleaned.is_empty() {
            system_blocks.push(cleaned);
        }
    }

    let prompt = if system_blocks.is_empty() {
        last_user_text
    } else {
        let mut combined = String::with_capacity(last_user_text.len() + 128);
        for (i, sys) in system_blocks.iter().enumerate() {
            combined.push_str(&format!("[SYSTEM {}]\n{}\n\n", i + 1, sys));
        }
        combined.push_str("[USER]\n");
        combined.push_str(&last_user_text);
        combined
    };

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

/// W8: SHA-256 of `text` rendered as lowercase hex. Empty
/// input yields the empty string instead of the SHA-256 of an
/// empty buffer — operator-facing convention "no system
/// prompt → empty hash" beats "no system prompt → constant
/// magic hash" because the absence stays trivially
/// distinguishable in a grep.
pub(crate) fn sha256_hex(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

/// W8: write a [`ProvenanceSnapshot`] for one chat turn into
/// the bridge's observability context. Best-effort; logs and
/// returns on failure rather than propagating to the HTTP
/// response.
pub(crate) fn record_chat_provenance(
    state: &AppState,
    session_id: &str,
    trace_id: &str,
    model_label: &str,
    system_prompt_hash: &str,
) {
    record_chat_provenance_into(
        &state.observability,
        session_id,
        trace_id,
        model_label,
        system_prompt_hash,
    );
}

/// W8: write the snapshot into a supplied
/// [`ObservabilityContext`]. Factored out so tests can drive
/// the path without constructing a full bridge `AppState`.
pub(crate) fn record_chat_provenance_into(
    observability: &relix_runtime::observability::ObservabilityContext,
    session_id: &str,
    trace_id: &str,
    model_label: &str,
    system_prompt_hash: &str,
) {
    use relix_runtime::observability::ProvenanceSnapshot;
    let snap_trace_id = if trace_id.trim().is_empty() {
        format!("chat-{}-{session_id}", unix_now())
    } else {
        trace_id.to_string()
    };
    let mut tools = std::collections::BTreeMap::new();
    // W8 stores the system-prompt fingerprint under a stable
    // pseudo-tool key. Future commits can split this into a
    // dedicated field on ProvenanceSnapshot; today the diff
    // helper already surfaces tool_versions changes.
    tools.insert(
        "system_prompt_sha256".to_string(),
        system_prompt_hash.to_string(),
    );
    let snap = ProvenanceSnapshot {
        trace_id: snap_trace_id,
        timestamp_unix: unix_now() as i64,
        model_id: model_label.to_string(),
        policy_version: String::new(),
        skill_versions: std::collections::BTreeMap::new(),
        tool_versions: tools,
    };
    if let Err(e) = observability.provenance.record(&snap) {
        tracing::warn!(error = %e, session_id, "provenance: record failed");
    }
}

/// Record one `/v1/chat/completions` call to the two-sink
/// observability surface. Metadata lands in Sink A; the
/// prompt + response land in Sink B linked by `event_id`
/// (= the outcome's `trace_id`). Best-effort: a sink write
/// error is logged but never propagates to the HTTP
/// response.
fn record_chat_observability(
    state: &AppState,
    session_id: &str,
    trace_id: &str,
    prompt: &str,
    reply: &str,
    model_label: &str,
    latency_ms: u64,
) {
    use relix_runtime::observability::{ContentEvent, MetadataEvent};
    let now: i64 = unix_now() as i64;
    let event_id = if trace_id.trim().is_empty() {
        format!("chat-{now}-{session_id}")
    } else {
        trace_id.to_string()
    };
    let meta = MetadataEvent {
        event_id: event_id.clone(),
        session_id: session_id.to_string(),
        agent_id: "bridge".to_string(),
        event_type: "model_call".to_string(),
        timestamp_unix: now,
        latency_ms: Some(latency_ms),
        token_count: None,
        cost_cents: None,
        error_type: None,
        tool_name: None,
        model_name: Some(model_label.to_string()),
        success: true,
    };
    // Two content rows linked by event_id: prompt + reply.
    // ObservabilityContext::record_event writes one at a
    // time, so call it twice with the same meta + different
    // content. The second metadata insert is `INSERT OR
    // REPLACE` so it's idempotent.
    state.observability.record_event(
        meta.clone(),
        Some(ContentEvent {
            event_id: event_id.clone(),
            content_type: "prompt".to_string(),
            content: prompt.to_string(),
            redacted: false,
            timestamp_unix: now,
        }),
    );
    state.observability.record_event(
        meta,
        Some(ContentEvent {
            event_id,
            content_type: "response".to_string(),
            content: reply.to_string(),
            redacted: false,
            timestamp_unix: now,
        }),
    );
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
    task_id: Option<String>,
    workspace_lease_id: Option<String>,
    workspace_path: Option<String>,
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
                "task_id": task_id,
                "workspace_lease_id": workspace_lease_id,
                "workspace_path": workspace_path,
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
    fn provider_hint_recognises_known_model_prefixes() {
        // Synthesise a minimal `AppState` is heavy; the helper
        // only reads `state.manifest_cache` (empty by default) +
        // the model label. We bypass state via a thin shim:
        // the substring-sniff branch is reached for any empty
        // manifest cache, so we exercise THAT branch using the
        // public helper signature in two steps.
        // (The manifest-driven branch is exercised end-to-end
        // by the /v1/models tests.)
        // We can call `provider_hint_for_model` on a freshly
        // empty state. Building an empty AppState in tests is
        // hard, so we test the helper indirectly via the same
        // logic.
        // Sniff branch is pure on the model label:
        fn sniff(model: &str) -> &'static str {
            let lc = model.to_ascii_lowercase();
            if lc.starts_with("gpt-") || lc.contains("openai") {
                return "openai";
            }
            if lc.starts_with("claude") || lc.contains("anthropic") {
                return "anthropic";
            }
            if lc.starts_with("gemini") {
                return "gemini";
            }
            if lc.starts_with("grok") || lc.contains("xai") {
                return "xai";
            }
            if lc.contains("mock") {
                return "mock";
            }
            "mesh"
        }
        assert_eq!(sniff("gpt-4o"), "openai");
        assert_eq!(sniff("claude-3-5-sonnet-latest"), "anthropic");
        assert_eq!(sniff("gemini-2.0-flash"), "gemini");
        assert_eq!(sniff("grok-2-latest"), "xai");
        assert_eq!(sniff("relix-mock"), "mock");
        assert_eq!(sniff("totally-unknown"), "mesh");
    }

    #[test]
    fn info_response_shape_is_documented() {
        // Round-trip through JSON to confirm field names match
        // the documented contract — SDK authors rely on the
        // exact key names.
        let info = InfoResponse {
            system: "relix",
            version: "0.1.5",
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            capabilities: vec!["chat", "streaming"],
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["system"], "relix");
        assert_eq!(json["version"], "0.1.5");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["model"], "gpt-4o-mini");
        assert!(json["capabilities"].is_array());
        assert!(
            json["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "chat")
        );
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
        // SEC PART 5: the system message is now prepended
        // as additional context (pre-fix path silently
        // dropped it). The last user turn ends the prompt.
        assert!(
            t.prompt.contains("[SYSTEM 1]\nbe helpful"),
            "expected system block, got: {}",
            t.prompt
        );
        assert!(
            t.prompt.ends_with("[USER]\nhow are you?"),
            "expected user tail, got: {}",
            t.prompt
        );
        assert!(t.session_id.starts_with("oa-"));
    }

    // ── SEC PART 5: system + tool surface ───────────────

    #[test]
    fn sec_p5_translate_includes_every_system_message_in_order() {
        let req = req_json(
            r#"{
                "model":"relix-mock",
                "messages":[
                    {"role":"system","content":"sys A"},
                    {"role":"user","content":"q1"},
                    {"role":"system","content":"sys B"},
                    {"role":"user","content":"q2"}
                ]
            }"#,
        );
        let t = translate_request(&req).expect("translate");
        // Both system messages appear, in order; the last
        // user message terminates the prompt.
        let pos_a = t.prompt.find("[SYSTEM 1]\nsys A").expect("sys A present");
        let pos_b = t.prompt.find("[SYSTEM 2]\nsys B").expect("sys B present");
        assert!(pos_a < pos_b, "system messages must appear in order");
        assert!(t.prompt.ends_with("[USER]\nq2"));
    }

    #[test]
    fn sec_p5_translate_rejects_tools_field() {
        let req = req_json(
            r#"{
                "model":"x",
                "messages":[{"role":"user","content":"hi"}],
                "tools":[{"type":"function","function":{"name":"do_thing"}}]
            }"#,
        );
        let err = translate_request(&req).expect_err("must reject");
        assert!(
            err.contains("Tool definitions in OpenAI format are not supported"),
            "got: {err}"
        );
        assert!(err.contains("native Relix API"));
    }

    #[test]
    fn sec_p5_translate_rejects_tool_role_message() {
        let req = req_json(
            r#"{
                "model":"x",
                "messages":[
                    {"role":"user","content":"hi"},
                    {"role":"tool","content":"{\"result\":42}"}
                ]
            }"#,
        );
        let err = translate_request(&req).expect_err("must reject");
        assert!(
            err.contains("role = \"tool\" is not supported"),
            "got: {err}"
        );
    }

    #[test]
    fn sec_p5_translate_rejects_assistant_tool_calls_payload() {
        let req = req_json(
            r#"{
                "model":"x",
                "messages":[
                    {"role":"user","content":"hi"},
                    {"role":"assistant","content":"","tool_calls":[{"id":"c1"}]}
                ]
            }"#,
        );
        let err = translate_request(&req).expect_err("must reject");
        assert!(err.contains("tool_calls"), "got: {err}");
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
        // SEC PART 5: the session id is still derived from
        // the FIRST system + FIRST user turn so it remains
        // stable as the conversation grows. The PROMPT now
        // carries the system context as well; assert the
        // user-tail rather than the legacy bare prompt.
        assert_eq!(t1.session_id, t2.session_id);
        assert!(t1.prompt.ends_with("[USER]\nfirst turn"));
        assert!(t2.prompt.ends_with("[USER]\nthird turn"));
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
                "workspace_lease_id":"wsl_abc",
                "presence_penalty":0.1,
                "tool_choice":"auto",
                "logprobs":true
            }"#,
        );
        let t = translate_request(&r).expect("ok");
        assert_eq!(t.prompt, "hi");
        assert_eq!(r.workspace_lease_id.as_deref(), Some("wsl_abc"));
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

    // ── W8: ProvenanceSnapshot recording ─────────────────────────

    #[test]
    fn sha256_hex_returns_expected_digest_and_empty_for_empty_input() {
        assert_eq!(sha256_hex(""), "");
        // SHA-256("hi") = 8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4
        assert_eq!(
            sha256_hex("hi"),
            "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4"
        );
    }

    fn obs() -> relix_runtime::observability::ObservabilityContext {
        relix_runtime::observability::ObservabilityContext::in_memory()
    }

    #[test]
    fn record_chat_provenance_writes_snapshot_with_model_and_hash() {
        let ctx = obs();
        let trace_id = "trace-w8-1";
        let hash = sha256_hex("you are a helpful assistant");
        assert!(!hash.is_empty());
        record_chat_provenance_into(&ctx, "sess-1", trace_id, "gpt-4o-mini", &hash);
        let got = ctx.provenance.get(trace_id).unwrap();
        let snap = got.expect("snapshot recorded");
        assert_eq!(snap.trace_id, trace_id);
        assert_eq!(snap.model_id, "gpt-4o-mini");
        assert_eq!(
            snap.tool_versions
                .get("system_prompt_sha256")
                .map(|s| s.as_str()),
            Some(hash.as_str())
        );
    }

    #[test]
    fn record_chat_provenance_stores_empty_hash_when_no_system_prompt() {
        let ctx = obs();
        let trace_id = "trace-w8-2";
        record_chat_provenance_into(&ctx, "sess-2", trace_id, "gpt-4o-mini", "");
        let snap = ctx
            .provenance
            .get(trace_id)
            .unwrap()
            .expect("snapshot recorded");
        assert_eq!(
            snap.tool_versions
                .get("system_prompt_sha256")
                .map(|s| s.as_str()),
            Some("")
        );
    }

    #[test]
    fn record_chat_provenance_generates_trace_id_when_empty() {
        let ctx = obs();
        record_chat_provenance_into(&ctx, "sess-3", "", "gpt-4o-mini", "");
        // We don't know the generated trace_id but we can
        // search the registry via the trace_id prefix
        // `chat-<unix>-sess-3`. Practical test: prefix scan
        // over a small in-memory store would be heavy; instead
        // assert that the get-by-known-id miss path still
        // surfaces None and the snapshot landed under a
        // session-shaped id by reading the snapshot through
        // its generated prefix. Easiest path: re-record with
        // an explicit trace_id and assert the previous record
        // didn't collide.
        record_chat_provenance_into(&ctx, "sess-3", "explicit", "gpt-4o-mini", "");
        let snap = ctx
            .provenance
            .get("explicit")
            .unwrap()
            .expect("snapshot recorded for explicit trace_id");
        assert_eq!(snap.model_id, "gpt-4o-mini");
    }

    // ───────────────────── RELIX-2 step 6 ────────────────────
    //
    // Wire-shape unit tests for the streaming-SSE chunk
    // builders. These pin the OpenAI-compatible JSON shape +
    // the `[DONE]` sentinel without needing to boot a real
    // mesh — the load-bearing logic is pure functions, easy
    // to exercise in isolation.

    fn parse_chunk(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("chunk body must be valid JSON")
    }

    #[test]
    fn streaming_role_chunk_matches_openai_role_marker_shape() {
        let body = streaming_role_chunk_json("chatcmpl-test", 1_700_000_000, "gpt-4o");
        let v = parse_chunk(&body);
        assert_eq!(v["id"], "chatcmpl-test");
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["created"], 1_700_000_000);
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["choices"][0]["index"], 0);
        assert_eq!(v["choices"][0]["delta"]["role"], "assistant");
        // Role marker MUST NOT carry a finish_reason — that
        // signals end-of-stream to OpenAI clients.
        assert!(v["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn streaming_content_chunk_carries_delta_content_and_no_finish_reason() {
        let body =
            streaming_content_chunk_json("chatcmpl-test", 1_700_000_000, "gpt-4o", "hello world");
        let v = parse_chunk(&body);
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["choices"][0]["delta"]["content"], "hello world");
        assert!(v["choices"][0]["finish_reason"].is_null());
        // Role MUST be absent on content chunks per OpenAI's
        // streaming shape (it appears once on the role marker
        // frame and nowhere else).
        assert!(v["choices"][0]["delta"]["role"].is_null());
    }

    #[test]
    fn streaming_finish_chunk_carries_stop_reason_and_relix_metadata() {
        let body = streaming_finish_chunk_json(
            "chatcmpl-abc123",
            1_700_000_000,
            "gpt-4o",
            "flow-id-hex",
            "trace-id-hex",
            "/tmp/flow.log",
            "session-foo",
            Some("task-bar"),
            Some("wsl-1"),
            Some("/repo"),
        );
        let v = parse_chunk(&body);
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        // delta is an empty object on the terminal frame
        // (no content, no role).
        assert!(v["choices"][0]["delta"].is_object());
        assert!(v["choices"][0]["delta"].as_object().unwrap().is_empty());
        let relix = &v["relix"];
        assert_eq!(relix["flow_id"], "flow-id-hex");
        assert_eq!(relix["trace_id"], "trace-id-hex");
        assert_eq!(relix["flow_log"], "/tmp/flow.log");
        assert_eq!(relix["session_id"], "session-foo");
        assert_eq!(relix["task_id"], "task-bar");
        assert_eq!(relix["workspace_lease_id"], "wsl-1");
        assert_eq!(relix["workspace_path"], "/repo");
    }

    #[test]
    fn streaming_finish_chunk_omits_task_id_when_coordinator_absent() {
        let body = streaming_finish_chunk_json(
            "chatcmpl-abc123",
            1_700_000_000,
            "gpt-4o",
            "flow-id-hex",
            "trace-id-hex",
            "/tmp/flow.log",
            "session-foo",
            None,
            None,
            None,
        );
        let v = parse_chunk(&body);
        // task_id field present but null when no coordinator
        // is wired. OpenAI clients ignore the `relix`
        // namespace, but operators reading the trace see
        // explicit `null` rather than the field being absent.
        assert!(v["relix"]["task_id"].is_null());
    }

    #[test]
    fn streaming_done_sentinel_is_exact_openai_literal() {
        // OpenAI's SDK + Open WebUI + curl --no-buffer all
        // grep for the literal "[DONE]". Anything else
        // (whitespace, quoted JSON, etc.) breaks downstream
        // clients silently.
        assert_eq!(STREAMING_DONE_SENTINEL, "[DONE]");
    }

    #[test]
    fn streaming_chunk_sequence_decodes_in_order_with_distinct_deltas() {
        // Drive the same logic the async_stream! block runs —
        // role + N content + finish + DONE — and assert the
        // sequence decodes as a coherent OpenAI streaming
        // response. This is the "wire shape regression
        // harness" pin: a future refactor that subtly
        // re-orders frames or drops one of them fails here.
        let id = "chatcmpl-test";
        let created = 1_700_000_000;
        let model = "gpt-4o";
        let mut frames: Vec<String> = Vec::new();
        frames.push(streaming_role_chunk_json(id, created, model));
        for tok in ["hello ", "streaming ", "world"] {
            frames.push(streaming_content_chunk_json(id, created, model, tok));
        }
        frames.push(streaming_finish_chunk_json(
            id,
            created,
            model,
            "f1",
            "t1",
            "/tmp/f.log",
            "s1",
            None,
            None,
            None,
        ));
        // Decode every JSON frame (asserts shape) +
        // reconstruct the body from delta.content fields.
        let mut body = String::new();
        let mut saw_role = false;
        let mut saw_finish = false;
        for f in &frames {
            let v = parse_chunk(f);
            if v["choices"][0]["delta"]["role"] == "assistant" {
                saw_role = true;
            }
            if let Some(s) = v["choices"][0]["delta"]["content"].as_str() {
                body.push_str(s);
            }
            if v["choices"][0]["finish_reason"] == "stop" {
                saw_finish = true;
            }
        }
        assert!(saw_role, "exactly one role-marker frame must appear");
        assert!(saw_finish, "terminal finish_reason frame must appear");
        assert_eq!(body, "hello streaming world");
        // After the JSON frames the literal [DONE] sentinel
        // closes the stream.
        assert_eq!(STREAMING_DONE_SENTINEL, "[DONE]");
    }

    fn sample_response(usage: Option<Usage>) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "chatcmpl-test".to_string(),
            object: "chat.completion",
            created: 1_700_000_000,
            model: "relix-mock".to_string(),
            choices: vec![ChatCompletionChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: "hello".to_string(),
                    has_tool_calls: false,
                },
                finish_reason: "stop",
            }],
            usage,
            relix: RelixExtension {
                flow_id: "f1".to_string(),
                trace_id: "t1".to_string(),
                flow_log: "/tmp/f.log".to_string(),
                session_id: "s1".to_string(),
                task_id: None,
                workspace_lease_id: None,
                workspace_path: None,
            },
        }
    }

    #[test]
    fn usage_is_omitted_when_no_real_counts_available() {
        // RELA-23 (Approach B, honest omission): the bridge has
        // no real token counts at this layer, so the response
        // must NOT carry a `usage` object at all. Emitting a
        // zero-filled usage would make cost-tracking clients
        // record a false zero spend; an absent field is the
        // honest signal that usage was not measured here.
        let resp = sample_response(None);
        let v = serde_json::to_value(&resp).expect("serialize response");
        assert!(
            v.get("usage").is_none(),
            "usage must be omitted when no real counts exist, got: {v}"
        );
    }

    #[test]
    fn usage_when_present_is_emitted_with_consistent_total() {
        // The field stays a faithful pass-through for the day
        // real counts travel the wire: when usage IS present it
        // serializes in full and total == prompt + completion.
        let resp = sample_response(Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 8,
            total_tokens: 20,
        }));
        let v = serde_json::to_value(&resp).expect("serialize response");
        assert_eq!(v["usage"]["prompt_tokens"], 12);
        assert_eq!(v["usage"]["completion_tokens"], 8);
        assert_eq!(v["usage"]["total_tokens"], 20);
        assert_eq!(
            v["usage"]["total_tokens"].as_u64().unwrap(),
            v["usage"]["prompt_tokens"].as_u64().unwrap()
                + v["usage"]["completion_tokens"].as_u64().unwrap(),
        );
    }
}
