//! Per-request tenant identifier.
//!
//! Every inbound HTTP request carries an opaque `tenant_id` we
//! attach as an `axum::Extension`. The middleware reads the
//! `X-Relix-Tenant` request header, falls back to `"default"`
//! when absent, and stashes the result for downstream handlers
//! to use.
//!
//! This is the foundation for multi-tenancy. Today, the
//! enforcement is purely book-keeping:
//!
//! - **Tasks** the bridge creates carry the tenant id in the
//!   `origin_surface` metadata field (a free-form label the
//!   coordinator already persists). When the actual
//!   `tenant_id` column lands on the tasks table, this is
//!   where the value flows.
//! - **Audit logs** record the tenant alongside the caller's
//!   subject_id, so an operator can grep "show me everything
//!   tenant=acme did this week" without joining tables.
//! - **Memory** isolation is on the SDK side today —
//!   `RelixClient::remember` namespaces its `subject_id` as
//!   `tenant:<id>`. The bridge passes the prompt + session_id
//!   through unchanged.
//!
//! Real cross-tenant access enforcement (a tenant can't see
//! another tenant's tasks, memories, or audit entries) requires
//! schema changes on the coordinator + memory tables and admission
//! checks downstream. The flow lands incrementally; the header
//! just needs to be in every request now so the value is
//! available when the enforcement code is added.

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

/// Identifier extracted from `X-Relix-Tenant` (or the default).
/// Cloned into each handler via the axum Extensions map.
#[derive(Clone, Debug)]
#[allow(dead_code)] // Reserved for downstream tenant-aware handlers.
pub struct TenantId(pub String);

impl TenantId {
    #[allow(dead_code)] // Reserved for downstream tenant-aware handlers.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Default tenant when the request header is absent. Matches
/// the SDK's `relix_sdk::DEFAULT_TENANT` constant so the two
/// sides agree on the wire identifier.
pub const DEFAULT_TENANT: &str = "default";

/// Maximum header length we accept. Operators can pass anything
/// up to this; longer values are truncated to the default so a
/// hostile caller can't blow up audit log entries with a
/// kilobyte tenant id.
pub const MAX_TENANT_LEN: usize = 128;

/// Extract the tenant id from the request's `X-Relix-Tenant`
/// header, falling back to [`DEFAULT_TENANT`] when absent or
/// when the header value is empty / non-ASCII / over-length.
/// Pure function — exported for tests.
pub fn extract_tenant_id(req: &Request) -> String {
    req.headers()
        .get("x-relix-tenant")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| s.len() <= MAX_TENANT_LEN)
        .filter(|s| s.chars().all(|c| c.is_ascii_graphic()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_TENANT.to_string())
}

/// Axum middleware. Stamps the per-request [`TenantId`] into
/// the request's Extensions so downstream handlers can pull it
/// via `Extension<TenantId>`. Also echoes the tenant id back as
/// the `X-Relix-Tenant` response header so SDK callers can
/// verify the bridge saw the same value they sent.
pub async fn tenant_middleware(mut req: Request, next: Next) -> Response {
    let tenant_value = extract_tenant_id(&req);
    let tenant_for_response = tenant_value.clone();
    req.extensions_mut().insert(TenantId(tenant_value));
    let mut resp = next.run(req).await;
    if let Ok(v) = axum::http::HeaderValue::from_str(&tenant_for_response) {
        resp.headers_mut().insert("x-relix-tenant", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    fn req_with(headers: &[(&str, &str)]) -> Request {
        let mut b = HttpRequest::builder().uri("/x");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn missing_header_returns_default() {
        let r = req_with(&[]);
        assert_eq!(extract_tenant_id(&r), DEFAULT_TENANT);
    }

    #[test]
    fn explicit_tenant_round_trips() {
        let r = req_with(&[("x-relix-tenant", "acme")]);
        assert_eq!(extract_tenant_id(&r), "acme");
    }

    #[test]
    fn empty_header_value_falls_back_to_default() {
        let r = req_with(&[("x-relix-tenant", "  ")]);
        assert_eq!(extract_tenant_id(&r), DEFAULT_TENANT);
    }

    #[test]
    fn overlong_header_falls_back_to_default() {
        let huge = "a".repeat(MAX_TENANT_LEN + 1);
        let r = req_with(&[("x-relix-tenant", huge.as_str())]);
        assert_eq!(extract_tenant_id(&r), DEFAULT_TENANT);
    }

    #[test]
    fn non_ascii_header_falls_back_to_default() {
        let r = req_with(&[("x-relix-tenant", "ten ant")]);
        // Contains a space which is not ASCII graphic.
        assert_eq!(extract_tenant_id(&r), DEFAULT_TENANT);
    }

    #[tokio::test]
    async fn tenant_id_flows_through_request_extensions() {
        use axum::Router;
        use axum::extract::Extension;
        use axum::http::StatusCode;
        use axum::routing::get;
        use tower::ServiceExt;

        async fn handler(Extension(t): Extension<TenantId>) -> String {
            t.0
        }
        let app = Router::new()
            .route("/x", get(handler))
            .layer(axum::middleware::from_fn(tenant_middleware));
        let resp = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/x")
                    .header("x-relix-tenant", "acme")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Response should also echo the tenant header back.
        let echo = resp
            .headers()
            .get("x-relix-tenant")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(echo, "acme");
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(body, "acme");
    }
}
