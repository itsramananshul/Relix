//! Per-request tenant identifier.
//!
//! PART 5 of the tenant-isolation rollout. The bridge derives
//! every request's tenant id from the auth principal — NOT from
//! the X-Relix-Tenant header — so an external caller cannot
//! impersonate another tenant by hand-crafting the header.
//!
//! ## Decision tree
//!
//! The resolver runs in this order:
//!
//! 1. **Authenticated principal with a binding.** When the
//!    request carries a bearer token whose first 8 hex chars
//!    appear in `[auth.tenant_bindings]`, the corresponding
//!    tenant id is canonical. The X-Relix-Tenant header (if
//!    any) is ignored — the binding wins.
//! 2. **Authenticated principal WITHOUT a binding,
//!    `multi_tenant_mode = true`.** Returns
//!    [`TenantResolution::MissingBinding`] so the caller can
//!    respond with HTTP 401: "No tenant binding found for this
//!    credential."
//! 3. **Trusted internal origin sending X-Relix-Tenant.** The
//!    source IP is in `[auth.trusted_internal_origins]` — the
//!    header value is accepted as advisory and returned. Used
//!    by the control-plane / reverse-proxy that already
//!    authenticated upstream.
//! 4. **Untrusted source sending X-Relix-Tenant.** The header
//!    is silently ignored — no error, just no effect. (An
//!    operator who wants the header to take effect must add
//!    the source IP to `trusted_internal_origins`.)
//! 5. **`multi_tenant_mode = false`.** Returns `None` — every
//!    downstream call proceeds as single-tenant.
//!
//! The middleware stamps the resolved tenant id (or sentinel
//! "missing") into the request's Extensions so handlers can
//! pull it out via `Extension<TenantId>`. The actual mesh-call
//! helper in `peer_call.rs` reads the same value when
//! building the outbound `RequestEnvelope.tenant_id`.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

tokio::task_local! {
    /// PART 3 — task-local tenant resolved by the middleware.
    /// Every wrapper helper that builds a mesh request reads
    /// this via [`current_tenant`] so no handler has to thread
    /// the tenant id through its signature manually. The
    /// middleware binds it via
    /// `CURRENT_TENANT.scope(tenant, next.run(req))` so the
    /// value is in scope for the entire downstream call chain
    /// (axum drives `next.run` in the same task; the task-local
    /// is visible to every `.await` inside the handler).
    ///
    /// Value semantics: `String` (always present after the
    /// middleware ran). `"default"` for single-tenant mode;
    /// the canonical tenant id for multi-tenant resolved
    /// requests. `current_tenant()` returns `Option<String>`;
    /// it returns `None` ONLY when called outside the
    /// middleware's scope (test code that constructs the
    /// envelope directly).
    pub static CURRENT_TENANT: String;
}

/// PART 3 — read the per-request tenant id resolved by the
/// middleware. Returns `Some(tid)` when called inside the
/// middleware's scope, `None` otherwise. Wrappers should
/// pass the result through to
/// `relix_runtime::dispatch::build_request_with_tenant`.
pub fn current_tenant() -> Option<String> {
    CURRENT_TENANT.try_with(|t| t.clone()).ok()
}

tokio::task_local! {
    /// GROUP 1 PHASE 1A — the authenticated caller's subject id
    /// for this request, resolved by [`tenant_middleware`] from
    /// the [`SUBJECT_HEADER`] principal header. This is the
    /// AUTHENTICATED principal channel — the same trust boundary
    /// as the bearer token and `X-Relix-Tenant` — and is the ONLY
    /// source of caller identity. Handlers MUST derive
    /// `from_subject_id` / `reader_subject_id` / `subject_id` from
    /// here via [`require_caller_subject`], never from the request
    /// body or path, so an authenticated caller can only act as
    /// themselves. `None` when no subject header was presented (or
    /// when called outside the middleware scope, e.g. direct test
    /// calls that don't set it).
    pub static CURRENT_SUBJECT: Option<String>;
}

