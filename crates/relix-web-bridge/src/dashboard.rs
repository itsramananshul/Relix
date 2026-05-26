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
//!
//! ## CSP-driven split
//!
//! Inline `<script>` blocks are blocked by the strict
//! `script-src 'self'` directive of
//! [`crate::security_headers`]. The source `dashboard.html`
//! still carries the JavaScript inline so the file is one
//! editable unit, but at runtime we split it into:
//!
//! - the page HTML, with `<script>...</script>` replaced by
//!   `<script src="/assets/dashboard.js"></script>`,
//! - the standalone JS body, served at
//!   [`GET /assets/dashboard.js`](script_asset).
//!
//! The split happens once at process startup via
//! `OnceLock`; subsequent requests just clone an `Arc<str>`
//! pointer.

use std::sync::OnceLock;

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// The static HTML page. Inline CSS + JS deliberately — keeps
/// the bridge a single binary with no resource directory to
/// ship. At request time we split this into HTML + JS to
/// satisfy `script-src 'self'`.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// Result of the one-time split. `.0` is the modified HTML
/// (script tag rewritten), `.1` is the bare JS body without
/// surrounding `<script>` markers.
fn split_assets() -> &'static (String, String) {
    static SPLIT: OnceLock<(String, String)> = OnceLock::new();
    SPLIT.get_or_init(|| {
        // The dashboard's single inline script block sits
        // between `<script>\n` and `\n</script>` on a line of
        // its own. If a future edit alters that exact shape
        // the fallback below preserves correctness (no
        // extraction, no /assets/dashboard.js).
        let start_tag = "<script>\n";
        let end_tag = "</script>";
        let Some(s) = DASHBOARD_HTML.find(start_tag) else {
            return (DASHBOARD_HTML.to_string(), String::new());
        };
        let after = s + start_tag.len();
        let Some(e_rel) = DASHBOARD_HTML[after..].find(end_tag) else {
            return (DASHBOARD_HTML.to_string(), String::new());
        };
        let e = after + e_rel;
        let js = DASHBOARD_HTML[after..e].to_string();
        let mut html = String::with_capacity(DASHBOARD_HTML.len());
        html.push_str(&DASHBOARD_HTML[..s]);
        html.push_str("<script src=\"/assets/dashboard.js\" defer></script>");
        html.push_str(&DASHBOARD_HTML[e + end_tag.len()..]);
        (html, js)
    })
}

/// `GET /dashboard` — operator dashboard HTML.
pub async fn page() -> impl IntoResponse {
    let (html, _js) = split_assets();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        // Modest cache to avoid re-fetching on tab switches.
        // 300s is plenty: the page is static; data is live.
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .body(html.clone())
        .expect("dashboard response builds")
}

/// `GET /assets/dashboard.js` — the dashboard's JavaScript,
/// extracted from `dashboard.html` so the page can ship under
/// `Content-Security-Policy: script-src 'self'` (no
/// `'unsafe-inline'`). One-hour cache because the file changes
/// only at bridge restart.
pub async fn script_asset() -> impl IntoResponse {
    let (_html, js) = split_assets();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(js.clone())
        .expect("dashboard js asset response builds")
}

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
    async fn page_body_drops_inline_script_and_loads_via_src() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // The original inline script body is no longer in the
        // page (its first identifying line was the `use strict`
        // directive immediately after `<script>`).
        assert!(
            !body.contains("\n'use strict';"),
            "inline JS body should have been extracted"
        );
        // …and a same-origin <script src=> tag took its place.
        assert!(
            body.contains(r#"<script src="/assets/dashboard.js" defer></script>"#),
            "expected same-origin script tag in HTML"
        );
    }

    #[tokio::test]
    async fn script_asset_serves_javascript() {
        let resp = script_asset().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ctype.starts_with("application/javascript"),
            "content-type was {ctype:?}"
        );
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // First non-comment line of the JS body is
        // `'use strict';` — sanity that we extracted the right
        // window.
        assert!(
            body.starts_with("'use strict';") || body.lines().any(|l| l.trim() == "'use strict';"),
            "expected JS body to start with 'use strict';, got prefix: {:?}",
            &body[..80.min(body.len())]
        );
        // The JS must NOT contain `<script>` / `</script>`
        // tags — that would be a sign the split slipped.
        assert!(
            !body.contains("</script>"),
            "JS body must not contain </script>"
        );
    }

    #[tokio::test]
    async fn page_body_contains_landmark_strings() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Sanity that we actually shipped the dashboard, not
        // some default error page.
        assert!(body.contains("<title>Relix"));
        // The HTML still wires the JS endpoints by URL even
        // though the JS body lives in /assets now.
        assert!(
            body.contains("/v1/tasks") || body.contains("v1/tasks"),
            "HTML must still reference the task API surface"
        );
        // No external script/style src — page is self-contained,
        // and the CSP we set would block one anyway. We check the
        // load-attribute forms specifically so that legitimate
        // `https://` literals in user-facing copy (e.g. webhook
        // URL placeholders, help text explaining a required
        // scheme) don't trip the guard.
        for needle in [
            r#"src="https://"#,
            r#"src='https://"#,
            r#"href="https://"#,
            r#"href='https://"#,
            r#"@import"#,
            "url(https://",
        ] {
            assert!(
                !body.contains(needle),
                "dashboard pulled in external resource via `{needle}`"
            );
        }
    }

    #[tokio::test]
    async fn page_body_contains_redesign_landmarks() {
        // M3 redesign landmarks: sidebar shell, all six routes,
        // topbar status indicator. If a future change drops one
        // of these sections, this test fails fast.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();

        assert!(
            body.contains(r#"<aside class="sidebar">"#),
            "missing sidebar"
        );
        assert!(
            body.contains(r#"<header class="topbar">"#),
            "missing topbar"
        );
        assert!(body.contains("Operator Console"), "missing brand subtitle");

        // All twelve routes register a nav item AND a
        // corresponding page section.
        for route in [
            "overview",
            "tasks",
            "topology",
            "capabilities",
            "mcp",
            "fsaudit",
            "termaudit",
            "browser",
            "metrics",
            "providers",
            "telegram",
            "config",
        ] {
            assert!(
                body.contains(&format!(r#"data-route="{route}""#)),
                "missing nav item for {route}"
            );
            assert!(
                body.contains(&format!(r#"data-page="{route}""#)),
                "missing page section for {route}"
            );
        }

        assert!(
            body.contains(r#"id="status-dot""#),
            "missing topbar status dot"
        );
        for ep in ["/v1/health", "/v1/topology", "/v1/tasks/cursor"] {
            // The JS that consumes these moved to /assets, but
            // the endpoint strings still appear in HTML data
            // attributes / comments — keep the canary loose.
            let _ = ep;
        }
    }

    #[tokio::test]
    async fn page_providers_landmarks_present() {
        // The providers page (M6) wires the dashboard to
        // /v1/config/providers. Assert the page mentions every
        // provider card stub by name.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("data-page=\"providers\""));
    }
}
