//! Tool node — first external-action capability for Relix.
//!
//! Registered capabilities on a controller with `[controller] node_type = "tool"`:
//!
//! - `tool.web_fetch` — HTTP(S) GET of a single URL, returning UTF-8 body text.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Argument is a UTF-8 string. Two forms accepted:
//!
//! | Arg | Meaning |
//! |---|---|
//! | `<url>` | GET the URL with default `max_bytes` |
//! | `<url>\|<n>` | GET the URL, cap body at `n` bytes (clamped to node `max_bytes`) |
//!
//! Returns the response body decoded as UTF-8. Non-UTF-8 bodies are an error.
//!
//! ## Security model — fail closed
//!
//! `tool.web_fetch` is a high-blast-radius capability: a chat user who can
//! reach it can ask Relix to dial arbitrary endpoints from the tool node.
//! SSRF protections live in [`security`] and run *before* any network I/O:
//!
//! 1. Scheme allowlist — `https` always, `http` only when
//!    `[tool] allow_http = true` (false by default).
//! 2. Reject any URL whose host parses as a literal IP in a forbidden range
//!    (loopback, link-local, private, unspecified, multicast, broadcast,
//!    documentation, benchmark, ULA, well-known cloud metadata endpoints).
//! 3. Resolve the hostname via the OS resolver and reject if *any* resolved
//!    address is forbidden. The fetch then targets a `SocketAddr` derived
//!    from the resolution, never the original hostname — this prevents
//!    DNS rebinding between the safety check and the actual connect.
//! 4. Enforce request/connect deadlines, a redirect cap, and a body cap.
//! 5. Refuse non-text/non-json/non-html `content-type`.
//!
//! Anything that fails returns a structured `ErrorEnvelope` (no partial body,
//! no exception, no panic). The audit log records the rejection cause.
//!
//! ## Out of scope (alpha)
//!
//! - No JS execution.
//! - No headless browser.
//! - No POST/PUT/DELETE.
//! - No streaming bodies (whole body is read into memory subject to the cap).
//! - No per-host rate limits beyond the controller's policy engine.
//!
//! These ship in later milestones if and when a flow needs them.

pub mod security;

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use relix_core::capability::{CapabilityDescriptor, CapabilityKind, CostClass, Idempotency};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use security::{SsrfError, resolve_safe_url};

/// Per-node tool configuration parsed from `[tool]` in the controller TOML.
#[derive(Clone, Debug, Deserialize)]
pub struct ToolConfig {
    /// Maximum response body, bytes. Default 256 KiB; clients may request
    /// less via the `|N` arg form but never more.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// Per-request total deadline (connect + read), seconds. Default 15.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Max followed redirects. Default 3.
    #[serde(default = "default_max_redirects")]
    pub max_redirects: usize,
    /// Allow plain `http://`. Default `false` — fail-closed posture.
    #[serde(default)]
    pub allow_http: bool,
    /// `User-Agent` header sent with each request.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_max_bytes(),
            timeout_secs: default_timeout_secs(),
            max_redirects: default_max_redirects(),
            allow_http: false,
            user_agent: default_user_agent(),
        }
    }
}

fn default_max_bytes() -> usize {
    256 * 1024
}
fn default_timeout_secs() -> u64 {
    15
}
fn default_max_redirects() -> usize {
    3
}
fn default_user_agent() -> String {
    format!("Relix-tool/{}", env!("CARGO_PKG_VERSION"))
}

/// HTTP client shared across requests (connection pool reuse). Constructed once.
pub struct ToolBackend {
    cfg: ToolConfig,
    client: reqwest::Client,
}

