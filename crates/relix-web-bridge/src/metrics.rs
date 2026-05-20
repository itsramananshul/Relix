//! Bridge-side lightweight runtime metrics.
//!
//! Today: SSE stream counters (active / total opened) used by
//! `/v1/health` so the dashboard can surface live stream
//! visibility. Distinct from the per-task chronicle which lives
//! on the Coordinator; these are bridge-process-local stats.
//!
//! Counters reset on bridge restart — like
//! `MeshClient::reconnect_counters`. Operators wanting durable
//! trend data should scrape `/v1/health` from an external
//! collector.

use serde::Serialize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// One currently-open SSE stream. Tagged with the task id +
/// when the stream started so operators can see "who's
/// watching what" without keeping every dashboard tab open.
#[derive(Debug, Clone, Serialize)]
pub struct ActiveStream {
    /// Monotonic per-process stream id. Wraps at u64::MAX
    /// (won't happen in practice).
    pub id: u64,
    /// Task id the stream is following.
    pub task_id: String,
    /// Wall-clock unix seconds when the stream opened.
    pub opened_at: i64,
}

#[derive(Debug, Default)]
pub struct StreamMetrics {
    /// Live count of currently-open SSE streams against
    /// `/v1/tasks/:id/events/stream`. Incremented when a stream
    /// handler enters its loop; decremented when the handler's
    /// future is dropped (client disconnect or terminal event).
    active: AtomicU64,
    /// Total number of streams that have ever been opened.
    /// Useful for "the dashboard reconnected N times" telemetry.
    opened_total: AtomicU64,
    /// Monotonic per-process stream id allocator.
    next_id: AtomicU64,
    /// Per-stream detail. Locked separately from the atomic
    /// counters so the hot-path (active load) stays
    /// uncontended; only the open/close path takes this lock.
    streams: Mutex<Vec<ActiveStream>>,
}

impl StreamMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    pub fn opened_total(&self) -> u64 {
        self.opened_total.load(Ordering::Relaxed)
    }

    /// Snapshot the current set of active streams.
    pub fn list_active(&self) -> Vec<ActiveStream> {
        self.streams.lock().expect("stream-detail lock").clone()
    }

    /// Returns an RAII guard that registers a new active
    /// stream against `task_id` and removes it on drop.
    /// Pin it inside an `async-stream` body so the
    /// lifecycle is tied to the stream's future.
    pub fn open(self: &Arc<Self>, task_id: String, opened_at: i64) -> StreamGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.active.fetch_add(1, Ordering::Relaxed);
        self.opened_total.fetch_add(1, Ordering::Relaxed);
        let entry = ActiveStream {
            id,
            task_id,
            opened_at,
        };
        self.streams.lock().expect("stream-detail lock").push(entry);
        StreamGuard {
            metrics: Arc::clone(self),
            id,
        }
    }
}

pub struct StreamGuard {
    metrics: Arc<StreamMetrics>,
    id: u64,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::Relaxed);
        // Remove this stream's detail entry. Linear scan but
        // the active count is operator-mesh-scale (≤ tens),
        // not request-scale — this is fine.
        let mut g = self.metrics.streams.lock().expect("stream-detail lock");
        if let Some(pos) = g.iter().position(|s| s.id == self.id) {
            g.swap_remove(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_increments_and_drop_decrements() {
        let m = StreamMetrics::new();
        assert_eq!(m.active(), 0);
        assert_eq!(m.opened_total(), 0);
        let g1 = m.open("abc".into(), 1000);
        assert_eq!(m.active(), 1);
        assert_eq!(m.opened_total(), 1);
        let g2 = m.open("def".into(), 1001);
        assert_eq!(m.active(), 2);
        assert_eq!(m.opened_total(), 2);
        drop(g1);
        assert_eq!(m.active(), 1);
        assert_eq!(m.opened_total(), 2);
        drop(g2);
        assert_eq!(m.active(), 0);
        // opened_total never goes down.
        assert_eq!(m.opened_total(), 2);
    }

    #[test]
    fn opened_total_monotonic_across_drops() {
        let m = StreamMetrics::new();
        for i in 0..5 {
            let _ = m.open(format!("t{i}"), i);
        }
        assert_eq!(m.active(), 0);
        assert_eq!(m.opened_total(), 5);
    }

    #[test]
    fn list_active_reflects_open_streams() {
        let m = StreamMetrics::new();
        let _g1 = m.open("task-a".into(), 1700000000);
        let _g2 = m.open("task-b".into(), 1700000005);
        let active = m.list_active();
        assert_eq!(active.len(), 2);
        let ids: Vec<&str> = active.iter().map(|s| s.task_id.as_str()).collect();
        assert!(ids.contains(&"task-a"));
        assert!(ids.contains(&"task-b"));
    }

    #[test]
    fn drop_removes_from_list_active() {
        let m = StreamMetrics::new();
        let g1 = m.open("task-a".into(), 1700000000);
        let _g2 = m.open("task-b".into(), 1700000005);
        assert_eq!(m.list_active().len(), 2);
        drop(g1);
        let remaining = m.list_active();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].task_id, "task-b");
    }

    #[test]
    fn stream_ids_are_monotonic() {
        let m = StreamMetrics::new();
        let g1 = m.open("a".into(), 1);
        let g2 = m.open("b".into(), 2);
        let g3 = m.open("c".into(), 3);
        let active = m.list_active();
        // Ids are assigned in open() call order.
        let mut ids: Vec<u64> = active.iter().map(|s| s.id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 1, 2]);
        drop(g1);
        drop(g2);
        drop(g3);
    }
}
