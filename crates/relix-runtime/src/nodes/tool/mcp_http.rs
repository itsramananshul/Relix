//! F13 — live MCP HTTP client.
//!
//! Companion to [`super::mcp_stdio`]. Reuses the JSON-RPC wire
//! types from `super::mcp::proto` — this file is pure HTTP +
//! lifecycle. No protocol shapes are redefined here.
//!
//! ## Transport
//!
//! One [`McpHttpClient`] per `[[tool.mcp.servers]]` entry whose
//! `transport = "http"`. The client speaks the
//! Streamable-HTTP-style variant of MCP: every JSON-RPC
//! request is sent as a single POST against the configured
//! `endpoint` URL with `Content-Type: application/json`. The
//! response body is parsed as JSON (one JSON-RPC response per
//! POST).
//!
//! Operators who need the legacy HTTP+SSE variant (initialize
//! once, subscribe to SSE for streamed responses) should
//! continue using stdio for now — the SSE response shape is
//! tracked as future work in `docs/sol-sflow-parity.md` and
//! the roadmap.
//!
//! ## Lifecycle
//!
//! The first `list_tools` or `call_tool` runs the MCP
//! `initialize` handshake against the server (POST the
//! `initialize` request, parse the result for the server's
//! capabilities). Subsequent calls skip the handshake — the
//! cached `initialized` bit is set after the first success.
//! If the server returns an error during initialize, the
//! bit stays unset and the next call retries the handshake.
//!
//! ## Retry / backoff
//!
//! Transport-level failures (connect errors, 5xx, 429) trigger
//! exponential backoff up to `reconnect_max` retries — `100ms *
//! 2^attempt`. JSON-RPC `error` responses (4xx-equivalent code
//! in the JSON-RPC envelope) propagate immediately — a server
//! that says "bad input" won't get better after a retry. EOF
//! / parse failures count as transport-level.
//!
//! ## Auth
//!
//! Each request gets an `Authorization: <value>` header iff the
//! server's `auth_header` is set. Relix does no envelope
//! wrapping — the operator writes the full header value.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use super::mcp::proto::{self, JsonRpcRequest, ToolsCallResult, ToolsListResult};

/// Default per-request HTTP timeout. Chosen to cover the
/// longest-running MCP tool calls operators have reported
/// (image generation, long-form search) without being so
/// long that a stuck server holds the controller's tokio
/// runtime hostage.
const HTTP_TIMEOUT_SECS: u64 = 60;

/// Errors produced by the HTTP client. Translated to the
/// operator-facing [`super::mcp::McpError`] vocabulary by the
/// registry layer so the envelope vocabulary stays in one
/// place.
#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("mcp http: build client: {0}")]
    BuildClient(String),
    #[error("mcp http: connect '{url}': {source}")]
    Connect {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("mcp http: read body from '{url}': {source}")]
    ReadBody {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("mcp http: HTTP {status} from '{url}': {body}")]
    BadStatus {
        url: String,
        status: u16,
        body: String,
    },
    #[error("mcp http: bad response: {0}")]
    BadResponse(String),
    #[error("mcp http: server error code {code}: {message}")]
    ServerError { code: i32, message: String },
}

impl HttpError {
    /// True if this failure is worth retrying. Connection
    /// errors and 5xx / 429 statuses are transient; 4xx and
    /// parse failures are not.
    fn is_transient(&self) -> bool {
        match self {
            HttpError::Connect { .. } => true,
            HttpError::ReadBody { .. } => true,
            HttpError::BadStatus { status, .. } => {
                *status == 429 || (*status >= 500 && *status <= 599)
            }
            HttpError::BadResponse(_) => false,
            HttpError::ServerError { .. } => false,
            HttpError::BuildClient(_) => false,
        }
    }
}

/// Per-server HTTP MCP client. Cheap to construct — the
/// `reqwest::Client` does no I/O until the first request.
pub struct McpHttpClient {
    /// Stable id (echoes [`super::mcp::McpServerConfig::id`]).
    pub server_id: String,
    /// Endpoint URL. The JSON-RPC request body is POSTed here.
    endpoint: String,
    /// Optional `Authorization` header value. None means no
    /// auth header is sent.
    auth_header: Option<String>,
    /// Reqwest client. Built once per server.
    http: reqwest::Client,
    /// Monotonic JSON-RPC id counter.
    next_id: AtomicU64,
    /// Set to `true` after the first successful `initialize`.
    initialized: AtomicBool,
    /// Max retries on transient errors.
    reconnect_max: u32,
}

