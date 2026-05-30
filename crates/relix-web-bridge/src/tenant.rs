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
    req.extensions_mut().insert(TenantId(
        tenant_value.unwrap_or_else(|| DEFAULT_TENANT.to_string()),
    ));
    let mut resp = next.run(req).await;
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

    #[test]
    fn fix_part5_tenant_config_from_auth_section_parses_ips_and_falls_back_on_empty() {
        use crate::config::AuthSection;
        let s = AuthSection {
            multi_tenant_mode: true,
            trusted_internal_origins: vec!["192.0.2.1".into(), "garbage".into()],
            tenant_bindings: HashMap::new(),
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
        };
        let cfg2 = TenantConfig::from_auth_section(&s2);
        assert!(cfg2.trusted_origins.iter().any(|ip| ip.is_loopback()));
    }
}
