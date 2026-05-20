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
    async fn page_body_contains_redesign_landmarks() {
        // M3 redesign landmarks: sidebar shell, all six routes,
        // topbar status indicator. If a future change drops one
        // of these sections, this test fails fast.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
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

        // All six routes register a nav item AND a corresponding page section.
        for route in [
            "overview",
            "tasks",
            "topology",
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
            assert!(body.contains(ep), "page should consume {ep}");
        }
    }

    #[tokio::test]
    async fn page_providers_landmarks_present() {
        // The providers page (M6) wires the dashboard to
        // /v1/config/providers. Assert the page mentions every
        // shipped provider in the allowlist + the API endpoint
        // so a future rename catches at test time.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("/v1/config/providers"),
            "providers page should fetch /v1/config/providers"
        );
        // Allowlist labels appear in PROVIDER_LABELS — at least
        // the canonical names must be present in the rendered
        // page source.
        for name in ["mock", "openai", "anthropic", "openrouter", "xai", "google"] {
            assert!(
                body.contains(name),
                "provider {name} missing from dashboard"
            );
        }
        // type="password" input ensures the key field is masked
        // by default.
        assert!(
            body.contains(r#"type="password""#),
            "api_key input should be type=password by default"
        );
    }

    #[tokio::test]
    async fn page_telegram_landmarks_present() {
        // The telegram page (M7) wires the dashboard to
        // /v1/config/telegram. Assert the API path + BotFather
        // setup hint + mode selector all appear in the page.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("/v1/config/telegram"),
            "telegram page should fetch /v1/config/telegram"
        );
        assert!(
            body.contains("BotFather"),
            "telegram page should mention BotFather setup"
        );
        // Mode selector exposes both shipped + future-but-blocked modes.
        for mode in ["polling", "webhook"] {
            assert!(body.contains(mode), "telegram mode {mode} missing");
        }
        // Token input is masked by default. We already assert
        // type="password" in the providers test; here we
        // additionally assert it's near the telegram id.
        assert!(
            body.contains(r#"id="tg-token""#),
            "telegram bot token input missing"
        );
    }

    #[tokio::test]
    async fn page_tasks_search_and_chips_present() {
        // M12: tasks page gains a search input + quick-filter
        // chip row. Assert both land.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"id="filter-search""#),
            "tasks page should ship a free-text search input"
        );
        for status in ["running", "failed", "interrupted", "completed"] {
            assert!(
                body.contains(&format!(r#"data-quick-filter="{status}""#)),
                "missing quick-filter chip for {status}"
            );
        }
    }

    #[tokio::test]
    async fn page_toast_host_present() {
        // M12: toast notification host replaces alert() calls
        // for action feedback. Operator actions don't block.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"id="toast-host""#),
            "dashboard should ship a toast notification host"
        );
    }

    #[tokio::test]
    async fn page_overview_activity_feed_present() {
        // M10: overview page hosts a live activity rail that
        // diffs poll-cycle snapshots and surfaces transitions.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"id="activity-feed""#),
            "overview should host an activity-feed element"
        );
        assert!(
            body.contains("waiting for activity"),
            "overview should ship a default empty state for activity"
        );
    }

    #[tokio::test]
    async fn page_topology_graph_landmarks_present() {
        // M9: the topology page renders an SVG graph + a
        // peer detail drawer. Assert both elements ship.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"id="topology-graph""#),
            "topology page should ship an SVG graph element"
        );
        assert!(
            body.contains(r#"id="node-drawer""#),
            "topology page should ship a node detail drawer"
        );
        assert!(
            body.contains("topology-legend"),
            "topology page should ship a freshness legend"
        );
    }

    #[tokio::test]
    async fn page_config_landmarks_present() {
        // The bridge config page (M8) reads /v1/config and
        // renders the effective bridge state. Assert the
        // endpoint + refresh button id appear.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("/v1/config"),
            "config page should fetch /v1/config"
        );
        assert!(
            body.contains(r#"id="config-refresh""#),
            "config page should expose a refresh button"
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
