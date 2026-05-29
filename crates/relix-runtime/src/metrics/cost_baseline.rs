//! GAP 22 Feature 2 follow-up — persistent rolling baselines for
//! per-provider cost rates and per-agent ask-human rates.
//!
//! The §7.11 metrics store already records every invocation; the
//! existing `eval_provider_cost_spike` + `eval_ask_human_drift`
//! evaluators in [`super::alert`] compare the most recent (1h)
//! window against the longer (24h) window each tick. What was
//! missing was a durable record of the baseline windows themselves:
//!
//! - the operator can't graph how the baseline drifted over the
//!   last week without re-running the aggregator every time;
//! - a spike alert's "what was the baseline at the time?" question
//!   has no on-disk answer once the rolling 24h window slides past
//!   the moment of the spike.
//!
//! This module ships a SQLite-backed store that the
//! [`super::spike_detector::CostSpikeDetector`] writes to every
//! `baseline_window_mins`. Each tick captures:
//!
//! - one [`CostBaselineWindow`] row per active model;
//! - one [`AskHumanRateWindow`] row per active agent;
//! - zero or more [`CostSpikeRecord`] rows when the detector's
//!   threshold is crossed.
//!
//! Rows older than `retention_days` (default 7d) are purged on the
//! next tick so the table doesn't grow without bound.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum BaselineStoreError {
    #[error("baseline store io: {0}")]
    Io(String),
    #[error("baseline store sql: {0}")]
    Sql(String),
}

impl From<rusqlite::Error> for BaselineStoreError {
    fn from(e: rusqlite::Error) -> Self {
        BaselineStoreError::Sql(e.to_string())
    }
}

/// One rolling window of per-model cost, persisted to disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBaselineWindow {
    pub id: String,
    pub provider: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub total_cost_micros: u64,
    pub invocation_count: u64,
    pub avg_cost_micros_per_call: u64,
    pub p95_cost_micros: u64,
    pub created_at_ms: i64,
}

/// One rolling window of per-agent ask-human rate, persisted to disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AskHumanRateWindow {
    pub id: String,
    pub agent: String,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub total_invocations: u64,
    pub ask_human_count: u64,
    pub ask_human_rate: f64,
    pub created_at_ms: i64,
}

/// One fired spike alert, archived for the operator to read back
/// after the rolling baseline has slid past the spike instant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostSpikeRecord {
    pub id: String,
    pub provider: String,
    pub current_avg_micros: u64,
    pub baseline_avg_micros: u64,
    pub spike_ratio: f64,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub created_at_ms: i64,
}

/// SQLite-backed baseline + spike-history store. Cheap to clone
/// (Arc<Mutex<Connection>> inside).
#[derive(Clone)]
pub struct CostBaselineStore {
    conn: Arc<Mutex<Connection>>,
    path: Option<PathBuf>,
}

