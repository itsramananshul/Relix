//! Shared flow-execution helper used by every chat handler.
//!
//! `execute_chat_flow` is the bridge's single seam to `FlowRunner`. It:
//!
//!   1. Validates the input characters (SIMP-018).
//!   2. **(B1)** Creates a Task on the Coordinator when one is wired,
//!      fail-soft. Adds the `flow_selected` event.
//!   3. Renders the SOL template with the supplied session + message.
//!   4. Materialises the rendered SOL to a per-request tempfile.
//!   5. Calls `FlowRunner::run` on the existing libp2p path.
//!   6. **(B1)** Writes the terminal `task.update` (completed/failed) +
//!      a `flow_completed` / `flow_failed` event. All best-effort.
//!   7. Surfaces a structured outcome (now including `task_id`) so
//!      JSON / SSE / OpenAI handlers all project the same underlying
//!      flow result.

use std::path::{Path, PathBuf};

use crate::AppState;
use crate::task_recorder::{TaskRecorder, make_title};
use crate::validate::{validate_input, validate_url};
use relix_core::types::TraceId;
use relix_runtime::flow_runner::{FlowRunOptions, FlowRunner, FlowRunnerError};
use relix_runtime::nodes::coordinator::FailureClass;

/// SEC PART 3: a two-pass `{{KEY}}` template renderer that
/// cannot chain-trigger a second substitution. The pre-fix
/// path called `.replace("{{SESSION}}", session_id)` followed
/// by `.replace("{{MESSAGE}}", message)` — if `session_id`
/// itself contained the literal `{{MESSAGE}}` (or any other
/// placeholder), the second `.replace` would substitute that
/// in too, letting an attacker control template expansion
/// past the bridge's intent.
///
/// Algorithm:
///   1. Walk every value once, escaping any `{{` substring to
///      a sentinel that the template engine can never re-emit.
///   2. Scan the template ONCE left-to-right; at each `{{KEY}}`
///      look up the escaped value and splice it in. Unknown
///      `{{KEY}}` runs pass through verbatim (preserves the
///      existing behaviour of `String::replace` on missing
///      keys — a no-op).
///   3. After the scan, replace each sentinel with the literal
///      `{{` so the rendered template still contains the
///      user-supplied bytes faithfully but the engine can't
///      re-trigger on them.
///
/// `ESCAPED_OPEN` is two ASCII bytes that don't appear in
/// SOL source (NUL is forbidden inside source by every
/// tokenizer), so a final substitution back to `{{` is
/// unambiguous.
pub(crate) fn render_template_safe(template: &str, pairs: &[(&str, &str)]) -> String {
    const ESCAPED_OPEN: &str = "\0{\0{\0";
    let escaped_pairs: Vec<(&str, String)> = pairs
        .iter()
        .map(|(k, v)| (*k, v.replace("{{", ESCAPED_OPEN)))
        .collect();
    // Single left-to-right scan: find `{{KEY}}` runs and splice.
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 <= bytes.len() && &bytes[i..i + 2] == b"{{" {
            // Find matching `}}` ahead.
            let rest = &template[i + 2..];
            if let Some(close_rel) = rest.find("}}") {
                let key = &rest[..close_rel];
                if let Some((_, v)) = escaped_pairs.iter().find(|(k, _)| *k == key) {
                    out.push_str(v);
                    i += 2 + close_rel + 2;
                    continue;
                }
            }
        }
        // Push the next char (preserving UTF-8 boundaries).
        let next = next_utf8_boundary(bytes, i);
        out.push_str(&template[i..next]);
        i = next;
    }
    // Restore the escaped `{{` so the rendered output is byte-
    // identical to what the user supplied (minus the splice).
    out.replace(ESCAPED_OPEN, "{{")
}

fn next_utf8_boundary(bytes: &[u8], from: usize) -> usize {
    let mut j = from + 1;
    while j < bytes.len() && (bytes[j] & 0xC0) == 0x80 {
        j += 1;
    }
    j.min(bytes.len())
}

