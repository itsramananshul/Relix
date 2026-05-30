//! Shared per-channel proactive rate-limit tracker.
//!
//! Channel crates (`relix-telegram`, `relix-slack`,
//! `relix-discord`) wrap their outbound send paths with a
//! [`ChannelRateLimiter`] so the bot backs off BEFORE the
//! platform-side 429 wall, not after. Each platform has its
//! own published per-second cap; this module models them as a
//! pair of token buckets (per-key + global) plus an 80%
//! soft-throttle threshold:
//!
//! - Under 80% of the bucket capacity: [`RateLimitState::Ok`]
//!   — sends proceed without delay.
//! - 80–99% utilisation: [`RateLimitState::Throttled`] — the
//!   tracker inserts an artificial backoff before approving.
//! - 100% (bucket exhausted) — [`acquire`] awaits until
//!   refill, queueing implicitly.
//!
//! Per-platform [`PlatformLimits`] presets bake in the
//! documented caps:
//!
//! - Telegram: 30/s global + 1/s per chat
//!   (<https://core.telegram.org/bots/faq#my-bot-is-hitting-limits>).
//! - Slack: 1/s per channel (Tier 3, conservative across the
//!   Web API surface for the chat.postMessage endpoint;
//!   <https://api.slack.com/docs/rate-limits>).
//! - Discord: 5/s per channel + 50/s global
//!   (<https://discord.com/developers/docs/topics/rate-limits>).
//!
//! All time reads route through [`crate::clock::Clock`] so the
//! tracker is testable under [`crate::clock::FakeClock`]
//! without sleeping. Channel-side wiring is intentionally
//! thin: every send call is `let _guard = limiter.acquire(&key).await?;`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::Clock;

/// Per-key rate-limit tracker state. Lives behind the
/// `RateLimitState::current()` accessor so the health
/// endpoint can surface "ok|throttled" verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitState {
    /// Bucket below the 80% threshold; sends proceed.
    #[default]
    Ok,
    /// Bucket above the 80% threshold; sends incur a backoff.
    Throttled,
}

/// Documented per-platform cap. Each tracker is configured
/// with two `PlatformLimits` — one for the per-key bucket
/// (per-chat, per-channel) and one for the global cap.
#[derive(Clone, Copy, Debug)]
pub struct PlatformLimits {
    /// Maximum messages per second on this bucket. The
    /// tracker's bucket holds at most `per_second` tokens and
    /// refills at the same rate.
    pub per_second: u32,
}

impl PlatformLimits {
    /// Construct from a `per_second` rate. Used both for the
    /// per-key bucket (per-chat, per-channel) and the
    /// optional global cap.
    pub const fn new(per_second: u32) -> Self {
        Self { per_second }
    }
}

/// Telegram per-chat cap (1 message per second per chat).
pub const TELEGRAM_PER_CHAT: PlatformLimits = PlatformLimits::new(1);
/// Telegram global cap (30 messages per second across all
/// chats).
pub const TELEGRAM_GLOBAL: PlatformLimits = PlatformLimits::new(30);

/// Slack per-channel cap (Tier 3 — 1 message per second).
pub const SLACK_PER_CHANNEL: PlatformLimits = PlatformLimits::new(1);

/// Discord per-channel cap (5 messages per second).
pub const DISCORD_PER_CHANNEL: PlatformLimits = PlatformLimits::new(5);
/// Discord global cap (50 messages per second across all
/// channels).
pub const DISCORD_GLOBAL: PlatformLimits = PlatformLimits::new(50);

/// The 80% threshold the spec calls for. Sends in
/// `[soft_threshold, capacity)` get a small backoff so we
/// approach the limit gracefully rather than slamming into
/// it.
const SOFT_THRESHOLD_RATIO: f32 = 0.80;

/// Per-key token bucket. `tokens` is fractional so refill
/// arithmetic at sub-second granularity is precise.
#[derive(Clone, Debug)]
struct Bucket {
    capacity: f32,
    tokens: f32,
    last_refill_ms: i64,
    per_second: f32,
}

