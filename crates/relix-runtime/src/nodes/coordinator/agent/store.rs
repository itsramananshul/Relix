//! SQLite-backed storage for the agent employee permission
//! model.
//!
//! Three tables live in the coordinator's database:
//!
//! - `agent_profiles`     — Phase 1+2.
//! - `approval_requests`  — Phase 4.
//! - `standing_approvals` — Phase 5.
//!
//! Categorical / sensitivity-tag list fields are stored as
//! JSON text so we can serialise `Vec<String>` without
//! reaching for serde-driven query helpers in the hot
//! admission path. The admission gate parses these once at
//! lookup; everyday calls just hit `AgentSnapshot::cached`.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

// ── Public record types ───────────────────────────────────

/// Full agent profile row. Returned by `agent.get` and the
/// gate-lookup path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub title: String,
    pub department: String,
    pub team: String,
    pub created_by: String,
    /// `active` / `suspended` / `disabled`.
    pub status: String,
    pub subject_id: String,
    pub surface_allowlist: Vec<String>,
    pub risk_ceiling: String,
    pub allow_categories: Vec<String>,
    pub deny_categories: Vec<String>,
    pub allow_sensitivity_tags: Vec<String>,
    pub deny_sensitivity_tags: Vec<String>,
    /// Categories that require operator approval before the
    /// call is admitted. Defaults to the six categories
    /// listed in `default_approval_categories`.
    pub approval_required_categories: Vec<String>,
    pub approval_timeout_secs: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The default set of capability categories that require an
/// operator approval before the gate admits the call. Per
/// the design spec's Phase 4 list.
pub fn default_approval_categories() -> Vec<String> {
    vec![
        "payments".to_string(),
        "production_deploy".to_string(),
        "credentials:read".to_string(),
        "email:send".to_string(),
        "external_api:write".to_string(),
        "browser.form_submit".to_string(),
    ]
}

/// A focused view tailored for the dispatch admission gate.
/// Re-creates only the fields the gate actually reads, so
/// we don't drag full strings through the hot path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentGateView {
    pub agent_id: String,
    pub subject_id: String,
    pub status: String,
    pub surface_allowlist: Vec<String>,
    pub risk_ceiling: String,
    pub allow_categories: Vec<String>,
    pub deny_categories: Vec<String>,
    pub allow_sensitivity_tags: Vec<String>,
    pub deny_sensitivity_tags: Vec<String>,
    pub approval_required_categories: Vec<String>,
    pub approval_timeout_secs: i64,
}

impl From<&AgentProfile> for AgentGateView {
    fn from(p: &AgentProfile) -> Self {
        Self {
            agent_id: p.agent_id.clone(),
            subject_id: p.subject_id.clone(),
            status: p.status.clone(),
            surface_allowlist: p.surface_allowlist.clone(),
            risk_ceiling: p.risk_ceiling.clone(),
            allow_categories: p.allow_categories.clone(),
            deny_categories: p.deny_categories.clone(),
            allow_sensitivity_tags: p.allow_sensitivity_tags.clone(),
            deny_sensitivity_tags: p.deny_sensitivity_tags.clone(),
            approval_required_categories: p.approval_required_categories.clone(),
            approval_timeout_secs: p.approval_timeout_secs,
        }
    }
}

/// Lightweight row used by `agent.list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub status: String,
    pub subject_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    /// Approved + the one-shot token has been consumed.
    /// Distinct from `Approved` so a replay can't reuse the
    /// same approval record.
    Consumed,
}

