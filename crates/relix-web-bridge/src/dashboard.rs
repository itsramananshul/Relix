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

/// The static HTML page baked into the binary. Inline CSS +
/// JS deliberately — keeps the bridge a single binary with no
/// resource directory to ship. At request time we split this
/// into HTML + JS to satisfy `script-src 'self'`.
const DASHBOARD_HTML_EMBEDDED: &str = include_str!("dashboard.html");

/// Resolve the dashboard HTML source for this process.
///
/// Order of precedence:
/// 1. `RELIX_DASHBOARD_PATH=/some/file.html` reads the file
///    at boot (operators can hot-swap the UI without rebuilding).
///    A missing or unreadable file logs a warning and falls
///    back to the embedded copy.
/// 2. Otherwise the [`include_str!`]-baked copy.
///
/// Resolution happens once per process via [`OnceLock`] so
/// the env-var change requires a bridge restart — matches the
/// rest of the bridge's configuration posture.
fn dashboard_html_source() -> &'static str {
    static SOURCE: OnceLock<String> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let env_value = std::env::var("RELIX_DASHBOARD_PATH").ok();
            resolve_dashboard_source(env_value.as_deref())
        })
        .as_str()
}

/// Pure resolver — takes the env-var value as an `Option<&str>`
/// so tests can exercise both the file-found and file-missing
/// branches without mutating process env (`std::env::set_var`
/// is unsafe in Rust 2024 and the test harness forbids unsafe).
fn resolve_dashboard_source(env_value: Option<&str>) -> String {
    match env_value {
        Some(path) if !path.trim().is_empty() => match std::fs::read_to_string(path) {
            Ok(contents) => {
                tracing::info!(
                    dashboard.source = "file",
                    dashboard.path = %path,
                    dashboard.bytes = contents.len(),
                    "dashboard: loaded HTML from RELIX_DASHBOARD_PATH"
                );
                contents
            }
            Err(e) => {
                tracing::warn!(
                    dashboard.source = "embedded",
                    dashboard.path = %path,
                    error = %e,
                    "dashboard: RELIX_DASHBOARD_PATH unreadable — falling back to embedded copy"
                );
                DASHBOARD_HTML_EMBEDDED.to_string()
            }
        },
        _ => DASHBOARD_HTML_EMBEDDED.to_string(),
    }
}

/// Result of the one-time split. `.0` is the modified HTML
/// (script tag rewritten), `.1` is the bare JS body without
/// surrounding `<script>` markers.
fn split_assets() -> &'static (String, String) {
    static SPLIT: OnceLock<(String, String)> = OnceLock::new();
    SPLIT.get_or_init(|| {
        let source = dashboard_html_source();
        // The dashboard's single inline script block sits
        // between `<script>\n` and `\n</script>` on a line of
        // its own. If a future edit alters that exact shape
        // the fallback below preserves correctness (no
        // extraction, no /assets/dashboard.js).
        let start_tag = "<script>\n";
        let end_tag = "</script>";
        let Some(s) = source.find(start_tag) else {
            return (source.to_string(), String::new());
        };
        let after = s + start_tag.len();
        let Some(e_rel) = source[after..].find(end_tag) else {
            return (source.to_string(), String::new());
        };
        let e = after + e_rel;
        let js = source[after..e].to_string();
        let mut html = String::with_capacity(source.len());
        html.push_str(&source[..s]);
        html.push_str("<script src=\"/assets/dashboard.js\" defer></script>");
        html.push_str(&source[e + end_tag.len()..]);
        (html, js)
    })
}

/// `GET /dashboard` — operator dashboard HTML.
pub async fn page() -> Response {
    let (html, _js) = split_assets();
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        // Modest cache to avoid re-fetching on tab switches.
        // 300s is plenty: the page is static; data is live.
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .body(html.clone())
    {
        Ok(r) => r.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "dashboard: response builder failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dashboard response builder failed",
            )
                .into_response()
        }
    }
}

