//! GAP 4 — SQLite-backed `SkillStore`.
//!
//! Persistent home for the auto-skill generator. Companion to the
//! file-based [`super::skills::SkillsCache`] which discovers
//! hand-authored SKILL.md files — this store owns the *learned*
//! skills, captured from successful task completions by the
//! `SkillExtractor`. The two surfaces coexist:
//!
//! - `SkillsCache`/`SkillMatcher` (existing): file-based, operator-
//!   curated, read into memory at controller boot.
//! - `SkillStore` (this module): SQLite-backed, learn-from-work,
//!   versioned, confidence-scored.
//!
//! Schema lives in [`Self::migrate`]; matches the spec exactly so
//! operators reading the GAP report can locate the columns by
//! name. Standard relix pragmas (WAL, FK on, NORMAL sync, 5s busy
//! timeout) apply via [`crate::db::apply_pragmas`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// One learned skill, materialised from the `skills` table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_agent: String,
    pub version: i64,
    pub confidence: f32,
    pub usage_count: i64,
    pub last_used_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub tags: Vec<String>,
    pub steps: Vec<SkillStep>,
    pub example_inputs: Vec<String>,
    pub example_outputs: Vec<String>,
    pub status: SkillStatus,
    /// Tenant-isolation work follow-up: per-tenant scoping. `None` on
    /// rows written before tenant isolation shipped + on rows written
    /// through the tenant-blind `insert` path. The
    /// `*_for_tenant` read methods filter by this column when the
    /// store was opened with `tenant_isolation = true`.
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// One step in a skill's procedure. Free-form `step` text + an
/// optional `tool` hint + an optional `prompt` template.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillStep {
    pub step: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Lifecycle status of a stored skill. `active` is the default;
/// `deprecated` rows are hidden from search but remain on disk;
/// `quarantined` rows are flagged by the integrity audit and
/// must be reviewed by an operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillStatus {
    Active,
    Deprecated,
    Quarantined,
}

impl SkillStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillStatus::Active => "active",
            SkillStatus::Deprecated => "deprecated",
            SkillStatus::Quarantined => "quarantined",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(SkillStatus::Active),
            "deprecated" => Some(SkillStatus::Deprecated),
            "quarantined" => Some(SkillStatus::Quarantined),
            _ => None,
        }
    }
}

/// One historical version of a skill. Inserted on every
/// refinement so the audit trail survives subsequent updates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillVersionRow {
    pub skill_id: String,
    pub version: i64,
    pub steps: Vec<SkillStep>,
    pub confidence: f32,
    pub updated_at_ms: i64,
    pub change_reason: Option<String>,
}

/// Filter applied to [`SkillStore::list`]. All fields optional;
/// `None` means "no constraint".
#[derive(Clone, Debug, Default)]
pub struct SkillFilter {
    pub agent: Option<String>,
    pub min_confidence: Option<f32>,
    pub status: Option<SkillStatus>,
    pub tag: Option<String>,
    pub limit: Option<usize>,
}

