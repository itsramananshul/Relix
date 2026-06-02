//! SQLite-backed `AgentRegistry` implementation.
//!
//! Provides [`SqliteAgentRegistry`] which implements the
//! [`relix_core::agent::AgentRegistry`] trait using the same SQLite/rusqlite
//! stack already used by the coordinator node and memory store.
//!
//! Schema (single `agents` table):
//!
//! ```sql
//! CREATE TABLE agents (
//!     agent_id   TEXT PRIMARY KEY,
//!     name       TEXT NOT NULL,
//!     role       TEXT NOT NULL,
//!     status     TEXT NOT NULL DEFAULT 'active',
//!     created_at INTEGER NOT NULL,   -- Unix seconds
//!     updated_at INTEGER NOT NULL    -- Unix seconds
//! );
//! ```
//!
//! ## Note on stack choice
//!
//! REL-18 originally specified SQLx + Postgres (Supabase). The workspace has
//! no SQLx dependency and all existing persistence uses rusqlite (SQLite); this
//! implementation follows that pattern for consistency. A Postgres-backed
//! implementation can be added behind the same `AgentRegistry` trait without
//! touching these types.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use relix_core::agent::{AgentError, AgentId, AgentRecord, AgentRegistry, AgentStatus};

// ── SqliteAgentRegistry ───────────────────────────────────────

/// SQLite-backed agent registry.
///
/// Wraps a `rusqlite::Connection` behind `Arc<Mutex<…>>` so it is
/// `Send + Sync` and can be shared across threads via `Arc`.
pub struct SqliteAgentRegistry {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteAgentRegistry {
    /// Open (or create) a SQLite database at `path` and initialise the schema.
    pub fn open(path: &Path) -> Result<Self, AgentError> {
        let conn = Connection::open(path).map_err(|e| AgentError::Storage(e.to_string()))?;
        let reg = SqliteAgentRegistry {
            conn: Arc::new(Mutex::new(conn)),
        };
        reg.init_schema()?;
        Ok(reg)
    }

    /// Open an in-memory database; useful for tests and ephemeral sessions.
    pub fn open_in_memory() -> Result<Self, AgentError> {
        let conn = Connection::open_in_memory().map_err(|e| AgentError::Storage(e.to_string()))?;
        let reg = SqliteAgentRegistry {
            conn: Arc::new(Mutex::new(conn)),
        };
        reg.init_schema()?;
        Ok(reg)
    }

    fn init_schema(&self) -> Result<(), AgentError> {
        self.conn
            .lock()
            .map_err(|e| AgentError::Storage(e.to_string()))?
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS agents (
                     agent_id   TEXT PRIMARY KEY,
                     name       TEXT NOT NULL,
                     role       TEXT NOT NULL,
                     status     TEXT NOT NULL DEFAULT 'active',
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );",
            )
            .map_err(|e| AgentError::Storage(e.to_string()))
    }
}

fn now_secs() -> Result<i64, AgentError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| AgentError::Storage(e.to_string()))
}

fn row_to_record(
    id: String,
    name: String,
    role: String,
    status_str: String,
    created_at: i64,
    updated_at: i64,
) -> Result<AgentRecord, AgentError> {
    let agent_id = AgentId::new(id)?;
    let status: AgentStatus = status_str.parse()?;
    Ok(AgentRecord {
        agent_id,
        name,
        role,
        status,
        created_at,
        updated_at,
    })
}

