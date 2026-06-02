//! Real `RemoteCallDispatcher` wired to libp2p RPC + per-flow event log (M6/S4).
//!
//! This module is the integration seam between the SOL VM and the M5 transport
//! layer. A `relix-cli flow-run` invocation (or any future host) builds a
//! [`FlowRunner`] and calls [`FlowRunner::run`]. The runner:
//!
//! 1. Brings up an ephemeral libp2p client (a `relix-runtime::transport::rpc`
//!    instance bound to a random local port).
//! 2. Resolves the configured peer aliases to libp2p `PeerId`s by dialing each
//!    and waiting for `PeerConnected`.
//! 3. Opens a per-flow event log keyed by a fresh `flow_id`.
//! 4. Compiles the SOL source through the verbatim port pipeline
//!    (lexer → parser → analyzer → codegen).
//! 5. Spawns the VM on `tokio::task::spawn_blocking` so the synchronous SOL
//!    interpreter can safely `block_on` the async libp2p client without
//!    poisoning the tokio worker pool — the [SIMP-014] bridge.
//! 6. Attaches a [`RealDispatcher`] that, for each `Inst::RemoteCall`,
//!    writes a `RemoteCallIssued` event **before** sending (log-before-act),
//!    issues the RPC through the real M5 path, decodes the response envelope,
//!    and writes either `RemoteCallCompleted` or `RemoteCallFailed`.
//! 7. Writes the terminal `FlowCompleted` / `FlowFailed` event and returns the
//!    final VM result.
//!
//! No mock transport, no policy bypass, no direct handler invocation. The
//! responder runs the full M5 admission pipeline (decode → deadline → identity
//! → capability lookup → policy → handler → audit) on every call.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::eventlog::{EventLog, EventType};
use relix_core::types::{FlowId, RequestId, TraceId};

use crate::dispatch::{build_request_with_surface, decode_response};
use crate::manifest::{ManifestCache, MeshClient};
use crate::sflow;
use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError, RemoteCallResult};
use crate::sol::vm::{VM, VM_ERROR_SENTINEL};
use crate::transport::envelope::ResponseResult;
use crate::transport::rpc::{self, Event as TransportEvent, Multiaddr, PeerId};

// ──────────────────────────── Peer alias config ────────────────────────────

/// `--peers <file>` content. A small TOML keyed by alias.
///
/// ```toml
/// [peers.memory]
/// addr = "/ip4/127.0.0.1/tcp/9001"
///
/// [peers.ai]
/// addr = "/ip4/127.0.0.1/tcp/9002"
/// ```
///
/// SIMP: alpha-only abstraction. Production (Gate 2+) resolves peers via
/// gossipsub'd manifests instead of a flat file.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PeersFile {
    /// `[peers.<alias>]` table.
    #[serde(default)]
    pub peers: HashMap<String, PeerEntry>,
}

/// One alias entry.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PeerEntry {
    /// Full libp2p multiaddr to dial. `/ip4/127.0.0.1/tcp/9001` for alpha demos.
    pub addr: String,
}

impl PeersFile {
    /// Load from a TOML file on disk.
    pub fn from_path(path: &Path) -> Result<Self, FlowRunnerError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| FlowRunnerError::Config(format!("{}: {}", path.display(), e)))?;
        let file: PeersFile = toml::from_str(&text)
            .map_err(|e| FlowRunnerError::Config(format!("{}: {}", path.display(), e)))?;
        Ok(file)
    }
}

// ──────────────────────────── FlowRunner ────────────────────────────────────

/// Options for a single `flow-run` invocation.
pub struct FlowRunOptions {
    /// Path to the `.sol` source file.
    pub flow_path: PathBuf,
    /// Caller's signed identity bundle (`relix-cli identity mint` output).
    pub identity_bundle: Bundle,
    /// 32-byte Ed25519 secret used as the local libp2p PeerId AND as the
    /// signer for the per-flow event log records. The libp2p PeerId is
    /// independent of the caller's identity subject_id (alpha SIMP — see
    /// docs/sol-runtime-analysis.md §6).
    ///
    /// SEC PART 2: wrapped in `Zeroizing` so the secret-key
    /// bytes are wiped from the heap when the `FlowRunOptions`
    /// (or any future clones) goes out of scope.
    pub client_key: zeroize::Zeroizing<[u8; 32]>,
    /// Peer alias map.
    pub peers: PeersFile,
    /// Where the per-flow event log goes. Defaults to
    /// `<RELIX_DATA_DIR or ~/.relix>/flow-runner/flows/<flow_id>.log`.
    pub data_dir: Option<PathBuf>,
    /// Per-call deadline in seconds (default 30).
    pub deadline_secs: i64,
    /// Optional discovered capability cache. When present, SOL flows may
    /// use a `capability:<method>` peer alias and the dispatcher will
    /// translate it to a concrete alias before issuing the RPC. Existing
    /// `remote_call("memory", ...)` calls are unaffected. (M10.3)
    #[allow(missing_docs)]
    pub capability_cache: Option<std::sync::Arc<ManifestCache>>,
    /// Optional pre-built [`MeshClient`] — when present, FlowRunner reuses
    /// the existing libp2p transport and the already-resolved peer ids
    /// instead of bringing up its own ephemeral peer per request. This is
    /// the main M11 speedup; CLI standalone callers leave it `None`.
    #[allow(missing_docs)]
    pub mesh_client: Option<std::sync::Arc<MeshClient>>,
    /// Optional caller-supplied trace id. When the bridge mints a
    /// trace_id upfront (so it can stamp the same id on both the
    /// Coordinator's attempt row and the per-flow event log),
    /// providing it here keeps the two in sync. `None` means the
    /// runner generates its own.
    pub trace_id: Option<TraceId>,
    /// Optional coordinator task id this flow is executing under.
    /// When present, every outbound SOL `remote_call` envelope is
    /// stamped with the same task id so responder-side approval gates,
    /// audits, and task-pausing logic can bind risky capability calls
    /// back to the durable work item.
    pub task_id: Option<String>,
    /// Optional session id this flow is executing under. Bridge chat
    /// and OpenAI-shim flows set this so session-scoped standing
    /// approvals can cover every tool/AI call inside the turn.
    pub session_id: Option<String>,
    /// Optional workspace path this flow is executing inside. Future
    /// workspace leases can set this so workspace-scoped standing
    /// approvals match the concrete run location.
    pub workspace_path: Option<String>,
    /// RELIX-2 step 5: optional chunk observer. Wired by the
    /// web bridge when serving a `stream: true` chat request.
    /// Each chunk yielded by a `remote_call_stream` opcode
    /// fires the callback synchronously, in arrival order,
    /// BEFORE the VM has finished collecting the concatenated
    /// result. The bridge uses this to ship tokens to an SSE
    /// HTTP response while the SOL flow is still running.
    /// `None` (default) means no observer — `remote_call_stream`
    /// still works but per-chunk callbacks are no-ops.
    pub chunk_observer: Option<ChunkObserver>,
    /// RELIX-2 step 5b: optional cancellation signal. When
    /// notified, an in-flight `remote_call_stream` aborts its
    /// libp2p substream read and returns a structured
    /// TRANSPORT error. The flow runner then writes the usual
    /// `FlowFailed` / `task.failed` audit trail so the
    /// chronicle honestly records the cancellation. `None`
    /// means no cancellation hook — `remote_call_stream` runs
    /// to natural completion.
    pub cancel_signal: Option<CancelSignal>,
    /// RELIX-7.19 GAP 4: shared last-confidence cell. When
    /// wired, every `remote_call` reads the responder's
    /// stamped `ResponseEnvelope::confidence` and writes it
    /// to the cell BEFORE returning to the SOL VM, so
    /// `last_confidence()` sees the latest dispatch-level
    /// score. The cell is also installed on the VM so the
    /// `Inst::LoadLastConfidence` opcode reads from the same
    /// storage. `None` keeps pre-7.19 behaviour (cell starts
    /// at 1.0 and never updates).
    pub last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
}

