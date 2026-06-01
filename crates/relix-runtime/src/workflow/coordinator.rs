//! Coordinator-side wiring for the workflow engine.
//!
//! Exposes a single `register` entry point that hangs four
//! capabilities off the dispatch bridge:
//!
//! - `workflow.run`      — run a workflow by name
//! - `workflow.list`     — enumerate available workflows
//! - `workflow.status`   — fetch a past execution by id
//! - `workflow.validate` — type-check a workflow source
//!
//! All four are unary. Args + responses are JSON unless
//! noted otherwise.
//!
//! Wire formats:
//!
//! - `workflow.run` args: `{"name": "<workflow>", "input": "<text>"}`
//! - `workflow.run` response: serialized [`chronicle::ExecutionRecord`]
//! - `workflow.list` response: array of `{name, description, version}`
//! - `workflow.status` args: `{"execution_id": "<hex>"}`
//! - `workflow.status` response: serialized [`chronicle::ExecutionRecord`]
//!   or `{"error": "..."}` on miss
//! - `workflow.validate` args: `{"source": "<yaml>"}`
//! - `workflow.validate` response: `{"ok": true, "name": "...", "warnings": []}`
//!   or `{"ok": false, "error": "..."}`

use std::collections::BTreeSet;
use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::{Deserialize, Serialize};

use super::chronicle::{ExecutionRecord, WorkflowChronicle, record_from};
use super::dispatcher::WorkflowDispatcher;
use super::executor::{WorkflowEvent, execute, execute_with_events};
use super::mesh_dispatcher::WorkflowDispatcherCell;
use super::parser::parse_str;
use super::store::{StoreError, WorkflowStore};
use super::validator::validate;
use crate::dispatch::{
    DispatchBridge, FnHandler, FnStreamingHandler, HandlerOutcome, HandlerStream, InvocationCtx,
};
use relix_core::types::error_kinds as ek;

/// Wire all four workflow capabilities into `bridge`. The
/// dispatcher cell is queried per-call — when empty (mesh
/// not yet up at startup) `workflow.run` returns a clear
/// "dispatcher not ready" error rather than panicking.
pub fn register(
    bridge: &mut DispatchBridge,
    store: WorkflowStore,
    chronicle: WorkflowChronicle,
    dispatcher_cell: WorkflowDispatcherCell,
    known_peers: Arc<BTreeSet<String>>,
) {
    register_run(
        bridge,
        store.clone(),
        chronicle.clone(),
        dispatcher_cell.clone(),
    );
    register_run_stream(bridge, store.clone(), chronicle.clone(), dispatcher_cell);
    register_list(bridge, store.clone());
    register_status(bridge, chronicle);
    register_validate(bridge, store.clone(), known_peers);
    register_reload(bridge, store);
}

#[derive(Debug, Deserialize)]
struct RunArgs {
    name: String,
    #[serde(default)]
    input: String,
}

#[derive(Debug, Deserialize)]
struct StatusArgs {
    execution_id: String,
}

#[derive(Debug, Deserialize)]
struct ValidateArgs {
    source: String,
}

#[derive(Debug, Serialize)]
struct ListEntry {
    name: String,
    description: String,
    version: u32,
    path: String,
}

#[derive(Debug, Serialize)]
struct ValidateOk {
    ok: bool,
    name: String,
    description: String,
    version: u32,
}

#[derive(Debug, Serialize)]
struct ValidateErr {
    ok: bool,
    error: String,
}

#[derive(Debug, Serialize)]
struct StatusMiss {
    error: String,
}

fn register_run(
    bridge: &mut DispatchBridge,
    store: WorkflowStore,
    chronicle: WorkflowChronicle,
    dispatcher_cell: WorkflowDispatcherCell,
) {
    bridge.register(
        "workflow.run",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            let chronicle = chronicle.clone();
            let cell = dispatcher_cell.clone();
            async move { handle_run(store, chronicle, cell, ctx).await }
        })),
    );
}

async fn handle_run(
    store: WorkflowStore,
    chronicle: WorkflowChronicle,
    dispatcher_cell: WorkflowDispatcherCell,
    ctx: InvocationCtx,
) -> HandlerOutcome {
    let args: RunArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(format!("workflow.run args must be JSON: {e}")),
    };
    let workflow = match store.get(&args.name) {
        Ok(w) => w,
        Err(e) => return store_err_to_outcome(&e),
    };
    let dispatcher: Arc<dyn WorkflowDispatcher> = match dispatcher_cell.get() {
        Some(d) => d.clone(),
        None => {
            return internal(
                "workflow dispatcher is not wired yet (mesh client still initialising); retry shortly"
                    .to_string(),
            );
        }
    };
    let started_at = unix_now();
    let result = execute(workflow, dispatcher, &args.input).await;
    let ended_at = unix_now();
    if let Err(e) = chronicle.record(
        &result,
        &args.input,
        started_at,
        ended_at,
        ctx.tenant_id_or_default(),
    ) {
        tracing::warn!(
            execution_id = %result.trace.execution_id,
            error = %e,
            "workflow.run: failed to persist execution to chronicle"
        );
    }
    let record = record_from(&result, &args.input, started_at, ended_at);
    match serde_json::to_vec(&record) {
        Ok(body) => HandlerOutcome::Ok(body),
        Err(e) => internal(format!("workflow.run: failed to encode response: {e}")),
    }
}

