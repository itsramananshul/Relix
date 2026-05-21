//! Router node — mesh observability + health control plane.
//!
//! Registered capabilities on a controller with
//! `[controller] node_type = "router"` AND `role = "router"`:
//!
//! - `router.heartbeat`       — controllers push liveness + caps every 60s.
//! - `router.network_summary` — operator-facing mesh overview.
//! - `router.session_list`    — operator-facing session browser.
//! - `router.log`             — controllers push structured log lines.
//!
//! ## Wire format
//!
//! Unlike the SIMP-016 alpha pipe-delimited capabilities, router
//! caps use CBOR-encoded request/response structs from
//! [`relix_core::router`]. The richer shape (lists, optionals,
//! nested records) doesn't fit cleanly into `key|val|val` and we
//! want stable evolvability for the operator surface.
//!
//! ## State
//!
//! [`RouterState`] holds:
//! - `peers`    — `peer_id → PeerRecord` (most recent heartbeat per peer)
//! - `sessions` — `session_id → SessionRecord` (cross-peer workflow traces)
//! - `logs`     — bounded ring (10k lines) of aggregated log records
//!
//! Two reaper background loops live in
//! [`crate::controller_runtime::run`]:
//! - 30s tick: [`RouterState::reap_stale_peers`] flips `healthy = false`
//!   on any peer whose last heartbeat is older than 90s.
//! - 300s tick: [`RouterState::reap_expired_sessions`] drops
//!   `completed`/`failed` sessions older than `session_ttl_secs`.
//!
//! ## Architecture invariants honored
//!
//! - 1: every router RPC is identity → policy → handler → audit
//!   (the bridge runs admission unchanged for these handlers).
//! - 2-4: the router NEVER makes LLM calls, NEVER holds provider keys,
//!   and makes no routing decisions outside SOL flows (the
//!   "router" name refers to network observability, not request
//!   routing — name collision with the AI router scaffold is
//!   intentional shorthand for "control plane").

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use relix_core::codec;
use relix_core::router::{
    HeartbeatRequest, HeartbeatResponse, LogRequest, LogResponse, NetworkSummaryRequest,
    NetworkSummaryResponse, PeerSummary, SessionListRequest, SessionListResponse, SessionRecord,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Stale threshold: mark `healthy = false` if no heartbeat in
/// 90 seconds (1.5x the 60s send interval).
pub const STALE_TIMEOUT_SECS: u64 = 90;

/// Hard cap on the in-memory log ring.
pub const LOG_RING_CAP: usize = 10_000;

/// Mutable router state. Wrapped in `Arc<Mutex<>>` for handlers
/// that need write access; the std mutex is non-reentrant so
/// every handler holds the guard for the *minimum* necessary
/// scope (build the response into an owned struct, drop the
/// guard, then return).
pub struct RouterState {
    pub peers: HashMap<String, PeerRecord>,
    pub sessions: HashMap<String, SessionRecord>,
    pub logs: Vec<LogLine>,
    pub session_ttl: Duration,
    pub router_peer_id: String,
    pub router_name: String,
    pub started_at: u64,
    /// Monotonic counter; never decremented on session reap.
    pub total_sessions: u64,
}

/// One known peer.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub peer_id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub last_heartbeat: u64,
    pub healthy: bool,
    pub groups: Vec<String>,
}

/// One aggregated log line.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub source_peer_id: String,
    pub level: String,
    pub message: String,
    pub task_id: Option<String>,
    pub timestamp: u64,
}

impl RouterState {
    pub fn new(router_peer_id: String, router_name: String, session_ttl_secs: u64) -> Self {
        Self {
            peers: HashMap::new(),
            sessions: HashMap::new(),
            logs: Vec::new(),
            session_ttl: Duration::from_secs(session_ttl_secs),
            router_peer_id,
            router_name,
            started_at: now_secs(),
            total_sessions: 0,
        }
    }

