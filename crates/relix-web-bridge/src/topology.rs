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

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;
use crate::lifecycle::LifecycleEvent;
use crate::metrics::ActiveStream;

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
    /// FIX 49: per-channel health snapshots pulled in
    /// parallel from the telegram, slack, and discord peers'
    /// `<channel>.health` capabilities. Missing entries
    /// (peer unreachable, cap not registered) are surfaced
    /// as `null` rather than omitted so dashboards can
    /// distinguish "channel down" from "channel not deployed".
    pub channels: ChannelsHealth,
}

/// FIX 49: aggregator for the three channel-side health
/// snapshots. Each field is `None` when the bridge could not
/// reach that channel's `<channel>.health` capability.
#[derive(Debug, Serialize, Default)]
pub struct ChannelsHealth {
    pub telegram: Option<relix_core::channel_health::ChannelHealthSnapshot>,
    pub slack: Option<relix_core::channel_health::ChannelHealthSnapshot>,
    pub discord: Option<relix_core::channel_health::ChannelHealthSnapshot>,
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

#[derive(Debug, Deserialize, Default)]
pub struct LifecycleEventsQuery {
    /// Return only events with `ts > since`. Default 0 reads
    /// the entire in-memory ring.
    #[serde(default)]
    pub since: Option<i64>,
    /// Cap on returned events. Defaults to ring capacity.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct LifecycleEventsResponse {
    pub events: Vec<LifecycleEvent>,
    /// Monotonic sequence cursor — opaque, server-assigned.
    /// Operators that want incremental polling pass back the
    /// max `ts` of the returned events on the next request.
    pub seq: i64,
    pub generated_at: i64,
}

/// One row of `/v1/streams`. Same shape as
/// `crate::metrics::ActiveStream` plus a derived `age_secs`
/// so dashboards don't have to compute the elapsed time.
#[derive(Debug, Serialize)]
pub struct StreamRow {
    pub id: u64,
    pub task_id: String,
    pub opened_at: i64,
    pub age_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct StreamsResponse {
    pub active: Vec<StreamRow>,
    pub opened_total: u64,
    pub generated_at: i64,
}

/// `GET /v1/streams` — list currently-open SSE streams. One
/// row per active stream tagged with task_id + opened_at +
/// age_secs. Useful for "which task is being watched right
/// now" operator visibility.
pub async fn streams_list(State(state): State<AppState>) -> Json<StreamsResponse> {
    let now = unix_secs();
    let rows: Vec<StreamRow> = state
        .stream_metrics
        .list_active()
        .into_iter()
        .map(|s: ActiveStream| StreamRow {
            id: s.id,
            task_id: s.task_id,
            opened_at: s.opened_at,
            age_secs: (now - s.opened_at).max(0),
        })
        .collect();
    Json(StreamsResponse {
        active: rows,
        opened_total: state.stream_metrics.opened_total(),
        generated_at: now,
    })
}

/// One row of `/v1/routing` — for a given capability method,
/// which peer the bridge's manifest cache would route to
/// right now, plus that peer's freshness so operators can
/// see "this routing decision is currently stale."
#[derive(Debug, Serialize)]
pub struct RoutingEntry {
    /// `<namespace>.<action>` capability method name.
    pub method: String,
    /// Operator-configured alias of the chosen peer. The
    /// bridge picks the first peer in cache that
    /// advertises the method — same semantics as
    /// `ManifestCache::find_alias_for_method`. `None` for
    /// methods advertised by a peer without an alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Hex `NodeId` of the chosen peer.
    pub node_id: String,
    /// `node_type` of the chosen peer (`memory`, `ai`,
    /// `tool`, `coordinator`, …).
    pub node_type: String,
    /// Freshness bucket of the chosen peer at snapshot
    /// time. `expired` here means routing would use a
    /// peer that may be unreachable.
    pub freshness: &'static str,
    /// Wall-clock unix seconds the chosen peer was last
    /// successfully refreshed.
    pub last_refreshed_at: i64,
    /// `true` when more than one peer advertises this
    /// method — operators can see "first-match-in-cache"
    /// is making a non-trivial choice.
    pub multiple_candidates: bool,
}

#[derive(Debug, Serialize)]
pub struct RoutingResponse {
    pub entries: Vec<RoutingEntry>,
    pub generated_at: i64,
    /// Honest description of how the bridge picks: first
    /// peer in cache that advertises the method. Surfaces
    /// to dashboards so the "why this peer" answer is
    /// available without operators reading source.
    pub policy: &'static str,
}

/// `GET /v1/routing` — snapshot of the bridge's current
/// capability-to-peer resolution. Pure projection of the
/// manifest cache — no probing, no orchestration. The
/// answer to "where would `tool.web_fetch` go right
/// now?" without per-call routing logs (which the
/// runtime doesn't record today).
/// One peer candidate for a method — extracted to keep
/// the by_method map's value type from being so wide
/// that clippy flags it.
#[derive(Debug, Clone)]
struct RoutingCandidate {
    alias: Option<String>,
    node_id: String,
    node_type: String,
    last_refreshed_at: i64,
}

pub async fn routing_snapshot(State(state): State<AppState>) -> Json<RoutingResponse> {
    let now = unix_secs();
    let entries_cached = state.manifest_cache.entries();
    // For each method, collect every peer that advertises
    // it. The first entry in `entries_cached` wins, mirroring
    // `ManifestCache::find_alias_for_method`'s
    // first-iteration-in-BTreeMap semantics.
    use std::collections::BTreeMap;
    let mut by_method: BTreeMap<String, Vec<RoutingCandidate>> = BTreeMap::new();
    for c in entries_cached.iter() {
        for cap in &c.manifest.capabilities {
            by_method
                .entry(cap.method_name.clone())
                .or_default()
                .push(RoutingCandidate {
                    alias: c.alias.clone(),
                    node_id: c.manifest.node_id.to_string(),
                    node_type: c.manifest.node_type.clone(),
                    last_refreshed_at: c.last_refreshed_at,
                });
        }
    }
    let mut entries: Vec<RoutingEntry> = by_method
        .into_iter()
        .map(|(method, candidates)| {
            let winner = candidates[0].clone();
            let secs_ago = (now - winner.last_refreshed_at).max(0);
            RoutingEntry {
                method,
                alias: winner.alias,
                node_id: winner.node_id,
                node_type: winner.node_type,
                freshness: freshness_label(secs_ago),
                last_refreshed_at: winner.last_refreshed_at,
                multiple_candidates: candidates.len() > 1,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.method.cmp(&b.method));
    Json(RoutingResponse {
        entries,
        generated_at: now,
        policy: "first peer in manifest cache that advertises the method (no scoring, no priority)",
    })
}

/// `GET /v1/topology/events?since=<ts>&limit=<n>` — recent
/// node lifecycle transitions (joins, freshness changes,
/// drops). Newest first. In-memory ring; resets on bridge
/// restart.
pub async fn lifecycle_events(
    State(state): State<AppState>,
    Query(q): Query<LifecycleEventsQuery>,
) -> Json<LifecycleEventsResponse> {
    let since = q.since.unwrap_or(0);
    let limit = q.limit.unwrap_or(500).min(2000);
    let (events, seq) = state.lifecycle_log.since(since, limit);
    Json(LifecycleEventsResponse {
        events,
        seq,
        generated_at: unix_secs(),
    })
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
    // FIX 49: fan out three concurrent `<channel>.health`
    // calls. Each failure → `None` so the bridge stays up
    // even when a channel peer is offline.
    let channels = fetch_channels_health(&state).await;
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
        channels,
    })
}

/// FIX 49: parallel fetch of the three channel peers'
/// `<channel>.health` capabilities. Always returns a
/// `ChannelsHealth` (default-empty when the mesh client
/// is missing).
async fn fetch_channels_health(state: &AppState) -> ChannelsHealth {
    let Some(mesh) = state.mesh_client.as_ref() else {
        return ChannelsHealth::default();
    };
    let deadline = state.cfg.transport.deadline_secs.clamp(2, 10);
    let identity = state.identity_bundle.clone();
    let mesh = mesh.clone();
    // `tokio::join!` runs the three calls concurrently. Each
    // returns `Option<Snapshot>` so a missing peer doesn't
    // sink the whole response.
    let (tg, sl, dc) = tokio::join!(
        fetch_one_channel_health(&mesh, "telegram", &identity, deadline),
        fetch_one_channel_health(&mesh, "slack", &identity, deadline),
        fetch_one_channel_health(&mesh, "discord", &identity, deadline),
    );
    ChannelsHealth {
        telegram: tg,
        slack: sl,
        discord: dc,
    }
}

async fn fetch_one_channel_health(
    mesh: &std::sync::Arc<relix_runtime::manifest::MeshClient>,
    alias: &str,
    identity: &relix_core::bundle::Bundle,
    deadline: i64,
) -> Option<relix_core::channel_health::ChannelHealthSnapshot> {
    let method = format!("{alias}.health");
    // PART 3: read the request-task's resolved tenant. The
    // `tokio::join!` inside `fetch_channels_health` runs in
    // the same task as the `/v1/health` handler so the
    // task-local `CURRENT_TENANT` is still in scope here.
    let envelope = relix_runtime::dispatch::build_request_with_tenant(
        &method,
        Vec::new(),
        identity.clone(),
        deadline,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = match mesh.call(alias, envelope).await {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(
                error = %e,
                alias = alias,
                "v1/health: channel health call failed; surfacing as null"
            );
            return None;
        }
    };
    let resp = relix_runtime::dispatch::decode_response(&resp_bytes).ok()?;
    match resp.res {
        relix_runtime::transport::envelope::ResponseResult::Ok(body) => {
            serde_json::from_slice::<relix_core::channel_health::ChannelHealthSnapshot>(&body).ok()
        }
        _ => None,
    }
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