/// RELIX-2 step 5: callback the bridge supplies via
/// [`FlowRunOptions::chunk_observer`]. Aliased so the
/// type signature stays out of clippy's
/// `type_complexity` warning at every call site and the
/// `[Arc<dyn Fn(&[u8]) + Send + Sync>]` shape lives in
/// one place.
pub type ChunkObserver = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// RELIX-2 step 5b: cancellation signal the bridge wires
/// through to the streaming dispatcher. The bridge holds the
/// notify clone; when the HTTP SSE consumer drops (client
/// disconnect), a `Drop` guard fires `notify_one()`, the
/// streaming dispatcher's `tokio::select!` against
/// `notified()` triggers, and the in-flight
/// `remote_call_stream` returns a structured TRANSPORT error.
/// The flow runner then writes the usual
/// `FlowFailed` / `task.failed` audit events — the audit log
/// honestly records "client cancelled mid-stream" instead of
/// silently dropping the response.
pub type CancelSignal = Arc<tokio::sync::Notify>;

/// What `FlowRunner::run` returns to the caller (and `relix-cli flow-run`
/// prints).
pub struct FlowRunResult {
    /// New flow id.
    pub flow_id: FlowId,
    /// Path of the flow log on disk.
    pub flow_log_path: PathBuf,
    /// Trace id (for cross-node audit correlation).
    pub trace_id: TraceId,
    /// VM exit value. For successful flows whose final value is a heap-string
    /// reference, the resolved string is in `final_string`.
    pub vm_exit: u64,
    /// Resolved final value as a UTF-8 string when the program returned a
    /// heap-string ref. None for non-string results (or for VM_ERROR_SENTINEL).
    pub final_string: Option<String>,
    /// Last RemoteCall error from the dispatcher, when the flow halted with
    /// VM_ERROR_SENTINEL.
    pub last_error: Option<String>,
    /// `error_kinds::*` value of the last RemoteCall error, when known.
    /// `None` when the flow halted with no remote-call error attached;
    /// `Some(0)` indicates a dispatcher-local failure (no peer reached).
    /// Carried so the bridge can derive a `FailureClass` for the Task
    /// without re-parsing `last_error`.
    pub last_error_kind: Option<u32>,
}

/// One run. Constructed and `.run()` consumes it.
pub struct FlowRunner {
    opts: FlowRunOptions,
}

impl FlowRunner {
    /// Wrap options.
    pub fn new(opts: FlowRunOptions) -> Self {
        Self { opts }
    }

    /// Execute the flow. Must run on a multi-threaded tokio runtime; the VM
    /// is moved onto `spawn_blocking` so the dispatcher can `block_on` libp2p.
    ///
    /// When [`FlowRunOptions::mesh_client`] is `Some`, the persistent
    /// libp2p client is reused — the TCP + Noise + Yamux handshake is paid
    /// **once** at bridge startup instead of per chat request. Otherwise the
    /// runner brings up its own ephemeral peer (used by `relix-cli flow-run`).
    pub async fn run(self) -> Result<FlowRunResult, FlowRunnerError> {
        let Self { opts } = self;

        let (client, peer_ids) = if let Some(mesh) = opts.mesh_client.clone() {
            // M11 fast path: zero per-request handshakes.
            (mesh.client(), mesh.peer_ids())
        } else {
            // Fallback path (CLI standalone).
            let local_port = 21_000 + (rand::random::<u16>() % 8_000);
            let (client, mut events, event_loop) = rpc::new(*opts.client_key, local_port)
                .await
                .map_err(|e| FlowRunnerError::Transport(format!("rpc::new: {e}")))?;
            tokio::spawn(event_loop.run());
            let peer_ids = dial_all_peers(&client, &mut events, &opts.peers).await?;
            (client, peer_ids)
        };

        // 3. Open the per-flow event log. The flow_log_signer = the local
        //    client_key; the log records are signed by whoever ran the flow
        //    (alpha-equivalent of "the owning controller" per RELIX-3 §3.2).
        let flow_id = FlowId::new();
        // Use the caller's trace_id if one was supplied (C2b.1) so
        // the per-flow event log and the Coordinator's attempt row
        // share the same correlation id. Otherwise mint one.
        let trace_id = opts.trace_id.unwrap_or_else(TraceId::new);
        let signer = SigningKey::from_bytes(&opts.client_key);
        let flow_log_path = resolve_flow_log_path(&opts.data_dir, flow_id);
        let event_log = EventLog::open(&flow_log_path, flow_id, signer.clone())
            .map_err(|e| FlowRunnerError::EventLog(format!("open: {e}")))?;
        let event_log = Arc::new(Mutex::new(event_log));

        // 4. Write FlowStarted (log-before-act: execution about to begin).
        let started_payload = encode_flow_started_payload(&opts, trace_id);
        append_log(&event_log, EventType::FlowStarted, started_payload)?;

        // 5. Build dispatcher (shared by both SOL and Sflow paths).
        let dispatcher: Arc<dyn RemoteCallDispatcher> = Arc::new(RealDispatcher {
            client: client.clone(),
            peer_ids,
            identity: opts.identity_bundle.clone(),
            trace_id,
            event_log: event_log.clone(),
            handle: tokio::runtime::Handle::current(),
            deadline_secs: opts.deadline_secs,
            capability_cache: opts.capability_cache.clone(),
            task_id: opts.task_id.clone(),
            session_id: opts.session_id.clone(),
            workspace_path: opts.workspace_path.clone(),
            mesh: opts.mesh_client.clone(),
            cancel_signal: opts.cancel_signal.clone(),
            last_confidence_cell: opts.last_confidence_cell.clone(),
        });

        // 6. Dispatch on file extension. `.sflow` runs the AST-walking
        //    executor. `.yml` / `.yaml` flows go through the YAML
        //    frontend (`crate::yaml_flow`) which lowers to SOL source
        //    text before falling into the SOL pipeline — same VM,
        //    same opcodes, same dispatcher, same chunk observer.
        //    Everything else (default `.sol`) compiles to the SOL
        //    bytecode directly and runs the VM.
        let ext = opts
            .flow_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());