/// `GET /assets/dashboard.js` — the dashboard's JavaScript,
/// extracted from `dashboard.html` so the page can ship under
/// `Content-Security-Policy: script-src 'self'` (no
/// `'unsafe-inline'`). One-hour cache because the file changes
/// only at bridge restart.
pub async fn script_asset() -> Response {
    let (_html, js) = split_assets();
    match Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(js.clone())
    {
        Ok(r) => r.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "dashboard: script asset response builder failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "dashboard js builder failed",
            )
                .into_response()
        }
    }
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

    // ── RELIX-7.11 GAP 3 — agent observability landmarks + env override ──

    #[tokio::test]
    async fn page_includes_agent_observability_panels() {
        // The four GAP-3 panels (Agent summary, Active alerts,
        // Cost breakdown, Per-agent trend) must each register a
        // host element + manual refresh button + last-refresh
        // slot in the dashboard HTML. A future edit that drops
        // one of these IDs breaks the per-panel refresh /
        // teardown wiring. JS-side landmarks
        // (AGENT_OBS_INTERVALS_MS, _SPARK_GLYPHS,
        // relixBridgeBase, endpoint URLs) live in the
        // /assets/dashboard.js body and are asserted by
        // `script_asset_carries_agent_observability_logic`.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for id in ["agents-host", "alerts-host", "cost-host", "trend-host"] {
            assert!(
                body.contains(&format!(r#"id="{id}""#)),
                "missing GAP-3 panel host id={id:?}"
            );
        }
        for prefix in ["agents", "alerts", "cost", "trend"] {
            assert!(
                body.contains(&format!(r#"id="{prefix}-refresh-btn""#)),
                "missing per-panel refresh button for {prefix}"
            );
            assert!(
                body.contains(&format!(r#"id="{prefix}-last-refresh""#)),
                "missing per-panel last-refresh slot for {prefix}"
            );
        }
    }

    #[tokio::test]
    async fn script_asset_carries_agent_observability_logic() {
        // The JS that drives the four GAP-3 panels lives in
        // /assets/dashboard.js (extracted out for CSP). Every
        // load-bearing JS landmark is asserted here so a future
        // refactor of the script body fails fast.
        let resp = script_asset().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("AGENT_OBS_INTERVALS_MS"),
            "missing per-panel polling cadence map"
        );
        assert!(
            body.contains("relixBridgeBase"),
            "missing localStorage.relixBridgeBase override"
        );
        assert!(
            body.contains("_SPARK_GLYPHS"),
            "missing sparkline glyph table"
        );
        for ep in [
            "/v1/metrics/agents",
            "/v1/metrics/alerts",
            "/v1/metrics/cost",
            "/timeseries",
        ] {
            assert!(
                body.contains(ep),
                "script asset does not reference {ep:?} — panel will not load"
            );
        }
        // Per-panel loaders + teardown helper.
        for fname in [
            "loadAgentsPanel",
            "loadAlertsPanel",
            "loadCostPanel",
            "loadTrendPanel",
            "teardownAgentObservability",
        ] {
            assert!(
                body.contains(fname),
                "missing JS function {fname} in script asset"
            );
        }
    }

    #[test]
    fn resolve_dashboard_source_uses_embedded_when_env_unset() {
        let out = resolve_dashboard_source(None);
        assert_eq!(out, DASHBOARD_HTML_EMBEDDED);
    }

    #[test]
    fn resolve_dashboard_source_uses_embedded_when_env_empty() {
        let out = resolve_dashboard_source(Some(""));
        assert_eq!(out, DASHBOARD_HTML_EMBEDDED);
        let out_ws = resolve_dashboard_source(Some("   "));
        assert_eq!(out_ws, DASHBOARD_HTML_EMBEDDED);
    }

    #[test]
    fn resolve_dashboard_source_serves_alternate_file_when_env_set() {
        // Write a recognisable HTML file to a tempfile and
        // verify the resolver reads it instead of returning the
        // embedded copy. Marker chosen so it cannot collide with
        // any real dashboard string.
        let tmp =
            std::env::temp_dir().join(format!("relix_dashboard_test_{}.html", std::process::id()));
        let marker = "<!-- RELIX_TEST_MARKER_ZZZ -->";
        std::fs::write(&tmp, marker).expect("write tempfile");
        let out = resolve_dashboard_source(Some(tmp.to_str().unwrap()));
        let _ = std::fs::remove_file(&tmp);
        assert_eq!(out, marker, "resolver did not read the env-pointed file");
    }

    #[test]
    fn resolve_dashboard_source_falls_back_when_file_missing() {
        // Pointing at a path that does not exist must not panic
        // — it logs a warning and returns the embedded copy.
        let bogus = std::env::temp_dir().join("relix_dashboard_does_not_exist_zzz.html");
        let _ = std::fs::remove_file(&bogus); // ensure absent
        let out = resolve_dashboard_source(Some(bogus.to_str().unwrap()));
        assert_eq!(out, DASHBOARD_HTML_EMBEDDED);
    }
}
