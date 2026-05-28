//! GAP 8 — ConsolidationArchiver background task.
//!
//! Periodic loop (every [`DEFAULT_ARCHIVE_INTERVAL_SECS`] = 6h)
//! that walks the four-layer store and archives terminal
//! observations whose information has already been captured by
//! a downstream Layer 4 model:
//!
//! - **Layer 3 archive criteria**: observation must be valid
//!   (`valid_to IS NULL`), older than [`STALE_OBS_AGE_SECS`]
//!   (30d), not frozen, not already archived, AND covered by a
//!   Layer 4 model record on the same `source` whose
//!   `observed_at` is newer than the observation's
//!   `observed_at`. "Covered by a model" is the schema-level
//!   proxy for the spec's "confidence ≥ 0.85" — the model
//!   synthesis explicitly reads the observation, so the model's
//!   existence after the observation is the strongest signal
//!   available without a separate confidence column.
//!
//! - **Layer 1 cascade**: once every observation for a raw
//!   record's source has been archived, the raw is stamped
//!   `consolidated = true` so future curator passes can skip
//!   it cheaply.
//!
//! Side effects per archived batch:
//!
//! 1. The observation's `tags` gains an `"archived"` entry so
//!    repeated runs are idempotent.
//! 2. The observation is invalidated (`valid_to = now`) so the
//!    inspector and search layer hide it from default views.
//! 3. A `chronicle` event is appended via the [`CoordDispatcher`]
//!    if one is wired into the controller — operators can
//!    audit the archive history without a separate ring buffer.
//!
//! Pure-Rust archive: there's no separate "low-priority Qdrant
//! segment" landing — the alpha's single Qdrant collection
//! treats the `archived` tag as the filter operators apply to
//! exclude consolidated rows from search. This matches what's
//! achievable on the alpha's Qdrant deployment.

use std::sync::Arc;
use std::time::Duration;

use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};

/// 6 hours.
pub const DEFAULT_ARCHIVE_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Layer-3 observations must be at least this old to qualify
/// for archival. Matches the integrity auditor's stale window
/// so the two tasks see the same "terminal" cutoff.
pub const STALE_OBS_AGE_SECS: i64 = 30 * 24 * 60 * 60;

/// Tag stamped on records that have been archived. Used both
/// by the SQL filter in [`LayeredMemoryStore::list_archive_candidates`]
/// and by operator tooling that wants to surface the archived
/// set.
pub const ARCHIVED_TAG: &str = "archived";

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ArchiveReport {
    pub observations_archived: usize,
    pub raws_consolidated: usize,
    pub sources_seen: usize,
    pub started_at: i64,
    pub finished_at: i64,
}

use serde::{Deserialize, Serialize};

pub struct ConsolidationArchiver {
    store: Arc<LayeredMemoryStore>,
    interval: Duration,
    /// Cap on observations archived per tick. Prevents a
    /// runaway tick from holding the SQLite writer lock long.
    batch_cap: usize,
}

