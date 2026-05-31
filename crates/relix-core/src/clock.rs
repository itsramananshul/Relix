//! NOT-DONE 1 — testable wall-clock abstraction.
//!
//! Every TTL / expiry decision in the codebase has to read the
//! current time. Doing that via `std::time::SystemTime::now()`
//! makes the surrounding code untestable without sleeping. The
//! [`Clock`] trait gives every TTL-sensitive surface a single
//! injection point: production code wires
//! [`SystemClock`]; tests wire [`FakeClock`] + drive
//! [`FakeClock::advance`] to step deterministic boundaries.
//!
//! Lives in `relix-core` so the channel crates, runtime,
//! controller, bridge, and CLI all import the same trait
//! object.
//!
//! ## Where the abstraction stops
//!
//! `Clock::now_ms` covers EXPLICIT timestamp reads. It does NOT
//! cover the time progression inside
//! `tokio::time::sleep` / `tokio::time::interval`. Tests that
//! exercise an `await sleep` use `tokio::time::pause()` +
//! `tokio::time::advance()` AND advance the [`FakeClock`] by
//! the same delta — that's the established Tokio pattern; the
//! `Clock` trait is orthogonal to it.

use std::sync::atomic::{AtomicI64, Ordering};

/// Trait every TTL-sensitive surface depends on instead of
/// calling [`std::time::SystemTime::now`] directly. Cheap to
/// clone via `Arc<dyn Clock>`.
pub trait Clock: Send + Sync {
    /// Unix milliseconds since the epoch. Implementations are
    /// expected to be cheap (single read; no allocations).
    fn now_ms(&self) -> i64;
}

/// Production implementation. Reads
/// [`std::time::SystemTime::now`] and converts to unix ms.
/// Saturates at `i64::MAX` on the (impossible) overflow path so
/// callers never see a wrap-around negative timestamp.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        system_now_ms(std::time::SystemTime::now())
    }
}

/// Convert a `SystemTime` to unix milliseconds for [`SystemClock`].
/// Consistent with [`crate::types::Timestamp::now`]: a clock error is
/// LOGGED and handled, never silently swallowed and never a panic.
/// - a pre-epoch clock → `WARN` + clamp to epoch 0 (the old code returned
///   `1970` silently via `.unwrap_or(0)`, hiding the fault)
/// - a clock past the `i64::MAX`-millisecond horizon → `WARN` + clamp to
///   `i64::MAX`
///
/// Factored out so the clock-error path is unit-testable with an injected
/// `SystemTime`.
fn system_now_ms(now: std::time::SystemTime) -> i64 {
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(dur) => {
            let ms = dur.as_millis();
            if ms > i64::MAX as u128 {
                tracing::warn!(
                    ms,
                    "system clock is past the i64::MAX-millisecond horizon — clamping now_ms to i64::MAX"
                );
                i64::MAX
            } else {
                ms as i64
            }
        }
        Err(_) => {
            tracing::warn!(
                "system clock is before the Unix epoch — clamping now_ms to epoch 0 (1970-01-01); check the system clock"
            );
            0
        }
    }
}

/// Test implementation. The current time is stored in an
/// [`AtomicI64`] so tests can advance it deterministically from
/// any thread. The whole instance is cheap to clone behind an
/// [`std::sync::Arc`]; advances are visible to every cloned
/// handle.
#[derive(Debug, Default)]
pub struct FakeClock {
    /// Backing storage for the current "now" value. Exposed so
    /// tests can `clock.now_ms.store(t, Ordering::SeqCst)` for
    /// absolute time jumps when [`Self::advance`] doesn't fit.
    pub now_ms: AtomicI64,
}

impl FakeClock {
    /// Construct with an explicit starting time.
    pub fn new(now_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(now_ms),
        }
    }

    /// Advance the clock by `ms` milliseconds. Negative deltas
    /// are accepted — tests occasionally need to "rewind" to
    /// exercise pre-issue conditions.
    pub fn advance(&self, ms: i64) {
        self.now_ms.fetch_add(ms, Ordering::SeqCst);
    }

    /// Absolute setter. Equivalent to
    /// `self.now_ms.store(t, Ordering::SeqCst)`. Provided for
    /// readability at the call site.
    pub fn set(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn system_clock_returns_positive_increasing_value() {
        let c = SystemClock;
        let t1 = c.now_ms();
        let t2 = c.now_ms();
        assert!(t1 > 0);
        assert!(t2 >= t1);
    }

    #[test]
    fn system_now_ms_clamps_pre_epoch_clock_to_zero_without_panic() {
        use std::time::{Duration, UNIX_EPOCH};
        // A clock error must be handled (clamp to epoch 0), not a
        // panic and not a silent wrong value — consistent with
        // `Timestamp::now`.
        let pre_epoch = UNIX_EPOCH - Duration::from_secs(5);
        assert_eq!(super::system_now_ms(pre_epoch), 0);
    }

    #[test]
    fn system_now_ms_converts_a_normal_instant_to_millis() {
        use std::time::{Duration, UNIX_EPOCH};
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(super::system_now_ms(t), 1_700_000_000_123);
    }

    #[test]
    fn fake_clock_returns_constructor_value() {
        let c = FakeClock::new(1_700_000_000_000);
        assert_eq!(c.now_ms(), 1_700_000_000_000);
    }

    #[test]
    fn fake_clock_advance_moves_now_ms_forward() {
        let c = FakeClock::new(1_000);
        c.advance(500);
        assert_eq!(c.now_ms(), 1_500);
        c.advance(0);
        assert_eq!(c.now_ms(), 1_500);
    }

    #[test]
    fn fake_clock_advance_accepts_negative_delta_for_rewind() {
        let c = FakeClock::new(1_000);
        c.advance(-200);
        assert_eq!(c.now_ms(), 800);
    }

    #[test]
    fn fake_clock_set_overwrites_absolute() {
        let c = FakeClock::new(1_000);
        c.set(5_000);
        assert_eq!(c.now_ms(), 5_000);
    }

    #[test]
    fn fake_clock_advances_are_visible_across_arc_clones() {
        let c = Arc::new(FakeClock::new(0));
        let c2 = c.clone();
        c.advance(100);
        assert_eq!(c2.now_ms(), 100);
        let h = std::thread::spawn(move || c2.advance(50));
        h.join().unwrap();
        assert_eq!(c.now_ms(), 150);
    }

    #[test]
    fn dyn_clock_dispatch_works_for_both_impls() {
        let sys: Arc<dyn Clock> = Arc::new(SystemClock);
        let fake: Arc<dyn Clock> = Arc::new(FakeClock::new(42));
        assert!(sys.now_ms() > 0);
        assert_eq!(fake.now_ms(), 42);
    }
}
