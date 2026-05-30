//! Shared per-channel health snapshot.
//!
//! Embedded on each channel's `ChannelState` so the
//! per-channel `<channel>.health` capability can return a
//! single canonical JSON shape. The same struct backs the
//! bridge's `/v1/health` aggregation under `channels.<name>`.
//!
//! Every timestamp is unix-millis; the channel controller
//! stamps them via `Clock::now_ms()` so the field semantics
//! are testable under [`crate::clock::FakeClock`].

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::channel_rate_limit::{ChannelRateLimiter, RateLimitState};
use crate::clock::Clock;

/// The canonical wire shape for a per-channel health
/// snapshot. Mirrors the user-spec field set verbatim so
/// dashboards can consume it as-is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChannelHealthSnapshot {
    /// `true` when the channel's `get_me`/`auth.test` succeeded
    /// at least once. Flipped by the controller after the
    /// initial identity probe.
    pub enabled: bool,
    /// Delivery mode the controller actually runs in.
    /// Examples: `"long_poll"`, `"webhook"`, `"events_api"`,
    /// `"polling"`.
    pub mode: String,
    /// Unix-millis of the most recent successful inbound
    /// poll (or webhook receive). `-1` when the controller
    /// has never seen a successful poll.
    pub last_poll_success_ms: i64,
    /// Unix-millis of the most recent inbound event (message,
    /// callback_query, interaction). `-1` when no events
    /// have ever arrived.
    pub last_event_received_ms: i64,
    /// Unix-millis of the most recent successful outbound
    /// send. `-1` when no successful sends.
    pub last_send_success_ms: i64,
    /// Number of consecutive send failures since the last
    /// success. Reset to 0 on every successful send. Operators
    /// alert on `> 0` to catch a stuck channel.
    pub consecutive_failures: u64,
    /// Current rate-limit state — `"ok"` or `"throttled"`.
    /// Pulled from the per-channel
    /// [`ChannelRateLimiter::state`] when one is wired;
    /// defaults to `Ok` when the tracker is absent.
    pub rate_limit_state: RateLimitState,
    /// Number of in-flight session mappings (channel-local
    /// state). Returned by the channel's session store.
    pub session_count: u64,
}

/// Shared mutable state behind a single mutex. Channel
/// controllers call the `record_*` methods on every poll /
/// event / send; the cap handler calls [`snapshot`] to
/// project the current values to JSON.
#[derive(Clone)]
pub struct ChannelHealth {
    inner: Arc<Mutex<HealthInner>>,
    clock: Arc<dyn Clock>,
    /// Optional shared rate-limit tracker. Snapshot reads
    /// `state()` to populate `rate_limit_state`. `None`
    /// surfaces as `Ok`.
    rate_limiter: Option<ChannelRateLimiter>,
}

struct HealthInner {
    enabled: bool,
    mode: String,
    last_poll_success_ms: i64,
    last_event_received_ms: i64,
    last_send_success_ms: i64,
    consecutive_failures: u64,
    /// Snapshot-time callback for session count — set by the
    /// channel at registration when a SessionStore is wired.
    /// Always returns 0 when no source is bound.
    session_count: u64,
}