impl ApprovalStatus {
    pub fn as_wire(&self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Consumed => "consumed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            "consumed" => Some(Self::Consumed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub approval_id: String,
    pub agent_id: String,
    pub subject_id: String,
    pub method: String,
    pub capability_category: String,
    pub args_redacted_hash: String,
    pub reason: String,
    pub approver_groups: Vec<String>,
    pub requested_at: i64,
    pub expires_at: i64,
    pub status: ApprovalStatus,
    pub decided_at: Option<i64>,
    pub decided_by: Option<String>,
    pub decision_note: Option<String>,
    pub task_id: Option<String>,
    pub approval_token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingApproval {
    pub standing_id: String,
    pub agent_id: String,
    pub match_category: String,
    pub match_path_glob: Option<String>,
    pub expires_at: i64,
    pub granted_by: String,
    pub note: String,
    pub created_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentStoreError {
    #[error("agent store: {0}")]
    Io(String),
    #[error("agent store: db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("agent store: not found: {0}")]
    NotFound(String),
    #[error("agent store: bad input: {0}")]
    BadInput(String),
    #[error("agent store: poisoned mutex")]
    Lock,
    #[error("agent store: json: {0}")]
    Json(String),
}

// ── Store ─────────────────────────────────────────────────

pub struct AgentStore {
    conn: Arc<Mutex<Connection>>,
}

impl AgentStore {
    pub fn open(path: &Path) -> Result<Self, AgentStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AgentStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "agent_store");
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, AgentStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ── agent_profiles ────────────────────────────────────

    /// Mint a new agent profile. Returns the freshly-allocated
    /// `agent_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_agent(
        &self,
        name: &str,
        role: &str,
        title: &str,
        department: &str,
        team: &str,
        created_by: &str,
        subject_id: &str,
        risk_ceiling: &str,
    ) -> Result<String, AgentStoreError> {
        for (label, val) in [
            ("name", name),
            ("role", role),
            ("title", title),
            ("department", department),
            ("team", team),
            ("created_by", created_by),
            ("subject_id", subject_id),
        ] {
            if val.trim().is_empty() {
                return Err(AgentStoreError::BadInput(format!("{label} required")));
            }
        }
        if !is_known_risk(risk_ceiling) {
            return Err(AgentStoreError::BadInput(format!(
                "risk_ceiling '{risk_ceiling}' not in safe/low/medium/high/critical"
            )));
        }
        let now = unix_now();
        let agent_id = new_agent_id(role);
        let approval_required = serde_json::to_string(&default_approval_categories())
            .map_err(|e| AgentStoreError::Json(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO agent_profiles (
                 agent_id, name, role, title, department, team,
                 created_by, status, subject_id, surface_allowlist,
                 risk_ceiling, allow_categories, deny_categories,
                 allow_sensitivity_tags, deny_sensitivity_tags,
                 approval_required_categories, approval_timeout_secs,
                 created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, '[]',
                       ?9, '[]', '[]', '[]', '[]', ?10, 86400, ?11, ?11)",
            params![
                agent_id,
                name,
                role,
                title,
                department,
                team,
                created_by,
                subject_id,
                risk_ceiling,
                approval_required,
                now,
            ],
        )?;
        Ok(agent_id)
    }

    pub fn get_agent(&self, agent_id: &str) -> Result<Option<AgentProfile>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_AGENT, params![agent_id], row_to_agent)
            .optional()?;
        Ok(row)
    }

