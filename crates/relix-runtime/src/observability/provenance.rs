//! Provenance registry — captures *what was active* when a
//! given trace ran, so operators can answer "the bot regressed
//! on Tuesday — what shipped between the good trace and the
//! bad one?".
//!
//! A [`ProvenanceSnapshot`] pins, for one `trace_id`:
//!
//! - the LLM model id,
//! - the policy version,
//! - the `{skill -> version}` map,
//! - the `{tool -> version}` map.
//!
//! Snapshots are persisted in SQLite (JSON blob columns for
//! the two maps) and indexed by `trace_id`. The registry can
//! diff two snapshots and produce a flat list of
//! [`ProvenanceChange`] entries — the regression report.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ProvenanceError {
    #[error("sqlite: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("lock poisoned")]
    Lock,
    #[error("snapshot for trace {0} not found")]
    NotFound(String),
}

/// One frozen snapshot of the agent's surface for a single
/// trace. `skill_versions` and `tool_versions` use `BTreeMap`
/// so diffs are deterministic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceSnapshot {
    pub trace_id: String,
    pub timestamp_unix: i64,
    pub model_id: String,
    pub policy_version: String,
    pub skill_versions: BTreeMap<String, String>,
    pub tool_versions: BTreeMap<String, String>,
}

/// One field-level difference between two snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProvenanceChange {
    Model {
        from: String,
        to: String,
    },
    Policy {
        from: String,
        to: String,
    },
    SkillAdded {
        name: String,
        version: String,
    },
    SkillRemoved {
        name: String,
        version: String,
    },
    SkillChanged {
        name: String,
        from: String,
        to: String,
    },
    ToolAdded {
        name: String,
        version: String,
    },
    ToolRemoved {
        name: String,
        version: String,
    },
    ToolChanged {
        name: String,
        from: String,
        to: String,
    },
}

/// The diff between two snapshots, in stable order:
/// model → policy → skill changes (added, removed, changed)
/// → tool changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceDiff {
    pub a: String,
    pub b: String,
    pub changes: Vec<ProvenanceChange>,
}

/// SQLite-backed registry. Cheap to clone (Arc inside).
pub struct ProvenanceRegistry {
    conn: Arc<Mutex<Connection>>,
}

impl ProvenanceRegistry {
    pub fn open(path: &Path) -> Result<Self, ProvenanceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ProvenanceError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, ProvenanceError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert or replace the snapshot for a trace. Maps are
    /// serialised as JSON columns.
    pub fn record(&self, snap: &ProvenanceSnapshot) -> Result<(), ProvenanceError> {
        let skills = serde_json::to_string(&snap.skill_versions)?;
        let tools = serde_json::to_string(&snap.tool_versions)?;
        let conn = self.conn.lock().map_err(|_| ProvenanceError::Lock)?;
        conn.execute(
            "INSERT OR REPLACE INTO provenance_snapshots \
             (trace_id, timestamp_unix, model_id, policy_version, skill_versions, tool_versions) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                snap.trace_id,
                snap.timestamp_unix,
                snap.model_id,
                snap.policy_version,
                skills,
                tools,
            ],
        )?;
        Ok(())
    }

    /// Fetch the snapshot for a trace, if recorded.
    pub fn get(&self, trace_id: &str) -> Result<Option<ProvenanceSnapshot>, ProvenanceError> {
        let conn = self.conn.lock().map_err(|_| ProvenanceError::Lock)?;
        let row = conn
            .query_row(
                "SELECT trace_id, timestamp_unix, model_id, policy_version, skill_versions, tool_versions \
                 FROM provenance_snapshots WHERE trace_id = ?1",
                params![trace_id],
                row_to_snapshot,
            )
            .optional()?;
        match row {
            Some(r) => r.map(Some),
            None => Ok(None),
        }
    }

    /// GAP 13: return the newest `limit` snapshots, newest
    /// first. Used by `GET /v1/provenance/recent` and the CLI
    /// `relix provenance history / audit` subcommands.
    pub fn list_recent(&self, limit: usize) -> Result<Vec<ProvenanceSnapshot>, ProvenanceError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| ProvenanceError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT trace_id, timestamp_unix, model_id, policy_version, skill_versions, tool_versions \
             FROM provenance_snapshots \
             ORDER BY timestamp_unix DESC, trace_id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_snapshot)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Compute the diff between two recorded snapshots.
    pub fn diff(&self, a: &str, b: &str) -> Result<ProvenanceDiff, ProvenanceError> {
        let snap_a = self
            .get(a)?
            .ok_or_else(|| ProvenanceError::NotFound(a.to_string()))?;
        let snap_b = self
            .get(b)?
            .ok_or_else(|| ProvenanceError::NotFound(b.to_string()))?;
        Ok(diff_snapshots(&snap_a, &snap_b))
    }
}