/// Pick the tempfile suffix that matches the configured
/// template's on-disk extension. The bridge writes the rendered
/// template to a tempfile; the FlowRunner dispatches on the
/// extension (`.sol` / `.yml` / `.yaml` / `.sflow`), so the
/// tempfile suffix must round-trip the original choice.
/// Defaults to `.sol` when the path has no extension or an
/// extension the FlowRunner doesn't recognise.
fn tempfile_suffix_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("yml") => ".yml",
        Some("yaml") => ".yaml",
        Some("sflow") => ".sflow",
        _ => ".sol",
    }
}

/// Successful end-to-end chat flow.
#[derive(Debug, Clone)]
pub struct FlowOutcome {
    /// The provider's reply text, resolved from the VM's final heap string.
    pub reply: String,
    /// 16-byte FlowId, hex-encoded.
    pub flow_id: String,
    /// 16-byte TraceId, hex-encoded.
    pub trace_id: String,
    /// On-disk path of the per-flow event log.
    pub flow_log_path: String,
    /// Coordinator-side Task id when persistence was wired AND the
    /// `task.create` call succeeded. `None` when the coordinator is
    /// absent or the call failed (fail-soft).
    pub task_id: Option<String>,
    /// Durable workspace lease used for this run, when the caller
    /// supplied `workspace_lease_id` and it resolved for the current
    /// tenant.
    pub workspace_lease_id: Option<String>,
    /// Resolved workspace path from the durable lease. This is what
    /// gets stamped onto dispatch envelopes for workspace-scoped
    /// standing approvals.
    pub workspace_path: Option<String>,
}

/// Categorised failure so handlers can pick the right HTTP status.
#[derive(Debug, thiserror::Error)]
pub enum FlowExecError {
    /// Invalid request body / characters — 400 Bad Request.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Mesh transport / dial / RPC layer failure — 502 Bad Gateway.
    #[error("mesh transport: {0}")]
    Transport(String),
    /// Required production dependency is unavailable.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Anything else surfaced by the runner — 500 Internal Server Error.
    #[error("{0}")]
    Internal(String),
}

