//! GAP 6 — Memory Integrity Auditor.
//!
//! Periodic background task that re-reads Layer 3 (Observation)
//! and Layer 4 (Model) and flags integrity issues. Runs every
//! [`DEFAULT_AUDIT_INTERVAL_SECS`] (24h) by default. Pure read
//! pass — never mutates the store. Findings are emitted as
//! structured `tracing::warn!` lines so existing log shippers
//! pick them up without new wiring.
//!
//! Three checks per tick:
//!
//! 1. **Contradiction sweep** — for each (source, valid
//!    observations) pair, run the same [`super::anomaly`]
//!    contradiction detector over each pair of observations and
//!    log every clash. The check that fires at write-time only
//!    sees prior observations; this sweep is the symmetric one
//!    that catches contradictions introduced by independent
//!    writes that happened to interleave.
//! 2. **Missing source attribution** — every Layer 3 / Layer 4
//!    record SHOULD have a non-empty `source`. If the migration
//!    or a buggy writer let one through, log it so the operator
//!    can repair it.
//! 3. **Stale unmodeled subjects** — sources that have valid
//!    observations older than [`STALE_OBS_AGE_SECS`] but no
//!    Layer 4 model are stuck; the curator presumably failed
//!    silently. Worth flagging.
//!
//! The result of each tick is a [`IntegrityReport`] that the
//! caller (the controller's scheduler) can also pull on demand
//! via [`MemoryIntegrityAuditor::run_once`].

use std::sync::Arc;
use std::time::Duration;

use crate::nodes::memory::anomaly;
use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer};

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 24 hours.
pub const DEFAULT_AUDIT_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Layer-3 observations older than 30 days that still have no
/// Layer-4 model on the same source flag a stale-subject
/// finding.
pub const STALE_OBS_AGE_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Default, Clone)]
pub struct IntegrityReport {
    pub contradictions: usize,
    pub missing_source: usize,
    pub stale_unmodeled: usize,
    pub unsourced_models: usize,
    pub sources_audited: usize,
    pub started_at: i64,
    pub finished_at: i64,
}

pub struct MemoryIntegrityAuditor {
    store: Arc<LayeredMemoryStore>,
    interval: Duration,
}

impl MemoryIntegrityAuditor {
    pub fn new(store: Arc<LayeredMemoryStore>) -> Self {
        Self {
            store,
            interval: Duration::from_secs(DEFAULT_AUDIT_INTERVAL_SECS),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Run one full audit pass synchronously. Pure read.
    pub fn run_once(&self) -> Result<IntegrityReport, String> {
        let started_at = unix_secs();
        let mut report = IntegrityReport {
            started_at,
            ..Default::default()
        };

        // ── 1. contradiction sweep ───────────────────────────
        let obs = self
            .store
            .list(Some(MemoryLayer::Observation), None, 10_000, 0)
            .map_err(|e| format!("integrity: list observations: {e}"))?;
        let mut by_source: std::collections::BTreeMap<String, Vec<_>> =
            std::collections::BTreeMap::new();
        for r in obs.into_iter().filter(|r| r.valid_to.is_none()) {
            by_source.entry(r.source.clone()).or_default().push(r);
        }
        for (source, group) in &by_source {
            report.sources_audited += 1;
            for (i, r) in group.iter().enumerate() {
                let rest = &group[(i + 1)..];
                if let Some(clash_id) = anomaly::first_contradiction(&r.text, rest) {
                    report.contradictions += 1;
                    tracing::warn!(
                        source = %source,
                        observation = %r.id,
                        contradicts = %clash_id,
                        "memory.integrity: contradiction detected"
                    );
                }
            }
        }

        // ── 2. missing source attribution ────────────────────
        let unsourced_obs = self
            .store
            .list_observations_missing_source(1000)
            .map_err(|e| format!("integrity: list observations missing source: {e}"))?;
        report.missing_source = unsourced_obs.len();
        for r in &unsourced_obs {
            tracing::warn!(
                observation = %r.id,
                "memory.integrity: observation has empty source"
            );
        }
        let unsourced_models = self
            .store
            .list_unsourced_models(1000)
            .map_err(|e| format!("integrity: list unsourced models: {e}"))?;
        report.unsourced_models = unsourced_models.len();
        for r in &unsourced_models {
            tracing::warn!(
                model = %r.id,
                "memory.integrity: model has empty source"
            );
        }

        // ── 3. stale unmodeled subjects ──────────────────────
        let now = unix_secs();
        let cutoff = now - STALE_OBS_AGE_SECS;
        let stale = self
            .store
            .list_stale_unmodeled_observations(cutoff, 1000)
            .map_err(|e| format!("integrity: list stale unmodeled: {e}"))?;
        let mut unique_sources: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for r in &stale {
            unique_sources.insert(r.source.clone());
        }
        report.stale_unmodeled = unique_sources.len();
        for source in &unique_sources {
            tracing::warn!(
                source = %source,
                "memory.integrity: source has stale observations but no Layer-4 model"
            );
        }

        report.finished_at = unix_secs();
        tracing::info!(
            sources_audited = report.sources_audited,
            contradictions = report.contradictions,
            missing_source = report.missing_source,
            unsourced_models = report.unsourced_models,
            stale_unmodeled = report.stale_unmodeled,
            "memory.integrity: audit pass complete"
        );
        Ok(report)
    }

    /// Spawn the audit loop on the current tokio runtime.
    /// Returns a JoinHandle the caller can `.abort()` to stop
    /// the loop (e.g. on controller shutdown).
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.interval);
            // First tick fires immediately; we don't want a
            // boot-time audit firing into a cold cache, so skip
            // the first one.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = self.run_once() {
                    tracing::warn!(error = %e, "memory.integrity: audit pass failed");
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};
    use tempfile::TempDir;

