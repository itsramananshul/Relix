//! GAP 11 — persistent transactional gateway store.
//!
//! Backed by SQLite. Records every action that flows through
//! the [`super::dispatcher::ToolDispatcher::dispatch_with_options`]
//! surface so the `execution.rollback` capability can reach
//! back across process restarts. Same pragmas/migration
//! pattern as the rest of the runtime stores.
//!
//! Schema:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS gateway_actions (
//!     action_id          TEXT PRIMARY KEY,
//!     transaction_id     TEXT NOT NULL,
//!     tool               TEXT NOT NULL,
//!     args               TEXT NOT NULL,
//!     result             TEXT,
//!     tier_tag           TEXT NOT NULL,
//!     tier_json          TEXT NOT NULL,
//!     idempotency_key    TEXT,
//!     dry_run            INTEGER NOT NULL DEFAULT 0,
//!     success            INTEGER NOT NULL,
//!     error              TEXT,
//!     actor              TEXT,
//!     rolled_back        INTEGER NOT NULL DEFAULT 0,
//!     started_at_ms      INTEGER NOT NULL,
//!     completed_at_ms    INTEGER NOT NULL
//! );
//! CREATE INDEX IF NOT EXISTS gateway_actions_tx
//!     ON gateway_actions(transaction_id, started_at_ms);
//! CREATE UNIQUE INDEX IF NOT EXISTS gateway_actions_idem
//!     ON gateway_actions(tool, idempotency_key)
//!     WHERE idempotency_key IS NOT NULL;
//! ```

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::gateway_tier::{GatewayDispatchOptions, GatewayTier};

#[derive(Debug, thiserror::Error)]
pub enum TransactionStoreError {
    #[error("transaction store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("transaction store: json: {0}")]
    Json(String),
    #[error("transaction store: action `{0}` not found")]
    NotFound(String),
}

impl From<serde_json::Error> for TransactionStoreError {
    fn from(e: serde_json::Error) -> Self {
        TransactionStoreError::Json(e.to_string())
    }
}

/// One row in `gateway_actions`. Read via
/// [`TransactionStore::list_for_transaction`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayActionRow {
    pub action_id: String,
    pub transaction_id: String,
    pub tool: String,
    pub args: String,
    pub result: Option<String>,
    pub tier: GatewayTier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub dry_run: bool,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub rolled_back: bool,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
}

/// Cheap-to-clone SQLite-backed transactional store.
#[derive(Clone)]
pub struct TransactionStore {
    conn: Arc<Mutex<Connection>>,
}

