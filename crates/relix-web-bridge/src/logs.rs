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
//! ## P3 — secret redaction
//!
//! Every line that flows through the SSE response is run
//! through [`relix_core::redact::redact_secrets`] before being
//! serialised to the wire. That helper masks API keys (Stripe,
//! Google, OpenAI / Anthropic shapes), bearer tokens, JWTs,
//! AWS access-key / session-token pairs, PEM blocks, and the
//! `password=` / `secret=` / `api_key=` inline-field shapes.
//! Operators who deliberately want raw content set
//! `[logging] redact_stream = false`; the bridge logs a
//! startup WARN in that case.
//!
//! ## P3 — single-connection-per-session cap
//!
//! The stream endpoint enforces at most one live SSE
//! connection per authenticated session token. When a second
//! request arrives bearing the same token, the existing
//! connection's `cancel` channel is signalled so the running
//! stream drains and exits — the new connection then takes
//! over. This prevents an attacker who steals a token from
//! quietly tailing operator logs alongside the legitimate
//! dashboard tab without the operator noticing both windows
//! drop content.
//!
//! ## Threading model
//!
//! The ring is `Arc<Mutex<VecDeque<LogLine>>>` (the per-write
//! critical section is a deque push + counter bump; never
//! contended for more than a few microseconds). The broadcast
//! channel is `tokio::sync::broadcast` with capacity 1024 — a
//! slow subscriber sees `Lagged` errors and skips, never wedges
//! the producer.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use tokio::sync::{broadcast, watch};
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
///
/// P3: also carries the per-session live-connection registry
/// so the SSE endpoint can enforce the
/// one-connection-per-session cap.
#[derive(Clone)]
pub struct LogRing {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
    tx: broadcast::Sender<LogLine>,
    /// P3: per-token live-stream cancel handles. Keyed by
    /// session token. When a second stream opens for the same
    /// token, the existing watch sender is replaced and its
    /// previous value is set to `true` — observers polling the
    /// receiver drain and close.
    live_streams: Arc<Mutex<HashMap<String, watch::Sender<bool>>>>,
}