        let (vm_exit, last_err, final_string) = match ext.as_deref() {
            Some("sflow") => {
                run_sflow(&opts.flow_path, dispatcher.clone(), event_log.clone()).await?
            }
            Some("yml") | Some("yaml") => {
                run_yaml(
                    &opts.flow_path,
                    dispatcher.clone(),
                    opts.chunk_observer.clone(),
                    opts.last_confidence_cell.clone(),
                )
                .await?
            }
            _ => {
                run_sol(
                    &opts.flow_path,
                    dispatcher.clone(),
                    opts.chunk_observer.clone(),
                    opts.last_confidence_cell.clone(),
                )
                .await?
            }
        };

        // 7. Terminal event.
        if vm_exit == VM_ERROR_SENTINEL {
            let cause = last_err
                .as_ref()
                .map(|e| format!("{e}"))
                .unwrap_or_else(|| "vm halted with sentinel".to_string());
            append_log(&event_log, EventType::FlowFailed, cause.as_bytes().to_vec())?;
        } else {
            let payload = final_string
                .clone()
                .unwrap_or_else(|| format!("vm_exit={vm_exit}"));
            append_log(&event_log, EventType::FlowCompleted, payload.into_bytes())?;
        }

        let last_error_kind = last_err.as_ref().map(|e| e.kind);
        let last_error = last_err.map(|e| e.to_string());
        Ok(FlowRunResult {
            flow_id,
            flow_log_path,
            trace_id,
            vm_exit,
            final_string,
            last_error,
            last_error_kind,
        })
    }
}

// ──────────────────────────── Per-language run helpers ──────────────────────

/// Run a `.sol` flow through the existing VM. Returns `(exit, last_err,
/// final_string)` matching the original FlowRunner::run shape so the caller
/// can write a uniform FlowCompleted / FlowFailed terminal event.
async fn run_sol(
    flow_path: &Path,
    dispatcher: Arc<dyn RemoteCallDispatcher>,
    chunk_observer: Option<ChunkObserver>,
    last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
) -> Result<(u64, Option<RemoteCallError>, Option<String>), FlowRunnerError> {
    // PART 1: SOL compile reads from disk + parses; both block.
    // Move it to the blocking pool so a tokio worker stays free.
    let flow_path_owned = flow_path.to_path_buf();
    let flow_name = flow_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<sol_flow>")
        .to_string();
    let compiled = tokio::task::spawn_blocking(move || compile_sol(&flow_path_owned))
        .await
        .map_err(|e| FlowRunnerError::Vm(format!("spawn_blocking join: {e}")))??;
    let vm_result = tokio::task::spawn_blocking(move || {
        let mut vm_builder = VM::from(&compiled.bytecode)
            .with_dispatcher(dispatcher)
            // P6: thread the per-flow fuel budget into the
            // VM. compile_sol resolves `#steps` directives;
            // when the source carries one it wins, otherwise
            // [`crate::sol::DEFAULT_MAX_STEPS`] applies.
            .with_fuel(compiled.max_steps)
            .with_flow_name(flow_name);
        if let Some(observer) = chunk_observer {
            vm_builder = vm_builder.with_chunk_observer(observer);
        }
        // RELIX-7.19 GAP 4: install the shared last-confidence
        // cell so the SOL `last_confidence()` builtin reads
        // the same storage the dispatcher writes after every
        // remote_call.
        if let Some(cell) = last_confidence_cell {
            vm_builder = vm_builder.with_last_confidence_cell(cell);
        }
        let mut vm = vm_builder;
        let exit = vm.run();
        let last_err = vm.last_error().cloned();
        let final_string = if exit == VM_ERROR_SENTINEL {
            None
        } else {
            vm.heap_string(exit).map(|s| s.to_string())
        };
        (exit, last_err, final_string)
    })
    .await
    .map_err(|e| FlowRunnerError::Vm(format!("spawn_blocking join: {e}")))?;
    Ok(vm_result)
}

/// Run a `.yml` / `.yaml` flow through the YAML frontend.
/// The frontend lowers to SOL source text and hands off to the
/// existing SOL compile pipeline, so YAML flows execute on the
/// exact same VM as `.sol` flows. Chunk observers and cancel
/// signals work identically.
async fn run_yaml(
    flow_path: &Path,
    dispatcher: Arc<dyn RemoteCallDispatcher>,
    chunk_observer: Option<ChunkObserver>,
    last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
) -> Result<(u64, Option<RemoteCallError>, Option<String>), FlowRunnerError> {
    // PART 1: file I/O + parsing run on the blocking pool so
    // we never block a tokio runtime worker on disk.
    let flow_name = flow_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<yaml_flow>")
        .to_string();
    let bytecode = crate::yaml_flow::compile_path_async(flow_path.to_path_buf())
        .await
        .map_err(|e| FlowRunnerError::Config(format!("yaml flow {}: {e}", flow_path.display())))?;
    let vm_result = tokio::task::spawn_blocking(move || {
        // P6: YAML flows compile via the yaml_flow frontend
        // which does not honour `#steps`; they get the SOL
        // crate's default fuel budget.
        let mut vm_builder = VM::from(&bytecode)
            .with_dispatcher(dispatcher)
            .with_fuel(crate::sol::DEFAULT_MAX_STEPS)
            .with_flow_name(flow_name);
        if let Some(observer) = chunk_observer {
            vm_builder = vm_builder.with_chunk_observer(observer);
        }
        if let Some(cell) = last_confidence_cell {
            vm_builder = vm_builder.with_last_confidence_cell(cell);
        }
        let mut vm = vm_builder;
        let exit = vm.run();
        let last_err = vm.last_error().cloned();
        let final_string = if exit == VM_ERROR_SENTINEL {
            None
        } else {
            vm.heap_string(exit).map(|s| s.to_string())
        };
        (exit, last_err, final_string)
    })
    .await
    .map_err(|e| FlowRunnerError::Vm(format!("spawn_blocking join: {e}")))?;
    Ok(vm_result)
}

