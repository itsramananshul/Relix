//! PH-WAVE2A — Decorrelated jittered backoff (Hermes-grade).
//!
//! Hermes's `retry_utils.py` implements decorrelated-jitter
//! exponential backoff: a strategy where each retry's delay is
//! sampled uniformly from `[base, min(cap, prev_delay * 3)]`.
//! Decorrelation prevents the herd of concurrent retriers from
//! synchronising onto the same exponential schedule and
//! re-saturating an already-overloaded upstream.
//!
//! ## API
//!
//! Two surfaces:
//!
//! 1. [`next_delay`] — pure stateful step: take the prior delay,
//!    return the next. Caller owns the state. Easy to mock/test.
//!
//! 2. [`Backoff`] — convenience struct holding `(base, cap,
//!    prev_delay)`. Iterator-shaped: `b.next_delay()` returns
//!    a `Duration` and updates internal state.
//!
//! ## Why decorrelated?
//!
//! Pure exponential (`delay = base * 2^attempt`) creates a
//! synchronised retry storm: every retrier wakes at the same
//! `t = base * 2^N` for each `N`. Decorrelation samples
//! uniformly from a window whose top is `prev_delay * 3`,
//! breaking the synchrony.
//!
//! Reference: AWS Architecture Blog
//! "Exponential Backoff And Jitter" (2015).
//!
//! ## Determinism
//!
//! The function is randomised by design. Callers that need
//! reproducibility should swap in a deterministic RNG via
//! [`next_delay_with_rng`].

use rand::Rng;
use std::time::Duration;

/// Compute the next decorrelated-jitter delay given the prior
/// delay. First call should pass `prev = base`. Uses the
/// thread-local RNG.
///
/// Algorithm:
/// 1. Compute `top = min(cap, prev * 3)`.
/// 2. Return `uniform(base, top)`.
///
/// Both clamped to `Duration::from_millis(0)` when inputs are
/// degenerate (cap < base, etc.).
pub fn next_delay(base: Duration, cap: Duration, prev: Duration) -> Duration {
    let mut rng = rand::thread_rng();
    next_delay_with_rng(&mut rng, base, cap, prev)
}

/// Same as [`next_delay`] but takes an explicit RNG. Used by
/// tests that need determinism.
pub fn next_delay_with_rng<R: Rng>(
    rng: &mut R,
    base: Duration,
    cap: Duration,
    prev: Duration,
) -> Duration {
    // SEC PART 6: `Duration::as_millis()` returns u128;
    // saturate via try_from so a pathological caller-supplied
    // `Duration::MAX` (≈584 million years) doesn't wrap to a
    // small ms value and then sample a tiny delay.
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let cap_ms = u64::try_from(cap.as_millis()).unwrap_or(u64::MAX);
    if cap_ms == 0 {
        return Duration::from_millis(0);
    }
    // Top is `prev * 3` clamped at `cap`. Saturating mul because
    // a 10-minute prev * 3 still fits comfortably in u64 ms, but
    // pathological caller-supplied prev=u64::MAX would otherwise
    // wrap.
    let prev_ms = u64::try_from(prev.as_millis()).unwrap_or(u64::MAX);
    let raw_top = prev_ms.saturating_mul(3);
    let top = raw_top.min(cap_ms).max(base_ms);
    if top <= base_ms {
        return Duration::from_millis(base_ms);
    }
    // rand 0.9 + uniform sampling.
    let sample = rng.gen_range(base_ms..=top);
    Duration::from_millis(sample)
}

/// Stateful convenience wrapper. `next_delay()` advances the
/// internal `prev` field; reset via [`Self::reset`] (e.g. after
/// a successful call).
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    prev: Duration,
}

