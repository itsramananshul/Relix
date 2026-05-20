//! `GET /v1/topology` — operator's view of the mesh as a set of
//! peers + freshness.
//!
//! Read-only projection of the bridge's `ManifestCache`. One row
//! per cached peer; capability detail still lives at
//! `/v1/capabilities` — this surface intentionally compresses to
//! per-node aggregates so operators can answer "which peers are
//! up, when were they last seen, what do they offer at a glance"
//! in one round-trip.
//!
//! Multi-node operational realism: the bridge does NOT actively
//! probe peers here. The `last_refreshed_at` field reflects the
//! most recent SUCCESSFUL `node.manifest` round-trip from the A.4
//! 60s background refresh. A stale timestamp is the signal that
//! the peer's refresh loop has been silently failing — operators
//! who see `last_refreshed_secs_ago > ~120` know the peer is
//! degraded even though cached capabilities may still route.
//!
//! Architectural note: this surface is purely read-only and
//! exposes mesh state that is already visible to any peer via
//! `node.manifest`. The bridge stays translation/presentation
//! only — no new orchestration, no scheduler.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use serde::Serialize;

use crate::config::AppState;

/// One row of `/v1/topology` — one cached peer with the
/// aggregates an operator cares about.
#[derive(Debug, Serialize)]
pub struct PeerView {
    /// Operator-configured alias (`memory`, `ai`, `tool`,
    /// `coordinator`, …). `None` when the peer was added
    /// without an alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Hex-encoded `NodeId`.
    pub node_id: String,
    /// Peer-advertised `node_type` discriminator.
    pub node_type: String,
    /// Peer-advertised `node_name` (operator-set label,
    /// distinct from `alias` — alias is local-only,
    /// `node_name` is what the peer calls itself).
    pub node_name: String,
    /// Schema version of the peer's manifest format.
    pub manifest_version: u64,
    /// Number of capabilities advertised.
    pub capability_count: usize,
    /// Method names of every capability advertised, sorted
    /// alphabetically. Compact enough that "which peer
    /// serves what" is one round-trip.
    pub methods: Vec<String>,
    /// Wall-clock unix seconds of the most recent
    /// successful `node.manifest` refresh from this peer.
    pub last_refreshed_at: i64,
    /// Convenience: `now - last_refreshed_at`. Operators
    /// look at this to spot stale peers without
    /// arithmetic.
    pub last_refreshed_secs_ago: i64,
    /// Best-effort freshness verdict for at-a-glance
    /// dashboards. `fresh` (<120s) / `stale` (<600s) /
    /// `expired` (>=600s). The bridge does not act on
    /// this — it's pure presentation, kept consistent
    /// with the manifest-refresh period (60s) so operators
    /// see "stale" if even one refresh tick was missed.
    pub freshness: &'static str,
}

#[derive(Debug, Serialize)]
pub struct TopologyResponse {
    /// Sorted alphabetically by alias (peers without alias
    /// sort last). Stable ordering so dashboards diff
    /// cleanly across refreshes.
    pub peers: Vec<PeerView>,
    /// Wall-clock unix seconds at which the bridge built
    /// this response.
    pub generated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        Json(self).into_response()
    }
}

/// Compact health-summary returned by `GET /v1/health`. Less
/// detail than `/v1/topology` (no per-peer rows), but adds bridge
/// uptime + cross-mesh reconnect counters that operators want at
/// the top of every dashboard. Plain `/health` (text "ok\n") is
/// preserved for liveness probes.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Always `"ok"` when this endpoint responds at all (the
    /// bridge is process-up by definition). Distinguishes
    /// `/v1/health` body from `/health` text for tooling.
    pub status: &'static str,
    /// Wall-clock unix seconds the bridge process started.
    pub started_at: i64,
    /// Wall-clock unix seconds at which the response was built.
    pub now: i64,
    /// `now - started_at`. Convenience for dashboards.
    pub uptime_secs: i64,
    /// `true` when the bridge's `[coordinator]` alias is set
    /// AND the mesh client is up. `false` ⇒ `task.*` endpoints
    /// return 503; chat continues fail-soft.
    pub coordinator_configured: bool,
    /// Total peers in the manifest cache.
    pub peer_count: usize,
    /// Peers broken down by freshness bucket (same bucketing
    /// as `/v1/topology`).
    pub peers_fresh: usize,
    pub peers_stale: usize,
    pub peers_expired: usize,
    /// Cross-mesh reconnect telemetry from `MeshClient`. `None`
    /// when the bridge has no MeshClient (discovery never
    /// succeeded). `attempts - successes > 0` is the flapping
    /// signal — a peer keeps disconnecting + reconnecting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconnect: Option<ReconnectCounters>,
    /// Bridge-process-local SSE stream metrics. Active count
    /// plus total opened since bridge start. Counters reset
    /// on restart.
    pub streams: StreamCounters,
}