impl TransactionStore {
    pub fn open(path: &Path) -> Result<Self, TransactionStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "execution_gateway");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, TransactionStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), TransactionStoreError> {
        crate::db::ensure_migration_table(conn)?;
        let current = crate::db::current_migration_version(conn)?;
        if current < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS gateway_actions (\
                     action_id        TEXT PRIMARY KEY,\
                     transaction_id   TEXT NOT NULL,\
                     tool             TEXT NOT NULL,\
                     args             TEXT NOT NULL,\
                     result           TEXT,\
                     tier_tag         TEXT NOT NULL,\
                     tier_json        TEXT NOT NULL,\
                     idempotency_key  TEXT,\
                     dry_run          INTEGER NOT NULL DEFAULT 0,\
                     success          INTEGER NOT NULL,\
                     error            TEXT,\
                     actor            TEXT,\
                     rolled_back      INTEGER NOT NULL DEFAULT 0,\
                     started_at_ms    INTEGER NOT NULL,\
                     completed_at_ms  INTEGER NOT NULL\
                 );\
                 CREATE INDEX IF NOT EXISTS gateway_actions_tx \
                     ON gateway_actions(transaction_id, started_at_ms);\
                 CREATE UNIQUE INDEX IF NOT EXISTS gateway_actions_idem \
                     ON gateway_actions(tool, idempotency_key) \
                     WHERE idempotency_key IS NOT NULL;",
            )?;
            crate::db::record_migration_applied(conn, 1)?;
        }
        Ok(())
    }

    /// Look up an earlier action with the same (tool, idempotency_key).
    /// Returns `None` when the key is unset or no prior call
    /// has used it. Used by the dispatcher to short-circuit
    /// retries.
    pub fn find_by_idempotency_key(
        &self,
        tool: &str,
        idempotency_key: &str,
    ) -> Result<Option<GatewayActionRow>, TransactionStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'transaction store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let row = conn
            .query_row(
                "SELECT action_id, transaction_id, tool, args, result, tier_tag, tier_json, \
                        idempotency_key, dry_run, success, error, actor, rolled_back, \
                        started_at_ms, completed_at_ms \
                 FROM gateway_actions \
                 WHERE tool = ?1 AND idempotency_key = ?2 \
                 ORDER BY completed_at_ms DESC LIMIT 1",
                params![tool, idempotency_key],
                row_to_action,
            )
            .optional()?;
        match row {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Persist a completed action row. Idempotent against the
    /// (tool, idempotency_key) unique index — duplicate inserts
    /// hit the index and we surface them as the same row to
    /// the caller via [`Self::find_by_idempotency_key`].
    pub fn record(&self, row: &GatewayActionRow) -> Result<(), TransactionStoreError> {
        let tier_json = serde_json::to_string(&row.tier)?;
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'transaction store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        conn.execute(
            "INSERT INTO gateway_actions \
             (action_id, transaction_id, tool, args, result, tier_tag, tier_json, \
              idempotency_key, dry_run, success, error, actor, rolled_back, \
              started_at_ms, completed_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                row.action_id,
                row.transaction_id,
                row.tool,
                row.args,
                row.result,
                row.tier.tag(),
                tier_json,
                row.idempotency_key,
                row.dry_run as i32,
                row.success as i32,
                row.error,
                row.actor,
                row.rolled_back as i32,
                row.started_at_ms,
                row.completed_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Fetch every action in a transaction, oldest-first.
    pub fn list_for_transaction(
        &self,
        transaction_id: &str,
    ) -> Result<Vec<GatewayActionRow>, TransactionStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'transaction store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let mut stmt = conn.prepare(
            "SELECT action_id, transaction_id, tool, args, result, tier_tag, tier_json, \
                    idempotency_key, dry_run, success, error, actor, rolled_back, \
                    started_at_ms, completed_at_ms \
             FROM gateway_actions WHERE transaction_id = ?1 \
             ORDER BY started_at_ms ASC, action_id ASC",
        )?;
        let rows = stmt.query_map(params![transaction_id], row_to_action)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Flip the `rolled_back` flag on one action. Used by the
    /// rollback handler after a Tier A compensating call
    /// succeeds.
    pub fn mark_rolled_back(&self, action_id: &str) -> Result<(), TransactionStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'transaction store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let n = conn.execute(
            "UPDATE gateway_actions SET rolled_back = 1 WHERE action_id = ?1",
            params![action_id],
        )?;
        if n == 0 {
            return Err(TransactionStoreError::NotFound(action_id.to_string()));
        }
        Ok(())
    }

    /// Direct lookup by action_id. Used by the evidence store
    /// to attach evidence rows back to the action.
    pub fn get(&self, action_id: &str) -> Result<Option<GatewayActionRow>, TransactionStoreError> {
        let conn = self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'transaction store lock poisoned'; recovering inner state");
            e.into_inner()
        });
        let row = conn
            .query_row(
                "SELECT action_id, transaction_id, tool, args, result, tier_tag, tier_json, \
                        idempotency_key, dry_run, success, error, actor, rolled_back, \
                        started_at_ms, completed_at_ms \
                 FROM gateway_actions WHERE action_id = ?1",
                params![action_id],
                row_to_action,
            )
            .optional()?;
        match row {
            Some(Ok(r)) => Ok(Some(r)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

/// Stable id minter — `g.<blake3-of-(tool,args,now,rand)[..16]>`.
/// Operators copy-paste these into the `execution.rollback`
/// CLI; we keep them human-readable.
pub fn mint_action_id(tool: &str, args: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.as_bytes());
    hasher.update(b"|");
    hasher.update(args.as_bytes());
    hasher.update(b"|");
    hasher.update(unix_millis().to_le_bytes().as_ref());
    let mut rnd = [0u8; 8];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut rnd);
    hasher.update(&rnd);
    format!("g.{}", hex::encode(&hasher.finalize().as_bytes()[..16]))
}

/// Stable transaction id used when callers don't supply one.
/// Format: `tx.<blake3[..16]>`. Different from action ids so
/// they don't collide in operator logs.
pub fn mint_transaction_id() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(unix_millis().to_le_bytes().as_ref());
    let mut rnd = [0u8; 16];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut rnd);
    hasher.update(&rnd);
    format!("tx.{}", hex::encode(&hasher.finalize().as_bytes()[..16]))
}