impl CostBaselineStore {
    pub fn open(path: &Path) -> Result<Self, BaselineStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| BaselineStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        let s = Self {
            conn: Arc::new(Mutex::new(conn)),
            path: Some(path.to_path_buf()),
        };
        s.init()?;
        Ok(s)
    }

    pub fn in_memory() -> Result<Self, BaselineStoreError> {
        let conn = Connection::open_in_memory()?;
        let s = Self {
            conn: Arc::new(Mutex::new(conn)),
            path: None,
        };
        s.init()?;
        Ok(s)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn init(&self) -> Result<(), BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS cost_baselines (
                 id TEXT PRIMARY KEY,
                 provider TEXT NOT NULL,
                 window_start_ms INTEGER NOT NULL,
                 window_end_ms INTEGER NOT NULL,
                 total_cost_micros INTEGER NOT NULL,
                 invocation_count INTEGER NOT NULL,
                 avg_cost_micros_per_call INTEGER NOT NULL,
                 p95_cost_micros INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cost_baselines_provider_created
                 ON cost_baselines (provider, created_at_ms DESC);
             CREATE INDEX IF NOT EXISTS cost_baselines_created
                 ON cost_baselines (created_at_ms DESC);

             CREATE TABLE IF NOT EXISTS ask_human_rate_baselines (
                 id TEXT PRIMARY KEY,
                 agent TEXT NOT NULL,
                 window_start_ms INTEGER NOT NULL,
                 window_end_ms INTEGER NOT NULL,
                 total_invocations INTEGER NOT NULL,
                 ask_human_count INTEGER NOT NULL,
                 ask_human_rate REAL NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS ask_human_baselines_agent_created
                 ON ask_human_rate_baselines (agent, created_at_ms DESC);

             CREATE TABLE IF NOT EXISTS cost_spike_history (
                 id TEXT PRIMARY KEY,
                 provider TEXT NOT NULL,
                 current_avg_micros INTEGER NOT NULL,
                 baseline_avg_micros INTEGER NOT NULL,
                 spike_ratio REAL NOT NULL,
                 window_start_ms INTEGER NOT NULL,
                 window_end_ms INTEGER NOT NULL,
                 created_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cost_spike_history_created
                 ON cost_spike_history (created_at_ms DESC);
             ",
        )?;
        Ok(())
    }

    pub fn insert_cost_baseline(&self, w: &CostBaselineWindow) -> Result<(), BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO cost_baselines
                 (id, provider, window_start_ms, window_end_ms,
                  total_cost_micros, invocation_count,
                  avg_cost_micros_per_call, p95_cost_micros, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                w.id,
                w.provider,
                w.window_start_ms,
                w.window_end_ms,
                w.total_cost_micros as i64,
                w.invocation_count as i64,
                w.avg_cost_micros_per_call as i64,
                w.p95_cost_micros as i64,
                w.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn insert_ask_human_baseline(
        &self,
        w: &AskHumanRateWindow,
    ) -> Result<(), BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO ask_human_rate_baselines
                 (id, agent, window_start_ms, window_end_ms,
                  total_invocations, ask_human_count, ask_human_rate, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                w.id,
                w.agent,
                w.window_start_ms,
                w.window_end_ms,
                w.total_invocations as i64,
                w.ask_human_count as i64,
                w.ask_human_rate,
                w.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn insert_spike(&self, r: &CostSpikeRecord) -> Result<(), BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO cost_spike_history
                 (id, provider, current_avg_micros, baseline_avg_micros,
                  spike_ratio, window_start_ms, window_end_ms, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                r.id,
                r.provider,
                r.current_avg_micros as i64,
                r.baseline_avg_micros as i64,
                r.spike_ratio,
                r.window_start_ms,
                r.window_end_ms,
                r.created_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn recent_cost_baselines(
        &self,
        provider: Option<&str>,
        limit: u32,
    ) -> Result<Vec<CostBaselineWindow>, BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 1_000) as i64;
        let rows = if let Some(p) = provider {
            let mut stmt = conn.prepare(
                "SELECT id, provider, window_start_ms, window_end_ms,
                        total_cost_micros, invocation_count,
                        avg_cost_micros_per_call, p95_cost_micros, created_at_ms
                 FROM cost_baselines
                 WHERE provider = ?1
                 ORDER BY created_at_ms DESC
                 LIMIT ?2",
            )?;
            let it = stmt.query_map(params![p, limit], row_to_cost_baseline)?;
            collect(it)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, provider, window_start_ms, window_end_ms,
                        total_cost_micros, invocation_count,
                        avg_cost_micros_per_call, p95_cost_micros, created_at_ms
                 FROM cost_baselines
                 ORDER BY created_at_ms DESC
                 LIMIT ?1",
            )?;
            let it = stmt.query_map(params![limit], row_to_cost_baseline)?;
            collect(it)?
        };
        Ok(rows)
    }

    pub fn recent_ask_human_baselines(
        &self,
        agent: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AskHumanRateWindow>, BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 1_000) as i64;
        let rows = if let Some(a) = agent {
            let mut stmt = conn.prepare(
                "SELECT id, agent, window_start_ms, window_end_ms,
                        total_invocations, ask_human_count, ask_human_rate, created_at_ms
                 FROM ask_human_rate_baselines
                 WHERE agent = ?1
                 ORDER BY created_at_ms DESC
                 LIMIT ?2",
            )?;
            let it = stmt.query_map(params![a, limit], row_to_ask_human)?;
            collect(it)?
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, agent, window_start_ms, window_end_ms,
                        total_invocations, ask_human_count, ask_human_rate, created_at_ms
                 FROM ask_human_rate_baselines
                 ORDER BY created_at_ms DESC
                 LIMIT ?1",
            )?;
            let it = stmt.query_map(params![limit], row_to_ask_human)?;
            collect(it)?
        };
        Ok(rows)
    }

    pub fn recent_spike_history(
        &self,
        limit: u32,
    ) -> Result<Vec<CostSpikeRecord>, BaselineStoreError> {
        let conn = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 1_000) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, provider, current_avg_micros, baseline_avg_micros,
                    spike_ratio, window_start_ms, window_end_ms, created_at_ms
             FROM cost_spike_history
             ORDER BY created_at_ms DESC
             LIMIT ?1",
        )?;
        let it = stmt.query_map(params![limit], row_to_spike)?;
        collect(it)
    }

    /// Average per-call cost across the last `hours` of persisted
    /// baselines for one provider. Returns `None` when no baselines
    /// exist in the window — the caller treats absent baselines as
    /// "not enough data to fire a spike alert."
    pub fn baseline_avg_micros(
        &self,
        provider: &str,
        hours: u32,
    ) -> Result<Option<u64>, BaselineStoreError> {
        let cutoff = super::collector::now_ms() - (hours as i64) * 3_600_000;
        let conn = self.conn.lock().unwrap();
        let n: Option<f64> = conn
            .query_row(
                "SELECT AVG(avg_cost_micros_per_call) FROM cost_baselines
                 WHERE provider = ?1 AND created_at_ms >= ?2",
                params![provider, cutoff],
                |r| r.get::<_, Option<f64>>(0),
            )
            .unwrap_or(None);
        Ok(n.map(|x| x.max(0.0) as u64))
    }

    /// Average ask-human-rate across the last `hours` of persisted
    /// baselines for one agent.
    pub fn baseline_ask_human_rate(
        &self,
        agent: &str,
        hours: u32,
    ) -> Result<Option<f64>, BaselineStoreError> {
        let cutoff = super::collector::now_ms() - (hours as i64) * 3_600_000;
        let conn = self.conn.lock().unwrap();
        let n: Option<f64> = conn
            .query_row(
                "SELECT AVG(ask_human_rate) FROM ask_human_rate_baselines
                 WHERE agent = ?1 AND created_at_ms >= ?2",
                params![agent, cutoff],
                |r| r.get::<_, Option<f64>>(0),
            )
            .unwrap_or(None);
        Ok(n)
    }

    /// Delete rows whose `created_at_ms` is older than
    /// `retention_days * 86_400_000` ms. Returns the total number
    /// of rows pruned across all three tables.
    pub fn purge_older_than(&self, retention_days: u32) -> Result<u64, BaselineStoreError> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = super::collector::now_ms() - (retention_days as i64) * 86_400_000;
        let conn = self.conn.lock().unwrap();
        let mut total = 0u64;
        for table in [
            "cost_baselines",
            "ask_human_rate_baselines",
            "cost_spike_history",
        ] {
            let n = conn.execute(
                &format!("DELETE FROM {table} WHERE created_at_ms < ?1"),
                params![cutoff],
            )?;
            total += n as u64;
        }
        Ok(total)
    }
}