/// Run a `.sflow` flow through the AST executor. Translates the executor's
/// `ExecOutcome` into the same `(exit, last_err, final_string)` tuple the
/// SOL path returns so the terminal-event branch above is shared.
async fn run_sflow(
    flow_path: &Path,
    dispatcher: Arc<dyn RemoteCallDispatcher>,
    event_log: Arc<Mutex<EventLog>>,
) -> Result<(u64, Option<RemoteCallError>, Option<String>), FlowRunnerError> {
    // PART 1: file I/O + parsing are blocking. Hand both off to
    // the blocking pool so the async caller's runtime worker
    // stays free.
    let flow_path_owned = flow_path.to_path_buf();
    let program = tokio::task::spawn_blocking(move || -> Result<_, FlowRunnerError> {
        let source = std::fs::read_to_string(&flow_path_owned).map_err(|e| {
            FlowRunnerError::Config(format!("read {}: {}", flow_path_owned.display(), e))
        })?;
        sflow::compile(&source).map_err(|e| FlowRunnerError::Config(format!("sflow parse: {e}")))
    })
    .await
    .map_err(|e| FlowRunnerError::Vm(format!("spawn_blocking join: {e}")))??;
    let chronicle: Arc<dyn sflow::executor::ChronicleSink> =
        Arc::new(EventLogChronicle { log: event_log });
    let exe = sflow::Executor::new(dispatcher, chronicle);
    let outcome = tokio::task::spawn_blocking(move || exe.run(&program))
        .await
        .map_err(|e| FlowRunnerError::Vm(format!("spawn_blocking join: {e}")))?;
    match outcome.error {
        Some(err) => Ok((
            VM_ERROR_SENTINEL,
            Some(RemoteCallError {
                kind: err.error_kind,
                peer: String::new(),
                method: format!("sflow:{}", err.kind.as_str()),
                cause: err.message,
            }),
            None,
        )),
        None => Ok((0, None, Some(outcome.result))),
    }
}

/// Chronicle sink that writes Sflow events into the per-flow EventLog using
/// `RemoteCallIssued` as a transport — the on-disk format is unchanged
/// (still signed, hash-chained CBOR records), but each Sflow event is
/// prefixed with `event=<sol.*>` so flow-log readers and operator tooling
/// can recognise it. Adding a dedicated EventType variant would change
/// the signed record format, which is a Gate 2 concern.
struct EventLogChronicle {
    log: Arc<Mutex<EventLog>>,
}

impl sflow::executor::ChronicleSink for EventLogChronicle {
    fn write(&self, kind: &str, payload: &str) {
        let body = format!("event={kind}\n{payload}\n").into_bytes();
        if let Ok(mut g) = self.log.lock() {
            // Best-effort: a write failure here must not crash the flow
            // (the executor has no path to surface it cleanly). The
            // terminal FlowCompleted / FlowFailed event will still be
            // written by the caller.
            let _ = g.append(EventType::RemoteCallIssued, body);
        }
    }
}

// ──────────────────────────── Dispatcher impl ───────────────────────────────

struct RealDispatcher {
    client: rpc::Client,
    peer_ids: HashMap<String, PeerId>,
    identity: Bundle,
    trace_id: TraceId,
    event_log: Arc<Mutex<EventLog>>,
    handle: tokio::runtime::Handle,
    deadline_secs: i64,
    capability_cache: Option<Arc<ManifestCache>>,
    task_id: Option<String>,
    session_id: Option<String>,
    workspace_path: Option<String>,
    /// When present, calls go through [`MeshClient::call`] which adds
    /// reconnect-on-transport-failure behaviour. When absent (the
    /// `relix-cli flow-run` path), we keep the original direct
    /// `Client::call(peer_id, ..)` flow.
    mesh: Option<Arc<MeshClient>>,
    /// RELIX-2 step 5b: cancellation signal. The streaming
    /// dispatcher's frame-read loop selects against
    /// `cancel_signal.notified()` so the bridge can cancel
    /// the in-flight substream read when the SSE consumer
    /// drops.
    cancel_signal: Option<CancelSignal>,
    /// RELIX-7.19 GAP 4: shared last-confidence cell. After
    /// every successful `remote_call` the dispatcher reads
    /// `ResponseEnvelope::confidence` and writes it here so
    /// the SOL `last_confidence()` builtin reflects the
    /// responder's score. `None` keeps pre-7.19 behaviour
    /// (cell stays at 1.0).
    last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
}

impl RealDispatcher {
    /// RELIX-2 step 5: resolve a peer alias to a libp2p
    /// PeerId. Shared between `remote_call` (unary) and
    /// `remote_call_stream` (streaming). Returns the resolved
    /// alias + peer id, or a structured error.
    fn resolve_peer(
        &self,
        peer_alias: &str,
        method: &str,
    ) -> Result<(String, PeerId), RemoteCallError> {
        let resolved_alias: String = if let Some(method_target) =
            peer_alias.strip_prefix("capability:")
        {
            let cache = match self.capability_cache.as_ref() {
                Some(c) => c,
                None => {
                    return Err(RemoteCallError::local(
                        peer_alias,
                        method,
                        "capability resolution requires a populated ManifestCache (host not wired)"
                            .to_string(),
                    ));
                }
            };
            match cache.find_alias_for_method(method_target) {
                Some(a) => a,
                None => {
                    return Err(RemoteCallError::local(
                        peer_alias,
                        method,
                        format!("no peer in manifest cache advertises method '{method_target}'"),
                    ));
                }
            }
        } else {
            peer_alias.to_string()
        };
        let Some(peer_id) = self.peer_ids.get(&resolved_alias).copied() else {
            return Err(RemoteCallError::local(
                peer_alias,
                method,
                format!("unknown peer alias '{resolved_alias}' (not in [peers] config)"),
            ));
        };
        Ok((resolved_alias, peer_id))
    }
}

fn build_flow_remote_request(
    method: &str,
    arg: &[u8],
    identity: Bundle,
    deadline_secs: i64,
    task_id: Option<&str>,
    session_id: Option<&str>,
    workspace_path: Option<&str>,
) -> Vec<u8> {
    build_request_with_surface(
        method.to_string(),
        arg.to_vec(),
        identity,
        deadline_secs,
        None,
        None,
        task_id.map(str::to_string),
        session_id.map(str::to_string),
        workspace_path.map(str::to_string),
    )
}

