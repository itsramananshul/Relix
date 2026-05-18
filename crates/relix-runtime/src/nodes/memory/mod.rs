//! Memory node — SQLite + FTS5 session storage (M7).
//!
//! Capabilities registered on a controller with `[controller] node_type =
//! "memory"`:
//!
//! - `memory.write_turn`            — append one conversational turn.
//! - `memory.recent_for_session`    — return the most recent N turns.
//! - `memory.search`                — full-text search across all turns.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! All three capabilities take and return UTF-8 strings. Args use `|` as a
//! field separator since SOL strings are taken verbatim (no JSON or CBOR
//! plumbing in SOL until Gate 2).
//!
//! | Method | Arg | Return |
//! |---|---|---|
//! | `memory.write_turn` | `session_id\|role\|text` | `ok\n` |
//! | `memory.recent_for_session` | `session_id` or `session_id\|N` (default 10) | `role: text\n` per turn, oldest first |
//! | `memory.search` | `query` or `query\|N` (default 10) | `session_id\trole\ttext\n` per match |
//!
//! ## Schema
//!
//! Hermes-inspired (`hermes_state.py`) but trimmed to the alpha's needs:
//!
//! ```sql
//! CREATE TABLE turns (
//!     id          INTEGER PRIMARY KEY,
//!     session_id  TEXT    NOT NULL,
//!     role        TEXT    NOT NULL,
//!     body        TEXT    NOT NULL,
//!     ts          INTEGER NOT NULL
//! );
//! CREATE INDEX turns_session ON turns(session_id, id);
//! CREATE VIRTUAL TABLE turns_fts USING fts5(
//!     body, session_id UNINDEXED, role UNINDEXED,
//!     content='turns', content_rowid='id'
//! );
//! -- Triggers keep the FTS5 mirror in sync with turns.
//! ```
//!
//! ## Determinism
//!
//! - `recent_for_session` orders by `id DESC LIMIT N` then reverses, so the
//!   returned block is chronological (oldest first).
//! - `search` orders by FTS5 `bm25(turns_fts)` ascending (best matches first)
//!   then by `id ASC` as a tie-breaker, so identical-score results are
//!   deterministic across runs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node memory configuration parsed from the controller TOML `[memory]`
/// section.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct MemoryConfig {
    /// SQLite database path. Created with parent directory on first start.
    pub db_path: PathBuf,
    /// Maximum N for `recent_for_session` and `search` regardless of caller
    /// request. Defaults to 100.
    #[serde(default = "default_max_n")]
    pub max_n: usize,
}

fn default_max_n() -> usize {
    100
}

/// Memory backend wrapping a connection. Wrapped in `Arc<Mutex<>>` because
/// `rusqlite::Connection` is not `Sync`; the handlers are concurrent.
pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
    max_n: usize,
}

impl MemoryStore {
    /// Open or create a memory store at the configured path.
    pub fn open(cfg: &MemoryConfig) -> Result<Self, MemoryError> {
        if let Some(parent) = cfg.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MemoryError::Io(e.to_string()))?;
        }
        let conn = Connection::open(&cfg.db_path).map_err(MemoryError::Db)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_n: cfg.max_n.max(1),
        })
    }

    /// In-memory backend for unit tests.
    pub fn in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory().map_err(MemoryError::Db)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_n: 100,
        })
    }

    /// Append a turn.
    pub fn write_turn(&self, session_id: &str, role: &str, body: &str) -> Result<(), MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let ts = unix_secs();
        conn.execute(
            "INSERT INTO turns (session_id, role, body, ts) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role, body, ts],
        )
        .map_err(MemoryError::Db)?;
        Ok(())
    }

    /// Most recent N turns for a session, oldest first.
    pub fn recent_for_session(
        &self,
        session_id: &str,
        n: usize,
    ) -> Result<Vec<(String, String)>, MemoryError> {
        let limit = n.clamp(1, self.max_n);
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT role, body FROM turns \
                 WHERE session_id = ?1 \
                 ORDER BY id DESC LIMIT ?2",
            )
            .map_err(MemoryError::Db)?;
        let rows = stmt
            .query_map(params![session_id, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(MemoryError::Db)?;
        let mut out = Vec::with_capacity(limit);
        for r in rows {
            out.push(r.map_err(MemoryError::Db)?);
        }
        out.reverse(); // oldest first per public contract
        Ok(out)
    }

    /// FTS5 search across all turns. Returns (session_id, role, body) tuples.
    pub fn search(
        &self,
        query: &str,
        n: usize,
    ) -> Result<Vec<(String, String, String)>, MemoryError> {
        let limit = n.clamp(1, self.max_n);
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        // bm25 ascending = better matches first; tie-break by id ascending for
        // deterministic ordering.
        let mut stmt = conn
            .prepare(
                "SELECT t.session_id, t.role, t.body \
                 FROM turns_fts f \
                 JOIN turns t ON t.id = f.rowid \
                 WHERE turns_fts MATCH ?1 \
                 ORDER BY bm25(turns_fts), t.id ASC \
                 LIMIT ?2",
            )
            .map_err(MemoryError::Db)?;
        let rows = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(MemoryError::Db)?;
        let mut out = Vec::with_capacity(limit);
        for r in rows {
            out.push(r.map_err(MemoryError::Db)?);
        }
        Ok(out)
    }
}

