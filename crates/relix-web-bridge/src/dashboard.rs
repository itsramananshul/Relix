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
    async fn page_streams_inspection_landmarks_present() {
        // M41: clicking the live-streams KPI tile opens a
        // per-stream inspection panel. The tile must be
        // marked clickable + the panel-render function must
        // ship.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"id="kpi-streams-tile""#),
            "live-streams KPI tile should be a click target"
        );
        assert!(
            body.contains("renderStreamsDetail"),
            "page should ship renderStreamsDetail()"
        );
        assert!(
            body.contains(r#"id="streams-detail-host""#),
            "page should ship the streams-detail host element"
        );
    }

    #[tokio::test]
    async fn page_retry_storm_landmarks_present() {
        // M40: anomaly banner adds a retry-storm signal sourced
        // from /v1/tasks/edges/recent. Topology page gets a
        // Recent retry chains card. Both must show up in the
        // rendered HTML.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // backgroundSync now fetches the edges aggregate.
        assert!(
            body.contains("/v1/tasks/edges/recent"),
            "backgroundSync should fetch /v1/tasks/edges/recent"
        );
        // Anomaly computation gains the retry-count signal.
        assert!(
            body.contains("retry_count_15min"),
            "computeAnomalies should track retry_count_15min"
        );
        // Topology page hosts the Recent retry chains card.
        assert!(
            body.contains(r#"id="cross-edges-host""#),
            "topology page should host the cross-edges card"
        );
        assert!(
            body.contains("Recent retry chains"),
            "topology page should label the cross-edges card"
        );
    }

    #[tokio::test]
    async fn page_edge_anchor_navigation_present() {
        // M38c: chain gaps render the retried_from edge anchor
        // (← evt N) when one is recorded; click jumps to the
        // triggering event in the timeline. data-jump-to-event
        // is the routing attribute.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("data-jump-to-event"),
            "page should ship data-jump-to-event for edge anchors"
        );
        assert!(
            body.contains("retryEdgeByAttempt"),
            "page should ship the edge-by-attempt index"
        );
        // Edges-not-recorded fallback must label itself honestly.
        assert!(
            body.contains("edge not recorded"),
            "older tasks without instrumentation should be labeled honestly"
        );
        // Timeline rows should carry data-event-id for precise scroll-to.
        assert!(
            body.contains("data-event-id"),
            "timeline rows should carry data-event-id"
        );
    }

    #[tokio::test]
    async fn page_peer_link_navigation_present() {
        // M37: timeline + execution path panel render peer
        // aliases as clickable links into the topology peer
        // drawer. data-peer-alias is the routing attribute.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("renderTimelinePayload"),
            "page should ship renderTimelinePayload()"
        );
        assert!(
            body.contains("data-peer-alias"),
            "page should attach data-peer-alias for click-to-drawer"
        );
        assert!(
            body.contains("class=\"peer-link\"") || body.contains("peer-link"),
            "page should ship the peer-link CSS class"
        );
    }

    #[tokio::test]
    async fn page_attempt_filter_present() {
        // M36: chain pills carry data-attempt-filter so clicking
        // one filters the timeline to that attempt's events.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("data-attempt-filter"),
            "page should ship data-attempt-filter on chain pills"
        );
        assert!(
            body.contains("clear-attempt-filter"),
            "page should ship the clear-filter affordance"
        );
        assert!(
            body.contains("attemptFilter"),
            "page should hold attemptFilter state"
        );
    }

    #[tokio::test]
    async fn page_execution_path_panel_present() {
        // M33: task detail surfaces an Execution path panel
        // when the chronicle has capability.invoked events.
        // Pairs each method with the bridge's current
        // routing target, honestly labeled.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("maybeFetchExecutionPath"),
            "page should ship maybeFetchExecutionPath()"
        );
        assert!(
            body.contains("renderExecutionPath"),
            "page should ship renderExecutionPath()"
        );
        assert!(
            body.contains("/v1/routing"),
            "page should fetch /v1/routing to derive execution path"
        );
        // The "not recorded yet" framing is load-bearing for
        // the Phase-1D honesty contract. If a future change
        // drops it, this test fails.
        assert!(
            body.contains("not recorded yet"),
            "execution path must honestly label the per-call-routing gap"
        );
        // Parser landmark — catches accidental removal of the
        // payload parser when the runtime adds the peer field.
        assert!(
            body.contains("parseCapabilityInvokedPayload"),
            "page should ship the capability.invoked payload parser"
        );
        // M35: model "not recorded yet" label for ai.chat
        // rows — load-bearing for the honesty contract.
        assert!(
            body.contains("model: not recorded yet"),
            "execution path must label the model-not-recorded gap explicitly"
        );
    }

    #[tokio::test]
    async fn page_anomaly_banner_present() {
        // M30: overview surfaces runtime anomaly counts
        // (peer flips, task failures, expired peers) when
        // signals exceed quiet thresholds.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("computeAnomalies"),
            "page should ship computeAnomalies()"
        );
        assert!(
            body.contains(r#"id="anomaly-banner""#),
            "page should ship the anomaly banner element"
        );
        assert!(
            body.contains("runtime anomalies"),
            "anomaly banner should label itself"
        );
    }

    #[tokio::test]
    async fn page_failure_panel_present() {
        // M29: failed/interrupted/cancelled tasks show a
        // failure breakdown panel with class + cause +
        // class-specific operator suggestion.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("renderFailurePanel"),
            "page should ship renderFailurePanel()"
        );
        // Every canonical failure class must have a suggestion
        // string in the mapping. Catches a future class added
        // without its operator guidance.
        for class in [
            "transient",
            "timeout",
            "unavailable",
            "policy_denied",
            "invalid_args",
            "permanent",
        ] {
            assert!(
                body.contains(&format!(r#"{class}:"#)),
                "failure suggestion missing for class {class}"
            );
        }
    }

    #[tokio::test]
    async fn page_topology_correlation_present() {
        // M28: failed / interrupted tasks surface a
        // "Topology events near this task" correlation
        // panel. Explicitly labeled as correlation, not
        // causation.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("maybeFetchTopologyCorrelation"),
            "page should ship maybeFetchTopologyCorrelation()"
        );
        assert!(
            body.contains("Correlation, not causation"),
            "correlation panel must label itself honestly"
        );
        assert!(
            body.contains(r#"id="correlation-slot""#),
            "page should ship the correlation slot element"
        );
    }

    #[tokio::test]
    async fn page_xref_panel_renderer_present() {
        // M27: task detail surfaces a Cross-references panel
        // with the IDs operators need to drill into per-flow
        // event logs and the audit chain.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("renderXrefPanel"),
            "page should ship renderXrefPanel()"
        );
        assert!(
            body.contains("Cross-references"),
            "page should expose a Cross-references panel"
        );
        // The CLI command template names should appear so a
        // future rename of relix-flow-inspect catches at test
        // time.
        assert!(
            body.contains("relix-flow-inspect"),
            "xref panel should suggest the relix-flow-inspect CLI"
        );
    }

    #[tokio::test]
    async fn page_retry_chain_renderer_present() {
        // M26 (Phase-1C causality): task detail renders a
        // retry-chain visualization derived from real
        // task_attempts rows.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("renderRetryChain"),
            "page should ship renderRetryChain()"
        );
        assert!(
            body.contains("chain-pill"),
            "page should ship the chain-pill CSS class"
        );
        assert!(
            body.contains("chain-gap"),
            "page should ship the inter-attempt gap marker"
        );
    }

    #[tokio::test]
    async fn page_timeline_renderer_present() {
        // M20: task detail panel renders chronicle events as a
        // visual timeline (default) with a toggle to raw view.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        // The rendering functions are inline in the page JS;
        // their names + CSS class hooks are the landmarks.
        assert!(
            body.contains("renderTimeline"),
            "page should ship renderTimeline()"
        );
        assert!(
            body.contains("timeline-marker"),
            "page should ship timeline-marker CSS class"
        );
        assert!(
            body.contains("timelineMarkerClass"),
            "page should ship the per-event-class marker mapper"
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
    async fn page_topology_lifecycle_events_consumed() {
        // M23 + M24: topology page fetches the server-side
        // lifecycle event log and renders it as a Recent
        // transitions card.
        let resp = page().await.into_response();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains("/v1/topology/events"),
            "topology page should fetch /v1/topology/events"
        );
        assert!(
            body.contains(r#"id="lifecycle-host""#),
            "topology page should ship a lifecycle events host"
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