/// Execute one chat turn through the configured SOL flow template.
pub async fn execute_chat_flow(
    state: &AppState,
    session_id: &str,
    message: &str,
    workspace_lease_id: Option<&str>,
) -> Result<FlowOutcome, FlowExecError> {
    validate_input(session_id, message).map_err(FlowExecError::InvalidInput)?;
    let workspace = resolve_workspace_binding(state, workspace_lease_id)?;

    // B1.2: best-effort task creation. None when coordinator is absent
    // or the call failed (TaskRecorder logs the warning).
    let task_id = create_task(
        state.task_recorder.as_ref(),
        task_persistence_required(state),
        "chat",
        "flows/chat_template.sol",
        &chat_params_json(session_id, message),
    )
    .await?;
    // C2b.1: mint the trace_id upfront so the Coordinator's attempt
    // row and the per-flow event log share the same correlation id.
    let trace_id = TraceId::new();
    let trace_hex = trace_id.to_string();
    if let (Some(rec), Some(tid)) = (state.task_recorder.as_ref(), task_id.as_ref()) {
        rec.event(tid, "flow.started", "flows/chat_template.sol")
            .await;
        // C2a.2: status -> running opens a new attempt row on the
        // Coordinator. Fail-soft; recorded as WARN on failure.
        rec.start_running(tid, &trace_hex).await;
        // Phase-1D M35: record the ai.chat capability invocation
        // with the resolved peer alias. The chat SOL template
        // hardcodes `remote_call("ai", "ai.chat", …)` so the
        // alias is unambiguously "ai" — same honesty as the
        // chat_with_tool flow's M34 record.
        rec.event(tid, "capability.invoked", "method=ai.chat peer=ai")
            .await;
    }
    // Phase 4: bind the created task to the workspace lease (if any)
    // so the lease's active-run fields reflect the work using it.
    bind_workspace_active_run(state, &workspace, task_id.as_deref());
    // W5: record the user turn in the chronicle so
    // task.session_export can reconstruct the transcript.
    record_chat_turn(
        state.task_recorder.as_ref(),
        task_id.as_ref(),
        "chat.user_turn",
        session_id,
        "user",
        message,
    )
    .await;

    // SEC PART 3: two-pass safe substitution — a session_id
    // containing the literal `{{MESSAGE}}` no longer triggers
    // a second substitution.
    let rendered = render_template_safe(
        &state.template,
        &[("SESSION", session_id), ("MESSAGE", message)],
    );

    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-")
        .suffix(tempfile_suffix_for(&state.cfg.flow.template_path))
        .tempfile()
        .map_err(|e| FlowExecError::Internal(format!("tempfile: {e}")))?;
    std::fs::write(tmp.path(), rendered.as_bytes())
        .map_err(|e| FlowExecError::Internal(format!("write tempfile: {e}")))?;
    let flow_path: PathBuf = tmp.path().to_path_buf();

    let opts = FlowRunOptions {
        flow_path,
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key.clone(),
        peers: state.peers.clone(),
        data_dir: state.cfg.transport.data_dir.clone(),
        deadline_secs: state.cfg.transport.deadline_secs,
        capability_cache: Some(state.manifest_cache.clone()),
        mesh_client: state.mesh_client.clone(),
        trace_id: Some(trace_id),
        task_id: task_id.clone(),
        session_id: Some(session_id.to_string()),
        workspace_path: workspace.workspace_path.clone(),
        chunk_observer: None,
        cancel_signal: None,
        last_confidence_cell: Some(relix_runtime::confidence::LastConfidenceCell::new()),
    };

    finalize_flow_run(
        FlowRunner::new(opts).run().await,
        state.task_recorder.as_ref(),
        task_id,
        Some(session_id.to_string()),
        workspace,
    )
    .await
}

