//! # relix-sdk
//!
//! Minimal Rust SDK for talking to a running Relix bridge over
//! HTTP. Deliberately does NOT depend on `relix-runtime` — app
//! developers get reqwest + serde + this thin client and nothing
//! else. The wire contract is the bridge's HTTP surface; this
//! crate is a typed convenience layer over it.
//!
//! ## Usage
//!
//! ```no_run
//! # async fn run() -> Result<(), relix_sdk::RelixError> {
//! let client = relix_sdk::RelixClient::new(
//!     "http://127.0.0.1:19791",
//!     "your-bridge-token",
//! );
//! let reply = client.chat("Hello, Relix!").await?;
//! println!("{reply}");
//! # Ok(())
//! # }
//! ```
//!
//! ## Tenant scoping
//!
//! Every request carries an opaque `tenant_id` header
//! (`X-Relix-Tenant`). Defaults to `"default"`. The bridge wires
//! the value through to task creation and audit log entries; the
//! mesh today does not enforce isolation across tenants — the
//! field is the foundation for multi-tenant deployments.
//! Configure via [`RelixClient::with_tenant`].

#![forbid(unsafe_code)]

use std::time::Duration;

use futures::Stream;
use serde::{Deserialize, Serialize};

/// Client error class.
#[derive(Debug, thiserror::Error)]
pub enum RelixError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

/// One memory search hit returned by [`RelixClient::search`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryResult {
    /// Stable id assigned by the memory peer.
    pub id: String,
    /// Verbatim text content of the hit.
    pub content: String,
    /// Tags the entry carried at write time, in original order.
    pub tags: Vec<String>,
    /// Cosine-similarity score in `[0.0, 1.0]`. `None` when the
    /// backend doesn't expose scores (mock provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

/// Full result of a chat call, including the durable task/workspace
/// binding the bridge created for it.
///
/// Phase 2 of the product spine ("task-bound execution") makes every
/// chat an explicit task-bound run; this struct surfaces that binding
/// to SDK callers so they can follow the run via `/v1/tasks/{task_id}`
/// or correlate it in the activity ledger. [`RelixClient::chat`]
/// returns just the reply text for the common case; use
/// [`RelixClient::chat_full`] when you need the scope ids.
///
/// All binding fields are optional: they are `None` when the bridge
/// has no coordinator wired (so no durable task was created) or no
/// workspace lease was bound to the call.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatReply {
    /// The assistant's reply text.
    pub reply: String,
    /// Coordinator-side durable task id this chat created, when a
    /// coordinator was wired and `task.create` succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Workspace lease id bound to the call, when the request carried
    /// (or resolved) an active workspace lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_lease_id: Option<String>,
    /// Resolved workspace path for the bound lease, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// Bridge flow id for the chat execution (always present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// Bridge trace id for the chat execution (always present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

/// Information about the running Relix bridge, returned by
/// `GET /v1/info`. Stable across patch versions; new fields land
/// as additive optional keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelixInfo {
    pub system: String,
    pub version: String,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Default tenant identifier — `"default"`. Used until the
/// caller flips it via [`RelixClient::with_tenant`].
pub const DEFAULT_TENANT: &str = "default";

/// HTTP-over-bridge client. Cheap to clone (`reqwest::Client`
/// already shares its connection pool internally). One client
/// per app process is normal; one per tenant is also fine.
#[derive(Clone)]
pub struct RelixClient {
    base_url: String,
    token: String,
    tenant: String,
    http: reqwest::Client,
}