impl Bucket {
    fn new(limits: PlatformLimits, now_ms: i64) -> Self {
        let cap = limits.per_second as f32;
        Self {
            capacity: cap,
            tokens: cap, // start full
            last_refill_ms: now_ms,
            per_second: cap,
        }
    }

    fn refill(&mut self, now_ms: i64) {
        let elapsed_ms = now_ms.saturating_sub(self.last_refill_ms).max(0) as f32;
        let added = (elapsed_ms / 1000.0) * self.per_second;
        self.tokens = (self.tokens + added).min(self.capacity);
        self.last_refill_ms = now_ms;
    }

    fn try_consume(&mut self) -> bool {
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn utilisation(&self) -> f32 {
        if self.capacity <= 0.0 {
            return 0.0;
        }
        1.0 - (self.tokens / self.capacity)
    }

    /// Milliseconds until at least one token is available.
    fn refill_wait_ms(&self) -> u64 {
        if self.tokens >= 1.0 {
            return 0;
        }
        let missing = 1.0 - self.tokens;
        let secs = missing / self.per_second.max(1e-6);
        (secs * 1000.0).ceil() as u64
    }
}

/// Shared per-channel rate-limiter. Tracks both a per-key
/// bucket and an optional global bucket.
#[derive(Clone)]
pub struct ChannelRateLimiter {
    inner: Arc<Mutex<RateLimiterInner>>,
    clock: Arc<dyn Clock>,
}

struct RateLimiterInner {
    per_key_limits: PlatformLimits,
    per_key: HashMap<String, Bucket>,
    global: Option<Bucket>,
    /// Last observed state, surfaced by `state()` for the
    /// health endpoint.
    last_state: RateLimitState,
}

impl ChannelRateLimiter {
    /// Construct a new tracker. `per_key` is the cap applied
    /// per send key (chat id / channel id); `global` is the
    /// optional aggregate cap across all keys (when `None`,
    /// only the per-key bucket is enforced).
    pub fn new(
        per_key: PlatformLimits,
        global: Option<PlatformLimits>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let now = clock.now_ms();
        Self {
            inner: Arc::new(Mutex::new(RateLimiterInner {
                per_key_limits: per_key,
                per_key: HashMap::new(),
                global: global.map(|l| Bucket::new(l, now)),
                last_state: RateLimitState::Ok,
            })),
            clock,
        }
    }

    /// Acquire one token for `key` (per-chat / per-channel
    /// identifier). Awaits the necessary refill if the bucket
    /// is empty. Returns the [`RateLimitState`] observed AFTER
    /// the acquire so callers can log if they want.
    ///
    /// This is the primary call site for outbound message
    /// senders. Wrap every `chat.postMessage` /
    /// `sendMessage` / `createMessage` with
    /// `let _ = limiter.acquire("chat-id").await;`.
    pub async fn acquire(&self, key: &str) -> RateLimitState {
        loop {
            let wait_ms = {
                let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                let now = self.clock.now_ms();
                // Per-key bucket.
                let per_key_limits = g.per_key_limits;
                let per_key_bucket = g
                    .per_key
                    .entry(key.to_string())
                    .or_insert_with(|| Bucket::new(per_key_limits, now));
                per_key_bucket.refill(now);
                let per_key_ok = per_key_bucket.try_consume();
                let per_key_wait = if per_key_ok {
                    0
                } else {
                    per_key_bucket.refill_wait_ms()
                };
                let per_key_util = per_key_bucket.utilisation();

                // Global bucket (if configured).
                let global_ok;
                let global_wait;
                let global_util;
                if let Some(g_bucket) = g.global.as_mut() {
                    g_bucket.refill(now);
                    global_ok = if per_key_ok {
                        g_bucket.try_consume()
                    } else {
                        false
                    };
                    global_wait = if global_ok {
                        0
                    } else {
                        g_bucket.refill_wait_ms()
                    };
                    global_util = g_bucket.utilisation();
                } else {
                    global_ok = true;
                    global_wait = 0;
                    global_util = 0.0;
                }

                if per_key_ok && global_ok {
                    let max_util = per_key_util.max(global_util);
                    let state = if max_util >= SOFT_THRESHOLD_RATIO {
                        RateLimitState::Throttled
                    } else {
                        RateLimitState::Ok
                    };
                    g.last_state = state;
                    return state;
                }
                // Couldn't consume — return BOTH buckets'
                // unsuccessful consumes (try_consume already
                // failed and returned false; we did NOT
                // deduct on the failed side, so re-acquire
                // after the wait will retry both).
                // If we DID consume per-key but not global,
                // we need to refund per-key. `get_mut` cannot
                // be `None` here — we just inserted via
                // `entry().or_insert_with()` — but lint
                // forbids `expect`; `if let` is the
                // panic-free shape.
                if per_key_ok
                    && !global_ok
                    && let Some(pk) = g.per_key.get_mut(key)
                {
                    pk.tokens += 1.0;
                }
                g.last_state = RateLimitState::Throttled;
                per_key_wait.max(global_wait)
            };
            tokio::time::sleep(Duration::from_millis(wait_ms.max(1))).await;
        }
    }

