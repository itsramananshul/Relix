//! Router node — six acceptance tests required by the
//! Router-Node implementation spec.
//!
//! These exercise [`relix_runtime::nodes::router::RouterState`]
//! directly (no transport / no dispatch bridge) so a regression
//! in the in-memory state machine is caught fast. Per-handler
//! decode/encode and dispatch-pipeline coverage live in
//! `nodes/router.rs` unit tests + the per-capability integration
//! tests that will land alongside the operator UI.

use std::time::{SystemTime, UNIX_EPOCH};

use relix_core::router::{HeartbeatRequest, LogRequest, NetworkSummaryRequest, SessionRecord};
use relix_runtime::nodes::router::{LOG_RING_CAP, RouterState};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn fresh_router() -> RouterState {
    RouterState::new("12D3KooWRouter".into(), "router".into(), 1800)
}

fn hb(peer_id: &str, name: &str, groups: &[&str], caps: &[&str]) -> HeartbeatRequest {
    HeartbeatRequest {
        peer_id: peer_id.into(),
        name: name.into(),
        capabilities: caps.iter().map(|s| s.to_string()).collect(),
        timestamp: now_secs(),
        groups: groups.iter().map(|s| s.to_string()).collect(),
    }
}

fn session(id: &str, status: &str, started: u64, updated: u64) -> SessionRecord {
    SessionRecord {
        session_id: id.into(),
        source_peer_id: "p1".into(),
        workflow_name: "w".into(),
        capabilities_used: vec![],
        route: vec!["p1".into()],
        status: status.into(),
        started_at: started,
        updated_at: updated,
    }
}

/// Test 1 — heartbeat_registers_peer.
/// Create a `RouterState`, call `handle_heartbeat` with a known
/// peer_id and capabilities. The peer must appear in
/// `router_state.peers` with `healthy = true`.
#[test]
fn heartbeat_registers_peer() {
    let mut s = fresh_router();
    let r = s.handle_heartbeat(hb(
        "12D3KooWAlpha",
        "tool-alpha",
        &["controllers"],
        &["tool.read_file", "tool.web_fetch"],
    ));
    assert_eq!(r.status, "ok");
    assert!(s.peers.contains_key("12D3KooWAlpha"));
    let p = s.peers.get("12D3KooWAlpha").unwrap();
    assert!(p.healthy);
    assert_eq!(p.capabilities.len(), 2);
    assert!(p.capabilities.iter().any(|c| c == "tool.read_file"));
}

/// Test 2 — stale_peer_marked_unhealthy.
/// Register a peer with `last_heartbeat` set to `now_secs() - 100`
/// (91 seconds ago, past the 90s threshold). Call
/// `reap_stale_peers()`. The peer's `healthy` field must be
/// false afterwards.
#[test]
fn stale_peer_marked_unhealthy() {
    let mut s = fresh_router();
    s.handle_heartbeat(hb("p1", "tool", &["controllers"], &[]));
    // Backdate the last heartbeat past the 90s threshold.
    s.peers.get_mut("p1").unwrap().last_heartbeat = now_secs().saturating_sub(100);
    s.reap_stale_peers();
    assert!(!s.peers.get("p1").unwrap().healthy);
}

/// Test 3 — session_reap_respects_ttl.
/// Insert a completed session with `updated_at` set to
/// `now_secs() - 2000` (past the 1800s default TTL). Insert a
/// running session with any `updated_at`. Call
/// `reap_expired_sessions()`. The completed session must be
/// gone; the running session must remain.
#[test]
fn session_reap_respects_ttl() {
    let mut s = fresh_router();
    let now = now_secs();
    s.track_session(session("s-completed", "completed", now - 3000, now - 2000));
    s.track_session(session("s-running", "running", now - 5, now - 5));
    s.reap_expired_sessions();
    assert!(!s.sessions.contains_key("s-completed"));
    assert!(s.sessions.contains_key("s-running"));
}

/// Test 4 — network_summary_counts_active_sessions.
/// Insert 2 running sessions and 1 completed session. Call
/// `handle_network_summary`. `active_sessions` must == 2.
#[test]
fn network_summary_counts_active_sessions() {
    let mut s = fresh_router();
    let now = now_secs();
    s.track_session(session("r1", "running", now, now));
    s.track_session(session("r2", "running", now, now));
    s.track_session(session("c1", "completed", now, now));
    let resp = s.handle_network_summary(NetworkSummaryRequest::default());
    assert_eq!(resp.active_sessions, 2);
    assert_eq!(resp.total_sessions_since_start, 3);
}

/// Test 5 — log_buffer_capped_at_10k.
/// Call `handle_log` 10,001 times. `router_state.logs.len()`
/// must == 10_000 afterwards. The oldest line is dropped.
#[test]
fn log_buffer_capped_at_10k() {
    let mut s = fresh_router();
    for i in 0..10_001u64 {
        s.handle_log(LogRequest {
            source_peer_id: "p1".into(),
            level: "info".into(),
            message: format!("line {i}"),
            task_id: None,
            timestamp: i,
        });
    }
    assert_eq!(s.logs.len(), LOG_RING_CAP);
    // Oldest line (i=0) must have been dropped first.
    assert_eq!(s.logs[0].message, "line 1");
    // Newest line must be present.
    assert_eq!(s.logs.last().unwrap().message, "line 10000");
}

/// Test 6 — org_filter_in_network_summary.
/// Register two peers — one with `groups = ["org-a.controllers"]`
/// and one with `groups = ["org-b.controllers"]`. Call
/// `handle_network_summary` with `org_filter = Some("org-a")`.
/// Only one peer (the org-a one) must be returned.
#[test]
fn org_filter_in_network_summary() {
    let mut s = fresh_router();
    s.handle_heartbeat(hb(
        "p-a",
        "alpha",
        &["org-a.controllers"],
        &["tool.read_file"],
    ));
    s.handle_heartbeat(hb(
        "p-b",
        "beta",
        &["org-b.controllers"],
        &["tool.read_file"],
    ));
    let resp = s.handle_network_summary(NetworkSummaryRequest {
        org_filter: Some("org-a".into()),
    });
    assert_eq!(resp.peer_count, 1);
    assert_eq!(resp.peers[0].peer_id, "p-a");
}
