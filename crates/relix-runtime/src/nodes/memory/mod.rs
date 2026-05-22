//! Memory node — SQLite + FTS5 session storage (M7) + persistent
//! agent memory (frozen-snapshot pattern, inspired by Hermes
//! `MEMORY.md` + `USER.md`).
//!
//! Capabilities registered on a controller with `[controller] node_type =
//! "memory"`:
//!
//! - `memory.write_turn`            — append one conversational turn.
//! - `memory.recent_for_session`    — return the most recent N turns.
//! - `memory.search`                — full-text search across all turns.
//! - `memory.agent_read`            — read agent + user persistent memory.
//! - `memory.agent_write`           — add/replace/remove/read persistent memory.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! All capabilities take and return UTF-8 strings. Args use `|` as a
//! field separator since SOL strings are taken verbatim (no JSON or CBOR
//! plumbing in SOL until Gate 2).
//!
//! | Method | Arg | Return |
//! |---|---|---|
//! | `memory.write_turn` | `session_id\|role\|text` | `ok\n` |
//! | `memory.recent_for_session` | `session_id` or `session_id\|N` (default 10) | `role: text\n` per turn, oldest first |
//! | `memory.search` | `query` or `query\|N` (default 10) | `session_id\trole\ttext\n` per match |
//! | `memory.agent_read` | `subject_id` | `agent_bytes=N\|user_bytes=M\n<N bytes><M bytes>` |
//! | `memory.agent_write` | `subject_id\|target\|action\|data` | `ok\|chars=N\n` for writes, raw content for read |
//!
//! ## Frozen-snapshot pattern
//!
//! `memory.agent_read` / `memory.agent_write` implement the
//! Hermes-style `MEMORY.md` + `USER.md` pattern. Memory is stored
//! durably in SQLite. Mid-session writes hit disk immediately but
//! the running AI session's system prompt does NOT re-render — the
//! snapshot is read once at chat-start and baked in. The refreshed
//! contents land in the next session.
//!
//! Two stores per `subject_id`:
//!
//! - `agent` — what the agent has learned about its environment,
//!   tools, project conventions, facts. Hard char cap: 2200.
//! - `user`  — what the agent knows about the user it serves —
//!   preferences, communication style, workflow habits. Hard char
//!   cap: 1375.
//!
//! Entry delimiter is `§` (U+00A7). Multi-character entries are
//! allowed; the delimiter only appears BETWEEN entries.
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

pub mod curator;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub use curator::{
    AiDispatcher, AiMeshDispatcher, AiPeerConfig, CuratorConfig, CuratorRunSummary, CuratorState,
    CuratorSubjectResult, spawn_curator_scheduler,
};

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
    /// Optional curator scheduler config. When `enabled = true`
    /// AND `ai_peer` is set, the memory controller spawns a
    /// periodic LLM-driven curation pass. See
    /// [`curator`] for the full design.
    #[serde(default)]
    pub curator: Option<CuratorConfig>,
}

fn default_max_n() -> usize {
    100
}

/// Hard char cap for the `agent` target — the agent's notes about
/// its environment, tools, project conventions. Matches the
/// Hermes `MEMORY.md` budget.
pub const AGENT_MEMORY_CAP_CHARS: usize = 2200;

/// Hard char cap for the `user` target — what the agent knows
/// about the user. Matches the Hermes `USER.md` budget.
pub const USER_MEMORY_CAP_CHARS: usize = 1375;

/// Section-sign character used as the entry delimiter between
/// agent-memory entries. Same convention Hermes uses.
pub const ENTRY_DELIMITER: char = '§';

/// Outcome of a `memory.agent_write` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentWriteOutcome {
    /// Write succeeded (add / replace / remove). Carries the new
    /// total character count of the target after the operation.
    Updated { chars: usize },
    /// Read returned the current content of the specified target.
    Read { content: String },
}

