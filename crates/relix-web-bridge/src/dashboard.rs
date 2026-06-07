//! Operator dashboard served at `/dashboard`.
//!
//! The dashboard is the React SPA under `apps/dashboard`. Its
//! `npm run build` emits a static bundle to
//! `crates/relix-web-bridge/dashboard-dist` (committed to the repo), and
//! this module serves that bundle at `/dashboard` (with the SPA history
//! fallback to `index.html` and assets under `/dashboard/assets/*`).
//!
//! Phase 2 Slice 3: the old single-file `dashboard.html` console is RETIRED.
//! There is no legacy HTML fallback any more — the React dist is the only
//! supported dashboard surface. If the bundle is missing (a source-only
//! checkout that never ran the frontend build), `/dashboard` returns an
//! honest "the dashboard bundle is not built" operator notice (HTTP 503)
//! that tells the operator to run the build. That notice deliberately does
//! NOT pretend to be the dashboard.
//!
//! The SPA is built with Vite `base: '/dashboard/'`, so its asset URLs are
//! absolute (`/dashboard/assets/…`) and load cleanly under the bridge's
//! strict default CSP (`script-src 'self'`, no inline scripts).

use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};

/// Resolve the built dashboard SPA bundle directory. The real app is a
/// Vite + React + TypeScript project under `apps/dashboard`; its
/// `npm run build` emits to `crates/relix-web-bridge/dashboard-dist`,
/// which is what this serves. Operators can override the location with
/// `RELIX_DASHBOARD_DIST`. Returns `None` when no built bundle is present
/// (a source-only checkout that hasn't run the frontend build) — in which
/// case `/dashboard` serves the honest missing-bundle notice, NOT a legacy
/// dashboard.
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