fn row_to_cost_baseline(r: &rusqlite::Row<'_>) -> rusqlite::Result<CostBaselineWindow> {
    Ok(CostBaselineWindow {
        id: r.get(0)?,
        provider: r.get(1)?,
        window_start_ms: r.get(2)?,
        window_end_ms: r.get(3)?,
        total_cost_micros: r.get::<_, i64>(4)?.max(0) as u64,
        invocation_count: r.get::<_, i64>(5)?.max(0) as u64,
        avg_cost_micros_per_call: r.get::<_, i64>(6)?.max(0) as u64,
        p95_cost_micros: r.get::<_, i64>(7)?.max(0) as u64,
        created_at_ms: r.get(8)?,
    })
}

fn row_to_ask_human(r: &rusqlite::Row<'_>) -> rusqlite::Result<AskHumanRateWindow> {
    Ok(AskHumanRateWindow {
        id: r.get(0)?,
        agent: r.get(1)?,
        window_start_ms: r.get(2)?,
        window_end_ms: r.get(3)?,
        total_invocations: r.get::<_, i64>(4)?.max(0) as u64,
        ask_human_count: r.get::<_, i64>(5)?.max(0) as u64,
        ask_human_rate: r.get(6)?,
        created_at_ms: r.get(7)?,
    })
}

fn row_to_spike(r: &rusqlite::Row<'_>) -> rusqlite::Result<CostSpikeRecord> {
    Ok(CostSpikeRecord {
        id: r.get(0)?,
        provider: r.get(1)?,
        current_avg_micros: r.get::<_, i64>(2)?.max(0) as u64,
        baseline_avg_micros: r.get::<_, i64>(3)?.max(0) as u64,
        spike_ratio: r.get(4)?,
        window_start_ms: r.get(5)?,
        window_end_ms: r.get(6)?,
        created_at_ms: r.get(7)?,
    })
}