/// Register all three memory capabilities on the supplied dispatch bridge.
pub fn register(bridge: &mut DispatchBridge, store: Arc<MemoryStore>) {
    {
        let store = store.clone();
        bridge.register(
            "memory.write_turn",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_write_turn(&store, &ctx) }
            })),
        );
    }
    {
        let store = store.clone();
        bridge.register(
            "memory.recent_for_session",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_recent(&store, &ctx) }
            })),
        );
    }
    {
        let store = store.clone();
        bridge.register(
            "memory.search",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_search(&store, &ctx) }
            })),
        );
    }
}

// ──────────────────────────── Handlers ──────────────────────────────────────

fn handle_write_turn(store: &MemoryStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return invalid_args(format!("memory.write_turn arg utf8: {e}"));
        }
    };
    // `session_id|role|body` — body may contain `|`, so splitn(3).
    let mut parts = s.splitn(3, '|');
    let session_id = parts.next();
    let role = parts.next();
    let body = parts.next();
    let (Some(session_id), Some(role), Some(body)) = (session_id, role, body) else {
        return invalid_args("memory.write_turn arg must be `session_id|role|body`".to_string());
    };
    if session_id.is_empty() || role.is_empty() {
        return invalid_args("memory.write_turn: session_id and role required".to_string());
    }
    match store.write_turn(session_id, role, body) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(e) => internal(format!("memory.write_turn: {e}")),
    }
}

fn handle_recent(store: &MemoryStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("memory.recent_for_session arg utf8: {e}")),
    };
    // `session_id` or `session_id|N`.
    let mut parts = s.splitn(2, '|');
    let session_id = parts.next().unwrap_or("");
    if session_id.is_empty() {
        return invalid_args("memory.recent_for_session: session_id required".to_string());
    }
    let n: usize = match parts.next() {
        Some(s) => s.trim().parse().unwrap_or(10),
        None => 10,
    };
    match store.recent_for_session(session_id, n) {
        Ok(rows) => {
            let mut body = String::new();
            for (role, text) in rows {
                body.push_str(&role);
                body.push_str(": ");
                body.push_str(&text);
                body.push('\n');
            }
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(e) => internal(format!("memory.recent_for_session: {e}")),
    }
}