impl AgentRegistry for SqliteAgentRegistry {
    fn create(&self, agent_id: AgentId, name: &str, role: &str) -> Result<AgentRecord, AgentError> {
        let now = now_secs()?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let result = conn.execute(
            "INSERT INTO agents (agent_id, name, role, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
            params![agent_id.as_str(), name, role, now],
        );

        match result {
            Ok(_) => Ok(AgentRecord {
                agent_id,
                name: name.to_string(),
                role: role.to_string(),
                status: AgentStatus::Active,
                created_at: now,
                updated_at: now,
            }),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("UNIQUE constraint failed") {
                    Err(AgentError::AlreadyExists(agent_id))
                } else {
                    Err(AgentError::Storage(msg))
                }
            }
        }
    }

    fn get(&self, agent_id: &AgentId) -> Result<Option<AgentRecord>, AgentError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let result = conn
            .query_row(
                "SELECT agent_id, name, role, status, created_at, updated_at
                 FROM agents WHERE agent_id = ?1",
                params![agent_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        match result {
            None => Ok(None),
            Some((id, name, role, status, created_at, updated_at)) => {
                row_to_record(id, name, role, status, created_at, updated_at).map(Some)
            }
        }
    }

    fn list(&self) -> Result<Vec<AgentRecord>, AgentError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT agent_id, name, role, status, created_at, updated_at
                 FROM agents ORDER BY created_at ASC",
            )
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let mut records = Vec::new();
        for row in rows {
            let (id, name, role, status, created_at, updated_at) =
                row.map_err(|e| AgentError::Storage(e.to_string()))?;
            records.push(row_to_record(
                id, name, role, status, created_at, updated_at,
            )?);
        }
        Ok(records)
    }

    fn revoke(&self, agent_id: &AgentId) -> Result<(), AgentError> {
        let now = now_secs()?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        let rows = conn
            .execute(
                "UPDATE agents SET status = 'revoked', updated_at = ?2
                 WHERE agent_id = ?1",
                params![agent_id.as_str(), now],
            )
            .map_err(|e| AgentError::Storage(e.to_string()))?;

        if rows == 0 {
            return Err(AgentError::NotFound(agent_id.clone()));
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> SqliteAgentRegistry {
        SqliteAgentRegistry::open_in_memory().expect("in-memory db")
    }

    fn id(s: &str) -> AgentId {
        AgentId::new(s).unwrap()
    }

    // ── create ────────────────────────────────────────────────

    #[test]
    fn create_returns_active_record() {
        let reg = reg();
        let rec = reg.create(id("agt_alice"), "Alice", "agent").unwrap();
        assert_eq!(rec.agent_id.as_str(), "agt_alice");
        assert_eq!(rec.name, "Alice");
        assert_eq!(rec.role, "agent");
        assert_eq!(rec.status, AgentStatus::Active);
        assert!(rec.created_at > 0);
        assert_eq!(rec.created_at, rec.updated_at);
    }

    #[test]
    fn duplicate_agent_id_rejected() {
        let reg = reg();
        reg.create(id("agt_bob"), "Bob", "agent").unwrap();
        let err = reg.create(id("agt_bob"), "Bob2", "agent").unwrap_err();
        assert!(matches!(err, AgentError::AlreadyExists(_)));
    }

    // ── get ───────────────────────────────────────────────────

    #[test]
    fn get_existing_returns_record() {
        let reg = reg();
        reg.create(id("agt_carol"), "Carol", "service").unwrap();
        let rec = reg.get(&id("agt_carol")).unwrap().expect("should exist");
        assert_eq!(rec.name, "Carol");
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let reg = reg();
        let result = reg.get(&id("agt_nobody")).unwrap();
        assert!(result.is_none());
    }

    // ── list ──────────────────────────────────────────────────

    #[test]
    fn list_empty_registry() {
        let reg = reg();
        assert!(reg.list().unwrap().is_empty());
    }

    #[test]
    fn list_includes_all_statuses() {
        let reg = reg();
        reg.create(id("agt_a1"), "A1", "agent").unwrap();
        reg.create(id("agt_a2"), "A2", "agent").unwrap();
        reg.revoke(&id("agt_a2")).unwrap();

        let records = reg.list().unwrap();
        assert_eq!(records.len(), 2);
        let statuses: Vec<_> = records.iter().map(|r| r.status).collect();
        assert!(statuses.contains(&AgentStatus::Active));
        assert!(statuses.contains(&AgentStatus::Revoked));
    }

    // ── revoke ────────────────────────────────────────────────

    #[test]
    fn revoke_updates_status() {
        let reg = reg();
        reg.create(id("agt_dave"), "Dave", "agent").unwrap();
        reg.revoke(&id("agt_dave")).unwrap();

        let rec = reg.get(&id("agt_dave")).unwrap().expect("should exist");
        assert_eq!(rec.status, AgentStatus::Revoked);
        assert!(!rec.is_active());
    }

    #[test]
    fn revoke_idempotent_for_already_revoked() {
        let reg = reg();
        reg.create(id("agt_eve"), "Eve", "agent").unwrap();
        reg.revoke(&id("agt_eve")).unwrap();
        // Revoking again should succeed (idempotent) since 1 row is still matched.
        reg.revoke(&id("agt_eve")).unwrap();
    }

    #[test]
    fn revoke_nonexistent_returns_not_found() {
        let reg = reg();
        let err = reg.revoke(&id("agt_ghost")).unwrap_err();
        assert!(matches!(err, AgentError::NotFound(_)));
    }

    // ── round-trip through DB ─────────────────────────────────

    #[test]
    fn created_record_survives_get_roundtrip() {
        let reg = reg();
        let original = reg.create(id("agt_frank"), "Frank", "operator").unwrap();
        let fetched = reg.get(&id("agt_frank")).unwrap().expect("must exist");
        assert_eq!(original.agent_id, fetched.agent_id);
        assert_eq!(original.name, fetched.name);
        assert_eq!(original.role, fetched.role);
        assert_eq!(original.status, fetched.status);
    }
}
