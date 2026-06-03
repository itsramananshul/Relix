//! The **Bench** — an Operative's execution workspace, and its
//! serverless **sleep/wake** lifecycle (Pillar 1, the
//! hibernate-to-~$0 model transplanted from Hermes's Modal/Daytona
//! backends).
//!
//! A Bench is where a Brief's work runs. The point of this module is
//! the lifecycle: a Bench can be **Active** (running, costing
//! compute) or **Hibernated** — nothing runs, only a filesystem/VM
//! **snapshot** persists, so an idle long-running Brief costs ~$0
//! between Shifts and **wakes with its exact state**. Each Bench is
//! keyed by its Brief (`task_id`).
//!
//! This module is the lifecycle ledger + state machine; the actual
//! snapshot/VM backend (a local worktree, a Modal snapshot, a
//! Daytona stoppable sandbox) plugs in behind the `snapshot_ref`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Where a Bench sits in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceState {
    /// Running — costing compute.
    Active,
    /// Asleep — nothing runs, only the snapshot persists (~$0).
    Hibernated,
}

/// One Bench's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub task_id: String,
    pub state: WorkspaceState,
    /// The snapshot to wake from (filesystem snapshot id, stopped-VM
    /// handle, worktree path…). `None` for a never-hibernated Bench.
    pub snapshot_ref: Option<String>,
    pub updated_at: i64,
}

/// A process-wide ledger of Benches, keyed by Brief id. Cheap to
/// clone (an `Arc` handle).
#[derive(Clone, Default)]
pub struct BenchLedger {
    inner: Arc<Mutex<HashMap<String, Workspace>>>,
}