/// GROUP 1 PHASE 1A — header carrying the authenticated caller's
/// subject id. Set by the authenticated session / front-end layer
/// (the principal channel), NEVER read from the request body. The
/// request body is the untrusted user-content channel; mixing
/// identity into it let any authenticated caller spoof any sender
/// and read or delete another user's messages.
pub const SUBJECT_HEADER: &str = "x-relix-subject";

/// Maximum accepted length of a subject id from the header.
pub const MAX_SUBJECT_LEN: usize = 256;

/// GROUP 1 PHASE 1A — read the authenticated caller subject bound
/// by the middleware for this request. `None` when absent.
pub fn current_subject() -> Option<String> {
    CURRENT_SUBJECT.try_with(|s| s.clone()).ok().flatten()
}

/// Parse + sanitise the [`SUBJECT_HEADER`] off the request
/// headers. Returns `None` for missing / empty / over-long /
/// non-ASCII-graphic values so a malformed header is treated as
/// "no authenticated subject" (fail closed downstream).
pub fn caller_subject_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(SUBJECT_HEADER)?.to_str().ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_SUBJECT_LEN
        || !trimmed.chars().all(|c| c.is_ascii_graphic())
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// GROUP 1 PHASE 1A — failure modes when reconciling a body /
/// path subject claim against the authenticated caller subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectError {
    /// No authenticated caller subject is available for this
    /// request — identity cannot be enforced, so fail closed
    /// (HTTP 401) rather than trusting the body.
    Unauthenticated,
    /// The body/path asserted a subject that is NOT the
    /// authenticated caller — a spoofing attempt (HTTP 403).
    Forbidden {
        claimed: String,
        authenticated: String,
    },
}

/// GROUP 1 PHASE 1A — resolve the caller's subject id for an
/// identity-bound operation.
///
/// Identity ALWAYS comes from the authenticated principal channel
/// ([`current_subject`]); the optional `body_claim` (a
/// `from_subject_id` / `reader_subject_id` / `subject_id` field or
/// path segment supplied on the wire) may only AGREE with it:
/// - no authenticated subject            → `Err(Unauthenticated)`
/// - body claims a DIFFERENT subject     → `Err(Forbidden)`
/// - body omits, or matches, the subject → `Ok(authenticated)`
///
/// A caller can therefore only ever act as themselves; the body
/// can never override or widen the authenticated identity.
pub fn require_caller_subject(body_claim: Option<&str>) -> Result<String, SubjectError> {
    let authed = current_subject()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(SubjectError::Unauthenticated)?;
    if let Some(claim) = body_claim.map(str::trim).filter(|c| !c.is_empty())
        && claim != authed
    {
        return Err(SubjectError::Forbidden {
            claimed: claim.to_string(),
            authenticated: authed,
        });
    }
    Ok(authed)
}

/// PART 3 — same as [`current_tenant`] but filters out the
/// single-tenant sentinel so callers can pass
/// `Option<&str>` directly to
/// `build_request_with_tenant`. When the middleware resolved
/// the request to single-tenant mode, the bound value is
/// `"default"`; downstream wrappers prefer to omit the field
/// entirely so the wire envelope's `tenant_id` stays
/// `None` for legacy responders.
pub fn current_tenant_or_none() -> Option<String> {
    let raw = current_tenant()?;
    if raw == DEFAULT_TENANT {
        None
    } else {
        Some(raw)
    }
}

/// Identifier extracted from the request — either derived
/// from an auth binding (canonical) or accepted from a
/// trusted source's header (advisory). Cloned into each
/// handler via the axum Extensions map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantId(pub String);

