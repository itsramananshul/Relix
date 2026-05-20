//! Minimal operator dashboard served at `/dashboard`.
//!
//! Single static HTML page. Vanilla JS only — no build step,
//! no bundler, no framework. The page consumes the existing
//! `/v1/tasks*` JSON endpoints described in
//! [`docs/task-api.md`](../../../docs/task-api.md); the bridge
//! does not introduce any new server-side state or
//! orchestration to support it (see
//! [`docs/bridge-invariants.md`](../../../docs/bridge-invariants.md)).
//!
//! The page intentionally stays small enough to read in one
//! sitting. If it grows beyond that the right move is to
//! split into static files served by an external web server —
//! the bridge is not a frontend host.

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// `GET /dashboard` — operator dashboard HTML.
pub async fn page() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        // Modest cache to avoid re-fetching on tab switches.
        // 300s is plenty: the page is static; data is live.
        .header(header::CACHE_CONTROL, "public, max-age=300")
        // Defensive against the page being embedded in an
        // iframe by an untrusted origin.
        .header("X-Frame-Options", "DENY")
        .header(header::CONTENT_SECURITY_POLICY, csp())
        .body(DASHBOARD_HTML.to_string())
        .expect("dashboard response builds")
}

/// CSP that only allows inline styles + scripts (the page is
/// fully self-contained) and disables every other origin.
fn csp() -> &'static str {
    "default-src 'none'; \
     style-src 'unsafe-inline'; \
     script-src 'unsafe-inline'; \
     connect-src 'self'; \
     form-action 'none'; \
     base-uri 'none'; \
     frame-ancestors 'none'"
}

/// The static HTML page. Inline CSS + JS deliberately — keeps
/// the bridge a single binary with no resource directory to
/// ship.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn page_returns_html_200() {
        let resp = page().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ctype.starts_with("text/html"), "content-type was {ctype:?}");
    }

    #[tokio::test]
    async fn page_body_contains_landmark_strings() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Sanity that we actually shipped the dashboard, not
        // some default error page.
        assert!(body.contains("<title>Relix"));
        // Calls the JSON endpoints described in task-api.md.
        assert!(body.contains("/v1/tasks"));
        // No external script src — page is self-contained, and
        // the CSP we set would block one anyway.
        assert!(
            !body.contains("https://"),
            "dashboard pulled in external resource"
        );
    }

    #[tokio::test]
    async fn page_sets_security_headers() {
        let resp = page().await.into_response();
        assert_eq!(
            resp.headers()
                .get("X-Frame-Options")
                .and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("default-src 'none'"));
        // Only same-origin XHR allowed — the dashboard talks
        // to the same bridge serving it.
        assert!(csp.contains("connect-src 'self'"));
        // No frame ancestors at all.
        assert!(csp.contains("frame-ancestors 'none'"));
    }
}