impl BenchLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensure a Bench is **Active** for `task_id`. Creates one if
    /// absent; **wakes** a hibernated one. Returns the snapshot to
    /// restore from when waking (so the caller can rehydrate exact
    /// state); `None` for a fresh or already-active Bench.
    pub fn ensure_active(&self, task_id: &str) -> Option<String> {
        let now = unix_now();
        let mut g = self.lock();
        match g.get_mut(task_id) {
            Some(w) => {
                let restore = if w.state == WorkspaceState::Hibernated {
                    w.snapshot_ref.clone()
                } else {
                    None
                };
                w.state = WorkspaceState::Active;
                w.updated_at = now;
                restore
            }
            None => {
                g.insert(
                    task_id.to_string(),
                    Workspace {
                        task_id: task_id.to_string(),
                        state: WorkspaceState::Active,
                        snapshot_ref: None,
                        updated_at: now,
                    },
                );
                None
            }
        }
    }

    /// **Hibernate** a Bench: persist `snapshot_ref` and drop to ~$0.
    /// No-op if the Bench doesn't exist.
    pub fn hibernate(&self, task_id: &str, snapshot_ref: impl Into<String>) {
        let now = unix_now();
        let mut g = self.lock();
        if let Some(w) = g.get_mut(task_id) {
            w.state = WorkspaceState::Hibernated;
            w.snapshot_ref = Some(snapshot_ref.into());
            w.updated_at = now;
        }
    }

    /// The Bench's current state, if it exists.
    pub fn state(&self, task_id: &str) -> Option<WorkspaceState> {
        self.lock().get(task_id).map(|w| w.state)
    }

    /// The Bench's snapshot reference, if any.
    pub fn snapshot_ref(&self, task_id: &str) -> Option<String> {
        self.lock()
            .get(task_id)
            .and_then(|w| w.snapshot_ref.clone())
    }

    /// Mark a Bench as freshly active — resets its idle clock
    /// without changing state. The heartbeat calls this each Shift
    /// tick so a Bench that's busy mid-work isn't auto-hibernated
    /// out from under it. No-op if the Bench doesn't exist.
    pub fn touch(&self, task_id: &str) {
        let now = unix_now();
        let mut g = self.lock();
        if let Some(w) = g.get_mut(task_id) {
            w.updated_at = now;
        }
    }

    /// The Briefs whose Bench is **Active** but has been idle (no
    /// `ensure_active`/`touch`) for at least `idle_after` seconds as
    /// of `now` — the serverless auto-sleep candidates. The caller
    /// snapshots each and calls [`hibernate`]. Longest-idle first.
    pub fn idle_active_benches(&self, now: i64, idle_after: i64) -> Vec<String> {
        let g = self.lock();
        let mut v: Vec<(String, i64)> = g
            .values()
            .filter(|w| w.state == WorkspaceState::Active)
            .filter(|w| now.saturating_sub(w.updated_at) >= idle_after)
            .map(|w| (w.task_id.clone(), w.updated_at))
            .collect();
        // Oldest `updated_at` = longest idle → first.
        v.sort_by_key(|(_, updated)| *updated);
        v.into_iter().map(|(id, _)| id).collect()
    }

    /// Serverless auto-sleep tick: hibernate every Active Bench
    /// idle past `idle_after`, taking each one's snapshot via
    /// `snapshot` (the backend handle — a worktree path, a Modal
    /// snapshot id, …). Returns the task_ids that were put to sleep.
    pub fn hibernate_idle<F>(&self, now: i64, idle_after: i64, snapshot: F) -> Vec<String>
    where
        F: Fn(&str) -> String,
    {
        let candidates = self.idle_active_benches(now, idle_after);
        for id in &candidates {
            let snap = snapshot(id);
            self.hibernate(id, snap);
        }
        candidates
    }

    /// Tear a Bench down entirely (its work is done).
    pub fn release(&self, task_id: &str) {
        self.lock().remove(task_id);
    }

    pub fn active_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|w| w.state == WorkspaceState::Active)
            .count()
    }

    pub fn hibernated_count(&self) -> usize {
        self.lock()
            .values()
            .filter(|w| w.state == WorkspaceState::Hibernated)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Workspace>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_hibernates_to_zero_and_wakes_with_its_snapshot() {
        let b = BenchLedger::new();

        // Fresh Bench: nothing to restore.
        assert_eq!(b.ensure_active("t1"), None);
        assert_eq!(b.state("t1"), Some(WorkspaceState::Active));
        assert_eq!(b.active_count(), 1);

        // Hibernate with a snapshot → ~$0.
        b.hibernate("t1", "snap-abc");
        assert_eq!(b.state("t1"), Some(WorkspaceState::Hibernated));
        assert_eq!(b.snapshot_ref("t1").as_deref(), Some("snap-abc"));
        assert_eq!(b.hibernated_count(), 1);
        assert_eq!(b.active_count(), 0);

        // Wake → returns the snapshot to restore from, goes Active.
        assert_eq!(b.ensure_active("t1").as_deref(), Some("snap-abc"));
        assert_eq!(b.state("t1"), Some(WorkspaceState::Active));
        // Already active → nothing to restore.
        assert_eq!(b.ensure_active("t1"), None);

        // Release tears it down.
        b.release("t1");
        assert_eq!(b.state("t1"), None);
    }

    #[test]
    fn benches_are_independent_per_brief() {
        let b = BenchLedger::new();
        b.ensure_active("a");
        b.ensure_active("b");
        b.hibernate("a", "snap-a");
        assert_eq!(b.state("a"), Some(WorkspaceState::Hibernated));
        assert_eq!(b.state("b"), Some(WorkspaceState::Active));
        assert_eq!(b.active_count(), 1);
        assert_eq!(b.hibernated_count(), 1);
        // Hibernating an unknown Bench is a no-op.
        b.hibernate("nope", "x");
        assert_eq!(b.state("nope"), None);
    }

    #[test]
    fn hibernate_idle_sleeps_idle_benches_with_their_snapshot() {
        let b = BenchLedger::new();
        b.ensure_active("a");
        b.ensure_active("c");
        let far = unix_now() + 10_000;

        // Snapshot fn names the handle after the bench id.
        let slept = b.hibernate_idle(far, 3600, |id| format!("snap-{id}"));
        assert_eq!(slept.len(), 2);
        assert_eq!(b.state("a"), Some(WorkspaceState::Hibernated));
        assert_eq!(b.state("c"), Some(WorkspaceState::Hibernated));
        assert_eq!(b.snapshot_ref("a").as_deref(), Some("snap-a"));
        assert_eq!(b.active_count(), 0);

        // Nothing idle now → no-op.
        b.ensure_active("d");
        assert!(
            b.hibernate_idle(unix_now(), 3600, |_| "x".into())
                .is_empty()
        );
        assert_eq!(b.state("d"), Some(WorkspaceState::Active));
    }

    #[test]
    fn idle_active_benches_lists_auto_sleep_candidates() {
        let b = BenchLedger::new();
        b.ensure_active("x");
        b.ensure_active("y");

        // Far in the future → both are long-idle candidates.
        let far = unix_now() + 10_000;
        assert_eq!(b.idle_active_benches(far, 3600).len(), 2);
        // At ~creation time → neither has aged out yet.
        assert!(b.idle_active_benches(unix_now(), 3600).is_empty());

        // A hibernated Bench is never an auto-sleep candidate.
        b.hibernate("x", "snap");
        assert_eq!(b.idle_active_benches(far, 3600), vec!["y".to_string()]);

        // touch resets the idle clock — y is busy again.
        b.touch("y");
        assert!(b.idle_active_benches(unix_now(), 3600).is_empty());
    }
}
