//! Plugin author SDK for Relix's `relix-plugin-v1` protocol.
//!
//! Plugin authors depend on this crate, register their capability
//! handlers, and call [`PluginServer::serve`]. The SDK:
//!
//! 1. Binds an axum HTTP server to `127.0.0.1:0` (kernel picks a
//!    free port).
//! 2. Writes `RELIX_PLUGIN_PORT=<port>` to stdout on its first
//!    line so the host loader can find it.
//! 3. Serves three endpoints:
//!    - `GET /health` → `{ "ok": true }` once the server is up.
//!    - `GET /ready`  → `{ "ok": true }` once
//!      [`PluginServer::mark_ready`] is called.
//!    - `POST /invoke` → routes to the registered handler.
//!
//! ## Invoke wire shape
//!
//! Request body:
//!
//! ```json
//! {
//!   "method":            "<dotted.method>",
//!   "args":              "<pipe-delimited utf-8>",
//!   "trace_id":          "<hex16>",
//!   "request_id":        "<hex16>",
//!   "caller_subject_id": "<hex32>",
//!   "deadline_unix":     <i64>
//! }
//! ```
//!
//! Successful response body:
//!
//! ```json
//! { "ok": true, "body": "<response string>" }
//! ```
//!
//! Error response body:
//!
//! ```json
//! { "ok": false, "error_kind": <u32>, "error_cause": "<msg>" }
//! ```
//!
//! Where `error_kind` mirrors `relix_core::types::error_kinds::*`
//! (a few stable constants exported as `ErrorKind` for SDK
//! consumers — `INVALID_ARGS = 5`, `RESPONDER_INTERNAL = 11`, …).
//!
//! ## Threading model
//!
//! Handlers are `async fn(InvokeRequest) -> Result<String,
//! PluginError>`. They run on the SDK's tokio runtime. Long-
//! running handlers do not block the server — axum dispatches
//! each call on its own task.
//!
//! ## Lifecycle
//!
//! ```rust,no_run
//! # use relix_plugin_sdk::{PluginServer, InvokeRequest, PluginError};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let mut server = PluginServer::new();
//! server.register("hello.greet", |req: InvokeRequest| async move {
//!     Ok(format!("Hello, {}!", req.args))
//! });
//! server.serve().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// Stable error kinds plugins return through the protocol. Mirror
/// of the subset of `relix_core::types::error_kinds` that makes
/// sense across the SDK boundary — see `docs/plugins.md` for the
/// full table.
pub mod error_kind {
    /// Caller supplied bad args (parse failure, missing fields,
    /// out-of-range values, …).
    pub const INVALID_ARGS: u32 = 5;
    /// Plugin-internal error (panic-recovered handler, downstream
    /// API rejection, …).
    pub const RESPONDER_INTERNAL: u32 = 11;
    /// Plugin received the call but its own backend is rate-
    /// limited / overloaded.
    pub const RESPONDER_OVERLOADED: u32 = 12;
    /// Plugin doesn't know the requested method. Should only
    /// happen if the host's manifest is out of sync.
    pub const UNKNOWN_METHOD: u32 = 4;
}

/// One inbound /invoke call after JSON decoding.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InvokeRequest {
    /// Dotted method name (`my_plugin.do_thing`).
    pub method: String,
    /// Pipe-delimited UTF-8 args.
    pub args: String,
    /// Hex-encoded TraceId from the host.
    #[serde(default)]
    pub trace_id: String,
    /// Hex-encoded RequestId from the host.
    #[serde(default)]
    pub request_id: String,
    /// Hex-encoded subject_id of the calling identity.
    #[serde(default)]
    pub caller_subject_id: String,
    /// Unix-seconds deadline. Plugins are expected to short-circuit
    /// past this, but the SDK does not enforce it — handlers
    /// can ignore the field if they don't have async cancellation.
    #[serde(default)]
    pub deadline_unix: i64,
}

/// Successful response body sent back through /invoke.
#[derive(Debug, Serialize)]
struct InvokeOkBody<'a> {
    ok: bool,
    body: &'a str,
}

