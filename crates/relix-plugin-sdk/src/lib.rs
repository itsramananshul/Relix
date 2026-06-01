//! Plugin author SDK for Relix's `relix-plugin-v1` protocol.
//!
//! Plugin authors depend on this crate, register their capability
//! handlers, and call [`PluginServer::serve`]. The SDK:
//!
//! 1. Reads the loopback-TLS certificate + key the host passes in
//!    via [`PLUGIN_TLS_CERT_ENV`] / [`PLUGIN_TLS_KEY_ENV`]
//!    (base64-DER), and the per-plugin bearer via
//!    [`PLUGIN_BEARER_ENV`]. If the TLS material or the bearer is
//!    absent, `serve()` FAILS CLOSED — it refuses to start rather
//!    than fall back to a plaintext listener the hardened host can
//!    no longer talk to anyway.
//! 2. Binds a TCP listener on `127.0.0.1:0` (kernel picks a free
//!    port), wraps every accepted connection in TLS using exactly
//!    the host-provided cert/key, and writes
//!    `RELIX_PLUGIN_PORT=<port>` to stdout so the host loader can
//!    find it.
//! 3. Speaks the host dispatcher's framing: newline-delimited JSON,
//!    one request frame per line, one response frame per line, over
//!    the TLS stream.
//!
//! ## Wire shape (SEC §11 / §11b)
//!
//! Request frame (one JSON object + `\n`), an `op`-tagged enum:
//!
//! ```json
//! {"op":"health"}
//! {"op":"invoke","bearer":"<hex>","request":{ "method": "...", "args": "...",
//!   "trace_id":"...", "request_id":"...", "caller_subject_id":"...", "deadline_unix":0 }}
//! ```
//!
//! Response frame (one JSON object + `\n`):
//!
//! ```json
//! {"ok":true}                                  // health
//! {"ok":true,"body":"<response string>"}       // invoke success
//! {"ok":false,"error_kind":<u32>,"error_cause":"<msg>"}  // invoke error / unauthorized
//! ```
//!
//! Where `error_kind` mirrors `relix_core::types::error_kinds::*`
//! (`INVALID_ARGS = 5`, `RESPONDER_INTERNAL = 11`, …), plus
//! [`error_kind::UNAUTHORIZED`] for a missing/wrong bearer.
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
//! server.serve().await?; // reads TLS cert/key + bearer from env
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

/// SEC PART 2: env var the host loader sets when it spawns a
/// plugin. Read once at `serve()` time; every `invoke` frame is
/// gated on a matching `bearer`.
pub const PLUGIN_BEARER_ENV: &str = "RELIX_PLUGIN_BEARER";

/// SEC §11b: env vars the host loader sets carrying the
/// per-plugin self-signed TLS certificate + private key, each
/// base64-encoded DER. The plugin serves TLS with exactly these
/// bytes; the host dispatcher pins the same cert.
pub const PLUGIN_TLS_CERT_ENV: &str = "RELIX_PLUGIN_TLS_CERT_DER_B64";
pub const PLUGIN_TLS_KEY_ENV: &str = "RELIX_PLUGIN_TLS_KEY_DER_B64";

/// Stable error kinds plugins return through the protocol.
pub mod error_kind {
    /// Caller supplied bad args.
    pub const INVALID_ARGS: u32 = 5;
    /// Plugin-internal error.
    pub const RESPONDER_INTERNAL: u32 = 11;
    /// Plugin's backend is rate-limited / overloaded.
    pub const RESPONDER_OVERLOADED: u32 = 12;
    /// Plugin doesn't know the requested method.
    pub const UNKNOWN_METHOD: u32 = 4;
    /// SEC §11b: invoke frame carried a missing/wrong bearer. The
    /// host dispatcher surfaces this as an unauthorized transport
    /// rejection (HTTP-401-equivalent).
    pub const UNAUTHORIZED: u32 = 401;
}

/// One inbound invoke call after JSON decoding. Field-compatible
/// with the host dispatcher's `InvokeRequest`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InvokeRequest {
    /// Dotted method name (`my_plugin.do_thing`).
    pub method: String,
    /// Pipe-delimited UTF-8 args.
    pub args: String,
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub caller_subject_id: String,
    #[serde(default)]
    pub deadline_unix: i64,
}

/// One framed request on the wire. Mirrors the host dispatcher's
/// `WireRequest` so the JSON is byte-compatible.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum WireRequest {
    Health,
    Invoke {
        #[serde(default)]
        bearer: String,
        request: InvokeRequest,
    },
}