    /// `router.heartbeat` — register-or-update the calling peer
    /// and return the router's current peer snapshot.
    pub fn handle_heartbeat(&mut self, req: HeartbeatRequest) -> HeartbeatResponse {
        let ts = now_secs();
        self.peers.insert(
            req.peer_id.clone(),
            PeerRecord {
                peer_id: req.peer_id,
                name: req.name,
                capabilities: req.capabilities,
                last_heartbeat: ts,
                healthy: true,
                groups: req.groups,
            },
        );
        HeartbeatResponse {
            status: "ok".into(),
            peers: self.peers.values().map(peer_summary).collect(),
        }
    }

    /// `router.network_summary` — operator-facing mesh overview.
    pub fn handle_network_summary(&self, req: NetworkSummaryRequest) -> NetworkSummaryResponse {
        let peers: Vec<PeerSummary> = self
            .peers
            .values()
            .filter(|p| match &req.org_filter {
                Some(org) if !org.is_empty() => p.groups.iter().any(|g| g.contains(org.as_str())),
                _ => true,
            })
            .map(peer_summary)
            .collect();
        let active_sessions = self
            .sessions
            .values()
            .filter(|s| s.status == "running")
            .count();
        NetworkSummaryResponse {
            router_peer_id: self.router_peer_id.clone(),
            router_name: self.router_name.clone(),
            peer_count: peers.len(),
            peers,
            active_sessions,
            total_sessions_since_start: self.total_sessions,
            uptime_secs: now_secs().saturating_sub(self.started_at),
            timestamp: now_secs(),
        }
    }

    /// `router.session_list` — operator-facing session browser
    /// with status filter + pagination.
    pub fn handle_session_list(&self, req: SessionListRequest) -> SessionListResponse {
        let limit = req.limit.unwrap_or(100);
        let offset = req.offset.unwrap_or(0);
        let mut sessions: Vec<&SessionRecord> = self
            .sessions
            .values()
            .filter(|s| match &req.status_filter {
                Some(f) => &s.status == f,
                None => true,
            })
            .collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
        let total = sessions.len();
        let page: Vec<SessionRecord> = sessions
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        SessionListResponse {
            sessions: page,
            total,
        }
    }

    /// `router.log` — append to the bounded ring. Drops the
    /// oldest lines when the ring exceeds `LOG_RING_CAP`.
    pub fn handle_log(&mut self, req: LogRequest) -> LogResponse {
        self.logs.push(LogLine {
            source_peer_id: req.source_peer_id,
            level: req.level,
            message: req.message,
            task_id: req.task_id,
            timestamp: req.timestamp,
        });
        if self.logs.len() > LOG_RING_CAP {
            let overflow = self.logs.len() - LOG_RING_CAP;
            self.logs.drain(0..overflow);
        }
        LogResponse {
            status: "ok".into(),
        }
    }

    /// Register or update a session (called when a workflow hop
    /// is recorded). Bumps the monotonic `total_sessions`
    /// counter on first insert only.
    pub fn track_session(&mut self, session: SessionRecord) {
        if !self.sessions.contains_key(&session.session_id) {
            self.total_sessions += 1;
        }
        self.sessions.insert(session.session_id.clone(), session);
    }

    /// Mark stale peers — `healthy = false` when no heartbeat
    /// in `STALE_TIMEOUT_SECS`. Pure state mutation; the
    /// background reaper task in `controller_runtime::run`
    /// calls this every 30s.
    pub fn reap_stale_peers(&mut self) {
        let now = now_secs();
        for peer in self.peers.values_mut() {
            if now.saturating_sub(peer.last_heartbeat) > STALE_TIMEOUT_SECS {
                peer.healthy = false;
            }
        }
    }

    /// Remove expired completed/failed sessions. Running
    /// sessions are never reaped.
    pub fn reap_expired_sessions(&mut self) {
        let now = now_secs();
        let ttl = self.session_ttl.as_secs();
        self.sessions.retain(|_, s| {
            if s.status == "running" {
                return true;
            }
            now.saturating_sub(s.updated_at) <= ttl
        });
    }
}

