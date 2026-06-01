//! Dispatcher that calls a plugin subprocess over a HARDENED
//! local transport.
//!
//! SEC §11: the previous transport was plaintext HTTP on a
//! loopback TCP port (`http://127.0.0.1:<port>`). Any process on
//! the host could connect to that port and SNIFF the per-plugin
//! bearer + the `/invoke` body, or inject calls. The bearer was
//! the only defense and it travelled in the clear.
//!
//! The transport is now a newline-delimited JSON exchange tunneled
//! through **TLS on loopback** (the option called out in the
//! release spec). At load time the host mints a fresh self-signed
//! certificate bound to `127.0.0.1`, hands the cert + key to the
//! plugin (which serves TLS with it), and PINS that exact cert in
//! the dispatcher's trust root — built-in CAs are not trusted, so
//! only the plugin the host just launched is accepted. The channel
//! is therefore confidential (a co-located process can no longer
//! read the bearer or payload off the wire) and authenticated (the
//! dispatcher will not talk to an impostor on the same port). The
//! per-plugin bearer remains as a SECONDARY defense, gating every
//! `invoke` against same-host callers.
//!
//! This module owns the CLIENT side and the cert helper. The
//! matching TLS server lives in the plugin SDK (and, for tests,
//! in this module's `tests` submodule via [`tls_acceptor`]).
//!
//! Everything here is safe Rust — the crate forbids `unsafe_code`.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `invoke` request body. Same shape relix-plugin-sdk decodes on
/// the plugin side; it now travels inside a [`WireRequest`] frame
/// over the TLS channel instead of an HTTP body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvokeRequest {
    pub method: String,
    pub args: String,
    pub trace_id: String,
    pub request_id: String,
    pub caller_subject_id: String,
    pub deadline_unix: i64,
}

/// One framed request on the wire. The `op` tag selects the
/// operation; `invoke` carries the bearer + the typed request.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum WireRequest {
    Health,
    Invoke {
        bearer: String,
        request: InvokeRequest,
    },
}

/// `invoke` response body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvokeResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub error_kind: Option<u32>,
    #[serde(default)]
    pub error_cause: Option<String>,
}

/// `health` response body.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthResponse {
    #[serde(default)]
    pub ok: bool,
}

/// Errors the dispatcher returns. Each one maps cleanly to an
/// `ErrorEnvelope` at the host capability handler.
#[derive(Debug, thiserror::Error)]
pub enum PluginInvokeError {
    /// Connection refused / TLS failure / I/O / timeout.
    #[error("transport: {0}")]
    Transport(String),
    /// Body decode failure — the plugin sent something we can't
    /// understand.
    #[error("decode: {0}")]
    Decode(String),
    /// Plugin returned `ok: false` with a structured error.
    /// `kind` mirrors `relix_core::types::error_kinds`.
    #[error("plugin err kind={kind} {cause}")]
    Plugin { kind: u32, cause: String },
}

/// A plugin's hardened transport endpoint: the loopback address
/// to dial plus the PINNED server certificate (DER) the
/// dispatcher must — and only — trust for that connection.
#[derive(Clone, Debug)]
pub struct PluginEndpoint {
    /// `127.0.0.1:<port>`.
    pub address: String,
    /// The plugin's self-signed certificate, DER-encoded. The
    /// dispatcher trusts exactly this cert (no built-in CAs).
    pub server_cert_der: Vec<u8>,
}

impl PluginEndpoint {
    pub fn new(address: impl Into<String>, server_cert_der: Vec<u8>) -> Self {
        Self {
            address: address.into(),
            server_cert_der,
        }
    }
}

#[derive(Clone)]
pub struct PluginDispatcher {
    endpoint: PluginEndpoint,
    invoke_timeout: Duration,
    /// SEC §11: per-plugin bearer token. Minted at load time (32
    /// random bytes hex-encoded). Secondary defense behind the
    /// pinned-TLS channel; still rejects same-host callers that
    /// lack the token.
    bearer_token: String,
}

fn io_err(msg: impl Into<String>) -> std::io::Error {
    std::io::Error::other(msg.into())
}

