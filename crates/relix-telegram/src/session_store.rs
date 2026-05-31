//! Channel-local mapping from `(chat_id, message_id)` to
//! `task_id`. Lets the async delivery path find the right
//! Telegram chat to reply to when a long-running flow finally
//! completes — without keeping the inbound handler blocked.
//!
//! Two implementations:
//!
//! - [`InMemorySessionStore`] — BTreeMap behind RwLock. Fast,
//!   forgetful. Loses all in-flight mappings on channel
//!   restart; the Coordinator's Task survives but the channel
//!   can no longer route the reply. Good for dev / tests.
//! - [`SqliteSessionStore`] — bundled SQLite, idempotent
//!   schema on open. Restart-safe: a channel that crashes
//!   mid-flow can re-open the same DB on restart and resume
//!   delivery. Recommended for production.
//!
//! Both implement the [`SessionStorage`] trait so the channel
//! controller can be parameterised by which backing store the
//! operator wants.
//!
//! Legacy alias: `SessionStore` is `InMemorySessionStore` for
//! existing callers and tests; production deployments wire
//! `SqliteSessionStore` explicitly.
//!
//! FIX 6 — TTL sweep. Every session row carries a
//! `last_seen_ms` stamp that is bumped on every `record` and
//! `lookup`. The [`spawn_session_sweeper`] task wakes up every
//! `sweep_interval` and deletes rows whose `last_seen_ms` is
//! older than `now_ms - ttl_hours * 3600_000`. Both the stamp
//! and the cutoff come from a [`relix_core::clock::Clock`] so
//! tests can drive the sweep deterministically without sleeping.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use relix_core::clock::{Clock, SystemClock};
use rusqlite::{Connection, params};

/// FIX 6: default time-to-live for an idle Telegram session.
/// Operators override via `[telegram] session_ttl_hours`. 24h
/// matches Telegram's own conversation-staleness heuristic so
/// the dashboard's session count doesn't grow unbounded across
/// long-running channels.
pub const DEFAULT_SESSION_TTL_HOURS: u32 = 24;

/// FIX 6: default sweep interval. One hour is a good balance
/// — sweep often enough that the session count tracks reality
/// for operators, rare enough that the sweep itself contributes
/// negligible IO. Operators override via the `spawn_session_sweeper`
/// argument.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Operator-controlled choice of where Telegram session
/// mappings live across restarts. The channel controller is
/// parameterised by this trait so the in-memory impl can drive
/// tests and the SQLite impl can drive production.
pub trait SessionStorage: Send + Sync {
    /// Record the mapping. Called by the inbound handler right
    /// after `task.create` succeeds. Idempotent: overwriting
    /// the same key is allowed (last write wins) so a
    /// reprocessed update from Telegram doesn't error.
    /// FIX 6: bumps `last_seen_ms` on every call so an active
    /// session is never swept.
    fn record(&self, chat_id: i64, message_id: i64, task_id: String);

    /// Look up the task_id for a `(chat_id, message_id)`.
    /// Returns `None` when the mapping isn't present. FIX 6:
    /// bumps `last_seen_ms` on hit so a session that's being
    /// polled stays warm — the sweeper only reaps truly idle
    /// mappings.
    fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String>;

    /// Drop the mapping after the reply is delivered.
    /// Returns the removed task_id when one was present.
    fn forget(&self, chat_id: i64, message_id: i64) -> Option<String>;

    /// Operator + test inspection — count of in-flight
    /// mappings.
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// FIX 6: delete every row whose `last_seen_ms` is strictly
    /// less than `cutoff_ms`. Returns the number of rows
    /// removed so the sweeper can log a single INFO line per
    /// non-empty sweep. Default impl returns 0 so backends that
    /// don't track per-row activity (the in-memory store) keep
    /// compiling without behaviour change.
    fn sweep_older_than(&self, _cutoff_ms: i64) -> usize {
        0
    }
}

/// Legacy alias used by existing callers + integration tests.
/// New code should pick `InMemorySessionStore` or
/// `SqliteSessionStore` explicitly.
pub type SessionStore = InMemorySessionStore;

/// In-memory store. Forgetful across process restarts.
#[derive(Default)]
pub struct InMemorySessionStore {
    inner: RwLock<BTreeMap<(i64, i64), String>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStorage for InMemorySessionStore {
    fn record(&self, chat_id: i64, message_id: i64, task_id: String) {
        let mut g = self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        g.insert((chat_id, message_id), task_id);
    }

    fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let g = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        g.get(&(chat_id, message_id)).cloned()
    }

    fn forget(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let mut g = self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        g.remove(&(chat_id, message_id))
    }

    fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                tracing::warn!("telegram session_store lock poisoned; recovering inner state");
                e.into_inner()
            })
            .len()
    }
}

