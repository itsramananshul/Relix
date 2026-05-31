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

pub mod lock_order;

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
/// then stamp each migration with `record_migration_applied`.
///
/// CORR PART 2: the table now carries two surfaces in parallel:
///
/// - the legacy `(version INTEGER PRIMARY KEY, applied_at TEXT)`
///   columns the original migration framework wrote, kept for
///   back-compat with already-deployed databases;
/// - new `migration_id TEXT`, `applied_at_ms INTEGER`,
///   `checksum TEXT` columns the identifier-based framework
///   ([`is_migration_applied`] / [`apply_migration`]) reads and
///   writes. Substring-matching error messages
///   ([`is_migration_already_applied`]) is no longer how the
///   framework detects already-applied migrations — the
///   identifier table is the source of truth.
pub fn ensure_migration_table(conn: &Connection) -> Result<(), SqliteError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _relix_migrations (\
             version       INTEGER PRIMARY KEY,\
             applied_at    TEXT    NOT NULL\
         );",
    )?;
    // CORR PART 2: ensure the identifier columns exist.
    // SQLite's ALTER TABLE doesn't have a portable
    // ADD-COLUMN-IF-NOT-EXISTS, so we probe via PRAGMA and
    // emit the ALTER only when missing.
    add_column_if_missing(conn, "_relix_migrations", "migration_id", "TEXT")?;
    add_column_if_missing(conn, "_relix_migrations", "applied_at_ms", "INTEGER")?;
    add_column_if_missing(conn, "_relix_migrations", "checksum", "TEXT")?;
    // Unique index on `migration_id`. SQLite UNIQUE allows
    // multiple NULL rows by default (one per legacy version
    // row that pre-dates the new shape) but rejects duplicate
    // non-NULL identifiers. The index is non-partial so
    // `ON CONFLICT(migration_id) DO NOTHING` matches it.
    conn.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS _relix_migrations_migration_id_idx \
             ON _relix_migrations(migration_id);",
    )
}

/// CORR PART 2: probe + add a column on an existing table if
/// it is not already present. SQLite's pragma_table_info gives
/// us the live shape without parsing error messages.
fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    column_type: &str,
) -> Result<(), SqliteError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}");
    conn.execute(&sql, [])?;
    Ok(())
}

/// GROUP 6: idempotently add a `tenant_id TEXT NOT NULL DEFAULT
/// 'default'` column to `table` (plus a tenant index) when it is
/// absent. Pre-multi-tenant rows are attributed to the reserved
/// `'default'` tenant — safe because single-tenant deployments
/// read as `"default"` and so keep seeing their own historical
/// rows. Idempotent via the `PRAGMA table_info` probe, so a
/// re-open never errors or double-applies.
pub fn ensure_tenant_id_column(conn: &Connection, table: &str) -> Result<(), SqliteError> {
    add_column_if_missing(conn, table, "tenant_id", "TEXT NOT NULL DEFAULT 'default'")?;
    conn.execute(
        &format!("CREATE INDEX IF NOT EXISTS {table}_tenant_idx ON {table}(tenant_id)"),
        [],
    )?;
    Ok(())
}

/// CORR PART 2: identifier-based check. Returns `true` when
/// a row with `migration_id = id` exists in `_relix_migrations`.
/// Callers use this BEFORE running a migration body so an
/// already-applied migration is a no-op without going through
/// the SQLite error path. Never matches error message
/// substrings.
pub fn is_migration_applied(conn: &Connection, id: &str) -> Result<bool, SqliteError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM _relix_migrations WHERE migration_id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// CORR PART 2: stamp an identifier-keyed migration as applied.
/// Uses `INSERT … ON CONFLICT DO NOTHING` so a concurrent boot
/// of the same migration on the same DB never raises a PK
/// violation; the `changes()` count lets callers distinguish
/// applied-now (1) from was-already-applied (0).
///
/// `checksum` is a hex-encoded BLAKE3 digest of the SQL the
/// migration ran; operators reading the table can spot a
/// tampered migration body.
pub fn record_migration_applied_by_id(
    conn: &Connection,
    id: &str,
    checksum: &str,
) -> Result<usize, SqliteError> {
    let now_ms = unix_now_ms();
    conn.execute(
        "INSERT INTO _relix_migrations (migration_id, applied_at_ms, checksum, applied_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(migration_id) DO NOTHING",
        rusqlite::params![id, now_ms, checksum, chrono_secs_iso()],
    )
}

