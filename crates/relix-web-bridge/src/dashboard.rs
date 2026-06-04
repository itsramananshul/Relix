//! Operator dashboard served at `/dashboard`.
//!
//! Single self-contained HTML file with every CSS rule and every
//! line of JavaScript inline (no external fetches, no CDN, no
//! bundler). The file lives at `src/dashboard.html` and is
//! `include_str!`-baked into the binary so the bridge ships as one
//! self-contained artifact — operators do not have to manage a
//! frontend asset directory.
//!
//! ## Per-route CSP
//!
//! The bridge's default CSP (see `crate::security_headers`) is the
//! strict `script-src 'self'` policy that forbids inline `<script>`
//! blocks. The dashboard ships its JavaScript inline (one file
//! covers all 18 sections), so the `/dashboard` route stamps its
//! own CSP that adds `'unsafe-inline'` to `script-src` and
//! `style-src`. The security-headers middleware in
//! `crate::security_headers` preserves a per-handler CSP rather
//! than overwriting it, so the strict default still applies to
//! every other route.
//!
//! ## Operator override
//!
//! `RELIX_DASHBOARD_PATH=/some/file.html` lets operators hot-swap
//! the dashboard at process start without rebuilding the binary.
//! A missing or unreadable file logs a warning and falls back to
//! the embedded copy. Resolved once per process via [`OnceLock`].

use std::sync::OnceLock;

use axum::{
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};

/// `X-Content-Type-Options` is not in `axum::http::header` re-export.
/// Defined as a constant here so the value is reused without a
/// per-request allocation.
fn xcto_header_name() -> HeaderName {
    HeaderName::from_static("x-content-type-options")
}

/// Embedded HTML. The build-time `include_str!` pulls the file in
/// from this crate's `src/` so the shipped binary needs no extra
/// resource directory.
const DASHBOARD_HTML_EMBEDDED: &str = include_str!("dashboard.html");

/// Per-route Content-Security-Policy stamped on `/dashboard`. Adds
/// `'unsafe-inline'` to `script-src` and `style-src` because the
/// dashboard ships its JS + CSS inline. The bridge's default strict
/// CSP (see `crate::security_headers`) still applies to every other
/// route; the security-headers middleware only stamps the default
/// when a handler has not already set one.
const DASHBOARD_CSP: &str = "default-src 'self'; \
                             script-src 'self' 'unsafe-inline'; \
                             style-src 'self' 'unsafe-inline'; \
                             img-src 'self' data:; \
                             connect-src 'self'";

fn dashboard_html_source() -> &'static str {
    static SOURCE: OnceLock<String> = OnceLock::new();
    SOURCE
        .get_or_init(|| {
            let env_value = std::env::var("RELIX_DASHBOARD_PATH").ok();
            resolve_dashboard_source(env_value.as_deref())
        })
        .as_str()
}

/// Pure resolver — takes the env-var value as an `Option<&str>` so
/// tests can exercise both the file-found and file-missing
/// branches without mutating process env.
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