/// Error response body.
#[derive(Debug, Serialize)]
struct InvokeErrBody<'a> {
    ok: bool,
    error_kind: u32,
    error_cause: &'a str,
}

/// Plugin-side error. Caught and rendered into the wire shape by
/// the dispatcher. `Internal` is the default; use
/// [`PluginError::invalid_args`] for caller-side mistakes.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PluginError {
    /// 4xx-equivalent: the args were malformed.
    #[error("invalid args: {0}")]
    InvalidArgs(String),
    /// 5xx-equivalent: the plugin itself failed.
    #[error("internal: {0}")]
    Internal(String),
    /// Plugin's own upstream is overloaded; the caller may retry.
    #[error("overloaded: {0}")]
    Overloaded(String),
}

impl PluginError {
    pub fn invalid_args(msg: impl Into<String>) -> Self {
        Self::InvalidArgs(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn overloaded(msg: impl Into<String>) -> Self {
        Self::Overloaded(msg.into())
    }
    pub fn kind(&self) -> u32 {
        match self {
            Self::InvalidArgs(_) => error_kind::INVALID_ARGS,
            Self::Internal(_) => error_kind::RESPONDER_INTERNAL,
            Self::Overloaded(_) => error_kind::RESPONDER_OVERLOADED,
        }
    }
}

/// One registered capability handler.
type HandlerFn = Arc<
    dyn Fn(
            InvokeRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, PluginError>> + Send>,
        > + Send
        + Sync,
>;

/// Plugin server. Build it, register handlers, call
/// [`PluginServer::serve`].
pub struct PluginServer {
    handlers: HashMap<String, HandlerFn>,
    /// Listen address. Default is `127.0.0.1:0` — the kernel
    /// picks a free port and we write it back on stdout. Tests
    /// can override via [`PluginServer::with_bind`].
    bind: String,
    /// Where to write the `RELIX_PLUGIN_PORT=<n>` line. Default
    /// is stdout; tests override.
    port_sink: PortSink,
    /// Wrapped in an Arc<Mutex<>> so /ready can flip from false
    /// to true atomically.
    ready: Arc<Mutex<bool>>,
}

enum PortSink {
    Stdout,
    Captured(Arc<Mutex<Vec<u8>>>),
}

impl PluginServer {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
            bind: "127.0.0.1:0".to_string(),
            port_sink: PortSink::Stdout,
            ready: Arc::new(Mutex::new(true)),
        }
    }

    /// Override the listen address. Used by tests to pin a known
    /// port; production always uses `127.0.0.1:0` and lets the
    /// kernel pick.
    pub fn with_bind(mut self, bind: impl Into<String>) -> Self {
        self.bind = bind.into();
        self
    }

    /// Capture the port line into a buffer instead of writing to
    /// stdout. Test-only — production callers should not need
    /// this.
    pub fn with_captured_port(mut self, sink: Arc<Mutex<Vec<u8>>>) -> Self {
        self.port_sink = PortSink::Captured(sink);
        self
    }

    /// Mark the plugin as ready to serve traffic. Defaults to
    /// `true` — call [`PluginServer::with_lazy_ready`] to start
    /// with `/ready` returning `not ready` until the plugin's
    /// own warmup completes.
    pub fn mark_ready(&self, ready: bool) {
        let r = self.ready.clone();
        tokio::spawn(async move {
            let mut g = r.lock().await;
            *g = ready;
        });
    }

    /// Start with `/ready = false`. The plugin author calls
    /// [`PluginServer::mark_ready`] from inside their warmup
    /// future once initialisation is done.
    pub fn with_lazy_ready(mut self) -> Self {
        self.ready = Arc::new(Mutex::new(false));
        self
    }

    /// Register a capability. `method` is the dotted method name
    /// the host will route to (`my_plugin.do_thing`); `f` is the
    /// async handler.
    pub fn register<F, Fut>(&mut self, method: impl Into<String>, f: F)
    where
        F: Fn(InvokeRequest) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<String, PluginError>> + Send + 'static,
    {
        let f = Arc::new(f);
        let wrapped: HandlerFn = Arc::new(move |req| {
            let f = f.clone();
            Box::pin(async move { (f)(req).await })
        });
        self.handlers.insert(method.into(), wrapped);
    }

    /// Bind, announce the port, and serve forever.
    pub async fn serve(self) -> Result<(), ServeError> {
        let listener = TcpListener::bind(&self.bind)
            .await
            .map_err(|e| ServeError::Bind(format!("{}: {e}", self.bind)))?;
        let local = listener
            .local_addr()
            .map_err(|e| ServeError::Bind(format!("local_addr: {e}")))?;
        announce_port(&self.port_sink, local.port()).await?;
        let state = AppState {
            handlers: Arc::new(self.handlers),
            ready: self.ready,
        };
        let app = Router::new()
            .route("/health", get(handle_health))
            .route("/ready", get(handle_ready))
            .route("/invoke", post(handle_invoke))
            .with_state(state);
        axum::serve(listener, app)
            .await
            .map_err(|e| ServeError::Serve(format!("{e}")))
    }
}

impl Default for PluginServer {
    fn default() -> Self {
        Self::new()
    }
}

async fn announce_port(sink: &PortSink, port: u16) -> Result<(), ServeError> {
    let line = format!("RELIX_PLUGIN_PORT={port}\n");
    match sink {
        PortSink::Stdout => {
            let mut out = tokio::io::stdout();
            out.write_all(line.as_bytes())
                .await
                .map_err(|e| ServeError::Bind(format!("write port to stdout: {e}")))?;
            out.flush()
                .await
                .map_err(|e| ServeError::Bind(format!("flush stdout: {e}")))?;
        }
        PortSink::Captured(buf) => {
            let mut g = buf.lock().await;
            g.extend_from_slice(line.as_bytes());
        }
    }
    Ok(())
}

#[derive(Clone)]
struct AppState {
    handlers: Arc<HashMap<String, HandlerFn>>,
    ready: Arc<Mutex<bool>>,
}

async fn handle_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn handle_ready(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let r = *state.ready.lock().await;
    if r {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "ok": false, "reason": "not ready" })),
        )
    }
}

