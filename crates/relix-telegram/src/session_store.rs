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

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, RwLock};

use rusqlite::{Connection, params};

/// Operator-controlled choice of where Telegram session
/// mappings live across restarts. The channel controller is
/// parameterised by this trait so the in-memory impl can drive
/// tests and the SQLite impl can drive production.
pub trait SessionStorage: Send + Sync {
    /// Record the mapping. Called by the inbound handler right
    /// after `task.create` succeeds. Idempotent: overwriting
    /// the same key is allowed (last write wins) so a
    /// reprocessed update from Telegram doesn't error.
    fn record(&self, chat_id: i64, message_id: i64, task_id: String);

    /// Look up the task_id for a `(chat_id, message_id)`.
    /// Returns `None` when the mapping isn't present.
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
        let mut g = self.inner.write().expect("poisoned");
        g.insert((chat_id, message_id), task_id);
    }

    fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let g = self.inner.read().expect("poisoned");
        g.get(&(chat_id, message_id)).cloned()
    }

    fn forget(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let mut g = self.inner.write().expect("poisoned");
        g.remove(&(chat_id, message_id))
    }

    fn len(&self) -> usize {
        self.inner.read().expect("poisoned").len()
    }
}

/// SQLite-backed store. Restart-safe: re-open the same path on
/// channel startup and in-flight mappings resume. Schema is
/// idempotent on open; one row per `(chat_id, message_id)`
/// with a UNIQUE constraint so reprocessed updates merge
/// rather than duplicate.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
}

impl SqliteSessionStore {
    /// Open or create the SQLite DB at `path`. Creates the
    /// parent directory if needed.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory backend for unit tests. Same schema, same
    /// behaviour modulo persistence across process restarts.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS telegram_sessions (
                 chat_id     INTEGER NOT NULL,
                 message_id  INTEGER NOT NULL,
                 task_id     TEXT    NOT NULL,
                 recorded_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
                 UNIQUE (chat_id, message_id)
             );",
        )
    }
}

impl SessionStorage for SqliteSessionStore {
    fn record(&self, chat_id: i64, message_id: i64, task_id: String) {
        let conn = self.conn.lock().expect("poisoned");
        // ON CONFLICT REPLACE so reprocessed Telegram updates
        // (rare but possible during restart races) merge.
        let _ = conn.execute(
            "INSERT INTO telegram_sessions (chat_id, message_id, task_id)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (chat_id, message_id) DO UPDATE SET
                 task_id = excluded.task_id,
                 recorded_at = strftime('%s', 'now')",
            params![chat_id, message_id, task_id],
        );
    }

    fn lookup(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let conn = self.conn.lock().expect("poisoned");
        conn.query_row(
            "SELECT task_id FROM telegram_sessions
             WHERE chat_id = ?1 AND message_id = ?2",
            params![chat_id, message_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
    }

    fn forget(&self, chat_id: i64, message_id: i64) -> Option<String> {
        let conn = self.conn.lock().expect("poisoned");
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
        let conn = self.conn.lock().expect("poisoned");
        conn.query_row("SELECT COUNT(*) FROM telegram_sessions", [], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n as usize)
        .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
