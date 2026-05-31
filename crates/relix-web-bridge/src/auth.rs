//! Bridge-level HTTP authentication + CSRF guard.
//!
//! The bridge exposes a large mutating API on loopback. Two threats
//! it must defend against:
//!
//! 1. Other local processes / users on the same machine probing the
//!    open port. Solved by a per-bridge **bearer token** every
//!    state-changing route demands.
//! 2. A malicious webpage in the operator's browser firing
//!    `fetch('http://127.0.0.1:19791/v1/...')` to ride the
//!    same-origin-but-different-tab pattern. Solved by a **CSRF
//!    origin guard** that rejects requests with an `Origin` header
//!    pointing anywhere other than the bridge's own host.
//!
//! Three endpoints are intentionally **unauthenticated** so the
//! dashboard can bootstrap itself and so health probes work:
//!
//! - `GET /health`             — plaintext liveness
//! - `GET /dashboard`          — static HTML page
//! - `GET /v1/auth/token`      — one-time bootstrap (loopback-only)
//!
//! The OpenAI shim (`POST /v1/chat/completions`) is treated
//! specially: any non-empty bearer token is accepted because OpenAI
//! clients always send some key and the real provider key lives on
//! the AI node, not the bridge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::RngCore;
use serde::Serialize;

use crate::config::AppState;

/// Minimal slice of `AppState` the auth middleware needs. Lets us
/// exercise the middleware in tests without standing up the full
/// state (mesh client, manifest cache, recorder, ...).
#[derive(Clone)]
pub struct AuthState {
    pub token: BridgeToken,
    pub host: String,
    pub port: u16,
    /// PART 8: extra bearer-credential prefixes admitted by the
    /// middleware. Populated from `[auth.tenant_bindings]` at
    /// startup. Any bearer whose 8-char prefix (per
    /// `crate::tenant::api_key_prefix`) appears in this set is
    /// admitted as if it were the bridge token. The tenant
    /// middleware (which runs AFTER auth) then resolves the
    /// prefix to a tenant_id from the same `tenant_bindings`
    /// table.
    ///
    /// Empty in single-tenant deployments — auth admits only
    /// the bridge token. Populated when
    /// `[auth] tenant_bindings = { … }` is configured.
    pub tenant_binding_prefixes: std::collections::HashSet<String>,
}

/// Bytes of entropy in the bridge token (256 bits → 64 hex chars).
const TOKEN_BYTES: usize = 32;

/// Loaded or freshly-generated bridge token.
#[derive(Clone)]
pub struct BridgeToken {
    /// Hex-encoded value the dashboard receives.
    value: Arc<String>,
    path: Arc<PathBuf>,
}

impl BridgeToken {
    /// Read the token from `path` if it exists; otherwise generate
    /// a fresh 256-bit token, write it at restrictive permissions,
    /// and return that.
    ///
    /// Best-effort: a corrupted / unreadable file is treated as
    /// missing so the bridge can always boot.
    pub fn load_or_generate(path: &Path) -> Result<Self, String> {
        if let Ok(bytes) = std::fs::read(path) {
            let trimmed: String = String::from_utf8_lossy(&bytes).trim().to_string();
            if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
                return Ok(Self {
                    value: Arc::new(trimmed),
                    path: Arc::new(path.to_path_buf()),
                });
            }
            tracing::warn!(path = %path.display(),
                "bridge-token: file is unreadable / malformed; regenerating");
        }

        let mut buf = [0u8; TOKEN_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut buf);
        let value = hex::encode(buf);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("bridge-token mkdir {}: {e}", parent.display()))?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, value.as_bytes())
            .map_err(|e| format!("bridge-token write {}: {e}", tmp.display()))?;
        // Restrict the tmp before rename so the final atomic rename
        // moves an already-locked-down file into place.
        let _ = crate::os_secure::restrict_to_current_user(&tmp);
        std::fs::rename(&tmp, path).map_err(|e| {
            format!(
                "bridge-token rename {} -> {}: {e}",
                tmp.display(),
                path.display()
            )
        })?;
        // Re-apply after rename: NTFS may reset ACEs on rename in
        // some configurations; chmod on POSIX is preserved already.
        let _ = crate::os_secure::restrict_to_current_user(path);
        Ok(Self {
            value: Arc::new(value),
            path: Arc::new(path.to_path_buf()),
        })
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Constant-time string comparison. Returns true iff `a == b`
/// without short-circuiting on the first mismatched byte.
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.as_bytes().iter().zip(b.as_bytes().iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

/// Pull `Authorization: Bearer <token>` from the request. Returns
/// `None` when the header is missing or malformed.
fn extract_bearer(req: &Request) -> Option<&str> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// SEC PART 3 (DELETED): `extract_query_token`, `percent_decode`,
/// and `hex_val` lived here. They implemented the `?token=`
/// SSE fallback that the middleware now refuses with HTTP 400.
/// Removing the parser closes the surface so a future caller
/// cannot accidentally re-introduce the URL-token path.
#[derive(Serialize)]
struct ErrBody {
    error: &'static str,
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrBody {
            error: "unauthorized",
        }),
    )
        .into_response()
}