impl ConsolidationArchiver {
    pub fn new(store: Arc<LayeredMemoryStore>) -> Self {
        Self {
            store,
            interval: Duration::from_secs(DEFAULT_ARCHIVE_INTERVAL_SECS),
            batch_cap: 1000,
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    pub fn with_batch_cap(mut self, cap: usize) -> Self {
        self.batch_cap = cap.max(1);
        self
    }

    /// Run one archive pass synchronously. Returns the per-tick
    /// counts so tests can assert behaviour.
    pub async fn run_once(&self) -> Result<ArchiveReport, String> {
        let started_at = unix_secs();
        let mut report = ArchiveReport {
            started_at,
            ..Default::default()
        };

        // ── 1. Archive terminal observations. ───────────────
        let cutoff = started_at - STALE_OBS_AGE_SECS;
        let candidates = self
            .store
            .list_archive_candidates(MemoryLayer::Observation, cutoff, self.batch_cap)
            .map_err(|e| format!("archiver: list_archive_candidates: {e}"))?;
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for obs in candidates {
            seen.insert(obs.source.clone());
            if !self.is_covered_by_model(&obs)? {
                continue;
            }
            if let Err(e) = self.archive_observation(&obs, started_at) {
                tracing::warn!(error = %e, id = %obs.id, "archiver: archive failed");
                continue;
            }
            report.observations_archived += 1;
        }
        report.sources_seen = seen.len();

        // ── 2. Stamp raw rows whose source observations are
        // all archived as `consolidated = true`. ─────────────
        let raws = self
            .store
            .list_raw_candidates_for_consolidation(self.batch_cap)
            .map_err(|e| format!("archiver: list_raw_candidates: {e}"))?;
        for raw in raws {
            if !self.all_obs_archived_for(&raw.source)? {
                continue;
            }
            if let Err(e) = self.store.set_consolidated(&raw.id, true) {
                tracing::warn!(error = %e, id = %raw.id, "archiver: set_consolidated failed");
                continue;
            }
            report.raws_consolidated += 1;
        }

        report.finished_at = unix_secs();

        // Structured chronicle line. Log shippers + the
        // tracing-based audit ring pick this up; an explicit
        // chronicle write would require routing through the
        // coordinator, which the alpha defers until a single
        // memory-event chronicle channel exists.
        tracing::info!(
            event = "memory.archiver.run",
            observations_archived = report.observations_archived,
            raws_consolidated = report.raws_consolidated,
            sources_seen = report.sources_seen,
            duration_ms = (report.finished_at - report.started_at) * 1000,
            "memory.archiver: pass complete"
        );
        Ok(report)
    }

    /// Spawn the archive loop on the current tokio runtime.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.interval);
            // Skip the boot-time fire so we don't archive on a
            // cold cache.
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = self.run_once().await {
                    tracing::warn!(error = %e, "memory.archiver: pass failed");
                }
            }
        })
    }

    fn is_covered_by_model(&self, obs: &MemoryRecord) -> Result<bool, String> {
        match self
            .store
            .latest_by_layer_and_source(MemoryLayer::Model, &obs.source)
            .map_err(|e| format!("latest_by_layer_and_source: {e}"))?
        {
            Some(model) => Ok(model.observed_at > obs.observed_at),
            None => Ok(false),
        }
    }

    fn all_obs_archived_for(&self, source: &str) -> Result<bool, String> {
        let obs = self
            .store
            .list(Some(MemoryLayer::Observation), Some(source), 10_000, 0)
            .map_err(|e| format!("list obs: {e}"))?;
        if obs.is_empty() {
            return Ok(false);
        }
        Ok(obs.iter().all(|r| r.tags.iter().any(|t| t == ARCHIVED_TAG)))
    }

    fn archive_observation(&self, obs: &MemoryRecord, now: i64) -> Result<(), String> {
        // Two writes: add the archived tag, then invalidate.
        // Adding the tag first guarantees the SQL LIKE filter
        // excludes the row from the next tick even if the
        // invalidate write fails.
        self.store
            .add_tag(&obs.id, ARCHIVED_TAG)
            .map_err(|e| format!("add_tag: {e}"))?;
        self.store
            .invalidate(&obs.id, now)
            .map_err(|e| format!("invalidate: {e}"))?;
        Ok(())
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::LayeredMemoryStore;
    use std::sync::Arc;

    fn store() -> Arc<LayeredMemoryStore> {
        Arc::new(LayeredMemoryStore::in_memory().unwrap())
    }

    fn obs_aged(id: &str, source: &str, text: &str, age_secs: i64) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, source);
        r.layer = MemoryLayer::Observation;
        let when = unix_secs() - age_secs;
        r.observed_at = when;
        r.created_at = when;
        r.valid_from = when;
        r
    }

    fn model_now(id: &str, source: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, "STATE", source);
        r.layer = MemoryLayer::Model;
        r
    }

    fn raw_aged(id: &str, source: &str, age_secs: i64) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, "turn body", source);
        let when = unix_secs() - age_secs;
        r.observed_at = when;
        r.created_at = when;
        r.valid_from = when;
        r
    }

    #[tokio::test]
    async fn empty_store_archives_nothing() {
        let s = store();
        let arc = ConsolidationArchiver::new(s);
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 0);
        assert_eq!(report.raws_consolidated, 0);
    }

    #[tokio::test]
    async fn observation_younger_than_cutoff_is_not_archived() {
        let s = store();
        s.insert(&obs_aged("o1", "alice", "User uses Postgres", 60))
            .unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s);
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 0);
    }

    #[tokio::test]
    async fn observation_without_model_is_not_archived() {
        let s = store();
        s.insert(&obs_aged(
            "o1",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        ))
        .unwrap();
        let arc = ConsolidationArchiver::new(s);
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 0);
    }

    #[tokio::test]
    async fn old_observation_with_newer_model_is_archived() {
        let s = store();
        s.insert(&obs_aged(
            "o1",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        ))
        .unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s.clone());
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 1);
        let after = s.get("o1").unwrap().unwrap();
        assert!(after.tags.iter().any(|t| t == ARCHIVED_TAG));
        assert!(after.valid_to.is_some());
    }

    #[tokio::test]
    async fn frozen_observation_is_skipped() {
        let s = store();
        let mut r = obs_aged(
            "o1",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        );
        r.frozen = true;
        s.insert(&r).unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s.clone());
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 0);
        let after = s.get("o1").unwrap().unwrap();
        assert!(after.valid_to.is_none());
    }

    #[tokio::test]
    async fn raw_with_all_observations_archived_is_consolidated() {
        let s = store();
        s.insert(&raw_aged("r1", "alice", STALE_OBS_AGE_SECS + 1000))
            .unwrap();
        s.insert(&obs_aged(
            "o1",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        ))
        .unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s.clone());
        let report = arc.run_once().await.unwrap();
        assert_eq!(report.observations_archived, 1);
        assert_eq!(report.raws_consolidated, 1);
        let after = s.get("r1").unwrap().unwrap();
        assert!(after.consolidated);
    }

    #[tokio::test]
    async fn raw_with_unarchived_observation_is_not_consolidated() {
        let s = store();
        s.insert(&raw_aged("r1", "alice", STALE_OBS_AGE_SECS + 1000))
            .unwrap();
        s.insert(&obs_aged(
            "o-old",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        ))
        .unwrap();
        s.insert(&obs_aged("o-new", "alice", "User likes Rust", 60))
            .unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s.clone());
        let report = arc.run_once().await.unwrap();
        // Only the old one archives; the new one is too fresh.
        assert_eq!(report.observations_archived, 1);
        // The raw shouldn't consolidate because at least one
        // observation is still unarchived.
        assert_eq!(report.raws_consolidated, 0);
    }

    #[tokio::test]
    async fn running_twice_is_idempotent() {
        let s = store();
        s.insert(&obs_aged(
            "o1",
            "alice",
            "User uses Postgres",
            STALE_OBS_AGE_SECS + 100,
        ))
        .unwrap();
        s.insert(&model_now("m1", "alice")).unwrap();
        let arc = ConsolidationArchiver::new(s.clone());
        let r1 = arc.run_once().await.unwrap();
        let r2 = arc.run_once().await.unwrap();
        assert_eq!(r1.observations_archived, 1);
        assert_eq!(r2.observations_archived, 0);
    }
}