/// Translate a `FlowRunner::run` outcome into a `FlowOutcome` while
/// making VM-level halts (e.g. tool node returned `policy_denied`)
/// visible as a real error response instead of a 200 OK with an empty
/// body, AND while writing the terminal task event/update when a
/// Coordinator is wired.
async fn finalize_flow_run(
    res: Result<relix_runtime::flow_runner::FlowRunResult, FlowRunnerError>,
    recorder: Option<&TaskRecorder>,
    task_id: Option<String>,
    session_id_for_turn: Option<String>,
    workspace: WorkspaceBinding,
) -> Result<FlowOutcome, FlowExecError> {
    match res {
        Ok(result) => {
            // VM halted because a remote_call failed — surface the
            // responder's error envelope so curl / Open WebUI see a
            // proper non-2xx rather than an empty `reply: ""`. The
            // flow log on disk still records every step
            // (RemoteCallIssued / RemoteCallFailed / FlowFailed).
            if let Some(err) = result.last_error {
                let flow_id = result.flow_id.to_string();
                let flow_log_path = result.flow_log_path.to_string_lossy().to_string();
                let cause_for_event = err.clone();
                let kind = result.last_error_kind.unwrap_or(0);
                let class = FailureClass::from_kind(kind);
                if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                    rec.event(tid, "task.failed", &cause_for_event).await;
                    rec.fail(tid, kind, &cause_for_event, class).await;
                }
                return Err(FlowExecError::Transport(format!(
                    "flow halted: {err} (flow_id={flow_id} flow_log={flow_log_path})"
                )));
            }
            let reply = result.final_string.unwrap_or_default();
            let flow_id = result.flow_id.to_string();
            let trace_id = result.trace_id.to_string();
            let flow_log_path = result.flow_log_path.to_string_lossy().to_string();

            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                // Keep the reply that goes into task_events short so the
                // ledger doesn't carry the full bodies (those live in the
                // per-flow event log on disk, which task.latest_flow_log_path
                // points at).
                let excerpt = truncate(&reply, 200);
                rec.event(tid, "task.completed", &excerpt).await;
                rec.complete(tid, &excerpt, &flow_id, &flow_log_path).await;
            }
            // W5: record the assistant turn in the chronicle so
            // task.session_export reads the full transcript. The
            // full reply lands here, not the 200-char excerpt the
            // task ledger gets.
            if let Some(sid) = session_id_for_turn.as_deref() {
                record_chat_turn(
                    recorder,
                    task_id.as_ref(),
                    "chat.assistant_turn",
                    sid,
                    "assistant",
                    &reply,
                )
                .await;
            }
            Ok(FlowOutcome {
                reply,
                flow_id,
                trace_id,
                flow_log_path,
                task_id,
                workspace_lease_id: workspace.workspace_lease_id,
                workspace_path: workspace.workspace_path,
            })
        }
        Err(FlowRunnerError::Transport(m)) => {
            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                rec.event(tid, "task.failed", &m).await;
                // FlowRunner-layer transport failure (libp2p dial /
                // RPC), not a responder error envelope; classify as
                // transient and tag the kind as TRANSPORT so operator
                // tooling matches.
                rec.fail(
                    tid,
                    relix_core::types::error_kinds::TRANSPORT,
                    &m,
                    FailureClass::Transient,
                )
                .await;
            }
            Err(FlowExecError::Transport(m))
        }
        Err(e) => {
            let msg = e.to_string();
            if let (Some(rec), Some(tid)) = (recorder, task_id.as_ref()) {
                rec.event(tid, "task.failed", &msg).await;
                // Config / EventLog / Vm: not safe to retry without
                // operator action — surface as permanent so the CLI
                // colours it accordingly and bounded auto-retry (when
                // it lands) skips these.
                rec.fail(tid, 0, &msg, FailureClass::Permanent).await;
            }
            Err(FlowExecError::Internal(msg))
        }
    }
}