/// Pure-function diff so tests can exercise the comparison
/// without touching SQLite.
pub fn diff_snapshots(a: &ProvenanceSnapshot, b: &ProvenanceSnapshot) -> ProvenanceDiff {
    let mut changes = Vec::new();
    if a.model_id != b.model_id {
        changes.push(ProvenanceChange::Model {
            from: a.model_id.clone(),
            to: b.model_id.clone(),
        });
    }
    if a.policy_version != b.policy_version {
        changes.push(ProvenanceChange::Policy {
            from: a.policy_version.clone(),
            to: b.policy_version.clone(),
        });
    }
    diff_map(
        &a.skill_versions,
        &b.skill_versions,
        |name, version| ProvenanceChange::SkillAdded { name, version },
        |name, version| ProvenanceChange::SkillRemoved { name, version },
        |name, from, to| ProvenanceChange::SkillChanged { name, from, to },
        &mut changes,
    );
    diff_map(
        &a.tool_versions,
        &b.tool_versions,
        |name, version| ProvenanceChange::ToolAdded { name, version },
        |name, version| ProvenanceChange::ToolRemoved { name, version },
        |name, from, to| ProvenanceChange::ToolChanged { name, from, to },
        &mut changes,
    );
    ProvenanceDiff {
        a: a.trace_id.clone(),
        b: b.trace_id.clone(),
        changes,
    }
}

fn diff_map(
    a: &BTreeMap<String, String>,
    b: &BTreeMap<String, String>,
    on_added: impl Fn(String, String) -> ProvenanceChange,
    on_removed: impl Fn(String, String) -> ProvenanceChange,
    on_changed: impl Fn(String, String, String) -> ProvenanceChange,
    out: &mut Vec<ProvenanceChange>,
) {
    for (name, version) in b.iter() {
        if !a.contains_key(name) {
            out.push(on_added(name.clone(), version.clone()));
        }
    }
    for (name, version) in a.iter() {
        if !b.contains_key(name) {
            out.push(on_removed(name.clone(), version.clone()));
        }
    }
    for (name, av) in a.iter() {
        if let Some(bv) = b.get(name)
            && av != bv
        {
            out.push(on_changed(name.clone(), av.clone(), bv.clone()));
        }
    }
}

fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS provenance_snapshots (\
             trace_id         TEXT PRIMARY KEY,\
             timestamp_unix   INTEGER NOT NULL,\
             model_id         TEXT NOT NULL,\
             policy_version   TEXT NOT NULL,\
             skill_versions   TEXT NOT NULL,\
             tool_versions    TEXT NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS provenance_snapshots_ts \
             ON provenance_snapshots(timestamp_unix DESC);",
    )?;
    Ok(())
}