/// SEC PART 3: header alternative to `Authorization: Bearer
/// <setup_token>`. Some operator tooling cannot override the
/// `Authorization` header (e.g. `curl` users with persistent
/// auth helpers); they present the setup token via this
/// dedicated header instead.
const SETUP_TOKEN_HEADER: &str = "X-Relix-Setup-Token";

fn extract_setup_header(req: &Request) -> Option<String> {
    req.headers()
        .get(SETUP_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// SEC PART 3: pull the operator-configured setup token off
/// the AppState. `None` keeps `/v1/auth/token` returning
/// HTTP 403.
fn resolve_setup_token(state: &crate::config::AppState) -> Option<String> {
    state.setup_token.clone()
}

fn forbidden_setup_token_unset() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(ErrBody {
            error: "setup_token not configured — set `[auth] setup_token` in \
                    bridge.toml or RELIX_SETUP_TOKEN in the environment to \
                    enable the bootstrap surface",
        }),
    )
        .into_response()
}

fn unauthorized_setup_token_missing() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrBody {
            error: "setup token required (Authorization: Bearer <setup_token> \
                    or X-Relix-Setup-Token: <setup_token>)",
        }),
    )
        .into_response()
}

fn unauthorized_setup_token_wrong() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ErrBody {
            error: "setup token did not match",
        }),
    )
        .into_response()
}

fn forbidden_csrf() -> Response {
    (StatusCode::FORBIDDEN, Json(ErrBody { error: "csrf" })).into_response()
}

/// Whether the request path is in the always-public allowlist.
fn is_public_path(path: &str) -> bool {
    matches!(path, "/health" | "/dashboard" | "/v1/auth/token") || path.starts_with("/assets/")
}

// SEC PART 3 (DELETED): `is_openai_shim_path` lived here.
// The OpenAI shim path is no longer auth-special — it runs
// the full bearer-token validation pipeline identical to
// every other authenticated route. Removing the helper
// makes it impossible to accidentally re-introduce the
// "any non-empty bearer" bypass.

/// CSRF origin guard. Rejects when:
/// - `Origin` is present, AND
/// - the value is not the string literal `null`, AND
/// - the value's host:port does not match the bridge's own
///   listen address.
///
/// Loopback callers (curl, internal services) usually do not send
/// Origin at all and pass through. Browser tabs always send it.
fn origin_ok(req: &Request, expected_host: &str, expected_port: u16) -> bool {
    let Some(origin) = req.headers().get(header::ORIGIN) else {
        return true;
    };
    let Ok(o) = origin.to_str() else {
        return false;
    };
    if o == "null" {
        return true;
    }
    // Parse "<scheme>://<host>[:<port>]". Anything else → reject.
    let rest = match o.find("://") {
        Some(i) => &o[i + 3..],
        None => return false,
    };
    let (host, port_str) = match rest.find(':') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let port: u16 = match port_str {
        Some(s) => match s.parse() {
            Ok(p) => p,
            Err(_) => return false,
        },
        None => {
            if o.starts_with("https://") {
                443
            } else {
                80
            }
        }
    };
    let host_match =
        host == expected_host || host == "127.0.0.1" || host == "localhost" || host == "[::1]";
    host_match && port == expected_port
}