    /// Lookup by the AIC subject_id — the admission gate's
    /// primary read path.
    pub fn get_by_subject(
        &self,
        subject_id: &str,
    ) -> Result<Option<AgentProfile>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT agent_id, name, role, title, department, team,
                        created_by, status, subject_id, surface_allowlist,
                        risk_ceiling, allow_categories, deny_categories,
                        allow_sensitivity_tags, deny_sensitivity_tags,
                        approval_required_categories, approval_timeout_secs,
                        created_at, updated_at
                 FROM agent_profiles WHERE subject_id = ?1",
                params![subject_id],
                row_to_agent,
            )
            .optional()?;
        Ok(row)
    }

    /// `agent.list` source. Filter by subject_id, or pass
    /// `None` to list all.
    pub fn list_agents(
        &self,
        subject_filter: Option<&str>,
    ) -> Result<Vec<AgentSnapshot>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let sql = if subject_filter.is_some() {
            "SELECT agent_id, name, role, status, subject_id
             FROM agent_profiles WHERE subject_id = ?1
             ORDER BY created_at DESC"
        } else {
            "SELECT agent_id, name, role, status, subject_id
             FROM agent_profiles ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let mapper = |r: &rusqlite::Row| {
            Ok(AgentSnapshot {
                agent_id: r.get(0)?,
                name: r.get(1)?,
                role: r.get(2)?,
                status: r.get(3)?,
                subject_id: r.get(4)?,
            })
        };
        let rows: Vec<AgentSnapshot> = if let Some(s) = subject_filter {
            stmt.query_map(params![s], mapper)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([], mapper)?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// Update one field. The set of writable fields is curated;
    /// silent-allow on agent_id / created_at is intentional —
    /// they're never operator-mutable.
    pub fn update_agent_field(
        &self,
        agent_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), AgentStoreError> {
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = match field {
            "status" => {
                if !["active", "suspended", "disabled"].contains(&value) {
                    return Err(AgentStoreError::BadInput(format!(
                        "status '{value}' not in active/suspended/disabled"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET status=?1, updated_at=?2 WHERE agent_id=?3",
                    params![value, now, agent_id],
                )?
            }
            "role" | "title" | "department" | "team" => {
                if value.trim().is_empty() {
                    return Err(AgentStoreError::BadInput(format!("{field} required")));
                }
                let sql = format!(
                    "UPDATE agent_profiles SET {field}=?1, updated_at=?2 WHERE agent_id=?3"
                );
                conn.execute(&sql, params![value, now, agent_id])?
            }
            "risk_ceiling" => {
                if !is_known_risk(value) {
                    return Err(AgentStoreError::BadInput(format!(
                        "risk_ceiling '{value}' not in safe/low/medium/high/critical"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET risk_ceiling=?1, updated_at=?2 WHERE agent_id=?3",
                    params![value, now, agent_id],
                )?
            }
            "approval_timeout_secs" => {
                let v: i64 = value
                    .parse()
                    .map_err(|_| AgentStoreError::BadInput(format!("not an i64: {value}")))?;
                if v <= 0 {
                    return Err(AgentStoreError::BadInput(
                        "approval_timeout_secs must be > 0".into(),
                    ));
                }
                conn.execute(
                    "UPDATE agent_profiles SET approval_timeout_secs=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![v, now, agent_id],
                )?
            }
            "surface_allowlist"
            | "allow_categories"
            | "deny_categories"
            | "allow_sensitivity_tags"
            | "deny_sensitivity_tags"
            | "approval_required_categories" => {
                // Accept either a JSON array or a comma-separated
                // list; normalise to JSON for storage.
                let json = normalise_string_list(value)
                    .map_err(|e| AgentStoreError::BadInput(format!("{field}: {e}")))?;
                let sql = format!(
                    "UPDATE agent_profiles SET {field}=?1, updated_at=?2 WHERE agent_id=?3"
                );
                conn.execute(&sql, params![json, now, agent_id])?
            }
            other => {
                return Err(AgentStoreError::BadInput(format!(
                    "unknown field '{other}'"
                )));
            }
        };
        if changed == 0 {
            return Err(AgentStoreError::NotFound(agent_id.into()));
        }
        Ok(())
    }

    /// Soft delete: flips status to `disabled`. Hard delete is
    /// intentionally not exposed — the AIC bundle remains
    /// valid and audit signatures must stay verifiable.
    pub fn soft_delete_agent(&self, agent_id: &str) -> Result<(), AgentStoreError> {
        self.update_agent_field(agent_id, "status", "disabled")
    }

    // ── approval_requests ─────────────────────────────────

    /// Insert a new pending approval. Returns the approval_id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_approval(
        &self,
        agent_id: &str,
        subject_id: &str,
        method: &str,
        capability_category: &str,
        args_redacted_hash: &str,
        reason: &str,
        approver_groups: &[String],
        task_id: Option<&str>,
        expires_at: i64,
    ) -> Result<String, AgentStoreError> {
        let now = unix_now();
        let approval_id = new_approval_id();
        let groups_json = serde_json::to_string(approver_groups)
            .map_err(|e| AgentStoreError::Json(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO approval_requests (
                 approval_id, agent_id, subject_id, method, capability_category,
                 args_redacted_hash, reason, approver_groups,
                 requested_at, expires_at, status, task_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)",
            params![
                approval_id,
                agent_id,
                subject_id,
                method,
                capability_category,
                args_redacted_hash,
                reason,
                groups_json,
                now,
                expires_at,
                task_id,
            ],
        )?;
        Ok(approval_id)
    }

    pub fn get_approval(
        &self,
        approval_id: &str,
    ) -> Result<Option<ApprovalRecord>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_APPROVAL, params![approval_id], row_to_approval)
            .optional()?;
        Ok(row)
    }

    /// Lookup an approval by its one-shot token. The
    /// admission gate calls this on every inbound that
    /// carries an `approval_token` header.
    pub fn get_approval_by_token(
        &self,
        token: &str,
    ) -> Result<Option<ApprovalRecord>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT approval_id, agent_id, subject_id, method, capability_category,
                        args_redacted_hash, reason, approver_groups,
                        requested_at, expires_at, status,
                        decided_at, decided_by, decision_note,
                        task_id, approval_token
                 FROM approval_requests WHERE approval_token = ?1",
                params![token],
                row_to_approval,
            )
            .optional()?;
        Ok(row)
    }

    /// Approve: stamp decided_at/by/note + generate the one-shot
    /// token. Returns the new token. Refuses to act on a
    /// terminal status.
    pub fn decide_approval(
        &self,
        approval_id: &str,
        decision: ApprovalStatus,
        decided_by: &str,
        note: &str,
    ) -> Result<Option<String>, AgentStoreError> {
        if !matches!(
            decision,
            ApprovalStatus::Approved | ApprovalStatus::Rejected
        ) {
            return Err(AgentStoreError::BadInput(
                "decide accepts only Approved/Rejected".into(),
            ));
        }
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let current: Option<String> = conn
            .query_row(
                "SELECT status FROM approval_requests WHERE approval_id = ?1",
                params![approval_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        match current {
            None => return Err(AgentStoreError::NotFound(approval_id.into())),
            Some(s) if s != "pending" => {
                return Err(AgentStoreError::BadInput(format!(
                    "approval is {s}, not pending"
                )));
            }
            _ => {}
        }
        let token = if decision == ApprovalStatus::Approved {
            Some(new_approval_token())
        } else {
            None
        };
        conn.execute(
            "UPDATE approval_requests SET
                 status = ?1,
                 decided_at = ?2,
                 decided_by = ?3,
                 decision_note = ?4,
                 approval_token = ?5
             WHERE approval_id = ?6",
            params![
                decision.as_wire(),
                now,
                decided_by,
                note,
                token,
                approval_id,
            ],
        )?;
        Ok(token)
    }

    /// Consume an approved approval's token. The admission gate
    /// calls this once it has admitted the call so a replay can't
    /// reuse the same token.
    pub fn consume_approval_token(&self, token: &str) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = conn.execute(
            "UPDATE approval_requests SET status = 'consumed'
             WHERE approval_token = ?1 AND status = 'approved'",
            params![token],
        )?;
        if changed == 0 {
            return Err(AgentStoreError::NotFound(format!("token: {token}")));
        }
        Ok(())
    }

    /// Newest-first pending approvals, capped at `limit`.
    pub fn list_pending_approvals(
        &self,
        limit: usize,
    ) -> Result<Vec<ApprovalRecord>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let cap = limit.clamp(1, 500);
        let mut stmt = conn.prepare(
            "SELECT approval_id, agent_id, subject_id, method, capability_category,
                    args_redacted_hash, reason, approver_groups,
                    requested_at, expires_at, status,
                    decided_at, decided_by, decision_note,
                    task_id, approval_token
             FROM approval_requests
             WHERE status = 'pending'
             ORDER BY requested_at ASC
             LIMIT ?1",
        )?;
        let rows: Vec<ApprovalRecord> = stmt
            .query_map(params![cap as i64], row_to_approval)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Find pending approvals whose `expires_at <= now`. Used by
    /// the auto-expire loop on the coordinator.
    pub fn list_expired_pending(
        &self,
        now: i64,
    ) -> Result<Vec<(String, Option<String>)>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT approval_id, task_id
             FROM approval_requests
             WHERE status = 'pending' AND expires_at <= ?1
             ORDER BY expires_at ASC
             LIMIT 100",
        )?;
        let rows = stmt
            .query_map(params![now], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn mark_expired(&self, approval_id: &str) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let now = unix_now();
        let changed = conn.execute(
            "UPDATE approval_requests SET status='expired', decided_at=?1
             WHERE approval_id=?2 AND status='pending'",
            params![now, approval_id],
        )?;
        if changed == 0 {
            return Err(AgentStoreError::NotFound(approval_id.into()));
        }
        Ok(())
    }

    // ── standing_approvals ────────────────────────────────

    pub fn create_standing(
        &self,
        agent_id: &str,
        match_category: &str,
        match_path_glob: Option<&str>,
        expires_at: i64,
        granted_by: &str,
        note: &str,
    ) -> Result<String, AgentStoreError> {
        if agent_id.trim().is_empty() || match_category.trim().is_empty() {
            return Err(AgentStoreError::BadInput(
                "agent_id and match_category required".into(),
            ));
        }
        let now = unix_now();
        let standing_id = new_standing_id();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO standing_approvals (
                 standing_id, agent_id, match_category, match_path_glob,
                 expires_at, granted_by, note, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                standing_id,
                agent_id,
                match_category,
                match_path_glob,
                expires_at,
                granted_by,
                note,
                now,
            ],
        )?;
        Ok(standing_id)
    }

    pub fn list_standing(&self, agent_id: &str) -> Result<Vec<StandingApproval>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT standing_id, agent_id, match_category, match_path_glob,
                    expires_at, granted_by, note, created_at
             FROM standing_approvals WHERE agent_id = ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![agent_id], |r| {
                Ok(StandingApproval {
                    standing_id: r.get(0)?,
                    agent_id: r.get(1)?,
                    match_category: r.get(2)?,
                    match_path_glob: r.get(3)?,
                    expires_at: r.get(4)?,
                    granted_by: r.get(5)?,
                    note: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// True iff `agent_id` has at least one non-expired standing
    /// approval covering `category`. Gate fast-path before
    /// minting an approval request.
    pub fn has_active_standing(
        &self,
        agent_id: &str,
        category: &str,
        now: i64,
    ) -> Result<bool, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM standing_approvals
             WHERE agent_id = ?1 AND match_category = ?2 AND expires_at > ?3",
            params![agent_id, category, now],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn revoke_standing(&self, standing_id: &str) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = conn.execute(
            "DELETE FROM standing_approvals WHERE standing_id = ?1",
            params![standing_id],
        )?;
        if changed == 0 {
            return Err(AgentStoreError::NotFound(standing_id.into()));
        }
        Ok(())
    }
}

// ── schema + helpers ──────────────────────────────────────

const SELECT_AGENT: &str = "SELECT agent_id, name, role, title, department, team,
        created_by, status, subject_id, surface_allowlist,
        risk_ceiling, allow_categories, deny_categories,
        allow_sensitivity_tags, deny_sensitivity_tags,
        approval_required_categories, approval_timeout_secs,
        created_at, updated_at
 FROM agent_profiles WHERE agent_id = ?1";

const SELECT_APPROVAL: &str =
    "SELECT approval_id, agent_id, subject_id, method, capability_category,
        args_redacted_hash, reason, approver_groups,
        requested_at, expires_at, status,
        decided_at, decided_by, decision_note,
        task_id, approval_token
 FROM approval_requests WHERE approval_id = ?1";

fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agent_profiles (
             agent_id        TEXT PRIMARY KEY,
             name            TEXT NOT NULL,
             role            TEXT NOT NULL,
             title           TEXT NOT NULL,
             department      TEXT NOT NULL,
             team            TEXT NOT NULL,
             created_by      TEXT NOT NULL,
             status          TEXT NOT NULL DEFAULT 'active',
             subject_id      TEXT NOT NULL,
             surface_allowlist TEXT NOT NULL DEFAULT '[]',
             risk_ceiling    TEXT NOT NULL DEFAULT 'medium',
             allow_categories TEXT NOT NULL DEFAULT '[]',
             deny_categories  TEXT NOT NULL DEFAULT '[]',
             allow_sensitivity_tags TEXT NOT NULL DEFAULT '[]',
             deny_sensitivity_tags  TEXT NOT NULL DEFAULT '[]',
             approval_required_categories TEXT NOT NULL DEFAULT '[]',
             approval_timeout_secs INTEGER NOT NULL DEFAULT 86400,
             created_at      INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS agent_profiles_subject
             ON agent_profiles(subject_id);

         CREATE TABLE IF NOT EXISTS approval_requests (
             approval_id     TEXT PRIMARY KEY,
             agent_id        TEXT NOT NULL,
             subject_id      TEXT NOT NULL,
             method          TEXT NOT NULL,
             capability_category TEXT NOT NULL,
             args_redacted_hash  TEXT NOT NULL,
             reason          TEXT NOT NULL,
             approver_groups TEXT NOT NULL DEFAULT '[]',
             requested_at    INTEGER NOT NULL,
             expires_at      INTEGER NOT NULL,
             status          TEXT NOT NULL DEFAULT 'pending',
             decided_at      INTEGER,
             decided_by      TEXT,
             decision_note   TEXT,
             task_id         TEXT,
             approval_token  TEXT UNIQUE
         );
         CREATE INDEX IF NOT EXISTS approval_requests_pending
             ON approval_requests(status, expires_at);
         CREATE INDEX IF NOT EXISTS approval_requests_agent
             ON approval_requests(agent_id, requested_at);

         CREATE TABLE IF NOT EXISTS standing_approvals (
             standing_id     TEXT PRIMARY KEY,
             agent_id        TEXT NOT NULL,
             match_category  TEXT NOT NULL,
             match_path_glob TEXT,
             expires_at      INTEGER NOT NULL,
             granted_by      TEXT NOT NULL,
             note            TEXT NOT NULL DEFAULT '',
             created_at      INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS standing_approvals_agent
             ON standing_approvals(agent_id, match_category, expires_at);",
    )
}

fn row_to_agent(r: &rusqlite::Row) -> rusqlite::Result<AgentProfile> {
    let surface_allowlist: String = r.get(9)?;
    let allow_categories: String = r.get(11)?;
    let deny_categories: String = r.get(12)?;
    let allow_sensitivity_tags: String = r.get(13)?;
    let deny_sensitivity_tags: String = r.get(14)?;
    let approval_required_categories: String = r.get(15)?;
    Ok(AgentProfile {
        agent_id: r.get(0)?,
        name: r.get(1)?,
        role: r.get(2)?,
        title: r.get(3)?,
        department: r.get(4)?,
        team: r.get(5)?,
        created_by: r.get(6)?,
        status: r.get(7)?,
        subject_id: r.get(8)?,
        surface_allowlist: parse_json_list(&surface_allowlist),
        risk_ceiling: r.get(10)?,
        allow_categories: parse_json_list(&allow_categories),
        deny_categories: parse_json_list(&deny_categories),
        allow_sensitivity_tags: parse_json_list(&allow_sensitivity_tags),
        deny_sensitivity_tags: parse_json_list(&deny_sensitivity_tags),
        approval_required_categories: parse_json_list(&approval_required_categories),
        approval_timeout_secs: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
    })
}

fn row_to_approval(r: &rusqlite::Row) -> rusqlite::Result<ApprovalRecord> {
    let groups_json: String = r.get(7)?;
    let status_str: String = r.get(10)?;
    let status = ApprovalStatus::parse(&status_str).unwrap_or(ApprovalStatus::Pending);
    Ok(ApprovalRecord {
        approval_id: r.get(0)?,
        agent_id: r.get(1)?,
        subject_id: r.get(2)?,
        method: r.get(3)?,
        capability_category: r.get(4)?,
        args_redacted_hash: r.get(5)?,
        reason: r.get(6)?,
        approver_groups: parse_json_list(&groups_json),
        requested_at: r.get(8)?,
        expires_at: r.get(9)?,
        status,
        decided_at: r.get(11)?,
        decided_by: r.get(12)?,
        decision_note: r.get(13)?,
        task_id: r.get(14)?,
        approval_token: r.get(15)?,
    })
}

fn parse_json_list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn normalise_string_list(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') {
        // Validate JSON.
        let v: Vec<String> = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
        return serde_json::to_string(&v).map_err(|e| e.to_string());
    }
    if trimmed.is_empty() {
        return Ok("[]".to_string());
    }
    let items: Vec<String> = trimmed
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    serde_json::to_string(&items).map_err(|e| e.to_string())
}

fn is_known_risk(s: &str) -> bool {
    matches!(s, "safe" | "low" | "medium" | "high" | "critical")
}

fn new_agent_id(role: &str) -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    let suffix = hex::encode(bytes);
    let role_slug: String = role
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    let role_slug = if role_slug.is_empty() {
        "agent".to_string()
    } else {
        role_slug.chars().take(20).collect()
    };
    format!("agt_{role_slug}_{suffix}")
}

fn new_approval_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("apr_{}", hex::encode(bytes))
}

fn new_standing_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("std_{}", hex::encode(bytes))
}