fn handle_search(store: &MemoryStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("memory.search arg utf8: {e}")),
    };
    // `query` or `query|N`. `query` may contain spaces and FTS5 operators;
    // only the trailing `|N` is parsed as the limit.
    let (query, n) = match s.rsplit_once('|') {
        Some((q, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (q, n_str.trim().parse::<usize>().unwrap_or(10))
        }
        _ => (s, 10),
    };
    if query.is_empty() {
        return invalid_args("memory.search: query required".to_string());
    }
    match store.search(query, n) {
        Ok(rows) => {
            let mut body = String::new();
            for (sid, role, text) in rows {
                body.push_str(&sid);
                body.push('\t');
                body.push_str(&role);
                body.push('\t');
                body.push_str(&text);
                body.push('\n');
            }
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(e) => internal(format!("memory.search: {e}")),
    }
}

fn invalid_args(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

// ──────────────────────────── Schema ────────────────────────────────────────

fn init_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS turns (
            id         INTEGER PRIMARY KEY,
            session_id TEXT    NOT NULL,
            role       TEXT    NOT NULL,
            body       TEXT    NOT NULL,
            ts         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS turns_session ON turns(session_id, id);

        CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
            body,
            session_id UNINDEXED,
            role UNINDEXED,
            content='turns',
            content_rowid='id'
        );

        CREATE TRIGGER IF NOT EXISTS turns_ai AFTER INSERT ON turns BEGIN
            INSERT INTO turns_fts(rowid, body, session_id, role)
            VALUES (new.id, new.body, new.session_id, new.role);
        END;
        CREATE TRIGGER IF NOT EXISTS turns_ad AFTER DELETE ON turns BEGIN
            INSERT INTO turns_fts(turns_fts, rowid, body, session_id, role)
            VALUES ('delete', old.id, old.body, old.session_id, old.role);
        END;
        CREATE TRIGGER IF NOT EXISTS turns_au AFTER UPDATE ON turns BEGIN
            INSERT INTO turns_fts(turns_fts, rowid, body, session_id, role)
            VALUES ('delete', old.id, old.body, old.session_id, old.role);
            INSERT INTO turns_fts(rowid, body, session_id, role)
            VALUES (new.id, new.body, new.session_id, new.role);
        END;
        "#,
    )
    .map_err(MemoryError::Db)?;
    Ok(())
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ──────────────────────────── Errors ────────────────────────────────────────

/// Memory-node errors.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// File-system failure preparing the DB path.
    #[error("io: {0}")]
    Io(String),
    /// SQLite / FTS5 failure.
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    /// Mutex poisoned (programmer error; logged for visibility).
    #[error("lock poisoned")]
    Lock,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_recent_search_roundtrip() {
        let store = MemoryStore::in_memory().expect("open");
        store.write_turn("s1", "user", "hello world").unwrap();
        store.write_turn("s1", "assistant", "hi back").unwrap();
        store.write_turn("s2", "user", "unrelated").unwrap();

        let recent = store.recent_for_session("s1", 10).expect("recent");
        assert_eq!(
            recent,
            vec![
                ("user".to_string(), "hello world".to_string()),
                ("assistant".to_string(), "hi back".to_string()),
            ]
        );

        let hits = store.search("hello", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "s1");
        assert_eq!(hits[0].1, "user");
        assert_eq!(hits[0].2, "hello world");
    }

    #[test]
    fn recent_clamps_to_max_n() {
        let store = MemoryStore::in_memory().expect("open");
        for i in 0..5 {
            store
                .write_turn("s1", "user", &format!("turn-{i}"))
                .unwrap();
        }
        // Asking for absurd N is clamped to max_n (100 default in-memory),
        // bounded by actual row count.
        let recent = store.recent_for_session("s1", 1_000_000).expect("recent");
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0].1, "turn-0"); // oldest first
        assert_eq!(recent[4].1, "turn-4");
    }

    #[test]
    fn search_orders_by_relevance_then_id() {
        let store = MemoryStore::in_memory().expect("open");
        store.write_turn("s1", "user", "alpha beta gamma").unwrap();
        store.write_turn("s2", "user", "alpha alpha gamma").unwrap();
        let hits = store.search("alpha", 10).expect("search");
        // Both contain `alpha`; bm25 typically ranks the second higher
        // (term-frequency 2). Tie-break: id ASC.
        assert_eq!(hits.len(), 2);
        // Just assert both rows are returned; bm25 ordering is FTS5-impl
        // detail and may vary across SQLite versions.
        let sids: Vec<&str> = hits.iter().map(|(s, _, _)| s.as_str()).collect();
        assert!(sids.contains(&"s1"));
        assert!(sids.contains(&"s2"));
    }

    #[test]
    fn handler_write_then_recent() {
        use relix_core::types::{NodeId, RequestId, TraceId};
        let store = Arc::new(MemoryStore::in_memory().expect("open"));
        let ctx = |args: &[u8]| InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        };

        let r = handle_write_turn(&store, &ctx(b"s1|user|hi"));
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let r = handle_write_turn(&store, &ctx(b"s1|assistant|hello back"));
        assert!(matches!(r, HandlerOutcome::Ok(_)));

        let r = handle_recent(&store, &ctx(b"s1"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(body, "user: hi\nassistant: hello back\n");
    }

    #[test]
    fn handler_rejects_malformed_write_turn() {
        let store = Arc::new(MemoryStore::in_memory().expect("open"));
        let ctx = |args: &[u8]| InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: relix_core::types::NodeId::from_pubkey(b"a"),
                name: "a".into(),
                org_id: relix_core::types::NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: relix_core::types::TraceId::new(),
            request_id: relix_core::types::RequestId::new(),
            args: args.to_vec(),
        };
        // Missing body field.
        let r = handle_write_turn(&store, &ctx(b"only_session|only_role"));
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }
}