/// `GET /dashboard` — operator dashboard HTML. Sets the per-route
/// CSP, the spec'd `X-Frame-Options: DENY`,
/// `X-Content-Type-Options: nosniff`, and `Referrer-Policy:
/// no-referrer` so the dashboard's data flow cannot leak to
/// cross-origin contexts.
pub async fn page() -> Response {
    let html = dashboard_html_source();
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DASHBOARD_CSP),
        )
        .header(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"))
        .header(xcto_header_name(), HeaderValue::from_static("nosniff"))
        .header(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        )
        .body(html.to_string())
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

/// Resolve the built dashboard SPA bundle directory. The real app is a
/// Vite + React + TypeScript project under `apps/dashboard`; its
/// `npm run build` emits to `crates/relix-web-bridge/dashboard-dist`,
/// which is what this serves. Operators can override the location with
/// `RELIX_DASHBOARD_DIST`. Returns `None` when no built bundle is
/// present (a source-only checkout that hasn't run the frontend build),
/// in which case the bridge falls back to the legacy single-file page.
pub fn resolve_spa_dir() -> Option<std::path::PathBuf> {
    let has_index = |p: &std::path::Path| p.join("index.html").is_file();
    if let Ok(p) = std::env::var("RELIX_DASHBOARD_DIST") {
        let pb = std::path::PathBuf::from(p);
        if has_index(&pb) {
            return Some(pb);
        }
        tracing::warn!(path = %pb.display(), "dashboard: RELIX_DASHBOARD_DIST has no index.html");
    }
    let default = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard-dist");
    if has_index(&default) {
        Some(default)
    } else {
        None
    }
}

/// Build the `/dashboard` router: the real React SPA when a built bundle
/// exists (served as static assets with an SPA history fallback to
/// `index.html`), otherwise the legacy single-file HTML page so a
/// source-only checkout still has a working operator console.
///
/// The SPA is built with Vite `base: '/dashboard/'`, so its asset URLs
/// are absolute (`/dashboard/assets/…`) and load cleanly under the
/// bridge's strict default CSP (`script-src 'self'`, no inline scripts).
pub fn dashboard_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use axum::routing::get;
    match resolve_spa_dir() {
        Some(dir) => {
            tracing::info!(path = %dir.display(), "dashboard: serving built SPA bundle");
            let index = dir.join("index.html");
            let serve = tower_http::services::ServeDir::new(&dir)
                .append_index_html_on_directories(true)
                .fallback(tower_http::services::ServeFile::new(index));
            axum::Router::new().nest_service("/dashboard", serve)
        }
        None => {
            tracing::info!("dashboard: no built SPA bundle found — serving legacy HTML page");
            axum::Router::new().route("/dashboard", get(page))
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
    async fn page_stamps_per_route_csp_with_unsafe_inline_for_scripts() {
        let resp = page().await.into_response();
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            csp.contains("default-src 'self'"),
            "csp missing default-src: {csp:?}"
        );
        assert!(
            csp.contains("script-src 'self' 'unsafe-inline'"),
            "csp missing inline script allowance: {csp:?}"
        );
        assert!(
            csp.contains("style-src 'self' 'unsafe-inline'"),
            "csp missing inline style allowance: {csp:?}"
        );
        assert!(
            csp.contains("img-src 'self' data:"),
            "csp missing data: img: {csp:?}"
        );
        assert!(
            csp.contains("connect-src 'self'"),
            "csp missing connect-src: {csp:?}"
        );
    }

    #[tokio::test]
    async fn page_stamps_xframe_xcto_and_referrer_policy_headers() {
        let resp = page().await.into_response();
        let xfo = resp
            .headers()
            .get(header::X_FRAME_OPTIONS)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(xfo, "DENY");
        let xcto = resp
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(xcto, "nosniff");
        let rp = resp
            .headers()
            .get(header::REFERRER_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(rp, "no-referrer");
    }

    /// Every spec'd dashboard section must register a top-level
    /// `<section id="section-<name>">` so navigation can show it
    /// and operator scripts can deep-link. A future edit that
    /// drops one of these IDs fails this gate fast.
    #[tokio::test]
    async fn page_body_contains_all_eighteen_section_ids() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for sec in [
            "overview",
            "chat",
            "memory",
            "approvals",
            "skills",
            "sessions",
            "reasoning",
            "credentials",
            "identity",
            "cost",
            "observability",
            "tenant",
            "planning",
            "workflows",
            "email",
            "plugins",
            "config",
            "logs",
        ] {
            assert!(
                body.contains(&format!("id=\"section-{sec}\"")),
                "missing section id for {sec}"
            );
        }
    }

    /// RELA-31: the four endpoint-group panels (tasks, cron,
    /// policy denials, mcp) must each register a `<section>` and
    /// a sidebar nav entry so they are reachable from navigation
    /// and not dead code. Asserts both the section landmark and
    /// the `data-nav` routing marker the SECTIONS array emits.
    #[tokio::test]
    async fn page_body_contains_rela31_panel_section_and_nav_ids() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for sec in ["tasks", "cron", "denials", "mcp"] {
            assert!(
                body.contains(&format!("id=\"section-{sec}\"")),
                "missing section id for new panel {sec}"
            );
            assert!(
                body.contains(&format!("data-section=\"{sec}\"")),
                "missing data-section marker for new panel {sec}"
            );
        }
        // Each panel must reference its real backend endpoint
        // group (the call path is built inline, so assert on the
        // endpoint string rather than a specific call expression).
        for needle in [
            "/v1/tasks?limit=",
            "/v1/cron/jobs",
            "/v1/policy/denials",
            "/v1/mcp/servers",
        ] {
            assert!(
                body.contains(needle),
                "new panel is not wired to its real endpoint: {needle}"
            );
        }
    }

    /// The dashboard ships everything inline — there must be no
    /// `<script src=>` or `<link href=>` pointing at an external
    /// resource (the strict CSP would block any such load anyway,
    /// but this catches the regression at the HTML level too).
    #[tokio::test]
    async fn page_has_no_external_script_or_style_loads() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for needle in [
            "src=\"https://",
            "src='https://",
            "src=\"http://",
            "src='http://",
            "src=\"//",
            "src='//",
            "src=\"/assets/dashboard.js",
            "@import",
        ] {
            assert!(
                !body.contains(needle),
                "dashboard pulled in external resource via `{needle}`"
            );
        }
    }

    /// The dashboard's JavaScript must never use `innerHTML` with
    /// dynamic data. The safe DOM builder (`el(...)`) and direct
    /// `textContent` writes are the only allowed paths.
    /// `outerHTML` is allowed in property names referenced by
    /// devtools, but no `node.innerHTML =` or `.innerHTML +=`
    /// assignment should appear in the source.
    #[tokio::test]
    async fn page_javascript_has_no_innerhtml_assignment() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for needle in [
            ".innerHTML =",
            ".innerHTML=",
            ".innerHTML +=",
            ".innerHTML+=",
        ] {
            assert!(
                !body.contains(needle),
                "dashboard JS contains forbidden innerHTML assignment: {needle:?}"
            );
        }
    }

    /// The SVG cost chart helper must be present and reachable
    /// from the cost panel. Sanity-check the source string.
    #[tokio::test]
    async fn page_carries_svg_chart_helper() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("function horizontalBars"),
            "missing horizontalBars helper"
        );
        assert!(
            body.contains("function lineChart"),
            "missing lineChart helper"
        );
        assert!(
            body.contains("svgEl('rect'"),
            "horizontalBars must build rect elements"
        );
    }

    /// Sidebar + 18-section nav landmarks must be present in the
    /// shipped page (operator-facing brand strings and nav data
    /// attributes the JS uses for routing).
    #[tokio::test]
    async fn page_landmarks_brand_and_nav() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("<title>Relix"), "missing title");
        assert!(
            body.contains("Agent Control Plane"),
            "missing brand subtitle"
        );
        assert!(body.contains("id=\"sidebar\""), "missing sidebar shell");
        assert!(
            body.contains("id=\"theme-toggle\""),
            "missing dark-mode toggle"
        );
        assert!(
            body.contains("data-section=\"overview\""),
            "missing nav routing data"
        );
    }

    /// Paperclip-inspired product shell guard: the embedded dashboard
    /// must ship as an actual control-plane surface, not a flat dump of
    /// unrelated panels. These landmarks keep the grouped navigation,
    /// topbar context, and bridge/spine status wiring intact.
    #[tokio::test]
    async fn page_carries_grouped_control_plane_shell() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        for needle in [
            "class=\"brand-mark\"",
            "id=\"dashboard-topbar\"",
            "id=\"topbar-section\"",
            "id=\"topbar-spine\"",
            "id=\"topbar-bridge\"",
            "class: 'nav-group-label'",
            "data-nav-group",
            "function updateTopbar(",
            "function updateSpineTopbar(",
            "function setStatusPill(",
        ] {
            assert!(
                body.contains(needle),
                "missing dashboard shell marker {needle:?}"
            );
        }
        for group in ["Work", "Agent Runtime", "Operations", "System"] {
            assert!(body.contains(group), "missing nav group {group}");
        }
    }

    /// Dark-mode toggle wires through to the `<html>` root via
    /// `setAttribute('data-theme', ...)` — operator tests assert
    /// that lookup string is present.
    #[tokio::test]
    async fn page_dark_mode_toggle_targets_data_theme_attribute() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("setAttribute('data-theme'"),
            "dashboard JS does not toggle data-theme"
        );
        assert!(
            body.contains("getElementById('theme-toggle')"),
            "dashboard JS does not bind the theme-toggle button"
        );
    }

    /// Logs section uses EventSource on /v1/logs/stream and the
    /// bridge installs that route. The endpoint is asserted by
    /// `logs.rs` tests; here we just confirm the dashboard wires
    /// to the documented path.
    #[tokio::test]
    async fn page_logs_section_subscribes_to_v1_logs_stream() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("new EventSource('/v1/logs/stream'"),
            "logs section is not subscribed to /v1/logs/stream"
        );
    }

    // ─────────────────────────────────────────────────────
    // P4 — dashboard auth-error contract tests
    // ─────────────────────────────────────────────────────

    /// Product-spine hook: the dashboard must consume the
    /// server-side control-plane manifest instead of carrying an
    /// unrelated, permanent copy of product surfaces. This does not
    /// decompose the monolith yet; it establishes the contract the
    /// split dashboard modules will hang from.
    #[tokio::test]
    async fn page_consumes_control_plane_dashboard_manifest() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("/v1/control-plane/dashboard"),
            "dashboard does not fetch the control-plane dashboard manifest"
        );
        assert!(
            body.contains("function applyDashboardManifest"),
            "dashboard has no manifest application hook"
        );
        assert!(
            body.contains("data-spine-id"),
            "dashboard nav does not expose spine ids"
        );
        assert!(
            body.contains("data-spine-status"),
            "dashboard nav does not expose spine status"
        );
    }

    /// Phase 6 — the manifest must drive a VISIBLE spine-status badge
    /// on each nav item, not just a hover tooltip, so navigation
    /// reflects the real product contract. Assert the rendering hook,
    /// the severity mapping, and the badge CSS are all present.
    #[tokio::test]
    async fn page_renders_visible_spine_status_badge_from_manifest() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("function setNavSpineBadge("),
            "missing visible nav badge renderer"
        );
        assert!(
            body.contains("function spineBadgeRank("),
            "missing spine status severity mapping"
        );
        // The renderer is actually invoked from manifest application.
        assert!(
            body.contains("setNavSpineBadge(item"),
            "applyDashboardManifest does not render the visible badge"
        );
        // The badge has styling for all three severities.
        for cls in [
            ".nav-spine-badge.ok",
            ".nav-spine-badge.warn",
            ".nav-spine-badge.err",
        ] {
            assert!(body.contains(cls), "missing badge style {cls}");
        }
        // Built via the safe DOM builder, never innerHTML (covered by
        // page_javascript_has_no_innerhtml_assignment too).
        assert!(
            body.contains("class: 'nav-spine-badge '"),
            "badge is not built with the safe el() builder"
        );
    }

    /// Dashboard bootstrap no longer replaces the entire console
    /// with a setup-token wall. When `/v1/auth/token` is absent or
    /// refuses the request, the shell still initializes and surfaces
    /// the auth state as a status pill/panel-level request failures.
    #[tokio::test]
    async fn page_bootstrap_continues_when_auth_token_endpoint_refuses() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // The bootstrap function MUST set authFailed = true on
        // both 401/403 responses and on transport errors.
        assert!(
            body.contains("self.authFailed = true"),
            "bootstrap missing authFailed flag flip"
        );
        assert!(
            body.contains("if (Bridge.authFailed)"),
            "initApp does not inspect Bridge.authFailed"
        );
        assert!(
            body.contains("setStatusPill('topbar-bridge', 'Bridge auth not loaded', 'warn')"),
            "auth failure is not surfaced in the dashboard shell"
        );
        assert!(
            !body.contains("Authentication Required"),
            "dashboard still carries a full-page auth wall"
        );
    }

    /// P4 test: "Dashboard with an invalid token shows the
    /// auth error screen after one retry." The bootstrap code
    /// path must explicitly retry exactly once with a 2s
    /// delay before giving up.
    #[tokio::test]
    async fn page_bootstrap_retries_once_with_two_second_delay() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // 2000ms delay before retry.
        assert!(
            body.contains("setTimeout(resolve, 2000)"),
            "bootstrap retry delay is not 2000ms"
        );
        // The retry calls the same fetchOnce helper a second
        // time and bails after that — search for the helper +
        // its `.then(fetchOnce)` chain.
        assert!(
            body.contains("function fetchOnce()") && body.contains(".then(fetchOnce)"),
            "bootstrap does not retry via fetchOnce"
        );
    }

    /// P4 test: "Dashboard with a valid token proceeds
    /// normally." The bootstrap path stores the token in
    /// sessionStorage AND lets initApp wire its handlers.
    #[tokio::test]
    async fn page_bootstrap_proceeds_when_auth_token_endpoint_returns_token() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Cached token in sessionStorage round-trips.
        assert!(
            body.contains("sessionStorage.setItem('relix-bridge-token'"),
            "bootstrap missing cache write"
        );
        // The success branch reaches startSessionExpiryProbe.
        assert!(
            body.contains("startSessionExpiryProbe()"),
            "bootstrap success path does not start the session probe"
        );
    }

    /// P4 test: "Dashboard session that expires marks the
    /// bridge status stale." The probe interval is 5 minutes
    /// and it checks /v1/health for 401/403 without replacing
    /// the page.
    #[tokio::test]
    async fn page_session_expiry_probe_calls_health_every_five_minutes() {
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // Probe function + 5 min interval expression.
        assert!(
            body.contains("function startSessionExpiryProbe()"),
            "session probe function missing"
        );
        assert!(
            body.contains("5 * 60 * 1000"),
            "session probe interval is not 5 minutes (5 * 60 * 1000 ms)"
        );
        // Probe target is /v1/health.
        assert!(
            body.contains("fetch('/v1/health'"),
            "session probe does not call /v1/health"
        );
        // 401/403 from the probe MUST mark the bridge status.
        assert!(
            body.contains("r.status === 401 || r.status === 403"),
            "session probe does not check 401/403"
        );
        assert!(
            body.contains("Bridge session expired"),
            "session expiry is not surfaced in the shell"
        );
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
        let bogus = std::env::temp_dir().join("relix_dashboard_does_not_exist_zzz.html");
        let _ = std::fs::remove_file(&bogus);
        let out = resolve_dashboard_source(Some(bogus.to_str().unwrap()));
        assert_eq!(out, DASHBOARD_HTML_EMBEDDED);
    }

    /// When the React SPA bundle is present (committed under the crate's
    /// `dashboard-dist`), `dashboard_router` serves `index.html` at
    /// `/dashboard/`. When it is absent the router falls back to the
    /// legacy single-file page at `/dashboard`. This test asserts
    /// whichever applies in this checkout, so the serving wiring is
    /// covered in both modes.
    #[tokio::test]
    async fn dashboard_router_serves_dashboard() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app: axum::Router<()> = dashboard_router();
        if resolve_spa_dir().is_some() {
            // SPA mode: the bundle is served at /dashboard/ as text/html.
            let resp = app
                .oneshot(Request::builder().uri("/dashboard/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let ctype = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(ctype.starts_with("text/html"), "spa index content-type: {ctype:?}");
        } else {
            // Legacy mode: the embedded HTML page is served at /dashboard.
            let resp = app
                .oneshot(Request::builder().uri("/dashboard").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }
}