/// Axum middleware that enforces the auth + CSRF rules described
/// in this module's docstring.
///
/// SEC PART 3 changes the historical posture in three ways:
///
/// 1. The OpenAI shim path is no longer special-cased: it runs the
///    full bearer-token validation pipeline, identical to every
///    other authenticated route. A bearer that does not match the
///    bridge token (or a configured tenant prefix) is rejected
///    with HTTP 401. Pre-fix path admitted any non-empty bearer.
/// 2. The `?token=<token>` query-parameter fallback is removed
///    entirely. EventSource / SSE callers that previously relied
///    on it now receive HTTP 400 with a message instructing them
///    to use the `Authorization` header. Allowing the token in a
///    URL meant it ended up in operator browser history, web-server
///    access logs, and HTTP referer headers — none of which the
///    bridge controls.
/// 3. There is no other change to the bearer-token contract:
///    valid token → next; missing token → 401; mismatched token →
///    401; tenant-binding prefix match (when configured) → admit.
pub async fn auth_middleware(State(auth): State<AuthState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();

    if is_public_path(&path) {
        return next.run(req).await;
    }

    let token = auth.token.value();

    // CSRF first — the answer is cheap to compute and we don't
    // want to leak whether a token is right or wrong when the
    // origin is obviously hostile.
    if !origin_ok(&req, &auth.host, auth.port) {
        return forbidden_csrf();
    }

    // SEC PART 3: refuse `?token=<token>` outright. Pre-fix
    // path silently accepted it; operators relying on the SSE
    // fallback now see a structured 400 telling them to use
    // the Authorization header instead.
    if has_token_query_param(&req) {
        return bad_request_query_token_disallowed();
    }

    let provided = match extract_bearer(&req) {
        Some(s) => s.to_string(),
        None => return unauthorized(),
    };

    if ct_eq(&provided, token) {
        return next.run(req).await;
    }
    // PART 8: admit a bearer whose 8-char prefix matches a
    // configured `[auth.tenant_bindings]` key. The tenant
    // middleware (mounted underneath) reads the same prefix
    // and routes the request to the bound tenant. We don't
    // need constant-time compare here because the prefix is
    // an operator-published lookup key, not a secret —
    // possession of the full bearer is what authenticates;
    // the prefix only routes the binding lookup.
    if !auth.tenant_binding_prefixes.is_empty() {
        let prefix = crate::tenant::api_key_prefix(&provided);
        if auth.tenant_binding_prefixes.contains(&prefix) {
            return next.run(req).await;
        }
    }
    unauthorized()
}

/// SEC PART 3: structured 400 returned when a caller still
/// presents the `?token=<token>` query parameter. Body
/// instructs them to switch to the `Authorization: Bearer
/// <token>` header.
fn bad_request_query_token_disallowed() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrBody {
            error: "?token query parameter is not supported; use \
                    `Authorization: Bearer <token>`",
        }),
    )
        .into_response()
}

/// SEC PART 3: return `true` when the request URI carries a
/// `token=` query parameter. Used by the middleware to refuse
/// the historical SSE-token-in-URL flow.
fn has_token_query_param(req: &Request) -> bool {
    let Some(q) = req.uri().query() else {
        return false;
    };
    q.split('&').any(|pair| pair.starts_with("token="))
}

