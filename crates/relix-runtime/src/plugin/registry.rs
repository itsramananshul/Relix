//! Plugin registry persisted in SQLite — one row per plugin
//! known to the host. Survives controller restarts so the
//! dashboard can show "last seen at" timestamps and prior status
//! across reboots.
//!
//! The plugin_id is a stable 16-hex content hash of
//! `(name, version, manifest_path)`. Re-registering the same
//! plugin returns the same id (idempotent registration).

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::manifest::PluginManifest;

/// SQLite-backed registry.
pub struct PluginRegistry {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("io: {0}")]
    Io(String),
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("lock poisoned")]
    Lock,
}

/// Lifecycle states. Mirrors what the dashboard shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    Registered,
    Active,
    Error,
    Disabled,
}

impl PluginStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Active => "active",
            Self::Error => "error",
            Self::Disabled => "disabled",
        }
    }
    pub fn from_wire(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "error" => Self::Error,
            "disabled" => Self::Disabled,
            _ => Self::Registered,
        }
    }
}

/// One row read back from the registry.
#[derive(Clone, Debug)]
pub struct StoredPlugin {
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub manifest_path: String,
    pub status: PluginStatus,
    pub error_message: String,
    pub registered_at: i64,
    pub last_seen_at: Option<i64>,
    /// JSON array of method names exposed by this plugin.
    pub capabilities: Vec<String>,
    pub node_type: String,
}

impl PluginRegistry {
    pub fn open(path: &Path) -> Result<Self, RegistryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| RegistryError::Io(format!("{}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "plugin_registry");
        crate::db::ensure_migration_table(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, RegistryError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), RegistryError> {
        // CORR PART 2: track schema in `_relix_migrations`.
        let body_sql = "CREATE TABLE IF NOT EXISTS plugins (\n                plugin_id     TEXT    NOT NULL PRIMARY KEY,\n                name          TEXT    NOT NULL,\n                version       TEXT    NOT NULL,\n                description   TEXT    NOT NULL,\n                author        TEXT    NOT NULL,\n                manifest_path TEXT    NOT NULL,\n                status        TEXT    NOT NULL DEFAULT 'registered',\n                error_message TEXT    NOT NULL DEFAULT '',\n                registered_at INTEGER NOT NULL,\n                last_seen_at  INTEGER,\n                capabilities  TEXT    NOT NULL,\n                node_type     TEXT    NOT NULL DEFAULT ''\n            );\n            CREATE INDEX IF NOT EXISTS plugins_status ON plugins(status);";
        crate::db::claim_legacy_migration(conn, "plugin_registry.v1", |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='plugins'",
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })?;
        if !crate::db::is_migration_applied(conn, "plugin_registry.v1")? {
            conn.execute_batch(body_sql)?;
            crate::db::record_migration_applied_by_id(
                conn,
                "plugin_registry.v1",
                &crate::db::checksum_sql(body_sql),
            )?;
        }
        Ok(())
    }

    /// Compute the stable plugin_id for a manifest path.
    pub fn plugin_id_for(m: &PluginManifest, manifest_path: &Path) -> String {
        let key = format!(
            "{}|{}|{}",
            m.plugin.name,
            m.plugin.version,
            manifest_path.display()
        );
        let hash = blake3::hash(key.as_bytes());
        hash.to_hex().to_string().chars().take(16).collect()
    }

    /// Register-or-update a plugin row.
    pub fn upsert(
        &self,
        m: &PluginManifest,
        manifest_path: &Path,
    ) -> Result<String, RegistryError> {
        let plugin_id = Self::plugin_id_for(m, manifest_path);
        let now = unix_secs();
        let caps_json = serde_json::to_string(
            &m.plugin
                .capabilities
                .provides
                .iter()
                .map(|c| c.method.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|e| RegistryError::Io(format!("caps json: {e}")))?;
        let node_type = m
            .plugin
            .node_type
            .as_ref()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        let conn = self.conn.lock().map_err(|_| RegistryError::Lock)?;
        conn.execute(
            "INSERT INTO plugins \
             (plugin_id, name, version, description, author, manifest_path, \
              status, error_message, registered_at, last_seen_at, capabilities, node_type) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(plugin_id) DO UPDATE SET \
                name=excluded.name, \
                version=excluded.version, \
                description=excluded.description, \
                author=excluded.author, \
                manifest_path=excluded.manifest_path, \
                capabilities=excluded.capabilities, \
                node_type=excluded.node_type",
            params![
                &plugin_id,
                &m.plugin.name,
                &m.plugin.version,
                &m.plugin.description,
                &m.plugin.author,
                manifest_path.display().to_string(),
                PluginStatus::Registered.as_wire(),
                "",
                now,
                Option::<i64>::None,
                caps_json,
                node_type,
            ],
        )?;
        Ok(plugin_id)
    }

    /// Update status (+ optional error message) for a known plugin.
    pub fn set_status(
        &self,
        plugin_id: &str,
        status: PluginStatus,
        error_message: Option<&str>,
    ) -> Result<(), RegistryError> {
        let conn = self.conn.lock().map_err(|_| RegistryError::Lock)?;
        conn.execute(
            "UPDATE plugins SET status = ?1, error_message = ?2 WHERE plugin_id = ?3",
            params![status.as_wire(), error_message.unwrap_or(""), plugin_id],
        )?;
        Ok(())
    }

    /// Stamp `last_seen_at = now` on a plugin. Called by the
    /// host after a successful /health probe.
    pub fn touch(&self, plugin_id: &str) -> Result<(), RegistryError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| RegistryError::Lock)?;
        conn.execute(
            "UPDATE plugins SET last_seen_at = ?1 WHERE plugin_id = ?2",
            params![now, plugin_id],
        )?;
        Ok(())
    }