fn peer_summary(p: &PeerRecord) -> PeerSummary {
    PeerSummary {
        peer_id: p.peer_id.clone(),
        name: p.name.clone(),
        capabilities: p.capabilities.clone(),
        last_heartbeat_secs: p.last_heartbeat,
        healthy: p.healthy,
        groups: p.groups.clone(),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ──────────────────────────── Registration ─────────────────────────────────

/// Wire all four router capabilities onto the dispatch bridge.
/// Caller is `controller_runtime::register_node_type_handlers`
/// when `[controller] role = "router"` is set.
pub fn register(bridge: &mut DispatchBridge, state: Arc<Mutex<RouterState>>) {
    {
        let st = state.clone();
        bridge.register(
            "router.heartbeat",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move { handle_heartbeat_rpc(&st, &ctx) }
            })),
        );
    }
    {
        let st = state.clone();
        bridge.register(
            "router.network_summary",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move { handle_network_summary_rpc(&st, &ctx) }
            })),
        );
    }
    {
        let st = state.clone();
        bridge.register(
            "router.session_list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move { handle_session_list_rpc(&st, &ctx) }
            })),
        );
    }
    {
        let st = state;
        bridge.register(
            "router.log",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let st = st.clone();
                async move { handle_log_rpc(&st, &ctx) }
            })),
        );
    }
}

// ──────────────────────────── RPC adapters ─────────────────────────────────

fn handle_heartbeat_rpc(state: &Mutex<RouterState>, ctx: &InvocationCtx) -> HandlerOutcome {
    let req: HeartbeatRequest = match codec::decode(&ctx.args) {
        Ok(r) => r,
        Err(e) => return invalid(format!("router.heartbeat decode: {e}")),
    };
    let resp = {
        let mut g = match state.lock() {
            Ok(g) => g,
            Err(e) => return internal(format!("router.heartbeat lock poisoned: {e}")),
        };
        g.handle_heartbeat(req)
    };
    encode_ok(&resp, "router.heartbeat")
}

fn handle_network_summary_rpc(state: &Mutex<RouterState>, ctx: &InvocationCtx) -> HandlerOutcome {
    let req: NetworkSummaryRequest = if ctx.args.is_empty() {
        NetworkSummaryRequest::default()
    } else {
        match codec::decode(&ctx.args) {
            Ok(r) => r,
            Err(e) => return invalid(format!("router.network_summary decode: {e}")),
        }
    };
    let resp = {
        let g = match state.lock() {
            Ok(g) => g,
            Err(e) => return internal(format!("router.network_summary lock poisoned: {e}")),
        };
        g.handle_network_summary(req)
    };
    encode_ok(&resp, "router.network_summary")
}

fn handle_session_list_rpc(state: &Mutex<RouterState>, ctx: &InvocationCtx) -> HandlerOutcome {
    let req: SessionListRequest = if ctx.args.is_empty() {
        SessionListRequest::default()
    } else {
        match codec::decode(&ctx.args) {
            Ok(r) => r,
            Err(e) => return invalid(format!("router.session_list decode: {e}")),
        }
    };
    let resp = {
        let g = match state.lock() {
            Ok(g) => g,
            Err(e) => return internal(format!("router.session_list lock poisoned: {e}")),
        };
        g.handle_session_list(req)
    };
    encode_ok(&resp, "router.session_list")
}

fn handle_log_rpc(state: &Mutex<RouterState>, ctx: &InvocationCtx) -> HandlerOutcome {
    let req: LogRequest = match codec::decode(&ctx.args) {
        Ok(r) => r,
        Err(e) => return invalid(format!("router.log decode: {e}")),
    };
    let resp = {
        let mut g = match state.lock() {
            Ok(g) => g,
            Err(e) => return internal(format!("router.log lock poisoned: {e}")),
        };
        g.handle_log(req)
    };
    encode_ok(&resp, "router.log")
}

