//! Router node RPC types.
//!
//! Four capabilities make up the router surface:
//! - `router.heartbeat` — every non-router controller pushes a
//!   liveness + capability snapshot every 60s.
//! - `router.network_summary` — operator-facing mesh overview.
//! - `router.session_list` — operator-facing session browser.
//! - `router.log` — controllers push structured log lines to
//!   the router for aggregation.
//!
//! All types are CBOR-serializable via the canonical
//! [`crate::codec`] encoder. Field additions must be at the end
//! and the consumer must tolerate unknown trailing fields
//! (serde does this by default for `Deserialize`).

use serde::{Deserialize, Serialize};

/// Sent by controller nodes every 60 seconds to `router.heartbeat`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// This controller's libp2p PeerId (base58).
    pub peer_id: String,
    /// Human-readable name from controller config.
    pub name: String,
    /// All capability method strings this node handles.
    pub capabilities: Vec<String>,
    /// Unix timestamp seconds.
    pub timestamp: u64,
    /// Org identity group memberships (from `IdentityBundle.groups`).
    pub groups: Vec<String>,
}

/// Reply to `router.heartbeat`. Echoes the router's currently
/// known peers so the controller can opportunistically reconcile
/// its own view of the mesh without a separate request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Always `"ok"` on a successful registration.
    pub status: String,
    /// Snapshot of every peer the router currently knows about.
    pub peers: Vec<PeerSummary>,
}

/// Lightweight peer record returned in heartbeat responses and
/// network summary responses. `healthy = false` means the router
/// has not seen a heartbeat from this peer within the stale
/// threshold (90s by default).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    /// libp2p PeerId (base58).
    pub peer_id: String,
    /// Human-readable name from the peer's controller config.
    pub name: String,
    /// All capability method strings the peer advertises.
    pub capabilities: Vec<String>,
    /// Unix timestamp seconds of the most recent heartbeat
    /// received from this peer.
    pub last_heartbeat_secs: u64,
    /// `true` if the last heartbeat was within the stale window.
    pub healthy: bool,
    /// Org identity group memberships from the peer's bundle.
    pub groups: Vec<String>,
}

/// `router.network_summary` — operator-facing mesh overview.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkSummaryRequest {
    /// Optional org filter — only return peers whose `groups`
    /// contain a token matching this substring. Empty = all
    /// orgs the caller is allowed to see.
    pub org_filter: Option<String>,
}

/// Reply to `router.network_summary`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSummaryResponse {
    /// Router's own peer_id (base58).
    pub router_peer_id: String,
    /// Router's human-readable name.
    pub router_name: String,
    /// Number of peers in `peers` (post-filter).
    pub peer_count: usize,
    /// Filtered peer list.
    pub peers: Vec<PeerSummary>,
    /// Count of sessions currently in the `"running"` state.
    pub active_sessions: usize,
    /// Total sessions ever registered with this router
    /// (monotonic; never decremented on reap).
    pub total_sessions_since_start: u64,
    /// Router uptime in seconds.
    pub uptime_secs: u64,
    /// Unix timestamp seconds of this response.
    pub timestamp: u64,
}

/// `router.session_list` — operator-facing session browser.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionListRequest {
    /// `"running"` | `"completed"` | `"failed"` | `None` = all.
    pub status_filter: Option<String>,
    /// Page size; default 100 server-side when `None`.
    pub limit: Option<usize>,
    /// Page offset; default 0 server-side when `None`.
    pub offset: Option<usize>,
}

/// Reply to `router.session_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResponse {
    /// Paged + sorted (newest first by `started_at`) session list.
    pub sessions: Vec<SessionRecord>,
    /// Total session count matching the filter (pre-paging).
    pub total: usize,
}

