//! Real-time log surface for the dashboard.
//!
//! The bridge already routes every tracing event to stdout via
//! `tracing_subscriber::fmt()`. This module adds a second sink
//! that keeps the last 500 formatted lines in a process-wide
//! ring buffer AND broadcasts every fresh line to any number of
//! subscribers. Dashboard Section 18 (`GET /v1/logs/stream`)
//! opens an SSE stream that:
//!
//!   1. Drains the ring buffer first (so the operator lands on
//!      recent context, not on an empty pane).
//!   2. Tails the broadcast channel for every new line until the
//!      browser closes the connection.
//!
//! The fmt layer is unchanged — stdout still gets every event
//! verbatim. The dashboard sink is additive.
//!
//! ## Threading model
//!
//! The ring is `Arc<Mutex<VecDeque<LogLine>>>` (the per-write
//! critical section is a deque push + counter bump; never
//! contended for more than a few microseconds). The broadcast
//! channel is `tokio::sync::broadcast` with capacity 1024 — a
//! slow subscriber sees `Lagged` errors and skips, never wedges
//! the producer.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{Event as TracingEvent, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// One captured log line as the dashboard sees it.
#[derive(Clone, Debug, Serialize)]
pub struct LogLine {
    /// Unix milliseconds when the tracing event fired on the
    /// publishing thread.
    pub ts_ms: i64,
    /// Tracing level — `"ERROR"`, `"WARN"`, `"INFO"`, `"DEBUG"`,
    /// `"TRACE"` — uppercased to match common log-viewer
    /// colour palettes.
    pub level: String,
    /// `module_path!()` of the publishing site (e.g.
    /// `relix_web_bridge::chat`).
    pub target: String,
    /// The event's main `message` field plus any `key=value`
    /// fields appended `key=value` pairs. Plain text, never JSON.
    pub message: String,
}

/// Ring capacity (lines retained in memory for replay on new
/// subscribers). Matches the dashboard spec's "last 500 lines".
pub const RING_CAPACITY: usize = 500;

/// Broadcast channel capacity. Larger than the ring so a brief
/// burst doesn't immediately push slow subscribers into
/// `Lagged`.
pub const BROADCAST_CAPACITY: usize = 1024;

/// Shared handle the tracing layer writes into and the SSE
/// handler reads from. Cheap to clone.
#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
    tx: broadcast::Sender<LogLine>,
}

impl LogRing {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
            tx,
        }
    }

    /// Push one line. Drops the oldest when at capacity. Always
    /// broadcasts; subscribers that have died simply consume the
    /// `Closed` signal next time they poll.
    pub fn push(&self, line: LogLine) {
        if let Ok(mut g) = self.inner.lock() {
            if g.len() >= RING_CAPACITY {
                g.pop_front();
            }
            g.push_back(line.clone());
        }
        // `send` returns Err only when there are zero receivers —
        // expected (no dashboard tab open) and not actionable.
        let _ = self.tx.send(line);
    }

    /// Snapshot the current ring contents. The returned `Vec`
    /// reads oldest → newest so the dashboard can render them
    /// top-down without re-sorting.
    pub fn snapshot(&self) -> Vec<LogLine> {
        match self.inner.lock() {
            Ok(g) => g.iter().cloned().collect(),
            Err(p) => p.into_inner().iter().cloned().collect(),
        }
    }

    /// New broadcast receiver. The dashboard handler holds one
    /// per active SSE connection.
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.tx.subscribe()
    }
}

impl Default for LogRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracing-subscriber Layer that pipes every event into a
/// [`LogRing`]. Composes with the existing fmt layer (stdout)
/// via `tracing_subscriber::registry().with(fmt).with(ring)`.
pub struct LogRingLayer {
    ring: LogRing,
}

impl LogRingLayer {
    pub fn new(ring: LogRing) -> Self {
        Self { ring }
    }
}

impl<S> Layer<S> for LogRingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &TracingEvent<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut message_buf = String::new();
        let mut visitor = MessageVisitor {
            buf: &mut message_buf,
        };
        event.record(&mut visitor);
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        let line = LogLine {
            ts_ms,
            level: metadata.level().to_string().to_uppercase(),
            target: metadata.target().to_string(),
            message: message_buf,
        };
        self.ring.push(line);
    }
}

/// Concatenates the canonical `message` field plus any
/// additional `field=value` pairs into a single plain-text
/// string. Same flat shape `fmt::layer()` produces, minus the
/// colour codes.
struct MessageVisitor<'a> {
    buf: &'a mut String,
}