fn register_run_stream(
    bridge: &mut DispatchBridge,
    store: WorkflowStore,
    chronicle: WorkflowChronicle,
    dispatcher_cell: WorkflowDispatcherCell,
) {
    bridge.register_streaming(
        "workflow.run.stream",
        Arc::new(FnStreamingHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            let chronicle = chronicle.clone();
            let cell = dispatcher_cell.clone();
            async move { handle_run_stream(store, chronicle, cell, ctx).await }
        })),
    );
}

async fn handle_run_stream(
    store: WorkflowStore,
    chronicle: WorkflowChronicle,
    dispatcher_cell: WorkflowDispatcherCell,
    ctx: InvocationCtx,
) -> Result<HandlerStream, relix_core::types::ErrorEnvelope> {
    let args: RunArgs =
        serde_json::from_slice(&ctx.args).map_err(|e| relix_core::types::ErrorEnvelope {
            kind: ek::INVALID_ARGS,
            cause: format!("workflow.run.stream args must be JSON: {e}"),
            retry_hint: 2,
            retry_after: None,
        })?;
    let workflow = store
        .get(&args.name)
        .map_err(|e| relix_core::types::ErrorEnvelope {
            kind: ek::INVALID_ARGS,
            cause: e.to_string(),
            retry_hint: 2,
            retry_after: None,
        })?;
    let dispatcher: Arc<dyn WorkflowDispatcher> = dispatcher_cell.get().cloned().ok_or_else(|| {
        relix_core::types::ErrorEnvelope {
            kind: ek::RESPONDER_INTERNAL,
            cause:
                "workflow dispatcher is not wired yet (mesh client still initialising); retry shortly"
                    .to_string(),
            retry_hint: 1,
            retry_after: None,
        }
    })?;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<WorkflowEvent>();
    // Drive execution in the background; this task owns the
    // sender so when the workflow finishes, the channel
    // closes and the stream consumer sees EOF after the
    // terminal `Finished` event.
    let input = args.input.clone();
    // GROUP 6: capture the verified tenant before moving into the
    // spawned task so the chronicle row is attributed correctly.
    let tenant = ctx.tenant_id_or_default().to_string();
    let started_at = unix_now();
    tokio::spawn(async move {
        let result = execute_with_events(workflow, dispatcher, &input, event_tx).await;
        let ended_at = unix_now();
        if let Err(e) = chronicle.record(&result, &input, started_at, ended_at, &tenant) {
            tracing::warn!(
                execution_id = %result.trace.execution_id,
                error = %e,
                "workflow.run.stream: failed to persist execution to chronicle"
            );
        }
    });

    // Adapt the unbounded receiver into the streaming
    // handler's bytes-or-error stream. Each WorkflowEvent
    // becomes one CBOR-framed JSON chunk; the terminal
    // `Finished` event closes the stream naturally.
    let stream = async_stream::stream! {
        while let Some(event) = event_rx.recv().await {
            match render_event(&event) {
                Ok(bytes) => yield Ok(bytes),
                Err(e) => {
                    yield Err(relix_core::types::ErrorEnvelope {
                        kind: ek::RESPONDER_INTERNAL,
                        cause: format!("workflow event encode: {e}"),
                        retry_hint: 1,
                        retry_after: None,
                    });
                    return;
                }
            }
        }
    };
    Ok(Box::pin(stream) as HandlerStream)
}

/// Render one [`WorkflowEvent`] into the JSON shape the
/// bridge re-emits as a `text/event-stream` event. Format
/// matches the table in `docs/workflows.md` so the wire
/// shape is stable across the runtime + bridge boundary.
fn render_event(event: &WorkflowEvent) -> serde_json::Result<Vec<u8>> {
    let value = match event {
        WorkflowEvent::Started {
            execution_id,
            workflow_name,
        } => serde_json::json!({
            "event": "started",
            "execution_id": execution_id.0,
            "workflow_name": workflow_name,
        }),
        WorkflowEvent::StepStarted {
            agent,
            peer,
            capability,
            input,
        } => serde_json::json!({
            "event": "step_started",
            "agent": agent,
            "peer": peer,
            "capability": capability,
            "input": input,
        }),
        WorkflowEvent::StepCompleted {
            agent,
            peer,
            capability,
            latency_ms,
            output,
        } => serde_json::json!({
            "event": "step_completed",
            "agent": agent,
            "peer": peer,
            "capability": capability,
            "latency_ms": latency_ms,
            "output": output,
        }),
        WorkflowEvent::StepFailed {
            agent,
            peer,
            capability,
            latency_ms,
            error,
        } => serde_json::json!({
            "event": "step_failed",
            "agent": agent,
            "peer": peer,
            "capability": capability,
            "latency_ms": latency_ms,
            "error": error,
        }),
        WorkflowEvent::Finished(result) => {
            let record = record_from(result, "", 0, 0);
            serde_json::json!({
                "event": "finished",
                "execution_id": record.execution_id,
                "workflow_name": record.workflow_name,
                "status": record.status,
                "result": record.result,
                "total_latency_ms": record.total_latency_ms,
                "steps": record.steps,
            })
        }
        WorkflowEvent::Cancelled { agent, reason } => serde_json::json!({
            "event": "cancelled",
            "agent": agent,
            "reason": reason,
        }),
    };
    serde_json::to_vec(&value)
}