    fn store_in(td: &TempDir) -> Arc<LayeredMemoryStore> {
        let path = td.path().join("mem.sqlite");
        Arc::new(LayeredMemoryStore::open(&path).expect("open"))
    }

    fn obs(id: &str, source: &str, text: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, source);
        r.layer = MemoryLayer::Observation;
        r
    }

    #[test]
    fn empty_store_reports_zero() {
        let td = TempDir::new().unwrap();
        let store = store_in(&td);
        let report = MemoryIntegrityAuditor::new(store).run_once().unwrap();
        assert_eq!(report.contradictions, 0);
        assert_eq!(report.missing_source, 0);
        assert_eq!(report.stale_unmodeled, 0);
        assert_eq!(report.sources_audited, 0);
    }

    #[test]
    fn contradiction_between_two_observations_is_counted() {
        let td = TempDir::new().unwrap();
        let store = store_in(&td);
        let a = obs("a1", "alice", "User likes Postgres");
        let b = obs("b1", "alice", "User dislikes Postgres");
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        let report = MemoryIntegrityAuditor::new(store).run_once().unwrap();
        assert!(report.contradictions >= 1);
        assert_eq!(report.sources_audited, 1);
    }

    #[test]
    fn observation_with_empty_source_is_counted() {
        let td = TempDir::new().unwrap();
        let store = store_in(&td);
        let mut r = obs("a1", "", "User likes Postgres");
        r.source = String::new();
        store.insert(&r).unwrap();
        let report = MemoryIntegrityAuditor::new(store).run_once().unwrap();
        assert_eq!(report.missing_source, 1);
    }

    #[test]
    fn stale_observation_without_model_is_flagged() {
        let td = TempDir::new().unwrap();
        let store = store_in(&td);
        let mut old = obs("o-old", "alice", "User likes terse replies");
        let long_ago = unix_secs() - (STALE_OBS_AGE_SECS + 10_000);
        old.created_at = long_ago;
        old.observed_at = long_ago;
        old.valid_from = long_ago;
        store.insert(&old).unwrap();
        let report = MemoryIntegrityAuditor::new(store).run_once().unwrap();
        assert_eq!(report.stale_unmodeled, 1);
    }

    #[test]
    fn observation_with_model_does_not_flag_stale() {
        let td = TempDir::new().unwrap();
        let store = store_in(&td);
        let mut old = obs("o-old", "alice", "User likes terse replies");
        let long_ago = unix_secs() - (STALE_OBS_AGE_SECS + 10_000);
        old.created_at = long_ago;
        old.observed_at = long_ago;
        old.valid_from = long_ago;
        store.insert(&old).unwrap();
        let mut model = MemoryRecord::new_raw("m1", "alice is terse.", "alice");
        model.layer = MemoryLayer::Model;
        store.insert(&model).unwrap();
        let report = MemoryIntegrityAuditor::new(store).run_once().unwrap();
        assert_eq!(report.stale_unmodeled, 0);
    }
}
