//! PH-BRIDGE-MCP-AUDIT — bridge-side observability ring for
//! `POST /v1/mcp/invoke` calls.
//!
//! Bounded in-memory ring. Every invocation through the bridge
//! pushes one entry (success or failure). Surfaced via
//! `GET /v1/mcp/audit` for dashboard / curl consumption.
//!
//! ## Privacy / size posture
//!
//! Entries store:
//! - `ts_secs` — wall-clock seconds at the moment of completion.
//! - `peer_alias` — which tool peer was dialed.
//! - `server_id` + `tool_name` — what was invoked.
//! - `args_len` — caller's raw arg byte count. **Not the args
//!   themselves** — they may carry secrets / large payloads.
//!   Operators correlating with full args use the bridge's
//!   intervention-audit log or a future per-call trace id.
//! - `outcome` — `"ok"` or `"err"`.
//! - `error_kind` — only set when `outcome == "err"`; the
//!   responder envelope kind (e.g. `"runtime_not_connected"`).
//! - `duration_ms` — wall-clock from call start to response.
//!
//! Ring is bounded at 256 entries; oldest evicted first. Resets
//! on bridge restart — same posture as the lifecycle ring and
//! the stream metrics ring.
//!
//! ## What this is NOT
//!
//! - **Not the chronicle.** The ring is bridge-process
//!   observability. When `/v1/mcp/invoke` carries a task id, the
//!   handler also writes a best-effort task event and durable
//!   activity entry.
//! - **Not a per-caller log.** The bridge holds a single
//!   identity bundle; per-request caller identification lands
//!   when bridge auth ships.
//! - **Not a replay store.** Args are not recorded; this ring
//!   answers "what was invoked and how often" — not "what
//!   exactly did the agent send."

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

/// PH-BRIDGE-MCP-AUDIT: one observation. Cheap to clone.
#[derive(Debug, Clone, Serialize)]
pub struct McpAuditEntry {
    pub ts_secs: i64,
    pub peer_alias: String,
    pub server_id: String,
    pub tool_name: String,
    pub args_len: usize,
    /// `"ok"` or `"err"`.
    pub outcome: String,
    /// Set only when `outcome == "err"`. Matches the responder
    /// envelope kind (e.g. `"runtime_not_connected"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    pub duration_ms: u64,
}

/// PH-BRIDGE-MCP-AUDIT: bounded ring of [`McpAuditEntry`].
/// FIFO eviction. Mutex critical sections are held only for
/// short insert / snapshot transactions.
#[derive(Debug)]
pub struct McpAuditRing {
    entries: Mutex<VecDeque<McpAuditEntry>>,
    capacity: usize,
}

/// Default ring capacity. Same conventional bound as the
/// runtime-side `tool.fs.audit_recent` and `tool.terminal.audit_recent`
/// — a busy bridge can't hold an unbounded history in memory.
pub const MCP_AUDIT_RING_DEFAULT: usize = 256;

impl Default for McpAuditRing {
    fn default() -> Self {
        Self::new(MCP_AUDIT_RING_DEFAULT)
    }
}

impl McpAuditRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, e: McpAuditEntry) {
        let mut g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("'mcp audit ring poisoned'; recovering inner state");
            e.into_inner()
        });
        if g.len() == self.capacity {
            g.pop_front();
        }
        g.push_back(e);
    }

    /// Snapshot the most recent `max` entries, newest first.
    pub fn snapshot_newest_first(&self, max: usize) -> Vec<McpAuditEntry> {
        let g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("'mcp audit ring poisoned'; recovering inner state");
            e.into_inner()
        });
        g.iter().rev().take(max).cloned().collect()
    }

    /// Count of entries currently in the ring (saturates at
    /// `capacity`).
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'mcp audit ring poisoned'; recovering inner state");
                e.into_inner()
            })
            .len()
    }

    /// Convenience for tests + future operator surfaces — pairs
    /// with `len()` per clippy's `len_zero` lint. Kept `pub` so
    /// callers don't have to compare `len() == 0`.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(ts: i64, name: &str, outcome: &str) -> McpAuditEntry {
        McpAuditEntry {
            ts_secs: ts,
            peer_alias: "tool".into(),
            server_id: "srv".into(),
            tool_name: name.into(),
            args_len: 0,
            outcome: outcome.into(),
            error_kind: if outcome == "err" {
                Some("kind".into())
            } else {
                None
            },
            duration_ms: 1,
        }
    }

    #[test]
    fn fresh_ring_is_empty() {
        let r = McpAuditRing::default();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.snapshot_newest_first(10).is_empty());
    }

    #[test]
    fn push_then_snapshot_newest_first() {
        let r = McpAuditRing::default();
        r.push(entry(100, "a", "ok"));
        r.push(entry(200, "b", "ok"));
        r.push(entry(300, "c", "err"));
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].tool_name, "c");
        assert_eq!(snap[1].tool_name, "b");
        assert_eq!(snap[2].tool_name, "a");
    }

    #[test]
    fn ring_bounded_by_capacity() {
        let r = McpAuditRing::new(3);
        for i in 0..10 {
            r.push(entry(i, &format!("t{i}"), "ok"));
        }
        assert_eq!(r.len(), 3);
        let snap = r.snapshot_newest_first(10);
        // Most recent 3 retained — t9, t8, t7.
        assert_eq!(snap[0].tool_name, "t9");
        assert_eq!(snap[1].tool_name, "t8");
        assert_eq!(snap[2].tool_name, "t7");
    }

    #[test]
    fn snapshot_caps_at_max() {
        let r = McpAuditRing::default();
        for i in 0..5 {
            r.push(entry(i, &format!("t{i}"), "ok"));
        }
        let snap = r.snapshot_newest_first(2);
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].tool_name, "t4");
        assert_eq!(snap[1].tool_name, "t3");
    }

    #[test]
    fn error_kind_is_omitted_when_outcome_ok() {
        let e = entry(1, "x", "ok");
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("error_kind"));
    }

    #[test]
    fn error_kind_is_included_when_outcome_err() {
        let e = entry(1, "x", "err");
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""error_kind":"kind""#));
    }

    #[test]
    fn default_capacity_matches_const() {
        let r = McpAuditRing::default();
        for i in 0..(MCP_AUDIT_RING_DEFAULT + 50) {
            r.push(entry(i as i64, "x", "ok"));
        }
        assert_eq!(r.len(), MCP_AUDIT_RING_DEFAULT);
    }

    #[test]
    fn zero_capacity_clamps_to_one() {
        let r = McpAuditRing::new(0);
        r.push(entry(1, "x", "ok"));
        r.push(entry(2, "y", "ok"));
        // capacity clamped to 1 -> only newest survives.
        assert_eq!(r.len(), 1);
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap[0].tool_name, "y");
    }
}