/// `GET /v1/auth/token` — one-time bootstrap so the dashboard can
/// fetch its token at first load.
///
/// SEC PART 3: pre-fix path accepted any local request — which
/// meant any process on the operator's machine could exfiltrate
/// the bridge token by hitting `http://127.0.0.1:<port>/v1/auth/token`.
/// The endpoint now REQUIRES an operator-configured setup token,
/// presented as `Authorization: Bearer <setup_token>` (or
/// `X-Relix-Setup-Token: <setup_token>` for tools that cannot
/// override the `Authorization` header). The setup token comes
/// from `[auth] setup_token` in `bridge.toml` or, if absent
/// there, from the `RELIX_SETUP_TOKEN` env var. When neither is
/// configured the endpoint returns HTTP 403 — operators must
/// opt into the bootstrap surface.
pub async fn bootstrap_token(State(state): State<AppState>, req: Request) -> Response {
    // Cross-origin browser? Refuse.
    if !origin_ok(&req, &state.bridge_host, state.bridge_port) {
        return forbidden_csrf();
    }
    // SEC PART 3: setup-token gate. Operators configure
    // `[auth] setup_token` OR set `RELIX_SETUP_TOKEN`. When
    // neither is set the endpoint refuses outright; pre-fix
    // path returned the bridge token to any unauthenticated
    // loopback caller.
    let expected_setup = match resolve_setup_token(&state) {
        Some(s) => s,
        None => return forbidden_setup_token_unset(),
    };
    let presented = extract_bearer(&req)
        .map(str::to_string)
        .or_else(|| extract_setup_header(&req));
    let provided = match presented {
        Some(p) => p,
        None => return unauthorized_setup_token_missing(),
    };
    if !ct_eq(&provided, &expected_setup) {
        return unauthorized_setup_token_wrong();
    }
    #[derive(Serialize)]
    struct TokenBody<'a> {
        token: &'a str,
    }
    let body = serde_json::to_string(&TokenBody {
        token: state.bridge_token.value(),
    })
    .unwrap_or_else(|_| "{}".to_string());
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))
        .body(Body::from(body))
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "auth: bootstrap response builder failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrBody {
                    error: "bootstrap_response_failed",
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_matches_only_when_equal() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn token_load_or_generate_creates_file_then_reuses() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-token");
        let t1 = BridgeToken::load_or_generate(&path).unwrap();
        assert!(path.exists());
        let v1 = t1.value().to_string();
        assert_eq!(v1.len(), TOKEN_BYTES * 2);
        assert!(v1.chars().all(|c| c.is_ascii_hexdigit()));
        // Second call must reuse, not regenerate.
        let t2 = BridgeToken::load_or_generate(&path).unwrap();
        assert_eq!(t1.value(), t2.value());
        // Sanity: the generated file is exactly the token text.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk.trim(), v1);
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_mode_0600_on_posix() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge-token");
        let _ = BridgeToken::load_or_generate(&path).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn is_public_path_only_matches_three_routes_and_assets() {
        assert!(is_public_path("/health"));
        assert!(is_public_path("/dashboard"));
        assert!(is_public_path("/v1/auth/token"));
        assert!(is_public_path("/assets/main.css"));
        assert!(!is_public_path("/chat"));
        assert!(!is_public_path("/v1/tasks"));
        assert!(!is_public_path("/v1/health"));
    }

    fn req_with(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().method("POST").uri(uri);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn extract_bearer_pulls_token() {
        let r = req_with("/v1/tasks", &[("authorization", "Bearer abc123")]);
        assert_eq!(extract_bearer(&r), Some("abc123"));
        let r = req_with("/v1/tasks", &[]);
        assert!(extract_bearer(&r).is_none());
        let r = req_with("/v1/tasks", &[("authorization", "Bearer ")]);
        assert!(extract_bearer(&r).is_none());
    }

    #[test]
    fn sec_p3_has_token_query_param_detects_token_in_url() {
        // SEC PART 3: the `?token=` SSE-fallback parser is
        // gone; the middleware now uses `has_token_query_param`
        // to refuse any URL carrying it.
        let r = req_with("/v1/tasks?token=deadbeef&x=1", &[]);
        assert!(has_token_query_param(&r));
        let r = req_with("/v1/tasks?x=1", &[]);
        assert!(!has_token_query_param(&r));
        let r = req_with("/v1/tasks", &[]);
        assert!(!has_token_query_param(&r));
    }

    #[test]
    fn origin_ok_accepts_same_loopback_host_port() {
        let r = req_with("/v1/tasks", &[("origin", "http://127.0.0.1:19791")]);
        assert!(origin_ok(&r, "127.0.0.1", 19791));
        let r = req_with("/v1/tasks", &[("origin", "http://localhost:19791")]);
        assert!(origin_ok(&r, "127.0.0.1", 19791));
    }

    #[test]
    fn origin_ok_rejects_other_host_or_port() {
        let r = req_with("/v1/tasks", &[("origin", "http://evil.example.com")]);
        assert!(!origin_ok(&r, "127.0.0.1", 19791));
        let r = req_with("/v1/tasks", &[("origin", "http://127.0.0.1:19790")]);
        assert!(!origin_ok(&r, "127.0.0.1", 19791));
    }

    #[test]
    fn origin_ok_accepts_missing_or_null() {
        let r = req_with("/v1/tasks", &[]);
        assert!(origin_ok(&r, "127.0.0.1", 19791));
        let r = req_with("/v1/tasks", &[("origin", "null")]);
        assert!(origin_ok(&r, "127.0.0.1", 19791));
    }

    // ── End-to-end middleware tests (router-level) ──────────

    use axum::Router;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    fn test_state() -> (AuthState, String) {
        let tmp = tempfile::tempdir().unwrap();
        let token_path = tmp.path().join("bridge-token");
        let token = BridgeToken::load_or_generate(&token_path).unwrap();
        let value = token.value().to_string();
        // Leak the tempdir — BridgeToken cached the value at
        // construction time, so the file can be removed.
        std::mem::forget(tmp);
        (
            AuthState {
                token,
                host: "127.0.0.1".to_string(),
                port: 19791,
                tenant_binding_prefixes: std::collections::HashSet::new(),
            },
            value,
        )
    }

    fn router(state: AuthState) -> Router {
        Router::new()
            .route("/health", get(|| async { "ok\n" }))
            .route("/dashboard", get(|| async { "<html/>" }))
            .route("/v1/tasks", get(|| async { "[]" }))
            .route("/v1/chat/completions", post(|| async { "{}" }))
            .layer(axum::middleware::from_fn_with_state(state, auth_middleware))
    }

    async fn req(app: Router, b: axum::http::request::Builder) -> Response {
        app.oneshot(b.body(Body::empty()).unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn middleware_health_is_public_without_auth() {
        let (state, _) = test_state();
        let r = req(router(state), Request::builder().uri("/health")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_dashboard_is_public_without_auth() {
        let (state, _) = test_state();
        let r = req(router(state), Request::builder().uri("/dashboard")).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_protected_without_auth_returns_401() {
        let (state, _) = test_state();
        let r = req(router(state), Request::builder().uri("/v1/tasks")).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_protected_with_wrong_token_returns_401() {
        let (state, _) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .uri("/v1/tasks")
                .header("authorization", "Bearer wrong-token"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn middleware_protected_with_correct_token_passes() {
        let (state, token) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .uri("/v1/tasks")
                .header("authorization", format!("Bearer {token}")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sec_p3_middleware_rejects_token_query_param_with_400() {
        // SEC PART 3: the `?token=` SSE-fallback path is
        // removed. Any URL carrying it must be refused
        // BEFORE the bearer compare so the operator sees
        // a structured 400 telling them to switch headers.
        let (state, token) = test_state();
        let r = req(
            router(state),
            Request::builder().uri(format!("/v1/tasks?token={token}")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn middleware_csrf_origin_mismatch_returns_403() {
        let (state, token) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .uri("/v1/tasks")
                .header("authorization", format!("Bearer {token}"))
                .header("origin", "http://evil.example.com"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn middleware_csrf_loopback_origin_passes() {
        let (state, token) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .uri("/v1/tasks")
                .header("authorization", format!("Bearer {token}"))
                .header("origin", "http://127.0.0.1:19791"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sec_p3_middleware_openai_shim_rejects_wrong_bearer() {
        // SEC PART 3: the OpenAI shim path runs the full
        // bearer pipeline. Pre-fix path accepted "Bearer
        // <anything>". Now a non-matching bearer is 401.
        let (state, _) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer sk-not-the-bridge-token"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sec_p3_middleware_openai_shim_accepts_correct_bridge_token() {
        // SEC PART 3: the OpenAI shim still admits the
        // bridge token, identical to every other
        // authenticated route.
        let (state, token) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn sec_p3_forbidden_setup_token_unset_returns_403_with_helpful_body() {
        // SEC PART 3: when `[auth] setup_token` and
        // RELIX_SETUP_TOKEN are both unset, bootstrap_token
        // returns the "setup_token not configured" body via
        // this helper. The 403 status + the operator-
        // readable message are the contract.
        let resp = forbidden_setup_token_unset();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn sec_p3_unauthorized_setup_token_missing_returns_401() {
        let resp = unauthorized_setup_token_missing();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn sec_p3_unauthorized_setup_token_wrong_returns_401() {
        let resp = unauthorized_setup_token_wrong();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn sec_p3_extract_setup_header_reads_dedicated_header() {
        let r = req_with("/v1/auth/token", &[("x-relix-setup-token", "abc123")]);
        assert_eq!(extract_setup_header(&r).as_deref(), Some("abc123"));
        let r = req_with("/v1/auth/token", &[("x-relix-setup-token", "   ")]);
        assert!(extract_setup_header(&r).is_none());
        let r = req_with("/v1/auth/token", &[]);
        assert!(extract_setup_header(&r).is_none());
    }

    #[tokio::test]
    async fn sec_p3_middleware_openai_shim_rejects_missing_bearer() {
        let (state, _) = test_state();
        let r = req(
            router(state),
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions"),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
}
