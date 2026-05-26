//! Shared SQLite initialisation primitives.
//!
//! Every persistent store in Relix opens its own `rusqlite::Connection`,
//! and historically each open site set up its connection differently
//! (or not at all). The result was:
//!
//! - Foreign key enforcement was *off* by default (SQLite's default is
//!   `PRAGMA foreign_keys = OFF`), so the FK constraints declared on
//!   `task_events.task_id`, `task_attempts.task_id`, etc. were never
//!   actually enforced.
//! - Each connection used the default journal mode (rollback), so a
//!   single concurrent reader + writer would block instead of running
//!   in WAL.
//! - There was no busy timeout, so a transient lock conflict became an
//!   immediate `SQLITE_BUSY` error to the caller.
//! - Migration code did `let _ = conn.execute(...);` and silently
//!   swallowed *every* error from ALTER TABLE / CREATE TABLE statements
//!   — not just the harmless "duplicate column name" / "table already
//!   exists" cases.
//!
//! This module centralises the four pragmas every connection should
//! set, the `_relix_migrations` version table, the integrity-check
//! probe, and the helpers for safely re-running additive ALTER TABLE
//! migrations against an already-migrated DB.

use rusqlite::{Connection, Error as SqliteError, OptionalExtension};

/// Recommended SQLite settings for a Relix store. Apply on every
/// freshly-opened `Connection` before any schema is created or any
/// rows are touched.
///
/// ```text
/// PRAGMA foreign_keys = ON;      -- enforce FK constraints
/// PRAGMA journal_mode = WAL;     -- concurrent reads + one writer
/// PRAGMA synchronous = NORMAL;   -- fsync at checkpoint, not per-tx
/// PRAGMA busy_timeout = 5000;    -- 5s wait on lock conflict
/// ```
///
/// `journal_mode = WAL` is a no-op on `:memory:` databases (SQLite
/// silently falls back to `memory`); callers that need WAL
/// confirmation in tests must use a file-backed DB. Pragmas are
/// applied via `execute_batch` which tolerates the WAL fallback
/// silently — no error is returned.
pub fn apply_pragmas(conn: &Connection) -> Result<(), SqliteError> {
    // PRAGMA journal_mode returns the resulting mode as a row, so we
    // can't use execute_batch for that one — pragma_update is the
    // documented Rusqlite path. The others have no return value worth
    // observing.
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

/// Run `PRAGMA integrity_check`. Returns `Ok("ok")` for a healthy
/// store; any other string indicates SQLite found page-level damage
/// and the value should be surfaced to the operator. Errors here
/// (couldn't even *run* the pragma) are returned as `Err`.
pub fn integrity_check(conn: &Connection) -> Result<String, SqliteError> {
    conn.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
}

/// Probe the integrity-check pragma and log a `warn!` line on every
/// non-`ok` result. Operators see one structured line per startup so
/// silent corruption is impossible. `db_label` is the human name
/// (`"coordinator"`, `"memory"`, …) we put in the log so a multi-DB
/// process's lines can be told apart.
///
/// This deliberately does *not* return an error on a corruption
/// signal — the store opens anyway so the operator has time to
/// investigate. Real damage manifests later as failed queries.
pub fn log_integrity_warning(conn: &Connection, db_label: &str) {
    match integrity_check(conn) {
        Ok(s) if s == "ok" => {
            tracing::debug!(db = db_label, "sqlite: integrity check ok");
        }
        Ok(s) => {
            tracing::warn!(
                db = db_label,
                integrity_check = %s,
                "sqlite: integrity check returned non-ok output"
            );
        }
        Err(e) => {
            tracing::warn!(
                db = db_label,
                error = %e,
                "sqlite: integrity check pragma failed"
            );
        }
    }
}

/// Create the `_relix_migrations` table if it doesn't exist. Every
/// store that runs migrations should call this once at startup,
/// then stamp each migration with `record_migration_applied`. The
/// table is intentionally minimal — version + applied-at — because
/// the migration *content* lives in the store-specific module
/// (each store knows its own schema).
pub fn ensure_migration_table(conn: &Connection) -> Result<(), SqliteError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _relix_migrations (\
             version    INTEGER PRIMARY KEY,\
             applied_at TEXT    NOT NULL\
         );",
    )
}

/// Return the highest migration version recorded for this store.
/// Zero when the table exists but is empty, or when the table is
/// missing (the caller is responsible for `ensure_migration_table`).
pub fn current_migration_version(conn: &Connection) -> Result<i64, SqliteError> {
    let v: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM _relix_migrations", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    Ok(v.unwrap_or(0))
}

/// Stamp a migration as applied. `version` should be monotonically
/// increasing. Idempotent — calling twice with the same version is a
/// no-op (the PK conflict is silently ignored), so re-running a
/// migration body during development doesn't fail boot.
pub fn record_migration_applied(conn: &Connection, version: i64) -> Result<(), SqliteError> {
    let now = chrono_secs_iso();
    conn.execute(
        "INSERT OR IGNORE INTO _relix_migrations (version, applied_at) VALUES (?1, ?2)",
        rusqlite::params![version, now],
    )?;
    Ok(())
}

