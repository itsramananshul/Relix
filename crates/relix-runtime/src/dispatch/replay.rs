//! P2 — RELIX-1 §1.9 replay-protection cache.
//!
//! The dispatch bridge must reject any inbound envelope whose
//! per-request nonce has already been observed within the
//! freshness window. The nonce is the envelope's [`RequestId`]
//! (`rid`) — 16 random bytes per RELIX-1 §1.4, generated per
//! envelope and not derived from any predictable field. The
//! cache stores the hex string of the rid against an
//! `expires_at_ms` past which the entry can be evicted.
//!
//! Window choice. The cache window MUST be at least as long as
//! the maximum clock-skew tolerance the admission step
//! enforces against the envelope's `issued_at_ms`. After the
//! freshness window elapses, the admission step rejects the
//! envelope as stale regardless of cache state — so we can
//! safely evict the entry. The default window matches the
//! default `max_clock_skew_ms = 5000` (5 seconds).
//!
//! Background eviction. The cache eagerly drops expired
//! entries on every [`Self::check_and_insert`] call (so a
//! steady-state inbound flow naturally garbage-collects), and
//! a separate background eviction task wakes every 60 seconds
//! to sweep any straggler entries when traffic is idle.
//!
//! Failure modes:
//! - Duplicate nonce inside the window → [`ReplayError::Replayed`].
//!   The dispatch path maps this to
//!   [`relix_core::types::error_kinds::REPLAY_REJECTED`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

/// Default freshness window in ms — RELIX-1 §1.9 mandates 5
/// minutes. A nonce is remembered for at least this long; the
/// admission freshness check rejects any envelope older than
/// this, so an entry past the window is necessarily stale and
/// safe to evict. Operators tune via the dispatch builder
/// (`set_freshness_window_ms`).
pub const DEFAULT_WINDOW_MS: i64 = 300_000;

/// Default ONE-SIDED future clock-skew allowance in ms. An
/// envelope stamped more than this far in the FUTURE is
/// rejected (a future-stamped envelope must never be admitted
/// just because it is "within window"). Kept small — real
/// callers are at most a few seconds ahead of the responder.
pub const DEFAULT_CLOCK_SKEW_MS: i64 = 5_000;

/// Hard cap on live cache entries. Bounds memory so a flood of
/// distinct nonces inside the window cannot grow the map
/// without limit; at the cap the oldest-expiry entry is evicted
/// to make room. ~1M entries ≈ a few hundred MB worst case.
pub const DEFAULT_MAX_ENTRIES: usize = 1_048_576;

/// Eager-eviction interval the background sweeper uses when
/// no inbound traffic is driving `check_and_insert` calls.
pub const EVICTION_INTERVAL_SECS: u64 = 60;

/// Errors the replay cache surfaces. One variant today; kept
/// as an enum so future additions (lock poisoning, capacity
/// exhaustion) stay backwards-compatible at the call site.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReplayError {
    /// The nonce was already observed inside the freshness
    /// window. Dispatch returns `REPLAY_REJECTED` to the
    /// caller.
    #[error("replay cache: nonce already observed")]
    Replayed,
}

/// Sliding-window nonce cache. Cheap to clone (one Arc).
#[derive(Clone)]
pub struct ReplayCache {
    inner: Arc<ReplayCacheInner>,
}

struct ReplayCacheInner {
    seen: Mutex<HashMap<String, i64>>,
    window_ms: i64,
    max_entries: usize,
}

impl ReplayCache {
    /// Build a new cache with the requested freshness window
    /// in ms and the default entry cap. Values below 1ms
    /// saturate to 1ms — a zero-width window would let every
    /// envelope through regardless of nonce reuse.
    pub fn new(window_ms: i64) -> Self {
        Self::with_cap(window_ms, DEFAULT_MAX_ENTRIES)
    }

    /// Build a cache with an explicit entry cap. Used by tests
    /// that want to exercise the bounded-eviction path without
    /// inserting a million rows.
    pub fn with_cap(window_ms: i64, max_entries: usize) -> Self {
        let window = window_ms.max(1);
        Self {
            inner: Arc::new(ReplayCacheInner {
                seen: Mutex::new(HashMap::new()),
                window_ms: window,
                max_entries: max_entries.max(1),
            }),
        }
    }