#[derive(Debug, Serialize)]
pub struct ReconnectCounters {
    pub attempts: u64,
    pub successes: u64,
}

#[derive(Debug, Serialize)]
pub struct StreamCounters {
    pub active: u64,
    pub opened_total: u64,
}

/// `GET /v1/health` — bridge + mesh status summary. Distinct
/// from `/health` which is a plaintext liveness probe.
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let now = unix_secs();
    let mut fresh = 0usize;
    let mut stale = 0usize;
    let mut expired = 0usize;
    let entries = state.manifest_cache.entries();
    for c in &entries {
        let secs_ago = (now - c.last_refreshed_at).max(0);
        match freshness_label(secs_ago) {
            "fresh" => fresh += 1,
            "stale" => stale += 1,
            _ => expired += 1,
        }
    }
    let reconnect = state.mesh_client.as_ref().map(|c| {
        let (attempts, successes) = c.reconnect_counters();
        ReconnectCounters {
            attempts,
            successes,
        }
    });
    let streams = StreamCounters {
        active: state.stream_metrics.active(),
        opened_total: state.stream_metrics.opened_total(),
    };
    Json(HealthResponse {
        status: "ok",
        started_at: state.started_at,
        now,
        uptime_secs: (now - state.started_at).max(0),
        coordinator_configured: state.task_recorder.is_some(),
        peer_count: entries.len(),
        peers_fresh: fresh,
        peers_stale: stale,
        peers_expired: expired,
        reconnect,
        streams,
    })
}

/// `GET /v1/topology` — list every peer in the bridge's
/// manifest cache with freshness aggregates.
pub async fn get(
    State(state): State<AppState>,
) -> Result<Json<TopologyResponse>, (StatusCode, Json<ApiError>)> {
    let now = unix_secs();
    let mut peers: Vec<PeerView> = state
        .manifest_cache
        .entries()
        .into_iter()
        .map(|c| {
            let mut methods: Vec<String> = c
                .manifest
                .capabilities
                .iter()
                .map(|cap| cap.method_name.clone())
                .collect();
            methods.sort();
            let secs_ago = (now - c.last_refreshed_at).max(0);
            PeerView {
                alias: c.alias,
                node_id: c.manifest.node_id.to_string(),
                node_type: c.manifest.node_type,
                node_name: c.manifest.node_name,
                manifest_version: c.manifest.manifest_version,
                capability_count: c.manifest.capabilities.len(),
                methods,
                last_refreshed_at: c.last_refreshed_at,
                last_refreshed_secs_ago: secs_ago,
                freshness: freshness_label(secs_ago),
            }
        })
        .collect();
    // Stable alias-first ordering. Peers with no alias sort
    // after aliased peers; within each group, sort by node_id
    // for deterministic output.
    peers.sort_by(|a, b| match (a.alias.as_ref(), b.alias.as_ref()) {
        (Some(x), Some(y)) => x.cmp(y).then(a.node_id.cmp(&b.node_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.node_id.cmp(&b.node_id),
    });
    Ok(Json(TopologyResponse {
        peers,
        generated_at: now,
    }))
}

/// Threshold buckets aligned with the 60s manifest-refresh
/// period:
///
/// - `fresh` — within the last refresh tick + a small grace
///   (120s) for clock skew + refresh duration.
/// - `stale` — between 120s and 600s. Indicates one or two
///   missed refresh ticks; the peer is probably reachable but
///   slow.
/// - `expired` — 600s+. The cached capabilities are still in
///   use by routing, but the peer has not responded for ~10
///   manifest periods. Operator action recommended.
fn freshness_label(secs_ago: i64) -> &'static str {
    if secs_ago < 120 {
        "fresh"
    } else if secs_ago < 600 {
        "stale"
    } else {
        "expired"
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_label_aligns_with_60s_refresh_period() {
        // 60s refresh period; 120s is "one missed tick plus
        // a grace window for clock skew + refresh duration".
        assert_eq!(freshness_label(0), "fresh");
        assert_eq!(freshness_label(60), "fresh");
        assert_eq!(freshness_label(119), "fresh");
        assert_eq!(freshness_label(120), "stale");
        assert_eq!(freshness_label(599), "stale");
        assert_eq!(freshness_label(600), "expired");
        assert_eq!(freshness_label(3600), "expired");
    }

    #[test]
    fn freshness_label_clamps_negative_secs_ago() {
        // Defensive: if clock skew puts the cached timestamp
        // in the "future" relative to `now`, treat the entry
        // as fresh rather than expired. The caller already
        // clamps with `.max(0)` but the label function should
        // be robust if called directly.
        assert_eq!(freshness_label(-5), "fresh");
    }
}