/// SQLite-backed store. Restart-safe: re-open the same path on
/// channel startup and in-flight mappings resume. Schema is
/// idempotent on open; one row per `(chat_id, message_id)`
/// with a UNIQUE constraint so reprocessed updates merge
/// rather than duplicate.
///
/// FIX 6: every row carries a `last_seen_ms` column stamped by
/// the injected [`Clock`]. The TTL sweep deletes rows whose
/// stamp is older than the operator-configured cut-off.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

impl SqliteSessionStore {
    /// Open or create the SQLite DB at `path`. Creates the
    /// parent directory if needed. Uses
    /// [`relix_core::clock::SystemClock`] for the `last_seen_ms`
    /// stamp; production callers use this entry point.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        Self::open_with_clock(path, Arc::new(SystemClock))
    }

    /// FIX 6: open variant that takes an explicit clock so the
    /// TTL sweeper test can drive `last_seen_ms` via
    /// `FakeClock::set`.
    pub fn open_with_clock(path: &Path, clock: Arc<dyn Clock>) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// In-memory backend for unit tests. Same schema, same
    /// behaviour modulo persistence across process restarts.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        Self::in_memory_with_clock(Arc::new(SystemClock))
    }

    /// FIX 6: in-memory variant with an injected clock, used
    /// by the sweep tests.
    pub fn in_memory_with_clock(clock: Arc<dyn Clock>) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// Production pragmas: FK enforcement on, WAL journal,
    /// synchronous=NORMAL, 5s busy timeout. Mirrors
    /// `relix_runtime::db::apply_pragmas`. Inlined here because
    /// `relix-telegram` does not depend on `relix-runtime`.
    fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(())
    }

    fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _relix_migrations (
                 version    INTEGER PRIMARY KEY,
                 applied_at TEXT    NOT NULL
             );
             CREATE TABLE IF NOT EXISTS telegram_sessions (
                 chat_id     INTEGER NOT NULL,
                 message_id  INTEGER NOT NULL,
                 task_id     TEXT    NOT NULL,
                 recorded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                 last_seen_ms INTEGER NOT NULL DEFAULT 0,
                 UNIQUE (chat_id, message_id)
             );",
        )?;
        // FIX 6: backfill the `last_seen_ms` column for any
        // pre-FIX-6 database. PRAGMA-driven existence check
        // keeps the ALTER idempotent — rusqlite re-raises with
        // a clear error on duplicate column on freshly-created
        // tables that already have the column. We swallow the
        // duplicate-column error explicitly so the first call
        // after an in-place upgrade succeeds and every
        // subsequent call is a no-op.
        if !column_exists(conn, "telegram_sessions", "last_seen_ms")? {
            conn.execute(
                "ALTER TABLE telegram_sessions
                 ADD COLUMN last_seen_ms INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        Ok(())
    }
}

/// Returns `true` if `column` is present on `table` in the
/// connection's current schema.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

impl SessionStorage for SqliteSessionStore {
    fn record(&self, chat_id: i64, message_id: i64, task_id: String) {
        let now_ms = self.clock.now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        // ON CONFLICT REPLACE so reprocessed Telegram updates
        // (rare but possible during restart races) merge.
        // FIX 6: bump `last_seen_ms` on every write so an active
        // session is never swept.
        let _ = conn.execute(
            "INSERT INTO telegram_sessions (chat_id, message_id, task_id, last_seen_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (chat_id, message_id) DO UPDATE SET
                 task_id = excluded.task_id,
                 recorded_at = strftime('%s', 'now'),
                 last_seen_ms = excluded.last_seen_ms",
            params![chat_id, message_id, task_id, now_ms],
        );
    }

    fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let now_ms = self.clock.now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        let task_id: Option<String> = conn
            .query_row(
                "SELECT task_id FROM telegram_sessions
                 WHERE chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id],
                |r| r.get::<_, String>(0),
            )
            .ok();
        // FIX 6: bump `last_seen_ms` on hit so a session that
        // is being polled stays warm. Best-effort — the lookup
        // itself succeeded so we don't fail the caller on a
        // bump-write blip.
        if task_id.is_some() {
            let _ = conn.execute(
                "UPDATE telegram_sessions
                 SET last_seen_ms = ?3
                 WHERE chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id, now_ms],
            );
        }
        task_id
    }

    fn forget(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        // Read-then-delete in one transaction so concurrent
        // lookups don't see a half-deleted row.
        let tx = conn.unchecked_transaction().ok()?;
        let task_id: Option<String> = tx
            .query_row(
                "SELECT task_id FROM telegram_sessions
                 WHERE chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id],
                |r| r.get(0),
            )
            .ok();
        if task_id.is_some() {
            let _ = tx.execute(
                "DELETE FROM telegram_sessions
                 WHERE chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id],
            );
        }
        let _ = tx.commit();
        task_id
    }

    fn len(&self) -> usize {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        conn.query_row("SELECT COUNT(*) FROM telegram_sessions", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }

    fn sweep_older_than(&self, cutoff_ms: i64) -> usize {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("telegram session_store lock poisoned; recovering inner state");
            e.into_inner()
        });
        conn.execute(
            "DELETE FROM telegram_sessions WHERE last_seen_ms < ?1",
            params![cutoff_ms],
        )
        .unwrap_or(0)
    }
}

