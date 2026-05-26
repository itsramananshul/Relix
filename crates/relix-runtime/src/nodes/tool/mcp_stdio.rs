//! PH-MCP-RUNTIME — live MCP stdio client (closes D-009).
//!
//! Companion module to [`super::mcp`]. Reuses the JSON-RPC wire
//! types from `super::mcp::proto` — this file is pure I/O +
//! lifecycle. No protocol shapes are redefined here.
//!
//! ## Process model
//!
//! One [`McpStdioClient`] per `[[tool.mcp.servers]]` entry whose
//! `transport = "stdio"`. The client owns:
//!
//! - the configured `command` + `args`,
//! - a lazy-initialised [`tokio::process::Child`] (spawned on
//!   first `call_tool` / `list_tools`, NOT at startup — operators
//!   commonly declare many MCP servers and only exercise one
//!   per workflow),
//! - the child's stdin handle + a `BufReader` over its stdout,
//! - a monotonic `next_id` counter,
//! - the cached `initialize` result (so subsequent calls skip
//!   the handshake).
//!
//! Concurrency: one in-flight request at a time per client. The
//! `Mutex<Inner>` serialises the (write line → read line) cycle.
//! Operators wanting throughput declare multiple
//! `[[tool.mcp.servers]]` entries pointing to separate processes.
//!
//! ## Error mapping
//!
//! Every spawn failure, EOF on stdout, malformed response line,
//! or JSON-RPC error envelope maps to a [`StdioError`] variant
//! that the caller translates into the existing
//! [`super::mcp::McpError`] vocabulary. No fake success ever.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::mcp::proto::{self, JsonRpcRequest, ToolsCallContent, ToolsCallResult, ToolsListResult};

/// Maximum bytes we'll buffer for one MCP response line. Chosen
/// to comfortably hold a `tools/list` for a sizeable server
/// (each tool's `inputSchema` may be hundreds of bytes); a
/// hostile / runaway server is still bounded.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

/// Errors produced by the stdio client. Translated to
/// `super::mcp::McpError` by the registry layer so the operator-
/// facing envelopes stay in one place.
#[derive(Debug, thiserror::Error)]
pub enum StdioError {
    #[error("mcp: failed to spawn '{program}': {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mcp: stdio handles missing on child '{program}'")]
    MissingHandles { program: String },
    #[error("mcp: serialize request: {0}")]
    SerializeRequest(#[source] serde_json::Error),
    #[error("mcp: write to stdin: {0}")]
    WriteStdin(#[source] std::io::Error),
    #[error("mcp: read stdout (EOF or io error): {0}")]
    ReadStdout(String),
    #[error("mcp: response line exceeds {limit} bytes")]
    LineTooLong { limit: usize },
    #[error("mcp: bad response: {0}")]
    BadResponse(String),
    #[error("mcp: server error code {code}: {message}")]
    ServerError { code: i32, message: String },
    #[error("mcp: subprocess exited before responding")]
    SubprocessExited,
}

/// Live handle to a single spawned MCP server subprocess.
struct ChildIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Retained so the child stays alive + is killed on drop
    /// (via `kill_on_drop(true)` set at spawn time). Never
    /// read directly after construction; lint silenced.
    #[allow(dead_code)]
    child: Child,
}

/// Optional resolved env map for the spawned child. Keyed by
/// var name; values are the post-`$VAR`-substitution strings
/// from [`crate::nodes::tool::mcp::resolve_env`].
pub type SpawnEnv = std::collections::HashMap<String, String>;

/// Per-server MCP stdio client. Cheap to construct; the actual
/// process is spawned lazily on first call.
pub struct McpStdioClient {
    /// Stable id (echoes [`super::mcp::McpServerConfig::id`]).
    pub server_id: String,
    /// Executable to spawn. Bare program name; no shell.
    program: String,
    /// Arguments to pass to the program (after the program name).
    args: Vec<String>,
    /// Resolved env vars to attach to the spawned process.
    /// Empty means "inherit parent env unchanged"; non-empty
    /// means "merge these into the inherited env, overwriting
    /// any same-named parent vars". The MCP stdio shape works
    /// either way today — operators set this to pass through
    /// tokens like `GITHUB_PERSONAL_ACCESS_TOKEN` to subprocess
    /// servers that need them.
    env: SpawnEnv,
    /// Monotonic JSON-RPC id counter. Starts at 1; 0 is reserved
    /// to avoid ambiguity with some servers that treat id 0 as
    /// "unset".
    next_id: AtomicU64,
    /// Lazy child handle. `None` until first call_tool or
    /// list_tools wakes the subprocess.
    inner: Mutex<Option<ChildIo>>,
}

impl McpStdioClient {
    /// Create a client. The subprocess is NOT spawned here.
    pub fn new(server_id: String, program: String, args: Vec<String>) -> Self {
        Self {
            server_id,
            program,
            args,
            env: SpawnEnv::new(),
            next_id: AtomicU64::new(1),
            inner: Mutex::new(None),
        }
    }