impl McpHttpClient {
    /// Build an HTTP client from operator-supplied config. The
    /// `endpoint` URL is validated by `super::mcp::validate_config`
    /// before construction, so this constructor only fails on
    /// reqwest builder errors (very rare).
    pub fn new(
        server_id: String,
        endpoint: String,
        auth_header: Option<String>,
        reconnect_max: u32,
    ) -> Result<Self, HttpError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .build()
            .map_err(|e| HttpError::BuildClient(e.to_string()))?;
        Ok(Self {
            server_id,
            endpoint,
            auth_header,
            http,
            next_id: AtomicU64::new(1),
            initialized: AtomicBool::new(false),
            reconnect_max,
        })
    }

    /// POST a JSON-RPC request and parse the response. Retries
    /// transient failures with exponential backoff. The caller
    /// gets either a parsed result value or a structured
    /// error; the JSON-RPC layer is fully unwrapped.
    async fn call(&self, req: &JsonRpcRequest) -> Result<Value, HttpError> {
        let body = serde_json::to_string(req)
            .map_err(|e| HttpError::BadResponse(format!("serialize request: {e}")))?;
        let mut attempt: u32 = 0;
        loop {
            match self.attempt_call(&body).await {
                Ok(value) => return Ok(value),
                Err(err) if err.is_transient() && attempt < self.reconnect_max => {
                    let delay_ms = 100u64 << attempt.min(8);
                    tracing::warn!(
                        server = %self.server_id,
                        attempt = attempt,
                        delay_ms = delay_ms,
                        error = %err,
                        "mcp http: transient failure; retrying with backoff"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    attempt += 1;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Single HTTP attempt — no retry. Pulled out of `call` so
    /// the retry loop can re-invoke without rebuilding the
    /// serialized body.
    async fn attempt_call(&self, body: &str) -> Result<Value, HttpError> {
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(body.to_string());
        if let Some(value) = &self.auth_header {
            req = req.header("authorization", value);
        }
        let resp = req.send().await.map_err(|e| HttpError::Connect {
            url: self.endpoint.clone(),
            source: e,
        })?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| HttpError::ReadBody {
            url: self.endpoint.clone(),
            source: e,
        })?;
        if !(200..300).contains(&status) {
            return Err(HttpError::BadStatus {
                url: self.endpoint.clone(),
                status,
                body: text,
            });
        }
        // Parse a JSON-RPC response envelope. The MCP spec
        // allows either a single JSON object OR an SSE stream
        // — we only handle the single-object case here.
        let parsed = proto::parse_response_line(text.trim())
            .map_err(|e| HttpError::BadResponse(format!("parse response: {e}; body: {text}")))?;
        if let Some(err) = parsed.error {
            return Err(HttpError::ServerError {
                code: err.code,
                message: err.message,
            });
        }
        parsed
            .result
            .ok_or_else(|| HttpError::BadResponse("response had no `result` field".into()))
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Run the MCP `initialize` handshake. Idempotent — the
    /// cached `initialized` bit short-circuits subsequent
    /// invocations.
    async fn ensure_initialized(&self) -> Result<(), HttpError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let req = JsonRpcRequest::initialize(self.next_id(), "relix", env!("CARGO_PKG_VERSION"));
        let _ = self.call(&req).await?;
        // Some MCP servers expect a `notifications/initialized`
        // notification immediately after the response. We
        // emit it best-effort — a missing-method response is
        // tolerated; the server signals readiness anyway by
        // accepting subsequent calls.
        let notif = proto::JsonRpcNotification::initialized();
        let body = serde_json::to_string(&notif)
            .map_err(|e| HttpError::BadResponse(format!("serialize notification: {e}")))?;
        // Notifications don't expect a body, so we tolerate any
        // non-2xx silently. Same goes for connect errors — the
        // initialized bit is set on the back of the
        // initialize handshake, not the notification.
        let mut req = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .body(body);
        if let Some(value) = &self.auth_header {
            req = req.header("authorization", value);
        }
        let _ = req.send().await;
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    /// `tools/list`. Returns the live tool list as reported by
    /// the server. Caller is the registry's
    /// `list_tools(server_id)` handler.
    pub async fn list_tools(&self) -> Result<ToolsListResult, HttpError> {
        self.ensure_initialized().await?;
        let req = JsonRpcRequest::tools_list(self.next_id());
        let value = self.call(&req).await?;
        serde_json::from_value(value)
            .map_err(|e| HttpError::BadResponse(format!("decode ToolsListResult: {e}")))
    }

    /// `tools/call`. Forwards the operator-supplied
    /// `arguments` JSON value to the server and returns the
    /// parsed result. The caller surfaces the encoded result
    /// to the SOL flow.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<ToolsCallResult, HttpError> {
        self.ensure_initialized().await?;
        let req = JsonRpcRequest::tools_call(self.next_id(), name, args);
        let value = self.call(&req).await?;
        serde_json::from_value(value)
            .map_err(|e| HttpError::BadResponse(format!("decode ToolsCallResult: {e}")))
    }
}

/// Translate a [`HttpError`] into the operator-facing
/// [`super::mcp::McpError`] vocabulary. Bad-shape responses
/// surface as `BadResponse`; everything else (including
/// server-side JSON-RPC errors) maps to `RuntimeNotConnected`
/// with the underlying cause prefixed `mcp:`. Matches the
/// stdio side's posture.
pub(crate) fn map_http_err(e: &HttpError) -> super::mcp::McpError {
    match e {
        HttpError::BadResponse(_) => super::mcp::McpError::BadResponse {
            reason: format!("mcp: bad response: {e}"),
        },
        _ => super::mcp::McpError::RuntimeNotConnected {
            reason: format!("mcp: {e}"),
        },
    }
}

/// Public wrapper around the registry-internal helper. Used
/// by the boot-time discovery path to give operators a single
/// shared `Arc<McpHttpClient>` per HTTP server.
pub fn build_client(
    server_id: String,
    endpoint: String,
    auth_header: Option<String>,
    reconnect_max: u32,
) -> Result<Arc<McpHttpClient>, HttpError> {
    Ok(Arc::new(McpHttpClient::new(
        server_id,
        endpoint,
        auth_header,
        reconnect_max,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Boot a tiny mock MCP HTTP server on a random port.
    /// Each request is logged into `received`; the response
    /// body is taken from `responses` in order (cycles after
    /// the last one).
    async fn boot_mock_server(
        responses: Vec<String>,
    ) -> (SocketAddr, Arc<Mutex<Vec<String>>>, oneshot::Sender<()>) {
        use axum::routing::post;
        use axum::{Router, extract::State, http::HeaderMap, response::IntoResponse};

        #[derive(Clone)]
        struct AppState {
            received: Arc<Mutex<Vec<String>>>,
            responses: Arc<Vec<String>>,
            cursor: Arc<AtomicUsize>,
        }

        async fn handler(
            State(state): State<AppState>,
            headers: HeaderMap,
            body: String,
        ) -> impl IntoResponse {
            let mut log = state.received.lock().unwrap();
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            log.push(format!("{auth}|{body}"));
            drop(log);
            let cursor = state.cursor.fetch_add(1, Ordering::Relaxed);
            let idx = cursor % state.responses.len().max(1);
            let resp = state.responses.get(idx).cloned().unwrap_or_default();
            (
                axum::http::StatusCode::OK,
                [("content-type", "application/json")],
                resp,
            )
        }

        let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app_state = AppState {
            received: received.clone(),
            responses: Arc::new(responses),
            cursor: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route("/mcp", post(handler))
            .with_state(app_state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .ok();
        });
        // Give the server a moment to start listening.
        tokio::time::sleep(Duration::from_millis(20)).await;
        (addr, received, shutdown_tx)
    }

    /// Initialize response, then a tools/list response. We
    /// also send a `notifications/initialized` POST between
    /// the two — the mock sees it as a "consume one
    /// response slot" but the client ignores the body. So
    /// the responses array has THREE entries: initialize,
    /// notifications-stub (any 200), tools/list.
    fn fixed_initialize_and_tools_list_responses() -> Vec<String> {
        vec![
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "mock", "version": "0.1"}
                }
            })
            .to_string(),
            // The notification POST. Server returns
            // anything 200; the client ignores the body.
            "{}".to_string(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [{
                        "name": "say_hello",
                        "description": "greet the operator",
                        "inputSchema": {"type": "object"}
                    }]
                }
            })
            .to_string(),
        ]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_mcp_server_returns_tool_list() {
        let (addr, received, shutdown) =
            boot_mock_server(fixed_initialize_and_tools_list_responses()).await;
        let endpoint = format!("http://{addr}/mcp");
        let client = McpHttpClient::new(
            "mock".into(),
            endpoint,
            None,
            0, // no retries
        )
        .expect("build client");
        let result = client.list_tools().await.expect("list_tools");
        assert_eq!(result.tools.len(), 1);
        assert_eq!(result.tools[0].name, "say_hello");
        // Server must have seen at least the initialize POST
        // and the tools/list POST.
        let log = received.lock().unwrap();
        assert!(log.len() >= 2, "expected ≥2 requests, got {}", log.len());
        assert!(
            log[0].contains("initialize"),
            "first request must be initialize: {:?}",
            log[0]
        );
        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_mcp_call_tool_round_trips() {
        let responses = vec![
            // initialize
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
            })
            .to_string(),
            // notifications/initialized — body ignored
            "{}".to_string(),
            // tools/call
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{"type": "text", "text": "hi there"}],
                    "isError": false
                }
            })
            .to_string(),
        ];
        let (addr, _received, shutdown) = boot_mock_server(responses).await;
        let endpoint = format!("http://{addr}/mcp");
        let client = McpHttpClient::new("mock".into(), endpoint, None, 0).expect("build");
        let res = client
            .call_tool("say_hello", serde_json::json!({"name": "alice"}))
            .await
            .expect("call_tool");
        assert_eq!(res.content.len(), 1);
        match &res.content[0] {
            super::super::mcp::proto::ToolsCallContent::Text { text } => {
                assert_eq!(text, "hi there");
            }
            other => panic!("expected Text content, got {other:?}"),
        }
        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_mcp_sends_auth_header_when_configured() {
        let (addr, received, shutdown) =
            boot_mock_server(fixed_initialize_and_tools_list_responses()).await;
        let endpoint = format!("http://{addr}/mcp");
        let client = McpHttpClient::new(
            "mock".into(),
            endpoint,
            Some("Bearer test-token-123".into()),
            0,
        )
        .expect("build");
        let _ = client.list_tools().await.expect("list_tools");
        let log = received.lock().unwrap();
        assert!(
            log.iter()
                .any(|line| line.starts_with("Bearer test-token-123|")),
            "expected at least one request with auth header, got: {log:?}"
        );
        let _ = shutdown.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_mcp_retries_then_succeeds_on_transient_error() {
        // First N responses are 5xx (we'll model this by
        // having the mock return a JSON-RPC error response
        // for the first call — actually a 5xx triggers
        // retry, while a JSON-RPC error does NOT. So the
        // real shape we need is "the mock 5xx's the first
        // request, then 200's the second". The fixed mock
        // above always 200s, so this test uses a slightly
        // different mock surface: an axum handler with a
        // gated counter.
        use axum::routing::post;
        use axum::{Router, extract::State, response::IntoResponse};

        #[derive(Clone)]
        struct AppState {
            cursor: Arc<AtomicUsize>,
            success_after: usize,
        }

        async fn handler(State(state): State<AppState>, _body: String) -> impl IntoResponse {
            let i = state.cursor.fetch_add(1, Ordering::Relaxed);
            if i < state.success_after {
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    [("content-type", "text/plain")],
                    "transient".to_string(),
                );
            }
            // success — return whatever
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2024-11-05", "capabilities": {}}
            })
            .to_string();
            (
                axum::http::StatusCode::OK,
                [("content-type", "application/json")],
                body,
            )
        }

        let state = AppState {
            cursor: Arc::new(AtomicUsize::new(0)),
            success_after: 2, // fail twice, succeed on attempt 3
        };
        let cursor_ref = state.cursor.clone();
        let app = Router::new().route("/mcp", post(handler)).with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let endpoint = format!("http://{addr}/mcp");
        let client = McpHttpClient::new("mock".into(), endpoint, None, 3).expect("build");
        // initialize is the first call. It should succeed
        // after retrying.
        client.ensure_initialized().await.expect("initialize");
        let attempts = cursor_ref.load(Ordering::Relaxed);
        assert!(attempts >= 3, "expected ≥3 attempts, got {attempts}");
        let _ = tx.send(());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_mcp_gives_up_after_max_retries() {
        // Server always 500s — exceed the retry budget.
        use axum::routing::post;
        use axum::{Router, response::IntoResponse};

        async fn handler(_body: String) -> impl IntoResponse {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [("content-type", "text/plain")],
                "always-down".to_string(),
            )
        }

        let app = Router::new().route("/mcp", post(handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await
                .ok();
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let endpoint = format!("http://{addr}/mcp");
        let client = McpHttpClient::new("mock".into(), endpoint, None, 2).expect("build");
        let err = client.ensure_initialized().await.expect_err("must fail");
        match &err {
            HttpError::BadStatus { status, .. } => assert_eq!(*status, 503),
            other => panic!("expected BadStatus, got {other:?}"),
        }
        // Mapped to operator-facing error vocabulary.
        let mapped = map_http_err(&err);
        match mapped {
            super::super::mcp::McpError::RuntimeNotConnected { .. } => {}
            other => panic!("expected RuntimeNotConnected, got {other:?}"),
        }
        let _ = tx.send(());
    }
}
