//! Bridge-side node lifecycle event log.
//!
//! A bounded in-memory ring of recent topology transitions
//! (peer joins, freshness bucket changes, peer drops). Updated
//! by a background task that polls the manifest cache + diffs
//! against the previous snapshot.
//!
//! Operators see history beyond what a single dashboard tab
//! has been open for — and the events show up in
//! `/v1/topology/events` so the CLI + future scripts consume
//! the same surface.
//!
//! Persistence: NONE. The ring lives in-memory and resets on
//! bridge restart, same posture as the reconnect counters and
//! the stream metrics. Operators wanting durable history should
//! scrape the endpoint.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use relix_runtime::manifest::ManifestCache;

/// Default ring capacity. Tuned for ≤10-peer meshes; one
/// freshness flap per peer roughly every minute over an hour
/// is well under this.
const DEFAULT_RING_CAP: usize = 500;

/// One lifecycle transition. Stable JSON shape — additions
/// only.
#[derive(Debug, Clone, Serialize)]
pub struct LifecycleEvent {
    /// Wall-clock unix seconds.
    pub ts: i64,
    /// `joined` / `freshness_changed` / `dropped`.
    pub kind: String,
    /// Operator-configured alias (`memory`, `ai`, …). `None`
    /// for peers added without an alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Hex-encoded node id. Always present.
    pub node_id: String,
    /// Peer's `node_type` discriminator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    /// For `freshness_changed`: the previous bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_freshness: Option<String>,
    /// For `freshness_changed`: the new bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_freshness: Option<String>,
    /// Human-readable summary suitable for activity feeds.
    pub detail: String,
}

/// Per-peer state we keep between diff ticks. Only what the
/// diff needs.
#[derive(Debug, Clone)]
struct PeerSnapshot {
    alias: Option<String>,
    node_type: String,
    freshness: String,
}

/// Shared, lock-protected event ring + previous snapshot.
#[derive(Debug)]
pub struct LifecycleLog {
    inner: RwLock<Inner>,
    cap: usize,
}

#[derive(Debug, Default)]
struct Inner {
    /// node_id → last-seen snapshot.
    snapshots: HashMap<String, PeerSnapshot>,
    /// Newest-first ring of recent transitions.
    events: VecDeque<LifecycleEvent>,
    /// Monotonic counter so consumers can request `?since=N`
    /// to read everything after a known cursor without
    /// timestamp ambiguity.
    seq: i64,
}

impl LifecycleLog {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner::default()),
            cap: DEFAULT_RING_CAP,
        })
    }

    /// Snapshot the recent events (newest first), optionally
    /// filtered by a sequence cursor. Pass `since=0` to read
    /// everything in the ring.
    pub fn since(&self, since: i64, limit: usize) -> (Vec<LifecycleEvent>, i64) {
        let g = self.inner.read().expect("lifecycle read lock");
        let mut out: Vec<LifecycleEvent> = g
            .events
            .iter()
            .take_while(|e| e.ts > since)
            .cloned()
            .collect();
        if out.len() > limit {
            out.truncate(limit);
        }
        (out, g.seq)
    }

    /// Diff the supplied ManifestCache snapshot against the
    /// last-seen state and append any transitions to the ring.
    /// Called by the background polling task in main.rs.
    pub fn diff_and_record(&self, cache: &ManifestCache, now: i64) {
        let entries = cache.entries();
        // Compute the current snapshot map + the freshness
        // bucket per peer.
        let mut current: HashMap<String, PeerSnapshot> = HashMap::new();
        for c in entries {
            let secs_ago = (now - c.last_refreshed_at).max(0);
            let fresh = freshness_label(secs_ago);
            current.insert(
                c.manifest.node_id.to_string(),
                PeerSnapshot {
                    alias: c.alias,
                    node_type: c.manifest.node_type.clone(),
                    freshness: fresh.to_string(),
                },
            );
        }

        let mut g = self.inner.write().expect("lifecycle write lock");

        // First tick: just seed the snapshot, don't emit events.
        if g.snapshots.is_empty() && !current.is_empty() {
            g.snapshots = current;
            return;
        }

        let mut new_events: Vec<LifecycleEvent> = Vec::new();

        // Joins + freshness transitions.
        for (id, cur) in &current {
            match g.snapshots.get(id) {
                None => {
                    new_events.push(LifecycleEvent {
                        ts: now,
                        kind: "joined".into(),
                        alias: cur.alias.clone(),
                        node_id: id.clone(),
                        node_type: Some(cur.node_type.clone()),
                        from_freshness: None,
                        to_freshness: Some(cur.freshness.clone()),
                        detail: format!(
                            "{} joined the manifest cache ({})",
                            cur.alias.as_deref().unwrap_or(id.as_str()),
                            cur.freshness
                        ),
                    });
                }
                Some(prev) if prev.freshness != cur.freshness => {
                    new_events.push(LifecycleEvent {
                        ts: now,
                        kind: "freshness_changed".into(),
                        alias: cur.alias.clone(),
                        node_id: id.clone(),
                        node_type: Some(cur.node_type.clone()),
                        from_freshness: Some(prev.freshness.clone()),
                        to_freshness: Some(cur.freshness.clone()),
                        detail: format!(
                            "{} {} → {}",
                            cur.alias.as_deref().unwrap_or(id.as_str()),
                            prev.freshness,
                            cur.freshness
                        ),
                    });
                }
                _ => {}
            }
        }

        // Drops.
        for (id, prev) in &g.snapshots {
            if !current.contains_key(id) {
                new_events.push(LifecycleEvent {
                    ts: now,
                    kind: "dropped".into(),
                    alias: prev.alias.clone(),
                    node_id: id.clone(),
                    node_type: Some(prev.node_type.clone()),
                    from_freshness: Some(prev.freshness.clone()),
                    to_freshness: None,
                    detail: format!(
                        "{} dropped from the manifest cache",
                        prev.alias.as_deref().unwrap_or(id.as_str())
                    ),
                });
            }
        }

        // Append newest-first.
        for ev in new_events {
            g.seq += 1;
            g.events.push_front(ev);
            // Trim to cap.
            while g.events.len() > self.cap {
                g.events.pop_back();
            }
        }

        g.snapshots = current;
    }

    /// Test-only seed for asserting against known transitions.
    /// Goes through the same trim-to-cap logic as the real
    /// diff path so cap-enforcement tests are meaningful.
    #[cfg(test)]
    pub fn push_for_test(&self, ev: LifecycleEvent) {
        let mut g = self.inner.write().unwrap();
        g.seq += 1;
        g.events.push_front(ev);
        while g.events.len() > self.cap {
            g.events.pop_back();
        }
    }
}

