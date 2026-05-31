//! Slack FIX 4 — historical-message filter persistence.
//!
//! On first boot the Slack controller records the wall-clock
//! moment as the channel's "bot start ts" and persists it in
//! SQLite. On every subsequent poll the controller drops every
//! inbound message whose Slack `ts` is strictly earlier than
//! the recorded start, so the bot never replays history it
//! could not possibly have been around for.
//!
//! Slack `ts` is a string like `"1700000000.000200"`. The
//! integer-second-then-microsecond-fraction layout is
//! lexicographically ordered, so a plain `str < str` comparison
//! is the right filter — no parsing needed.
//!
//! Persistence is keyed off `channel_id` so a bot that watches
//! multiple channels (future) can record one start per channel
//! without leaking history from one into another. Today's
//! single-channel controller writes exactly one row.

use std::path::Path;
use std::sync::Mutex;

use relix_core::clock::Clock;
use rusqlite::{Connection, OptionalExtension, params};

/// SQLite-backed bot-start-ts store. Restart-safe — re-opening
/// the same path on controller startup returns the original
/// stamp so a bot reboot doesn't trigger a fresh history
/// replay.
pub struct SlackBotStartStore {
    conn: Mutex<Connection>,
}

impl SlackBotStartStore {
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
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS slack_bot_start (
                 channel_id TEXT PRIMARY KEY,
                 bot_start_ts TEXT NOT NULL,
                 recorded_at_ms INTEGER NOT NULL
             );",
        )
    }

    /// Read the recorded bot-start timestamp for `channel_id`.
    /// Returns `None` when the channel has never been recorded.
    pub fn get(&self, channel_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        conn.query_row(
            "SELECT bot_start_ts FROM slack_bot_start WHERE channel_id = ?1",
            params![channel_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// Read the recorded ts OR record `now_ts` as the floor and
    /// return it. Idempotent across restarts: a re-open of the
    /// same DB observes the first-ever write.
    pub fn get_or_init(&self, channel_id: &str, now_ts: &str, clock: &dyn Clock) -> String {
        if let Some(existing) = self.get(channel_id) {
            return existing;
        }
        let now_ms = clock.now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        });
        // INSERT OR IGNORE so a race between two `get_or_init`
        // calls on different connections still produces a single
        // canonical first-write value. The follow-up SELECT
        // returns whichever winner landed.
        let _ = conn.execute(
            "INSERT OR IGNORE INTO slack_bot_start
                 (channel_id, bot_start_ts, recorded_at_ms)
             VALUES (?1, ?2, ?3)",
            params![channel_id, now_ts, now_ms],
        );
        conn.query_row(
            "SELECT bot_start_ts FROM slack_bot_start WHERE channel_id = ?1",
            params![channel_id],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| now_ts.to_string())
    }
}

/// Format a unix-seconds `i64` as a Slack-shape `ts` string
/// (`"<secs>.000000"`). Slack `ts` is technically
/// microsecond-resolution; we always stamp `.000000` because we
/// never need sub-second uniqueness at the controller startup
/// moment.
pub fn unix_secs_to_slack_ts(secs: i64) -> String {
    format!("{secs}.000000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::clock::{FakeClock, SystemClock};
    use std::sync::Arc;

    #[test]
    fn fix4_get_returns_none_on_empty_store() {
        let s = SlackBotStartStore::in_memory().unwrap();
        assert!(s.get("C0").is_none());
    }

    #[test]
    fn fix4_get_or_init_persists_first_write() {
        let clock = FakeClock::new(1_000);
        let s = SlackBotStartStore::in_memory().unwrap();
        let ts = s.get_or_init("C0", "1700000000.000000", &clock);
        assert_eq!(ts, "1700000000.000000");
        // Second call returns the SAME ts even with a different
        // `now_ts` candidate.
        let ts2 = s.get_or_init("C0", "1900000000.000000", &clock);
        assert_eq!(ts2, "1700000000.000000", "first-write wins");
    }

    #[test]
    fn fix4_get_or_init_per_channel_isolation() {
        let clock = SystemClock;
        let s = SlackBotStartStore::in_memory().unwrap();
        let a = s.get_or_init("C-A", "1700000000.000000", &clock);
        let b = s.get_or_init("C-B", "1800000000.000000", &clock);
        assert_eq!(a, "1700000000.000000");
        assert_eq!(b, "1800000000.000000");
        // The first-write semantics is per-channel, not global.
    }

    #[test]
    fn fix4_persists_across_reopen() {
        // The single most important guarantee: a bot restart
        // observes the original start ts and does NOT generate a
        // fresh one, so old messages stay filtered.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("slack-bot-start.db");
        let clock = SystemClock;
        {
            let s = SlackBotStartStore::open(&path).unwrap();
            s.get_or_init("C0", "1700000000.000000", &clock);
        }
        let reopened = SlackBotStartStore::open(&path).unwrap();
        assert_eq!(
            reopened.get("C0").as_deref(),
            Some("1700000000.000000"),
            "start ts must survive restart"
        );
        // get_or_init with a NEW candidate must NOT overwrite.
        let ts = reopened.get_or_init("C0", "1900000000.000000", &clock);
        assert_eq!(ts, "1700000000.000000");
    }

    #[test]
    fn fix4_string_ts_lexicographic_order_works_for_slack_format() {
        // Slack `ts` is `"<secs>.<microsecs>"`. Zero-padded
        // microsecond fraction + integer seconds means
        // lexicographic comparison matches numeric comparison
        // FOR the same-width second prefix Slack guarantees in
        // the foreseeable future. This is the assumption the
        // controller's filter relies on; lock it in a test so a
        // future Slack format change is loud.
        let earlier = "1700000000.000000";
        let later = "1700000001.000000";
        assert!(earlier < later, "lex order must match numeric");
        let later_fraction = "1700000000.000200";
        assert!(earlier < later_fraction, "fraction breaks ties");
    }

    #[test]
    fn fix4_recorded_at_ms_uses_injected_clock() {
        let clock = Arc::new(FakeClock::new(42_000_000));
        let s = SlackBotStartStore::in_memory().unwrap();
        s.get_or_init("C0", "1700000000.000000", clock.as_ref());
        let conn = s.conn.lock().unwrap();
        let recorded_ms: i64 = conn
            .query_row(
                "SELECT recorded_at_ms FROM slack_bot_start WHERE channel_id = ?1",
                params!["C0"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded_ms, 42_000_000);
    }

    #[test]
    fn unix_secs_to_slack_ts_pads_microsecond_fraction() {
        assert_eq!(unix_secs_to_slack_ts(1_700_000_000), "1700000000.000000");
    }
}