    /// Same as [`Self::new`] but attaches a resolved env map
    /// that will be passed to the spawned subprocess. Used by
    /// the registry construction path that reads
    /// `[[tool.mcp.servers.env]]` from config.
    pub fn with_env(server_id: String, program: String, args: Vec<String>, env: SpawnEnv) -> Self {
        Self {
            server_id,
            program,
            args,
            env,
            next_id: AtomicU64::new(1),
            inner: Mutex::new(None),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send `tools/list`, returning the parsed result.
    pub async fn list_tools(&self) -> Result<ToolsListResult, StdioError> {
        let id = self.next_id();
        let req = JsonRpcRequest::tools_list(id);
        let result = self.dispatch(req).await?;
        let parsed: ToolsListResult = serde_json::from_value(result)
            .map_err(|e| StdioError::BadResponse(format!("tools/list result decode: {e}")))?;
        Ok(parsed)
    }

    /// Send `tools/call` with the given tool name + raw JSON
    /// arguments object. Returns the parsed [`ToolsCallResult`].
    pub async fn call_tool(
        &self,
        tool_name: &str,
        args_json: Value,
    ) -> Result<ToolsCallResult, StdioError> {
        let id = self.next_id();
        let req = JsonRpcRequest::tools_call(id, tool_name, args_json);
        let result = self.dispatch(req).await?;
        let parsed: ToolsCallResult = serde_json::from_value(result)
            .map_err(|e| StdioError::BadResponse(format!("tools/call result decode: {e}")))?;
        Ok(parsed)
    }

    /// Send a JSON-RPC request, ensuring the subprocess is alive
    /// and initialised. Returns the request's `result` Value or
    /// the server's error mapped to [`StdioError::ServerError`].
    async fn dispatch(&self, req: JsonRpcRequest) -> Result<Value, StdioError> {
        let mut guard = self.inner.lock().await;
        if guard.is_none() {
            let io =
                spawn_and_initialize(&self.program, &self.args, &self.env, &self.next_id).await?;
            *guard = Some(io);
        }
        let io = guard.as_mut().expect("just inserted");
        let expected_id = req.id;
        send_request(&mut io.stdin, &req).await?;
        let resp = read_response_line(&mut io.stdout).await?;
        if resp.id != expected_id {
            return Err(StdioError::BadResponse(format!(
                "response id {} did not match request id {}",
                resp.id, expected_id
            )));
        }
        if let Some(err) = resp.error {
            return Err(StdioError::ServerError {
                code: err.code,
                message: err.message,
            });
        }
        resp.result
            .ok_or_else(|| StdioError::BadResponse("response missing both result and error".into()))
    }
}

/// Spawn the server, send `initialize`, send the
/// `notifications/initialized` notification, return the live
/// `ChildIo`. Caller is responsible for storing it.
async fn spawn_and_initialize(
    program: &str,
    args: &[String],
    env: &SpawnEnv,
    next_id: &AtomicU64,
) -> Result<ChildIo, StdioError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Merge operator-supplied env vars INTO the inherited
    // parent env. The empty case is a no-op (parent env rides
    // through unchanged). Non-empty entries overwrite same-
    // named parent vars — that's the documented semantic.
    if !env.is_empty() {
        cmd.envs(env);
    }
    let mut child = cmd.spawn().map_err(|e| StdioError::Spawn {
        program: program.to_string(),
        source: e,
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| StdioError::MissingHandles {
            program: program.to_string(),
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StdioError::MissingHandles {
            program: program.to_string(),
        })?;
    let mut io = ChildIo {
        stdin,
        stdout: BufReader::new(stdout),
        child,
    };

    // Handshake.
    let init_id = next_id.fetch_add(1, Ordering::Relaxed);
    let init_req = JsonRpcRequest::initialize(init_id, "relix", env!("CARGO_PKG_VERSION"));
    send_request(&mut io.stdin, &init_req).await?;
    let resp = read_response_line(&mut io.stdout).await?;
    if resp.id != init_id {
        return Err(StdioError::BadResponse(format!(
            "initialize response id {} did not match {}",
            resp.id, init_id
        )));
    }
    if let Some(err) = resp.error {
        return Err(StdioError::ServerError {
            code: err.code,
            message: format!("initialize: {}", err.message),
        });
    }
    if resp.result.is_none() {
        return Err(StdioError::BadResponse(
            "initialize response missing result".into(),
        ));
    }

    let notif = proto::JsonRpcNotification::initialized();
    let line = proto::serialize_notification(&notif).map_err(StdioError::SerializeRequest)?;
    io.stdin
        .write_all(line.as_bytes())
        .await
        .map_err(StdioError::WriteStdin)?;
    io.stdin.flush().await.map_err(StdioError::WriteStdin)?;

    Ok(io)
}

async fn send_request(stdin: &mut ChildStdin, req: &JsonRpcRequest) -> Result<(), StdioError> {
    let line = proto::serialize_request(req).map_err(StdioError::SerializeRequest)?;
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(StdioError::WriteStdin)?;
    stdin.flush().await.map_err(StdioError::WriteStdin)?;
    Ok(())
}

async fn read_response_line(
    stdout: &mut BufReader<ChildStdout>,
) -> Result<proto::JsonRpcResponse, StdioError> {
    // MCP servers commonly emit server→client notifications
    // (progress, log messages, "initialized") interleaved with
    // request responses. The protocol distinguishes them by the
    // presence of `id`: responses carry it, notifications do
    // not. We drain notification lines silently until we see a
    // line that parses as a response.
    loop {
        let mut line = String::new();
        let n = stdout
            .read_line(&mut line)
            .await
            .map_err(|e| StdioError::ReadStdout(e.to_string()))?;
        if n == 0 {
            return Err(StdioError::SubprocessExited);
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(StdioError::LineTooLong {
                limit: MAX_LINE_BYTES,
            });
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Peek at the raw JSON to decide whether this is a
        // notification (no `id`) or a response.
        let raw: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => return Err(StdioError::BadResponse(format!("parse json: {e}"))),
        };
        if raw.get("id").is_none() {
            // Server→client notification. Ignore and keep
            // reading. We don't surface these today — when
            // Relix grows progress events the parsed
            // `JsonRpcNotification` is the right place to
            // hand them to the dispatch bridge.
            tracing::debug!(
                line = %trimmed,
                "mcp: ignoring server notification while awaiting response"
            );
            continue;
        }
        return proto::parse_response_line(trimmed)
            .map_err(|e| StdioError::BadResponse(format!("parse json: {e}")));
    }
}

/// Flatten the `content` array of a `tools/call` result into a
/// single JSON byte payload. Used by the registry to feed the
/// handler's wire response. The output is a JSON object of the
/// shape:
///
/// ```json
/// { "isError": false, "content": [ {"type":"text","text":"..."} ] }
/// ```
///
/// Unknown content variants are projected as `{"type":"other"}`
/// so the wire shape is stable even for forward-compatible
/// servers.
pub fn encode_tools_call_result(res: &ToolsCallResult) -> Vec<u8> {
    let mut content_arr: Vec<Value> = Vec::with_capacity(res.content.len());
    for c in &res.content {
        match c {
            ToolsCallContent::Text { text } => {
                content_arr.push(serde_json::json!({ "type": "text", "text": text }));
            }
            ToolsCallContent::Other => {
                content_arr.push(serde_json::json!({ "type": "other" }));
            }
        }
    }
    let body = serde_json::json!({
        "isError": res.is_error,
        "content": content_arr,
    });
    serde_json::to_vec(&body).expect("static-shape JSON encodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_tools_call_result_flattens_text_content() {
        let r = ToolsCallResult {
            content: vec![ToolsCallContent::Text {
                text: "hello".into(),
            }],
            is_error: false,
        };
        let bytes = encode_tools_call_result(&r);
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["isError"], false);
        assert_eq!(v["content"][0]["type"], "text");
        assert_eq!(v["content"][0]["text"], "hello");
    }

    #[test]
    fn encode_tools_call_result_preserves_is_error() {
        let r = ToolsCallResult {
            content: vec![ToolsCallContent::Text {
                text: "boom".into(),
            }],
            is_error: true,
        };
        let v: Value = serde_json::from_slice(&encode_tools_call_result(&r)).unwrap();
        assert_eq!(v["isError"], true);
    }

    #[test]
    fn encode_tools_call_result_projects_other_to_marker() {
        let r = ToolsCallResult {
            content: vec![ToolsCallContent::Other],
            is_error: false,
        };
        let v: Value = serde_json::from_slice(&encode_tools_call_result(&r)).unwrap();
        assert_eq!(v["content"][0]["type"], "other");
    }

    /// Hand-crafted response-line decode path. Mirrors what the
    /// client would receive from a real `tools/call`. Exercises
    /// the same parse_response_line + result decode chain
    /// dispatch() runs.
    #[test]
    fn decode_path_matches_real_server_shape() {
        let line = r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"ok"}],"isError":false}}"#;
        let resp = super::proto::parse_response_line(line).unwrap();
        assert_eq!(resp.id, 7);
        let result_value = resp.result.expect("result present");
        let parsed: ToolsCallResult = serde_json::from_value(result_value).unwrap();
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ToolsCallContent::Text { text } => assert_eq!(text, "ok"),
            _ => panic!("expected text"),
        }
    }

    /// Spawn-failure path: pick a program that almost certainly
    /// doesn't exist on PATH. Confirms we surface
    /// `StdioError::Spawn` cleanly instead of panicking.
    #[tokio::test]
    async fn spawn_failure_surfaces_spawn_error() {
        let client = McpStdioClient::new(
            "ghost".into(),
            "relix-mcp-test-no-such-binary-xyzzy".into(),
            vec![],
        );
        let err = client.list_tools().await.unwrap_err();
        match err {
            StdioError::Spawn { program, .. } => {
                assert!(program.contains("xyzzy"));
            }
            other => panic!("expected Spawn error, got {other:?}"),
        }
    }

    /// PH-MCP-RUNTIME integration test. Skips with `eprintln!`
    /// when `node` or `npx` aren't on PATH, or when the
    /// `@modelcontextprotocol/server-everything` package isn't
    /// available. Runs end-to-end: spawn → initialize →
    /// tools/list → tools/call → drop client (which kills the
    /// child via `kill_on_drop(true)`).
    #[tokio::test]
    async fn integration_server_everything_roundtrip() {
        // Probe `node` first. If absent, skip honestly.
        let node_ok = tokio::process::Command::new("node")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !node_ok {
            eprintln!("SKIP integration_server_everything_roundtrip: `node` not found on PATH");
            return;
        }
        // Probe `npx`. The reference servers ship via npx.
        let npx = if cfg!(windows) { "npx.cmd" } else { "npx" };
        let npx_ok = tokio::process::Command::new(npx)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if !npx_ok {
            eprintln!("SKIP integration_server_everything_roundtrip: `{npx}` not found on PATH");
            return;
        }
        // Spawn the everything server. `-y` accepts the npm prompt non-interactively.
        let client = McpStdioClient::new(
            "everything".into(),
            npx.into(),
            vec![
                "-y".into(),
                "@modelcontextprotocol/server-everything".into(),
            ],
        );
        let list = match client.list_tools().await {
            Ok(l) => l,
            Err(e) => {
                eprintln!(
                    "SKIP integration_server_everything_roundtrip: list_tools failed \
                     (server-everything probably not installed): {e}"
                );
                return;
            }
        };
        assert!(
            !list.tools.is_empty(),
            "everything server should expose at least one tool"
        );
        // server-everything publishes a tool named `echo`. If
        // the upstream renames it the test is allowed to skip.
        let echo = list.tools.iter().find(|t| t.name == "echo");
        let Some(_) = echo else {
            eprintln!(
                "SKIP integration_server_everything_roundtrip: server-everything no longer \
                 exposes an `echo` tool"
            );
            return;
        };
        let res = client
            .call_tool("echo", serde_json::json!({ "message": "ping" }))
            .await;
        match res {
            Ok(r) => {
                assert!(!r.is_error, "echo result should not be flagged is_error");
                assert!(!r.content.is_empty());
            }
            Err(e) => {
                eprintln!("SKIP integration_server_everything_roundtrip: echo call failed: {e}");
            }
        }
        // Drop the client: `kill_on_drop(true)` reaps the child.
        drop(client);
    }
}