/// Char-cap for a memory target. Returns `None` for an invalid
/// target name.
fn target_cap(target: &str) -> Option<usize> {
    match target {
        "agent" => Some(AGENT_MEMORY_CAP_CHARS),
        "user" => Some(USER_MEMORY_CAP_CHARS),
        _ => None,
    }
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

    /// Persistent agent memory: read both `agent` and `user`
    /// content for a `subject_id`. Missing rows return empty
    /// strings (not an error) — first-call agents start blank.
    pub fn agent_read(&self, subject_id: &str) -> Result<(String, String), MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let agent = read_target(&conn, subject_id, "agent")?;
        let user = read_target(&conn, subject_id, "user")?;
        Ok((agent, user))
    }

    /// Persistent agent memory: write or read one target.
    ///
    /// `target` is `"agent"` or `"user"`. `action` is one of
    /// `"add"`, `"replace"`, `"remove"`, `"read"`. `data`
    /// semantics by action:
    ///
    /// - `add`: `data` is the new entry text. Appended to the
    ///   existing content, separated by [`ENTRY_DELIMITER`] when
    ///   the target was non-empty.
    /// - `replace`: `data` is `<find>\t<replacement>`. The unique
    ///   entry containing `<find>` is replaced wholesale with
    ///   `<replacement>`.  Ambiguous `<find>` returns an error.
    /// - `remove`: `data` is the substring identifying the entry
    ///   to drop. The matched entry (and its delimiter) is
    ///   removed; ambiguous matches return an error.
    /// - `read`: `data` is ignored. Returns the current content
    ///   of the target.
    ///
    /// Caps are enforced on every write — a write that would
    /// push the target past its cap returns `MemoryError::CapExceeded`
    /// with the proposed and max char counts.
    pub fn agent_write(
        &self,
        subject_id: &str,
        target: &str,
        action: &str,
        data: &str,
    ) -> Result<AgentWriteOutcome, MemoryError> {
        let Some(cap) = target_cap(target) else {
            return Err(MemoryError::InvalidArg(format!(
                "target must be 'agent' or 'user', got '{target}'"
            )));
        };
        if subject_id.is_empty() {
            return Err(MemoryError::InvalidArg("subject_id required".to_string()));
        }
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let current = read_target(&conn, subject_id, target)?;
        let new_content: String = match action {
            "read" => {
                // Read returns directly without writing.
                return Ok(AgentWriteOutcome::Read { content: current });
            }
            "add" => {
                if data.is_empty() {
                    return Err(MemoryError::InvalidArg(
                        "add: data (new entry text) required".to_string(),
                    ));
                }
                if data.contains(ENTRY_DELIMITER) {
                    return Err(MemoryError::InvalidArg(format!(
                        "add: entry text must not contain the entry delimiter '{}'",
                        ENTRY_DELIMITER
                    )));
                }
                if current.is_empty() {
                    data.to_string()
                } else {
                    format!("{current}{ENTRY_DELIMITER}{data}")
                }
            }
            "replace" => {
                let (find, replacement) = match data.split_once('\t') {
                    Some(p) => p,
                    None => {
                        return Err(MemoryError::InvalidArg(
                            "replace: data must be '<find>\\t<replacement>'".to_string(),
                        ));
                    }
                };
                if find.is_empty() {
                    return Err(MemoryError::InvalidArg(
                        "replace: <find> must not be empty".to_string(),
                    ));
                }
                if replacement.contains(ENTRY_DELIMITER) {
                    return Err(MemoryError::InvalidArg(format!(
                        "replace: <replacement> must not contain the entry delimiter '{}'",
                        ENTRY_DELIMITER
                    )));
                }
                let entries: Vec<&str> = current.split(ENTRY_DELIMITER).collect();
                let matches: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.contains(find))
                    .map(|(i, _)| i)
                    .collect();
                if matches.is_empty() {
                    return Err(MemoryError::NotFound(format!(
                        "replace: no entry contains '{find}'"
                    )));
                }
                if matches.len() > 1 {
                    return Err(MemoryError::Ambiguous(format!(
                        "replace: {} entries contain '{find}' — pick a more unique substring",
                        matches.len()
                    )));
                }
                let mut new_entries: Vec<String> =
                    entries.iter().map(|s| (*s).to_string()).collect();
                new_entries[matches[0]] = replacement.to_string();
                new_entries.join(&ENTRY_DELIMITER.to_string())
            }
            "remove" => {
                if data.is_empty() {
                    return Err(MemoryError::InvalidArg(
                        "remove: data (find substring) required".to_string(),
                    ));
                }
                let entries: Vec<&str> = current.split(ENTRY_DELIMITER).collect();
                let matches: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.contains(data))
                    .map(|(i, _)| i)
                    .collect();
                if matches.is_empty() {
                    return Err(MemoryError::NotFound(format!(
                        "remove: no entry contains '{data}'"
                    )));
                }
                if matches.len() > 1 {
                    return Err(MemoryError::Ambiguous(format!(
                        "remove: {} entries contain '{data}' — pick a more unique substring",
                        matches.len()
                    )));
                }
                let kept: Vec<String> = entries
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != matches[0])
                    .map(|(_, s)| (*s).to_string())
                    .collect();
                kept.join(&ENTRY_DELIMITER.to_string())
            }
            other => {
                return Err(MemoryError::InvalidArg(format!(
                    "action must be 'add', 'replace', 'remove', or 'read'; got '{other}'"
                )));
            }
        };
        let new_chars = new_content.chars().count();
        if new_chars > cap {
            return Err(MemoryError::CapExceeded {
                target: target.to_string(),
                proposed: new_chars,
                cap,
            });
        }
        upsert_target(&conn, subject_id, target, &new_content)?;
        Ok(AgentWriteOutcome::Updated { chars: new_chars })
    }

    /// Curator-only: atomically replace the full content of
    /// one (subject_id, target) row. Bypasses the
    /// `memory.agent_write` action vocabulary (add / replace /
    /// remove / read) because curation needs to set the whole
    /// blob at once. Caps are still enforced.
    pub fn agent_set_content(
        &self,
        subject_id: &str,
        target: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        let Some(cap) = curator_target_cap(target) else {
            return Err(MemoryError::InvalidArg(format!(
                "target must be 'agent' or 'user', got '{target}'"
            )));
        };
        if subject_id.is_empty() {
            return Err(MemoryError::InvalidArg("subject_id required".to_string()));
        }
        let chars = content.chars().count();
        if chars > cap {
            return Err(MemoryError::CapExceeded {
                target: target.to_string(),
                proposed: chars,
                cap,
            });
        }
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        upsert_target(&conn, subject_id, target, content)?;
        Ok(())
    }

    /// Curator-only: enumerate every subject_id that has at
    /// least one agent_memory row, and the combined character
    /// count of its agent + user content. Used by the
    /// scheduler to skip agents below the curation threshold.
    pub fn list_subjects_with_total_chars(&self) -> Result<Vec<(String, usize)>, MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT subject_id, SUM(LENGTH(content)) \
                 FROM agent_memory \
                 GROUP BY subject_id \
                 ORDER BY subject_id ASC",
            )
            .map_err(MemoryError::Db)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })
            .map_err(MemoryError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(MemoryError::Db)?);
        }
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