impl RemoteCallDispatcher for RealDispatcher {
    fn remote_call(&self, peer_alias: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
        // a) Resolve peer alias. Support a `capability:<method>` form (M10.3)
        //    so flows can target a method instead of a hard-coded alias.
        //    Existing alias usage is unchanged.
        let resolved_alias: String = if let Some(method_target) =
            peer_alias.strip_prefix("capability:")
        {
            let cache = match self.capability_cache.as_ref() {
                Some(c) => c,
                None => {
                    return Err(RemoteCallError::local(
                        peer_alias,
                        method,
                        "capability resolution requires a populated ManifestCache (host not wired)"
                            .to_string(),
                    ));
                }
            };
            match cache.find_alias_for_method(method_target) {
                Some(a) => a,
                None => {
                    return Err(RemoteCallError::local(
                        peer_alias,
                        method,
                        format!("no peer in manifest cache advertises method '{method_target}'"),
                    ));
                }
            }
        } else {
            peer_alias.to_string()
        };

        let Some(peer_id) = self.peer_ids.get(&resolved_alias).copied() else {
            return Err(RemoteCallError::local(
                peer_alias,
                method,
                format!("unknown peer alias '{resolved_alias}' (not in [peers] config)"),
            ));
        };
        let peer_alias = resolved_alias.as_str();

        // b) Build envelope. We extract the request_id afterwards so logs and
        //    errors can correlate to the responder's audit record.
        let envelope_bytes = build_flow_remote_request(
            method,
            arg,
            self.identity.clone(),
            self.deadline_secs,
            self.task_id.as_deref(),
            self.session_id.as_deref(),
            self.workspace_path.as_deref(),
        );
        let request_id = peek_request_id(&envelope_bytes);

        // c) RemoteCallIssued — log-before-act.
        let issued =
            encode_remote_call_issued_payload(peer_alias, method, arg, self.trace_id, request_id);
        if let Err(e) = append_log(&self.event_log, EventType::RemoteCallIssued, issued) {
            return Err(RemoteCallError::local(
                peer_alias,
                method,
                format!("event log append (issued): {e}"),
            ));
        }

        // d) Dispatch. We are on a spawn_blocking thread; block_on is safe.
        //
        // When a MeshClient is wired (bridge path) we go through its
        // call-with-reconnect entry point so a peer that died and came
        // back doesn't fail the request. When it isn't (CLI standalone)
        // we fall through to the direct Client::call path that worked
        // before A.4.
        let started_at = std::time::Instant::now();
        let resp_bytes_result = if let Some(mesh) = self.mesh.clone() {
            let alias_owned = peer_alias.to_string();
            self.handle.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs((self.deadline_secs + 5) as u64),
                    async move {
                        mesh.call(&alias_owned, envelope_bytes)
                            .await
                            .map_err(|e| e.cause)
                    },
                )
                .await
            })
        } else {
            self.handle.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs((self.deadline_secs + 5) as u64),
                    self.client.call(peer_id, envelope_bytes),
                )
                .await
            })
        };
        let latency_ms = started_at.elapsed().as_millis() as u64;

        // e) Surface outcomes.
        let outcome = match resp_bytes_result {
            Ok(Ok(resp_bytes)) => match decode_response(&resp_bytes) {
                Ok(resp) => {
                    // RELIX-7.19 GAP 4: publish the responder's
                    // confidence score to the shared cell so
                    // SOL `last_confidence()` reflects it on
                    // the very next opcode after `remote_call`.
                    if let (Some(cell), Some(score)) =
                        (self.last_confidence_cell.as_ref(), resp.confidence)
                    {
                        cell.set(score);
                    } else if let Some(cell) = self.last_confidence_cell.as_ref() {
                        // Responder didn't score → don't
                        // mutate; SOL sees whatever the last
                        // scored call left.
                        let _ = cell;
                    }
                    match resp.res {
                        ResponseResult::Ok(body) => Ok(body.to_vec()),
                        ResponseResult::Err(env) => {
                            Err(RemoteCallError::from_envelope(peer_alias, method, &env))
                        }
                        ResponseResult::StreamHandle(_) => Err(RemoteCallError::local(
                            peer_alias,
                            method,
                            "stream response not supported by alpha synchronous dispatcher",
                        )),
                    }
                }
                Err(e) => Err(RemoteCallError::local(
                    peer_alias,
                    method,
                    format!("response decode: {e}"),
                )),
            },
            Ok(Err(transport_err)) => Err(RemoteCallError::local(
                peer_alias,
                method,
                format!("transport: {transport_err}"),
            )),
            Err(_elapsed) => Err(RemoteCallError::local(
                peer_alias,
                method,
                "outbound RPC timed out at dispatcher",
            )),
        };

        // f) Terminal event for this call.
        match &outcome {
            Ok(body) => {
                let completed = encode_remote_call_completed_payload(
                    peer_alias, method, request_id, latency_ms, body,
                );
                let _ = append_log(&self.event_log, EventType::RemoteCallCompleted, completed);
            }
            Err(err) => {
                let failed = encode_remote_call_failed_payload(
                    peer_alias, method, request_id, latency_ms, err,
                );
                let _ = append_log(&self.event_log, EventType::RemoteCallFailed, failed);
            }
        }

        outcome
    }

    /// RELIX-2 step 5: streaming variant. Opens a
    /// `/relix/rpc/stream/1` substream against the resolved
    /// peer, writes the same RELIX-1 RequestEnvelope the
    /// unary path uses, then reads `StreamFrame`s until the
    /// remote sends `End` or `Err`. Each `Chunk` is reported
    /// to `on_chunk` synchronously AND appended to the
    /// concatenated body the VM ultimately receives.
    ///
    /// Per-call event-log entries match the unary path:
    /// `RemoteCallIssued` before the dial, then either
    /// `RemoteCallCompleted` or `RemoteCallFailed` once the
    /// stream terminates.
    fn remote_call_stream(
        &self,
        peer_alias: &str,
        method: &str,
        arg: &[u8],
        on_chunk: &dyn Fn(&[u8]),
    ) -> RemoteCallResult {
        let (resolved_alias, peer_id) = self.resolve_peer(peer_alias, method)?;
        let peer_alias = resolved_alias.as_str();

        let envelope_bytes = build_flow_remote_request(
            method,
            arg,
            self.identity.clone(),
            self.deadline_secs,
            self.task_id.as_deref(),
            self.session_id.as_deref(),
            self.workspace_path.as_deref(),
        );
        let request_id = peek_request_id(&envelope_bytes);

        // RemoteCallIssued — log-before-act. Marked as a
        // streaming call in the payload so the per-flow log
        // reader can tell the two paths apart.
        let issued =
            encode_remote_call_issued_payload(peer_alias, method, arg, self.trace_id, request_id);
        if let Err(e) = append_log(&self.event_log, EventType::RemoteCallIssued, issued) {
            return Err(RemoteCallError::local(
                peer_alias,
                method,
                format!("event log append (issued): {e}"),
            ));
        }

        let started_at = std::time::Instant::now();
        let client = self.client.clone();
        let outer_timeout = Duration::from_secs((self.deadline_secs + 5) as u64);
        let peer_alias_owned = peer_alias.to_string();
        let method_owned = method.to_string();
        let cancel_signal = self.cancel_signal.clone();

        // Drive the substream protocol synchronously from
        // this blocking thread. block_on is safe here — the
        // dispatcher always runs inside `spawn_blocking`.
        let stream_result: RemoteCallResult = self.handle.block_on(async move {
            use crate::transport::stream::{StreamFrame, StreamReader, write_request_envelope};

            let opened =
                tokio::time::timeout(outer_timeout, async { client.open_stream(peer_id).await })
                    .await;
            let mut raw_stream = match opened {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    return Err(RemoteCallError::local(
                        peer_alias_owned.clone(),
                        method_owned.clone(),
                        format!("stream open: {e}"),
                    ));
                }
                Err(_) => {
                    return Err(RemoteCallError::local(
                        peer_alias_owned.clone(),
                        method_owned.clone(),
                        "outbound stream open timed out at dispatcher",
                    ));
                }
            };
            if let Err(e) = write_request_envelope(&mut raw_stream, &envelope_bytes).await {
                return Err(RemoteCallError::local(
                    peer_alias_owned.clone(),
                    method_owned.clone(),
                    format!("stream write envelope: {e}"),
                ));
            }
            let mut reader = StreamReader::new(raw_stream);
            let mut concatenated: Vec<u8> = Vec::new();
            loop {
                // RELIX-2 step 5b: race the next-frame await
                // against the cancellation signal. If the
                // bridge cancels (HTTP client dropped the SSE
                // response), we return TRANSPORT-classed
                // error so the FlowRunner writes a
                // `task.failed` / `chat.assistant_turn`
                // partial record. When no signal is wired,
                // `pending` ensures the select! just waits
                // on the frame read.
                let frame = match cancel_signal.as_ref() {
                    Some(signal) => {
                        let signal = signal.clone();
                        tokio::select! {
                            f = tokio::time::timeout(outer_timeout, reader.next_frame()) => f,
                            () = signal.notified() => {
                                return Err(RemoteCallError {
                                    kind: relix_core::types::error_kinds::TRANSPORT,
                                    peer: peer_alias_owned.clone(),
                                    method: method_owned.clone(),
                                    cause: "stream cancelled by caller".to_string(),
                                });
                            }
                        }
                    }
                    None => tokio::time::timeout(outer_timeout, reader.next_frame()).await,
                };
                match frame {
                    Ok(Ok(Some(StreamFrame::Header { .. }))) => {
                        // Header carries audit-correlation
                        // metadata; the unary path doesn't
                        // surface it to the VM so we don't
                        // either. Just continue to chunks.
                        continue;
                    }
                    Ok(Ok(Some(StreamFrame::Chunk(bytes)))) => {
                        on_chunk(bytes.as_ref());
                        concatenated.extend_from_slice(bytes.as_ref());
                    }
                    Ok(Ok(Some(StreamFrame::End))) => break,
                    Ok(Ok(Some(StreamFrame::Err { kind, cause }))) => {
                        return Err(RemoteCallError {
                            kind,
                            peer: peer_alias_owned.clone(),
                            method: method_owned.clone(),
                            cause,
                        });
                    }
                    Ok(Ok(None)) => {
                        // EOF without an explicit
                        // terminator — treat as graceful
                        // close.
                        break;
                    }
                    Ok(Err(e)) => {
                        return Err(RemoteCallError::local(
                            peer_alias_owned.clone(),
                            method_owned.clone(),
                            format!("stream frame read: {e}"),
                        ));
                    }
                    Err(_) => {
                        return Err(RemoteCallError::local(
                            peer_alias_owned.clone(),
                            method_owned.clone(),
                            "stream frame read timed out",
                        ));
                    }
                }
            }
            Ok(concatenated)
        });

        let latency_ms = started_at.elapsed().as_millis() as u64;

        // Per-call terminal event.
        match &stream_result {
            Ok(body) => {
                let completed = encode_remote_call_completed_payload(
                    peer_alias, method, request_id, latency_ms, body,
                );
                let _ = append_log(&self.event_log, EventType::RemoteCallCompleted, completed);
            }
            Err(err) => {
                let failed = encode_remote_call_failed_payload(
                    peer_alias, method, request_id, latency_ms, err,
                );
                let _ = append_log(&self.event_log, EventType::RemoteCallFailed, failed);
            }
        }

        stream_result
    }
}