impl ChannelHealth {
    /// Construct a new health tracker. `mode` is the initial
    /// delivery mode (e.g. `"long_poll"`); the channel may
    /// flip it via [`set_mode`] if it transitions modes at
    /// runtime.
    pub fn new(
        mode: impl Into<String>,
        clock: Arc<dyn Clock>,
        rate_limiter: Option<ChannelRateLimiter>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HealthInner {
                enabled: false,
                mode: mode.into(),
                last_poll_success_ms: -1,
                last_event_received_ms: -1,
                last_send_success_ms: -1,
                consecutive_failures: 0,
                session_count: 0,
            })),
            clock,
            rate_limiter,
        }
    }

    /// Flip the channel to `enabled = true`. Called once after
    /// the controller's identity probe (`get_me` /
    /// `auth.test`) succeeds.
    pub fn mark_enabled(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.enabled = true;
    }

    /// Stamp the current clock as the last successful poll
    /// timestamp.
    pub fn record_poll_success(&self) {
        let now = self.clock.now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.last_poll_success_ms = now;
    }

    /// Stamp the current clock as the last received event
    /// timestamp.
    pub fn record_event_received(&self) {
        let now = self.clock.now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.last_event_received_ms = now;
    }

    /// Stamp the current clock as the last successful send +
    /// reset `consecutive_failures` to 0.
    pub fn record_send_success(&self) {
        let now = self.clock.now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.last_send_success_ms = now;
        g.consecutive_failures = 0;
    }

    /// Bump `consecutive_failures` by one. Called by the
    /// channel after every send failure.
    pub fn record_send_failure(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.consecutive_failures = g.consecutive_failures.saturating_add(1);
    }

    /// Update the operator-visible session count. Channels
    /// call this from their session-store sweep or after
    /// every `record`/`forget` if they want exact accounting.
    pub fn set_session_count(&self, n: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.session_count = n;
    }

    /// Switch the mode label. Used when a channel transitions
    /// between long-poll and webhook modes at runtime (today
    /// the mode is fixed at startup; future modes may flip).
    pub fn set_mode(&self, mode: impl Into<String>) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.mode = mode.into();
    }

    /// Project the current values to the operator-visible
    /// JSON shape. Reads the rate-limit tracker's state on
    /// every call so the snapshot reflects the most recent
    /// utilisation.
    pub fn snapshot(&self) -> ChannelHealthSnapshot {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        ChannelHealthSnapshot {
            enabled: g.enabled,
            mode: g.mode.clone(),
            last_poll_success_ms: g.last_poll_success_ms,
            last_event_received_ms: g.last_event_received_ms,
            last_send_success_ms: g.last_send_success_ms,
            consecutive_failures: g.consecutive_failures,
            rate_limit_state: self
                .rate_limiter
                .as_ref()
                .map(|r| r.state())
                .unwrap_or(RateLimitState::Ok),
            session_count: g.session_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn fresh() -> ChannelHealth {
        ChannelHealth::new("long_poll", Arc::new(FakeClock::new(1_000)), None)
    }

    #[test]
    fn snapshot_default_shape_uses_negative_one_for_unstamped_timestamps() {
        let h = fresh();
        let s = h.snapshot();
        assert!(!s.enabled);
        assert_eq!(s.mode, "long_poll");
        assert_eq!(s.last_poll_success_ms, -1);
        assert_eq!(s.last_event_received_ms, -1);
        assert_eq!(s.last_send_success_ms, -1);
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.rate_limit_state, RateLimitState::Ok);
        assert_eq!(s.session_count, 0);
    }

    #[test]
    fn mark_enabled_flips_the_enabled_field() {
        let h = fresh();
        assert!(!h.snapshot().enabled);
        h.mark_enabled();
        assert!(h.snapshot().enabled);
    }

    #[test]
    fn record_poll_success_stamps_clock_now() {
        let clock = Arc::new(FakeClock::new(1_234_567_890));
        let h = ChannelHealth::new("long_poll", clock.clone(), None);
        h.record_poll_success();
        assert_eq!(h.snapshot().last_poll_success_ms, 1_234_567_890);
    }

    #[test]
    fn record_event_received_uses_injected_clock() {
        let clock = Arc::new(FakeClock::new(42));
        let h = ChannelHealth::new("webhook", clock.clone(), None);
        h.record_event_received();
        assert_eq!(h.snapshot().last_event_received_ms, 42);
    }

    #[test]
    fn record_send_success_resets_consecutive_failures() {
        let h = fresh();
        h.record_send_failure();
        h.record_send_failure();
        h.record_send_failure();
        assert_eq!(h.snapshot().consecutive_failures, 3);
        h.record_send_success();
        assert_eq!(
            h.snapshot().consecutive_failures,
            0,
            "success must reset the failure counter"
        );
    }

    #[test]
    fn record_send_failure_saturates_at_u64_max() {
        let h = fresh();
        // Set a high value via direct mutation; the saturating
        // add path is the contract under stress.
        {
            let mut g = h.inner.lock().unwrap();
            g.consecutive_failures = u64::MAX - 1;
        }
        h.record_send_failure();
        assert_eq!(h.snapshot().consecutive_failures, u64::MAX);
        h.record_send_failure();
        // No panic — saturating_add keeps it at MAX.
        assert_eq!(h.snapshot().consecutive_failures, u64::MAX);
    }

    #[test]
    fn set_session_count_round_trips() {
        let h = fresh();
        h.set_session_count(42);
        assert_eq!(h.snapshot().session_count, 42);
        h.set_session_count(7);
        assert_eq!(h.snapshot().session_count, 7);
    }

    #[test]
    fn set_mode_flips_the_label() {
        let h = fresh();
        assert_eq!(h.snapshot().mode, "long_poll");
        h.set_mode("webhook");
        assert_eq!(h.snapshot().mode, "webhook");
    }

    #[test]
    fn snapshot_pulls_rate_limit_state_from_tracker() {
        use crate::channel_rate_limit::{ChannelRateLimiter, PlatformLimits};
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(0));
        let tracker = ChannelRateLimiter::new(PlatformLimits::new(10), None, clock.clone());
        let h = ChannelHealth::new("long_poll", clock, Some(tracker.clone()));
        // Burn 9 tokens — utilisation hits 90% → Throttled.
        for _ in 0..9 {
            tracker.try_acquire("c").unwrap();
        }
        let s = h.snapshot();
        assert_eq!(s.rate_limit_state, RateLimitState::Throttled);
    }

    #[test]
    fn snapshot_serialises_to_documented_field_names() {
        // FIX 49: lock the wire shape — operator dashboards
        // consume these names verbatim; a rename would
        // silently break them.
        let h = fresh();
        h.mark_enabled();
        h.record_poll_success();
        h.record_event_received();
        h.record_send_success();
        h.set_session_count(3);
        let s = h.snapshot();
        let j = serde_json::to_value(&s).unwrap();
        assert_eq!(j["enabled"], serde_json::json!(true));
        assert_eq!(j["mode"], serde_json::json!("long_poll"));
        assert!(j.get("last_poll_success_ms").is_some());
        assert!(j.get("last_event_received_ms").is_some());
        assert!(j.get("last_send_success_ms").is_some());
        assert_eq!(j["consecutive_failures"], serde_json::json!(0));
        assert_eq!(j["rate_limit_state"], serde_json::json!("ok"));
        assert_eq!(j["session_count"], serde_json::json!(3));
    }
}