async fn handle_invoke(
    State(state): State<AppState>,
    Json(req): Json<InvokeRequest>,
) -> Json<serde_json::Value> {
    let Some(handler) = state.handlers.get(&req.method).cloned() else {
        return Json(serde_json::json!({
            "ok": false,
            "error_kind": error_kind::UNKNOWN_METHOD,
            "error_cause": format!("plugin has no handler for `{}`", req.method),
        }));
    };
    let method = req.method.clone();
    match handler(req).await {
        Ok(body) => Json(
            serde_json::to_value(InvokeOkBody {
                ok: true,
                body: &body,
            })
            .unwrap(),
        ),
        Err(e) => Json(
            serde_json::to_value(InvokeErrBody {
                ok: false,
                error_kind: e.kind(),
                error_cause: &format!("{method}: {e}"),
            })
            .unwrap(),
        ),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("bind: {0}")]
    Bind(String),
    #[error("serve: {0}")]
    Serve(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Pull the line `RELIX_PLUGIN_PORT=<n>` out of a captured
    /// byte buffer, parse the port, panic on failure.
    fn parse_announced_port(buf: &[u8]) -> u16 {
        let s = std::str::from_utf8(buf).unwrap();
        s.lines()
            .find_map(|l| l.strip_prefix("RELIX_PLUGIN_PORT="))
            .and_then(|n| n.trim().parse::<u16>().ok())
            .expect("port line")
    }

    async fn start(server: PluginServer) -> (u16, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let buf_clone = buf.clone();
        let server = server.with_captured_port(buf_clone);
        // Server takes itself by value and serves forever; we
        // spawn it and consume the captured port line.
        tokio::spawn(async move { server.serve().await.unwrap() });
        // Spin until the announce happens.
        let mut port = 0u16;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let g = buf.lock().await;
            if !g.is_empty() {
                port = parse_announced_port(&g);
                break;
            }
        }
        assert_ne!(port, 0, "port not announced");
        (port, buf)
    }

    #[tokio::test]
    async fn server_announces_port_to_captured_sink() {
        let (port, _buf) = start(PluginServer::new()).await;
        assert!(port > 0);
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let (port, _) = start(PluginServer::new()).await;
        let r = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body.get("ok").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test]
    async fn ready_defaults_to_true() {
        let (port, _) = start(PluginServer::new()).await;
        let r = reqwest::get(format!("http://127.0.0.1:{port}/ready"))
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn lazy_ready_returns_503_until_marked() {
        let mut server = PluginServer::new().with_lazy_ready();
        // Use a side channel so we can flip ready from the test
        // after the server starts.
        let ready_handle = server.ready.clone();
        server.register("noop.touch", |_| async move { Ok(String::new()) });
        let (port, _) = start(server).await;
        let r = reqwest::get(format!("http://127.0.0.1:{port}/ready"))
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 503);
        // Flip and recheck.
        *ready_handle.lock().await = true;
        let r2 = reqwest::get(format!("http://127.0.0.1:{port}/ready"))
            .await
            .unwrap();
        assert_eq!(r2.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn invoke_routes_to_registered_handler() {
        let mut server = PluginServer::new();
        server.register("hello.greet", |req: InvokeRequest| async move {
            Ok(format!("Hello, {}!", req.args))
        });
        let (port, _) = start(server).await;
        let client = reqwest::Client::new();
        let r = client
            .post(format!("http://127.0.0.1:{port}/invoke"))
            .json(&serde_json::json!({
                "method": "hello.greet",
                "args": "alice",
                "trace_id": "deadbeef",
                "request_id": "cafef00d",
                "caller_subject_id": "00".repeat(32),
                "deadline_unix": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["ok"], serde_json::json!(true));
        assert_eq!(body["body"], serde_json::json!("Hello, alice!"));
    }

    #[tokio::test]
    async fn invoke_unknown_method_returns_protocol_error() {
        let server = PluginServer::new();
        let (port, _) = start(server).await;
        let client = reqwest::Client::new();
        let r = client
            .post(format!("http://127.0.0.1:{port}/invoke"))
            .json(&serde_json::json!({
                "method": "no.such.thing",
                "args": "",
                "trace_id": "",
                "request_id": "",
                "caller_subject_id": "",
                "deadline_unix": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["ok"], serde_json::json!(false));
        assert_eq!(
            body["error_kind"].as_u64().unwrap(),
            error_kind::UNKNOWN_METHOD as u64
        );
        assert!(body["error_cause"].as_str().unwrap().contains("no handler"));
    }

    #[tokio::test]
    async fn invoke_handler_invalid_args_maps_to_protocol_error() {
        let mut server = PluginServer::new();
        server.register("x.bad", |_| async move {
            Err::<String, _>(PluginError::invalid_args("nope"))
        });
        let (port, _) = start(server).await;
        let client = reqwest::Client::new();
        let r = client
            .post(format!("http://127.0.0.1:{port}/invoke"))
            .json(&serde_json::json!({
                "method": "x.bad",
                "args": "",
                "trace_id": "",
                "request_id": "",
                "caller_subject_id": "",
                "deadline_unix": 0,
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["ok"], serde_json::json!(false));
        assert_eq!(
            body["error_kind"].as_u64().unwrap(),
            error_kind::INVALID_ARGS as u64
        );
        assert!(body["error_cause"].as_str().unwrap().contains("nope"));
    }

    #[tokio::test]
    async fn invoke_handler_internal_error_maps_to_protocol_error() {
        let mut server = PluginServer::new();
        server.register("x.broken", |_| async move {
            Err::<String, _>(PluginError::internal("boom"))
        });
        let (port, _) = start(server).await;
        let client = reqwest::Client::new();
        let r = client
            .post(format!("http://127.0.0.1:{port}/invoke"))
            .json(&serde_json::json!({
                "method": "x.broken",
                "args": "",
                "trace_id": "",
                "request_id": "",
                "caller_subject_id": "",
                "deadline_unix": 0,
            }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(
            body["error_kind"].as_u64().unwrap(),
            error_kind::RESPONDER_INTERNAL as u64
        );
        assert!(body["error_cause"].as_str().unwrap().contains("boom"));
    }
}