// ──────────────────────────── Helpers ───────────────────────────────────────

fn append_log(
    log: &Arc<Mutex<EventLog>>,
    kind: EventType,
    payload: Vec<u8>,
) -> Result<u64, FlowRunnerError> {
    let mut guard = log
        .lock()
        .map_err(|_| FlowRunnerError::EventLog("log mutex poisoned".into()))?;
    guard
        .append(kind, payload)
        .map_err(|e| FlowRunnerError::EventLog(e.to_string()))
}

fn compile_sol(path: &Path) -> Result<crate::sol::CompiledFlow, FlowRunnerError> {
    if !path.exists() {
        return Err(FlowRunnerError::Config(format!(
            "flow file not found: {}",
            path.display()
        )));
    }
    // P6: route through the directive-aware compile entry
    // point so any `#steps N` directive at the top of the
    // source is honoured per-flow. `default_max_steps = 0`
    // means "use the SOL crate's default", since the flow
    // runner today has no operator-level [sol] max_steps
    // wiring.
    crate::sol::compile_path_with_directives(path, 0)
        .map_err(|e| FlowRunnerError::Config(e.to_string()))
}

async fn dial_all_peers(
    client: &rpc::Client,
    events: &mut tokio::sync::mpsc::Receiver<TransportEvent>,
    peers: &PeersFile,
) -> Result<HashMap<String, PeerId>, FlowRunnerError> {
    if peers.peers.is_empty() {
        return Ok(HashMap::new());
    }
    // Issue every dial first, then collect PeerConnected events for each one.
    // We resolve aliases by remembering the dial order: each PeerConnected
    // event carries the peer's libp2p PeerId; matching addr ↔ alias happens
    // here since libp2p does not surface the dialed multiaddr on the event.
    //
    // Alpha simplification: dial sequentially and pair PeerConnected with the
    // dial whose addr matches the event's reported address (the libp2p Event
    // carries the remote multiaddr).
    let mut want: HashMap<String, Multiaddr> = HashMap::new();
    for (alias, entry) in &peers.peers {
        let addr: Multiaddr = entry.addr.parse().map_err(|e| {
            FlowRunnerError::Config(format!("peer '{alias}' invalid multiaddr: {e:?}"))
        })?;
        client
            .dial(addr.clone())
            .await
            .map_err(|e| FlowRunnerError::Transport(format!("dial '{alias}': {e}")))?;
        want.insert(alias.clone(), addr);
    }

    let mut out = HashMap::new();
    let timeout = Duration::from_secs(10);
    let deadline = tokio::time::Instant::now() + timeout;
    while out.len() < want.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            let missing: Vec<&String> = want
                .keys()
                .filter(|k| !out.contains_key(k.as_str()))
                .collect();
            return Err(FlowRunnerError::Transport(format!(
                "timed out connecting to peers: {missing:?}"
            )));
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TransportEvent::PeerConnected { peer_id, address })) => {
                if let Some((alias, _)) = want
                    .iter()
                    .find(|(_, addr)| ends_with_addr(address.clone(), addr))
                {
                    out.entry(alias.clone()).or_insert(peer_id);
                }
            }
            Ok(Some(_)) => {} // other events ignored during the dial-window
            Ok(None) => {
                return Err(FlowRunnerError::Transport(
                    "transport event stream closed during dial".into(),
                ));
            }
            Err(_) => {
                let missing: Vec<&String> = want
                    .keys()
                    .filter(|k| !out.contains_key(k.as_str()))
                    .collect();
                return Err(FlowRunnerError::Transport(format!(
                    "timed out connecting to peers: {missing:?}"
                )));
            }
        }
    }
    Ok(out)
}

