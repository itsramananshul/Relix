# Implementation Prompt — Relix Router Node

Paste this entire prompt directly into Claude Code.

---

## Context

You are working in the Relix codebase at `D:\DATA\WORK\OpenPrem\Apps\Relix\`.

Relix is a production-grade decentralized AI agent platform. Every node is a
`relix-controller` binary. Nodes run as libp2p peers on a mesh — they discover
each other via Kademlia DHT, authenticate with Ed25519 IdentityBundles, enforce
policy via Cedar (coming), and audit every RPC via hash-chained event logs.

You are adding a **Router Node** — a new role for the controller binary that acts
as the network's observability, health, and operator control plane. It is the
single endpoint external operators and monitoring systems connect to. It tracks
peer health, session routes, aggregated logs, and mesh capability inventory.

This is a production feature for multi-org deployments. It must:
- Pass all existing tests (`cargo test --workspace` stays green)
- Follow existing patterns in `crates/relix-core/` and `crates/relix-runtime/`
- Never break the 5 architecture invariants (see below)
- Be config-differentiated: same binary, different `role` field

## Architecture Invariants — DO NOT VIOLATE

1. The responding node enforces: identity → policy → handler → audit on every RPC.
2. AI provider keys live ONLY in the AI node's local config.
3. The web backend in RELIX_MODE makes zero LLM provider calls.
4. No routing decision outside SOL flows.
5. Adding a new channel node requires zero changes to memory/AI/tool/web nodes.

---

## What to Build

### Step 1 — Add `role` and `router_peer_id` to the controller config

File: `crates/relix-runtime/src/controller_runtime.rs` (or wherever
`ControllerConfig` is defined — search the codebase for `ControllerConfig` or
`[controller]` TOML section parsing).

Add these fields to the `[controller]` TOML section struct:

```rust
/// "controller" (default) or "router"
#[serde(default = "default_role")]
pub role: String,

/// Non-router nodes: the libp2p PeerId of the designated router node.
/// Format: base58 multiaddr string, e.g. "12D3KooW..."
pub router_peer_id: Option<String>,

/// Router-only: seconds to retain completed/failed sessions (default 1800 = 30 min)
#[serde(default = "default_session_ttl")]
pub session_ttl_secs: u64,

fn default_role() -> String { "controller".into() }
fn default_session_ttl() -> u64 { 1800 }
```

Add example config files:

`configs/router-node.toml`:
```toml
[controller]
name = "router"
node_type = "router"
listen_port = 9010
role = "router"
session_ttl_secs = 1800

[identity]
key_path = "dev-keys/router.key"

[trust]
org_root_key_path = "dev-keys/org-root.pub"

[policy]
file = "configs/policies/router.toml"

[peers]
```

`configs/policies/router.toml`:
```toml
[admit]
groups = ["operators", "controllers"]

[[rules]]
name = "controllers_heartbeat"
method = "router.heartbeat"
allow_groups = ["controllers"]

[[rules]]
name = "operators_network_summary"
method = "router.network_summary"
allow_groups = ["operators"]

[[rules]]
name = "operators_session_list"
method = "router.session_list"
allow_groups = ["operators"]

[[rules]]
name = "controllers_log"
method = "router.log"
allow_groups = ["controllers"]
```

Update existing controller configs (`configs/local-server.toml` and any others)
to add:
```toml
[controller]
# ... existing fields ...
role = "controller"
router_peer_id = ""   # fill in with router's peer_id in production
```

---

### Step 2 — Define Router RPC types in `relix-core`

Create `crates/relix-core/src/router.rs`:

```rust
//! Router node RPC types — CBOR-serializable request/response pairs
//! for the four router capabilities.

use serde::{Deserialize, Serialize};

/// Sent by controller nodes every 60 seconds to router.heartbeat
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// This controller's libp2p PeerId (base58)
    pub peer_id: String,
    /// Human-readable name from config
    pub name: String,
    /// All capability method strings this node handles
    pub capabilities: Vec<String>,
    /// Unix timestamp seconds
    pub timestamp: u64,
    /// Org identity group memberships (from IdentityBundle)
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub status: String,
    /// Router echoes all currently known healthy peers back
    pub peers: Vec<PeerSummary>,
}