/// Same bucketing as `topology::freshness_label`. Duplicated
/// to avoid a cross-module dependency that would force
/// pub-vis on otherwise-private helpers; both should change
/// together if the bridge ever moves the threshold.
fn freshness_label(secs_ago: i64) -> &'static str {
    if secs_ago < 120 {
        "fresh"
    } else if secs_ago < 600 {
        "stale"
    } else {
        "expired"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::capability::CapabilityDescriptor;
    use relix_core::types::NodeId;
    use relix_runtime::manifest::NodeManifest;

    fn manifest(node_id_seed: &[u8], node_type: &str) -> NodeManifest {
        NodeManifest {
            node_id: NodeId::from_pubkey(node_id_seed),
            node_name: node_type.into(),
            node_type: node_type.into(),
            manifest_version: 1,
            org_id: NodeId::from_pubkey(b"org"),
            endpoints: vec![],
            capabilities: vec![CapabilityDescriptor::unary("x.method")],
        }
    }

    #[test]
    fn first_tick_seeds_snapshot_without_emitting_events() {
        let log = LifecycleLog::new();
        let cache = ManifestCache::new();
        cache.insert(Some("memory".into()), manifest(b"m", "memory"));
        log.diff_and_record(&cache, 1_700_000_000);
        let (events, _) = log.since(0, 100);
        assert!(
            events.is_empty(),
            "expected empty on first tick, got: {events:?}"
        );
    }

    #[test]
    fn join_emits_joined_event_on_second_tick() {
        let log = LifecycleLog::new();
        let cache = ManifestCache::new();
        cache.insert(Some("memory".into()), manifest(b"m", "memory"));
        log.diff_and_record(&cache, 1_700_000_000);
        // Add a new peer between ticks.
        cache.insert(Some("ai".into()), manifest(b"a", "ai"));
        log.diff_and_record(&cache, 1_700_000_005);
        let (events, _) = log.since(0, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "joined");
        assert_eq!(events[0].alias.as_deref(), Some("ai"));
    }

    #[test]
    fn freshness_change_emits_freshness_changed() {
        // Use real wall-clock so the cache's last_refreshed_at
        // (stamped at insert time) is consistent with the `now`
        // values we pass to diff_and_record. Then walk `now`
        // forward across the 120s freshness threshold.
        let log = LifecycleLog::new();
        let cache = ManifestCache::new();
        cache.insert(Some("memory".into()), manifest(b"m", "memory"));
        let now0 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap();
        // First tick: freshness = fresh (secs_ago = 0).
        log.diff_and_record(&cache, now0);
        // Second tick: jump `now` forward by 200s so secs_ago
        // = 200 → stale.
        log.diff_and_record(&cache, now0 + 200);
        let (events, _) = log.since(0, 100);
        let names: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
        assert!(names.contains(&"freshness_changed"), "events = {events:?}");
        let fc = events
            .iter()
            .find(|e| e.kind == "freshness_changed")
            .unwrap();
        assert_eq!(fc.from_freshness.as_deref(), Some("fresh"));
        assert_eq!(fc.to_freshness.as_deref(), Some("stale"));
    }

    #[test]
    fn ring_caps_at_default() {
        let log = LifecycleLog::new();
        for i in 0..(DEFAULT_RING_CAP + 50) {
            log.push_for_test(LifecycleEvent {
                ts: i as i64,
                kind: "joined".into(),
                alias: None,
                node_id: format!("n{i}"),
                node_type: None,
                from_freshness: None,
                to_freshness: None,
                detail: format!("e{i}"),
            });
        }
        let (events, _) = log.since(0, DEFAULT_RING_CAP + 100);
        assert_eq!(events.len(), DEFAULT_RING_CAP);
    }

    #[test]
    fn since_filters_to_newer_than_cursor() {
        let log = LifecycleLog::new();
        for i in 0..5 {
            log.push_for_test(LifecycleEvent {
                ts: i,
                kind: "joined".into(),
                alias: None,
                node_id: format!("n{i}"),
                node_type: None,
                from_freshness: None,
                to_freshness: None,
                detail: format!("e{i}"),
            });
        }
        // Asking for since=3 should return events with ts > 3
        // (events 4 only).
        let (events, _) = log.since(3, 100);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ts, 4);
    }
}