fn collect<T>(
    it: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>, BaselineStoreError> {
    let mut v = Vec::new();
    for row in it {
        v.push(row?);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cb(provider: &str, avg: u64, created: i64) -> CostBaselineWindow {
        CostBaselineWindow {
            id: format!("{provider}:{created}"),
            provider: provider.into(),
            window_start_ms: created - 3_600_000,
            window_end_ms: created,
            total_cost_micros: avg * 10,
            invocation_count: 10,
            avg_cost_micros_per_call: avg,
            p95_cost_micros: avg + 100,
            created_at_ms: created,
        }
    }

    fn ah(agent: &str, rate: f64, created: i64) -> AskHumanRateWindow {
        AskHumanRateWindow {
            id: format!("{agent}:{created}"),
            agent: agent.into(),
            window_start_ms: created - 3_600_000,
            window_end_ms: created,
            total_invocations: 100,
            ask_human_count: (rate * 100.0) as u64,
            ask_human_rate: rate,
            created_at_ms: created,
        }
    }

    fn spike(provider: &str, ratio: f64, created: i64) -> CostSpikeRecord {
        CostSpikeRecord {
            id: format!("spike:{provider}:{created}"),
            provider: provider.into(),
            current_avg_micros: 9000,
            baseline_avg_micros: 3000,
            spike_ratio: ratio,
            window_start_ms: created - 3_600_000,
            window_end_ms: created,
            created_at_ms: created,
        }
    }

    #[test]
    fn round_trips_baselines() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        let w = cb("gpt-4o-mini", 250, now);
        s.insert_cost_baseline(&w).unwrap();
        let rows = s.recent_cost_baselines(Some("gpt-4o-mini"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], w);
    }

    #[test]
    fn recent_cost_baselines_orders_newest_first() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        s.insert_cost_baseline(&cb("m", 100, now - 2 * 3_600_000))
            .unwrap();
        s.insert_cost_baseline(&cb("m", 200, now - 3_600_000))
            .unwrap();
        s.insert_cost_baseline(&cb("m", 300, now)).unwrap();
        let rows = s.recent_cost_baselines(Some("m"), 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].avg_cost_micros_per_call, 300);
        assert_eq!(rows[2].avg_cost_micros_per_call, 100);
    }

    #[test]
    fn baseline_avg_micros_computes_window_average() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        // Three rows in the last 24h: 100, 200, 300 → avg 200.
        s.insert_cost_baseline(&cb("m", 100, now - 23 * 3_600_000))
            .unwrap();
        s.insert_cost_baseline(&cb("m", 200, now - 10 * 3_600_000))
            .unwrap();
        s.insert_cost_baseline(&cb("m", 300, now)).unwrap();
        let avg = s.baseline_avg_micros("m", 24).unwrap().unwrap();
        assert_eq!(avg, 200);
    }

    #[test]
    fn baseline_avg_micros_returns_none_when_empty() {
        let s = CostBaselineStore::in_memory().unwrap();
        let avg = s.baseline_avg_micros("never-seen", 24).unwrap();
        assert!(avg.is_none());
    }

    #[test]
    fn baseline_avg_micros_ignores_rows_outside_window() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        // Row older than 24h — should be ignored.
        s.insert_cost_baseline(&cb("m", 1_000_000, now - 48 * 3_600_000))
            .unwrap();
        // Row inside 24h.
        s.insert_cost_baseline(&cb("m", 200, now)).unwrap();
        let avg = s.baseline_avg_micros("m", 24).unwrap().unwrap();
        assert_eq!(avg, 200);
    }

    #[test]
    fn round_trips_ask_human_baselines_and_avg_rate() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        s.insert_ask_human_baseline(&ah("alice", 0.10, now - 2 * 3_600_000))
            .unwrap();
        s.insert_ask_human_baseline(&ah("alice", 0.30, now))
            .unwrap();
        let rows = s.recent_ask_human_baselines(Some("alice"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        let avg = s.baseline_ask_human_rate("alice", 24).unwrap().unwrap();
        assert!((avg - 0.20).abs() < 1e-9);
    }

    #[test]
    fn round_trips_spike_history() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        s.insert_spike(&spike("m", 3.5, now)).unwrap();
        s.insert_spike(&spike("m2", 2.1, now - 1_000)).unwrap();
        let rows = s.recent_spike_history(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].provider, "m");
        assert!((rows[0].spike_ratio - 3.5).abs() < 1e-9);
    }

    #[test]
    fn purge_older_than_drops_old_rows_only() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        // 8d-old row.
        s.insert_cost_baseline(&cb("m", 100, now - 8 * 86_400_000))
            .unwrap();
        // 1d-old row.
        s.insert_cost_baseline(&cb("m", 200, now - 86_400_000))
            .unwrap();
        let dropped = s.purge_older_than(7).unwrap();
        assert_eq!(dropped, 1);
        let rows = s.recent_cost_baselines(Some("m"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].avg_cost_micros_per_call, 200);
    }

    #[test]
    fn purge_with_retention_zero_is_noop() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        s.insert_cost_baseline(&cb("m", 100, now)).unwrap();
        let dropped = s.purge_older_than(0).unwrap();
        assert_eq!(dropped, 0);
    }

    #[test]
    fn recent_cost_baselines_unscoped_returns_every_provider() {
        let s = CostBaselineStore::in_memory().unwrap();
        let now = super::super::collector::now_ms();
        s.insert_cost_baseline(&cb("p1", 100, now)).unwrap();
        s.insert_cost_baseline(&cb("p2", 200, now)).unwrap();
        let rows = s.recent_cost_baselines(None, 10).unwrap();
        assert_eq!(rows.len(), 2);
    }
}