impl<'a> tracing::field::Visit for MessageVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            // `message` is special-cased: it carries the event's
            // primary text and lands without a key prefix, so the
            // result reads naturally.
            if !self.buf.is_empty() {
                self.buf.push(' ');
            }
            let _ = write!(self.buf, "{value:?}");
        } else {
            if !self.buf.is_empty() {
                self.buf.push(' ');
            }
            let _ = write!(self.buf, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            if !self.buf.is_empty() {
                self.buf.push(' ');
            }
            self.buf.push_str(value);
        } else {
            if !self.buf.is_empty() {
                self.buf.push(' ');
            }
            let _ = write!(self.buf, "{}={value}", field.name());
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        if !self.buf.is_empty() {
            self.buf.push(' ');
        }
        let _ = write!(self.buf, "{}={value}", field.name());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if !self.buf.is_empty() {
            self.buf.push(' ');
        }
        let _ = write!(self.buf, "{}={value}", field.name());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if !self.buf.is_empty() {
            self.buf.push(' ');
        }
        let _ = write!(self.buf, "{}={value}", field.name());
    }
}

/// `GET /v1/logs/stream` — Server-Sent Events stream of bridge
/// logs. Emits one `event: log` per line. The dashboard
/// (Section 18) consumes this with `EventSource`.
///
/// Frame shape:
/// ```text
/// event: log
/// data: {"ts_ms":..., "level":"INFO", "target":"...", "message":"..."}
///
/// ```
///
/// The handler:
///   1. Drains the ring buffer first so the dashboard lands on
///      ~500 lines of recent context.
///   2. Subscribes to the live broadcast and forwards every new
///      line.
///   3. Sends a keep-alive comment every 15s so reverse proxies
///      don't close idle connections.
pub async fn stream(
    State(state): State<crate::config::AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let ring = state.log_ring.clone();
    let snapshot = ring.snapshot();
    let rx = ring.subscribe();
    let s = async_stream::stream! {
        // Replay the ring first. JSON-encode each line; if
        // encoding fails (shouldn't — LogLine is a plain
        // struct) skip the line rather than aborting the
        // stream.
        for line in snapshot {
            if let Ok(payload) = serde_json::to_string(&line) {
                yield Ok(Event::default().event("log").data(payload));
            }
        }
        // Then tail the broadcast directly. `Lagged` means we
        // dropped lines for a slow subscriber — skip and keep
        // pulling. `Closed` means the producer dropped its
        // sender (process is shutting down) — end the stream.
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(line) => {
                    if let Ok(payload) = serde_json::to_string(&line) {
                        yield Ok(Event::default().event("log").data(payload));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(s).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{Registry, fmt};

    /// The ring drops the oldest entry once it's at capacity and
    /// keeps the cap stable thereafter.
    #[test]
    fn ring_caps_at_capacity_and_drops_oldest_first() {
        let ring = LogRing::new();
        for i in 0..(RING_CAPACITY + 7) {
            ring.push(LogLine {
                ts_ms: i as i64,
                level: "INFO".into(),
                target: "t".into(),
                message: format!("msg{i}"),
            });
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), RING_CAPACITY);
        // The first 7 entries were popped off the front.
        assert_eq!(snap[0].message, format!("msg{}", 7));
        assert_eq!(
            snap[RING_CAPACITY - 1].message,
            format!("msg{}", RING_CAPACITY + 6)
        );
    }

    /// New subscribers see only future broadcasts — the
    /// `snapshot()` step covers the ring's history.
    #[tokio::test]
    async fn subscribe_receives_future_pushes() {
        let ring = LogRing::new();
        let mut rx = ring.subscribe();
        ring.push(LogLine {
            ts_ms: 1,
            level: "WARN".into(),
            target: "t".into(),
            message: "first".into(),
        });
        let recv = tokio::time::timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("recv timeout")
            .expect("recv error");
        assert_eq!(recv.message, "first");
    }

    /// The LogRingLayer captures the `tracing::info!` macro's
    /// `message` field verbatim.
    #[test]
    fn layer_captures_info_event_message() {
        // Use a Registry with ONLY the LogRingLayer so we do not
        // clobber the global subscriber from other tests. The
        // `with_default` guard is per-thread and scopes the
        // subscriber to this block.
        let ring = LogRing::new();
        let subscriber = Registry::default().with(LogRingLayer::new(ring.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(event_id = 42, "captured-message-text");
        });
        let snap = ring.snapshot();
        assert!(!snap.is_empty(), "no log line captured");
        let last = &snap[snap.len() - 1];
        assert_eq!(last.level, "INFO");
        assert!(
            last.message.contains("captured-message-text"),
            "message missing: {:?}",
            last.message,
        );
        assert!(
            last.message.contains("event_id=42"),
            "field missing: {:?}",
            last.message,
        );
    }

    /// The layer composes with the standard fmt layer without
    /// either one swallowing events meant for the other.
    #[test]
    fn layer_composes_with_fmt_layer() {
        let ring = LogRing::new();
        let layered = Registry::default()
            .with(fmt::layer().with_writer(std::io::sink))
            .with(LogRingLayer::new(ring.clone()));
        tracing::subscriber::with_default(layered, || {
            tracing::warn!("composed-event");
        });
        assert!(
            ring.snapshot()
                .iter()
                .any(|l| l.message.contains("composed-event") && l.level == "WARN")
        );
    }
}
