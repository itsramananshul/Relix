//! RELIX-7.19 — `LastConfidenceCell`.
//!
//! Lock-free shared slot for "the confidence score of the
//! most recently completed `remote_call` in this execution
//! context." The dispatch bridge writes; the SOL VM's
//! `last_confidence()` builtin reads.
//!
//! Stored as the bit pattern of an `f32` in an `AtomicU32`
//! so reads + writes are wait-free and cross-thread safe
//! without any locking. The initial value is `1.0` so flows
//! that read `last_confidence()` before making any
//! `remote_call` see a neutral score (per spec).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Cheap-to-clone (Arc-backed) shared confidence slot. One
/// cell is created per VM execution and threaded into both
/// the VM (for the `last_confidence()` builtin) and the
/// dispatcher integration that writes to it after every
/// remote call.
#[derive(Clone, Debug)]
pub struct LastConfidenceCell {
    inner: Arc<AtomicU32>,
}

impl Default for LastConfidenceCell {
    fn default() -> Self {
        Self::new()
    }
}

impl LastConfidenceCell {
    /// Construct with initial value `1.0` (the
    /// neutral / "no calls yet" reading per spec).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
        }
    }

    /// Construct with an explicit initial value. Used by
    /// tests + scenarios where the host wants a different
    /// starting reading (e.g. carry-over from a prior step).
    pub fn with_initial(value: f32) -> Self {
        Self {
            inner: Arc::new(AtomicU32::new(value.clamp(0.0, 1.0).to_bits())),
        }
    }

    /// Read the current confidence. Wait-free; never blocks.
    pub fn get(&self) -> f32 {
        f32::from_bits(self.inner.load(Ordering::Relaxed))
    }

    /// Write a new confidence value. Clamped to `[0.0, 1.0]`.
    /// Wait-free; never blocks.
    pub fn set(&self, value: f32) {
        self.inner
            .store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_cell_starts_at_one() {
        let c = LastConfidenceCell::new();
        assert!((c.get() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_then_get_round_trips() {
        let c = LastConfidenceCell::new();
        c.set(0.42);
        assert!((c.get() - 0.42).abs() < 1e-6);
    }

    #[test]
    fn clamping_on_set_prevents_out_of_range_values() {
        let c = LastConfidenceCell::new();
        c.set(2.5);
        assert!((c.get() - 1.0).abs() < 1e-6);
        c.set(-0.5);
        assert_eq!(c.get(), 0.0);
    }

    #[test]
    fn clones_share_the_same_storage() {
        let c1 = LastConfidenceCell::new();
        let c2 = c1.clone();
        c1.set(0.25);
        assert!((c2.get() - 0.25).abs() < 1e-6);
    }
}
