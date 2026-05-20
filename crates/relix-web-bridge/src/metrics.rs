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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

    /// Returns an RAII guard that increments `active` +
    /// `opened_total` on construction and decrements `active`
    /// on drop. Pin it inside an `async-stream` body so the
    /// lifecycle is tied to the stream's future.
    pub fn open(self: &Arc<Self>) -> StreamGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.opened_total.fetch_add(1, Ordering::Relaxed);
        StreamGuard {
            metrics: Arc::clone(self),
        }
    }
}

pub struct StreamGuard {
    metrics: Arc<StreamMetrics>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::Relaxed);
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
        let g1 = m.open();
        assert_eq!(m.active(), 1);
        assert_eq!(m.opened_total(), 1);
        let g2 = m.open();
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
        for _ in 0..5 {
            let _ = m.open();
        }
        assert_eq!(m.active(), 0);
        assert_eq!(m.opened_total(), 5);
    }
}