impl RelixClient {
    /// Construct a new client. `base_url` is the bridge's HTTP
    /// root (e.g. `http://127.0.0.1:19791`); `token` is the
    /// bearer the bridge accepts via `Authorization: Bearer <token>`
    /// (generated on first boot at `~/.relix/bridge-token`).
    ///
    /// Defaults: 30s request timeout, no retries (caller decides),
    /// tenant = `"default"`.
    pub fn new(base_url: &str, token: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            tenant: DEFAULT_TENANT.to_string(),
            http,
        }
    }

    /// Replace the tenant identifier the client sends with each
    /// request. The value is opaque to the SDK — anything the
    /// bridge admits is fine.
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// Current tenant id. Useful in tests / debug logs.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Current base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Read the bridge's server info (`GET /v1/info`). The fields
    /// match the [`RelixInfo`] doc strings.
    pub async fn info(&self) -> Result<RelixInfo, RelixError> {
        let url = format!("{}/v1/info", self.base_url);
        let r = self
            .http
            .get(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-relix-tenant", &self.tenant)
            .send()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        let status = r.status();
        let body = r
            .text()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(RelixError::Http {
                status: status.as_u16(),
                body,
            });
        }
        serde_json::from_str(&body).map_err(|e| RelixError::Decode(e.to_string()))
    }

    /// One-shot chat call. Returns the assistant's reply text.
    /// Uses the bridge's `POST /chat` endpoint (the native Relix
    /// shape — the OpenAI-compat shim sits alongside but is not
    /// what an SDK targets).
    ///
    /// This is the convenience form that discards the spine binding.
    /// Use [`RelixClient::chat_full`] to also obtain the durable
    /// `task_id` and workspace scope the bridge bound to the call.
    pub async fn chat(&self, prompt: &str) -> Result<String, RelixError> {
        Ok(self.chat_full(prompt).await?.reply)
    }

    /// One-shot chat call returning the reply text together with the
    /// durable task/workspace binding the bridge created (Phase 2 of
    /// the product spine). Callers that want to follow or audit the
    /// run read `task_id` off the returned [`ChatReply`].
    pub async fn chat_full(&self, prompt: &str) -> Result<ChatReply, RelixError> {
        self.chat_full_in_workspace(prompt, None).await
    }

    /// Like [`RelixClient::chat_full`] but binds the chat to an
    /// existing workspace lease (Phase 4 "execution workspaces"). The
    /// bridge resolves `workspace_lease_id` against the tenant's
    /// active leases and stamps the resolved path into the dispatch
    /// envelope; the resolved lease/path are echoed back on the
    /// returned [`ChatReply`].
    pub async fn chat_full_in_workspace(
        &self,
        prompt: &str,
        workspace_lease_id: Option<&str>,
    ) -> Result<ChatReply, RelixError> {
        let url = format!("{}/chat", self.base_url);
        let mut body = serde_json::Map::new();
        body.insert("session_id".into(), new_session_id(&self.tenant).into());
        body.insert("message".into(), prompt.into());
        if let Some(lease) = workspace_lease_id.map(str::trim).filter(|s| !s.is_empty()) {
            body.insert("workspace_lease_id".into(), lease.into());
        }
        let r = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-relix-tenant", &self.tenant)
            .header("content-type", "application/json")
            .body(serde_json::Value::Object(body).to_string())
            .send()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        let status = r.status();
        let text = r
            .text()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(RelixError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        // `POST /chat` returns `{ "reply": "...", "task_id": ..., ... }`.
        // `ChatReply` uses `#[serde(default)]` on every optional field,
        // so additional bridge fields (flow_log, etc.) are ignored and
        // missing binding fields decode as `None`.
        let reply: ChatReply =
            serde_json::from_str(&text).map_err(|e| RelixError::Decode(e.to_string()))?;
        if reply.reply.is_empty() && !text.contains("\"reply\"") {
            return Err(RelixError::Decode(format!(
                "no `reply` in response: {text}"
            )));
        }
        Ok(reply)
    }

    /// Streaming chat via the bridge's SSE endpoint
    /// (`POST /chat/stream`). Each emitted item is one chunk of
    /// the reply; concatenating all items yields the full text.
    pub async fn chat_stream(
        &self,
        prompt: &str,
    ) -> Result<impl Stream<Item = Result<String, RelixError>> + Send + 'static, RelixError> {
        let url = format!("{}/chat/stream", self.base_url);
        let body = serde_json::json!({
            "session_id": new_session_id(&self.tenant),
            "message": prompt,
        });
        let r = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-relix-tenant", &self.tenant)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(RelixError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let byte_stream = r.bytes_stream();
        let s = async_stream::stream! {
            use futures::StreamExt;
            let mut byte_stream = std::pin::pin!(byte_stream);
            let mut buf = String::new();
            while let Some(chunk) = byte_stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(e) => {
                        yield Err(RelixError::Transport(e.to_string()));
                        return;
                    }
                };
                let s = match std::str::from_utf8(&bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => continue,
                };
                buf.push_str(&s);
                // SSE frames are separated by a blank line. CRLF is
                // tolerated for callers routed through a non-Relix
                // proxy that rewrites line endings.
                loop {
                    let crlf = buf.find("\r\n\r\n");
                    let lf = buf.find("\n\n");
                    let (end, sep_len) = match (crlf, lf) {
                        (Some(c), Some(l)) if c < l => (c, 4),
                        (_, Some(l)) => (l, 2),
                        (Some(c), None) => (c, 4),
                        (None, None) => break,
                    };
                    let frame = buf[..end].to_string();
                    buf.drain(..end + sep_len);
                    let mut event_kind: Option<String> = None;
                    let mut data_parts: Vec<String> = Vec::new();
                    for raw_line in frame.lines() {
                        if raw_line.is_empty() || raw_line.starts_with(':') {
                            continue;
                        }
                        if let Some(rest) = raw_line.strip_prefix("event:") {
                            event_kind = Some(rest.trim().to_string());
                        } else if let Some(rest) = raw_line.strip_prefix("data:") {
                            // Per the SSE spec the leading single
                            // space after `data:` is significant — it
                            // is stripped, but additional leading
                            // whitespace is preserved.
                            data_parts.push(
                                rest.strip_prefix(' ').unwrap_or(rest).to_string(),
                            );
                        }
                    }
                    if data_parts.is_empty() {
                        continue;
                    }
                    let payload = data_parts.join("\n");
                    // The bridge sends `event: chunk` frames whose
                    // `data:` field is PLAIN TEXT (a slice of the
                    // reply), and `event: done` frames whose `data:`
                    // field is a JSON metadata blob. The pre-fix
                    // parser tried to JSON-parse every payload and
                    // silently dropped chunks when parse failed,
                    // which is why chat_stream yielded zero items.
                    match event_kind.as_deref() {
                        Some("done") => {
                            // Terminal frame. We don't surface the
                            // metadata as a stream item — callers
                            // see end-of-stream and read the trace
                            // via /v1/tasks endpoints if they care.
                            // `[DONE]` is the OpenAI-compat sentinel.
                            return;
                        }
                        Some("error") => {
                            yield Err(RelixError::Http {
                                status: 0,
                                body: payload,
                            });
                            return;
                        }
                        _ => {
                            // `event: chunk` (or no event field at
                            // all — defaults to "message"). Treat as
                            // raw text; the OpenAI shim sends JSON
                            // here, so we fall back to extracting a
                            // `chunk` / `text` field if the payload
                            // happens to be valid JSON. Otherwise we
                            // yield the literal text.
                            if payload == "[DONE]" {
                                return;
                            }
                            let text =
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&payload)
                                {
                                    v.get("chunk")
                                        .or_else(|| v.get("text"))
                                        .and_then(|x| x.as_str())
                                        .map(|s| s.to_string())
                                        .unwrap_or(payload)
                                } else {
                                    payload
                                };
                            if !text.is_empty() {
                                yield Ok(text);
                            }
                        }
                    }
                }
            }
        };
        Ok(Box::pin(s))
    }

    /// Persist a memory entry on behalf of the current tenant.
    /// Uses the bridge's `POST /v1/memory/embed` route. `tags`
    /// is an opaque list the bridge persists verbatim.
    pub async fn remember(&self, content: &str, tags: &[&str]) -> Result<(), RelixError> {
        let url = format!("{}/v1/memory/embed", self.base_url);
        let body = serde_json::json!({
            "subject_id": format!("tenant:{}", self.tenant),
            "target": "agent",
            "chunk": content,
            "tags": tags,
        });
        let r = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-relix-tenant", &self.tenant)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            return Err(RelixError::Http {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    /// Search the tenant's memory via the bridge's
    /// `POST /v1/memory/search` route. Returns up to `top_k`
    /// results sorted by score descending.
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryResult>, RelixError> {
        let url = format!("{}/v1/memory/search", self.base_url);
        let body = serde_json::json!({
            "subject_id": format!("tenant:{}", self.tenant),
            "target": "agent",
            "query": query,
            "top_k": 10,
        });
        let r = self
            .http
            .post(&url)
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-relix-tenant", &self.tenant)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        let status = r.status();
        let text = r
            .text()
            .await
            .map_err(|e| RelixError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(RelixError::Http {
                status: status.as_u16(),
                body: text,
            });
        }
        // The bridge wraps results in `{ "hits": [...] }` or
        // returns the array directly — accept either.
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| RelixError::Decode(e.to_string()))?;
        let arr = v
            .get("hits")
            .and_then(|h| h.as_array())
            .or_else(|| v.as_array())
            .ok_or_else(|| {
                RelixError::Decode(format!("search response had no hits array: {text}"))
            })?;
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let id = entry
                .get("id")
                .or_else(|| entry.get("embedding_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = entry
                .get("content")
                .or_else(|| entry.get("chunk_text"))
                .or_else(|| entry.get("text"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tags: Vec<String> = entry
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let score = entry
                .get("score")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32);
            out.push(MemoryResult {
                id,
                content,
                tags,
                score,
            });
        }
        Ok(out)
    }
}

/// Generate a session id rooted in the tenant and the current
/// time. The tenant prefix keeps tenants from colliding. A
/// process-global atomic counter is appended so two calls from
/// one tenant never collide even when the platform clock is too
/// coarse to advance between them (observed on macOS, where the
/// timestamp alone could repeat across rapid consecutive calls).
fn new_session_id(tenant: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("sdk-{tenant}-{now:x}-{seq:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_explicit_token_and_default_tenant() {
        let c = RelixClient::new("http://127.0.0.1:19791/", "tok");
        // Trailing slash trimmed.
        assert_eq!(c.base_url(), "http://127.0.0.1:19791");
        assert_eq!(c.tenant(), DEFAULT_TENANT);
    }

    #[test]
    fn with_tenant_overrides_default() {
        let c = RelixClient::new("http://x", "t").with_tenant("acme");
        assert_eq!(c.tenant(), "acme");
    }

    #[test]
    fn session_id_includes_tenant_prefix() {
        let s = new_session_id("acme");
        assert!(s.starts_with("sdk-acme-"));
        // Different invocations should produce different ids.
        let s2 = new_session_id("acme");
        assert_ne!(s, s2);
    }

    #[test]
    fn session_ids_are_unique_across_rapid_calls() {
        // Tight loop with no delay: on platforms with coarse clock
        // resolution the timestamp can repeat, so uniqueness must
        // come from the atomic counter suffix, not the clock.
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for _ in 0..10_000 {
            assert!(
                seen.insert(new_session_id("acme")),
                "session id collision across rapid calls"
            );
        }
    }

    #[test]
    fn relix_info_round_trips_json() {
        let info = RelixInfo {
            system: "relix".into(),
            version: "0.1.5".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            capabilities: vec!["chat".into(), "streaming".into()],
        };
        let j = serde_json::to_string(&info).unwrap();
        let back: RelixInfo = serde_json::from_str(&j).unwrap();
        assert_eq!(back.system, "relix");
        assert_eq!(back.provider, "openai");
        assert_eq!(back.capabilities.len(), 2);
    }

    #[test]
    fn memory_result_round_trips_json() {
        let m = MemoryResult {
            id: "m1".into(),
            content: "hello".into(),
            tags: vec!["alpha".into()],
            score: Some(0.9),
        };
        let j = serde_json::to_string(&m).unwrap();
        let back: MemoryResult = serde_json::from_str(&j).unwrap();
        assert_eq!(back.id, "m1");
        assert_eq!(back.tags, vec!["alpha".to_string()]);
        assert_eq!(back.score, Some(0.9));
    }

    /// Reusable in-process HTTP server: accepts one TCP
    /// connection, reads the request, writes the supplied
    /// response bytes verbatim, and returns the raw request
    /// text so the caller can assert on headers + body.
    async fn one_shot_server(response_bytes: Vec<u8>) -> (u16, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            sock.write_all(&response_bytes).await.unwrap();
            sock.shutdown().await.ok();
            req
        });
        (port, handle)
    }

    /// PART 1 — real-server test that chat() round-trips the
    /// bridge's documented `{ "reply": "...", ... }` body shape.
    /// Uses a one-shot tokio TCP listener (no httpmock / wiremock
    /// dependency) so the test exercises the real reqwest +
    /// serde_json path end-to-end.
    #[tokio::test]
    async fn chat_returns_chat_response_with_non_empty_text_from_real_server() {
        let body = r#"{"reply":"hello from the bridge","flow_id":"f1","trace_id":"t1","flow_log":"/tmp/log","task_id":"task-1"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        )
        .into_bytes();
        let (port, handle) = one_shot_server(response).await;
        let c = RelixClient::new(&format!("http://127.0.0.1:{port}"), "tok");
        let reply = c.chat("hi there").await.expect("chat");
        assert_eq!(reply, "hello from the bridge");
        let req = handle.await.unwrap();
        // The SDK posts to /chat and sends the JSON envelope the
        // bridge expects.
        assert!(req.starts_with("POST /chat "), "request line wrong: {req}");
        assert!(req.contains(r#""message":"hi there""#));
        assert!(req.contains(r#""session_id""#));
    }

    /// Phase 2 — chat_full surfaces the durable task binding the
    /// bridge created so SDK callers can follow / audit the run.
    #[tokio::test]
    async fn chat_full_surfaces_task_and_workspace_binding() {
        let body = r#"{"reply":"hi","flow_id":"f1","trace_id":"t1","flow_log":"/tmp/log","task_id":"task-42","workspace_lease_id":"lease-7","workspace_path":"/work/acme"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        )
        .into_bytes();
        let (port, handle) = one_shot_server(response).await;
        let c = RelixClient::new(&format!("http://127.0.0.1:{port}"), "tok");
        let reply = c
            .chat_full_in_workspace("hi there", Some("lease-7"))
            .await
            .expect("chat_full");
        assert_eq!(reply.reply, "hi");
        assert_eq!(reply.task_id.as_deref(), Some("task-42"));
        assert_eq!(reply.workspace_lease_id.as_deref(), Some("lease-7"));
        assert_eq!(reply.workspace_path.as_deref(), Some("/work/acme"));
        assert_eq!(reply.flow_id.as_deref(), Some("f1"));
        let req = handle.await.unwrap();
        // The workspace lease id rides the request body so the bridge
        // can resolve + bind it.
        assert!(
            req.contains(r#""workspace_lease_id":"lease-7""#),
            "lease id not sent: {req}"
        );
    }

    /// A bridge with no coordinator wired returns no task_id; the
    /// binding fields decode as None rather than erroring.
    #[tokio::test]
    async fn chat_full_tolerates_absent_binding() {
        let body = r#"{"reply":"hi","flow_id":"f1","trace_id":"t1","flow_log":"/tmp/log"}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body,
        )
        .into_bytes();
        let (port, handle) = one_shot_server(response).await;
        let c = RelixClient::new(&format!("http://127.0.0.1:{port}"), "tok");
        let reply = c.chat_full("hi").await.expect("chat_full");
        assert_eq!(reply.reply, "hi");
        assert!(reply.task_id.is_none());
        assert!(reply.workspace_lease_id.is_none());
        let req = handle.await.unwrap();
        // No lease id requested → not present in the body.
        assert!(
            !req.contains("workspace_lease_id"),
            "unexpected lease: {req}"
        );
    }

    /// PART 1 — real-server test that chat_stream() yields at
    /// least one chunk with non-empty text against the actual
    /// SSE wire shape the bridge emits:
    ///
    ///   event: chunk\n
    ///   data: <raw text>\n\n
    ///   ...
    ///   event: done\n
    ///   data: {json metadata}\n\n
    ///
    /// The pre-fix parser tried to JSON-parse every `data:` payload
    /// and silently dropped chunks when the parse failed — so the
    /// stream yielded zero items against the real bridge. This
    /// test would have caught that bug.
    #[tokio::test]
    async fn chat_stream_yields_at_least_one_chunk_with_non_empty_text_from_real_server() {
        use futures::StreamExt;
        // The exact wire shape `crate::sse::build_chunked_sse`
        // produces. We hand-craft the bytes here to be sure the
        // SDK sees the bridge's literal output, not a JSON-wrapped
        // fixture that masks the bug.
        let sse_body = "event: chunk\r\n\
                        data: Hello \r\n\
                        \r\n\
                        event: chunk\r\n\
                        data: world\r\n\
                        \r\n\
                        event: chunk\r\n\
                        data: !\r\n\
                        \r\n\
                        event: done\r\n\
                        data: {\"flow_id\":\"f1\",\"trace_id\":\"t1\",\"flow_log\":\"/tmp/x\",\"task_id\":null}\r\n\
                        \r\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            sse_body.len(),
            sse_body,
        )
        .into_bytes();
        let (port, handle) = one_shot_server(response).await;
        let c = RelixClient::new(&format!("http://127.0.0.1:{port}"), "tok");
        let stream = c.chat_stream("hi").await.expect("chat_stream");
        let mut stream = std::pin::pin!(stream);
        let mut chunks: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("stream item"));
        }
        assert!(
            !chunks.is_empty(),
            "chat_stream yielded zero chunks; SSE parser is broken"
        );
        let first_non_empty = chunks
            .iter()
            .find(|c| !c.is_empty())
            .cloned()
            .unwrap_or_default();
        assert!(
            !first_non_empty.is_empty(),
            "every yielded chunk was empty; SSE parser regressed",
        );
        assert_eq!(
            chunks.concat(),
            "Hello world!",
            "chunks concatenate to the full reply"
        );
        let req = handle.await.unwrap();
        assert!(
            req.starts_with("POST /chat/stream "),
            "request line wrong: {req}"
        );
        assert!(
            req.to_lowercase().contains("accept: text/event-stream"),
            "missing accept header: {req}"
        );
    }

    /// End-to-end test against an in-process one-shot server
    /// that mimics the bridge's `/v1/info` shape. Verifies the
    /// client sends the documented headers and decodes the
    /// response correctly.
    #[tokio::test]
    async fn info_round_trips_against_a_one_shot_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = r#"{"system":"relix","version":"0.1.5","provider":"mock","model":"relix-mock","capabilities":["chat"]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            req
        });
        let c = RelixClient::new(&format!("http://127.0.0.1:{port}"), "tok-xyz");
        let info = c.info().await.expect("info");
        assert_eq!(info.system, "relix");
        assert_eq!(info.version, "0.1.5");
        assert_eq!(info.provider, "mock");
        let req = server.await.unwrap();
        // Documented headers must ride every call.
        assert!(
            req.to_lowercase().contains("authorization: bearer tok-xyz"),
            "missing auth header"
        );
        assert!(
            req.to_lowercase().contains("x-relix-tenant: default"),
            "missing tenant header"
        );
    }
}