fn encode_ok<T: serde::Serialize>(value: &T, method: &str) -> HandlerOutcome {
    match codec::encode(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("{method} encode: {e}")),
    }
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> RouterState {
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

    #[test]
    fn heartbeat_registers_peer_marked_healthy() {
        let mut s = fresh();
        let r = s.handle_heartbeat(hb("p1", "tool", &["controllers"], &["tool.read_file"]));
        assert_eq!(r.status, "ok");
        assert_eq!(r.peers.len(), 1);
        let p = s.peers.get("p1").unwrap();
        assert!(p.healthy);
        assert_eq!(p.capabilities, vec!["tool.read_file"]);
    }

    #[test]
    fn second_heartbeat_overwrites_existing_record() {
        let mut s = fresh();
        s.handle_heartbeat(hb("p1", "tool", &["controllers"], &["tool.read_file"]));
        s.handle_heartbeat(hb(
            "p1",
            "tool",
            &["controllers"],
            &["tool.read_file", "tool.write_file"],
        ));
        assert_eq!(s.peers.get("p1").unwrap().capabilities.len(), 2);
    }

    #[test]
    fn reap_stale_peers_flips_healthy_false() {
        let mut s = fresh();
        s.handle_heartbeat(hb("p1", "tool", &["controllers"], &[]));
        // Backdate the last heartbeat past the threshold.
        s.peers.get_mut("p1").unwrap().last_heartbeat = now_secs().saturating_sub(100);
        s.reap_stale_peers();
        assert!(!s.peers.get("p1").unwrap().healthy);
    }

    #[test]
    fn reap_stale_peers_leaves_fresh_alone() {
        let mut s = fresh();
        s.handle_heartbeat(hb("p1", "tool", &["controllers"], &[]));
        s.reap_stale_peers();
        assert!(s.peers.get("p1").unwrap().healthy);
    }

    #[test]
    fn session_reap_respects_ttl() {
        let mut s = fresh();
        let now = now_secs();
        s.track_session(session("s-completed", "completed", now - 3000, now - 2000));
        s.track_session(session("s-running", "running", now - 10, now - 10));
        s.reap_expired_sessions();
        assert!(!s.sessions.contains_key("s-completed"));
        assert!(s.sessions.contains_key("s-running"));
    }

    #[test]
    fn track_session_bumps_total_only_on_first_insert() {
        let mut s = fresh();
        let now = now_secs();
        s.track_session(session("s-1", "running", now, now));
        s.track_session(session("s-1", "completed", now, now)); // update, not new
        s.track_session(session("s-2", "running", now, now));
        assert_eq!(s.total_sessions, 2);
    }

    #[test]
    fn network_summary_counts_active_sessions() {
        let mut s = fresh();
        let now = now_secs();
        s.track_session(session("r1", "running", now, now));
        s.track_session(session("r2", "running", now, now));
        s.track_session(session("c1", "completed", now, now));
        let resp = s.handle_network_summary(NetworkSummaryRequest::default());
        assert_eq!(resp.active_sessions, 2);
        assert_eq!(resp.total_sessions_since_start, 3);
    }

    #[test]
    fn network_summary_org_filter_matches_substring() {
        let mut s = fresh();
        s.handle_heartbeat(hb(
            "p1",
            "a-tool",
            &["org-a.controllers"],
            &["tool.read_file"],
        ));
        s.handle_heartbeat(hb(
            "p2",
            "b-tool",
            &["org-b.controllers"],
            &["tool.read_file"],
        ));
        let resp = s.handle_network_summary(NetworkSummaryRequest {
            org_filter: Some("org-a".into()),
        });
        assert_eq!(resp.peer_count, 1);
        assert_eq!(resp.peers[0].peer_id, "p1");
    }

    #[test]
    fn session_list_paginates_and_filters() {
        let mut s = fresh();
        let now = now_secs();
        for i in 0..5 {
            s.track_session(session(
                &format!("s-{i}"),
                if i % 2 == 0 { "running" } else { "completed" },
                now - i as u64,
                now - i as u64,
            ));
        }
        let resp = s.handle_session_list(SessionListRequest {
            status_filter: Some("running".into()),
            limit: Some(2),
            offset: Some(0),
        });
        assert_eq!(resp.total, 3);
        assert_eq!(resp.sessions.len(), 2);
        // Newest first by started_at.
        assert_eq!(resp.sessions[0].session_id, "s-0");
    }

    #[test]
    fn log_buffer_capped_at_10k() {
        let mut s = fresh();
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
        // Oldest line dropped.
        assert_eq!(s.logs[0].message, "line 1");
    }
}