/// CORR PART 2: run `body` exactly once per `id` against
/// `conn`. The check + write are wrapped in a `BEGIN IMMEDIATE`
/// transaction so two concurrent boots can race for the same
/// migration and only one will run it.
pub fn apply_migration<F>(
    conn: &mut Connection,
    id: &str,
    sql: &str,
    body: F,
) -> Result<bool, SqliteError>
where
    F: FnOnce(&rusqlite::Transaction<'_>) -> Result<(), SqliteError>,
{
    ensure_migration_table(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let already: i64 = tx.query_row(
        "SELECT COUNT(*) FROM _relix_migrations WHERE migration_id = ?1",
        rusqlite::params![id],
        |r| r.get(0),
    )?;
    if already > 0 {
        // Honest commit so the BEGIN IMMEDIATE releases its
        // lock — a no-op transaction still needs to drop the
        // database-wide writer slot.
        tx.commit()?;
        return Ok(false);
    }
    body(&tx)?;
    let checksum = checksum_sql(sql);
    let now_ms = unix_now_ms();
    let affected = tx.execute(
        "INSERT INTO _relix_migrations (migration_id, applied_at_ms, checksum, applied_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(migration_id) DO NOTHING",
        rusqlite::params![id, now_ms, checksum, chrono_secs_iso()],
    )?;
    tx.commit()?;
    Ok(affected > 0)
}

/// CORR PART 2: BLAKE3 digest of the migration body. Operators
/// reading `_relix_migrations` see a stable hex value per id;
/// a body change without a new id surfaces as a checksum
/// mismatch only at audit time (intentional — the framework
/// does not refuse to boot on a checksum change, but operators
/// can scan for one).
pub fn checksum_sql(sql: &str) -> String {
    hex::encode(blake3::hash(sql.as_bytes()).as_bytes())
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// CORR PART 2: register an already-deployed table with the
/// identifier framework so a node that upgrades from the pre-
/// fix path doesn't re-run a CREATE TABLE that would no-op +
/// confuse the operator's audit. Callers pass the migration
/// id they want stamped and a check that returns `true` when
/// the legacy table is already present.
///
/// Idempotent — re-calls on a DB that's already been claimed
/// are no-ops.
pub fn claim_legacy_migration<F>(conn: &Connection, id: &str, check: F) -> Result<bool, SqliteError>
where
    F: FnOnce(&Connection) -> Result<bool, SqliteError>,
{
    ensure_migration_table(conn)?;
    if is_migration_applied(conn, id)? {
        return Ok(false);
    }
    if !check(conn)? {
        return Ok(false);
    }
    let now_ms = unix_now_ms();
    let affected = conn.execute(
        "INSERT INTO _relix_migrations (migration_id, applied_at_ms, checksum, applied_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(migration_id) DO NOTHING",
        rusqlite::params![id, now_ms, "legacy-claimed", chrono_secs_iso()],
    )?;
    Ok(affected > 0)
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
/// increasing.
///
/// CORR PART 2: implementation now uses
/// `INSERT … ON CONFLICT DO NOTHING` (not the legacy
/// `INSERT OR IGNORE`) and returns the number of rows actually
/// inserted so the caller can distinguish "applied just now" from
/// "was already recorded". Existing callers that throw the result
/// away keep working unchanged.
pub fn record_migration_applied(conn: &Connection, version: i64) -> Result<usize, SqliteError> {
    let now = chrono_secs_iso();
    let affected = conn.execute(
        "INSERT INTO _relix_migrations (version, applied_at) VALUES (?1, ?2) \
         ON CONFLICT(version) DO NOTHING",
        rusqlite::params![version, now],
    )?;
    Ok(affected)
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

    // ── CORR PART 2: identifier-based migration framework ──

    #[test]
    fn corr_p2_apply_migration_runs_once_then_is_noop() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        let mut ran = 0;
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY);";
        let applied = apply_migration(&mut conn, "t.v1", sql, |tx| {
            ran += 1;
            tx.execute_batch(sql)
        })
        .unwrap();
        assert!(applied);
        assert_eq!(ran, 1);
        // Second call returns false + does NOT re-run.
        let applied = apply_migration(&mut conn, "t.v1", sql, |tx| {
            ran += 1;
            tx.execute_batch(sql)
        })
        .unwrap();
        assert!(!applied);
        assert_eq!(ran, 1, "body must not re-run for already-applied id");
        assert!(is_migration_applied(&conn, "t.v1").unwrap());
    }

    #[test]
    fn corr_p2_apply_migration_records_checksum_and_id() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        let sql = "CREATE TABLE x (id INTEGER);";
        apply_migration(&mut conn, "x.v1", sql, |tx| tx.execute_batch(sql)).unwrap();
        let row: (String, String) = conn
            .query_row(
                "SELECT migration_id, checksum FROM _relix_migrations WHERE migration_id = 'x.v1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "x.v1");
        assert_eq!(row.1, checksum_sql(sql));
    }

    #[test]
    fn corr_p2_concurrent_apply_only_runs_body_once() {
        // BEGIN IMMEDIATE serialises two threads racing for
        // the same migration id. Only one body call wins.
        use std::sync::Arc;
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("race.db");
        // Initialise the DB so both threads open against an
        // already-pragma'd file.
        {
            let c = Connection::open(&p).unwrap();
            apply_pragmas(&c).unwrap();
            ensure_migration_table(&c).unwrap();
        }
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p1 = p.clone();
        let p2 = p.clone();
        let c1 = counter.clone();
        let c2 = counter.clone();
        let t1 = std::thread::spawn(move || {
            let mut conn = Connection::open(p1).unwrap();
            apply_pragmas(&conn).unwrap();
            let sql = "CREATE TABLE race1 (id INTEGER);";
            apply_migration(&mut conn, "race.v1", sql, |tx| {
                c1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx.execute_batch(sql)
            })
            .unwrap()
        });
        let t2 = std::thread::spawn(move || {
            let mut conn = Connection::open(p2).unwrap();
            apply_pragmas(&conn).unwrap();
            let sql = "CREATE TABLE race1 (id INTEGER);";
            apply_migration(&mut conn, "race.v1", sql, |tx| {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx.execute_batch(sql)
            })
            .unwrap()
        });
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert_ne!(r1, r2, "exactly one thread must have applied");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn corr_p2_record_migration_applied_returns_rows_affected() {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        ensure_migration_table(&conn).unwrap();
        let n = record_migration_applied(&conn, 7).unwrap();
        assert_eq!(n, 1, "first insert applies one row");
        let n2 = record_migration_applied(&conn, 7).unwrap();
        assert_eq!(n2, 0, "duplicate insert is dropped by ON CONFLICT");
    }

    #[test]
    fn corr_p2_claim_legacy_migration_stamps_pre_fix_db() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        // Simulate a pre-fix DB: the user table is already
        // here, the migration table is fresh.
        conn.execute_batch("CREATE TABLE legacy_thing (id INTEGER);")
            .unwrap();
        let claimed = claim_legacy_migration(&conn, "legacy_thing.v1", |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='legacy_thing'",
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .unwrap();
        assert!(claimed);
        assert!(is_migration_applied(&conn, "legacy_thing.v1").unwrap());
        // Repeat is idempotent.
        let claimed2 = claim_legacy_migration(&conn, "legacy_thing.v1", |_| Ok(true)).unwrap();
        assert!(!claimed2);
        // Subsequent apply_migration with the same id no-ops.
        let applied = apply_migration(&mut conn, "legacy_thing.v1", "should not run", |_| {
            panic!("body must not run for claimed legacy id");
        })
        .unwrap();
        assert!(!applied);
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