fn register_list(bridge: &mut DispatchBridge, store: WorkflowStore) {
    bridge.register(
        "workflow.list",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let store = store.clone();
            async move { handle_list(store) }
        })),
    );
}

fn handle_list(store: WorkflowStore) -> HandlerOutcome {
    let entries = match store.list() {
        Ok(v) => v,
        Err(e) => return store_err_to_outcome(&e),
    };
    let out: Vec<ListEntry> = entries
        .into_iter()
        .map(|e| ListEntry {
            name: e.name,
            description: e.description,
            version: e.version,
            path: e.path.display().to_string(),
        })
        .collect();
    match serde_json::to_vec(&out) {
        Ok(body) => HandlerOutcome::Ok(body),
        Err(e) => internal(format!("workflow.list encode: {e}")),
    }
}

fn register_status(bridge: &mut DispatchBridge, chronicle: WorkflowChronicle) {
    bridge.register(
        "workflow.status",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let chronicle = chronicle.clone();
            async move { handle_status(chronicle, ctx) }
        })),
    );
}

fn handle_status(chronicle: WorkflowChronicle, ctx: InvocationCtx) -> HandlerOutcome {
    let args: StatusArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(format!("workflow.status args must be JSON: {e}")),
    };
    match chronicle.get(&args.execution_id) {
        Ok(Some(rec)) => match serde_json::to_vec(&rec) {
            Ok(body) => HandlerOutcome::Ok(body),
            Err(e) => internal(format!("workflow.status encode: {e}")),
        },
        Ok(None) => {
            let body = serde_json::to_vec(&StatusMiss {
                error: format!("execution `{}` not found", args.execution_id),
            })
            .unwrap_or_default();
            HandlerOutcome::Ok(body)
        }
        Err(e) => internal(format!("workflow.status chronicle: {e}")),
    }
}

fn register_validate(
    bridge: &mut DispatchBridge,
    store: WorkflowStore,
    known_peers: Arc<BTreeSet<String>>,
) {
    bridge.register(
        "workflow.validate",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let _store = store.clone();
            let known_peers = known_peers.clone();
            async move { handle_validate(ctx, known_peers) }
        })),
    );
}

fn handle_validate(ctx: InvocationCtx, known_peers: Arc<BTreeSet<String>>) -> HandlerOutcome {
    let args: ValidateArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(format!("workflow.validate args must be JSON: {e}")),
    };
    let parsed = match parse_str(&args.source) {
        Ok(w) => w,
        Err(e) => {
            let body = serde_json::to_vec(&ValidateErr {
                ok: false,
                error: format!("parse error: {e}"),
            })
            .unwrap_or_default();
            return HandlerOutcome::Ok(body);
        }
    };
    let peer_check: Option<&BTreeSet<String>> = if known_peers.is_empty() {
        None
    } else {
        Some(known_peers.as_ref())
    };
    if let Err(e) = validate(&parsed, peer_check) {
        let body = serde_json::to_vec(&ValidateErr {
            ok: false,
            error: format!("validation error: {e}"),
        })
        .unwrap_or_default();
        return HandlerOutcome::Ok(body);
    }
    let body = serde_json::to_vec(&ValidateOk {
        ok: true,
        name: parsed.name,
        description: parsed.description,
        version: parsed.version,
    })
    .unwrap_or_default();
    HandlerOutcome::Ok(body)
}

fn register_reload(bridge: &mut DispatchBridge, store: WorkflowStore) {
    bridge.register(
        "workflow.reload",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                store.clear_cache();
                let body =
                    serde_json::to_vec(&serde_json::json!({ "ok": true })).unwrap_or_default();
                HandlerOutcome::Ok(body)
            }
        })),
    );
}

fn store_err_to_outcome(e: &StoreError) -> HandlerOutcome {
    match e {
        StoreError::NotFound { .. } => invalid(e.to_string()),
        StoreError::Parse { .. } => invalid(e.to_string()),
        StoreError::DirMissing(_) => invalid(e.to_string()),
        // SECTION 8: a path-traversal name or an oversize file is
        // a bad-request from the caller — surface as INVALID_ARGS,
        // never a 500.
        StoreError::InvalidName { .. } | StoreError::TooLarge { .. } => invalid(e.to_string()),
        StoreError::DirIo { .. } | StoreError::FileIo { .. } => internal(e.to_string()),
    }
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Helper used by [`record_from`] callers that want the
/// execution serialised as JSON without going through the
/// handler chain.
#[allow(dead_code)]
pub fn execution_record_json(record: &ExecutionRecord) -> Vec<u8> {
    serde_json::to_vec(record).unwrap_or_default()
}
