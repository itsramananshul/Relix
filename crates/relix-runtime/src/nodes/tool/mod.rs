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

use std::net::{IpAddr, SocketAddr};
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

/// Tool backend. Holds the per-node config and rebuilds a small reqwest
/// client *per request* so each fetch can pin its hostname to the IPs the
/// SSRF guard validated. The per-request build is the price for closing the
/// DNS-rebind window between `security::resolve_safe_url` and the connect.
pub struct ToolBackend {
    cfg: ToolConfig,
}

impl ToolBackend {
    /// Build the backend. Probes a client up-front so any TLS / config
    /// problem (e.g. an unusable root store) surfaces at startup, not on the
    /// first request.
    pub fn new(cfg: ToolConfig) -> Result<Self, ToolError> {
        let _probe = Self::build_client(&cfg, None)
            .map_err(|e| ToolError::Build(format!("client probe: {e}")))?;
        Ok(Self { cfg })
    }

    /// Construct a reqwest client with the configured deadlines / redirect
    /// cap, optionally pinning a hostname to a specific set of socket
    /// addresses (the M9 DNS-pinning lever). When `pin` is `None` the
    /// resulting client behaves like the default OS resolver — used only
    /// when the URL host is already a literal IP (so there's nothing to
    /// resolve in the first place) or for the startup probe.
    fn build_client(
        cfg: &ToolConfig,
        pin: Option<(&str, &[SocketAddr])>,
    ) -> Result<reqwest::Client, reqwest::Error> {
        let mut b = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.timeout_secs.min(10)))
            .redirect(reqwest::redirect::Policy::limited(cfg.max_redirects));
        if let Some((host, addrs)) = pin {
            b = b.resolve_to_addrs(host, addrs);
        }
        b.build()
    }

    /// Run the configured capability against a single URL.
    ///
    /// Order of operations matters for safety:
    ///
    /// 1. **Validate** scheme & host with `security::resolve_safe_url`. The
    ///    resolver returns every IP DNS gave us and rejects if *any* of
    ///    them is in a forbidden range (no DNS-rebind "pick the safe one"
    ///    smuggling).
    /// 2. **Pin** the request's hostname to those validated IPs via
    ///    `reqwest::ClientBuilder::resolve_to_addrs`. The reqwest client
    ///    bypasses its built-in resolver, so the TCP connect targets the
    ///    inspected address even if the upstream resolver subsequently
    ///    returns something else. The URL still contains the hostname →
    ///    `Host` header and TLS SNI keep pointing at the original origin.
    /// 3. **Stream** the body into a bounded buffer; abort if the response
    ///    exceeds the cap. `content-type` is filtered to text-like.
    ///
    /// Per-hop redirect re-validation is a separate concern (tracked
    /// in `docs/tool-node-security.md`); the redirect policy here caps
    /// the *number* of follows, not their targets.
    pub async fn fetch(&self, raw_url: &str, max_bytes_request: usize) -> WebFetchOutcome {
        let cap = max_bytes_request.min(self.cfg.max_bytes).max(1);

        let target = match resolve_safe_url(raw_url, self.cfg.allow_http).await {
            Ok(t) => t,
            Err(e) => return WebFetchOutcome::Rejected(e),
        };

        // M9 DNS pinning: pre-compute the SocketAddrs we will allow the
        // hostname to resolve to. For an IP-literal URL there is nothing to
        // pin (reqwest does not run the resolver in that case).
        let host_str = target
            .normalized_url
            .host_str()
            .expect("resolve_safe_url guarantees a host")
            .to_string();
        let port = target.normalized_url.port_or_known_default().unwrap_or(
            if target.normalized_url.scheme() == "https" {
                443
            } else {
                80
            },
        );
        let pinned_addrs: Vec<SocketAddr> = target
            .resolved
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        let is_ip_literal = host_str.parse::<IpAddr>().is_ok();

        let pin: Option<(&str, &[SocketAddr])> = if is_ip_literal {
            None
        } else {
            Some((host_str.as_str(), pinned_addrs.as_slice()))
        };
        let client = match Self::build_client(&self.cfg, pin) {
            Ok(c) => c,
            Err(e) => {
                return WebFetchOutcome::Transport(format!("client build with pin: {e}"));
            }
        };

        let url = target.normalized_url.clone();
        let resp = match client.get(url).send().await {
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

    // ──────────────────── DNS pinning live tests ──────────────────────────
    //
    // Strategy: bring up a tiny axum HTTP server on a random loopback port,
    // then exercise `build_client` with a synthetic hostname that has no
    // real DNS. If pinning works, reqwest connects to the loopback server
    // (we get our test body back); if it didn't, reqwest's resolver would
    // fail with NXDOMAIN. The test proves the post-validation connect goes
    // to the pinned address, not whatever the resolver returns at request
    // time — i.e. defeats DNS rebinding between guard and connect.

    /// Spawn a one-shot axum server returning a fixed body. Returns the
    /// bound `SocketAddr`. Drops with the test scope.
    async fn spawn_loopback_server(body: &'static str) -> SocketAddr {
        use axum::{Router, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(move || async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        addr
    }

    #[tokio::test]
    async fn pin_forces_connect_to_validated_ip_not_dns() {
        // Bring up loopback server.
        let addr = spawn_loopback_server("pin-works\n").await;

        // Build a client pinned to a hostname that almost certainly does
        // NOT resolve via the system resolver (`.invalid` is RFC 2606
        // reserved). If pinning didn't work, reqwest would fail with
        // NXDOMAIN. If it does, reqwest connects to 127.0.0.1:addr.port.
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let pin: &[SocketAddr] = &[addr];
        let client =
            ToolBackend::build_client(&cfg, Some(("rebind.invalid", pin))).expect("client builds");
        let url = format!("http://rebind.invalid:{}/", addr.port());
        let resp = client.get(&url).send().await.expect("connect via pin");
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert_eq!(body, "pin-works\n");
    }

    #[tokio::test]
    async fn pin_to_one_ip_ignores_other_addresses_in_dns() {
        // This test simulates a rebinding-style trick: the URL hostname
        // *could* in principle resolve to both a loopback (forbidden) and
        // a public IP at request time. We pin to ONLY the validated
        // (public) IP and confirm reqwest connects there. We use a second
        // loopback server as the "validated" target to keep the test
        // hermetic, and supply an unrelated forbidden-looking address in
        // the URL host (which wouldn't normally resolve at all).
        let validated_addr = spawn_loopback_server("validated-host\n").await;
        let _decoy_addr = spawn_loopback_server("decoy-NEVER-SHOULD-SEE\n").await;

        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        // Pin maps "example.invalid" *only* to the validated socket. Even
        // if a later resolver returned the decoy or a true rebind IP,
        // reqwest will only use entries in this pin list.
        let pin: &[SocketAddr] = &[validated_addr];
        let client =
            ToolBackend::build_client(&cfg, Some(("example.invalid", pin))).expect("client builds");
        let url = format!("http://example.invalid:{}/", validated_addr.port());
        let body = client
            .get(&url)
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "validated-host\n");
    }

    #[tokio::test]
    async fn unpinned_hostname_fails_dns_proving_pin_is_load_bearing() {
        // Sanity test: without a pin, the same `.invalid` hostname fails.
        // If this ever started succeeding it would mean either (a) the
        // test environment poisoned its DNS, or (b) reqwest grew an
        // implicit fallback — either way our pin assumption needs to be
        // re-examined.
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let client = ToolBackend::build_client(&cfg, None).expect("client builds");
        let url = "http://rebind-control.invalid:9/";
        let r = client.get(url).send().await;
        assert!(
            r.is_err(),
            "expected DNS failure without pin (got success — pin test is meaningless)"
        );
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