/// Register all memory capabilities on the supplied dispatch bridge.
///
/// `ai_cell` is the shared `OnceCell` populated by the memory
/// controller post-startup when `[memory.curator.ai_peer]` is
/// configured. The `memory.agent_curate` handler captures it
/// and reads through to whatever's set; an empty cell yields a
/// `RESPONDER_INTERNAL` "ai dispatcher not configured" error
/// for that one call. The curator scheduler captures the SAME
/// cell so manual + scheduled paths see the same dispatcher.
pub fn register(
    bridge: &mut DispatchBridge,
    store: Arc<MemoryStore>,
    ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
) {
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
    {
        let store = store.clone();
        bridge.register(
            "memory.agent_read",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_agent_read(&store, &ctx) }
            })),
        );
    }
    {
        let store = store.clone();
        bridge.register(
            "memory.agent_write",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_agent_write(&store, &ctx) }
            })),
        );
    }
    {
        let store = store.clone();
        let ai = ai_cell.clone();
        bridge.register(
            "memory.agent_curate",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                let ai = ai.clone();
                async move { handle_agent_curate(&store, &ai, &ctx).await }
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

fn handle_agent_read(store: &MemoryStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("memory.agent_read arg utf8: {e}")),
    };
    let subject_id = s.trim();
    if subject_id.is_empty() {
        return invalid_args("memory.agent_read: subject_id required".to_string());
    }
    let (agent, user) = match store.agent_read(subject_id) {
        Ok(pair) => pair,
        Err(e) => return internal(format!("memory.agent_read: {e}")),
    };
    let agent_bytes = agent.as_bytes();
    let user_bytes = user.as_bytes();
    let header = format!(
        "agent_bytes={}|user_bytes={}\n",
        agent_bytes.len(),
        user_bytes.len()
    );
    let mut out = Vec::with_capacity(header.len() + agent_bytes.len() + user_bytes.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(agent_bytes);
    out.extend_from_slice(user_bytes);
    HandlerOutcome::Ok(out)
}

fn handle_agent_write(store: &MemoryStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("memory.agent_write arg utf8: {e}")),
    };
    // `subject_id|target|action|data` — data may contain `|`,
    // so splitn(4).
    let mut parts = s.splitn(4, '|');
    let subject_id = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let action = parts.next().unwrap_or("");
    let data = parts.next().unwrap_or("");
    if subject_id.is_empty() || target.is_empty() || action.is_empty() {
        return invalid_args(
            "memory.agent_write arg must be `subject_id|target|action|data`".to_string(),
        );
    }
    match store.agent_write(subject_id, target, action, data) {
        Ok(AgentWriteOutcome::Updated { chars }) => {
            HandlerOutcome::Ok(format!("ok|chars={chars}\n").into_bytes())
        }
        Ok(AgentWriteOutcome::Read { content }) => HandlerOutcome::Ok(content.into_bytes()),
        Err(MemoryError::InvalidArg(c)) => invalid_args(format!("memory.agent_write: {c}")),
        Err(MemoryError::NotFound(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("memory.agent_write: {c}"),
            retry_hint: 2,
            retry_after: None,
        }),
        Err(MemoryError::Ambiguous(c)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("memory.agent_write: {c}"),
            retry_hint: 2,
            retry_after: None,
        }),
        Err(MemoryError::CapExceeded {
            target: t,
            proposed,
            cap,
        }) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "memory.agent_write: '{t}' write would be {proposed} chars (cap {cap}). \
                 Remove old entries before adding new ones."
            ),
            retry_hint: 2,
            retry_after: None,
        }),
        Err(e) => internal(format!("memory.agent_write: {e}")),
    }
}