/// Execute one chat turn through the configured *tool-augmented* SOL flow
/// template (M9). Returns the same [`FlowOutcome`] shape so callers don't
/// have to switch on the variant — the only difference at this layer is the
/// `{{TOOL_URL}}` substitution, the fact that the flow performs an extra
/// `tool.web_fetch` remote call before the AI step, and the additional
/// `capability.invoked` event on the Task chronicle. SOL still owns
/// the orchestration; this function only selects the template.
pub async fn execute_chat_with_tool_flow(
    state: &AppState,
    session_id: &str,
    message: &str,
    url: &str,
    workspace_lease_id: Option<&str>,
) -> Result<FlowOutcome, FlowExecError> {
    let Some(tool_template) = state.tool_template.as_ref() else {
        return Err(FlowExecError::InvalidInput(
            "tool flow not configured (set [flow] tool_template_path in bridge config)".into(),
        ));
    };
    validate_input(session_id, message).map_err(FlowExecError::InvalidInput)?;
    validate_url(url).map_err(FlowExecError::InvalidInput)?;
    let workspace = resolve_workspace_binding(state, workspace_lease_id)?;

    let task_id = create_task(
        state.task_recorder.as_ref(),
        task_persistence_required(state),
        "chat_with_tool",
        "flows/chat_with_tool.sol",
        &chat_with_tool_params_json(session_id, message, url),
    )
    .await?;
    let trace_id = TraceId::new();
    let trace_hex = trace_id.to_string();
    if let (Some(rec), Some(tid)) = (state.task_recorder.as_ref(), task_id.as_ref()) {
        rec.event(tid, "flow.started", "flows/chat_with_tool.sol")
            .await;
        rec.start_running(tid, &trace_hex).await;
        // Pre-execution capability intent. Useful for operators
        // triaging failures: even if the tool peer rejects the URL,
        // the task chronicle says what was attempted.
        //
        // Payload format: `method=X target=Y peer=Z` where
        // `peer` is the alias the bridge expects to handle the
        // call. For chat_with_tool the resolution is static —
        // the SOL template hardcodes `remote_call("tool", …)`
        // so the alias is unambiguously "tool". Future flows
        // that use `capability:method` resolution will pull the
        // alias from `manifest_cache.find_alias_for_method`.
        //
        // The Phase-1D Execution path panel reads this field
        // and labels the row "recorded" instead of falling back
        // to the routing snapshot — the operator sees ground
        // truth, not a current-view inference.
        rec.event(
            tid,
            "capability.invoked",
            &format!("method=tool.web_fetch target={url} peer=tool"),
        )
        .await;
        // M35: the chat_with_tool template also calls ai.chat
        // after the web_fetch. Same honest recording — the SOL
        // template hardcodes `remote_call("ai", "ai.chat", …)`.
        rec.event(tid, "capability.invoked", "method=ai.chat peer=ai")
            .await;
    }
    // Phase 4: bind the created task to the workspace lease (if any).
    bind_workspace_active_run(state, &workspace, task_id.as_deref());
    // W5: record the user turn for the tool-augmented flow too.
    record_chat_turn(
        state.task_recorder.as_ref(),
        task_id.as_ref(),
        "chat.user_turn",
        session_id,
        "user",
        message,
    )
    .await;

    // SEC PART 3: safe two-pass substitution (see
    // `render_template_safe`).
    let rendered = render_template_safe(
        tool_template,
        &[
            ("SESSION", session_id),
            ("MESSAGE", message),
            ("TOOL_URL", url),
        ],
    );

    let tool_template_suffix = state
        .cfg
        .flow
        .tool_template_path
        .as_deref()
        .map(tempfile_suffix_for)
        .unwrap_or(".sol");
    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-tool-")
        .suffix(tool_template_suffix)
        .tempfile()
        .map_err(|e| FlowExecError::Internal(format!("tempfile: {e}")))?;
    std::fs::write(tmp.path(), rendered.as_bytes())
        .map_err(|e| FlowExecError::Internal(format!("write tempfile: {e}")))?;
    let flow_path: PathBuf = tmp.path().to_path_buf();

    let opts = FlowRunOptions {
        flow_path,
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key.clone(),
        peers: state.peers.clone(),
        data_dir: state.cfg.transport.data_dir.clone(),
        deadline_secs: state.cfg.transport.deadline_secs,
        capability_cache: Some(state.manifest_cache.clone()),
        mesh_client: state.mesh_client.clone(),
        trace_id: Some(trace_id),
        task_id: task_id.clone(),
        session_id: Some(session_id.to_string()),
        workspace_path: workspace.workspace_path.clone(),
        chunk_observer: None,
        cancel_signal: None,
        last_confidence_cell: Some(relix_runtime::confidence::LastConfidenceCell::new()),
    };

    finalize_flow_run(
        FlowRunner::new(opts).run().await,
        state.task_recorder.as_ref(),
        task_id,
        Some(session_id.to_string()),
        workspace,
    )
    .await
}

fn task_persistence_required(state: &AppState) -> bool {
    state
        .cfg
        .coordinator
        .as_ref()
        .is_some_and(|coord| coord.required)
}

/// Task creation. In local/dev mode this is fail-soft and returns `None`
/// when persistence is not configured or the Coordinator call fails. When
/// `[coordinator] required = true`, it fails before flow dispatch so work
/// cannot run without a durable task id.
///
/// On success, emits a `task.created` chronology event so the
/// timeline is self-describing from line 1 (no need to cross-
/// reference `tasks.created_at`).
async fn create_task(
    recorder: Option<&TaskRecorder>,
    required: bool,
    flow_label: &str,
    flow_template: &str,
    params_json: &str,
) -> Result<Option<String>, FlowExecError> {
    let Some(rec) = recorder else {
        return if required {
            Err(FlowExecError::Unavailable(
                "coordinator task persistence is required but unavailable".into(),
            ))
        } else {
            Ok(None)
        };
    };
    let title = make_title(flow_label, params_json, 64);
    let tid = if required {
        rec.create_required(&title, flow_template, params_json)
            .await
            .map_err(FlowExecError::Unavailable)?
    } else {
        let Some(tid) = rec.create(&title, flow_template, params_json).await else {
            return Ok(None);
        };
        tid
    };
    rec.event(&tid, "task.created", flow_template).await;
    Ok(Some(tid))
}