impl LogRing {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAPACITY))),
            tx,
            live_streams: Arc::new(Mutex::new(HashMap::new())),
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

    /// P3 — claim a stream slot for the given session token.
    /// Replaces any previous live-stream cancellation handle
    /// for the same token, signalling the prior stream to
    /// drain. Returns the [`watch::Receiver`] the new stream
    /// awaits to learn it has been superseded.
    pub fn claim_stream_slot(&self, token: &str) -> watch::Receiver<bool> {
        let (tx, rx) = watch::channel(false);
        let mut map = self.live_streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = map.insert(token.to_string(), tx) {
            // Signal the previous holder to close. Ignore the
            // result: a closed receiver means the previous
            // stream already exited.
            let _ = prev.send(true);
        }
        rx
    }

    /// P3 — release the per-token slot when an SSE stream
    /// finishes. Idempotent: if another claim has already
    /// replaced the slot, this call is a no-op so a
    /// stale-stream cleanup doesn't tear down the new owner.
    pub fn release_stream_slot(&self, token: &str, our_sender: &watch::Sender<bool>) {
        let mut map = self.live_streams.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(current) = map.get(token) {
            // Compare by sender identity via `same_channel`.
            // `tokio::sync::watch::Sender` exposes
            // `same_channel` for this exact case.
            if current.same_channel(our_sender) {
                map.remove(token);
            }
        }
    }

    /// P3 — current number of live per-token stream slots. Used
    /// by tests + future operator surfaces.
    #[allow(dead_code)]
    pub fn live_stream_count(&self) -> usize {
        self.live_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
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

/// P3 — apply the configured redaction policy to a [`LogLine`].
/// Returns the line untouched when redaction is disabled OR
/// the message scanned clean of secret-shaped substrings.
fn redact_line(line: &LogLine, redact_stream: bool) -> LogLine {
    if !redact_stream {
        return line.clone();
    }
    LogLine {
        ts_ms: line.ts_ms,
        level: line.level.clone(),
        target: line.target.clone(),
        message: relix_core::redact::redact_secrets(&line.message),
    }
}

/// P3 — extract the bearer token presented on the inbound SSE
/// request. The bridge's auth middleware admits the request
/// only when this header matches the bridge token (or a
/// tenant-binding prefix); the value is what we key the
/// per-session-cap registry on.
fn extract_session_token(req: &Request) -> Option<String> {
    let v = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
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
///
/// P3:
///   - Every line passes through [`relix_core::redact::redact_secrets`]
///     before being serialised (subject to `[logging] redact_stream`).
///   - A second connection bearing the same session token
///     cancels the first.
pub async fn stream(State(state): State<crate::config::AppState>, req: Request) -> Response {
    let ring = state.log_ring.clone();
    let redact_stream = state.cfg.logging.redact_stream;
    let token = match extract_session_token(&req) {
        Some(t) => t,
        None => {
            // The auth middleware should have rejected this
            // already, but defend in depth: refuse without a
            // bearer so the per-session cap has a non-empty
            // key.
            return (StatusCode::UNAUTHORIZED, "log stream requires bearer auth").into_response();
        }
    };
    let snapshot = ring.snapshot();
    let rx = ring.subscribe();
    let cancel_rx = ring.claim_stream_slot(&token);
    // Capture a Sender clone for the slot we just claimed so the
    // post-stream cleanup can `same_channel`-compare against it
    // and release ONLY if we're still the slot owner.
    let our_sender = {
        let map = ring.live_streams.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&token).cloned()
    };
    let ring_for_cleanup = ring.clone();
    let token_for_cleanup = token.clone();
    let s = async_stream::stream! {
        // Replay the ring first. JSON-encode each line; if
        // encoding fails (shouldn't — LogLine is a plain
        // struct) skip the line rather than aborting the
        // stream.
        for line in snapshot {
            let safe = redact_line(&line, redact_stream);
            if let Ok(payload) = serde_json::to_string(&safe) {
                yield Ok::<_, Infallible>(Event::default().event("log").data(payload));
            }
        }
        // Then tail the broadcast directly, racing the cancel
        // signal that fires when a second connection supersedes
        // this one. `Lagged` means we dropped lines for a slow
        // subscriber — skip and keep pulling. `Closed` means
        // the producer dropped its sender (process is shutting
        // down) — end the stream.
        let mut rx = rx;
        let mut cancel_rx = cancel_rx;
        loop {
            tokio::select! {
                changed = cancel_rx.changed() => {
                    // `Err` ⇒ the watch sender was dropped
                    // (slot was released without superseding —
                    // shouldn't happen on this code path but
                    // close anyway). `Ok` ⇒ a second connection
                    // claimed the slot.
                    match changed {
                        Ok(()) => {
                            if *cancel_rx.borrow_and_update() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                recv = rx.recv() => {
                    match recv {
                        Ok(line) => {
                            let safe = redact_line(&line, redact_stream);
                            if let Ok(payload) = serde_json::to_string(&safe) {
                                yield Ok(Event::default().event("log").data(payload));
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        // Release the per-token slot only if we still own it.
        // A superseding connection has already replaced the
        // entry under the same key; releasing then would tear
        // down the new owner.
        if let Some(sender) = our_sender.as_ref() {
            ring_for_cleanup.release_stream_slot(&token_for_cleanup, sender);
        }
    };
    Sse::new(s)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
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

    // ─────────────────────────────────────────────────────
    // P3 — redaction tests
    // ─────────────────────────────────────────────────────

    fn line_with(msg: &str) -> LogLine {
        LogLine {
            ts_ms: 1,
            level: "INFO".into(),
            target: "t".into(),
            message: msg.into(),
        }
    }

    #[test]
    fn p3_log_line_with_bearer_token_is_redacted_before_streaming() {
        // P3 test: "A log line containing a bearer token is
        // redacted before streaming."
        let line = line_with(
            "outbound request Authorization: Bearer 0123456789abcdef0123456789abcdef0123456789",
        );
        let safe = redact_line(&line, true);
        assert!(
            !safe
                .message
                .contains("0123456789abcdef0123456789abcdef0123456789"),
            "bearer token leaked: {}",
            safe.message
        );
        assert!(
            safe.message.contains("REDACTED"),
            "redaction marker missing: {}",
            safe.message
        );
    }

    #[test]
    fn p3_log_line_with_api_key_sk_pattern_is_redacted_before_streaming() {
        // P3 test: "A log line containing an API key matching
        // sk- pattern is redacted before streaming."
        // Assemble the fake secret-shaped token at runtime so no full
        // provider-key-shaped literal sits in source.
        let fake_key = ["sk", "-abc123def456ghi789jkl012mno345pqr"].concat();
        let line = line_with(&format!("dispatched call with key {fake_key}"));
        let safe = redact_line(&line, true);
        assert!(
            !safe.message.contains(&fake_key),
            "API key leaked: {}",
            safe.message
        );
        assert!(safe.message.contains("REDACTED"));
    }

    #[test]
    fn p3_log_line_with_jwt_is_redacted_before_streaming() {
        // P3 test: "A log line containing a JWT is redacted
        // before streaming."
        let line = line_with(
            "session bound token=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
             eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIn0.\
             SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        );
        let safe = redact_line(&line, true);
        assert!(
            !safe
                .message
                .contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
            "JWT leaked: {}",
            safe.message
        );
        assert!(safe.message.contains("REDACTED"));
    }

    #[test]
    fn p3_plain_log_line_passes_through_unchanged() {
        // P3 test: "A plain log line with no secrets passes
        // through unchanged."
        let line = line_with("processed task task_id=abc count=42 latency_ms=12");
        let safe = redact_line(&line, true);
        assert_eq!(safe.message, line.message);
    }

    #[test]
    fn redact_disabled_passes_secrets_through_unchanged() {
        let line = line_with("Authorization: Bearer 0123456789abcdef0123456789abcdef01234567");
        let safe = redact_line(&line, false);
        assert_eq!(safe.message, line.message);
    }

    // ─────────────────────────────────────────────────────
    // P3 — connection cap tests
    // ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn p3_second_connection_with_same_token_closes_the_first() {
        // P3 test: "A second connection with the same session
        // token closes the first connection."
        let ring = LogRing::new();
        let token = "session-token-abc";
        let mut first = ring.claim_stream_slot(token);
        // Initial value is `false` (no cancel yet).
        assert!(!*first.borrow());
        assert_eq!(ring.live_stream_count(), 1);
        // A second claim under the same token replaces the
        // first slot AND signals the previous holder to drain.
        let _second = ring.claim_stream_slot(token);
        // The first receiver observes a change.
        first.changed().await.expect("first must be notified");
        assert!(*first.borrow(), "first holder must observe cancel = true");
        // Only one slot is registered (the new one took over).
        assert_eq!(ring.live_stream_count(), 1);
    }

    #[tokio::test]
    async fn second_connection_with_different_token_does_not_close_first() {
        let ring = LogRing::new();
        let first = ring.claim_stream_slot("token-A");
        let _second = ring.claim_stream_slot("token-B");
        assert_eq!(ring.live_stream_count(), 2);
        // First has not been cancelled.
        assert!(!*first.borrow());
    }

    #[tokio::test]
    async fn release_stream_slot_is_no_op_when_slot_already_replaced() {
        let ring = LogRing::new();
        // Acquire a slot, retain a clone of its sender, then
        // replace it. The retained sender should NOT match the
        // current map entry, so release_stream_slot leaves the
        // new owner alone.
        let _rx_old = ring.claim_stream_slot("token-X");
        let stale_sender = ring
            .live_streams
            .lock()
            .unwrap()
            .get("token-X")
            .cloned()
            .unwrap();
        let _rx_new = ring.claim_stream_slot("token-X");
        assert_eq!(ring.live_stream_count(), 1);
        // Stale releaser is a no-op.
        ring.release_stream_slot("token-X", &stale_sender);
        assert_eq!(
            ring.live_stream_count(),
            1,
            "stale release must not evict new owner"
        );
    }

    #[tokio::test]
    async fn release_stream_slot_evicts_when_called_by_current_owner() {
        let ring = LogRing::new();
        let _rx = ring.claim_stream_slot("token-Y");
        let current = ring
            .live_streams
            .lock()
            .unwrap()
            .get("token-Y")
            .cloned()
            .unwrap();
        ring.release_stream_slot("token-Y", &current);
        assert_eq!(ring.live_stream_count(), 0);
    }
}