    /// The hard entry cap — read-only accessor for diagnostics.
    pub fn max_entries(&self) -> usize {
        self.inner.max_entries
    }

    /// Check whether `nonce` has been observed in the current
    /// window. Atomically inserts on the not-seen path. On the
    /// seen path returns [`ReplayError::Replayed`] without
    /// extending the existing entry's expiry — so an attacker
    /// cannot keep an entry alive by replaying it.
    ///
    /// Side effect: every call evicts expired entries before
    /// checking, so a steady inflow keeps the map size bounded.
    pub fn check_and_insert(&self, nonce: &str, now_ms: i64) -> Result<(), ReplayError> {
        let mut seen = self.inner.seen.lock().unwrap_or_else(|e| e.into_inner());
        // Evict expired entries first. The retain pass costs
        // O(N) on the live map; in steady state the map size
        // is bounded by ~ `peak_inbound_rate * window_secs`.
        seen.retain(|_, &mut expires_at| expires_at > now_ms);
        if seen.contains_key(nonce) {
            return Err(ReplayError::Replayed);
        }
        // Bound memory: if we are at the hard cap after evicting
        // expired entries, drop the entry that expires soonest
        // to make room. A flood of distinct in-window nonces
        // therefore cannot grow the map past `max_entries`.
        if seen.len() >= self.inner.max_entries {
            let oldest = seen
                .iter()
                .min_by_key(|(_, expires_at)| **expires_at)
                .map(|(k, _)| k.clone());
            if let Some(oldest) = oldest {
                seen.remove(&oldest);
            }
        }
        // `saturating_add` so an i64::MAX window (the
        // "freshness disabled" sentinel used in some tests
        // that fake the bridge clock at small values) does
        // not overflow when stamping the entry's expiry.
        seen.insert(
            nonce.to_string(),
            now_ms.saturating_add(self.inner.window_ms),
        );
        Ok(())
    }