/// Build the wire payload for a `chat.user_turn` /
/// `chat.assistant_turn` chronicle event. The coordinator's
/// `task.session_export` capability parses this with
/// `splitn(4, '|')` so the `content` slot can carry its own
/// pipes verbatim.
pub fn chat_turn_payload(session_id: &str, role: &str, ts: i64, content: &str) -> String {
    format!("{session_id}|{role}|{ts}|{content}")
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Best-effort chronicle write for one chat turn. Silent
/// no-op when no coordinator is wired or `task_id` is None;
/// `TaskRecorder::event` already log-warns on transport
/// failures.
async fn record_chat_turn(
    recorder: Option<&TaskRecorder>,
    task_id: Option<&String>,
    event_type: &str,
    session_id: &str,
    role: &str,
    content: &str,
) {
    if let (Some(rec), Some(tid)) = (recorder, task_id) {
        let payload = chat_turn_payload(session_id, role, unix_now_secs(), content);
        rec.event(tid, event_type, &payload).await;
    }
}

/// Compact JSON for `task.create`'s `params_json`. Inline so we don't
/// pull serde_json's full machinery for two field types. The Coordinator
/// stores this verbatim and never parses it.
fn chat_params_json(session_id: &str, message: &str) -> String {
    let m = json_escape(message);
    let s = json_escape(session_id);
    format!(r#"{{"session_id":"{s}","message":"{m}"}}"#)
}

fn chat_with_tool_params_json(session_id: &str, message: &str, url: &str) -> String {
    let m = json_escape(message);
    let s = json_escape(session_id);
    let u = json_escape(url);
    format!(r#"{{"session_id":"{s}","message":"{m}","url":"{u}"}}"#)
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// RELIX-2 step 5: streaming variant of [`execute_chat_flow`].
/// Drives the same SOL flow pipeline but the rendered template
/// MUST use `remote_call_stream("ai", "ai.chat.stream", ...)`
/// so the flow opens a streaming substream against the AI
/// node. `on_chunk` fires for each chunk the AI provider
/// streams; the bridge's HTTP handler wires this to an SSE
/// channel that ships tokens to the client as they arrive.
///
/// The final returned `FlowOutcome` carries the concatenated
/// body (same shape as the unary path) so downstream
/// observability (memory recording, task ledger update,
/// chronicle event for chat.assistant_turn) sees the complete
/// turn — the streaming benefit is purely in WHEN the client
/// observes the tokens, not in WHETHER the final body is
/// captured.
pub async fn execute_chat_flow_streaming(
    state: &AppState,
    session_id: &str,
    message: &str,
    workspace_lease_id: Option<&str>,
    streaming_template: &str,
    on_chunk: relix_runtime::flow_runner::ChunkObserver,
    cancel_signal: relix_runtime::flow_runner::CancelSignal,
) -> Result<FlowOutcome, FlowExecError> {
    validate_input(session_id, message).map_err(FlowExecError::InvalidInput)?;
    let workspace = resolve_workspace_binding(state, workspace_lease_id)?;

    let task_id = create_task(
        state.task_recorder.as_ref(),
        task_persistence_required(state),
        "chat",
        "flows/chat_template_streaming.sol",
        &chat_params_json(session_id, message),
    )
    .await?;
    let trace_id = TraceId::new();
    let trace_hex = trace_id.to_string();
    if let (Some(rec), Some(tid)) = (state.task_recorder.as_ref(), task_id.as_ref()) {
        rec.event(tid, "flow.started", "flows/chat_template_streaming.sol")
            .await;
        rec.start_running(tid, &trace_hex).await;
        rec.event(tid, "capability.invoked", "method=ai.chat.stream peer=ai")
            .await;
    }
    // Phase 4: bind the created task to the workspace lease (if any).
    bind_workspace_active_run(state, &workspace, task_id.as_deref());
    record_chat_turn(
        state.task_recorder.as_ref(),
        task_id.as_ref(),
        "chat.user_turn",
        session_id,
        "user",
        message,
    )
    .await;

    // SEC PART 3: safe two-pass substitution.
    let rendered = render_template_safe(
        streaming_template,
        &[("SESSION", session_id), ("MESSAGE", message)],
    );
    let stream_template_suffix = state
        .cfg
        .flow
        .streaming_template_path
        .as_deref()
        .map(tempfile_suffix_for)
        .unwrap_or(".sol");
    let tmp = tempfile::Builder::new()
        .prefix("relix-bridge-chat-stream-")
        .suffix(stream_template_suffix)
        .tempfile()
        .map_err(|e| FlowExecError::Internal(format!("tempfile: {e}")))?;
    std::fs::write(tmp.path(), rendered.as_bytes())
        .map_err(|e| FlowExecError::Internal(format!("write tempfile: {e}")))?;
    let flow_path: PathBuf = tmp.path().to_path_buf();

    let opts = FlowRunOptions {
        flow_path,
        identity_bundle: state.identity_bundle.clone(),
        client_key: state.client_key.clone(),
        peers: state.peers.clone(),
        data_dir: state.cfg.transport.data_dir.clone(),
        deadline_secs: state.cfg.transport.deadline_secs,
        capability_cache: Some(state.manifest_cache.clone()),
        mesh_client: state.mesh_client.clone(),
        trace_id: Some(trace_id),
        task_id: task_id.clone(),
        session_id: Some(session_id.to_string()),
        workspace_path: workspace.workspace_path.clone(),
        chunk_observer: Some(on_chunk),
        cancel_signal: Some(cancel_signal),
        last_confidence_cell: Some(relix_runtime::confidence::LastConfidenceCell::new()),
    };

    finalize_flow_run(
        FlowRunner::new(opts).run().await,
        state.task_recorder.as_ref(),
        task_id,
        Some(session_id.to_string()),
        workspace,
    )
    .await
}

#[derive(Debug, Clone, Default)]
struct WorkspaceBinding {
    workspace_lease_id: Option<String>,
    workspace_path: Option<String>,
}

/// Phase 4 — when a chat created a durable task AND bound a workspace
/// lease, stamp the task onto the lease so the workspace's "active
/// run" reflects the work currently using it. Best-effort: a binding
/// failure is logged and never fails the chat itself.
fn bind_workspace_active_run(
    state: &AppState,
    workspace: &WorkspaceBinding,
    task_id: Option<&str>,
) {
    let (Some(lease_id), Some(tid)) = (workspace.workspace_lease_id.as_deref(), task_id) else {
        return;
    };
    if let Err(e) =
        crate::workspaces::bind_active_run_for_current_tenant(state, lease_id, tid, None)
    {
        tracing::warn!(error = %e, lease_id, task_id = tid, "workspace active-run bind failed");
    }
}

fn resolve_workspace_binding(
    state: &AppState,
    workspace_lease_id: Option<&str>,
) -> Result<WorkspaceBinding, FlowExecError> {
    let Some(lease_id) = workspace_lease_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(WorkspaceBinding::default());
    };
    let lease = crate::workspaces::resolve_active_lease_for_current_tenant(state, lease_id)
        .map_err(FlowExecError::InvalidInput)?;
    Ok(WorkspaceBinding {
        workspace_lease_id: Some(lease.lease_id),
        workspace_path: Some(lease.workspace_path),
    })
}

/// Truncate a string at `n` characters (not bytes), appending an
/// ellipsis when trimmed. Used to keep task_events payloads compact.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_quotes_and_newlines() {
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("plain"), "plain");
    }

    #[test]
    fn truncate_at_char_boundary() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc…");
        // multi-byte safety
        assert_eq!(truncate("αβγδε", 3), "αβ…");
    }

    #[test]
    fn chat_params_json_shape() {
        let s = chat_params_json("demo", "hello world");
        assert_eq!(s, r#"{"session_id":"demo","message":"hello world"}"#);
    }

    #[tokio::test]
    async fn required_task_creation_fails_when_recorder_unavailable() {
        let err = create_task(None, true, "chat", "flows/chat_template.sol", "{}")
            .await
            .unwrap_err();
        assert!(matches!(err, FlowExecError::Unavailable(_)));
    }

    #[tokio::test]
    async fn optional_task_creation_stays_fail_soft_without_recorder() {
        let task_id = create_task(None, false, "chat", "flows/chat_template.sol", "{}")
            .await
            .expect("optional create should not fail");
        assert!(task_id.is_none());
    }

    #[test]
    fn chat_turn_payload_round_trips_through_coordinator_parser() {
        // The W5 contract: the bridge's payload writer + the
        // coordinator's `parse_chat_turn_payload` are mirror
        // images. Pin them together so a future drift in
        // either side fails this test.
        let payload = chat_turn_payload("sess-A", "user", 1_700_000_001, "hello | world");
        assert_eq!(payload, "sess-A|user|1700000001|hello | world");
        let turn = relix_runtime::nodes::coordinator::parse_chat_turn_payload(
            "sess-A",
            "chat.user_turn",
            &payload,
            0,
        )
        .expect("payload parses");
        assert_eq!(turn.role, "user");
        assert_eq!(turn.content, "hello | world");
        assert_eq!(turn.timestamp_unix, 1_700_000_001);
        assert_eq!(turn.session_id, "sess-A");
    }

    // ── SEC PART 3: safe template substitution ───────────

    #[test]
    fn render_template_safe_handles_simple_substitution() {
        let template = "session={{SESSION}} message={{MESSAGE}}";
        let out = render_template_safe(template, &[("SESSION", "s1"), ("MESSAGE", "hi")]);
        assert_eq!(out, "session=s1 message=hi");
    }

    #[test]
    fn render_template_safe_no_double_substitution_when_value_contains_placeholder() {
        // Pre-fix path used `.replace("{{SESSION}}", session_id)` then
        // `.replace("{{MESSAGE}}", message)`; a session_id of
        // `xxx{{MESSAGE}}yyy` would trigger the second
        // substitution. The two-pass engine must NOT.
        let template = "S={{SESSION}} M={{MESSAGE}}";
        let out = render_template_safe(
            template,
            &[("SESSION", "alpha{{MESSAGE}}beta"), ("MESSAGE", "PWNED")],
        );
        // The {{MESSAGE}} inside SESSION must survive verbatim,
        // not be expanded.
        assert_eq!(out, "S=alpha{{MESSAGE}}beta M=PWNED");
        assert!(!out.contains("PWNEDbeta"), "double-sub detected: {out}");
    }

    #[test]
    fn render_template_safe_unknown_placeholder_passes_through_unchanged() {
        let template = "{{KNOWN}} {{UNKNOWN}}";
        let out = render_template_safe(template, &[("KNOWN", "yes")]);
        assert_eq!(out, "yes {{UNKNOWN}}");
    }

    #[test]
    fn render_template_safe_preserves_value_containing_three_open_braces() {
        // Defence in depth: a value with `{{{` survives the
        // sentinel-escape round trip without becoming a
        // double-substituted run.
        let template = "{{V}}";
        let out = render_template_safe(template, &[("V", "{{{notakey}}}")]);
        assert_eq!(out, "{{{notakey}}}");
    }
}