/// One session record — a workflow hop-trace across peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Unique session id (caller-supplied).
    pub session_id: String,
    /// The peer that originated the session.
    pub source_peer_id: String,
    /// Workflow name / handle.
    pub workflow_name: String,
    /// Capability method strings invoked during this session.
    pub capabilities_used: Vec<String>,
    /// Ordered list of peer_ids the workflow visited.
    pub route: Vec<String>,
    /// `"running"` | `"completed"` | `"failed"`.
    pub status: String,
    /// Unix timestamp seconds when the session started.
    pub started_at: u64,
    /// Unix timestamp seconds of the most recent update.
    pub updated_at: u64,
}

/// `router.log` — controllers push structured log lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRequest {
    /// Source peer (base58 PeerId).
    pub source_peer_id: String,
    /// `"info"` | `"warn"` | `"error"`.
    pub level: String,
    /// Free-form message; redaction is the source's responsibility.
    pub message: String,
    /// Optional task id this log entry pertains to.
    pub task_id: Option<String>,
    /// Unix timestamp seconds when the source produced the line.
    pub timestamp: u64,
}

/// Reply to `router.log`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogResponse {
    /// Always `"ok"` on accept.
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    #[test]
    fn heartbeat_round_trip() {
        let h = HeartbeatRequest {
            peer_id: "12D3KooW".into(),
            name: "tool-a".into(),
            capabilities: vec!["tool.web_fetch".into(), "tool.pdf".into()],
            timestamp: 1700000000,
            groups: vec!["controllers".into(), "org-a".into()],
        };
        let bytes = codec::encode(&h).unwrap();
        let back: HeartbeatRequest = codec::decode(&bytes).unwrap();
        assert_eq!(back.peer_id, "12D3KooW");
        assert_eq!(back.capabilities.len(), 2);
    }

    #[test]
    fn network_summary_round_trip() {
        let r = NetworkSummaryResponse {
            router_peer_id: "12D3KooWRouter".into(),
            router_name: "router".into(),
            peer_count: 3,
            peers: vec![PeerSummary {
                peer_id: "p1".into(),
                name: "tool".into(),
                capabilities: vec!["tool.read_file".into()],
                last_heartbeat_secs: 1700000000,
                healthy: true,
                groups: vec!["controllers".into()],
            }],
            active_sessions: 1,
            total_sessions_since_start: 42,
            uptime_secs: 9000,
            timestamp: 1700000010,
        };
        let bytes = codec::encode(&r).unwrap();
        let back: NetworkSummaryResponse = codec::decode(&bytes).unwrap();
        assert_eq!(back.peer_count, 3);
        assert_eq!(back.total_sessions_since_start, 42);
    }

    #[test]
    fn session_record_round_trip() {
        let s = SessionRecord {
            session_id: "s-001".into(),
            source_peer_id: "p1".into(),
            workflow_name: "do_work".into(),
            capabilities_used: vec!["tool.web_get".into(), "memory.put".into()],
            route: vec!["p1".into(), "p2".into(), "p3".into()],
            status: "running".into(),
            started_at: 1700000000,
            updated_at: 1700000050,
        };
        let bytes = codec::encode(&s).unwrap();
        let back: SessionRecord = codec::decode(&bytes).unwrap();
        assert_eq!(back.route.len(), 3);
        assert_eq!(back.status, "running");
    }

    #[test]
    fn log_request_round_trip() {
        let l = LogRequest {
            source_peer_id: "p1".into(),
            level: "warn".into(),
            message: "took longer than expected".into(),
            task_id: Some("t-42".into()),
            timestamp: 1700000000,
        };
        let bytes = codec::encode(&l).unwrap();
        let back: LogRequest = codec::decode(&bytes).unwrap();
        assert_eq!(back.level, "warn");
        assert_eq!(back.task_id.as_deref(), Some("t-42"));
    }

    #[test]
    fn org_filter_optional_in_request() {
        let bytes = codec::encode(&NetworkSummaryRequest::default()).unwrap();
        let back: NetworkSummaryRequest = codec::decode(&bytes).unwrap();
        assert!(back.org_filter.is_none());
    }
}