/// One framed response. Field-compatible with the host
/// dispatcher's `InvokeResponse` / `HealthResponse` (all fields
/// optional with serde defaults on the reader side).
#[derive(Debug, Serialize)]
struct WireResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_kind: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_cause: Option<String>,
}

impl WireResponse {
    fn health(ok: bool) -> Self {
        Self {
            ok,
            body: None,
            error_kind: None,
            error_cause: None,
        }
    }
    fn ok_body(body: String) -> Self {
        Self {
            ok: true,
            body: Some(body),
            error_kind: None,
            error_cause: None,
        }
    }
    fn error(kind: u32, cause: String) -> Self {
        Self {
            ok: false,
            body: None,
            error_kind: Some(kind),
            error_cause: Some(cause),
        }
    }
    fn to_line(&self) -> String {
        // The static struct shape always serialises; if it ever
        // fails we still emit a valid error frame so we never
        // panic mid-connection.
        serde_json::to_string(self).unwrap_or_else(|e| {
            format!(
                "{{\"ok\":false,\"error_kind\":{},\"error_cause\":\"response serialise failed: {e}\"}}",
                error_kind::RESPONDER_INTERNAL
            )
        })
    }
}

/// Plugin-side error.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PluginError {
    #[error("invalid args: {0}")]
    InvalidArgs(String),
    #[error("internal: {0}")]
    Internal(String),
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
    bind: String,
    port_sink: PortSink,
    ready: Arc<Mutex<bool>>,
    /// Test-only override for the expected bearer.
    expected_bearer_override: Option<String>,
    /// Test-only override for the TLS cert + key (DER). Production
    /// callers leave this `None` and let `serve()` read the env.
    tls_override: Option<(Vec<u8>, Vec<u8>)>,
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
            expected_bearer_override: None,
            tls_override: None,
        }
    }

    /// Test seam: supply the expected bearer directly instead of
    /// reading [`PLUGIN_BEARER_ENV`].
    pub fn with_bearer_for_test(mut self, token: impl Into<String>) -> Self {
        self.expected_bearer_override = Some(token.into());
        self
    }

    /// Test seam: supply the TLS cert + key (DER) directly instead
    /// of reading [`PLUGIN_TLS_CERT_ENV`] / [`PLUGIN_TLS_KEY_ENV`].
    pub fn with_tls_for_test(mut self, cert_der: Vec<u8>, key_der: Vec<u8>) -> Self {
        self.tls_override = Some((cert_der, key_der));
        self
    }

    /// Override the listen address (host:port). Production uses
    /// `127.0.0.1:0`.
    pub fn with_bind(mut self, bind: impl Into<String>) -> Self {
        self.bind = bind.into();
        self
    }

    /// Capture the announced port into a buffer instead of stdout.
    pub fn with_captured_port(mut self, sink: Arc<Mutex<Vec<u8>>>) -> Self {
        self.port_sink = PortSink::Captured(sink);
        self
    }

    /// Mark the plugin ready (health frames report `ok`).
    pub fn mark_ready(&self, ready: bool) {
        let r = self.ready.clone();
        tokio::spawn(async move {
            let mut g = r.lock().await;
            *g = ready;
        });
    }

    /// Start with health `ok=false` until [`PluginServer::mark_ready`].
    pub fn with_lazy_ready(mut self) -> Self {
        self.ready = Arc::new(Mutex::new(false));
        self
    }

    /// Register a capability handler.
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

    /// Resolve the TLS cert + key, failing closed when neither an
    /// override nor the env provides them. SEC §11b: there is NO
    /// plaintext fallback — a plugin that cannot serve TLS refuses
    /// to start.
    fn resolve_tls(&self) -> Result<(Vec<u8>, Vec<u8>), ServeError> {
        if let Some((c, k)) = &self.tls_override {
            return Ok((c.clone(), k.clone()));
        }
        let cert_b64 = std::env::var(PLUGIN_TLS_CERT_ENV)
            .ok()
            .filter(|s| !s.is_empty());
        let key_b64 = std::env::var(PLUGIN_TLS_KEY_ENV)
            .ok()
            .filter(|s| !s.is_empty());
        match (cert_b64, key_b64) {
            (Some(c), Some(k)) => {
                let cert = base64::engine::general_purpose::STANDARD
                    .decode(c)
                    .map_err(|e| {
                        ServeError::TlsConfig(format!("{PLUGIN_TLS_CERT_ENV} base64: {e}"))
                    })?;
                let key = base64::engine::general_purpose::STANDARD
                    .decode(k)
                    .map_err(|e| {
                        ServeError::TlsConfig(format!("{PLUGIN_TLS_KEY_ENV} base64: {e}"))
                    })?;
                Ok((cert, key))
            }
            _ => Err(ServeError::TlsConfigMissing),
        }
    }

    /// Resolve the expected bearer, failing closed when absent.
    fn resolve_bearer(&self) -> Result<String, ServeError> {
        self.expected_bearer_override
            .clone()
            .or_else(|| std::env::var(PLUGIN_BEARER_ENV).ok())
            .filter(|s| !s.is_empty())
            .ok_or(ServeError::BearerMissing)
    }

    /// Bind the loopback-TLS listener, announce the port, and serve
    /// forever.
    ///
    /// SEC §11b: fails closed (returns `Err`) if the TLS cert/key
    /// or the bearer are not provided — never a plaintext fallback.
    pub async fn serve(self) -> Result<(), ServeError> {
        let (cert_der, key_der) = self.resolve_tls()?;
        let expected_bearer = self.resolve_bearer()?;
        let acceptor = build_tls_acceptor(cert_der, key_der)?;

        let listener = TcpListener::bind(&self.bind)
            .await
            .map_err(|e| ServeError::Bind(format!("{}: {e}", self.bind)))?;
        let local = listener
            .local_addr()
            .map_err(|e| ServeError::Bind(format!("local_addr: {e}")))?;
        announce_port(&self.port_sink, local.port()).await?;

        let state = Arc::new(ServerState {
            handlers: self.handlers,
            ready: self.ready,
            bearer: expected_bearer,
        });

        loop {
            let (tcp, _peer) = listener
                .accept()
                .await
                .map_err(|e| ServeError::Serve(format!("accept: {e}")))?;
            let acceptor = acceptor.clone();
            let state = state.clone();
            tokio::spawn(async move {
                match acceptor.accept(tcp).await {
                    Ok(tls) => {
                        if let Err(e) = serve_connection(tls, state).await {
                            tracing::debug!("plugin connection ended: {e}");
                        }
                    }
                    Err(e) => {
                        // A non-TLS / wrong-cert client fails here —
                        // it never reaches a handler. This is the
                        // hardening: plaintext probes are rejected.
                        tracing::debug!("plugin TLS handshake rejected: {e}");
                    }
                }
            });
        }
    }
}

