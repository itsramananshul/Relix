//! Universal HTTP response security headers.
//!
//! Three headers are stamped on every response the bridge emits,
//! regardless of route:
//!
//! - **`Content-Security-Policy`** — strict same-origin policy.
//!   Scripts and styles must load from the bridge itself; `data:`
//!   URLs are allowed for images so the dashboard's tiny inline
//!   icons round-trip without a separate fetch; WebSocket
//!   connections to `ws:`/`wss:` schemes are permitted so the
//!   `/ws/chat` endpoint works. `'unsafe-inline'` is kept ONLY
//!   for styles — the static dashboard CSS lives inline; the
//!   ~8 KLoC of JavaScript is served from a separate `/assets/`
//!   route so `script-src 'self'` (no `'unsafe-inline'`) takes
//!   effect.
//!
//! - **`X-Frame-Options: DENY`** — refuses any framing attempt
//!   regardless of the parent origin. Belt-and-braces with the
//!   CSP `frame-ancestors 'none'` directive (older browsers
//!   without CSP support still respect XFO).
//!
//! - **`X-Content-Type-Options: nosniff`** — prevents the
//!   browser from MIME-sniffing a response into a more
//!   executable type. Mostly affects user-uploaded assets we
//!   don't have today; cheap to stamp universally.
//!
//! Applied as an axum middleware so the headers ride every
//! handler, not just the dashboard HTML page. JSON responses
//! get the headers too — harmless for JSON consumers, plus a
//! defence in depth in case a JSON route is ever rendered
//! into HTML by a misbehaving client.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// Static CSP value. Matches the docs in the module docstring.
const CSP_VALUE: &str = "default-src 'self'; \
                         script-src 'self'; \
                         style-src 'self' 'unsafe-inline'; \
                         img-src 'self' data:; \
                         connect-src 'self' ws: wss:; \
                         frame-ancestors 'none'; \
                         base-uri 'none'; \
                         form-action 'none'";

/// `X-Content-Type-Options`. Custom HeaderName because axum's
/// `header` module re-exports only the IANA-blessed names and
/// this one ships under its `x-` prefix.
fn xcto_name() -> HeaderName {
    HeaderName::from_static("x-content-type-options")
}

/// Axum middleware. Stamps the three security headers on the
/// response AFTER the inner handler returns. Handlers that
/// already set a stricter CSP (e.g. the dashboard handler
/// historically did) get their header preserved — we only set
/// each header if the inner response didn't already.
pub async fn security_headers_middleware(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    if !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP_VALUE),
        );
    }
    if !headers.contains_key(header::X_FRAME_OPTIONS) {
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    let xcto = xcto_name();
    if !headers.contains_key(&xcto) {
        headers.insert(xcto, HeaderValue::from_static("nosniff"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    fn router() -> Router {
        Router::new()
            .route("/echo", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(security_headers_middleware))
    }

    async fn fetch(uri: &str) -> Response {
        let app = router();
        app.oneshot(HttpRequest::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn csp_header_present_on_arbitrary_route() {
        let r = fetch("/echo").await;
        assert_eq!(r.status(), StatusCode::OK);
        let csp = r
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("default-src 'self'"), "wrong CSP, got {csp:?}");
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("style-src 'self' 'unsafe-inline'"));
        assert!(csp.contains("img-src 'self' data:"));
        assert!(csp.contains("connect-src 'self' ws: wss:"));
        assert!(csp.contains("frame-ancestors 'none'"));
    }

    #[tokio::test]
    async fn xframe_options_deny_present() {
        let r = fetch("/echo").await;
        let xfo = r
            .headers()
            .get(header::X_FRAME_OPTIONS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(xfo, "DENY");
    }

    #[tokio::test]
    async fn x_content_type_options_nosniff_present() {
        let r = fetch("/echo").await;
        let xcto = r
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(xcto, "nosniff");
    }

    /// Inner handlers that set a stricter CSP shouldn't be
    /// clobbered by the layer. The middleware only stamps each
    /// header when absent.
    #[tokio::test]
    async fn middleware_does_not_overwrite_handler_csp() {
        async fn h() -> Response {
            let mut r = Response::new(Body::from("hi"));
            r.headers_mut().insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'"),
            );
            r
        }
        let app = Router::new()
            .route("/strict", get(h))
            .layer(axum::middleware::from_fn(security_headers_middleware));
        let r = app
            .oneshot(
                HttpRequest::builder()
                    .uri("/strict")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let csp = r
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(csp, "default-src 'none'");
    }
}