/// Lightweight peer record returned in heartbeat responses and network summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSummary {
    pub peer_id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub last_heartbeat_secs: u64,
    pub healthy: bool,
    pub groups: Vec<String>,
}

/// router.network_summary — operator-facing mesh overview
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSummaryRequest {
    /// Optional org filter — only return peers from this org. Empty = all orgs caller can see.
    pub org_filter: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkSummaryResponse {
    pub router_peer_id: String,
    pub router_name: String,
    pub peer_count: usize,
    pub peers: Vec<PeerSummary>,
    pub active_sessions: usize,
    pub total_sessions_since_start: u64,
    pub uptime_secs: u64,
    pub timestamp: u64,
}

/// router.session_list — operator-facing session browser
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListRequest {
    pub status_filter: Option<String>,   // "running" | "completed" | "failed" | None = all
    pub limit: Option<usize>,            // default 100
    pub offset: Option<usize>,           // default 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionListResponse {
    pub sessions: Vec<SessionRecord>,
    pub total: usize,
}

/// One session record (a workflow hop-trace across peers)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub source_peer_id: String,
    pub workflow_name: String,
    pub capabilities_used: Vec<String>,
    /// Ordered list of peer_ids the workflow visited
    pub route: Vec<String>,
    pub status: String,
    pub started_at: u64,
    pub updated_at: u64,
}