/// GAP 3: render a stored skill as a SKILL.md document body.
///
/// The output follows the Linux Foundation SKILL.md shape used
/// by [`crate::nodes::ai::skills::discover_skills`]: a top-level
/// `# <name>` heading, a short description paragraph, a
/// `## Procedure` ordered list of steps (with optional
/// `(tool: ...)` and indented prompt bodies), an optional
/// `## Examples` section, plus a trailing metadata block.
///
/// Pure function — no filesystem I/O. Use
/// [`write_stored_skill_md`] when you want the file written to
/// disk.
pub fn render_stored_skill_md(skill: &StoredSkill) -> String {
    let mut out = String::new();
    out.push_str("# ");
    out.push_str(skill.name.trim());
    out.push_str("\n\n");
    if !skill.description.trim().is_empty() {
        out.push_str(skill.description.trim());
        out.push_str("\n\n");
    }

    out.push_str("## Procedure\n\n");
    if skill.steps.is_empty() {
        out.push_str("_(no steps recorded)_\n\n");
    } else {
        for (idx, step) in skill.steps.iter().enumerate() {
            use std::fmt::Write as _;
            let header = match &step.tool {
                Some(t) if !t.trim().is_empty() => format!("(tool: `{}`) ", t.trim()),
                _ => String::new(),
            };
            let _ = writeln!(out, "{}. {}{}", idx + 1, header, step.step.trim());
            if let Some(prompt) = &step.prompt
                && !prompt.trim().is_empty()
            {
                for line in prompt.lines() {
                    out.push_str("   > ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out.push('\n');
    }

    if !skill.example_inputs.is_empty() || !skill.example_outputs.is_empty() {
        out.push_str("## Examples\n\n");
        let n = skill.example_inputs.len().max(skill.example_outputs.len());
        for i in 0..n {
            let input = skill
                .example_inputs
                .get(i)
                .map(String::as_str)
                .unwrap_or("—");
            let output = skill
                .example_outputs
                .get(i)
                .map(String::as_str)
                .unwrap_or("—");
            use std::fmt::Write as _;
            let _ = writeln!(out, "- **Input:** {input}");
            let _ = writeln!(out, "  **Output:** {output}");
        }
        out.push('\n');
    }

    out.push_str("## Metadata\n\n");
    use std::fmt::Write as _;
    let _ = writeln!(out, "- id: `{}`", skill.id);
    let _ = writeln!(out, "- source_agent: `{}`", skill.source_agent);
    let _ = writeln!(out, "- version: {}", skill.version);
    let _ = writeln!(out, "- confidence: {:.2}", skill.confidence);
    let _ = writeln!(out, "- usage_count: {}", skill.usage_count);
    let _ = writeln!(out, "- status: {}", skill.status.as_str());
    if !skill.tags.is_empty() {
        let _ = writeln!(out, "- tags: {}", skill.tags.join(", "));
    }
    out
}

/// GAP 3: write a stored skill out as a SKILL.md file. When
/// `path` is a directory the helper writes
/// `<dir>/{slug(skill.name)}.md`; when it's a regular-file path
/// the body is written there verbatim. Returns the final path
/// the body was written to. Parent directories are created if
/// missing.
pub fn write_stored_skill_md(
    path: &std::path::Path,
    skill: &StoredSkill,
) -> std::io::Result<std::path::PathBuf> {
    let body = render_stored_skill_md(skill);
    let target = if path.is_dir() {
        let slug = crate::nodes::ai::skills::slugify_for_filename(&skill.name);
        path.join(format!("{slug}.md"))
    } else {
        path.to_path_buf()
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, body)?;
    Ok(target)
}

#[derive(Debug, thiserror::Error)]
pub enum SkillStoreError {
    #[error("skill store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("skill store: json: {0}")]
    Json(String),
    #[error("skill store: skill `{0}` not found")]
    NotFound(String),
    #[error("skill store: invalid status `{0}`")]
    InvalidStatus(String),
    /// Tenant-isolation: returned by `*_for_tenant` methods
    /// when the store was opened with
    /// `tenant_isolation = true` but the caller did not
    /// supply a tenant id.
    #[error("skill store: tenant_id required in multi-tenant mode")]
    MissingTenant,
}

impl From<serde_json::Error> for SkillStoreError {
    fn from(e: serde_json::Error) -> Self {
        SkillStoreError::Json(e.to_string())
    }
}

/// Cheap-to-clone SQLite-backed skill store.
#[derive(Clone)]
pub struct SkillStore {
    conn: Arc<Mutex<Connection>>,
    /// Tenant-isolation: when `true`, the `*_for_tenant`
    /// read methods fail closed on a missing tenant id AND
    /// every read filters `WHERE tenant_id = ?`. The
    /// pre-isolation tenant-blind methods (`get`, `list`,
    /// `search`) still exist for callers that have not yet
    /// migrated; they continue to ignore tenant. Mirrors the
    /// `LayeredMemoryStore::tenant_isolation` flag added in
    /// the PART 4 tenant-isolation work.
    tenant_isolation: bool,
}

impl SkillStore {
    /// Open (or create) the store at `path`. Applies the standard
    /// pragmas, runs migrations, runs an integrity check, and
    /// returns the wrapped connection. Tenant isolation defaults
    /// to OFF; callers opt in via
    /// [`Self::open_with_tenant_isolation`].
    pub fn open(path: &Path) -> Result<Self, SkillStoreError> {
        Self::open_with_tenant_isolation(path, false)
    }

    /// Tenant-isolation variant. When `tenant_isolation = true`,
    /// the `*_for_tenant` read methods fail closed on a missing
    /// tenant id AND apply `WHERE tenant_id = ?` to every read.
    pub fn open_with_tenant_isolation(
        path: &Path,
        tenant_isolation: bool,
    ) -> Result<Self, SkillStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "skills");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tenant_isolation,
        })
    }

    /// In-memory store. Tests + callers that want a transient
    /// skill library use this.
    pub fn open_in_memory() -> Result<Self, SkillStoreError> {
        Self::open_in_memory_with_tenant_isolation(false)
    }

    /// In-memory tenant-isolation variant. Same fail-closed +
    /// `WHERE tenant_id = ?` semantics as
    /// [`Self::open_with_tenant_isolation`].
    pub fn open_in_memory_with_tenant_isolation(
        tenant_isolation: bool,
    ) -> Result<Self, SkillStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tenant_isolation,
        })
    }

    /// `true` when this store enforces per-tenant filtering on
    /// the `*_for_tenant` read methods.
    pub fn tenant_isolation_enabled(&self) -> bool {
        self.tenant_isolation
    }

    fn migrate(conn: &Connection) -> Result<(), SkillStoreError> {
        crate::db::ensure_migration_table(conn)?;
        let current = crate::db::current_migration_version(conn)?;
        if current < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS skills (\
                     id                    TEXT PRIMARY KEY,\
                     name                  TEXT NOT NULL,\
                     description           TEXT NOT NULL,\
                     source_agent          TEXT NOT NULL,\
                     version               INTEGER NOT NULL DEFAULT 1,\
                     confidence            REAL NOT NULL DEFAULT 0.5,\
                     usage_count           INTEGER NOT NULL DEFAULT 0,\
                     last_used_ms          INTEGER,\
                     created_at_ms         INTEGER NOT NULL,\
                     updated_at_ms         INTEGER NOT NULL,\
                     tags                  TEXT NOT NULL DEFAULT '[]',\
                     steps_json            TEXT NOT NULL,\
                     example_inputs_json   TEXT NOT NULL DEFAULT '[]',\
                     example_outputs_json  TEXT NOT NULL DEFAULT '[]',\
                     status                TEXT NOT NULL DEFAULT 'active'\
                 );\
                 CREATE INDEX IF NOT EXISTS skills_agent_idx \
                     ON skills(source_agent);\
                 CREATE INDEX IF NOT EXISTS skills_status_idx \
                     ON skills(status);\
                 CREATE TABLE IF NOT EXISTS skill_versions (\
                     skill_id      TEXT NOT NULL,\
                     version       INTEGER NOT NULL,\
                     steps_json    TEXT NOT NULL,\
                     confidence    REAL NOT NULL,\
                     updated_at_ms INTEGER NOT NULL,\
                     change_reason TEXT,\
                     PRIMARY KEY (skill_id, version)\
                 );",
            )?;
            crate::db::record_migration_applied(conn, 1)?;
        }
        if current < 2 {
            // Tenant-isolation follow-up: per-tenant scoping
            // column + partial index for tenant-filtered reads.
            // The column is nullable so pre-migration rows
            // continue to read fine through the tenant-blind
            // `get` / `list` / `search` methods. The
            // `*_for_tenant` variants apply `WHERE tenant_id = ?`.
            if !column_exists(conn, "skills", "tenant_id")? {
                conn.execute("ALTER TABLE skills ADD COLUMN tenant_id TEXT", [])?;
            }
            conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_skills_tenant \
                     ON skills(tenant_id) WHERE tenant_id IS NOT NULL;",
            )?;
            crate::db::record_migration_applied(conn, 2)?;
        }
        Ok(())
    }

    /// Insert a new skill. The first version is also recorded in
    /// `skill_versions` so the audit trail is non-empty from the
    /// moment a skill is born.
    pub fn insert(&self, skill: &StoredSkill) -> Result<(), SkillStoreError> {
        let tags_json = serde_json::to_string(&skill.tags)?;
        let steps_json = serde_json::to_string(&skill.steps)?;
        let inputs_json = serde_json::to_string(&skill.example_inputs)?;
        let outputs_json = serde_json::to_string(&skill.example_outputs)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO skills \
             (id, name, description, source_agent, version, confidence, \
              usage_count, last_used_ms, created_at_ms, updated_at_ms, \
              tags, steps_json, example_inputs_json, example_outputs_json, \
              status, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                skill.id,
                skill.name,
                skill.description,
                skill.source_agent,
                skill.version,
                skill.confidence as f64,
                skill.usage_count,
                skill.last_used_ms,
                skill.created_at_ms,
                skill.updated_at_ms,
                tags_json,
                steps_json,
                inputs_json,
                outputs_json,
                skill.status.as_str(),
                skill.tenant_id,
            ],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO skill_versions \
             (skill_id, version, steps_json, confidence, updated_at_ms, change_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                skill.id,
                skill.version,
                steps_json,
                skill.confidence as f64,
                skill.updated_at_ms,
                "initial insert"
            ],
        )?;
        Ok(())
    }

    /// Look up one skill by id. Returns `None` when no row
    /// exists; never returns `NotFound` (the caller decides how
    /// to handle absence).
    pub fn get(&self, id: &str) -> Result<Option<StoredSkill>, SkillStoreError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status \
                 FROM skills WHERE id = ?1",
                params![id],
                row_to_skill,
            )
            .optional()?;
        match row {
            Some(Ok(s)) => Ok(Some(s)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// List skills matching `filter`. Returns newest-first;
    /// callers that need a different sort sort the returned
    /// vec.
    pub fn list(&self, filter: &SkillFilter) -> Result<Vec<StoredSkill>, SkillStoreError> {
        let mut sql = String::from(
            "SELECT id, name, description, source_agent, version, \
                    confidence, usage_count, last_used_ms, created_at_ms, \
                    updated_at_ms, tags, steps_json, example_inputs_json, \
                    example_outputs_json, status \
             FROM skills WHERE 1=1",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(a) = &filter.agent {
            sql.push_str(" AND source_agent = ?");
            args.push(Box::new(a.clone()));
        }
        if let Some(c) = filter.min_confidence {
            sql.push_str(" AND confidence >= ?");
            args.push(Box::new(c as f64));
        }
        if let Some(st) = filter.status {
            sql.push_str(" AND status = ?");
            args.push(Box::new(st.as_str().to_string()));
        }
        if let Some(t) = &filter.tag {
            // tags column is a JSON array; substring match is
            // good enough for the alpha and dodges json1
            // extension dependency.
            sql.push_str(" AND tags LIKE ?");
            args.push(Box::new(format!("%\"{t}\"%")));
        }
        sql.push_str(" ORDER BY created_at_ms DESC, id ASC");
        if let Some(l) = filter.limit {
            sql.push_str(" LIMIT ?");
            args.push(Box::new(l as i64));
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let rows = stmt.query_map(arg_refs.as_slice(), row_to_skill)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Simple substring search over name + description + tags.
    /// Excludes `deprecated` rows. Case-insensitive. The alpha
    /// does NOT use FTS5 here because the skill catalogue is
    /// small (hundreds, not millions) and the operator-facing
    /// CLI/HTTP surface wants forgiving substring semantics, not
    /// strict token matching.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        min_confidence: Option<f32>,
        agent: Option<&str>,
    ) -> Result<Vec<StoredSkill>, SkillStoreError> {
        let q = query.trim();
        let needle = format!("%{}%", q.to_lowercase());
        let limit_i: i64 = limit.clamp(1, 500) as i64;
        let conn = self.lock();
        let (sql, agent_filter) = match agent {
            Some(_) => (
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status \
                 FROM skills \
                 WHERE status != 'deprecated' \
                   AND source_agent = ?4 \
                   AND (lower(name) LIKE ?1 OR lower(description) LIKE ?1 OR lower(tags) LIKE ?1) \
                   AND confidence >= ?2 \
                 ORDER BY confidence DESC, usage_count DESC, id ASC \
                 LIMIT ?3",
                true,
            ),
            None => (
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status \
                 FROM skills \
                 WHERE status != 'deprecated' \
                   AND (lower(name) LIKE ?1 OR lower(description) LIKE ?1 OR lower(tags) LIKE ?1) \
                   AND confidence >= ?2 \
                 ORDER BY confidence DESC, usage_count DESC, id ASC \
                 LIMIT ?3",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let min_conf = min_confidence.unwrap_or(0.0) as f64;
        let rows = if agent_filter {
            stmt.query_map(
                params![needle, min_conf, limit_i, agent.unwrap_or("")],
                row_to_skill,
            )?
            .collect::<Vec<_>>()
        } else {
            stmt.query_map(params![needle, min_conf, limit_i], row_to_skill)?
                .collect::<Vec<_>>()
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Tenant-isolation: tenant-aware variant of [`Self::get`].
    /// Returns the row only when its `tenant_id` matches;
    /// returns `Ok(None)` (not Err) when the row exists but
    /// belongs to a different tenant — a leaked id must not
    /// reveal cross-tenant existence. When the store was
    /// opened with `tenant_isolation = false`, this falls
    /// through to [`Self::get`] so callers can migrate
    /// incrementally.
    pub fn get_for_tenant(
        &self,
        id: &str,
        tenant_id: Option<&str>,
    ) -> Result<Option<StoredSkill>, SkillStoreError> {
        if !self.tenant_isolation {
            return self.get(id);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err(SkillStoreError::MissingTenant),
        };
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status, tenant_id \
                 FROM skills WHERE id = ?1 AND tenant_id = ?2",
                params![id, tenant],
                row_to_skill_with_tenant,
            )
            .optional()?;
        match row {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Tenant-isolation: tenant-aware variant of
    /// [`Self::search`]. Adds `WHERE tenant_id = ?` to the
    /// SQL so cross-tenant rows are NEVER returned. Same
    /// fail-closed-on-missing-tenant semantics as
    /// [`Self::get_for_tenant`].
    pub fn search_for_tenant(
        &self,
        query: &str,
        limit: usize,
        min_confidence: Option<f32>,
        agent: Option<&str>,
        tenant_id: Option<&str>,
    ) -> Result<Vec<StoredSkill>, SkillStoreError> {
        if !self.tenant_isolation {
            return self.search(query, limit, min_confidence, agent);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err(SkillStoreError::MissingTenant),
        };
        let q = query.trim();
        let needle = format!("%{}%", q.to_lowercase());
        let limit_i: i64 = limit.clamp(1, 500) as i64;
        let min_conf = min_confidence.unwrap_or(0.0) as f64;
        let conn = self.lock();
        let (sql, has_agent) = match agent {
            Some(_) => (
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status, tenant_id \
                 FROM skills \
                 WHERE tenant_id = ?1 \
                   AND status != 'deprecated' \
                   AND confidence >= ?2 \
                   AND (LOWER(name) LIKE ?3 \
                        OR LOWER(description) LIKE ?3 \
                        OR LOWER(tags) LIKE ?3) \
                   AND source_agent = ?5 \
                 ORDER BY usage_count DESC, confidence DESC \
                 LIMIT ?4",
                true,
            ),
            None => (
                "SELECT id, name, description, source_agent, version, \
                        confidence, usage_count, last_used_ms, created_at_ms, \
                        updated_at_ms, tags, steps_json, example_inputs_json, \
                        example_outputs_json, status, tenant_id \
                 FROM skills \
                 WHERE tenant_id = ?1 \
                   AND status != 'deprecated' \
                   AND confidence >= ?2 \
                   AND (LOWER(name) LIKE ?3 \
                        OR LOWER(description) LIKE ?3 \
                        OR LOWER(tags) LIKE ?3) \
                 ORDER BY usage_count DESC, confidence DESC \
                 LIMIT ?4",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if has_agent {
            stmt.query_map(
                params![tenant, min_conf, needle, limit_i, agent.unwrap_or("")],
                row_to_skill_with_tenant,
            )?
            .collect::<Vec<_>>()
        } else {
            stmt.query_map(
                params![tenant, min_conf, needle, limit_i],
                row_to_skill_with_tenant,
            )?
            .collect::<Vec<_>>()
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Tenant-isolation: tenant-aware variant of
    /// [`Self::list`]. Adds `WHERE tenant_id = ?` to the
    /// generated SQL. Same fail-closed-on-missing-tenant
    /// semantics as [`Self::get_for_tenant`].
    pub fn list_for_tenant(
        &self,
        filter: &SkillFilter,
        tenant_id: Option<&str>,
    ) -> Result<Vec<StoredSkill>, SkillStoreError> {
        if !self.tenant_isolation {
            return self.list(filter);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t.to_string(),
            _ => return Err(SkillStoreError::MissingTenant),
        };
        let mut sql = String::from(
            "SELECT id, name, description, source_agent, version, \
                    confidence, usage_count, last_used_ms, created_at_ms, \
                    updated_at_ms, tags, steps_json, example_inputs_json, \
                    example_outputs_json, status, tenant_id \
             FROM skills WHERE tenant_id = ?",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        args.push(Box::new(tenant));
        if let Some(a) = &filter.agent {
            sql.push_str(" AND source_agent = ?");
            args.push(Box::new(a.clone()));
        }
        if let Some(c) = filter.min_confidence {
            sql.push_str(" AND confidence >= ?");
            args.push(Box::new(c as f64));
        }
        if let Some(st) = filter.status {
            sql.push_str(" AND status = ?");
            args.push(Box::new(st.as_str().to_string()));
        }
        if let Some(t) = &filter.tag {
            sql.push_str(" AND tags LIKE ?");
            args.push(Box::new(format!("%\"{t}\"%")));
        }
        sql.push_str(" ORDER BY created_at_ms DESC, id ASC");
        if let Some(l) = filter.limit {
            sql.push_str(" LIMIT ?");
            args.push(Box::new(l as i64));
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let arg_refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();
        let rows = stmt.query_map(arg_refs.as_slice(), row_to_skill_with_tenant)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Update one skill's confidence value. Clamped to
    /// `[0.05, 0.95]` so it can't drift to 0/1 and confuse
    /// downstream consumers expecting a non-extreme float.
    pub fn update_confidence(&self, id: &str, new_confidence: f32) -> Result<(), SkillStoreError> {
        let clamped = new_confidence.clamp(0.05, 0.95) as f64;
        let now = unix_millis();
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE skills SET confidence = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![clamped, now, id],
        )?;
        if n == 0 {
            return Err(SkillStoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Bump `usage_count` and stamp `last_used_ms`. Used by the
    /// confidence-scoring path on every applied skill.
    pub fn increment_usage(&self, id: &str) -> Result<(), SkillStoreError> {
        let now = unix_millis();
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE skills SET usage_count = usage_count + 1, \
                                last_used_ms = ?1, updated_at_ms = ?1 \
             WHERE id = ?2",
            params![now, id],
        )?;
        if n == 0 {
            return Err(SkillStoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Persist a refined set of steps as a new version. Writes
    /// the new row to `skill_versions` AND updates `skills`
    /// (steps_json + version + updated_at_ms) in a single
    /// transaction so a crash mid-write can't leave the two
    /// tables disagreeing.
    pub fn add_version(
        &self,
        skill_id: &str,
        new_steps: &[SkillStep],
        change_reason: Option<&str>,
    ) -> Result<i64, SkillStoreError> {
        let steps_json = serde_json::to_string(new_steps)?;
        let now = unix_millis();
        let mut conn_owned = self.lock();
        let tx = conn_owned.transaction()?;
        let current_version: Option<i64> = tx
            .query_row(
                "SELECT version FROM skills WHERE id = ?1",
                params![skill_id],
                |r| r.get(0),
            )
            .optional()?;
        let current = match current_version {
            Some(v) => v,
            None => return Err(SkillStoreError::NotFound(skill_id.to_string())),
        };
        let new_version = current + 1;
        let confidence: f64 = tx.query_row(
            "SELECT confidence FROM skills WHERE id = ?1",
            params![skill_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO skill_versions \
             (skill_id, version, steps_json, confidence, updated_at_ms, change_reason) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                skill_id,
                new_version,
                steps_json,
                confidence,
                now,
                change_reason
            ],
        )?;
        tx.execute(
            "UPDATE skills SET version = ?1, steps_json = ?2, updated_at_ms = ?3 \
             WHERE id = ?4",
            params![new_version, steps_json, now, skill_id],
        )?;
        tx.commit()?;
        Ok(new_version)
    }

    /// Update arbitrary mutable fields on a skill. `None`
    /// arguments leave the corresponding column untouched. When
    /// `steps` is supplied, a new version is recorded just like
    /// [`Self::add_version`] does — operators using the inspector
    /// surface to fix a buggy skill get the same audit trail the
    /// refinement engine produces.
    pub fn update(
        &self,
        id: &str,
        description: Option<&str>,
        tags: Option<&[String]>,
        steps: Option<&[SkillStep]>,
        status: Option<SkillStatus>,
        change_reason: Option<&str>,
    ) -> Result<(), SkillStoreError> {
        let now = unix_millis();
        let mut conn_owned = self.lock();
        let tx = conn_owned.transaction()?;
        let current_version: Option<i64> = tx
            .query_row(
                "SELECT version FROM skills WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let current_version = match current_version {
            Some(v) => v,
            None => return Err(SkillStoreError::NotFound(id.to_string())),
        };
        if let Some(d) = description {
            tx.execute(
                "UPDATE skills SET description = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![d, now, id],
            )?;
        }
        if let Some(t) = tags {
            let tags_json = serde_json::to_string(t)?;
            tx.execute(
                "UPDATE skills SET tags = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![tags_json, now, id],
            )?;
        }
        if let Some(st) = status {
            tx.execute(
                "UPDATE skills SET status = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![st.as_str(), now, id],
            )?;
        }
        if let Some(new_steps) = steps {
            let steps_json = serde_json::to_string(new_steps)?;
            let new_version = current_version + 1;
            let confidence: f64 = tx.query_row(
                "SELECT confidence FROM skills WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO skill_versions \
                 (skill_id, version, steps_json, confidence, updated_at_ms, change_reason) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, new_version, steps_json, confidence, now, change_reason],
            )?;
            tx.execute(
                "UPDATE skills SET version = ?1, steps_json = ?2, updated_at_ms = ?3 \
                 WHERE id = ?4",
                params![new_version, steps_json, now, id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Add up to 3 example inputs / outputs to a skill — used by
    /// the extractor (on the first hit) and the confidence-
    /// scoring path (when a skill is successfully applied). Older
    /// examples are evicted FIFO to keep the on-disk payload
    /// bounded.
    pub fn record_example(
        &self,
        id: &str,
        input: &str,
        output: &str,
        max_examples: usize,
    ) -> Result<(), SkillStoreError> {
        let conn = self.lock();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT example_inputs_json, example_outputs_json \
                 FROM skills WHERE id = ?1",
                params![id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        let (inputs_json, outputs_json) = match row {
            Some(t) => t,
            None => return Err(SkillStoreError::NotFound(id.to_string())),
        };
        let mut inputs: Vec<String> = serde_json::from_str(&inputs_json).unwrap_or_default();
        let mut outputs: Vec<String> = serde_json::from_str(&outputs_json).unwrap_or_default();
        inputs.push(input.to_string());
        outputs.push(output.to_string());
        let cap = max_examples.max(1);
        while inputs.len() > cap {
            inputs.remove(0);
        }
        while outputs.len() > cap {
            outputs.remove(0);
        }
        let new_inputs = serde_json::to_string(&inputs)?;
        let new_outputs = serde_json::to_string(&outputs)?;
        let now = unix_millis();
        conn.execute(
            "UPDATE skills SET example_inputs_json = ?1, example_outputs_json = ?2, updated_at_ms = ?3 \
             WHERE id = ?4",
            params![new_inputs, new_outputs, now, id],
        )?;
        Ok(())
    }

    /// Return every version row for a skill, oldest first. Used
    /// by `relix skills show <id>` and `GET /v1/skills/:id`.
    pub fn versions(&self, skill_id: &str) -> Result<Vec<SkillVersionRow>, SkillStoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT skill_id, version, steps_json, confidence, updated_at_ms, change_reason \
             FROM skill_versions WHERE skill_id = ?1 \
             ORDER BY version ASC",
        )?;
        let rows = stmt.query_map(params![skill_id], |r| {
            let steps_json: String = r.get(2)?;
            let steps: Vec<SkillStep> = serde_json::from_str(&steps_json).unwrap_or_default();
            Ok(SkillVersionRow {
                skill_id: r.get(0)?,
                version: r.get(1)?,
                steps,
                confidence: r.get::<_, f64>(3)? as f32,
                updated_at_ms: r.get(4)?,
                change_reason: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Aggregate statistics: counts, average confidence, top 5
    /// by usage. One pass over the table — the alpha's skill
    /// catalogue stays well under 100k rows so this is cheap.
    pub fn stats(&self) -> Result<SkillStats, SkillStoreError> {
        let conn = self.lock();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))?;
        let active: i64 = conn.query_row(
            "SELECT COUNT(*) FROM skills WHERE status = 'active'",
            [],
            |r| r.get(0),
        )?;
        let avg_conf: Option<f64> = conn
            .query_row(
                "SELECT AVG(confidence) FROM skills WHERE status = 'active'",
                [],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()?
            .flatten();
        let mut top_stmt = conn.prepare(
            "SELECT id, name, description, source_agent, version, \
                    confidence, usage_count, last_used_ms, created_at_ms, \
                    updated_at_ms, tags, steps_json, example_inputs_json, \
                    example_outputs_json, status \
             FROM skills \
             WHERE status = 'active' \
             ORDER BY usage_count DESC, confidence DESC \
             LIMIT 5",
        )?;
        let top_rows = top_stmt.query_map([], row_to_skill)?;
        let mut top_5_by_usage = Vec::new();
        for r in top_rows {
            top_5_by_usage.push(r??);
        }
        let mut recent_stmt = conn.prepare(
            "SELECT id, name, description, source_agent, version, \
                    confidence, usage_count, last_used_ms, created_at_ms, \
                    updated_at_ms, tags, steps_json, example_inputs_json, \
                    example_outputs_json, status \
             FROM skills \
             ORDER BY created_at_ms DESC \
             LIMIT 5",
        )?;
        let recent_rows = recent_stmt.query_map([], row_to_skill)?;
        let mut recently_created = Vec::new();
        for r in recent_rows {
            recently_created.push(r??);
        }
        Ok(SkillStats {
            total_skills: total as usize,
            active_skills: active as usize,
            avg_confidence: avg_conf.unwrap_or(0.0) as f32,
            top_5_by_usage,
            recently_created,
        })
    }

    /// List skills eligible for refinement: status=active,
    /// usage_count >= `min_usage`, confidence >= `min_confidence`.
    /// Used by the background refinement task.
    pub fn list_refinement_candidates(
        &self,
        min_usage: i64,
        min_confidence: f32,
        limit: usize,
    ) -> Result<Vec<StoredSkill>, SkillStoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, description, source_agent, version, \
                    confidence, usage_count, last_used_ms, created_at_ms, \
                    updated_at_ms, tags, steps_json, example_inputs_json, \
                    example_outputs_json, status \
             FROM skills \
             WHERE status = 'active' \
               AND usage_count >= ?1 \
               AND confidence >= ?2 \
             ORDER BY usage_count DESC \
             LIMIT ?3",
        )?;
        let limit_i: i64 = limit.clamp(1, 1000) as i64;
        let rows = stmt.query_map(
            params![min_usage, min_confidence as f64, limit_i],
            row_to_skill,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Aggregate counts surfaced by `memory.skill_stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillStats {
    pub total_skills: usize,
    pub active_skills: usize,
    pub avg_confidence: f32,
    pub top_5_by_usage: Vec<StoredSkill>,
    pub recently_created: Vec<StoredSkill>,
}

/// Returns `true` when `column` exists on `table` in the
/// connection's current schema. Used by the tenant-isolation
/// migration to make the ALTER TABLE idempotent.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, SkillStoreError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn row_to_skill(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<StoredSkill, SkillStoreError>> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let description: String = row.get(2)?;
    let source_agent: String = row.get(3)?;
    let version: i64 = row.get(4)?;
    let confidence: f64 = row.get(5)?;
    let usage_count: i64 = row.get(6)?;
    let last_used_ms: Option<i64> = row.get(7)?;
    let created_at_ms: i64 = row.get(8)?;
    let updated_at_ms: i64 = row.get(9)?;
    let tags_json: String = row.get(10)?;
    let steps_json: String = row.get(11)?;
    let inputs_json: String = row.get(12)?;
    let outputs_json: String = row.get(13)?;
    let status_str: String = row.get(14)?;
    let parse = || -> Result<StoredSkill, SkillStoreError> {
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let steps: Vec<SkillStep> = serde_json::from_str(&steps_json)?;
        let example_inputs: Vec<String> = serde_json::from_str(&inputs_json).unwrap_or_default();
        let example_outputs: Vec<String> = serde_json::from_str(&outputs_json).unwrap_or_default();
        let status = SkillStatus::parse(&status_str)
            .ok_or_else(|| SkillStoreError::InvalidStatus(status_str.clone()))?;
        Ok(StoredSkill {
            id,
            name,
            description,
            source_agent,
            version,
            confidence: confidence as f32,
            usage_count,
            last_used_ms,
            created_at_ms,
            updated_at_ms,
            tags,
            steps,
            example_inputs,
            example_outputs,
            status,
            // Tenant-isolation legacy path: the tenant-blind
            // SELECTs don't read the column, so we expose
            // None. The `*_for_tenant` methods use a separate
            // decoder that includes the tenant_id column.
            tenant_id: None,
        })
    };
    Ok(parse())
}

/// Tenant-aware row decoder. Same shape as [`row_to_skill`]
/// but expects column 15 to be the `tenant_id`. Used by the
/// `*_for_tenant` SELECTs which explicitly include the
/// column.
fn row_to_skill_with_tenant(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<StoredSkill, SkillStoreError>> {
    let inner = row_to_skill(row)?;
    let tenant: Option<String> = row.get(15)?;
    Ok(inner.map(|mut s| {
        s.tenant_id = tenant;
        s
    }))
}

/// Mint a new skill id. UUIDv4 over a blake3 hash of the source
/// agent + name + current timestamp + 64 random bits. Avoids
/// pulling the `uuid` crate (already pulled transitively, but
/// the alpha keeps direct deps lean) and stays collision-safe
/// for the catalogue sizes operators will see.
pub fn mint_skill_id(source_agent: &str, name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source_agent.as_bytes());
    hasher.update(b"|");
    hasher.update(name.as_bytes());
    hasher.update(b"|");
    hasher.update(unix_millis().to_le_bytes().as_ref());
    let mut rnd: [u8; 8] = [0; 8];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut rnd);
    hasher.update(&rnd);
    let hex = hex::encode(&hasher.finalize().as_bytes()[..16]);
    // Render as 8-4-4-4-12 (UUID-ish) so operators can paste the
    // id into UUID-shaped fields without choking.
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, name: &str) -> StoredSkill {
        StoredSkill {
            id: id.to_string(),
            name: name.to_string(),
            description: format!("desc for {name}"),
            source_agent: "agent.alpha".into(),
            version: 1,
            confidence: 0.5,
            usage_count: 0,
            last_used_ms: None,
            created_at_ms: 1_000_000,
            updated_at_ms: 1_000_000,
            tags: vec!["tag1".into(), "rust".into()],
            steps: vec![
                SkillStep {
                    step: "fetch the data".into(),
                    tool: Some("http.get".into()),
                    prompt: None,
                },
                SkillStep {
                    step: "summarise".into(),
                    tool: None,
                    prompt: Some("summarise this in 3 bullets".into()),
                },
            ],
            example_inputs: vec!["fetch the latest".into()],
            example_outputs: vec!["here are the latest".into()],
            status: SkillStatus::Active,
            tenant_id: None,
        }
    }

    // ---- GAP 3: render_stored_skill_md + write_stored_skill_md ----

    #[test]
    fn render_stored_skill_md_contains_every_section() {
        let s = sample("id-1", "fetch_and_summarise");
        let md = render_stored_skill_md(&s);
        assert!(md.starts_with("# fetch_and_summarise"));
        assert!(md.contains("## Procedure"));
        assert!(md.contains("(tool: `http.get`)"));
        assert!(md.contains("1. (tool: `http.get`) fetch the data"));
        assert!(md.contains("2. summarise"));
        assert!(md.contains("   > summarise this in 3 bullets"));
        assert!(md.contains("## Examples"));
        assert!(md.contains("**Input:** fetch the latest"));
        assert!(md.contains("**Output:** here are the latest"));
        assert!(md.contains("## Metadata"));
        assert!(md.contains("- id: `id-1`"));
        assert!(md.contains("- source_agent: `agent.alpha`"));
        assert!(md.contains("- confidence: 0.50"));
        assert!(md.contains("- status: active"));
        assert!(md.contains("- tags: tag1, rust"));
    }

    #[test]
    fn render_stored_skill_md_handles_empty_steps_and_examples_gracefully() {
        let mut s = sample("id-2", "empty_skill");
        s.steps.clear();
        s.example_inputs.clear();
        s.example_outputs.clear();
        s.tags.clear();
        let md = render_stored_skill_md(&s);
        assert!(md.contains("_(no steps recorded)_"));
        // No `## Examples` heading when both example lists are
        // empty.
        assert!(!md.contains("## Examples"));
        // Tags line omitted when empty.
        assert!(!md.contains("- tags:"));
    }

    #[test]
    fn write_stored_skill_md_creates_a_slugged_file_when_target_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let s = sample("id-3", "Fetch and Summarise!");
        let out = write_stored_skill_md(dir.path(), &s).unwrap();
        assert_eq!(out.parent().unwrap(), dir.path());
        let stem = out.file_stem().unwrap().to_string_lossy().to_string();
        assert!(
            stem.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "stem {stem:?} should be slugified"
        );
        assert!(out.extension().and_then(|x| x.to_str()) == Some("md"));
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.starts_with("# Fetch and Summarise!"));
    }

    #[test]
    fn write_stored_skill_md_respects_explicit_file_path_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested/dir/MY_SKILL.md");
        let s = sample("id-4", "explicit_path");
        let out = write_stored_skill_md(&target, &s).unwrap();
        assert_eq!(out, target);
        assert!(target.exists());
    }

    #[test]
    fn insert_then_get_round_trips_every_field() {
        let store = SkillStore::open_in_memory().unwrap();
        let s = sample("id-1", "fetch_and_summarise");
        store.insert(&s).unwrap();
        let got = store.get("id-1").unwrap().expect("present");
        assert_eq!(got, s);
    }

    #[test]
    fn get_returns_none_for_missing() {
        let store = SkillStore::open_in_memory().unwrap();
        assert!(store.get("nope").unwrap().is_none());
    }

    #[test]
    fn list_with_no_filter_returns_all() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        store.insert(&sample("b", "beta")).unwrap();
        let rows = store.list(&SkillFilter::default()).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn list_filters_by_min_confidence() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut high = sample("a", "alpha");
        high.confidence = 0.9;
        let mut low = sample("b", "beta");
        low.confidence = 0.3;
        store.insert(&high).unwrap();
        store.insert(&low).unwrap();
        let rows = store
            .list(&SkillFilter {
                min_confidence: Some(0.7),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "a");
    }

    #[test]
    fn list_filters_by_agent() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut a = sample("a", "alpha");
        a.source_agent = "agent.x".into();
        let mut b = sample("b", "beta");
        b.source_agent = "agent.y".into();
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        let rows = store
            .list(&SkillFilter {
                agent: Some("agent.y".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "b");
    }

    #[test]
    fn search_matches_name_description_or_tag() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "deploy_to_prod")).unwrap();
        store.insert(&sample("b", "fetch_data")).unwrap();
        let hits = store.search("deploy", 10, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn search_with_min_confidence_excludes_low_rows() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut high = sample("a", "deploy_prod");
        high.confidence = 0.9;
        let mut low = sample("b", "deploy_dev");
        low.confidence = 0.2;
        store.insert(&high).unwrap();
        store.insert(&low).unwrap();
        let hits = store.search("deploy", 10, Some(0.5), None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn search_excludes_deprecated_skills() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut a = sample("a", "deploy_prod");
        a.status = SkillStatus::Deprecated;
        store.insert(&a).unwrap();
        store.insert(&sample("b", "deploy_dev")).unwrap();
        let hits = store.search("deploy", 10, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b");
    }

    #[test]
    fn update_confidence_writes_clamped_value() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        // Above ceiling — clamped to 0.95.
        store.update_confidence("a", 1.5).unwrap();
        let s = store.get("a").unwrap().unwrap();
        assert!((s.confidence - 0.95).abs() < 1e-6);
        // Below floor — clamped to 0.05.
        store.update_confidence("a", -0.5).unwrap();
        let s = store.get("a").unwrap().unwrap();
        assert!((s.confidence - 0.05).abs() < 1e-6);
    }

    #[test]
    fn update_confidence_on_missing_id_errors() {
        let store = SkillStore::open_in_memory().unwrap();
        let err = store.update_confidence("ghost", 0.7).unwrap_err();
        assert!(matches!(err, SkillStoreError::NotFound(_)));
    }

    #[test]
    fn increment_usage_increments_and_stamps_last_used() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        store.increment_usage("a").unwrap();
        let s = store.get("a").unwrap().unwrap();
        assert_eq!(s.usage_count, 1);
        assert!(s.last_used_ms.is_some());
    }

    #[test]
    fn add_version_creates_new_version_row_and_bumps_version() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        let new_steps = vec![SkillStep {
            step: "refined step".into(),
            tool: None,
            prompt: None,
        }];
        let new_ver = store
            .add_version("a", &new_steps, Some("test refinement"))
            .unwrap();
        assert_eq!(new_ver, 2);
        let s = store.get("a").unwrap().unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.steps, new_steps);
        let versions = store.versions("a").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[1].version, 2);
        assert_eq!(
            versions[1].change_reason.as_deref(),
            Some("test refinement")
        );
    }

    #[test]
    fn versions_returns_chronological_history() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        let v2 = vec![SkillStep {
            step: "v2".into(),
            tool: None,
            prompt: None,
        }];
        let v3 = vec![SkillStep {
            step: "v3".into(),
            tool: None,
            prompt: None,
        }];
        store
            .add_version("a", &v2, Some("first refinement"))
            .unwrap();
        store
            .add_version("a", &v3, Some("second refinement"))
            .unwrap();
        let versions = store.versions("a").unwrap();
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[2].steps, v3);
    }

    #[test]
    fn update_description_only_does_not_touch_version() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        store
            .update("a", Some("new desc"), None, None, None, None)
            .unwrap();
        let s = store.get("a").unwrap().unwrap();
        assert_eq!(s.description, "new desc");
        assert_eq!(s.version, 1);
        let versions = store.versions("a").unwrap();
        assert_eq!(versions.len(), 1);
    }

    #[test]
    fn update_with_new_steps_creates_a_version_row() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        let new_steps = vec![SkillStep {
            step: "fixed".into(),
            tool: None,
            prompt: None,
        }];
        store
            .update(
                "a",
                None,
                None,
                Some(&new_steps),
                None,
                Some("operator fix"),
            )
            .unwrap();
        let s = store.get("a").unwrap().unwrap();
        assert_eq!(s.version, 2);
        assert_eq!(s.steps, new_steps);
        let versions = store.versions("a").unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1].change_reason.as_deref(), Some("operator fix"));
    }

    #[test]
    fn update_status_to_deprecated_hides_from_search() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "deploy_prod")).unwrap();
        let hits_before = store.search("deploy", 10, None, None).unwrap();
        assert_eq!(hits_before.len(), 1);
        store
            .update("a", None, None, None, Some(SkillStatus::Deprecated), None)
            .unwrap();
        let hits_after = store.search("deploy", 10, None, None).unwrap();
        assert!(hits_after.is_empty());
    }

    #[test]
    fn record_example_caps_at_max_and_evicts_oldest() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        // Start empty, add 5, cap at 3 — only newest 3 remain.
        store.record_example("a", "i1", "o1", 3).unwrap();
        store.record_example("a", "i2", "o2", 3).unwrap();
        store.record_example("a", "i3", "o3", 3).unwrap();
        store.record_example("a", "i4", "o4", 3).unwrap();
        store.record_example("a", "i5", "o5", 3).unwrap();
        let s = store.get("a").unwrap().unwrap();
        // The initial sample inserts one input ("fetch the latest")
        // and one output. We then add five more, capped at 3 →
        // only "i3", "i4", "i5" survive on each side.
        assert_eq!(s.example_inputs, vec!["i3", "i4", "i5"]);
        assert_eq!(s.example_outputs, vec!["o3", "o4", "o5"]);
    }

    #[test]
    fn stats_aggregates_total_active_avg_and_top_lists() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut a = sample("a", "alpha");
        a.confidence = 0.8;
        a.usage_count = 10;
        let mut b = sample("b", "beta");
        b.confidence = 0.6;
        b.usage_count = 5;
        let mut c = sample("c", "gamma");
        c.confidence = 0.4;
        c.status = SkillStatus::Deprecated;
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        store.insert(&c).unwrap();
        let s = store.stats().unwrap();
        assert_eq!(s.total_skills, 3);
        assert_eq!(s.active_skills, 2);
        // avg confidence over the active rows: (0.8 + 0.6) / 2 = 0.7.
        assert!((s.avg_confidence - 0.7).abs() < 1e-5);
        // Top-5-by-usage: a (10) before b (5); c excluded as
        // deprecated.
        assert_eq!(s.top_5_by_usage.len(), 2);
        assert_eq!(s.top_5_by_usage[0].id, "a");
    }

    #[test]
    fn list_refinement_candidates_requires_thresholds() {
        let store = SkillStore::open_in_memory().unwrap();
        let mut hot = sample("hot", "hot_skill");
        hot.usage_count = 15;
        hot.confidence = 0.8;
        let mut cold = sample("cold", "cold_skill");
        cold.usage_count = 2;
        cold.confidence = 0.8;
        let mut shy = sample("shy", "low_conf");
        shy.usage_count = 50;
        shy.confidence = 0.3;
        store.insert(&hot).unwrap();
        store.insert(&cold).unwrap();
        store.insert(&shy).unwrap();
        let eligible = store.list_refinement_candidates(10, 0.7, 50).unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].id, "hot");
    }

    #[test]
    fn mint_skill_id_is_uuidish_and_unique_across_calls() {
        let a = mint_skill_id("agent.x", "fetch");
        let b = mint_skill_id("agent.x", "fetch");
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.matches('-').count(), 4);
    }

    #[test]
    fn skill_status_round_trip_parses_every_variant() {
        for s in [
            SkillStatus::Active,
            SkillStatus::Deprecated,
            SkillStatus::Quarantined,
        ] {
            assert_eq!(SkillStatus::parse(s.as_str()), Some(s));
        }
        assert!(SkillStatus::parse("nope").is_none());
    }

    // ---- Tenant-isolation follow-up (post-Part 8): per-tenant
    // scoping in SkillStore. Mirrors the LayeredMemoryStore
    // precedent in nodes/memory/schema.rs (commit a0d00f1).

    fn tenant_sample(id: &str, name: &str, tenant: Option<&str>) -> StoredSkill {
        let mut s = sample(id, name);
        s.tenant_id = tenant.map(|t| t.to_string());
        s
    }

    #[test]
    fn tenant_isolation_flag_defaults_to_false() {
        let store = SkillStore::open_in_memory().unwrap();
        assert!(!store.tenant_isolation_enabled());
    }

    #[test]
    fn tenant_isolation_opt_in_enables_flag() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        assert!(store.tenant_isolation_enabled());
    }

    #[test]
    fn list_for_tenant_hides_cross_tenant_rows() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&tenant_sample("a", "alpha", Some("tenant-a")))
            .unwrap();
        store
            .insert(&tenant_sample("b", "beta", Some("tenant-b")))
            .unwrap();
        let only_a = store
            .list_for_tenant(&SkillFilter::default(), Some("tenant-a"))
            .unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].id, "a");
        assert_eq!(only_a[0].tenant_id.as_deref(), Some("tenant-a"));
    }

    #[test]
    fn list_for_tenant_fails_closed_on_missing_tenant() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&tenant_sample("a", "alpha", Some("tenant-a")))
            .unwrap();
        let err = store
            .list_for_tenant(&SkillFilter::default(), None)
            .unwrap_err();
        assert!(matches!(err, SkillStoreError::MissingTenant));
        let err = store
            .list_for_tenant(&SkillFilter::default(), Some("   "))
            .unwrap_err();
        assert!(matches!(err, SkillStoreError::MissingTenant));
    }

    #[test]
    fn list_for_tenant_falls_through_when_isolation_disabled() {
        let store = SkillStore::open_in_memory().unwrap();
        store.insert(&sample("a", "alpha")).unwrap();
        let rows = store
            .list_for_tenant(&SkillFilter::default(), None)
            .unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn search_for_tenant_hides_cross_tenant_rows() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&tenant_sample("a", "deploy_to_prod", Some("tenant-a")))
            .unwrap();
        store
            .insert(&tenant_sample("b", "deploy_to_prod", Some("tenant-b")))
            .unwrap();
        let only_a = store
            .search_for_tenant("deploy", 10, None, None, Some("tenant-a"))
            .unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].id, "a");
    }

    #[test]
    fn search_for_tenant_fails_closed_on_missing_tenant() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        let err = store
            .search_for_tenant("deploy", 10, None, None, None)
            .unwrap_err();
        assert!(matches!(err, SkillStoreError::MissingTenant));
    }

    #[test]
    fn get_for_tenant_returns_none_on_cross_tenant_id() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&tenant_sample("a", "alpha", Some("tenant-a")))
            .unwrap();
        // Same id exists, but under a different tenant — must not
        // leak the row by returning Some.
        let absent = store.get_for_tenant("a", Some("tenant-b")).unwrap();
        assert!(absent.is_none());
        let present = store.get_for_tenant("a", Some("tenant-a")).unwrap();
        assert!(present.is_some());
        assert_eq!(present.unwrap().tenant_id.as_deref(), Some("tenant-a"));
    }

    #[test]
    fn get_for_tenant_fails_closed_on_missing_tenant() {
        let store = SkillStore::open_in_memory_with_tenant_isolation(true).unwrap();
        let err = store.get_for_tenant("anything", None).unwrap_err();
        assert!(matches!(err, SkillStoreError::MissingTenant));
    }
}
