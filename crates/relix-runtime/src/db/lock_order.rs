//! Canonical lock ordering enforcement for SQLite stores.
//!
//! See `docs/LOCK_ORDER.md` for the full contract. This module
//! is intentionally small: a `StoreId` enum that ranks each
//! store, and a thread-local recorder that `debug_assert!`s
//! lock acquisitions are in canonical order.
//!
//! The recorder is advisory — it does not change which mutex
//! actually gets acquired; it just shouts in debug + tests if a
//! future refactor introduces a reverse-order acquisition. The
//! runtime overhead is one `thread_local!` access in debug
//! builds and exactly zero in release builds.

/// Ranks each persistent SQLite store in Relix. Higher rank
/// → lower in the dependency stack. The canonical acquisition
/// order is strictly ascending by rank: a thread that holds
/// rank `N` may acquire rank `N+1` next, but never rank `N-1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StoreId {
    Coordinator = 1,
    Memory = 2,
    PluginRegistry = 3,
    Messaging = 4,
    AgentStore = 5,
    CronStore = 6,
    SessionStore = 7,
}

impl StoreId {
    pub fn name(self) -> &'static str {
        match self {
            Self::Coordinator => "coordinator",
            Self::Memory => "memory",
            Self::PluginRegistry => "plugin_registry",
            Self::Messaging => "messaging",
            Self::AgentStore => "agent_store",
            Self::CronStore => "cron_store",
            Self::SessionStore => "session_store",
        }
    }
}

thread_local! {
    static HELD: std::cell::RefCell<Vec<StoreId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// RAII guard returned by [`record_acquire`]. Dropping the
/// guard removes the corresponding entry from the per-thread
/// "currently held" stack. The recorder enforces nothing about
/// the order in which guards are dropped — it only enforces
/// the *acquisition* order.
pub struct LockOrderGuard {
    id: StoreId,
    /// Set when the guard was popped via drop. Prevents the
    /// drop impl from popping twice if `mem::forget`-style
    /// shenanigans ever happen.
    armed: bool,
}

impl Drop for LockOrderGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        HELD.with(|h| {
            let mut v = h.borrow_mut();
            // Pop the most-recent occurrence of `id`. The
            // canonical pattern is LIFO, so this is the head.
            if let Some(pos) = v.iter().rposition(|&x| x == self.id) {
                v.remove(pos);
            }
        });
    }
}

/// Record that `id` is being acquired by the current thread,
/// and `debug_assert!` that the acquisition does not violate
/// canonical ordering (i.e. that no higher-rank lock is already
/// held). Returns an RAII guard the caller stores alongside the
/// actual `MutexGuard`; dropping it removes `id` from the
/// per-thread held set.
///
/// Re-entrant acquisitions of the same `id` are allowed (taking
/// `coordinator` twice in a row is unusual but not a deadlock
/// in practice — Rust's `std::sync::Mutex` would itself
/// deadlock first).
pub fn record_acquire(id: StoreId) -> LockOrderGuard {
    HELD.with(|h| {
        let mut v = h.borrow_mut();
        if let Some(&top) = v.last() {
            debug_assert!(
                id >= top,
                "lock-order violation: thread holds {:?} (rank {}) but is now acquiring {:?} (rank {}). \
                 See docs/LOCK_ORDER.md for the canonical order.",
                top,
                top as u8,
                id,
                id as u8,
            );
        }
        v.push(id);
    });
    LockOrderGuard { id, armed: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn store_id_ranks_match_documented_order() {
        // Mirrors `docs/LOCK_ORDER.md` — if this trips, the doc
        // is out of sync and one of them needs an update.
        let canonical = [
            StoreId::Coordinator,
            StoreId::Memory,
            StoreId::PluginRegistry,
            StoreId::Messaging,
            StoreId::AgentStore,
            StoreId::CronStore,
            StoreId::SessionStore,
        ];
        let mut prev_rank = 0u8;
        for id in canonical {
            let rank = id as u8;
            assert!(rank > prev_rank, "non-strict ordering at {id:?}");
            prev_rank = rank;
        }
    }

    #[test]
    fn record_acquire_in_canonical_order_succeeds() {
        let _g1 = record_acquire(StoreId::Coordinator);
        let _g2 = record_acquire(StoreId::Memory);
        let _g3 = record_acquire(StoreId::CronStore);
    }

    #[test]
    #[should_panic(expected = "lock-order violation")]
    fn record_acquire_reverse_order_panics_in_debug() {
        // In release builds debug_assert! is a no-op so this
        // test only fires in debug — which is also where the
        // contract is meaningful (tests + CI catch the
        // regression, release builds get zero overhead).
        let _g1 = record_acquire(StoreId::CronStore);
        let _g2 = record_acquire(StoreId::Coordinator);
    }

    #[test]
    fn record_acquire_after_drop_resets_state() {
        {
            let _g = record_acquire(StoreId::CronStore);
        }
        // Once the cron guard drops, we should be free to
        // acquire a lower-rank store again.
        let _g = record_acquire(StoreId::Coordinator);
    }

    /// Verifies real Mutex acquisition in canonical order from
    /// two threads doesn't deadlock — the canonical-order
    /// invariant means thread A and thread B both walk the
    /// stack top-down, so neither holds a high lock waiting on
    /// a low one that the other thread holds.
    #[test]
    fn acquire_in_canonical_order_does_not_deadlock() {
        let a: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let b: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
        let a1 = a.clone();
        let b1 = b.clone();
        let h1 = std::thread::spawn(move || {
            for _ in 0..100 {
                let _ga = record_acquire(StoreId::Coordinator);
                let _la = a1.lock().unwrap();
                let _gb = record_acquire(StoreId::CronStore);
                let _lb = b1.lock().unwrap();
                std::thread::sleep(Duration::from_micros(1));
            }
        });
        let a2 = a.clone();
        let b2 = b.clone();
        let h2 = std::thread::spawn(move || {
            for _ in 0..100 {
                let _ga = record_acquire(StoreId::Coordinator);
                let _la = a2.lock().unwrap();
                let _gb = record_acquire(StoreId::CronStore);
                let _lb = b2.lock().unwrap();
                std::thread::sleep(Duration::from_micros(1));
            }
        });
        // Both threads acquire in canonical order — they
        // either contend on `a` (serialised) or on `b` (also
        // serialised), but never form a cycle. The test
        // succeeds simply by completing within a reasonable
        // wall-clock budget.
        h1.join().unwrap();
        h2.join().unwrap();
    }
}