/// Honest operator notice served at `/dashboard` when no React bundle is
/// present. Returns HTTP 503 with a tiny plain page telling the operator to
/// build the dashboard. It is intentionally NOT a dashboard — there is no
/// app shell, nav, or fake data — so a missing build reads as a clear
/// build/operator error, not a working (but old) product.
pub async fn missing_bundle_notice() -> Response {
    const BODY: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Relix \u{2014} dashboard not built</title></head><body>\
<h1>Dashboard bundle not found</h1>\
<p>The Relix operator dashboard is a React app that must be built before the \
web-bridge can serve it. This is a build/setup step, not a product error.</p>\
<p>Build it, then reload:</p>\
<pre>cd apps/dashboard\nnpm install\nnpm run build</pre>\
<p>That emits <code>crates/relix-web-bridge/dashboard-dist/</code>, which the \
<code>/dashboard</code> route serves. Set <code>RELIX_DASHBOARD_DIST</code> to \
point at a bundle in another location.</p>\
<p>The bridge API at <code>/v1/*</code> is unaffected by this.</p>\
</body></html>";
    match Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(BODY.to_string())
    {
        Ok(r) => r.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "dashboard: missing-bundle notice builder failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "dashboard bundle not built — run `npm run build` in apps/dashboard",
            )
                .into_response()
        }
    }
}

/// Build the `/dashboard` router: the React SPA bundle when present (served
/// as static assets with an SPA history fallback to `index.html`), otherwise
/// the honest missing-bundle notice. There is no legacy single-file
/// dashboard fallback (retired in Phase 2 Slice 3).
pub fn dashboard_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    use axum::routing::get;
    match resolve_spa_dir() {
        Some(dir) => {
            tracing::info!(path = %dir.display(), "dashboard: serving built React SPA bundle");
            let index = dir.join("index.html");
            let serve = tower_http::services::ServeDir::new(&dir)
                .append_index_html_on_directories(true)
                .fallback(tower_http::services::ServeFile::new(index));
            axum::Router::new().nest_service("/dashboard", serve)
        }
        None => {
            tracing::warn!(
                "dashboard: no React bundle found (run `npm run build` in apps/dashboard) — \
                 serving the missing-bundle notice"
            );
            axum::Router::new().route("/dashboard", get(missing_bundle_notice))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    /// The missing-bundle notice is an honest 503 operator/build error — NOT
    /// a dashboard. It must say how to build, and must NOT carry the React
    /// app's mount point or asset bundle (so it can never be mistaken for the
    /// real product).
    #[tokio::test]
    async fn missing_bundle_notice_is_honest_503_not_a_dashboard() {
        let resp = missing_bundle_notice().await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ctype.starts_with("text/html"), "ctype was {ctype:?}");
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("npm run build"),
            "notice must tell the operator how to build"
        );
        assert!(
            body.contains("dashboard-dist"),
            "notice should name the dist directory"
        );
        // It is NOT the React app shell, and pulls in no app bundle.
        assert!(
            !body.contains("id=\"root\""),
            "notice must not be the React app shell"
        );
        assert!(
            !body.contains("/dashboard/assets/"),
            "notice must not load the app bundle"
        );
    }

    /// Phase 2 Slice 3 — generated-dist parity guard. The committed React
    /// bundle must be present (so `/dashboard` serves React, NOT the
    /// missing-bundle notice) AND its `index.html` must reference only assets
    /// that actually exist in the bundle. This catches the classic dist-drift
    /// bug — `index.html` pointing at a stale hashed bundle after a forgotten
    /// `npm run build` — at test time instead of as a blank page in prod.
    #[test]
    fn committed_react_dist_present_and_index_references_existing_assets() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard-dist");
        let index = dir.join("index.html");
        assert!(
            index.is_file(),
            "committed React dist missing index.html at {} — run `npm run build` in apps/dashboard",
            index.display()
        );
        let html = std::fs::read_to_string(&index).expect("read dist index.html");
        // It must be the React shell (Vite mounts into #root).
        assert!(
            html.contains("id=\"root\""),
            "dist index.html is not the React shell"
        );
        // Every `/dashboard/assets/<file>` reference must resolve to a real
        // file in the bundle.
        let needle = "/dashboard/assets/";
        let mut rest = html.as_str();
        let mut checked = 0;
        while let Some(i) = rest.find(needle) {
            rest = &rest[i + needle.len()..];
            let end = rest.find(['"', '\'']).unwrap_or(rest.len());
            let asset = &rest[..end];
            assert!(
                !asset.is_empty(),
                "empty asset reference in dist index.html"
            );
            assert!(
                dir.join("assets").join(asset).is_file(),
                "index.html references a missing bundle asset `{asset}` — rebuild apps/dashboard so dashboard-dist is in sync"
            );
            checked += 1;
            rest = &rest[end..];
        }
        assert!(
            checked >= 1,
            "dist index.html referenced no /dashboard/assets/* bundle — the build looks wrong"
        );
        // The resolver must pick this committed bundle up, so the bridge
        // serves React at /dashboard (not the missing-bundle notice).
        assert!(
            resolve_spa_dir().is_some(),
            "resolve_spa_dir() should find the committed React bundle"
        );
    }

    /// In this repo the committed React bundle is present, so
    /// `dashboard_router` serves `index.html` (text/html) at `/dashboard/`
    /// AND the hashed assets at `/dashboard/assets/*`. Proves the real SPA
    /// serving wiring end to end (no legacy fallback path remains).
    #[tokio::test]
    async fn dashboard_router_serves_react_spa_and_assets() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        assert!(
            resolve_spa_dir().is_some(),
            "this repo must ship the committed React bundle"
        );

        // /dashboard/ → the SPA index (text/html).
        let app: axum::Router<()> = dashboard_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/dashboard/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ctype.starts_with("text/html"),
            "spa index content-type: {ctype:?}"
        );

        // /dashboard/assets/<hashed bundle> → 200. Resolve the asset name
        // from the committed index.html so the test tracks the real build.
        let dir = resolve_spa_dir().unwrap();
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        let needle = "/dashboard/assets/";
        let i = html.find(needle).expect("index.html references an asset");
        let after = &html[i + needle.len()..];
        let end = after.find(['"', '\'']).unwrap();
        let asset = &after[..end];
        let app2: axum::Router<()> = dashboard_router();
        let resp2 = app2
            .oneshot(
                Request::builder()
                    .uri(format!("/dashboard/assets/{asset}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp2.status(),
            StatusCode::OK,
            "asset {asset} should serve 200"
        );
    }
}