impl Backoff {
    /// New backoff anchored at `base`, capped at `cap`. Starts
    /// with `prev = base` so the first sampled delay sits in
    /// `[base, min(cap, base * 3)]`.
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            prev: base,
        }
    }

    /// Sample the next delay AND update internal state.
    pub fn next_delay(&mut self) -> Duration {
        let d = next_delay(self.base, self.cap, self.prev);
        self.prev = d;
        d
    }

    /// Same with an explicit RNG (for deterministic tests).
    pub fn next_delay_with_rng<R: Rng>(&mut self, rng: &mut R) -> Duration {
        let d = next_delay_with_rng(rng, self.base, self.cap, self.prev);
        self.prev = d;
        d
    }

    /// Reset internal state to base. Call after a successful
    /// attempt so the next failure starts fresh.
    pub fn reset(&mut self) {
        self.prev = self.base;
    }

    /// Read-only view of the current `prev` (for tracing fields).
    pub fn prev(&self) -> Duration {
        self.prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn delay_is_at_least_base() {
        let mut rng = StdRng::seed_from_u64(42);
        let d = next_delay_with_rng(&mut rng, ms(100), ms(10_000), ms(100));
        assert!(d.as_millis() >= 100);
    }

    #[test]
    fn delay_never_exceeds_cap() {
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..200 {
            let d = next_delay_with_rng(&mut rng, ms(100), ms(500), ms(10_000));
            assert!(
                d.as_millis() <= 500,
                "delay {}ms exceeded cap=500ms",
                d.as_millis()
            );
        }
    }

    #[test]
    fn returns_zero_when_cap_is_zero() {
        let mut rng = StdRng::seed_from_u64(1);
        let d = next_delay_with_rng(&mut rng, ms(100), Duration::ZERO, ms(100));
        assert_eq!(d, Duration::ZERO);
    }

    #[test]
    fn returns_base_when_cap_equals_base() {
        let mut rng = StdRng::seed_from_u64(3);
        let d = next_delay_with_rng(&mut rng, ms(100), ms(100), ms(100));
        assert_eq!(d, ms(100));
    }

    #[test]
    fn returns_base_when_prev_is_smaller_than_base() {
        // Degenerate caller: prev < base means top would be
        // prev * 3 < base * 3. Clamp at base. Result: exact base.
        let mut rng = StdRng::seed_from_u64(5);
        let d = next_delay_with_rng(&mut rng, ms(500), ms(10_000), ms(10));
        assert_eq!(d, ms(500));
    }

    #[test]
    fn top_is_prev_times_three_clamped_at_cap() {
        // Force a high prev so the cap matters.
        let mut rng = StdRng::seed_from_u64(9);
        for _ in 0..100 {
            let d = next_delay_with_rng(&mut rng, ms(100), ms(1_000), ms(500));
            // prev*3 = 1500 > cap=1000 → effective top = 1000.
            assert!(d.as_millis() <= 1000);
            assert!(d.as_millis() >= 100);
        }
    }

    #[test]
    fn no_saturation_panic_on_huge_prev() {
        // Pathological caller supplies near-MAX prev. Must not
        // panic; saturating_mul keeps top bounded.
        let mut rng = StdRng::seed_from_u64(11);
        let d = next_delay_with_rng(
            &mut rng,
            ms(100),
            ms(60_000),
            Duration::from_secs(u32::MAX as u64),
        );
        assert!(d.as_millis() <= 60_000);
        assert!(d.as_millis() >= 100);
    }

    #[test]
    fn backoff_advances_internal_state() {
        let mut rng = StdRng::seed_from_u64(13);
        let mut b = Backoff::new(ms(100), ms(10_000));
        let d1 = b.next_delay_with_rng(&mut rng);
        let d2 = b.next_delay_with_rng(&mut rng);
        let d3 = b.next_delay_with_rng(&mut rng);
        // Each sample must be at-least-base. Over many calls
        // the prev should trend upward toward cap (not
        // monotonically — that's the decorrelation point).
        for d in [d1, d2, d3] {
            assert!(d.as_millis() >= 100);
            assert!(d.as_millis() <= 10_000);
        }
        // prev advances after each call.
        assert_eq!(b.prev(), d3);
    }

    #[test]
    fn backoff_reset_returns_to_base() {
        let mut rng = StdRng::seed_from_u64(17);
        let mut b = Backoff::new(ms(100), ms(10_000));
        let _ = b.next_delay_with_rng(&mut rng);
        let _ = b.next_delay_with_rng(&mut rng);
        b.reset();
        assert_eq!(b.prev(), ms(100));
    }

    #[test]
    fn decorrelation_introduces_variance() {
        // Statistical sanity: 200 samples with a 100ms..1s
        // window must NOT all collapse to a single delay.
        let mut rng = StdRng::seed_from_u64(23);
        let mut b = Backoff::new(ms(100), ms(1_000));
        let mut samples: Vec<u64> = Vec::with_capacity(200);
        for _ in 0..200 {
            samples.push(b.next_delay_with_rng(&mut rng).as_millis() as u64);
            b.reset();
        }
        let min = *samples.iter().min().unwrap();
        let max = *samples.iter().max().unwrap();
        // Must span at least 100ms — anything tighter than that
        // means the RNG is broken or the sampler is degenerate.
        assert!(
            max - min >= 100,
            "decorrelation produced near-zero variance: min={min} max={max}"
        );
    }
}