impl TenantId {
    /// Borrow the underlying tenant id string. Used by
    /// handlers that need the string form (audit
    /// attribution, error messages, etc.).
    #[allow(dead_code)] // PART 3 callers will exercise this.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Default tenant when the request is in single-tenant mode
/// AND no header / binding applied. Matches the SDK's
/// `relix_sdk::DEFAULT_TENANT` constant so the two sides
/// agree on the wire identifier.
pub const DEFAULT_TENANT: &str = "default";

/// Maximum header length we accept. Operators can pass
/// anything up to this; longer values are silently dropped.
pub const MAX_TENANT_LEN: usize = 128;

/// Number of leading hex chars of a bearer token used as
/// the `tenant_bindings` lookup key. Long enough to avoid
/// realistic collisions; short enough that the operator
/// only has to copy a manageable prefix into their config
/// file.
pub const API_KEY_PREFIX_LEN: usize = 8;

/// Outcome of [`resolve_tenant`]. The middleware turns each
/// variant into a different HTTP response shape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TenantResolution {
    /// Resolved cleanly. The String is the canonical tenant
    /// id (from binding or trusted header).
    Resolved(String),
    /// Authenticated request whose credential has no
    /// `[auth.tenant_bindings]` entry AND
    /// `multi_tenant_mode = true`. The middleware returns
    /// HTTP 401 with the documented copy.
    MissingBinding,
    /// Single-tenant mode + no binding + no trusted header.
    /// Downstream callers proceed with `None` tenant.
    SingleTenant,
}

/// Pure-function resolver. Independent of axum so unit tests
/// drive every branch without spinning a Router. The four
/// inputs are: the auth bindings table, the trusted-origin
/// whitelist, the multi-tenant-mode flag, the request's
/// source IP, the bearer token if any, and the
/// X-Relix-Tenant header value if any.
pub fn resolve_tenant(
    tenant_bindings: &HashMap<String, String>,
    trusted_origins: &[IpAddr],
    multi_tenant_mode: bool,
    source_ip: IpAddr,
    bearer_token: Option<&str>,
    header_tenant: Option<&str>,
) -> TenantResolution {
    // Step 1: derive from auth binding.
    if let Some(tok) = bearer_token {
        let prefix = api_key_prefix(tok);
        if let Some(bound) = tenant_bindings.get(&prefix) {
            return TenantResolution::Resolved(bound.clone());
        }
        // Authenticated request whose credential is unknown
        // to the bindings table. In multi-tenant mode this
        // is a hard 401 — every credential MUST map to a
        // tenant.
        if multi_tenant_mode {
            return TenantResolution::MissingBinding;
        }
    } else if multi_tenant_mode {
        // No bearer + multi-tenant mode is also a hard 401
        // — every request needs a credential we can bind.
        return TenantResolution::MissingBinding;
    }
    // Step 2: trusted-origin header. Only honoured when the
    // source IP is in the whitelist; ignored otherwise.
    if let Some(raw) = header_tenant
        && trusted_origins.contains(&source_ip)
        && let Some(clean) = sanitize_header_value(raw)
    {
        return TenantResolution::Resolved(clean);
    }
    // Step 3: legacy single-tenant fall-through.
    TenantResolution::SingleTenant
}

/// First [`API_KEY_PREFIX_LEN`] chars of a bearer token, used
/// as the `tenant_bindings` lookup key. Lowercased so the
/// operator-side config doesn't have to match case.
pub fn api_key_prefix(token: &str) -> String {
    token
        .chars()
        .take(API_KEY_PREFIX_LEN)
        .collect::<String>()
        .to_lowercase()
}

/// Apply the same sanity filters the legacy resolver applied:
/// non-empty, ASCII-graphic, length-bounded. Returns `None`
/// when the value fails any filter.
fn sanitize_header_value(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() > MAX_TENANT_LEN {
        return None;
    }
    if !trimmed.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Parse the request's `Authorization: Bearer <token>` header
/// (when present). Returns `None` for missing / malformed
/// shapes.
pub fn extract_bearer_from_headers(headers: &HeaderMap) -> Option<&str> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let prefix = "Bearer ";
    if !raw.starts_with(prefix) {
        return None;
    }
    let token = raw[prefix.len()..].trim();
    if token.is_empty() {
        return None;
    }
    Some(token)
}