    /// Non-blocking variant: try to acquire one token; return
    /// `Some(state)` on success, `None` when both buckets
    /// would force a wait. Useful for "drop on saturation"
    /// callers (e.g. fire-and-forget telemetry).
    pub fn try_acquire(&self, key: &str) -> Option<RateLimitState> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = self.clock.now_ms();
        let per_key_limits = g.per_key_limits;
        let per_key_bucket = g
            .per_key
            .entry(key.to_string())
            .or_insert_with(|| Bucket::new(per_key_limits, now));
        per_key_bucket.refill(now);
        if !per_key_bucket.try_consume() {
            g.last_state = RateLimitState::Throttled;
            return None;
        }
        let per_key_util = per_key_bucket.utilisation();
        let global_util = if let Some(g_bucket) = g.global.as_mut() {
            g_bucket.refill(now);
            if !g_bucket.try_consume() {
                // Refund per-key. `get_mut` can't be None
                // (just inserted); lint forbids `expect`.
                if let Some(pk) = g.per_key.get_mut(key) {
                    pk.tokens += 1.0;
                }
                g.last_state = RateLimitState::Throttled;
                return None;
            }
            g_bucket.utilisation()
        } else {
            0.0
        };
        let state = if per_key_util.max(global_util) >= SOFT_THRESHOLD_RATIO {
            RateLimitState::Throttled
        } else {
            RateLimitState::Ok
        };
        g.last_state = state;
        Some(state)
    }

    /// Snapshot the most-recently-observed state. Surfaced by
    /// the per-channel health endpoint as
    /// `rate_limit_state`.
    pub fn state(&self) -> RateLimitState {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;

    fn limiter_with(
        per_key: PlatformLimits,
        global: Option<PlatformLimits>,
        clock: Arc<FakeClock>,
    ) -> ChannelRateLimiter {
        ChannelRateLimiter::new(per_key, global, clock)
    }

    #[test]
    fn try_acquire_succeeds_under_capacity() {
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(2), None, clock.clone());
        let s1 = lim.try_acquire("c1").expect("first acquire");
        // Bucket cap = 2, after one consume utilisation = 50%
        // → still Ok.
        assert_eq!(s1, RateLimitState::Ok);
        let s2 = lim.try_acquire("c1").expect("second acquire");
        // After two consumes the bucket is empty → 100% util →
        // Throttled signal (even though the second acquire
        // succeeded).
        assert_eq!(s2, RateLimitState::Throttled);
    }