    /// Snapshot of the current entry count. Used by the
    /// background eviction loop's debug log so operators can
    /// watch the cache shrink under no-traffic conditions.
    pub fn len(&self) -> usize {
        self.inner
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// True iff the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Window in ms — read-only accessor for the bridge's
    /// startup log + the admission step that drives freshness
    /// checks against the same value.
    pub fn window_ms(&self) -> i64 {
        self.inner.window_ms
    }

    /// Manually evict expired entries. Called by the
    /// background sweeper on its 60s tick.
    pub fn evict_expired(&self, now_ms: i64) -> usize {
        let mut seen = self.inner.seen.lock().unwrap_or_else(|e| e.into_inner());
        let before = seen.len();
        seen.retain(|_, &mut expires_at| expires_at > now_ms);
        before - seen.len()
    }

    /// Spawn the background eviction task. Returns a
    /// [`tokio::task::JoinHandle`] so the caller can abort the
    /// loop on shutdown. The task wakes every
    /// [`EVICTION_INTERVAL_SECS`] and removes any expired
    /// entries. Operators get a `trace` log on the swept
    /// count.
    pub fn spawn_eviction_task(
        &self,
        clock: Arc<dyn relix_core::clock::Clock>,
    ) -> tokio::task::JoinHandle<()> {
        let cache = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(EVICTION_INTERVAL_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            tick.tick().await;
            loop {
                tick.tick().await;
                let evicted = cache.evict_expired(clock.now_ms());
                if evicted > 0 {
                    tracing::trace!(
                        evicted,
                        remaining = cache.len(),
                        "replay cache: background eviction pass complete"
                    );
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_admits_then_duplicate_is_rejected() {
        let cache = ReplayCache::new(5_000);
        let now = 1_000_000;
        cache
            .check_and_insert("nonce-A", now)
            .expect("first admits");
        match cache.check_and_insert("nonce-A", now) {
            Err(ReplayError::Replayed) => {}
            other => panic!("expected Replayed, got {other:?}"),
        }
    }

    #[test]
    fn distinct_nonces_both_admit() {
        let cache = ReplayCache::new(5_000);
        cache.check_and_insert("nonce-A", 1).unwrap();
        cache.check_and_insert("nonce-B", 1).unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn entry_expires_after_window_and_can_be_re_observed() {
        let cache = ReplayCache::new(5_000);
        cache.check_and_insert("nonce-A", 0).unwrap();
        // Same instant → still rejected.
        match cache.check_and_insert("nonce-A", 0) {
            Err(ReplayError::Replayed) => {}
            other => panic!("got {other:?}"),
        }
        // Past the window → eviction runs before the contains
        // check, so the nonce is observable again.
        cache
            .check_and_insert("nonce-A", 5_001)
            .expect("post-window admit");
    }

    #[test]
    fn replayed_entry_does_not_extend_its_own_expiry() {
        // An attacker who replays a nonce at the boundary must
        // not push the entry's expiry forward. Originally
        // inserted at t=0 with window=5000; expiry = 5000. A
        // replay at t=4999 must NOT shift expiry to 9999.
        let cache = ReplayCache::new(5_000);
        cache.check_and_insert("nonce-A", 0).unwrap();
        match cache.check_and_insert("nonce-A", 4_999) {
            Err(ReplayError::Replayed) => {}
            other => panic!("got {other:?}"),
        }
        // At t=5001 the cache must let the nonce through (the
        // replay attempt did not push expiry forward).
        cache
            .check_and_insert("nonce-A", 5_001)
            .expect("expiry not extended by replay attempt");
    }

    #[test]
    fn cache_is_bounded_by_the_entry_cap() {
        // SECTION 7: with a hard cap of 3 and a large window
        // (nothing expires), inserting 5 distinct nonces must
        // leave the map at the cap — never unbounded growth.
        let cache = ReplayCache::with_cap(1_000_000, 3);
        for (i, k) in ["a", "b", "c", "d", "e"].iter().enumerate() {
            cache.check_and_insert(k, i as i64).unwrap();
        }
        assert_eq!(cache.len(), 3, "cache must stay bounded at the entry cap");
    }

    #[test]
    fn evict_expired_returns_number_of_removed_entries() {
        let cache = ReplayCache::new(5_000);
        cache.check_and_insert("a", 0).unwrap();
        cache.check_and_insert("b", 0).unwrap();
        cache.check_and_insert("c", 100).unwrap();
        // Advance past the first two entries' expiry but not
        // the third.
        let removed = cache.evict_expired(5_050);
        assert_eq!(removed, 2);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn window_below_one_ms_saturates_to_one() {
        let cache = ReplayCache::new(0);
        assert_eq!(cache.window_ms(), 1);
        cache.check_and_insert("x", 0).unwrap();
        match cache.check_and_insert("x", 0) {
            Err(ReplayError::Replayed) => {}
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_eviction_task_returns_handle_that_can_be_aborted() {
        let cache = ReplayCache::new(5_000);
        let clock: Arc<dyn relix_core::clock::Clock> = Arc::new(relix_core::clock::SystemClock);
        let handle = cache.spawn_eviction_task(clock);
        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test(start_paused = true)]
    async fn background_eviction_task_removes_expired_entries() {
        // P2 test: "The eviction task removes expired nonces."
        // Drive tokio's clock past the eviction interval AND
        // the cache's freshness window so the swept-count log
        // line fires.
        let cache = ReplayCache::new(5_000);
        let fake = Arc::new(relix_core::clock::FakeClock::new(0));
        let clock: Arc<dyn relix_core::clock::Clock> = fake.clone();
        cache.check_and_insert("nonce-A", 0).unwrap();
        cache.check_and_insert("nonce-B", 0).unwrap();
        assert_eq!(cache.len(), 2);
        let handle = cache.spawn_eviction_task(clock);
        // Yield first so the spawned task gets a chance to
        // create its `tokio::time::interval` BEFORE we start
        // advancing — under start_paused the interval ticks
        // only respond to advance calls that happen AFTER the
        // interval is registered.
        tokio::task::yield_now().await;
        // Advance FakeClock past the cache's window so both
        // entries are stale.
        fake.set(10_000);
        // Drive tokio's runtime clock forward in chunks past
        // the eviction interval, yielding between each chunk
        // so the spawned task's `interval.tick().await` wakes
        // and re-arms. Two ticks: one to consume the initial
        // immediate tick the loop discards, one to fire the
        // actual eviction pass.
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(EVICTION_INTERVAL_SECS + 1)).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(
            cache.len(),
            0,
            "eviction task should have swept stale nonces"
        );
        handle.abort();
        let _ = handle.await;
    }
}