/// PART 5 axum extractor wrapper around [`resolve_tenant`].
/// Reads the relevant signals from `req` + the cloned bridge
/// auth config it captured in state. Returns the same enum
/// as the pure resolver so the middleware can decide whether
/// to short-circuit with 401.
pub fn extract_tenant_id(
    bindings: &HashMap<String, String>,
    trusted_origins: &[IpAddr],
    multi_tenant_mode: bool,
    source_ip: IpAddr,
    headers: &HeaderMap,
) -> TenantResolution {
    let bearer = extract_bearer_from_headers(headers);
    let header_tenant = headers.get("x-relix-tenant").and_then(|v| v.to_str().ok());
    resolve_tenant(
        bindings,
        trusted_origins,
        multi_tenant_mode,
        source_ip,
        bearer,
        header_tenant,
    )
}

/// Bundled snapshot of the bridge's auth-related config that
/// the tenant middleware needs at request time. Cheap to
/// clone — strings + ip addresses + a small HashMap. Built
/// once at boot from the operator's `[auth]` section.
#[derive(Clone, Debug)]
pub struct TenantConfig {
    pub multi_tenant_mode: bool,
    pub trusted_origins: Vec<IpAddr>,
    pub tenant_bindings: HashMap<String, String>,
}

impl Default for TenantConfig {
    fn default() -> Self {
        Self {
            multi_tenant_mode: false,
            trusted_origins: vec![
                "127.0.0.1".parse().expect("ipv4 loopback parses"),
                "::1".parse().expect("ipv6 loopback parses"),
            ],
            tenant_bindings: HashMap::new(),
        }
    }
}

impl TenantConfig {
    /// Build from the parsed [`crate::config::AuthSection`].
    /// Untrusted-looking IP strings are skipped at boot with
    /// a WARN log so the operator notices the typo before
    /// production traffic flows.
    pub fn from_auth_section(section: &crate::config::AuthSection) -> Self {
        let mut origins = Vec::with_capacity(section.trusted_internal_origins.len());
        for raw in &section.trusted_internal_origins {
            match raw.parse::<IpAddr>() {
                Ok(ip) => origins.push(ip),
                Err(e) => tracing::warn!(
                    raw = %raw,
                    error = %e,
                    "auth: skipping unparseable trusted_internal_origins entry"
                ),
            }
        }
        if origins.is_empty() {
            // Fall back to loopback so a misconfigured / typo'd
            // `trusted_internal_origins` doesn't lock the
            // operator out of the dashboard.
            origins = TenantConfig::default().trusted_origins;
        }
        Self {
            multi_tenant_mode: section.multi_tenant_mode,
            trusted_origins: origins,
            tenant_bindings: section.tenant_bindings.clone(),
        }
    }
}