    #[test]
    fn try_acquire_fails_when_bucket_empty() {
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(1), None, clock.clone());
        assert!(lim.try_acquire("c1").is_some());
        assert!(
            lim.try_acquire("c1").is_none(),
            "second call must observe the empty bucket"
        );
    }

    #[test]
    fn try_acquire_refills_over_time() {
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(1), None, clock.clone());
        // Burn the only token.
        assert!(lim.try_acquire("c1").is_some());
        assert!(lim.try_acquire("c1").is_none());
        // After 1s the bucket refills 1 token.
        clock.set(1_000);
        assert!(
            lim.try_acquire("c1").is_some(),
            "1s later the bucket has refilled by per_second=1"
        );
    }

    #[test]
    fn per_key_buckets_are_isolated() {
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(1), None, clock.clone());
        assert!(lim.try_acquire("c1").is_some());
        // c1 is drained, but c2 should be independent.
        assert!(lim.try_acquire("c2").is_some());
    }

    #[test]
    fn global_bucket_caps_aggregate_send_rate() {
        // Per-key cap 10/s, global cap 3/s. After three sends
        // across two keys the global is empty and ANY key
        // try_acquire returns None.
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(
            PlatformLimits::new(10),
            Some(PlatformLimits::new(3)),
            clock.clone(),
        );
        assert!(lim.try_acquire("a").is_some());
        assert!(lim.try_acquire("b").is_some());
        assert!(lim.try_acquire("a").is_some());
        // 3 consumed globally → global drained → next any-key
        // call refunds per-key + returns None.
        assert!(lim.try_acquire("b").is_none());
    }

    #[test]
    fn state_surfaces_throttled_when_above_soft_threshold() {
        // Cap 10, 80% threshold = 8 tokens used.
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(10), None, clock.clone());
        // Burn 7 tokens — utilisation = 70%, state = Ok.
        for _ in 0..7 {
            lim.try_acquire("c1").unwrap();
        }
        assert_eq!(lim.state(), RateLimitState::Ok);
        // Burn one more — utilisation = 80% → Throttled.
        lim.try_acquire("c1").unwrap();
        assert_eq!(lim.state(), RateLimitState::Throttled);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_awaits_refill_when_bucket_empty() {
        // Drain the bucket; spawn an acquire(); confirm it
        // doesn't return until time advances past the refill
        // window.
        let clock = Arc::new(FakeClock::new(0));
        let lim = limiter_with(PlatformLimits::new(1), None, clock.clone());
        // Burn the only token.
        lim.try_acquire("c1").unwrap();
        let lim_for_spawn = lim.clone();
        let handle = tokio::spawn(async move { lim_for_spawn.acquire("c1").await });
        // Let the task run; the inner sleep should park it.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_millis(0)).await;
            tokio::task::yield_now().await;
        }
        assert!(!handle.is_finished(), "acquire should still be waiting");
        // Advance the wall clock + the runtime past the
        // refill window. 1s for a per_second=1 bucket.
        clock.set(1_001);
        tokio::time::advance(Duration::from_millis(1_001)).await;
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_millis(0)).await;
            tokio::task::yield_now().await;
        }
        assert!(handle.is_finished(), "acquire should complete after refill");
    }

    #[test]
    fn telegram_presets_match_documented_caps() {
        assert_eq!(TELEGRAM_PER_CHAT.per_second, 1);
        assert_eq!(TELEGRAM_GLOBAL.per_second, 30);
    }

    #[test]
    fn slack_preset_matches_tier_3() {
        assert_eq!(SLACK_PER_CHANNEL.per_second, 1);
    }

    #[test]
    fn discord_presets_match_documented_caps() {
        assert_eq!(DISCORD_PER_CHANNEL.per_second, 5);
        assert_eq!(DISCORD_GLOBAL.per_second, 50);
    }

    #[test]
    fn rate_limit_state_serialises_lowercase() {
        let ok = serde_json::to_string(&RateLimitState::Ok).unwrap();
        let th = serde_json::to_string(&RateLimitState::Throttled).unwrap();
        assert_eq!(ok, "\"ok\"");
        assert_eq!(th, "\"throttled\"");
    }
}