async fn handle_agent_curate(
    store: &MemoryStore,
    ai_cell: &tokio::sync::OnceCell<Arc<dyn AiDispatcher>>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("memory.agent_curate arg utf8: {e}")),
    };
    // `subject_id|ai_peer_alias` — ai_peer_alias is informational
    // today; the dispatcher is configured at controller startup
    // and the alias is fixed there.  We parse and accept the
    // arg for forward-compat (multi-AI-peer routing later).
    let mut parts = s.splitn(2, '|');
    let subject_id = parts.next().unwrap_or("").trim();
    let _ai_alias = parts.next().unwrap_or("ai").trim();
    if subject_id.is_empty() {
        return invalid_args("memory.agent_curate: subject_id required".to_string());
    }
    let Some(dispatcher) = ai_cell.get() else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: "memory.agent_curate: AI dispatcher not configured (missing [memory.curator.ai_peer])".to_string(),
            retry_hint: 0,
            retry_after: None,
        });
    };
    match curator::curate_subject(store, dispatcher.as_ref(), subject_id).await {
        Ok(res) => HandlerOutcome::Ok(res.to_wire().into_bytes()),
        Err(curator::CuratorError::Store(e)) => internal(format!("memory.agent_curate: {e}")),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("memory.agent_curate: {e}"),
            retry_hint: 1,
            retry_after: None,
        }),
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

        -- Persistent per-agent memory (frozen-snapshot pattern).
        -- One row per (subject_id, target) pair. `content` is the
        -- raw text including the `§` entry delimiter between
        -- entries; the empty string means "no memory yet".
        CREATE TABLE IF NOT EXISTS agent_memory (
            subject_id TEXT    NOT NULL,
            target     TEXT    NOT NULL,
            content    TEXT    NOT NULL DEFAULT '',
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (subject_id, target)
        );
        "#,
    )
    .map_err(MemoryError::Db)?;
    Ok(())
}

fn read_target(conn: &Connection, subject_id: &str, target: &str) -> Result<String, MemoryError> {
    let mut stmt = conn
        .prepare(
            "SELECT content FROM agent_memory \
             WHERE subject_id = ?1 AND target = ?2",
        )
        .map_err(MemoryError::Db)?;
    let mut rows = stmt
        .query(params![subject_id, target])
        .map_err(MemoryError::Db)?;
    match rows.next().map_err(MemoryError::Db)? {
        Some(row) => Ok(row.get::<_, String>(0).map_err(MemoryError::Db)?),
        None => Ok(String::new()),
    }
}