/// Match a `PeerConnected` event's reported address against a wanted dial
/// address by checking the trailing `/tcp/<port>` segment. libp2p sometimes
/// adds extra protocol segments to the reported address, but the dialed
/// `/ip4/.../tcp/<port>` is always a prefix.
fn ends_with_addr(reported: Multiaddr, wanted: &Multiaddr) -> bool {
    let reported_s = reported.to_string();
    let wanted_s = wanted.to_string();
    reported_s.starts_with(&wanted_s)
}

fn peek_request_id(envelope_bytes: &[u8]) -> RequestId {
    // Cheap: decode the envelope and read rid. Not a hot path (called once
    // per remote_call); the envelope decode is the same one the responder
    // does, so any decode failure here would mean the responder fails too.
    match codec::decode::<crate::transport::envelope::RequestEnvelope>(envelope_bytes) {
        Ok(env) => env.rid,
        Err(_) => RequestId([0u8; 16]),
    }
}

fn resolve_flow_log_path(data_dir: &Option<PathBuf>, flow_id: FlowId) -> PathBuf {
    let base = data_dir.clone().unwrap_or_else(|| {
        std::env::var("RELIX_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".relix")
            })
    });
    base.join("flow-runner")
        .join("flows")
        .join(format!("{flow_id}.log"))
}

// ──────────────────────────── Event payloads ────────────────────────────────
//
// Alpha payloads are simple text — typed/CBOR payloads land at Gate 2 along
// with the replay-mode VM. `relix-flow-inspect` already prints these as UTF-8.

fn encode_flow_started_payload(opts: &FlowRunOptions, trace_id: TraceId) -> Vec<u8> {
    let mut out = format!(
        "flow={}\ntrace_id={}\nidentity_issuer={}\n",
        opts.flow_path.display(),
        trace_id,
        opts.identity_bundle.header.kid
    );
    if let Some(task_id) = opts.task_id.as_deref() {
        out.push_str("task_id=");
        out.push_str(task_id);
        out.push('\n');
    }
    out.into_bytes()
}

fn encode_remote_call_issued_payload(
    peer: &str,
    method: &str,
    arg: &[u8],
    trace_id: TraceId,
    request_id: RequestId,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128 + arg.len());
    let _ = writeln!(buf, "peer={peer}");
    let _ = writeln!(buf, "method={method}");
    let _ = writeln!(buf, "trace_id={trace_id}");
    let _ = writeln!(buf, "request_id={request_id}");
    let _ = writeln!(buf, "arg_bytes={}", arg.len());
    buf
}

fn encode_remote_call_completed_payload(
    peer: &str,
    method: &str,
    request_id: RequestId,
    latency_ms: u64,
    body: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128 + body.len());
    let _ = writeln!(buf, "peer={peer}");
    let _ = writeln!(buf, "method={method}");
    let _ = writeln!(buf, "request_id={request_id}");
    let _ = writeln!(buf, "latency_ms={latency_ms}");
    let _ = writeln!(buf, "body_bytes={}", body.len());
    buf
}

fn encode_remote_call_failed_payload(
    peer: &str,
    method: &str,
    request_id: RequestId,
    latency_ms: u64,
    err: &RemoteCallError,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(192);
    let _ = writeln!(buf, "peer={peer}");
    let _ = writeln!(buf, "method={method}");
    let _ = writeln!(buf, "request_id={request_id}");
    let _ = writeln!(buf, "latency_ms={latency_ms}");
    let _ = writeln!(buf, "error_kind={}", err.kind);
    let _ = writeln!(buf, "cause={}", err.cause);
    buf
}

// ──────────────────────────── Errors ────────────────────────────────────────