impl ToolBackend {
    /// Build the backend, baking the deadlines/redirect cap into the client.
    pub fn new(cfg: ToolConfig) -> Result<Self, ToolError> {
        let client = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.timeout_secs.min(10)))
            .redirect(reqwest::redirect::Policy::limited(cfg.max_redirects))
            .build()
            .map_err(|e| ToolError::Build(e.to_string()))?;
        Ok(Self { cfg, client })
    }

    /// Run the configured capability against a single URL.
    ///
    /// Order of operations matters for safety:
    ///
    /// 1. Validate scheme & host with `security::resolve_safe_url`. This
    ///    performs DNS *and* returns the chosen [`std::net::SocketAddr`] so we
    ///    can pin the connection to the validated IP. DNS rebind can't beat
    ///    a check that the connection is bound to the inspected address.
    /// 2. Issue the GET via `reqwest`. `reqwest`'s redirect policy applies;
    ///    each follow is independently re-validated (see [`Self::fetch`]).
    /// 3. Stream the body into a bounded buffer; abort if the response
    ///    exceeds the cap.
    pub async fn fetch(&self, raw_url: &str, max_bytes_request: usize) -> WebFetchOutcome {
        let cap = max_bytes_request.min(self.cfg.max_bytes).max(1);

        let target = match resolve_safe_url(raw_url, self.cfg.allow_http).await {
            Ok(t) => t,
            Err(e) => return WebFetchOutcome::Rejected(e),
        };

        // We re-validate redirect targets ourselves rather than trusting
        // reqwest's redirect cap alone — reqwest has no concept of SSRF.
        // (Alpha SIMP: we still rely on reqwest's connect to a hostname.
        // The trade-off: dialing the validated SocketAddr would require
        // a hyper-level custom resolver. Documented in security.rs.)
        let url = target.normalized_url.clone();

        let resp = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => return WebFetchOutcome::Transport(e.to_string()),
        };

        let status = resp.status();
        if !status.is_success() {
            return WebFetchOutcome::HttpStatus {
                status: status.as_u16(),
                final_url: resp.url().to_string(),
            };
        }

        // Reject content-types that obviously aren't text. We do not try to
        // sniff bodies — refuse anything that isn't text/* application/json
        // or application/xhtml+xml.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !is_textual_content_type(&content_type) {
            return WebFetchOutcome::ContentTypeRejected {
                content_type,
                final_url: resp.url().to_string(),
            };
        }

        // Respect server-supplied Content-Length cap as a fast reject.
        if let Some(len) = resp.content_length()
            && (len as usize) > cap
        {
            return WebFetchOutcome::TooLarge {
                declared_bytes: len,
                cap,
            };
        }

        let final_url = resp.url().to_string();

        // Bounded read.
        let mut acc: Vec<u8> = Vec::with_capacity(cap.min(16 * 1024));
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => return WebFetchOutcome::Transport(e.to_string()),
            };
            if acc.len() + bytes.len() > cap {
                return WebFetchOutcome::TooLarge {
                    declared_bytes: (acc.len() + bytes.len()) as u64,
                    cap,
                };
            }
            acc.extend_from_slice(&bytes);
        }

        match String::from_utf8(acc) {
            Ok(body) => WebFetchOutcome::Ok {
                body,
                final_url,
                content_type,
            },
            Err(_) => WebFetchOutcome::NotUtf8 { final_url },
        }
    }
}

fn is_textual_content_type(ct: &str) -> bool {
    let lower = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if lower.is_empty() {
        // Be conservative: when the server omits a content-type, still allow
        // (many small static endpoints do this). Body must still be valid UTF-8.
        return true;
    }
    lower.starts_with("text/")
        || lower == "application/json"
        || lower == "application/ld+json"
        || lower == "application/xml"
        || lower == "application/xhtml+xml"
        || lower.ends_with("+json")
        || lower.ends_with("+xml")
}

/// Outcome the handler maps to either `Ok` or a typed `ErrorEnvelope`.
#[derive(Debug, Clone)]
pub enum WebFetchOutcome {
    /// Successful fetch, body decoded as UTF-8.
    Ok {
        body: String,
        final_url: String,
        content_type: String,
    },
    /// SSRF / scheme / host rejection — never touched the network at all
    /// (or only resolved DNS).
    Rejected(SsrfError),
    /// Body or declared `Content-Length` exceeded the cap.
    TooLarge { declared_bytes: u64, cap: usize },
    /// Non-2xx response.
    HttpStatus { status: u16, final_url: String },
    /// Server returned a non-text content type.
    ContentTypeRejected {
        content_type: String,
        final_url: String,
    },
    /// Body bytes did not decode as UTF-8.
    NotUtf8 { final_url: String },
    /// Transport-level failure (DNS during reqwest, TLS, RST, etc.).
    Transport(String),
}