fn upsert_target(
    conn: &Connection,
    subject_id: &str,
    target: &str,
    content: &str,
) -> Result<(), MemoryError> {
    let ts = unix_secs();
    conn.execute(
        "INSERT INTO agent_memory (subject_id, target, content, updated_at) \
         VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(subject_id, target) DO UPDATE SET \
            content    = excluded.content, \
            updated_at = excluded.updated_at",
        params![subject_id, target, content, ts],
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

/// Mirror of [`target_cap`] for curator-side code. Same
/// table — kept identical so the two stay in lock-step.
fn curator_target_cap(target: &str) -> Option<usize> {
    target_cap(target)
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
    /// `agent_write` arg was malformed (caller's fault).
    #[error("invalid arg: {0}")]
    InvalidArg(String),
    /// `replace` / `remove` `<find>` matched no existing entry.
    #[error("not found: {0}")]
    NotFound(String),
    /// `replace` / `remove` `<find>` matched more than one entry.
    #[error("ambiguous: {0}")]
    Ambiguous(String),
    /// Write would exceed the target's hard char cap.
    #[error("'{target}' cap exceeded: {proposed} > {cap}")]
    CapExceeded {
        /// Target being written (`agent` or `user`).
        target: String,
        /// Char count the write would produce.
        proposed: usize,
        /// Hard cap for the target.
        cap: usize,
    },
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

    // ── Agent memory (frozen-snapshot) ────────────────────────────

    #[test]
    fn agent_read_empty_returns_empty_strings() {
        let store = MemoryStore::in_memory().expect("open");
        let (a, u) = store.agent_read("alice").unwrap();
        assert!(a.is_empty());
        assert!(u.is_empty());
    }

    #[test]
    fn agent_write_add_first_entry_has_no_delimiter() {
        let store = MemoryStore::in_memory().expect("open");
        let out = store
            .agent_write("alice", "agent", "add", "remember to test caps")
            .unwrap();
        match out {
            AgentWriteOutcome::Updated { chars } => assert_eq!(chars, 21),
            _ => panic!("expected Updated"),
        }
        let (a, _) = store.agent_read("alice").unwrap();
        assert_eq!(a, "remember to test caps");
    }

    #[test]
    fn agent_write_add_subsequent_entry_uses_section_sign() {
        let store = MemoryStore::in_memory().expect("open");
        store.agent_write("alice", "agent", "add", "first").unwrap();
        store
            .agent_write("alice", "agent", "add", "second")
            .unwrap();
        let (a, _) = store.agent_read("alice").unwrap();
        assert_eq!(a, "first§second");
    }

    #[test]
    fn agent_write_rejects_entry_containing_delimiter() {
        let store = MemoryStore::in_memory().expect("open");
        let err = store
            .agent_write("alice", "agent", "add", "has § inside")
            .unwrap_err();
        match err {
            MemoryError::InvalidArg(_) => {}
            other => panic!("expected InvalidArg, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_add_rejects_at_2201_chars_on_agent_target() {
        let store = MemoryStore::in_memory().expect("open");
        // Single entry of exactly 2201 chars (cap is 2200).
        let blob: String = (0..2201).map(|_| 'x').collect();
        let err = store
            .agent_write("alice", "agent", "add", &blob)
            .unwrap_err();
        match err {
            MemoryError::CapExceeded {
                target,
                proposed,
                cap,
            } => {
                assert_eq!(target, "agent");
                assert_eq!(proposed, 2201);
                assert_eq!(cap, AGENT_MEMORY_CAP_CHARS);
            }
            other => panic!("expected CapExceeded, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_add_rejects_at_1376_chars_on_user_target() {
        let store = MemoryStore::in_memory().expect("open");
        let blob: String = (0..1376).map(|_| 'y').collect();
        let err = store
            .agent_write("alice", "user", "add", &blob)
            .unwrap_err();
        match err {
            MemoryError::CapExceeded {
                target,
                proposed,
                cap,
            } => {
                assert_eq!(target, "user");
                assert_eq!(proposed, 1376);
                assert_eq!(cap, USER_MEMORY_CAP_CHARS);
            }
            other => panic!("expected CapExceeded, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_replace_finds_by_substring() {
        let store = MemoryStore::in_memory().expect("open");
        store
            .agent_write("alice", "agent", "add", "rust uses cargo")
            .unwrap();
        store
            .agent_write("alice", "agent", "add", "python uses pip")
            .unwrap();
        store
            .agent_write("alice", "agent", "replace", "rust\trust uses cargo + uv")
            .unwrap();
        let (a, _) = store.agent_read("alice").unwrap();
        assert_eq!(a, "rust uses cargo + uv§python uses pip");
    }

    #[test]
    fn agent_write_replace_ambiguous_substring_rejects() {
        let store = MemoryStore::in_memory().expect("open");
        store
            .agent_write("alice", "agent", "add", "alpha-one")
            .unwrap();
        store
            .agent_write("alice", "agent", "add", "alpha-two")
            .unwrap();
        let err = store
            .agent_write("alice", "agent", "replace", "alpha\twhatever")
            .unwrap_err();
        match err {
            MemoryError::Ambiguous(_) => {}
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_replace_unknown_substring_rejects() {
        let store = MemoryStore::in_memory().expect("open");
        store.agent_write("alice", "agent", "add", "first").unwrap();
        let err = store
            .agent_write("alice", "agent", "replace", "no-match\tx")
            .unwrap_err();
        match err {
            MemoryError::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_remove_drops_matched_entry() {
        let store = MemoryStore::in_memory().expect("open");
        store
            .agent_write("alice", "agent", "add", "keep me")
            .unwrap();
        store
            .agent_write("alice", "agent", "add", "drop me")
            .unwrap();
        store
            .agent_write("alice", "agent", "add", "also keep")
            .unwrap();
        store
            .agent_write("alice", "agent", "remove", "drop")
            .unwrap();
        let (a, _) = store.agent_read("alice").unwrap();
        assert_eq!(a, "keep me§also keep");
    }

    #[test]
    fn agent_write_read_action_returns_current_target() {
        let store = MemoryStore::in_memory().expect("open");
        store
            .agent_write("alice", "agent", "add", "agent-thing")
            .unwrap();
        store
            .agent_write("alice", "user", "add", "user-thing")
            .unwrap();
        let out = store.agent_write("alice", "user", "read", "").unwrap();
        match out {
            AgentWriteOutcome::Read { content } => assert_eq!(content, "user-thing"),
            _ => panic!("expected Read"),
        }
    }

    #[test]
    fn agent_write_rejects_unknown_target() {
        let store = MemoryStore::in_memory().expect("open");
        let err = store
            .agent_write("alice", "secrets", "add", "shh")
            .unwrap_err();
        match err {
            MemoryError::InvalidArg(c) => assert!(c.contains("'agent' or 'user'")),
            other => panic!("expected InvalidArg, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_rejects_unknown_action() {
        let store = MemoryStore::in_memory().expect("open");
        let err = store
            .agent_write("alice", "agent", "delete-all", "")
            .unwrap_err();
        match err {
            MemoryError::InvalidArg(c) => {
                assert!(c.contains("'add', 'replace', 'remove', or 'read'"))
            }
            other => panic!("expected InvalidArg, got {other:?}"),
        }
    }

    #[test]
    fn agent_write_subject_isolation_two_subjects() {
        let store = MemoryStore::in_memory().expect("open");
        store
            .agent_write("alice", "agent", "add", "alice-notes")
            .unwrap();
        store
            .agent_write("bob", "agent", "add", "bob-notes")
            .unwrap();
        let (a_alice, _) = store.agent_read("alice").unwrap();
        let (a_bob, _) = store.agent_read("bob").unwrap();
        assert_eq!(a_alice, "alice-notes");
        assert_eq!(a_bob, "bob-notes");
        // Neither sees the other's content.
        assert!(!a_alice.contains("bob"));
        assert!(!a_bob.contains("alice"));
    }

    #[test]
    fn handle_agent_read_header_format() {
        use relix_core::types::{NodeId, RequestId, TraceId};
        let store = Arc::new(MemoryStore::in_memory().expect("open"));
        store.agent_write("alice", "agent", "add", "hello").unwrap();
        store.agent_write("alice", "user", "add", "world!").unwrap();
        let ctx = InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: b"alice".to_vec(),
        };
        let r = handle_agent_read(&store, &ctx);
        let body = match r {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        // Header line is `agent_bytes=5|user_bytes=6\n`.
        let nl = body.iter().position(|b| *b == b'\n').unwrap();
        let header = std::str::from_utf8(&body[..nl]).unwrap();
        assert_eq!(header, "agent_bytes=5|user_bytes=6");
        let payload = &body[nl + 1..];
        assert_eq!(payload, b"helloworld!");
    }

    #[test]
    fn handle_agent_write_cap_exceeded_uses_invalid_args_kind() {
        use relix_core::types::{NodeId, RequestId, TraceId};
        let store = Arc::new(MemoryStore::in_memory().expect("open"));
        let blob: String = (0..2201).map(|_| 'x').collect();
        let arg = format!("alice|agent|add|{blob}");
        let ctx = InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"a"),
                name: "a".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: arg.into_bytes(),
        };
        match handle_agent_write(&store, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("cap"));
            }
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
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