/// Build a `GatewayActionRow` from a successful dispatch.
pub fn build_success_row(
    tool: &str,
    args: &str,
    result: Option<String>,
    options: &GatewayDispatchOptions,
    started_at_ms: i64,
    completed_at_ms: i64,
) -> GatewayActionRow {
    let tier = options
        .tier
        .clone()
        .unwrap_or(GatewayTier::HumanRollbackPlan {
            rollback_plan: String::new(),
        });
    GatewayActionRow {
        action_id: mint_action_id(tool, args),
        transaction_id: options
            .transaction_id
            .clone()
            .unwrap_or_else(mint_transaction_id),
        tool: tool.to_string(),
        args: args.to_string(),
        result,
        tier,
        idempotency_key: options.idempotency_key.clone(),
        dry_run: options.dry_run,
        success: true,
        error: None,
        actor: options.actor.clone(),
        rolled_back: false,
        started_at_ms,
        completed_at_ms,
    }
}

/// Same as [`build_success_row`] but for failed dispatches.
pub fn build_failure_row(
    tool: &str,
    args: &str,
    error: String,
    options: &GatewayDispatchOptions,
    started_at_ms: i64,
    completed_at_ms: i64,
) -> GatewayActionRow {
    let tier = options
        .tier
        .clone()
        .unwrap_or(GatewayTier::HumanRollbackPlan {
            rollback_plan: String::new(),
        });
    GatewayActionRow {
        action_id: mint_action_id(tool, args),
        transaction_id: options
            .transaction_id
            .clone()
            .unwrap_or_else(mint_transaction_id),
        tool: tool.to_string(),
        args: args.to_string(),
        result: None,
        tier,
        idempotency_key: options.idempotency_key.clone(),
        dry_run: options.dry_run,
        success: false,
        error: Some(error),
        actor: options.actor.clone(),
        rolled_back: false,
        started_at_ms,
        completed_at_ms,
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_action(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<GatewayActionRow, TransactionStoreError>> {
    let action_id: String = r.get(0)?;
    let transaction_id: String = r.get(1)?;
    let tool: String = r.get(2)?;
    let args: String = r.get(3)?;
    let result: Option<String> = r.get(4)?;
    let _tier_tag: String = r.get(5)?;
    let tier_json: String = r.get(6)?;
    let idempotency_key: Option<String> = r.get(7)?;
    let dry_run: i64 = r.get(8)?;
    let success: i64 = r.get(9)?;
    let error: Option<String> = r.get(10)?;
    let actor: Option<String> = r.get(11)?;
    let rolled_back: i64 = r.get(12)?;
    let started_at_ms: i64 = r.get(13)?;
    let completed_at_ms: i64 = r.get(14)?;
    let parse = || -> Result<GatewayActionRow, TransactionStoreError> {
        let tier: GatewayTier = serde_json::from_str(&tier_json)?;
        Ok(GatewayActionRow {
            action_id,
            transaction_id,
            tool,
            args,
            result,
            tier,
            idempotency_key,
            dry_run: dry_run != 0,
            success: success != 0,
            error,
            actor,
            rolled_back: rolled_back != 0,
            started_at_ms,
            completed_at_ms,
        })
    };
    Ok(parse())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(action_id: &str, tx: &str, tool: &str, key: Option<&str>) -> GatewayActionRow {
        GatewayActionRow {
            action_id: action_id.to_string(),
            transaction_id: tx.to_string(),
            tool: tool.to_string(),
            args: "{}".into(),
            result: Some("ok".into()),
            tier: GatewayTier::HumanRollbackPlan {
                rollback_plan: "rollback hint".into(),
            },
            idempotency_key: key.map(str::to_string),
            dry_run: false,
            success: true,
            error: None,
            actor: Some("alice".into()),
            rolled_back: false,
            started_at_ms: 1,
            completed_at_ms: 2,
        }
    }

    #[test]
    fn record_then_list_round_trips() {
        let s = TransactionStore::open_in_memory().unwrap();
        s.record(&sample("a1", "tx1", "tool.x", None)).unwrap();
        s.record(&sample("a2", "tx1", "tool.y", None)).unwrap();
        s.record(&sample("a3", "tx2", "tool.z", None)).unwrap();
        let rows = s.list_for_transaction("tx1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action_id, "a1");
        assert_eq!(rows[1].action_id, "a2");
    }

    #[test]
    fn idempotency_key_lookup_returns_the_recorded_row() {
        let s = TransactionStore::open_in_memory().unwrap();
        s.record(&sample("a1", "tx1", "tool.x", Some("idem-1")))
            .unwrap();
        let got = s.find_by_idempotency_key("tool.x", "idem-1").unwrap();
        assert!(got.is_some());
        let got = got.unwrap();
        assert_eq!(got.action_id, "a1");
        // Different tool with the same key → no match.
        let other = s.find_by_idempotency_key("tool.y", "idem-1").unwrap();
        assert!(other.is_none());
    }

    #[test]
    fn duplicate_idempotency_insert_fails_via_unique_index() {
        let s = TransactionStore::open_in_memory().unwrap();
        s.record(&sample("a1", "tx1", "tool.x", Some("k"))).unwrap();
        // Second insert with same (tool, idempotency_key)
        // hits the unique index.
        let err = s
            .record(&sample("a2", "tx1", "tool.x", Some("k")))
            .unwrap_err();
        assert!(matches!(err, TransactionStoreError::Sqlite(_)));
    }

    #[test]
    fn mark_rolled_back_flips_the_flag() {
        let s = TransactionStore::open_in_memory().unwrap();
        s.record(&sample("a1", "tx1", "tool.x", None)).unwrap();
        s.mark_rolled_back("a1").unwrap();
        let row = s.get("a1").unwrap().unwrap();
        assert!(row.rolled_back);
    }

    #[test]
    fn mark_rolled_back_on_missing_id_errors() {
        let s = TransactionStore::open_in_memory().unwrap();
        let err = s.mark_rolled_back("ghost").unwrap_err();
        assert!(matches!(err, TransactionStoreError::NotFound(_)));
    }

    #[test]
    fn tier_a_round_trips_through_storage() {
        let s = TransactionStore::open_in_memory().unwrap();
        let mut row = sample("a1", "tx1", "memory.write", None);
        row.tier = GatewayTier::AutoCompensated {
            compensating_tool: "memory.delete".into(),
            compensating_args: json!({"id": "abc"}),
        };
        s.record(&row).unwrap();
        let got = s.get("a1").unwrap().unwrap();
        match got.tier {
            GatewayTier::AutoCompensated {
                compensating_tool,
                compensating_args,
            } => {
                assert_eq!(compensating_tool, "memory.delete");
                assert_eq!(compensating_args, json!({"id": "abc"}));
            }
            other => panic!("expected Tier A, got {other:?}"),
        }
    }

    #[test]
    fn build_success_row_carries_options() {
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id("tx-A")
            .with_idempotency_key("k-A")
            .auto_compensated("memory.delete", json!({"id": "x"}))
            .with_actor("alice");
        let row = build_success_row(
            "memory.write",
            r#"{"text":"hi"}"#,
            Some("ok".into()),
            &opts,
            10,
            20,
        );
        assert_eq!(row.transaction_id, "tx-A");
        assert_eq!(row.idempotency_key.as_deref(), Some("k-A"));
        assert_eq!(row.actor.as_deref(), Some("alice"));
        assert!(row.success);
        assert_eq!(row.tier.tag(), "auto_compensated");
    }

    #[test]
    fn build_failure_row_records_error_text() {
        let opts = GatewayDispatchOptions::legacy(false, Some("hint".into()));
        let row = build_failure_row("tool.x", "{}", "boom".into(), &opts, 5, 6);
        assert!(!row.success);
        assert_eq!(row.error.as_deref(), Some("boom"));
        assert_eq!(row.tier.tag(), "human_rollback");
    }

    #[test]
    fn mint_action_id_is_unique_and_prefixed() {
        let a = mint_action_id("tool.x", "args");
        let b = mint_action_id("tool.x", "args");
        assert!(a.starts_with("g."));
        assert!(b.starts_with("g."));
        assert_ne!(a, b);
    }

    #[test]
    fn mint_transaction_id_is_unique_and_prefixed() {
        let a = mint_transaction_id();
        let b = mint_transaction_id();
        assert!(a.starts_with("tx."));
        assert!(b.starts_with("tx."));
        assert_ne!(a, b);
    }
}