fn new_approval_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AgentStore {
        AgentStore::in_memory().unwrap()
    }

    // ── agent CRUD ───────────────────────────────────────

    #[test]
    fn create_then_get_round_trips_every_field() {
        let s = store();
        let id = s
            .create_agent(
                "Research Assistant",
                "research_assistant",
                "Junior research analyst",
                "research",
                "research-ops",
                "alice",
                "subj-1",
                "medium",
            )
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.name, "Research Assistant");
        assert_eq!(p.role, "research_assistant");
        assert_eq!(p.status, "active");
        assert_eq!(p.subject_id, "subj-1");
        assert_eq!(p.risk_ceiling, "medium");
        assert_eq!(p.approval_timeout_secs, 86400);
        assert!(
            p.approval_required_categories
                .contains(&"payments".to_string())
        );
    }

    #[test]
    fn create_rejects_unknown_risk_ceiling() {
        let s = store();
        let r = s.create_agent("n", "r", "t", "d", "t", "c", "subj", "extreme");
        assert!(matches!(r, Err(AgentStoreError::BadInput(_))));
    }

    #[test]
    fn get_by_subject_returns_the_profile() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj-x", "low")
            .unwrap();
        let p = s.get_by_subject("subj-x").unwrap().unwrap();
        assert_eq!(p.agent_id, id);
    }

    #[test]
    fn get_by_subject_unknown_returns_none() {
        let s = store();
        assert!(s.get_by_subject("nope").unwrap().is_none());
    }

    #[test]
    fn list_agents_filters_by_subject_id() {
        let s = store();
        s.create_agent("a", "r", "t", "d", "t", "c", "subj-1", "low")
            .unwrap();
        s.create_agent("b", "r", "t", "d", "t", "c", "subj-2", "low")
            .unwrap();
        let one = s.list_agents(Some("subj-1")).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "a");
        let all = s.list_agents(None).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn update_status_validates_and_writes() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        s.update_agent_field(&id, "status", "suspended").unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.status, "suspended");
        // Bad value rejected.
        assert!(matches!(
            s.update_agent_field(&id, "status", "frozen"),
            Err(AgentStoreError::BadInput(_))
        ));
    }

    #[test]
    fn update_allow_categories_accepts_comma_separated() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        s.update_agent_field(&id, "allow_categories", "browser, fetch, summarise")
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(
            p.allow_categories,
            vec!["browser".to_string(), "fetch".into(), "summarise".into()]
        );
    }

    #[test]
    fn update_unknown_field_rejected() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        assert!(matches!(
            s.update_agent_field(&id, "name", "x"),
            Err(AgentStoreError::BadInput(_))
        ));
    }

    #[test]
    fn soft_delete_sets_status_to_disabled() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        s.soft_delete_agent(&id).unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "disabled");
    }

    // ── approvals ────────────────────────────────────────

    #[test]
    fn create_then_get_approval_round_trips() {
        let s = store();
        let id = s
            .create_approval(
                "agt-1",
                "subj-1",
                "tool.web_post",
                "external_api:write",
                "deadbeef",
                "form submit",
                &["ops".into(), "admin".into()],
                Some("task-1"),
                unix_now() + 86400,
            )
            .unwrap();
        let r = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(r.method, "tool.web_post");
        assert_eq!(r.status, ApprovalStatus::Pending);
        assert_eq!(r.task_id.as_deref(), Some("task-1"));
        assert!(r.approval_token.is_none());
    }

    #[test]
    fn decide_approved_mints_a_token_and_consume_invalidates_replay() {
        let s = store();
        let id = s
            .create_approval(
                "agt-1",
                "subj-1",
                "tool.x",
                "cat",
                "",
                "",
                &[],
                None,
                unix_now() + 60,
            )
            .unwrap();
        let token = s
            .decide_approval(&id, ApprovalStatus::Approved, "alice", "ok")
            .unwrap()
            .expect("approved -> Some(token)");
        let by_token = s.get_approval_by_token(&token).unwrap().unwrap();
        assert_eq!(by_token.status, ApprovalStatus::Approved);
        s.consume_approval_token(&token).unwrap();
        // Replay fails.
        assert!(matches!(
            s.consume_approval_token(&token),
            Err(AgentStoreError::NotFound(_))
        ));
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status,
            ApprovalStatus::Consumed
        );
    }

    #[test]
    fn decide_rejected_returns_no_token() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, unix_now() + 60)
            .unwrap();
        let token = s
            .decide_approval(&id, ApprovalStatus::Rejected, "alice", "nope")
            .unwrap();
        assert!(token.is_none());
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status,
            ApprovalStatus::Rejected
        );
    }

    #[test]
    fn decide_refuses_terminal_approval() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, unix_now() + 60)
            .unwrap();
        s.decide_approval(&id, ApprovalStatus::Approved, "alice", "")
            .unwrap();
        // Second decision rejected.
        assert!(matches!(
            s.decide_approval(&id, ApprovalStatus::Rejected, "alice", ""),
            Err(AgentStoreError::BadInput(_))
        ));
    }

    #[test]
    fn list_pending_returns_only_pending_oldest_first() {
        let s = store();
        let _a = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, unix_now() + 60)
            .unwrap();
        let b = s
            .create_approval("b", "s", "m", "c", "", "", &[], None, unix_now() + 60)
            .unwrap();
        // Decide b → not pending.
        s.decide_approval(&b, ApprovalStatus::Approved, "alice", "")
            .unwrap();
        let v = s.list_pending_approvals(50).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn list_expired_pending_returns_past_deadlines() {
        let s = store();
        let _id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, 100)
            .unwrap();
        // expires_at = 100; query with now = 1000.
        let v = s.list_expired_pending(1000).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn mark_expired_flips_status() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, 100)
            .unwrap();
        s.mark_expired(&id).unwrap();
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status,
            ApprovalStatus::Expired
        );
    }

    // ── standing approvals ───────────────────────────────

    #[test]
    fn create_standing_then_has_active_returns_true() {
        let s = store();
        let _id = s
            .create_standing("agt-1", "fs", None, unix_now() + 86400, "alice", "")
            .unwrap();
        assert!(s.has_active_standing("agt-1", "fs", unix_now()).unwrap());
        assert!(
            !s.has_active_standing("agt-1", "browser", unix_now())
                .unwrap()
        );
    }

    #[test]
    fn has_active_standing_returns_false_after_expiry() {
        let s = store();
        let _id = s
            .create_standing("agt-1", "fs", None, 100, "alice", "")
            .unwrap();
        assert!(!s.has_active_standing("agt-1", "fs", 1000).unwrap());
    }

    #[test]
    fn revoke_standing_drops_the_row() {
        let s = store();
        let id = s
            .create_standing("agt-1", "fs", None, unix_now() + 60, "alice", "")
            .unwrap();
        s.revoke_standing(&id).unwrap();
        assert!(!s.has_active_standing("agt-1", "fs", unix_now()).unwrap());
        assert!(matches!(
            s.revoke_standing(&id),
            Err(AgentStoreError::NotFound(_))
        ));
    }

    #[test]
    fn list_standing_returns_rows_for_agent() {
        let s = store();
        s.create_standing("agt-1", "fs", None, unix_now() + 60, "alice", "n1")
            .unwrap();
        s.create_standing("agt-1", "browser", None, unix_now() + 60, "alice", "n2")
            .unwrap();
        s.create_standing("agt-2", "fs", None, unix_now() + 60, "alice", "n3")
            .unwrap();
        let v = s.list_standing("agt-1").unwrap();
        assert_eq!(v.len(), 2);
    }
}