impl PluginDispatcher {
    /// Build a dispatcher bound to a plugin's pinned-TLS endpoint.
    pub fn connect(
        endpoint: PluginEndpoint,
        invoke_timeout_secs: u64,
        bearer_token: String,
    ) -> Self {
        Self {
            endpoint,
            invoke_timeout: Duration::from_secs(invoke_timeout_secs.max(1)),
            bearer_token,
        }
    }

    /// The endpoint address. Exposed for diagnostics and tests.
    pub fn endpoint_address(&self) -> &str {
        &self.endpoint.address
    }

    /// Borrow the per-plugin bearer token. Exposed so the host
    /// loader can mirror it into the plugin's environment.
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    /// Probe readiness. `Ok(true)` if the plugin answered a
    /// `health` frame with `ok: true`; `Ok(false)` for a
    /// negative/garbled reply OR any transport failure (endpoint
    /// not yet bound) so the loader's poll loop can simply retry.
    pub async fn health(&self) -> Result<bool, PluginInvokeError> {
        let payload = serde_json::to_string(&WireRequest::Health)
            .map_err(|e| PluginInvokeError::Decode(format!("encode health: {e}")))?;
        let line = match self.exchange(payload, Duration::from_secs(5)).await {
            Ok(l) => l,
            Err(_) => return Ok(false),
        };
        let resp: HealthResponse = serde_json::from_str(line.trim())
            .map_err(|e| PluginInvokeError::Decode(format!("health: {e}")))?;
        Ok(resp.ok)
    }

    /// Send an `invoke` frame and return the plugin's response
    /// body on `ok: true`; convert `ok: false` to
    /// [`PluginInvokeError::Plugin`].
    pub async fn invoke(&self, req: InvokeRequest) -> Result<String, PluginInvokeError> {
        let frame = WireRequest::Invoke {
            bearer: self.bearer_token.clone(),
            request: req,
        };
        let payload = serde_json::to_string(&frame)
            .map_err(|e| PluginInvokeError::Decode(format!("encode invoke: {e}")))?;
        let line = self.exchange(payload, self.invoke_timeout).await?;
        let body: InvokeResponse = serde_json::from_str(line.trim())
            .map_err(|e| PluginInvokeError::Decode(format!("invoke: {e}")))?;
        if body.ok {
            Ok(body.body.unwrap_or_default())
        } else {
            Err(PluginInvokeError::Plugin {
                kind: body.error_kind.unwrap_or(11),
                cause: body
                    .error_cause
                    .unwrap_or_else(|| "(no error_cause)".to_string()),
            })
        }
    }

    /// Open a pinned-TLS connection to the endpoint, write one
    /// JSON line, read one JSON line back, under an overall
    /// timeout.
    async fn exchange(
        &self,
        payload: String,
        timeout: Duration,
    ) -> Result<String, PluginInvokeError> {
        match tokio::time::timeout(timeout, self.connect_and_round_trip(payload)).await {
            Ok(Ok(line)) => Ok(line),
            Ok(Err(e)) => Err(PluginInvokeError::Transport(e.to_string())),
            Err(_) => Err(PluginInvokeError::Transport("timeout".to_string())),
        }
    }

    async fn connect_and_round_trip(&self, payload: String) -> std::io::Result<String> {
        use tokio::net::TcpStream;
        use tokio_rustls::TlsConnector;
        use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
        use tokio_rustls::rustls::{ClientConfig, RootCertStore};

        // Pin EXACTLY the plugin's self-signed cert; trust no
        // built-in CA. A different cert on the same port fails the
        // handshake.
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(self.endpoint.server_cert_der.clone()))
            .map_err(|e| io_err(format!("pin cert: {e}")))?;
        let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| io_err(format!("tls versions: {e}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));

        let tcp = TcpStream::connect(&self.endpoint.address).await?;
        // The cert carries an IP SAN for 127.0.0.1.
        let server_name = ServerName::IpAddress(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)).into(),
        );
        let tls = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| io_err(format!("tls handshake: {e}")))?;
        round_trip(tls, payload).await
    }
}