    pub fn get(&self, plugin_id: &str) -> Result<Option<StoredPlugin>, RegistryError> {
        let conn = self.conn.lock().map_err(|_| RegistryError::Lock)?;
        conn.query_row(
            "SELECT plugin_id, name, version, description, author, manifest_path, \
                    status, error_message, registered_at, last_seen_at, capabilities, node_type \
             FROM plugins WHERE plugin_id = ?1",
            params![plugin_id],
            row_to_stored,
        )
        .optional()
        .map_err(Into::into)
    }

    /// List all plugins, newest registration first.
    pub fn list(&self) -> Result<Vec<StoredPlugin>, RegistryError> {
        let conn = self.conn.lock().map_err(|_| RegistryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT plugin_id, name, version, description, author, manifest_path, \
                    status, error_message, registered_at, last_seen_at, capabilities, node_type \
             FROM plugins ORDER BY registered_at DESC, plugin_id ASC",
        )?;
        let rows = stmt.query_map([], row_to_stored)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn row_to_stored(row: &rusqlite::Row) -> rusqlite::Result<StoredPlugin> {
    let caps_json: String = row.get(10)?;
    let caps: Vec<String> = serde_json::from_str(&caps_json).unwrap_or_default();
    Ok(StoredPlugin {
        plugin_id: row.get(0)?,
        name: row.get(1)?,
        version: row.get(2)?,
        description: row.get(3)?,
        author: row.get(4)?,
        manifest_path: row.get(5)?,
        status: PluginStatus::from_wire(&row.get::<_, String>(6)?),
        error_message: row.get(7)?,
        registered_at: row.get(8)?,
        last_seen_at: row.get::<_, Option<i64>>(9)?,
        capabilities: caps,
        node_type: row.get(11)?,
    })
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PluginManifest;
    use std::io::Write;

    fn write_manifest(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("dummy")).unwrap();
        let mut f = std::fs::File::create(dir.path().join("plugin.toml")).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        dir
    }

    fn full() -> &'static str {
        r#"
            [plugin]
            name        = "test-plugin"
            version     = "0.1.0"
            description = "Plugin for tests"
            author      = "tester"

            [[plugin.capabilities.provides]]
            method      = "test.alpha"
            description = "alpha"
            risk_level  = "low"

            [[plugin.capabilities.provides]]
            method      = "test.beta"
            description = "beta"

            [plugin.runtime]
            kind                = "subprocess"
            binary              = "./dummy"
            protocol            = "relix-plugin-v1"
            invoke_timeout_secs = 10
        "#
    }

    #[test]
    fn upsert_returns_stable_plugin_id() {
        let dir = write_manifest(full());
        let path = dir.path().join("plugin.toml");
        let m = PluginManifest::load_from_path(&path).unwrap();
        let r = PluginRegistry::in_memory().unwrap();
        let id_a = r.upsert(&m, &path).unwrap();
        let id_b = r.upsert(&m, &path).unwrap();
        assert_eq!(id_a.len(), 16);
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn get_returns_capabilities_and_status() {
        let dir = write_manifest(full());
        let path = dir.path().join("plugin.toml");
        let m = PluginManifest::load_from_path(&path).unwrap();
        let r = PluginRegistry::in_memory().unwrap();
        let id = r.upsert(&m, &path).unwrap();
        let row = r.get(&id).unwrap().unwrap();
        assert_eq!(row.name, "test-plugin");
        assert_eq!(row.capabilities, vec!["test.alpha", "test.beta"]);
        assert_eq!(row.status, PluginStatus::Registered);
        assert!(row.last_seen_at.is_none());
    }

    #[test]
    fn set_status_persists() {
        let dir = write_manifest(full());
        let path = dir.path().join("plugin.toml");
        let m = PluginManifest::load_from_path(&path).unwrap();
        let r = PluginRegistry::in_memory().unwrap();
        let id = r.upsert(&m, &path).unwrap();
        r.set_status(&id, PluginStatus::Active, None).unwrap();
        assert_eq!(r.get(&id).unwrap().unwrap().status, PluginStatus::Active);
        r.set_status(&id, PluginStatus::Error, Some("bad startup"))
            .unwrap();
        let row = r.get(&id).unwrap().unwrap();
        assert_eq!(row.status, PluginStatus::Error);
        assert_eq!(row.error_message, "bad startup");
    }

    #[test]
    fn touch_sets_last_seen_at() {
        let dir = write_manifest(full());
        let path = dir.path().join("plugin.toml");
        let m = PluginManifest::load_from_path(&path).unwrap();
        let r = PluginRegistry::in_memory().unwrap();
        let id = r.upsert(&m, &path).unwrap();
        r.touch(&id).unwrap();
        assert!(r.get(&id).unwrap().unwrap().last_seen_at.is_some());
    }

    #[test]
    fn list_returns_rows_newest_first() {
        let dir1 = write_manifest(full());
        let p1 = dir1.path().join("plugin.toml");
        let m1 = PluginManifest::load_from_path(&p1).unwrap();
        let r = PluginRegistry::in_memory().unwrap();
        r.upsert(&m1, &p1).unwrap();
        let v = r.list().unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "test-plugin");
    }
}