fn row_to_snapshot(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ProvenanceSnapshot, ProvenanceError>> {
    let trace_id: String = r.get(0)?;
    let timestamp_unix: i64 = r.get(1)?;
    let model_id: String = r.get(2)?;
    let policy_version: String = r.get(3)?;
    let skills_json: String = r.get(4)?;
    let tools_json: String = r.get(5)?;
    let parsed = (|| -> Result<ProvenanceSnapshot, ProvenanceError> {
        let skill_versions: BTreeMap<String, String> = serde_json::from_str(&skills_json)?;
        let tool_versions: BTreeMap<String, String> = serde_json::from_str(&tools_json)?;
        Ok(ProvenanceSnapshot {
            trace_id,
            timestamp_unix,
            model_id,
            policy_version,
            skill_versions,
            tool_versions,
        })
    })();
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(
        trace: &str,
        ts: i64,
        model: &str,
        policy: &str,
        skills: &[(&str, &str)],
        tools: &[(&str, &str)],
    ) -> ProvenanceSnapshot {
        ProvenanceSnapshot {
            trace_id: trace.into(),
            timestamp_unix: ts,
            model_id: model.into(),
            policy_version: policy.into(),
            skill_versions: skills
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            tool_versions: tools
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        }
    }

    #[test]
    fn record_and_get_round_trips() {
        let r = ProvenanceRegistry::in_memory().unwrap();
        let s = snap(
            "t1",
            100,
            "gpt-4o-2024-08-06",
            "policy-v3",
            &[("greet", "1.0.0"), ("plan", "2.1.0")],
            &[("web_search", "0.4.2")],
        );
        r.record(&s).unwrap();
        let got = r.get("t1").unwrap().expect("snapshot present");
        assert_eq!(got, s);
        assert!(r.get("nope").unwrap().is_none());
    }

    #[test]
    fn diff_flags_model_policy_skill_tool_changes() {
        let a = snap(
            "a",
            100,
            "gpt-4o-2024-08-06",
            "policy-v3",
            &[("greet", "1.0.0"), ("plan", "2.1.0"), ("retired", "0.1.0")],
            &[("web_search", "0.4.2"), ("calc", "1.0.0")],
        );
        let b = snap(
            "b",
            200,
            "gpt-4o-2024-11-20",
            "policy-v4",
            &[("greet", "1.0.0"), ("plan", "2.2.0"), ("fresh", "0.1.0")],
            &[("web_search", "0.5.0"), ("calc", "1.0.0")],
        );
        let d = diff_snapshots(&a, &b);
        assert_eq!(d.a, "a");
        assert_eq!(d.b, "b");
        assert!(d.changes.contains(&ProvenanceChange::Model {
            from: "gpt-4o-2024-08-06".into(),
            to: "gpt-4o-2024-11-20".into(),
        }));
        assert!(d.changes.contains(&ProvenanceChange::Policy {
            from: "policy-v3".into(),
            to: "policy-v4".into(),
        }));
        assert!(d.changes.contains(&ProvenanceChange::SkillAdded {
            name: "fresh".into(),
            version: "0.1.0".into(),
        }));
        assert!(d.changes.contains(&ProvenanceChange::SkillRemoved {
            name: "retired".into(),
            version: "0.1.0".into(),
        }));
        assert!(d.changes.contains(&ProvenanceChange::SkillChanged {
            name: "plan".into(),
            from: "2.1.0".into(),
            to: "2.2.0".into(),
        }));
        assert!(d.changes.contains(&ProvenanceChange::ToolChanged {
            name: "web_search".into(),
            from: "0.4.2".into(),
            to: "0.5.0".into(),
        }));
        // calc is unchanged → no entry.
        assert!(
            !d.changes
                .iter()
                .any(|c| matches!(c, ProvenanceChange::ToolChanged { name, .. } if name == "calc"))
        );
    }

    #[test]
    fn diff_of_identical_snapshots_is_empty() {
        let s = snap("x", 0, "m", "p", &[("s", "1.0")], &[("t", "1.0")]);
        let d = diff_snapshots(&s, &s);
        assert!(d.changes.is_empty());
    }

    #[test]
    fn registry_diff_surfaces_missing_traces() {
        let r = ProvenanceRegistry::in_memory().unwrap();
        let a = snap("a", 0, "m1", "p1", &[], &[]);
        r.record(&a).unwrap();
        let err = r.diff("a", "missing").unwrap_err();
        assert!(matches!(err, ProvenanceError::NotFound(ref t) if t == "missing"));
        let err = r.diff("missing", "a").unwrap_err();
        assert!(matches!(err, ProvenanceError::NotFound(ref t) if t == "missing"));
    }

    #[test]
    fn registry_diff_returns_change_list() {
        let r = ProvenanceRegistry::in_memory().unwrap();
        let a = snap("a", 0, "m1", "p1", &[], &[]);
        let b = snap("b", 10, "m2", "p1", &[], &[]);
        r.record(&a).unwrap();
        r.record(&b).unwrap();
        let d = r.diff("a", "b").unwrap();
        assert_eq!(d.changes.len(), 1);
        assert!(matches!(d.changes[0], ProvenanceChange::Model { .. }));
    }
}