/// PART 5 axum middleware. Resolves the per-request tenant
/// per the decision tree at the top of this file and either
/// (a) stashes `TenantId` into request Extensions and runs
/// the next handler, or (b) short-circuits with HTTP 401.
pub async fn tenant_middleware(
    State(cfg): State<TenantConfig>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut req: Request,
    next: Next,
) -> Response {
    let outcome = extract_tenant_id(
        &cfg.tenant_bindings,
        &cfg.trusted_origins,
        cfg.multi_tenant_mode,
        addr.ip(),
        req.headers(),
    );
    let (tenant_value, header_echo) = match outcome {
        TenantResolution::Resolved(t) => (Some(t.clone()), t),
        TenantResolution::SingleTenant => (None, DEFAULT_TENANT.to_string()),
        TenantResolution::MissingBinding => {
            let body = r#"{"error":"No tenant binding found for this credential. Configure a tenant binding in [auth.tenant_bindings]."}"#;
            return match Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
            {
                Ok(r) => r,
                Err(_) => StatusCode::UNAUTHORIZED.into_response(),
            };
        }
    };
    let scope_value = tenant_value
        .clone()
        .unwrap_or_else(|| DEFAULT_TENANT.to_string());
    req.extensions_mut().insert(TenantId(scope_value.clone()));
    // GROUP 1 PHASE 1A — resolve the authenticated caller
    // subject from the principal header BEFORE running the
    // handler, and bind it as a task-local. Identity-bound
    // handlers read it via `require_caller_subject` so caller
    // identity comes from the authenticated channel, never the
    // request body.
    let caller_subject = caller_subject_from_headers(req.headers());
    // PART 3 — bind the resolved tenant as a task-local so
    // every downstream wrapper (`call_peer_*`, `proxy_json`,
    // …) can read it without the handler having to thread
    // the value through its signature manually. The scope
    // covers the entire `next.run(req)` future; when the
    // handler awaits a mesh call, the wrapper inside reads
    // `current_tenant()` and stamps it onto the envelope via
    // `build_request_with_tenant`.
    let mut resp = CURRENT_TENANT
        .scope(
            scope_value,
            CURRENT_SUBJECT.scope(caller_subject, next.run(req)),
        )
        .await;
    if let Ok(v) = axum::http::HeaderValue::from_str(&header_echo) {
        resp.headers_mut().insert("x-relix-tenant", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lb() -> IpAddr {
        "127.0.0.1".parse().unwrap()
    }
    fn external() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }
    fn trusted() -> Vec<IpAddr> {
        vec![lb()]
    }
    fn binding_map() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("deadbeef".into(), "acme".into());
        m.insert("cafef00d".into(), "globex".into());
        m
    }

    #[test]
    fn fix_part5_binding_wins_over_header() {
        // Token starts with deadbeef → acme. The conflicting
        // header value is ignored, even from a trusted origin.
        let r = resolve_tenant(
            &binding_map(),
            &trusted(),
            true,
            lb(),
            Some("deadbeefXXXX"),
            Some("globex"),
        );
        assert_eq!(r, TenantResolution::Resolved("acme".into()));
    }

    #[test]
    fn fix_part5_unknown_binding_in_multi_tenant_mode_is_missing_binding() {
        let r = resolve_tenant(
            &binding_map(),
            &trusted(),
            true,
            lb(),
            Some("unknown-prefix-token"),
            None,
        );
        assert_eq!(r, TenantResolution::MissingBinding);
    }

    #[test]
    fn fix_part5_no_credential_in_multi_tenant_mode_is_missing_binding() {
        let r = resolve_tenant(&binding_map(), &trusted(), true, lb(), None, None);
        assert_eq!(r, TenantResolution::MissingBinding);
    }

    #[test]
    fn fix_part5_header_honoured_from_trusted_origin() {
        // No credential, single-tenant mode. The trusted
        // loopback peer is allowed to advise the tenant via
        // header.
        let r = resolve_tenant(&binding_map(), &trusted(), false, lb(), None, Some("acme"));
        assert_eq!(r, TenantResolution::Resolved("acme".into()));
    }

    #[test]
    fn fix_part5_header_ignored_from_untrusted_origin() {
        // External caller's header is silently ignored. With
        // multi_tenant_mode = false + no credential, the
        // resolver returns SingleTenant.
        let r = resolve_tenant(
            &binding_map(),
            &trusted(),
            false,
            external(),
            None,
            Some("acme"),
        );
        assert_eq!(r, TenantResolution::SingleTenant);
    }

    #[test]
    fn fix_part5_header_from_untrusted_origin_does_not_short_circuit_multi_tenant_401() {
        // External caller sending a header in multi-tenant
        // mode still gets the MissingBinding 401 — the
        // ignored header doesn't satisfy the binding
        // requirement.
        let r = resolve_tenant(
            &binding_map(),
            &trusted(),
            true,
            external(),
            None,
            Some("acme"),
        );
        assert_eq!(r, TenantResolution::MissingBinding);
    }

    #[test]
    fn fix_part5_single_tenant_mode_with_no_credential_returns_single_tenant() {
        let r = resolve_tenant(&binding_map(), &trusted(), false, lb(), None, None);
        assert_eq!(r, TenantResolution::SingleTenant);
    }

    #[test]
    fn fix_part5_api_key_prefix_lowercases_and_truncates() {
        // 8 chars, lowercased.
        assert_eq!(api_key_prefix("DeadBeef123"), "deadbeef");
        // Shorter than 8 → keep the whole string.
        assert_eq!(api_key_prefix("abc"), "abc");
        // Empty → empty (no binding will match).
        assert_eq!(api_key_prefix(""), "");
    }

    #[test]
    fn fix_part5_header_value_sanitisers_match_legacy_filters() {
        // Empty / whitespace-only → ignored.
        assert!(sanitize_header_value("").is_none());
        assert!(sanitize_header_value("   ").is_none());
        // Over-length → ignored.
        let huge = "a".repeat(MAX_TENANT_LEN + 1);
        assert!(sanitize_header_value(&huge).is_none());
        // Non-ASCII-graphic → ignored.
        assert!(sanitize_header_value("acme tenant").is_none());
        // Valid → trimmed + accepted.
        assert_eq!(sanitize_header_value("  acme  "), Some("acme".into()));
    }

    #[test]
    fn fix_part5_extract_bearer_handles_missing_and_malformed() {
        let mut h = HeaderMap::new();
        assert!(extract_bearer_from_headers(&h).is_none());
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Basic foo".parse().unwrap(),
        );
        assert!(extract_bearer_from_headers(&h).is_none());
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer  ".parse().unwrap(),
        );
        assert!(extract_bearer_from_headers(&h).is_none());
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer abcdef".parse().unwrap(),
        );
        assert_eq!(extract_bearer_from_headers(&h), Some("abcdef"));
    }

    // ── GROUP 1 PHASE 1A: authenticated-caller-subject gate ──
    //
    // `require_caller_subject` is the gate EVERY identity-bound
    // handler (messaging send / inbox / read / thread / delete,
    // and the swept siblings) now calls as its first line. These
    // tests exercise it directly under the same `CURRENT_SUBJECT`
    // scope the middleware binds, so they prove the handler
    // behaviour without standing up a full AppState + mesh.

    #[tokio::test]
    async fn phase1a_caller_authed_as_a_cannot_act_as_b() {
        // Caller authenticated as subject "A". A body/path that
        // claims subject "B" (spoof) → Forbidden, which the
        // handlers surface as HTTP 403. This is the send-as-B,
        // read-B's-inbox, delete-B's-message attack.
        let out = CURRENT_SUBJECT
            .scope(Some("A".to_string()), async {
                require_caller_subject(Some("B"))
            })
            .await;
        assert_eq!(
            out,
            Err(SubjectError::Forbidden {
                claimed: "B".to_string(),
                authenticated: "A".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn phase1a_legitimate_caller_acts_as_self() {
        // Subject A sending as A / reading A's own messages: the
        // body either omits the subject or names A. Both resolve
        // to A — admitted.
        let claimed_self = CURRENT_SUBJECT
            .scope(Some("A".to_string()), async {
                require_caller_subject(Some("A"))
            })
            .await;
        assert_eq!(claimed_self, Ok("A".to_string()));

        let omitted = CURRENT_SUBJECT
            .scope(Some("A".to_string()), async {
                require_caller_subject(None)
            })
            .await;
        assert_eq!(omitted, Ok("A".to_string()));
    }

    #[tokio::test]
    async fn phase1a_no_authenticated_subject_fails_closed() {
        // No principal header bound → identity cannot be enforced,
        // so the gate fails closed (HTTP 401) rather than trusting
        // the body's claim.
        let bound_none = CURRENT_SUBJECT
            .scope(None, async { require_caller_subject(Some("A")) })
            .await;
        assert_eq!(bound_none, Err(SubjectError::Unauthenticated));
        // Outside any middleware scope, also unauthenticated.
        assert_eq!(
            require_caller_subject(Some("A")),
            Err(SubjectError::Unauthenticated)
        );
    }

    #[test]
    fn phase1a_subject_header_parsing_sanitises() {
        let mut h = HeaderMap::new();
        assert!(caller_subject_from_headers(&h).is_none());
        h.insert(SUBJECT_HEADER, "  subj-1  ".parse().unwrap());
        assert_eq!(caller_subject_from_headers(&h).as_deref(), Some("subj-1"));
        // Empty / whitespace-only → None.
        h.insert(SUBJECT_HEADER, "   ".parse().unwrap());
        assert!(caller_subject_from_headers(&h).is_none());
        // Non-ASCII-graphic (control chars / spaces inside) → None.
        h.insert(SUBJECT_HEADER, "a b".parse().unwrap());
        assert!(caller_subject_from_headers(&h).is_none());
    }

    #[tokio::test]
    async fn fix_part3_current_tenant_returns_none_outside_scope() {
        // Outside the middleware's scope (e.g. a direct test
        // call), the task-local is unbound and the helper
        // returns None so the wrapper falls through to the
        // legacy tenant-blind path.
        assert!(current_tenant().is_none());
        assert!(current_tenant_or_none().is_none());
    }

    #[tokio::test]
    async fn fix_part3_current_tenant_reads_value_from_scope() {
        // Inside `CURRENT_TENANT.scope(...)`, the helper
        // returns the bound value. The middleware uses the
        // same pattern to bind the resolved tenant for the
        // entire downstream handler chain.
        let observed = CURRENT_TENANT
            .scope("acme".to_string(), async { current_tenant() })
            .await;
        assert_eq!(observed, Some("acme".to_string()));
    }

    #[tokio::test]
    async fn fix_part3_current_tenant_or_none_filters_default_sentinel() {
        // In single-tenant mode the middleware binds
        // `DEFAULT_TENANT`. The `_or_none` variant filters
        // that sentinel so the outbound envelope's
        // `tenant_id` stays `None` for legacy responders.
        let observed = CURRENT_TENANT
            .scope(DEFAULT_TENANT.to_string(), async {
                current_tenant_or_none()
            })
            .await;
        assert!(observed.is_none());
        // A real tenant id passes through.
        let observed = CURRENT_TENANT
            .scope("acme".to_string(), async { current_tenant_or_none() })
            .await;
        assert_eq!(observed, Some("acme".to_string()));
    }

    #[test]
    fn fix_part5_tenant_config_from_auth_section_parses_ips_and_falls_back_on_empty() {
        use crate::config::AuthSection;
        let s = AuthSection {
            multi_tenant_mode: true,
            trusted_internal_origins: vec!["192.0.2.1".into(), "garbage".into()],
            tenant_bindings: HashMap::new(),
            setup_token: None,
        };
        let cfg = TenantConfig::from_auth_section(&s);
        // The valid one was kept; the garbage was dropped.
        assert_eq!(cfg.trusted_origins.len(), 1);
        assert_eq!(
            cfg.trusted_origins[0],
            "192.0.2.1".parse::<IpAddr>().unwrap()
        );
        // If the entire list is invalid we fall back to
        // loopback so the operator isn't locked out.
        let s2 = AuthSection {
            multi_tenant_mode: false,
            trusted_internal_origins: vec!["nope".into()],
            tenant_bindings: HashMap::new(),
            setup_token: None,
        };
        let cfg2 = TenantConfig::from_auth_section(&s2);
        assert!(cfg2.trusted_origins.iter().any(|ip| ip.is_loopback()));
    }
}