/// FlowRunner-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum FlowRunnerError {
    /// Config (peer file, paths) parse / load failure.
    #[error("config: {0}")]
    Config(String),
    /// Transport (libp2p) failure.
    #[error("transport: {0}")]
    Transport(String),
    /// Event log failure.
    #[error("event log: {0}")]
    EventLog(String),
    /// VM-runner (spawn_blocking, compile) failure.
    #[error("vm: {0}")]
    Vm(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn peers_file_parses() {
        let toml = r#"
            [peers.memory]
            addr = "/ip4/127.0.0.1/tcp/9001"

            [peers.ai]
            addr = "/ip4/127.0.0.1/tcp/9002"
        "#;
        let file: PeersFile = toml::from_str(toml).expect("parse");
        assert_eq!(file.peers.len(), 2);
        assert_eq!(file.peers["memory"].addr, "/ip4/127.0.0.1/tcp/9001");
    }

    #[test]
    fn flow_log_path_uses_data_dir() {
        let p = resolve_flow_log_path(&Some(PathBuf::from("/tmp/relix-test")), FlowId([0u8; 16]));
        assert!(p.ends_with("00000000000000000000000000000000.log"));
        assert!(p.to_string_lossy().contains("/tmp/relix-test"));
    }

    #[test]
    fn flow_remote_request_stamps_task_id_when_flow_is_task_bound() {
        let bundle = mock_bundle();
        let bytes = build_flow_remote_request(
            "tool.web_fetch",
            b"{}",
            bundle,
            30,
            Some("task-123"),
            Some("sess-123"),
            Some("D:/work/relix"),
        );
        let req: crate::transport::envelope::RequestEnvelope =
            relix_core::codec::decode(&bytes).expect("request decodes");
        assert_eq!(req.method, "tool.web_fetch");
        assert_eq!(req.task_id.as_deref(), Some("task-123"));
        assert_eq!(req.session_id.as_deref(), Some("sess-123"));
        assert_eq!(req.workspace_path.as_deref(), Some("D:/work/relix"));
        assert!(req.surface.is_none());
        assert!(req.approval_token.is_none());
    }

    #[test]
    fn flow_remote_request_leaves_task_id_absent_for_standalone_runs() {
        let bundle = mock_bundle();
        let bytes = build_flow_remote_request("ai.chat", b"hello", bundle, 30, None, None, None);
        let req: crate::transport::envelope::RequestEnvelope =
            relix_core::codec::decode(&bytes).expect("request decodes");
        assert_eq!(req.method, "ai.chat");
        assert!(req.task_id.is_none());
        assert!(req.session_id.is_none());
        assert!(req.workspace_path.is_none());
    }

    fn mock_bundle() -> Bundle {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use relix_core::identity::{IdentityBundle, issue_identity};
        use relix_core::types::NodeId;

        let root = SigningKey::generate(&mut OsRng);
        let subject = SigningKey::generate(&mut OsRng);
        let bundle = IdentityBundle {
            subject_id: NodeId::from_pubkey(&subject.verifying_key().to_bytes()),
            name: "flow-test".into(),
            org_id: NodeId::from_pubkey(&root.verifying_key().to_bytes()),
            groups: vec!["chat".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        issue_identity(bundle, &root, 3600).expect("identity issues")
    }

    /// Stub dispatcher used to exercise dispatcher-replacement plumbing without
    /// a real libp2p stack. The real path is exercised by the integration
    /// script in `scripts/alpha-bringup-m6.sh`.
    struct CountingDispatcher {
        called: std::sync::atomic::AtomicU32,
        peer_map: HashMap<String, ()>,
    }

    impl RemoteCallDispatcher for CountingDispatcher {
        fn remote_call(&self, peer: &str, _method: &str, _arg: &[u8]) -> RemoteCallResult {
            if !self.peer_map.contains_key(peer) {
                return Err(RemoteCallError::local(peer, "any", "unknown peer alias"));
            }
            self.called
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(b"ok".to_vec())
        }
    }

    #[test]
    fn unknown_peer_alias_surfaces_local_error() {
        let mut map = HashMap::new();
        map.insert("memory".to_string(), ());
        let d = CountingDispatcher {
            called: 0.into(),
            peer_map: map,
        };
        let err = d
            .remote_call("nonexistent", "x", b"")
            .expect_err("must fail");
        assert_eq!(err.kind, 0);
        assert!(err.cause.contains("unknown peer alias"));
    }

    /// Dispatcher that records every (peer, method) call and replies according
    /// to a scripted response sequence. Used to exercise sequential SOL
    /// orchestration in isolation; the real two-process path is the M6/S7
    /// bringup script.
    struct ScriptedDispatcher {
        calls: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
        responses: std::sync::Mutex<Vec<RemoteCallResult>>,
    }

    impl ScriptedDispatcher {
        fn new(responses: Vec<RemoteCallResult>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                responses: std::sync::Mutex::new(responses.into_iter().rev().collect()),
            })
        }
        fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RemoteCallDispatcher for ScriptedDispatcher {
        fn remote_call(&self, peer: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
            self.calls
                .lock()
                .unwrap()
                .push((peer.to_string(), method.to_string(), arg.to_vec()));
            self.responses.lock().unwrap().pop().unwrap_or_else(|| {
                Err(RemoteCallError::local(peer, method, "no scripted response"))
            })
        }
    }

    fn compile_chained_health() -> Vec<crate::sol::bytecode::Inst> {
        use crate::sol::analyzer::Analyzer;
        use crate::sol::bytecode::Codegen;
        use crate::sol::lexer::Lexer;
        use crate::sol::parser::Parser;

        // Tests run with the workspace as cwd, so the repo-root flow path
        // is reachable as `../../flows/...` from `crates/relix-runtime/`.
        let path = PathBuf::from("../../flows/chained_health.sol");
        assert!(
            path.exists(),
            "fixture not found: {} (run from workspace root)",
            path.display()
        );
        let mut lex = Lexer::from(path.to_str().unwrap());
        let tokens = lex.tokens();
        let mut parser = Parser::from(tokens);
        let mut program = parser.run();
        let mut analyzer = Analyzer::new();
        analyzer.run(&mut program);
        Codegen::from(analyzer.tt_arena).gen_bcode(&program)
    }

    #[test]
    fn chained_flow_compiles_and_dispatches_in_order() {
        let bc = compile_chained_health();
        let remote_count = bc
            .iter()
            .filter(|i| matches!(i, crate::sol::bytecode::Inst::RemoteCall))
            .count();
        assert_eq!(remote_count, 2, "expected exactly two RemoteCall opcodes");

        let disp = ScriptedDispatcher::new(vec![Ok(b"memory=ok".to_vec()), Ok(b"ai=ok".to_vec())]);
        let mut vm = crate::sol::vm::VM::from(&bc).with_dispatcher(disp.clone());
        let _ = vm.run();
        let calls = disp.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "memory");
        assert_eq!(calls[1].0, "ai");
        assert!(vm.last_error().is_none(), "VM must not have errored");
    }

    #[test]
    fn first_call_failure_short_circuits_chain() {
        let bc = compile_chained_health();
        let disp = ScriptedDispatcher::new(vec![
            Err(RemoteCallError {
                kind: 6,
                peer: "memory".into(),
                method: "node.health".into(),
                cause: "policy denied".into(),
            }),
            // Should NEVER be popped — VM halts after first failure.
            Ok(b"ai=ok".to_vec()),
        ]);
        let mut vm = crate::sol::vm::VM::from(&bc).with_dispatcher(disp.clone());
        let exit = vm.run();
        assert_eq!(exit, crate::sol::vm::VM_ERROR_SENTINEL);
        let calls = disp.calls();
        assert_eq!(calls.len(), 1, "second remote_call must not have run");
        assert_eq!(calls[0].0, "memory");
        let err = vm.last_error().expect("must have error");
        assert_eq!(err.kind, 6);
    }
}