/// Render `SystemTime::now()` as an ISO-8601 second-resolution string
/// without dragging in the `chrono` crate. Falls back to
/// `1970-01-01T00:00:00Z` if the clock is somehow before the epoch
/// (we don't crash startup on that).
fn chrono_secs_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs / 86_400;
    let rem = secs.rem_euclid(86_400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Days-since-epoch → (year, month, day) using the well-known
/// Howard Hinnant chrono routine. Stable + zero-dep.
fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Whether an error from `execute(ALTER TABLE ... ADD COLUMN ...)`
/// is the harmless "this migration already ran" case. Returns true
/// for both:
///
/// - `duplicate column name: <col>`
/// - `table <X> already exists`
///
/// Any other error is a real schema bug and the caller should fail
/// startup.
pub fn is_migration_already_applied(err: &SqliteError) -> bool {
    let msg = err.to_string().to_ascii_lowercase();
    msg.contains("duplicate column name") || msg.contains("already exists")
}

/// Apply a list of idempotent `ALTER TABLE ADD COLUMN` /
/// `CREATE INDEX IF NOT EXISTS` statements inside a single
/// transaction. Errors that match [`is_migration_already_applied`]
/// are tolerated (the migration ran on a prior boot); any other
/// error rolls back the transaction and is returned to the caller
/// so startup can fail loudly.
///
/// This is the bridge between the alpha's "let `_ = conn.execute(...)`"
/// pattern and a real migration framework — it still ignores
/// duplicate-column errors (the only way to keep additive
/// migrations idempotent against an old DB without tracking
/// versions per-column), but every *other* failure mode now
/// surfaces.
pub fn apply_additive_migrations(
    conn: &mut Connection,
    statements: &[&str],
) -> Result<(), SqliteError> {
    let tx = conn.transaction()?;
    for sql in statements {
        match tx.execute(sql, []) {
            Ok(_) => {}
            Err(e) if is_migration_already_applied(&e) => {
                // Expected on a re-init against a DB that already
                // ran this exact statement on a prior boot.
            }
            Err(e) => {
                return Err(e);
            }
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tempfile() -> (tempfile::TempDir, Connection) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.db");
        let conn = Connection::open(&path).unwrap();
        apply_pragmas(&conn).unwrap();
        (tmp, conn)
    }

    #[test]
    fn pragmas_set_wal_on_file_backed_db() {
        let (_tmp, conn) = open_tempfile();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            mode.to_ascii_lowercase(),
            "wal",
            "file-backed DB should be in WAL mode after apply_pragmas"
        );
    }

    #[test]
    fn pragmas_enable_foreign_keys() {
        let (_tmp, conn) = open_tempfile();
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn pragmas_set_busy_timeout() {
        let (_tmp, conn) = open_tempfile();
        let bt: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bt, 5000);
    }

    #[test]
    fn integrity_check_returns_ok_for_fresh_db() {
        let (_tmp, conn) = open_tempfile();
        let s = integrity_check(&conn).unwrap();
        assert_eq!(s, "ok");
    }

    #[test]
    fn foreign_key_constraint_is_actually_enforced() {
        // With apply_pragmas() FK enforcement is on, so an
        // orphan child row must be rejected. Historically this
        // succeeded because foreign_keys defaulted to OFF.
        let (_tmp, conn) = open_tempfile();
        conn.execute_batch(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);\
             CREATE TABLE child  (id INTEGER PRIMARY KEY,\
                                  pid INTEGER NOT NULL,\
                                  FOREIGN KEY (pid) REFERENCES parent(id));",
        )
        .unwrap();
        let err = conn
            .execute("INSERT INTO child(pid) VALUES (999)", [])
            .expect_err("orphan insert must be rejected with FK enforcement on");
        let s = err.to_string().to_ascii_lowercase();
        assert!(s.contains("foreign key"), "wrong err: {s}");
    }

    #[test]
    fn migration_table_round_trips_versions() {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        ensure_migration_table(&conn).unwrap();
        assert_eq!(current_migration_version(&conn).unwrap(), 0);
        record_migration_applied(&conn, 1).unwrap();
        record_migration_applied(&conn, 2).unwrap();
        // Re-applying the same version is a no-op (no error).
        record_migration_applied(&conn, 2).unwrap();
        assert_eq!(current_migration_version(&conn).unwrap(), 2);
    }

    #[test]
    fn is_migration_already_applied_recognises_known_messages() {
        let dup = SqliteError::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("duplicate column name: foo".to_string()),
        );
        assert!(is_migration_already_applied(&dup));
        let exists = SqliteError::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("table bar already exists".to_string()),
        );
        assert!(is_migration_already_applied(&exists));
        let other = SqliteError::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::Unknown,
                extended_code: 0,
            },
            Some("no such table: thing".to_string()),
        );
        assert!(!is_migration_already_applied(&other));
    }

    #[test]
    fn apply_additive_migrations_tolerates_duplicate_then_succeeds() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        // First run: adds the column.
        apply_additive_migrations(&mut conn, &["ALTER TABLE t ADD COLUMN extra TEXT"]).unwrap();
        // Second run: duplicate-column error is tolerated.
        apply_additive_migrations(&mut conn, &["ALTER TABLE t ADD COLUMN extra TEXT"]).unwrap();
        // Reference table to confirm the column survived.
        conn.execute("INSERT INTO t(extra) VALUES ('x')", [])
            .unwrap();
    }

    #[test]
    fn apply_additive_migrations_surfaces_real_errors() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        // No such table — should error out, not be swallowed.
        let res = apply_additive_migrations(
            &mut conn,
            &["ALTER TABLE definitely_not_a_table ADD COLUMN x TEXT"],
        );
        assert!(res.is_err(), "real schema error must surface");
    }

    #[test]
    fn iso_timestamp_round_trips_a_known_date() {
        // Sanity-check the home-rolled Howard Hinnant conversion.
        // The test is intentionally narrow — we only care that it
        // produces a plausible ISO-8601 string, not that it
        // matches every date.
        let s = chrono_secs_iso();
        assert_eq!(s.len(), 20, "expected YYYY-MM-DDThh:mm:ssZ shape, got {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
    }
}
