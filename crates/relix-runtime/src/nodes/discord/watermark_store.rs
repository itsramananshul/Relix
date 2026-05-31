//! Discord FIX 2 — persistent polling-cursor watermark.
//!
//! The in-memory cursor on [`super::state::ChannelState`] is
//! reset on every process restart, which means a bot reboot
//! re-polls the entire `get_messages` history and re-processes
//! every message Discord returns. This store persists the
//! `(channel_id, last_message_id)` pair so a restart resumes
//! from the last-seen snowflake.
//!
//! Discord snowflake message ids are lexicographically ordered
//! (the underlying integer is encoded as a base-10 string with
//! variable width but the controller's `advance_cursor` already
//! parses to u64 for monotonic comparison — see
//! [`super::controller::advance_cursor`]). We store the string
//! verbatim so the controller's existing comparison logic does
//! not need to change.

use std::path::Path;
use std::sync::Mutex;

use relix_core::clock::Clock;
use rusqlite::{Connection, OptionalExtension, params};

/// SQLite-backed Discord watermark store. Restart-safe.
pub struct DiscordWatermarkStore {
    conn: Mutex<Connection>,
}

impl DiscordWatermarkStore {
    /// Open or create the SQLite DB at `path`. Creates the
    /// parent directory if needed.
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory backend for unit tests.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::apply_pragmas(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn apply_pragmas(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Ok(())
    }

    fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
        // FIX 2: exact schema from the spec.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS discord_watermarks (
                 channel_id TEXT PRIMARY KEY,
                 last_message_id TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );",
        )
    }

    /// Read the persisted watermark for `channel_id`. Returns
    /// `None` when the channel has never been recorded — the
    /// controller then falls through to its existing
    /// "empty cursor = bootstrap from current tail" branch.
    pub fn get(&self, channel_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        conn.query_row(
            "SELECT last_message_id FROM discord_watermarks WHERE channel_id = ?1",
            params![channel_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// Persist the most-recently-seen message id for
    /// `channel_id`. `INSERT … ON CONFLICT DO UPDATE` so every
    /// poll cycle writes idempotently; `updated_at_ms` comes
    /// from the injected clock so test runs are deterministic.
    pub fn record(&self, channel_id: &str, last_message_id: &str, clock: &dyn Clock) {
        let now_ms = clock.now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        let _ = conn.execute(
            "INSERT INTO discord_watermarks
                 (channel_id, last_message_id, updated_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(channel_id) DO UPDATE SET
                 last_message_id = excluded.last_message_id,
                 updated_at_ms = excluded.updated_at_ms",
            params![channel_id, last_message_id, now_ms],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::clock::{FakeClock, SystemClock};

    #[test]
    fn fix2_get_returns_none_on_empty_store() {
        let s = DiscordWatermarkStore::in_memory().unwrap();
        assert!(s.get("C0").is_none());
    }

    #[test]
    fn fix2_record_then_get_round_trips() {
        let clock = FakeClock::new(1_700_000_000_000);
        let s = DiscordWatermarkStore::in_memory().unwrap();
        s.record("C0", "1234567890123456789", &clock);
        assert_eq!(
            s.get("C0").as_deref(),
            Some("1234567890123456789"),
            "stored watermark must round-trip"
        );
    }

    #[test]
    fn fix2_record_overwrites_on_conflict() {
        let clock = FakeClock::new(1_700_000_000_000);
        let s = DiscordWatermarkStore::in_memory().unwrap();
        s.record("C0", "old-id", &clock);
        clock.set(1_700_000_001_000);
        s.record("C0", "new-id", &clock);
        assert_eq!(s.get("C0").as_deref(), Some("new-id"));
    }

    #[test]
    fn fix2_per_channel_isolation() {
        let clock = SystemClock;
        let s = DiscordWatermarkStore::in_memory().unwrap();
        s.record("C-A", "alpha", &clock);
        s.record("C-B", "bravo", &clock);
        assert_eq!(s.get("C-A").as_deref(), Some("alpha"));
        assert_eq!(s.get("C-B").as_deref(), Some("bravo"));
    }

    #[test]
    fn fix2_persists_across_reopen() {
        // FIX 2's most important guarantee. A bridge restart
        // must observe the original watermark or it would re-
        // process every message Discord returns next poll.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("discord-watermarks.db");
        let clock = SystemClock;
        {
            let s = DiscordWatermarkStore::open(&path).unwrap();
            s.record("C0", "1500000000000000000", &clock);
        }
        let reopened = DiscordWatermarkStore::open(&path).unwrap();
        assert_eq!(
            reopened.get("C0").as_deref(),
            Some("1500000000000000000"),
            "watermark must survive bridge restart"
        );
    }

    #[test]
    fn fix2_updated_at_ms_uses_injected_clock() {
        let clock = FakeClock::new(99_000_000);
        let s = DiscordWatermarkStore::in_memory().unwrap();
        s.record("C0", "1234567890", &clock);
        let conn = s.conn.lock().unwrap();
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at_ms FROM discord_watermarks WHERE channel_id = ?1",
                params!["C0"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(updated_at, 99_000_000);
    }
}