/// router.log — controllers push structured log lines to the router
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRequest {
    pub source_peer_id: String,
    pub level: String,       // "info" | "warn" | "error"
    pub message: String,
    pub task_id: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogResponse {
    pub status: String,
}
```

Add `pub mod router;` to `crates/relix-core/src/lib.rs`.

---

### Step 3 — Implement `RouterNode` handler in `relix-runtime`

Create `crates/relix-runtime/src/nodes/router.rs`:

This file implements the router node's in-memory state and the four capability
handlers. Follow the exact same pattern as `crates/relix-runtime/src/nodes/`
existing node implementations (look at how memory node or coordinator node is
structured — same `Handler` trait, same dispatch pattern).

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

use relix_core::router::{
    HeartbeatRequest, HeartbeatResponse, LogRequest, LogResponse,
    NetworkSummaryRequest, NetworkSummaryResponse, PeerSummary,
    SessionListRequest, SessionListResponse, SessionRecord,
};

/// Stale threshold: mark unhealthy if no heartbeat in 90 seconds (1.5x the 60s send interval)
const STALE_TIMEOUT_SECS: u64 = 90;

/// Session TTL default: 30 minutes for completed/failed sessions
const DEFAULT_SESSION_TTL_SECS: u64 = 1800;

pub struct RouterState {
    /// peer_id → PeerRecord
    pub peers: HashMap<String, PeerRecord>,
    /// session_id → SessionRecord
    pub sessions: HashMap<String, SessionRecord>,
    /// Aggregated log lines from all peers
    pub logs: Vec<LogLine>,
    /// Session TTL for completed/failed sessions
    pub session_ttl: Duration,
    /// Router's own peer_id
    pub router_peer_id: String,
    /// Human-readable name
    pub router_name: String,
    /// When the router started (Unix secs)
    pub started_at: u64,
    /// Total sessions ever registered (monotonic counter, never decremented on reap)
    pub total_sessions: u64,
}

pub struct PeerRecord {
    pub peer_id: String,
    pub name: String,
    pub capabilities: Vec<String>,
    pub last_heartbeat: u64,
    pub healthy: bool,
    pub groups: Vec<String>,
}

pub struct LogLine {
    pub source_peer_id: String,
    pub level: String,
    pub message: String,
    pub task_id: Option<String>,
    pub timestamp: u64,
}

impl RouterState {
    pub fn new(router_peer_id: String, router_name: String, session_ttl_secs: u64) -> Self {
        let started_at = now_secs();
        Self {
            peers: HashMap::new(),
            sessions: HashMap::new(),
            logs: Vec::new(),
            session_ttl: Duration::from_secs(session_ttl_secs),
            router_peer_id,
            router_name,
            started_at,
            total_sessions: 0,
        }
    }

    /// Handle router.heartbeat
    pub fn handle_heartbeat(&mut self, req: HeartbeatRequest) -> HeartbeatResponse {
        let ts = now_secs();
        self.peers.insert(req.peer_id.clone(), PeerRecord {
            peer_id: req.peer_id.clone(),
            name: req.name,
            capabilities: req.capabilities,
            last_heartbeat: ts,
            healthy: true,
            groups: req.groups,
        });

        let peers = self.healthy_peers_summary();
        HeartbeatResponse { status: "ok".into(), peers }
    }

    /// Handle router.network_summary
    pub fn handle_network_summary(&self, req: NetworkSummaryRequest) -> NetworkSummaryResponse {
        let peers: Vec<PeerSummary> = self.peers.values()
            .filter(|p| {
                if let Some(ref org) = req.org_filter {
                    p.groups.iter().any(|g| g.contains(org.as_str()))
                } else {
                    true
                }
            })
            .map(|p| peer_summary(p))
            .collect();

        NetworkSummaryResponse {
            router_peer_id: self.router_peer_id.clone(),
            router_name: self.router_name.clone(),
            peer_count: peers.len(),
            peers,
            active_sessions: self.sessions.values().filter(|s| s.status == "running").count(),
            total_sessions_since_start: self.total_sessions,
            uptime_secs: now_secs().saturating_sub(self.started_at),
            timestamp: now_secs(),
        }
    }

    /// Handle router.session_list
    pub fn handle_session_list(&self, req: SessionListRequest) -> SessionListResponse {
        let limit = req.limit.unwrap_or(100);
        let offset = req.offset.unwrap_or(0);

        let mut sessions: Vec<&SessionRecord> = self.sessions.values()
            .filter(|s| {
                req.status_filter.as_ref()
                    .map(|f| &s.status == f)
                    .unwrap_or(true)
            })
            .collect();

        // Sort by started_at descending (newest first)
        sessions.sort_by(|a, b| b.started_at.cmp(&a.started_at));

        let total = sessions.len();
        let sessions: Vec<SessionRecord> = sessions
            .into_iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();

        SessionListResponse { sessions, total }
    }

    /// Handle router.log
    pub fn handle_log(&mut self, req: LogRequest) -> LogResponse {
        self.logs.push(LogLine {
            source_peer_id: req.source_peer_id,
            level: req.level,
            message: req.message,
            task_id: req.task_id,
            timestamp: req.timestamp,
        });
        // Keep only last 10,000 log lines in memory
        if self.logs.len() > 10_000 {
            self.logs.drain(0..self.logs.len() - 10_000);
        }
        LogResponse { status: "ok".into() }
    }

    /// Register or update a session (called when a workflow hop is recorded)
    pub fn track_session(&mut self, session: SessionRecord) {
        if !self.sessions.contains_key(&session.session_id) {
            self.total_sessions += 1;
        }
        self.sessions.insert(session.session_id.clone(), session);
    }

    /// Mark stale peers: healthy = false if no heartbeat in STALE_TIMEOUT_SECS
    pub fn reap_stale_peers(&mut self) {
        let now = now_secs();
        for peer in self.peers.values_mut() {
            if now.saturating_sub(peer.last_heartbeat) > STALE_TIMEOUT_SECS {
                peer.healthy = false;
            }
        }
    }

    /// Remove expired completed/failed sessions
    pub fn reap_expired_sessions(&mut self) {
        let now = now_secs();
        let ttl = self.session_ttl.as_secs();
        self.sessions.retain(|_, s| {
            if s.status == "running" { return true; }
            now.saturating_sub(s.updated_at) <= ttl
        });
    }

    fn healthy_peers_summary(&self) -> Vec<PeerSummary> {
        self.peers.values().map(peer_summary).collect()
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
```

Add the handler registration to `crates/relix-runtime/src/nodes/mod.rs` — follow
the existing pattern for how other node types register their capabilities with the
dispatch layer.

The four capability method strings to register:
- `router.heartbeat`
- `router.network_summary`
- `router.session_list`
- `router.log`

---

### Step 4 — Wire into `controller_runtime.rs`

In `crates/relix-runtime/src/controller_runtime.rs`, in the `run()` function:

1. After reading config, check `config.controller.role`:
   - If `"router"`: initialize `RouterState`, skip the heartbeat sender background task, start the stale-peer reaper (30s interval) and session reaper (300s interval)
   - If `"controller"` (default): initialize the heartbeat sender background task

2. **Heartbeat sender** (controller role only):
   - If `config.controller.router_peer_id` is set and non-empty:
   - Wait 1.5 seconds after startup, then fire the initial heartbeat
   - Then spawn a task that loops with a 60-second `tokio::time::interval`
   - Each tick: collect `peer_id`, `name`, capabilities list, groups from the loaded IdentityBundle
   - Send `router.heartbeat` RPC to the router's peer_id via the existing libp2p transport
   - Log success at DEBUG, log failure at WARN (do not crash — the router being down is non-fatal)

3. **Stale peer reaper** (router role only):
   - 30-second `tokio::time::interval`
   - Each tick: call `router_state.reap_stale_peers()`

4. **Session reaper** (router role only):
   - 300-second `tokio::time::interval`
   - Each tick: call `router_state.reap_expired_sessions()`

---

### Step 5 — Register capabilities in the CapabilityDescriptor

In `crates/relix-core/src/capability.rs` (or wherever capabilities are defined),
add descriptors for all four router capabilities. Follow the exact same pattern as
existing capabilities. Each descriptor needs: method string, description, categories,
environment_requirements.

```
router.heartbeat       — category: "router", "health"
router.network_summary — category: "router", "observability"  
router.session_list    — category: "router", "observability"
router.log             — category: "router", "observability"
```

---

### Step 6 — Tests

Create `crates/relix-runtime/tests/router_node.rs`:

Test 1 — `heartbeat_registers_peer`: Create a `RouterState`, call `handle_heartbeat`
with a known peer_id and capabilities. Assert the peer appears in `router_state.peers`
with `healthy = true`.

Test 2 — `stale_peer_marked_unhealthy`: Register a peer with `last_heartbeat` set
to `now_secs() - 100` (91 seconds ago, past the 90s threshold). Call `reap_stale_peers()`.
Assert the peer's `healthy` field is now `false`.

Test 3 — `session_reap_respects_ttl`: Insert a completed session with `updated_at`
set to `now_secs() - 2000` (past 1800s default TTL). Insert a running session with
any `updated_at`. Call `reap_expired_sessions()`. Assert the completed session is gone,
the running session remains.

Test 4 — `network_summary_counts_active_sessions`: Insert 2 running sessions and
1 completed session. Call `handle_network_summary`. Assert `active_sessions == 2`.

Test 5 — `log_buffer_capped_at_10k`: Call `handle_log` 10,001 times. Assert
`router_state.logs.len() == 10_000`.

Test 6 — `org_filter_in_network_summary`: Register two peers — one with
`groups = ["org-a.controllers"]` and one with `groups = ["org-b.controllers"]`.
Call `handle_network_summary` with `org_filter = Some("org-a")`. Assert only one
peer is returned.

---

### Step 7 — CLI

In `crates/relix-cli/src/main.rs`, add a `router` subcommand:

```
relix-cli router status        → calls router.network_summary, pretty-prints peers + sessions
relix-cli router peers         → calls router.network_summary, prints peer table: PEER_ID | NAME | CAPS | HEALTHY | LAST_HEARTBEAT
relix-cli router sessions      → calls router.session_list, prints session table: ID | WORKFLOW | STATUS | ROUTE | AGE
relix-cli router sessions --status running
relix-cli router sessions --limit 50 --offset 0
```

Follow the exact same CLI pattern as the existing `relix-cli task` subcommands.

---

### Acceptance Criteria

When done, the following must all be true:

1. `cargo test --workspace` passes with zero failures
2. `cargo clippy --workspace --all-targets -- -D warnings` passes clean
3. A controller node started with `configs/router-node.toml` logs `"Starting controller with role: router"` and registers four capabilities: `router.heartbeat`, `router.network_summary`, `router.session_list`, `router.log`
4. A controller node started with `role = "controller"` and a valid `router_peer_id` begins sending heartbeats 1.5 seconds after startup and every 60 seconds thereafter
5. `relix-cli router status` returns a valid network summary from a running router node
6. All 6 new tests pass
7. No existing test is broken
8. No architecture invariant is violated — in particular, the router node NEVER makes LLM calls, NEVER holds provider keys, and ALL RPCs to the router are identity-verified and audited before the handler fires