struct ServerState {
    handlers: HashMap<String, HandlerFn>,
    ready: Arc<Mutex<bool>>,
    /// SEC §11b: every `invoke` frame must carry this bearer.
    bearer: String,
}

/// Build a rustls TLS acceptor from a DER cert + PKCS#8 key.
fn build_tls_acceptor(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<TlsAcceptor, ServeError> {
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let certs = vec![CertificateDer::from(cert_der)];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| ServeError::TlsConfig(format!("tls versions: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| ServeError::TlsConfig(format!("server cert: {e}")))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Serve newline-JSON frames over one (already TLS-wrapped)
/// connection until the peer closes it.
async fn serve_connection<S>(stream: S, state: Arc<ServerState>) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // peer closed
        }
        let resp = dispatch_frame(line.trim(), &state).await;
        write_half.write_all(resp.to_line().as_bytes()).await?;
        write_half.write_all(b"\n").await?;
        write_half.flush().await?;
    }
}

async fn dispatch_frame(line: &str, state: &ServerState) -> WireResponse {
    let frame: WireRequest = match serde_json::from_str(line) {
        Ok(f) => f,
        Err(e) => {
            return WireResponse::error(error_kind::INVALID_ARGS, format!("bad frame: {e}"));
        }
    };
    match frame {
        WireRequest::Health => {
            let ready = *state.ready.lock().await;
            WireResponse::health(ready)
        }
        WireRequest::Invoke { bearer, request } => {
            // SEC §11b: the OS-pinned TLS channel is the primary
            // boundary; the bearer is the secondary defense and
            // must still reject a caller without the right token.
            if bearer.is_empty() || !ct_eq(&bearer, &state.bearer) {
                return WireResponse::error(
                    error_kind::UNAUTHORIZED,
                    "invoke requires the per-plugin bearer matching RELIX_PLUGIN_BEARER"
                        .to_string(),
                );
            }
            let Some(handler) = state.handlers.get(&request.method).cloned() else {
                return WireResponse::error(
                    error_kind::UNKNOWN_METHOD,
                    format!("plugin has no handler for `{}`", request.method),
                );
            };
            let method = request.method.clone();
            match handler(request).await {
                Ok(body) => WireResponse::ok_body(body),
                Err(e) => WireResponse::error(e.kind(), format!("{method}: {e}")),
            }
        }
    }
}

/// Constant-time string compare so the SDK doesn't leak a token
/// prefix via per-byte short-circuit timing.
fn ct_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
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

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("bind: {0}")]
    Bind(String),
    #[error("serve: {0}")]
    Serve(String),
    /// SEC §11b: no TLS cert/key provided (env unset and no test
    /// override). The server refuses to start rather than fall
    /// back to plaintext.
    #[error(
        "loopback-TLS cert/key not provided ({PLUGIN_TLS_CERT_ENV} / {PLUGIN_TLS_KEY_ENV} unset); \
         refusing to start a plaintext plugin server"
    )]
    TlsConfigMissing,
    /// SEC §11b: TLS cert/key were provided but malformed.
    #[error("tls config: {0}")]
    TlsConfig(String),
    /// SEC §11b: no bearer provided ({PLUGIN_BEARER_ENV} unset and
    /// no test override). The server refuses to start unauthenticated.
    #[error("per-plugin bearer not provided ({PLUGIN_BEARER_ENV} unset); refusing to start")]
    BearerMissing,
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::plugin::{
        InvokeRequest as RtInvokeRequest, PluginDispatcher, PluginEndpoint,
    };

    /// Mint a self-signed cert + key (DER) bound to 127.0.0.1 —
    /// the shape the host loader produces and pins.
    fn loopback_cert() -> (Vec<u8>, Vec<u8>) {
        use rcgen::{CertificateParams, KeyPair, SanType};
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = vec![SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
        ))];
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        (cert.der().to_vec(), key_pair.serialize_der())
    }

    fn parse_announced_port(buf: &[u8]) -> u16 {
        let s = std::str::from_utf8(buf).unwrap();
        s.lines()
            .find_map(|l| l.strip_prefix("RELIX_PLUGIN_PORT="))
            .and_then(|n| n.trim().parse::<u16>().ok())
            .expect("port line")
    }

    /// Start a TLS plugin server in-process; return (port, cert_der)
    /// so a dispatcher can pin the cert and dial it.
    async fn start_tls(mut server: PluginServer, bearer: &str) -> (u16, Vec<u8>) {
        let (cert_der, key_der) = loopback_cert();
        let buf = Arc::new(Mutex::new(Vec::new()));
        server = server
            .with_tls_for_test(cert_der.clone(), key_der)
            .with_bearer_for_test(bearer)
            .with_captured_port(buf.clone());
        tokio::spawn(async move { server.serve().await.unwrap() });
        let mut port = 0u16;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let g = buf.lock().await;
            if !g.is_empty() {
                port = parse_announced_port(&g);
                break;
            }
        }
        assert_ne!(port, 0, "port not announced");
        (port, cert_der)
    }

    fn dispatcher(port: u16, cert: Vec<u8>, bearer: &str) -> PluginDispatcher {
        PluginDispatcher::connect(
            PluginEndpoint::new(format!("127.0.0.1:{port}"), cert),
            5,
            bearer.to_string(),
        )
    }

    fn req(method: &str, args: &str) -> RtInvokeRequest {
        RtInvokeRequest {
            method: method.to_string(),
            args: args.to_string(),
            trace_id: "t".to_string(),
            request_id: "r".to_string(),
            caller_subject_id: "alice".to_string(),
            deadline_unix: 0,
        }
    }

    async fn await_healthy(d: &PluginDispatcher) {
        for _ in 0..100 {
            if let Ok(true) = d.health().await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("server never became healthy");
    }

    #[tokio::test]
    async fn serve_fails_closed_without_tls_env() {
        // SEC §11b criterion 1: no TLS material → refuse to start.
        let server = PluginServer::new().with_bearer_for_test("tok");
        let err = server.serve().await.unwrap_err();
        assert!(matches!(err, ServeError::TlsConfigMissing), "got {err:?}");
    }

    #[tokio::test]
    async fn serve_fails_closed_without_bearer() {
        let (cert, key) = loopback_cert();
        let server = PluginServer::new().with_tls_for_test(cert, key);
        let err = server.serve().await.unwrap_err();
        assert!(matches!(err, ServeError::BearerMissing), "got {err:?}");
    }

    #[tokio::test]
    async fn health_returns_ok_over_tls() {
        let (port, cert) = start_tls(PluginServer::new(), "tok").await;
        let d = dispatcher(port, cert, "tok");
        assert!(matches!(d.health().await, Ok(true)));
    }

    #[tokio::test]
    async fn lazy_ready_health_false_until_marked() {
        let mut server = PluginServer::new().with_lazy_ready();
        let ready_handle = server.ready.clone();
        server.register("noop.touch", |_| async move { Ok(String::new()) });
        let (port, cert) = start_tls(server, "tok").await;
        let d = dispatcher(port, cert, "tok");
        assert!(matches!(d.health().await, Ok(false)));
        *ready_handle.lock().await = true;
        assert!(matches!(d.health().await, Ok(true)));
    }

    #[tokio::test]
    async fn invoke_routes_to_registered_handler() {
        let mut server = PluginServer::new();
        server.register("hello.greet", |req: InvokeRequest| async move {
            Ok(format!("Hello, {}!", req.args))
        });
        let (port, cert) = start_tls(server, "tok").await;
        let d = dispatcher(port, cert, "tok");
        await_healthy(&d).await;
        let out = d.invoke(req("hello.greet", "alice")).await.unwrap();
        assert_eq!(out, "Hello, alice!");
    }

    #[tokio::test]
    async fn invoke_unknown_method_returns_protocol_error() {
        let (port, cert) = start_tls(PluginServer::new(), "tok").await;
        let d = dispatcher(port, cert, "tok");
        await_healthy(&d).await;
        let err = d.invoke(req("no.such", "")).await.unwrap_err();
        match err {
            relix_runtime::plugin::PluginInvokeError::Plugin { kind, cause } => {
                assert_eq!(kind, error_kind::UNKNOWN_METHOD);
                assert!(cause.contains("no handler"), "cause: {cause}");
            }
            o => panic!("expected Plugin error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_handler_invalid_args_maps_to_protocol_error() {
        let mut server = PluginServer::new();
        server.register("x.bad", |_| async move {
            Err::<String, _>(PluginError::invalid_args("nope"))
        });
        let (port, cert) = start_tls(server, "tok").await;
        let d = dispatcher(port, cert, "tok");
        await_healthy(&d).await;
        let err = d.invoke(req("x.bad", "")).await.unwrap_err();
        match err {
            relix_runtime::plugin::PluginInvokeError::Plugin { kind, cause } => {
                assert_eq!(kind, error_kind::INVALID_ARGS);
                assert!(cause.contains("nope"), "cause: {cause}");
            }
            o => panic!("expected Plugin error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_handler_internal_error_maps_to_protocol_error() {
        let mut server = PluginServer::new();
        server.register("x.broken", |_| async move {
            Err::<String, _>(PluginError::internal("boom"))
        });
        let (port, cert) = start_tls(server, "tok").await;
        let d = dispatcher(port, cert, "tok");
        await_healthy(&d).await;
        let err = d.invoke(req("x.broken", "")).await.unwrap_err();
        match err {
            relix_runtime::plugin::PluginInvokeError::Plugin { kind, .. } => {
                assert_eq!(kind, error_kind::RESPONDER_INTERNAL);
            }
            o => panic!("expected Plugin error, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_with_wrong_bearer_rejected() {
        let mut server = PluginServer::new();
        server.register("ok.method", |_| async move { Ok("body".to_string()) });
        let (port, cert) = start_tls(server, "expected-token").await;
        let d = dispatcher(port, cert, "attacker-guess");
        await_healthy(&d).await;
        let err = d.invoke(req("ok.method", "")).await.unwrap_err();
        match err {
            relix_runtime::plugin::PluginInvokeError::Plugin { kind, .. } => {
                assert_eq!(kind, error_kind::UNAUTHORIZED);
            }
            o => panic!("expected unauthorized, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn unpinned_cert_fails_tls_handshake() {
        // SEC §11b criterion 3: a dispatcher holding the WRONG cert
        // cannot complete the TLS handshake to the SDK server, so it
        // never reports healthy and invoke surfaces a transport error.
        let (port, _real_cert) = start_tls(PluginServer::new(), "tok").await;
        let (wrong_cert, _k) = loopback_cert();
        let d = dispatcher(port, wrong_cert, "tok");
        for _ in 0..10 {
            assert!(
                !matches!(d.health().await, Ok(true)),
                "handshake succeeded with an unpinned cert"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let err = d.invoke(req("ok.method", "")).await.unwrap_err();
        assert!(
            matches!(err, relix_runtime::plugin::PluginInvokeError::Transport(_)),
            "expected TLS transport error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn invoke_with_correct_bearer_admitted() {
        let mut server = PluginServer::new();
        server.register("ok.method", |_| async move { Ok("ok-body".to_string()) });
        let (port, cert) = start_tls(server, "expected-token").await;
        let d = dispatcher(port, cert, "expected-token");
        await_healthy(&d).await;
        assert_eq!(d.invoke(req("ok.method", "")).await.unwrap(), "ok-body");
    }
}