/// Write one newline-terminated JSON line, then read one line
/// back. `serde_json::to_string` never emits a bare newline, so a
/// single `\n` is an unambiguous frame delimiter.
async fn round_trip<S>(stream: S, payload: String) -> std::io::Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    write_half.write_all(payload.as_bytes()).await?;
    write_half.write_all(b"\n").await?;
    write_half.flush().await?;
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(io_err("plugin closed the connection before replying"));
    }
    Ok(line)
}

/// SEC §11: mint a fresh self-signed certificate (+ private key),
/// both DER-encoded, bound to `127.0.0.1` via an IP SAN. The host
/// loader hands the pair to the plugin (which serves TLS with it)
/// and pins the cert in the dispatcher. A fresh cert per plugin
/// load means a leaked cert is useless after a restart.
pub(crate) fn generate_loopback_cert() -> Result<(Vec<u8>, Vec<u8>), String> {
    use rcgen::{CertificateParams, KeyPair, SanType};
    let mut params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    params.subject_alt_names = vec![SanType::IpAddress(std::net::IpAddr::V4(
        std::net::Ipv4Addr::new(127, 0, 0, 1),
    ))];
    let key_pair = KeyPair::generate().map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    Ok((cert_der, key_der))
}

/// Build a rustls TLS acceptor from a DER cert + PKCS#8 key. Used
/// by the transport tests to stand up the matching TLS server; the
/// plugin SDK (the production server side) builds its own acceptor.
#[cfg(test)]
pub(crate) fn tls_acceptor(
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
) -> Result<tokio_rustls::TlsAcceptor, String> {
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let provider = Arc::new(tokio_rustls::rustls::crypto::aws_lc_rs::default_provider());
    let certs = vec![CertificateDer::from(cert_der)];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der));
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("server cert: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn invoke_req(method: &str) -> InvokeRequest {
        InvokeRequest {
            method: method.to_string(),
            args: "{}".to_string(),
            trace_id: "t".to_string(),
            request_id: "r".to_string(),
            caller_subject_id: "alice".to_string(),
            deadline_unix: 0,
        }
    }

    /// Handle one framed request: enforce the bearer on `invoke`
    /// (secondary defense), echo the method back on success.
    fn handle_frame(line: &str, expected_bearer: &str) -> String {
        let req: WireRequest = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_string(&InvokeResponse {
                    ok: false,
                    body: None,
                    error_kind: Some(11),
                    error_cause: Some(format!("bad frame: {e}")),
                })
                .unwrap();
            }
        };
        match req {
            WireRequest::Health => serde_json::to_string(&HealthResponse { ok: true }).unwrap(),
            WireRequest::Invoke { bearer, request } => {
                if bearer != expected_bearer {
                    return serde_json::to_string(&InvokeResponse {
                        ok: false,
                        error_kind: Some(401),
                        error_cause: Some("unauthorized: bearer mismatch".to_string()),
                        body: None,
                    })
                    .unwrap();
                }
                serde_json::to_string(&InvokeResponse {
                    ok: true,
                    body: Some(format!("handled:{}", request.method)),
                    error_kind: None,
                    error_cause: None,
                })
                .unwrap()
            }
        }
    }

    async fn serve_conn<S>(stream: &mut S, bearer: &str)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte).await {
                Ok(0) => return,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    buf.push(byte[0]);
                }
                Err(_) => return,
            }
        }
        let line = String::from_utf8_lossy(&buf).to_string();
        let resp = handle_frame(&line, bearer);
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.write_all(b"\n").await;
        let _ = stream.flush().await;
    }

    /// Stand up an in-process plugin server speaking the hardened
    /// TLS transport. Returns the dial address + the pinned cert
    /// DER (what the loader passes to the dispatcher).
    async fn spawn_tls_server(bearer: String, max_conns: usize) -> (String, Vec<u8>) {
        let (cert_der, key_der) = generate_loopback_cert().expect("gen cert");
        let acceptor = tls_acceptor(cert_der.clone(), key_der).expect("acceptor");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().unwrap().to_string();
        let bearer = Arc::new(bearer);
        tokio::spawn(async move {
            for _ in 0..max_conns {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let b = bearer.clone();
                tokio::spawn(async move {
                    if let Ok(mut tls) = acceptor.accept(tcp).await {
                        serve_conn(&mut tls, &b).await;
                    }
                });
            }
        });
        (addr, cert_der)
    }

    #[tokio::test]
    async fn tls_is_in_place_not_plaintext() {
        // SEC §11 criterion 2: the transport is TLS on loopback,
        // not plaintext HTTP. A plaintext client gets garbage /
        // a handshake error, never a readable response.
        let (addr, _cert) = spawn_tls_server("tok".to_string(), 4).await;
        // A raw (non-TLS) client writing a plaintext frame must NOT
        // get a usable plaintext reply — the server expects a TLS
        // ClientHello, so the bytes are rejected at the TLS layer.
        let mut raw = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let _ = raw.write_all(b"{\"op\":\"health\"}\n").await;
        let mut buf = [0u8; 64];
        let read = tokio::time::timeout(Duration::from_millis(500), raw.read(&mut buf)).await;
        let plaintext_ok = matches!(read, Ok(Ok(n)) if n > 0
            && String::from_utf8_lossy(&buf[..n]).contains("\"ok\":true"));
        assert!(
            !plaintext_ok,
            "server answered a PLAINTEXT health probe — transport is not TLS-protected"
        );
        println!("transport: TLS on loopback at {addr}; plaintext probe rejected");
    }

    #[tokio::test]
    async fn legitimate_invoke_over_hardened_transport_succeeds() {
        // SEC §11 criterion 3: a legitimate plugin (here an
        // in-process TLS server) loads and is invokable end to end
        // over the hardened transport.
        let bearer = "correct-horse-battery".to_string();
        let (addr, cert) = spawn_tls_server(bearer.clone(), 8).await;
        let disp = PluginDispatcher::connect(PluginEndpoint::new(addr, cert), 5, bearer);

        let mut ready = false;
        for _ in 0..50 {
            if let Ok(true) = disp.health().await {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "server never became healthy over hardened transport");

        let out = disp.invoke(invoke_req("do_thing")).await.unwrap();
        assert_eq!(out, "handled:do_thing");
    }

    #[tokio::test]
    async fn invoke_with_wrong_bearer_is_rejected() {
        // The pinned-TLS channel is the primary boundary; the
        // bearer is the secondary defense and must still reject a
        // caller that presents the wrong token.
        let (addr, cert) = spawn_tls_server("the-real-token".to_string(), 8).await;
        let disp = PluginDispatcher::connect(
            PluginEndpoint::new(addr, cert),
            5,
            "attacker-guess".to_string(),
        );
        for _ in 0..50 {
            if let Ok(true) = disp.health().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let err = disp.invoke(invoke_req("do_thing")).await.unwrap_err();
        match err {
            PluginInvokeError::Plugin { kind, cause } => {
                assert_eq!(kind, 401, "expected unauthorized, got: {cause}");
            }
            other => panic!("expected Plugin 401, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatcher_rejects_untrusted_cert() {
        // Pinning works: a dispatcher holding the WRONG cert (a
        // different self-signed cert) cannot complete the TLS
        // handshake to the server, so it never reaches health-ok.
        let (addr, _real_cert) = spawn_tls_server("tok".to_string(), 4).await;
        let (wrong_cert, _k) = generate_loopback_cert().unwrap();
        let disp =
            PluginDispatcher::connect(PluginEndpoint::new(addr, wrong_cert), 2, "tok".to_string());
        // Health swallows transport errors as `Ok(false)`; it must
        // never report healthy against an untrusted cert.
        for _ in 0..10 {
            assert!(
                !matches!(disp.health().await, Ok(true)),
                "handshake succeeded with an unpinned cert"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // And an explicit invoke surfaces a transport error.
        let err = disp.invoke(invoke_req("x")).await.unwrap_err();
        assert!(
            matches!(err, PluginInvokeError::Transport(_)),
            "expected a TLS transport error, got {err:?}"
        );
    }
}
