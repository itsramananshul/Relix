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
    pub async fn chat(&self, prompt: &str) -> Result<String, RelixError> {
        let url = format!("{}/chat", self.base_url);
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
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| RelixError::Decode(e.to_string()))?;
        // `POST /chat` returns `{ "reply": "...", ... }`. Be
        // forgiving about additional fields the bridge may add.
        let reply = v
            .get("reply")
            .and_then(|s| s.as_str())
            .ok_or_else(|| RelixError::Decode(format!("no `reply` in response: {text}")))?;
        Ok(reply.to_string())
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
                while let Some(end) = buf.find("\n\n") {
                    let frame = buf[..end].to_string();
                    buf.drain(..end + 2);
                    for line in frame.lines() {
                        if let Some(payload) = line.strip_prefix("data:") {
                            let payload = payload.trim();
                            if payload.is_empty() || payload == "[DONE]" {
                                continue;
                            }
                            // The bridge emits SSE frames with a JSON object
                            // carrying `chunk` (or `text`) keys; be lenient
                            // about which field name the payload uses.
                            match serde_json::from_str::<serde_json::Value>(payload) {
                                Ok(v) => {
                                    let txt = v
                                        .get("chunk")
                                        .or_else(|| v.get("text"))
                                        .and_then(|x| x.as_str())
                                        .map(|s| s.to_string());
                                    if let Some(t) = txt
                                        && !t.is_empty()
                                    {
                                        yield Ok(t);
                                    }
                                }
                                Err(_) => continue,
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

/// Generate a deterministic-ish session id rooted in the tenant
/// and the current time. Tenants don't collide because the
/// tenant prefix participates in the hash; multiple concurrent
/// calls from one tenant get different ids because the
/// timestamp and nanos differ.
fn new_session_id(tenant: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sdk-{tenant}-{now:x}")
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