/// FIX 6: spawn the background TTL sweep. Wakes every
/// `sweep_interval`, computes the cutoff from the injected
/// clock, and deletes every session whose `last_seen_ms` is
/// strictly less. Logs one INFO line per non-empty sweep so
/// operators see the high-water mark in their controller logs;
/// quiet on empty sweeps so a steady-state idle channel
/// doesn't spam.
///
/// The returned `JoinHandle` lets the controller cancel the
/// sweep on shutdown if it wants to; production callers
/// typically `drop` it and let the tokio runtime stop it at
/// process exit.
pub fn spawn_session_sweeper<S>(
    store: Arc<S>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    sweep_interval: Duration,
) -> tokio::task::JoinHandle<()>
where
    S: SessionStorage + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(sweep_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ttl_ms = ttl.as_millis().min(i64::MAX as u128) as i64;
        loop {
            interval.tick().await;
            let now_ms = clock.now_ms();
            let cutoff = now_ms.saturating_sub(ttl_ms);
            let removed = store.sweep_older_than(cutoff);
            if removed > 0 {
                tracing::info!(
                    removed,
                    cutoff_ms = cutoff,
                    "telegram session_store: TTL sweep removed {removed} stale session(s)"
                );
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::clock::FakeClock;

    /// Test driver: exercise the trait contract against any
    /// impl. Both stores must behave identically against the
    /// same operations (modulo persistence across restarts,
    /// which has its own dedicated test below).
    fn exercise_storage(s: &dyn SessionStorage) {
        assert!(s.is_empty());
        assert!(s.lookup(1, 2).is_none());
        s.record(1, 2, "abc".into());
        assert_eq!(s.lookup(1, 2).as_deref(), Some("abc"));
        assert_eq!(s.len(), 1);
        // Overwrite: last write wins.
        s.record(1, 2, "abc-v2".into());
        assert_eq!(s.lookup(1, 2).as_deref(), Some("abc-v2"));
        assert_eq!(s.len(), 1);
        // Distinct keys coexist.
        s.record(1, 3, "b".into());
        s.record(2, 2, "c".into());
        assert_eq!(s.len(), 3);
        // Forget returns the prior value.
        let removed = s.forget(1, 2);
        assert_eq!(removed.as_deref(), Some("abc-v2"));
        assert_eq!(s.len(), 2);
        // Forgetting a missing key is None, not error.
        assert!(s.forget(99, 99).is_none());
    }

    #[test]
    fn in_memory_storage_satisfies_trait_contract() {
        let s = InMemorySessionStore::new();
        exercise_storage(&s);
    }

    #[test]
    fn sqlite_storage_satisfies_trait_contract() {
        let s = SqliteSessionStore::in_memory().unwrap();
        exercise_storage(&s);
    }

    #[test]
    fn sqlite_storage_persists_across_reopen() {
        // Restart-safe: write to a file, drop the store, open
        // a fresh handle to the same path, observe the
        // recorded mapping.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("sessions.db");
        {
            let s = SqliteSessionStore::open(&path).unwrap();
            s.record(100, 5, "abc-restart-safe".into());
            assert_eq!(s.lookup(100, 5).as_deref(), Some("abc-restart-safe"));
            // store drops here.
        }
        let reopened = SqliteSessionStore::open(&path).unwrap();
        assert_eq!(
            reopened.lookup(100, 5).as_deref(),
            Some("abc-restart-safe"),
            "mapping must survive process restart"
        );
        assert_eq!(reopened.len(), 1);
    }

    #[test]
    fn sqlite_storage_open_creates_parent_dir() {
        // Operators who point at `dev-data/telegram/sessions.db`
        // shouldn't have to `mkdir` first.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("nested").join("deeper").join("sessions.db");
        let s = SqliteSessionStore::open(&path).unwrap();
        s.record(1, 1, "x".into());
        assert_eq!(s.lookup(1, 1).as_deref(), Some("x"));
        assert!(path.exists());
    }

    #[test]
    fn fix6_sweep_removes_rows_older_than_cutoff() {
        // FIX 6: a row recorded at t=1_000ms with a sweep
        // cutoff of 2_000ms MUST be deleted; a row recorded at
        // t=3_000ms with the same cutoff MUST survive.
        let clock = Arc::new(FakeClock::new(1_000));
        let store = SqliteSessionStore::in_memory_with_clock(clock.clone()).unwrap();
        store.record(1, 1, "old".into());
        clock.set(3_000);
        store.record(2, 2, "fresh".into());
        let removed = store.sweep_older_than(2_000);
        assert_eq!(removed, 1, "sweep removes exactly the one stale row");
        assert!(store.lookup(1, 1).is_none(), "stale row gone");
        assert_eq!(
            store.lookup(2, 2).as_deref(),
            Some("fresh"),
            "fresh row survives"
        );
    }

    #[test]
    fn fix6_lookup_bumps_last_seen_so_active_sessions_survive_sweep() {
        // FIX 6: a session that's still being polled bumps
        // `last_seen_ms` on every lookup so the sweeper never
        // reaps it.
        let clock = Arc::new(FakeClock::new(1_000));
        let store = SqliteSessionStore::in_memory_with_clock(clock.clone()).unwrap();
        store.record(1, 1, "active".into());
        // Time passes; before sweeping, the consumer polls.
        clock.set(5_000);
        assert_eq!(store.lookup(1, 1).as_deref(), Some("active"));
        // Now sweep with a cutoff that would have killed the
        // original t=1_000 stamp.
        let removed = store.sweep_older_than(4_000);
        assert_eq!(removed, 0, "lookup must have bumped last_seen past cutoff");
        assert_eq!(
            store.lookup(1, 1).as_deref(),
            Some("active"),
            "active session survives sweep"
        );
    }

    #[test]
    fn fix6_record_bumps_last_seen_after_initial_insert() {
        // FIX 6: a session that gets re-recorded (Telegram
        // restart races, or a reprocessed update) bumps
        // `last_seen_ms` so the sweeper observes the most
        // recent activity, not the first.
        let clock = Arc::new(FakeClock::new(1_000));
        let store = SqliteSessionStore::in_memory_with_clock(clock.clone()).unwrap();
        store.record(1, 1, "v1".into());
        clock.set(10_000);
        store.record(1, 1, "v2".into());
        let removed = store.sweep_older_than(5_000);
        assert_eq!(removed, 0, "re-record bumped last_seen past cutoff");
        assert_eq!(store.lookup(1, 1).as_deref(), Some("v2"));
    }

    #[test]
    fn fix6_in_memory_store_sweep_is_noop_default() {
        // FIX 6: the default trait impl for `sweep_older_than`
        // returns 0 so backends that don't track per-row
        // activity (the in-memory store) keep compiling.
        let s = InMemorySessionStore::new();
        s.record(1, 1, "x".into());
        assert_eq!(s.sweep_older_than(i64::MAX), 0);
        // The row survives because the default impl is a
        // documented no-op, not an actual delete.
        assert_eq!(s.lookup(1, 1).as_deref(), Some("x"));
    }

    /// FIX 6: the background sweeper sweeps exactly when the
    /// interval ticks past — driven deterministically by
    /// `tokio::time::pause` + `tokio::time::advance` +
    /// `FakeClock::set` so no real time elapses in the test.
    #[tokio::test(start_paused = true)]
    async fn fix6_spawn_session_sweeper_deletes_stale_rows_on_tick() {
        let clock = Arc::new(FakeClock::new(1_000));
        let store = Arc::new(
            SqliteSessionStore::in_memory_with_clock(clock.clone()).expect("in_memory_with_clock"),
        );
        // Seed a stale row at t=1_000ms.
        store.record(1, 1, "stale".into());
        // Move the wall clock forward AND advance the tokio
        // runtime so the interval fires. The TTL is 1s (= 1000
        // ms) and we advance past it; the sweep tick has to
        // observe the FakeClock at the new time.
        let _handle = spawn_session_sweeper(
            store.clone(),
            clock.clone(),
            Duration::from_millis(1_000),
            Duration::from_millis(500),
        );
        // Let the spawned task register its first interval
        // tick. The first `interval.tick()` fires immediately
        // on the first poll; advance + yield_now lets it run.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_millis(0)).await;
            tokio::task::yield_now().await;
        }
        // Bump wall clock past TTL and advance another sweep
        // interval to trigger the second tick. The second tick
        // is the one that observes the stale stamp.
        clock.set(10_000);
        tokio::time::advance(Duration::from_millis(500)).await;
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_millis(0)).await;
            tokio::task::yield_now().await;
        }
        assert_eq!(store.len(), 0, "sweeper must have deleted the stale row");
    }
}