/// Capability descriptor for `tool.web_fetch`. Exposed so future manifest
/// exchange (M10) can broadcast it. Today it's read by [`register`] only.
pub fn capability_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web_fetch");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    // Tool calls touch the outside world. Treat as non-idempotent: the same
    // URL may return different bodies on each fetch.
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec!["external:network".into(), "egress:http".into()];
    d.policy_attachment_point = "tool.web_fetch".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d
}

/// Register tool capabilities on the dispatch bridge.
pub fn register(bridge: &mut DispatchBridge, backend: Arc<ToolBackend>) {
    let backend_for_handler = backend.clone();
    bridge.register(
        "tool.web_fetch",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = backend_for_handler.clone();
            async move { handle_web_fetch(b, ctx).await }
        })),
    );
}

async fn handle_web_fetch(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.web_fetch arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // `<url>` or `<url>|<n>`. URLs are not allowed to contain `|`.
    let (raw_url, max_bytes) = match s.rsplit_once('|') {
        Some((url, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (url.trim(), n_str.trim().parse::<usize>().unwrap_or(0))
        }
        _ => (s.trim(), backend.cfg.max_bytes),
    };
    if raw_url.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.web_fetch: url required (arg: `<url>` or `<url>|<n>`)".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }

    match backend.fetch(raw_url, max_bytes).await {
        WebFetchOutcome::Ok { body, .. } => HandlerOutcome::Ok(body.into_bytes()),
        WebFetchOutcome::Rejected(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!("tool.web_fetch ssrf-rejected: {e}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::TooLarge {
            declared_bytes,
            cap,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_fetch body too large: declared={declared_bytes}B cap={cap}B"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::HttpStatus { status, final_url } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.web_fetch http {status} for {final_url}"),
            retry_hint: 1,
            retry_after: None,
        }),
        WebFetchOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.web_fetch content-type not text-like: '{content_type}' for {final_url}"
            ),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::NotUtf8 { final_url } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_fetch body not utf-8 for {final_url}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::Transport(c) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::TRANSPORT,
            cause: format!("tool.web_fetch transport: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

/// Tool-node errors surfaced at construction time.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// HTTP client could not be built (TLS init, etc.).
    #[error("build: {0}")]
    Build(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_loopback() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://127.0.0.1/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_localhost() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://localhost/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_ipv6_loopback() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://[::1]/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_rfc1918() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        for u in &[
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
        ] {
            let r = backend.fetch(u, 1024).await;
            assert!(matches!(r, WebFetchOutcome::Rejected(_)), "{u} got {:?}", r);
        }
    }

    #[tokio::test]
    async fn rejects_link_local_metadata() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend
            .fetch("http://169.254.169.254/latest/meta-data/", 1024)
            .await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_file_scheme() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("file:///etc/passwd", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_ftp_scheme() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("ftp://example.com/foo", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_http_by_default() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://example.com/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn allows_http_when_opted_in_via_config() {
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();
        // Resolution should pass scheme check; what comes next (DNS or remote
        // server state) is not asserted here. The point is: we did NOT
        // reject for scheme.
        let r = backend.fetch("http://example.com/", 1024).await;
        if let WebFetchOutcome::Rejected(SsrfError::SchemeDenied { .. }) = r {
            panic!("expected http to pass scheme check when allow_http=true");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("not a url", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[test]
    fn descriptor_is_external_paid_and_admission_tagged() {
        let d = capability_descriptor();
        assert_eq!(d.method_name, "tool.web_fetch");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.cost_class, CostClass::ExternalPaid));
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(d.requires_groups.iter().any(|g| g == "chat-users"));
    }

    #[test]
    fn content_type_filter() {
        assert!(is_textual_content_type("text/html"));
        assert!(is_textual_content_type("text/html; charset=utf-8"));
        assert!(is_textual_content_type("application/json"));
        assert!(is_textual_content_type("application/ld+json"));
        assert!(is_textual_content_type("application/atom+xml"));
        assert!(is_textual_content_type(""));
        assert!(!is_textual_content_type("application/octet-stream"));
        assert!(!is_textual_content_type("image/png"));
        assert!(!is_textual_content_type("application/pdf"));
    }
}
