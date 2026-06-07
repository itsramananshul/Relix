//! SQLite-backed storage for the company **work-object spine**
//! above the Issue: **Mandates** (the durable "why") and
//! **Campaigns** (workstreams grouping issues under a mandate).
//!
//! Two tables live in their own coordinator-side database:
//!
//! - `mandates` — Phase 1. The high-level outcomes a company cares
//!   about; may nest (a mandate can have a parent mandate) to form a
//!   mandate hierarchy.
//! - `campaigns` — Phase 1. A workstream under a mandate; the unit
//!   an Issue links up to.
//!
//! Both objects are **tenant-scoped** (a Company is the
//! product-facing name for a tenant), and every read offers a
//! tenant-scoped variant so a caller scoped to tenant A can
//! never read tenant B's spine — mirroring the agent store's
//! GROUP-6 isolation.
//!
//! This module is deliberately self-contained (its own store,
//! its own schema, its own tests) so it adds the spine objects
//! without touching the existing coordinator Task ledger. The
//! Issue itself remains the evolved Task; Mandates/Campaigns are the
//! durable objects it links up to.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

// ── Public record types ───────────────────────────────────

/// A Mandate / Initiative — the durable "why." Belongs to a
/// Company (tenant); may nest under a parent mandate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mandate {
    pub mandate_id: String,
    pub tenant_id: String,
    pub title: String,
    pub description: String,
    /// The agent that owns this mandate (usually the CEO or a
    /// senior agent). `None` until assigned.
    pub owner_agent_id: Option<String>,
    /// `planned` / `active` / `achieved` / `cancelled`.
    pub status: String,
    /// Parent mandate, for a mandate hierarchy. `None` at the top.
    pub parent_mandate_id: Option<String>,
    /// OBJECT-LEVEL billing code (company-model §6.6) — cross-team cost
    /// attribution at the Mandate level. `None` when unset. Inherited
    /// by a linked Brief's run only when no Brief in the chain carries
    /// its own code (see [`crate::nodes::coordinator::ObjectBillingResolver`]).
    pub billing_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A Campaign — a workstream grouping issues under a mandate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Campaign {
    pub campaign_id: String,
    pub tenant_id: String,
    pub title: String,
    /// The mandate this workstream serves. `None` if unlinked.
    pub mandate_id: Option<String>,
    /// The lead agent for the campaign. `None` until assigned.
    pub lead_agent_id: Option<String>,
    /// `backlog` / `planned` / `in_progress` / `completed` /
    /// `cancelled`.
    pub status: String,
    /// Optional shared workspace/environment the campaign's work
    /// runs in (a cwd, a worktree, or — later — a sandbox id).
    pub workspace: Option<String>,
    /// OBJECT-LEVEL billing code (company-model §6.6) — cross-team cost
    /// attribution at the Campaign level. `None` when unset. Takes
    /// precedence over the linked Mandate's code in the run-stamp
    /// fallback (see [`crate::nodes::coordinator::ObjectBillingResolver`]).
    pub billing_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A **Guild** — the product face of a tenant (the company). Holds
/// its display name; branding / budget can extend this later.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guild {
    /// The tenant id this Guild *is*.
    pub tenant_id: String,
    pub display_name: String,
    /// The Guild's monthly **Allowance** (budget) in cents. `None`
    /// = no cap.
    pub monthly_allowance_cents: Option<i64>,
    /// OBJECT-LEVEL billing code (company-model §6.6) — the Guild-wide
    /// default cost-attribution code. `None` when unset. The lowest-
    /// precedence fallback when a Brief, its ancestors, and its linked
    /// Campaign/Mandate all carry no code
    /// (see [`crate::nodes::coordinator::ObjectBillingResolver`]).
    pub billing_code: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The Guild's spine at a glance — Mandate & Campaign totals plus
/// the in-flight subset, in one tenant-scoped read. The companion /
/// dashboard pairs this with the Roster + board summaries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpineCounts {
    pub mandates_total: i64,
    /// Mandates with status `active`.
    pub mandates_active: i64,
    pub campaigns_total: i64,
    /// Campaigns with status `in_progress`.
    pub campaigns_active: i64,
}

/// A Mandate with its immediate spine children — the drill-down a
/// dashboard renders for one Mandate: the Mandate itself, its direct
/// sub-Mandates, and the Campaigns hanging off it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MandateTree {
    pub mandate: Mandate,
    pub child_mandates: Vec<Mandate>,
    pub campaigns: Vec<Campaign>,
}

/// A persisted **Mandate Team Plan** (company-model §4.2/§4.5) — the
/// durable artifact produced by `mandate.team_plan`. The list fields
/// are stored as JSON text exactly as the planning handler built them;
/// [`TeamPlan::to_json`] reconstructs the operator-facing object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamPlan {
    pub plan_id: String,
    pub tenant_id: String,
    pub mandate_id: String,
    pub actor_id: String,
    pub description: String,
    /// JSON array of role strings.
    pub proposed_roles: String,
    /// JSON array of `{role, agent_id, subject_id}` objects.
    pub pending_hires: String,
    /// JSON array of clearance id strings.
    pub clearance_ids: String,
    /// JSON array of `{role, reason}` objects.
    pub denials: String,
    /// JSON array of next-step strings.
    pub next_steps: String,
    /// `planned` / `staffing` / `awaiting_clearance`.
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl TeamPlan {
    /// Reconstruct the operator-facing JSON object, parsing the stored
    /// JSON-text list fields back into arrays (malformed text degrades
    /// to an empty array rather than poisoning the read).
    pub fn to_json(&self) -> serde_json::Value {
        let arr = |s: &str| -> serde_json::Value {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
        };
        serde_json::json!({
            "plan_id": self.plan_id,
            "tenant_id": self.tenant_id,
            "mandate_id": self.mandate_id,
            "actor_id": self.actor_id,
            "description": self.description,
            "proposed_roles": arr(&self.proposed_roles),
            "pending_hires": arr(&self.pending_hires),
            "clearance_ids": arr(&self.clearance_ids),
            "denials": arr(&self.denials),
            "next_steps": arr(&self.next_steps),
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        })
    }
}

/// Inputs for [`SpineStore::record_team_plan`]. The list fields are
/// pre-serialized JSON strings (the planning handler already builds
/// them as `serde_json` values).
pub struct TeamPlanRecord<'a> {
    pub tenant_id: &'a str,
    pub mandate_id: &'a str,
    pub actor_id: &'a str,
    pub description: &'a str,
    pub proposed_roles_json: &'a str,
    pub pending_hires_json: &'a str,
    pub clearance_ids_json: &'a str,
    pub denials_json: &'a str,
    pub next_steps_json: &'a str,
    pub status: &'a str,
}

/// A persisted **Mandate Orchestration run** — the durable record of
/// one `mandate.orchestrate` call (company-model §4.6). List fields are
/// stored as JSON text; [`OrchestrationRun::to_json`] reconstructs the
/// operator-facing object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrchestrationRun {
    pub run_id: String,
    pub tenant_id: String,
    pub mandate_id: String,
    pub mode: String,
    pub dry_run: bool,
    pub input_signature: String,
    /// `blocked` / `planned` / `created` / `assigned`.
    pub status: String,
    /// JSON array of created Brief task_ids.
    pub created_brief_ids: String,
    /// JSON array of **reused** (already-marked) Brief task_ids — Briefs
    /// the run recognized by source marker instead of recreating.
    pub existing_brief_ids: String,
    /// JSON array of assigned Brief task_ids.
    pub assigned_brief_ids: String,
    /// JSON array of skipped-role objects (role + reason) the run could
    /// not act on (e.g. no ready agent).
    pub skipped: String,
    /// JSON array of the stable source-marker keys the run touched.
    pub source_markers: String,
    /// JSON array of blocker objects.
    pub blockers: String,
    /// JSON array of next-action strings.
    pub next_actions: String,
    pub created_at: i64,
}

impl OrchestrationRun {
    pub fn to_json(&self) -> serde_json::Value {
        let arr = |s: &str| -> serde_json::Value {
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::Array(Vec::new()))
        };
        serde_json::json!({
            "run_id": self.run_id,
            "tenant_id": self.tenant_id,
            "mandate_id": self.mandate_id,
            "mode": self.mode,
            "dry_run": self.dry_run,
            "input_signature": self.input_signature,
            "status": self.status,
            "created_brief_ids": arr(&self.created_brief_ids),
            "existing_brief_ids": arr(&self.existing_brief_ids),
            "assigned_brief_ids": arr(&self.assigned_brief_ids),
            "skipped": arr(&self.skipped),
            "source_markers": arr(&self.source_markers),
            "blockers": arr(&self.blockers),
            "next_actions": arr(&self.next_actions),
            "created_at": self.created_at,
        })
    }
}

/// Inputs for [`SpineStore::record_orchestration_run`]. List fields are
/// pre-serialized JSON strings.
pub struct OrchestrationRunRecord<'a> {
    pub tenant_id: &'a str,
    pub mandate_id: &'a str,
    pub mode: &'a str,
    pub dry_run: bool,
    pub input_signature: &'a str,
    pub status: &'a str,
    pub created_brief_ids_json: &'a str,
    pub existing_brief_ids_json: &'a str,
    pub assigned_brief_ids_json: &'a str,
    pub skipped_json: &'a str,
    pub source_markers_json: &'a str,
    pub blockers_json: &'a str,
    pub next_actions_json: &'a str,
}

/// A persisted Prime Assistant proposal (the governed "describe → plan"
/// record). `mandate_id` / `created_brief_ids` are empty until `prime.approve`
/// flips `status` to `approved`.
#[derive(Debug, Clone)]
pub struct PrimeProposalRow {
    pub proposal_id: String,
    pub tenant_id: String,
    pub proposer_id: String,
    pub message: String,
    pub proposal_json: String,
    /// `proposed` / `approved` / `rejected`.
    pub status: String,
    pub mandate_id: String,
    pub created_brief_ids: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl PrimeProposalRow {
    /// The full proposal record as JSON for the API. The stored
    /// `proposal_json` (the structured plan) is inlined under `proposal`.
    pub fn to_json(&self) -> serde_json::Value {
        let plan: serde_json::Value =
            serde_json::from_str(&self.proposal_json).unwrap_or(serde_json::Value::Null);
        let created: serde_json::Value =
            serde_json::from_str(&self.created_brief_ids).unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "proposal_id": self.proposal_id,
            "status": self.status,
            "message": self.message,
            "mandate_id": if self.mandate_id.is_empty() { serde_json::Value::Null } else { serde_json::json!(self.mandate_id) },
            "created_brief_ids": created,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "proposal": plan,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpineStoreError {
    #[error("spine store: {0}")]
    Io(String),
    #[error("spine store: db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("spine store: not found: {0}")]
    NotFound(String),
    #[error("spine store: bad input: {0}")]
    BadInput(String),
    #[error("spine store: poisoned mutex")]
    Lock,
}

// ── Status vocabularies ───────────────────────────────────

fn is_mandate_status(s: &str) -> bool {
    matches!(s, "planned" | "active" | "achieved" | "cancelled")
}

fn is_campaign_status(s: &str) -> bool {
    matches!(
        s,
        "backlog" | "planned" | "in_progress" | "completed" | "cancelled"
    )
}

// ── Store ─────────────────────────────────────────────────

pub struct SpineStore {
    conn: Arc<Mutex<Connection>>,
}

impl SpineStore {
    pub fn open(path: &Path) -> Result<Self, SpineStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SpineStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "spine_store");
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, SpineStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ── guilds (the Company face of a tenant) ────────────

    /// Set (upsert) a Guild's display name for `tenant_id`.
    pub fn set_guild_name(
        &self,
        tenant_id: &str,
        display_name: &str,
    ) -> Result<(), SpineStoreError> {
        if display_name.trim().is_empty() {
            return Err(SpineStoreError::BadInput(
                "guild display_name required".into(),
            ));
        }
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        conn.execute(
            "INSERT INTO guilds (tenant_id, display_name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(tenant_id) DO UPDATE SET display_name = ?2, updated_at = ?3",
            params![tenant, display_name.trim(), now],
        )?;
        Ok(())
    }

    /// Read a Guild by tenant id. `None` until a name or Allowance
    /// is set.
    pub fn get_guild(&self, tenant_id: &str) -> Result<Option<Guild>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT tenant_id, display_name, monthly_allowance_cents,
                        billing_code, created_at, updated_at
                 FROM guilds WHERE tenant_id = ?1",
                params![normalize_tenant(tenant_id)],
                |r| {
                    Ok(Guild {
                        tenant_id: r.get(0)?,
                        display_name: r.get(1)?,
                        monthly_allowance_cents: r.get(2)?,
                        billing_code: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Set or clear a Guild's monthly **Allowance** (cents). `None`
    /// clears the cap. Creates the Guild (named after the tenant) if
    /// it doesn't exist yet.
    pub fn set_guild_allowance(
        &self,
        tenant_id: &str,
        cents: Option<i64>,
    ) -> Result<(), SpineStoreError> {
        if let Some(c) = cents
            && c < 0
        {
            return Err(SpineStoreError::BadInput("allowance must be >= 0".into()));
        }
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        conn.execute(
            "INSERT INTO guilds
                 (tenant_id, display_name, monthly_allowance_cents, created_at, updated_at)
             VALUES (?1, ?1, ?2, ?3, ?3)
             ON CONFLICT(tenant_id)
                 DO UPDATE SET monthly_allowance_cents = ?2, updated_at = ?3",
            params![tenant, cents, now],
        )?;
        Ok(())
    }

    /// Set or clear a Guild's OBJECT-LEVEL **billing code**
    /// (company-model §6.6). `None`/empty clears it. Creates the Guild
    /// (named after the tenant) if it doesn't exist yet. Mirrors
    /// [`set_guild_allowance`](Self::set_guild_allowance).
    pub fn set_guild_billing_code(
        &self,
        tenant_id: &str,
        code: Option<&str>,
    ) -> Result<(), SpineStoreError> {
        let tenant = normalize_tenant(tenant_id);
        let code = code.and_then(non_empty);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        conn.execute(
            "INSERT INTO guilds
                 (tenant_id, display_name, billing_code, created_at, updated_at)
             VALUES (?1, ?1, ?2, ?3, ?3)
             ON CONFLICT(tenant_id)
                 DO UPDATE SET billing_code = ?2, updated_at = ?3",
            params![tenant, code, now],
        )?;
        Ok(())
    }

    // ── mandate strategy gate (Phase 4) ──────────────────

    /// Propose (or re-propose) a strategy for a Mandate — sets it
    /// `proposed` and stores the plan `doc`.
    ///
    /// NOTE: this is the *queryable* half of the gate
    /// (`strategy_approved` is the predicate a hire / team-build path
    /// must check). Wiring it as a hard pre-condition on hiring (the
    /// company-model §10.3 enforcement) requires the hire→Mandate
    /// coupling and is tracked as follow-up — today nothing in the
    /// runtime blocks a hire on this predicate, so do NOT describe it
    /// as "enforced" until that wiring lands.
    ///
    /// Tenant-guarded: the Mandate must belong to `tenant` (a Guild
    /// can't touch another's).
    pub fn propose_strategy(
        &self,
        tenant: &str,
        mandate_id: &str,
        doc: &str,
    ) -> Result<(), SpineStoreError> {
        let now = unix_now();
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, mandate_id, tenant)?;
        conn.execute(
            "INSERT INTO mandate_strategy (mandate_id, status, doc, updated_at)
             VALUES (?1, 'proposed', ?2, ?3)
             ON CONFLICT(mandate_id)
                 DO UPDATE SET status='proposed', doc=?2, updated_at=?3",
            params![mandate_id, doc, now],
        )?;
        Ok(())
    }

    /// Approve a proposed strategy (proposed → approved). Tenant-guarded.
    pub fn approve_strategy(&self, tenant: &str, mandate_id: &str) -> Result<(), SpineStoreError> {
        self.decide_strategy(tenant, mandate_id, "approved")
    }

    /// Reject a proposed strategy (proposed → rejected). Tenant-guarded.
    pub fn reject_strategy(&self, tenant: &str, mandate_id: &str) -> Result<(), SpineStoreError> {
        self.decide_strategy(tenant, mandate_id, "rejected")
    }

    fn decide_strategy(
        &self,
        tenant: &str,
        mandate_id: &str,
        to: &str,
    ) -> Result<(), SpineStoreError> {
        let now = unix_now();
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, mandate_id, tenant)?;
        let changed = conn.execute(
            "UPDATE mandate_strategy SET status=?1, updated_at=?2
             WHERE mandate_id=?3 AND status='proposed'",
            params![to, now, mandate_id],
        )?;
        if changed == 0 {
            return Err(SpineStoreError::NotFound(format!(
                "no proposed strategy for {mandate_id}"
            )));
        }
        Ok(())
    }

    /// The Mandate's strategy status (`proposed`/`approved`/`rejected`),
    /// or `None` if none was proposed. Tenant-guarded.
    pub fn strategy_status(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<Option<String>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, mandate_id, tenant)?;
        let row = conn
            .query_row(
                "SELECT status FROM mandate_strategy WHERE mandate_id = ?1",
                params![mandate_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// The Mandate's strategy doc text (the proposed/approved/rejected body),
    /// or `None` if none was proposed. Tenant-guarded. Read-only accessor used to
    /// surface the proposed strategy for review (and to assert provenance in
    /// tests).
    pub fn strategy_doc(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<Option<String>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, mandate_id, tenant)?;
        let row = conn
            .query_row(
                "SELECT doc FROM mandate_strategy WHERE mandate_id = ?1",
                params![mandate_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Is the Mandate's strategy approved? The gate the CEO/hire flow
    /// checks before spawning a team. Tenant-guarded.
    pub fn strategy_approved(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<bool, SpineStoreError> {
        Ok(self.strategy_status(tenant, mandate_id)?.as_deref() == Some("approved"))
    }

    // ── mandate team plans (Prime team-build) ────────────────

    /// Persist a Team Plan for a Mandate (`mandate.team_plan`). The
    /// Mandate must exist in `tenant`. Returns the new `plan_id`. Each
    /// call appends a new row; `latest_team_plan` reads the newest.
    pub fn record_team_plan(&self, rec: &TeamPlanRecord) -> Result<String, SpineStoreError> {
        let tenant = normalize_tenant(rec.tenant_id);
        let now = unix_now();
        let plan_id = new_plan_id();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, rec.mandate_id, tenant)?;
        conn.execute(
            "INSERT INTO mandate_team_plans (
                 plan_id, tenant_id, mandate_id, actor_id, description,
                 proposed_roles, pending_hires, clearance_ids, denials,
                 next_steps, status, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![
                plan_id,
                tenant,
                rec.mandate_id,
                rec.actor_id,
                rec.description,
                rec.proposed_roles_json,
                rec.pending_hires_json,
                rec.clearance_ids_json,
                rec.denials_json,
                rec.next_steps_json,
                rec.status,
                now,
            ],
        )?;
        Ok(plan_id)
    }

    /// The most recent Team Plan for a Mandate, scoped to `tenant` —
    /// `None` if the Mandate has never been planned. A known `plan` /
    /// `mandate_id` from another Guild reads as `None` (tenant
    /// isolation).
    pub fn latest_team_plan(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<Option<TeamPlan>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT plan_id, tenant_id, mandate_id, actor_id, description,
                        proposed_roles, pending_hires, clearance_ids, denials,
                        next_steps, status, created_at, updated_at
                 FROM mandate_team_plans
                 WHERE tenant_id = ?1 AND mandate_id = ?2
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                params![tenant, mandate_id],
                row_to_team_plan,
            )
            .optional()?;
        Ok(row)
    }

    // ── mandate orchestration runs ───────────────────────────

    /// Persist one orchestration run. The Mandate must exist in
    /// `tenant`. Returns the new `run_id`. Each call appends a row;
    /// `latest_orchestration_run` reads the newest.
    pub fn record_orchestration_run(
        &self,
        rec: &OrchestrationRunRecord,
    ) -> Result<String, SpineStoreError> {
        let tenant = normalize_tenant(rec.tenant_id);
        let now = unix_now();
        let run_id = new_run_id();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        require_mandate_in_tenant(&conn, rec.mandate_id, tenant)?;
        conn.execute(
            "INSERT INTO mandate_orchestration_runs (
                 run_id, tenant_id, mandate_id, mode, dry_run, input_signature,
                 status, created_brief_ids, existing_brief_ids, assigned_brief_ids,
                 skipped, source_markers, blockers, next_actions, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                run_id,
                tenant,
                rec.mandate_id,
                rec.mode,
                if rec.dry_run { 1 } else { 0 },
                rec.input_signature,
                rec.status,
                rec.created_brief_ids_json,
                rec.existing_brief_ids_json,
                rec.assigned_brief_ids_json,
                rec.skipped_json,
                rec.source_markers_json,
                rec.blockers_json,
                rec.next_actions_json,
                now,
            ],
        )?;
        Ok(run_id)
    }

    /// The most recent orchestration run for a Mandate, scoped to
    /// `tenant` (`None` if never orchestrated; cross-Guild reads as
    /// `None`).
    pub fn latest_orchestration_run(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<Option<OrchestrationRun>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT run_id, tenant_id, mandate_id, mode, dry_run, input_signature,
                        status, created_brief_ids, existing_brief_ids, assigned_brief_ids,
                        skipped, source_markers, blockers, next_actions, created_at
                 FROM mandate_orchestration_runs
                 WHERE tenant_id = ?1 AND mandate_id = ?2
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT 1",
                params![tenant, mandate_id],
                row_to_orchestration_run,
            )
            .optional()?;
        Ok(row)
    }

    /// Recent orchestration runs for a Mandate (newest first), scoped
    /// to `tenant`.
    pub fn list_orchestration_runs(
        &self,
        tenant: &str,
        mandate_id: &str,
        limit: usize,
    ) -> Result<Vec<OrchestrationRun>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let cap = limit.clamp(1, 200) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT run_id, tenant_id, mandate_id, mode, dry_run, input_signature,
                    status, created_brief_ids, existing_brief_ids, assigned_brief_ids,
                    skipped, source_markers, blockers, next_actions, created_at
             FROM mandate_orchestration_runs
             WHERE tenant_id = ?1 AND mandate_id = ?2
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![tenant, mandate_id, cap], row_to_orchestration_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ── Prime Assistant proposals ────────────────────────────
    //
    // A Prime proposal is the governed "describe what you want → plan"
    // record: it is written by `prime.propose` (read-only — NOTHING else is
    // mutated) and later flipped to `approved` by `prime.approve`, which is
    // the ONLY path that creates the Mandate + Briefs. There is no Mandate at
    // propose time (mandate_id is empty until approval), so this table does
    // NOT FK to `mandates`. Tenant-scoped: a proposal from another Guild
    // reads as `None`, never leaking its existence.

    /// Persist a fresh Prime proposal (status `proposed`). `message` is the
    /// (already secret-redacted) operator request; `proposal_json` is the
    /// full structured plan. Returns the new `proposal_id`.
    pub fn record_prime_proposal(
        &self,
        tenant_id: &str,
        proposer_id: &str,
        message: &str,
        proposal_json: &str,
    ) -> Result<String, SpineStoreError> {
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let proposal_id = new_proposal_id();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        conn.execute(
            "INSERT INTO prime_proposals
                 (proposal_id, tenant_id, proposer_id, message, proposal_json,
                  status, mandate_id, created_brief_ids, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'proposed', '', '[]', ?6, ?6)",
            params![
                proposal_id,
                tenant,
                proposer_id,
                message,
                proposal_json,
                now
            ],
        )?;
        Ok(proposal_id)
    }

    /// One proposal, scoped to `tenant` — `None` for an unknown id OR a
    /// proposal owned by another Guild (no cross-tenant existence leak).
    pub fn get_prime_proposal(
        &self,
        tenant: &str,
        proposal_id: &str,
    ) -> Result<Option<PrimeProposalRow>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT proposal_id, tenant_id, proposer_id, message, proposal_json,
                        status, mandate_id, created_brief_ids, created_at, updated_at
                 FROM prime_proposals
                 WHERE tenant_id = ?1 AND proposal_id = ?2",
                params![tenant, proposal_id],
                row_to_prime_proposal,
            )
            .optional()?;
        Ok(row)
    }

    /// Flip a proposal to `approved`, stamping the created Mandate + Brief
    /// ids. Idempotent at the SQL layer (the caller gates re-approval first).
    /// Returns whether a row changed (false = unknown id / wrong tenant).
    pub fn mark_prime_proposal_approved(
        &self,
        tenant: &str,
        proposal_id: &str,
        mandate_id: &str,
        created_brief_ids_json: &str,
    ) -> Result<bool, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let n = conn.execute(
            "UPDATE prime_proposals
                 SET status = 'approved', mandate_id = ?3,
                     created_brief_ids = ?4, updated_at = ?5
             WHERE tenant_id = ?1 AND proposal_id = ?2 AND status = 'proposed'",
            params![tenant, proposal_id, mandate_id, created_brief_ids_json, now],
        )?;
        Ok(n == 1)
    }

    /// Recent proposals for a Guild, newest first (for the companion history).
    pub fn list_prime_proposals(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<PrimeProposalRow>, SpineStoreError> {
        let tenant = normalize_tenant(tenant);
        let cap = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT proposal_id, tenant_id, proposer_id, message, proposal_json,
                    status, mandate_id, created_brief_ids, created_at, updated_at
             FROM prime_proposals
             WHERE tenant_id = ?1
             ORDER BY created_at DESC, rowid DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![tenant, cap], row_to_prime_proposal)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Approved Prime proposals that already materialised a Mandate — the
    /// autonomous Prime driver's primary candidate set (these carry the
    /// `prime.start` capability). Bounded; **oldest-first** so older approved
    /// work progresses first. `tenant=None` spans **all** Guilds (each row
    /// carries its own `tenant_id`, so a cross-Guild tick processes each under
    /// its own tenant); `tenant=Some(g)` scopes to one Guild only.
    pub fn list_approved_prime_proposals(
        &self,
        tenant: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PrimeProposalRow>, SpineStoreError> {
        let cap = limit.clamp(1, 200) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let cols = "proposal_id, tenant_id, proposer_id, message, proposal_json,
                    status, mandate_id, created_brief_ids, created_at, updated_at";
        let rows = match tenant {
            Some(t) => {
                let t = normalize_tenant(t);
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM prime_proposals
                     WHERE tenant_id = ?1 AND status = 'approved' AND mandate_id <> ''
                     ORDER BY created_at ASC, rowid ASC LIMIT ?2"
                ))?;
                stmt.query_map(params![t, cap], row_to_prime_proposal)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM prime_proposals
                     WHERE status = 'approved' AND mandate_id <> ''
                     ORDER BY created_at ASC, rowid ASC LIMIT ?1"
                ))?;
                stmt.query_map(params![cap], row_to_prime_proposal)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// PROPOSED (not-yet-decided) Prime proposals — the autonomous Prime
    /// **standing-authority** driver's proposal-approval candidate set. Bounded;
    /// **oldest-first** so older pending work is approved first. Excludes
    /// `approved` / `rejected` rows (so an already-approved proposal is never
    /// re-approved and a rejected one is never resurrected). `tenant=None` spans
    /// **all** Guilds (each row carries its own `tenant_id`, so the caller checks
    /// standing authority per the row's own Guild); `tenant=Some(g)` scopes to
    /// one Guild only.
    pub fn list_proposed_prime_proposals(
        &self,
        tenant: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PrimeProposalRow>, SpineStoreError> {
        let cap = limit.clamp(1, 200) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let cols = "proposal_id, tenant_id, proposer_id, message, proposal_json,
                    status, mandate_id, created_brief_ids, created_at, updated_at";
        let rows = match tenant {
            Some(t) => {
                let t = normalize_tenant(t);
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM prime_proposals
                     WHERE tenant_id = ?1 AND status = 'proposed'
                     ORDER BY created_at ASC, rowid ASC LIMIT ?2"
                ))?;
                stmt.query_map(params![t, cap], row_to_prime_proposal)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM prime_proposals
                     WHERE status = 'proposed'
                     ORDER BY created_at ASC, rowid ASC LIMIT ?1"
                ))?;
                stmt.query_map(params![cap], row_to_prime_proposal)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    // ── runtime settings (tenant-scoped product toggles) ─────
    //
    // A small generic key→value store for tenant-scoped RUNTIME switches an
    // operator flips from the product (no restart / no env edit). Deliberately
    // generic so future runtime toggles reuse it; only the autonomous-Prime
    // switch is exposed today. No existence leak: a read for an unset
    // tenant+key returns `None`, exactly like a never-written value.

    /// Read the raw string value of a runtime setting for `tenant_id`+`key`.
    /// `None` when unset (a missing row reads identically to an unset value).
    pub fn get_runtime_setting(
        &self,
        tenant_id: &str,
        key: &str,
    ) -> Result<Option<String>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT value FROM runtime_settings WHERE tenant_id = ?1 AND key = ?2",
                params![normalize_tenant(tenant_id), key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
    }

    /// Upsert a runtime setting for `tenant_id`+`key`. `updated_by` records who
    /// last flipped it (operator subject / label) for the audit trail.
    pub fn set_runtime_setting(
        &self,
        tenant_id: &str,
        key: &str,
        value: &str,
        updated_by: &str,
    ) -> Result<(), SpineStoreError> {
        if key.trim().is_empty() {
            return Err(SpineStoreError::BadInput(
                "runtime setting key required".into(),
            ));
        }
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        conn.execute(
            "INSERT INTO runtime_settings (tenant_id, key, value, updated_at, updated_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(tenant_id, key)
                 DO UPDATE SET value = ?3, updated_at = ?4, updated_by = ?5",
            params![tenant, key, value, now, updated_by],
        )?;
        Ok(())
    }

    /// Read a runtime setting as a bool for `tenant_id`+`key`. `None` when
    /// unset; `Some(true/false)` parsed from any truthy/falsey spelling so a
    /// value written by the bool setter (canonical `1`/`0`) or by hand is read
    /// consistently.
    pub fn get_runtime_setting_bool(
        &self,
        tenant_id: &str,
        key: &str,
    ) -> Result<Option<bool>, SpineStoreError> {
        Ok(self
            .get_runtime_setting(tenant_id, key)?
            .map(|v| runtime_truthy(&v)))
    }

    /// Set a bool runtime setting for `tenant_id`+`key`, stored canonically as
    /// `1`/`0` so the [`list_tenants_with_runtime_bool`] filter is exact.
    ///
    /// [`list_tenants_with_runtime_bool`]: Self::list_tenants_with_runtime_bool
    pub fn set_runtime_setting_bool(
        &self,
        tenant_id: &str,
        key: &str,
        enabled: bool,
        updated_by: &str,
    ) -> Result<(), SpineStoreError> {
        self.set_runtime_setting(tenant_id, key, if enabled { "1" } else { "0" }, updated_by)
    }

    /// List the tenants whose runtime bool `key` is currently **truthy** —
    /// the autonomous-Prime watcher's per-Guild enable set when the env
    /// override is off. Bounded read; tenant order is stable (by tenant id).
    /// Matches any truthy spelling, not just the canonical `1`, so a
    /// hand-written value still counts.
    pub fn list_tenants_with_runtime_bool(
        &self,
        key: &str,
    ) -> Result<Vec<String>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT tenant_id, value FROM runtime_settings WHERE key = ?1 ORDER BY tenant_id ASC",
        )?;
        let rows = stmt
            .query_map(params![key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|(_, v)| runtime_truthy(v))
            .map(|(t, _)| t)
            .collect())
    }

    // ── mandates ─────────────────────────────────────────────

    /// Create a Mandate. Returns the freshly-allocated `mandate_id`.
    /// `parent_mandate_id`, when set, must reference an existing
    /// mandate in the same tenant.
    pub fn create_mandate(
        &self,
        tenant_id: &str,
        title: &str,
        description: &str,
        owner_agent_id: Option<&str>,
        parent_mandate_id: Option<&str>,
    ) -> Result<String, SpineStoreError> {
        if title.trim().is_empty() {
            return Err(SpineStoreError::BadInput("title required".into()));
        }
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let mandate_id = new_mandate_id();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        if let Some(parent) = parent_mandate_id.and_then(non_empty) {
            require_mandate_in_tenant(&conn, parent, tenant)?;
        }
        conn.execute(
            "INSERT INTO mandates (
                 mandate_id, tenant_id, title, description, owner_agent_id,
                 status, parent_mandate_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'planned', ?6, ?7, ?7)",
            params![
                mandate_id,
                tenant,
                title.trim(),
                description.trim(),
                owner_agent_id.and_then(non_empty),
                parent_mandate_id.and_then(non_empty),
                now,
            ],
        )?;
        Ok(mandate_id)
    }

    pub fn get_mandate(&self, mandate_id: &str) -> Result<Option<Mandate>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_MANDATE, params![mandate_id], row_to_mandate)
            .optional()?;
        Ok(row)
    }

    /// Tenant-scoped mandate read — returns the mandate ONLY when it
    /// belongs to `tenant`, so tenant A cannot read tenant B's
    /// mandate by id.
    pub fn get_mandate_for_tenant(
        &self,
        mandate_id: &str,
        tenant: &str,
    ) -> Result<Option<Mandate>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
                        status, parent_mandate_id, billing_code, created_at, updated_at
                 FROM mandates WHERE mandate_id = ?1 AND tenant_id = ?2",
                params![mandate_id, normalize_tenant(tenant)],
                row_to_mandate,
            )
            .optional()?;
        Ok(row)
    }

    /// List a tenant's mandates, newest first. `status_filter`
    /// narrows to one status when set.
    pub fn list_mandates(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
    ) -> Result<Vec<Mandate>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let tenant = normalize_tenant(tenant);
        let (sql, with_status) = match status_filter.and_then(non_empty) {
            Some(_) => (
                "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
                        status, parent_mandate_id, billing_code, created_at, updated_at
                 FROM mandates WHERE tenant_id = ?1 AND status = ?2
                 ORDER BY created_at DESC",
                true,
            ),
            None => (
                "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
                        status, parent_mandate_id, billing_code, created_at, updated_at
                 FROM mandates WHERE tenant_id = ?1 ORDER BY created_at DESC",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<Mandate> = if with_status {
            stmt.query_map(params![tenant, status_filter.unwrap()], row_to_mandate)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map(params![tenant], row_to_mandate)?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// Live (`planned` / `active`) Mandates — the autonomous Prime driver's
    /// bare-Mandate candidate set (it may plan / orchestrate these; per-Brief
    /// runs are left to the heartbeat / `brief.run`). Bounded; **oldest-first**.
    /// `tenant=None` spans **all** Guilds (each row carries its own `tenant_id`),
    /// `tenant=Some(g)` scopes to one Guild only.
    pub fn list_active_mandates(
        &self,
        tenant: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Mandate>, SpineStoreError> {
        let cap = limit.clamp(1, 200) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let cols = "mandate_id, tenant_id, title, description, owner_agent_id,
                    status, parent_mandate_id, billing_code, created_at, updated_at";
        let rows = match tenant {
            Some(t) => {
                let t = normalize_tenant(t);
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM mandates
                     WHERE tenant_id = ?1 AND status IN ('planned', 'active')
                     ORDER BY created_at ASC, rowid ASC LIMIT ?2"
                ))?;
                stmt.query_map(params![t, cap], row_to_mandate)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM mandates
                     WHERE status IN ('planned', 'active')
                     ORDER BY created_at ASC, rowid ASC LIMIT ?1"
                ))?;
                stmt.query_map(params![cap], row_to_mandate)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// PHASE 5 (companion): Mandates whose title contains `query`
    /// (case-insensitive substring, LIKE wildcards escaped),
    /// tenant-scoped, newest first. Empty `query` → empty result.
    pub fn search_mandates(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Mandate>, SpineStoreError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let tenant = normalize_tenant(tenant);
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let lim = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
                    status, parent_mandate_id, billing_code, created_at, updated_at
             FROM mandates
             WHERE tenant_id = ?1 AND title LIKE ?2 ESCAPE '\\'
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows: Vec<Mandate> = stmt
            .query_map(params![tenant, pattern, lim], row_to_mandate)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 1 (spine tree): a Mandate's direct child Mandates
    /// (sub-Mandates), newest first. Tenant-scoped — the drill-down
    /// for a nested-Mandate tree in the dashboard.
    pub fn list_child_mandates(
        &self,
        tenant: &str,
        parent_mandate_id: &str,
    ) -> Result<Vec<Mandate>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let tenant = normalize_tenant(tenant);
        let mut stmt = conn.prepare(
            "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
                    status, parent_mandate_id, billing_code, created_at, updated_at
             FROM mandates WHERE tenant_id = ?1 AND parent_mandate_id = ?2
             ORDER BY created_at DESC",
        )?;
        let rows: Vec<Mandate> = stmt
            .query_map(params![tenant, parent_mandate_id], row_to_mandate)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 1 (spine tree): a Mandate plus its immediate spine
    /// children (direct sub-Mandates + its Campaigns) in one
    /// tenant-scoped read. `None` when the Mandate doesn't exist in
    /// `tenant`.
    pub fn mandate_tree(
        &self,
        tenant: &str,
        mandate_id: &str,
    ) -> Result<Option<MandateTree>, SpineStoreError> {
        let Some(mandate) = self.get_mandate_for_tenant(mandate_id, tenant)? else {
            return Ok(None);
        };
        Ok(Some(MandateTree {
            mandate,
            child_mandates: self.list_child_mandates(tenant, mandate_id)?,
            campaigns: self.list_campaigns(tenant, Some(mandate_id))?,
        }))
    }

    /// PHASE 5 (companion): the Guild's spine counts in one
    /// tenant-scoped read — Mandate & Campaign totals plus the
    /// in-flight subset.
    pub fn guild_counts(&self, tenant: &str) -> Result<SpineCounts, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let tenant = normalize_tenant(tenant);
        let one = |sql: &str, status: Option<&str>| -> rusqlite::Result<i64> {
            match status {
                Some(s) => conn.query_row(sql, params![tenant, s], |r| r.get(0)),
                None => conn.query_row(sql, params![tenant], |r| r.get(0)),
            }
        };
        Ok(SpineCounts {
            mandates_total: one("SELECT COUNT(*) FROM mandates WHERE tenant_id = ?1", None)?,
            mandates_active: one(
                "SELECT COUNT(*) FROM mandates WHERE tenant_id = ?1 AND status = ?2",
                Some("active"),
            )?,
            campaigns_total: one("SELECT COUNT(*) FROM campaigns WHERE tenant_id = ?1", None)?,
            campaigns_active: one(
                "SELECT COUNT(*) FROM campaigns WHERE tenant_id = ?1 AND status = ?2",
                Some("in_progress"),
            )?,
        })
    }

    /// Update one writable mandate field. Writable: `status`,
    /// `title`, `description`, `owner_agent_id`,
    /// `parent_mandate_id`, `billing_code`. A `parent_mandate_id` change is validated
    /// (must exist in-tenant, no self-parent, no cycle).
    pub fn update_mandate_field(
        &self,
        mandate_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), SpineStoreError> {
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let changed = match field {
            "status" => {
                if !is_mandate_status(value) {
                    return Err(SpineStoreError::BadInput(format!(
                        "mandate status '{value}' not in planned/active/achieved/cancelled"
                    )));
                }
                conn.execute(
                    "UPDATE mandates SET status=?1, updated_at=?2 WHERE mandate_id=?3",
                    params![value, now, mandate_id],
                )?
            }
            "title" => {
                if value.trim().is_empty() {
                    return Err(SpineStoreError::BadInput("title required".into()));
                }
                conn.execute(
                    "UPDATE mandates SET title=?1, updated_at=?2 WHERE mandate_id=?3",
                    params![value.trim(), now, mandate_id],
                )?
            }
            "description" => conn.execute(
                "UPDATE mandates SET description=?1, updated_at=?2 WHERE mandate_id=?3",
                params![value.trim(), now, mandate_id],
            )?,
            "owner_agent_id" => conn.execute(
                "UPDATE mandates SET owner_agent_id=?1, updated_at=?2 WHERE mandate_id=?3",
                params![non_empty(value), now, mandate_id],
            )?,
            "parent_mandate_id" => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    conn.execute(
                        "UPDATE mandates SET parent_mandate_id=NULL, updated_at=?1 WHERE mandate_id=?2",
                        params![now, mandate_id],
                    )?
                } else {
                    if trimmed == mandate_id {
                        return Err(SpineStoreError::BadInput(
                            "a mandate cannot be its own parent".into(),
                        ));
                    }
                    // Parent must exist in the same tenant.
                    let tenant: Option<String> = conn
                        .query_row(
                            "SELECT tenant_id FROM mandates WHERE mandate_id=?1",
                            params![mandate_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let Some(tenant) = tenant else {
                        return Err(SpineStoreError::NotFound(mandate_id.into()));
                    };
                    require_mandate_in_tenant(&conn, trimmed, &tenant)?;
                    if creates_mandate_cycle(&conn, mandate_id, trimmed)? {
                        return Err(SpineStoreError::BadInput(
                            "parent change would create a mandate cycle".into(),
                        ));
                    }
                    conn.execute(
                        "UPDATE mandates SET parent_mandate_id=?1, updated_at=?2 WHERE mandate_id=?3",
                        params![trimmed, now, mandate_id],
                    )?
                }
            }
            // OBJECT-LEVEL billing code (company-model §6.6). Empty clears it.
            "billing_code" => {
                let t = value.trim();
                let stored: Option<&str> = if t.is_empty() { None } else { Some(t) };
                conn.execute(
                    "UPDATE mandates SET billing_code=?1, updated_at=?2 WHERE mandate_id=?3",
                    params![stored, now, mandate_id],
                )?
            }
            other => {
                return Err(SpineStoreError::BadInput(format!(
                    "unknown mandate field '{other}'"
                )));
            }
        };
        if changed == 0 {
            return Err(SpineStoreError::NotFound(mandate_id.into()));
        }
        Ok(())
    }

    // ── campaigns ──────────────────────────────────────────

    /// Create a Campaign. `mandate_id`, when set, must reference an
    /// existing mandate in the same tenant.
    pub fn create_campaign(
        &self,
        tenant_id: &str,
        title: &str,
        mandate_id: Option<&str>,
        lead_agent_id: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<String, SpineStoreError> {
        if title.trim().is_empty() {
            return Err(SpineStoreError::BadInput("title required".into()));
        }
        let tenant = normalize_tenant(tenant_id);
        let now = unix_now();
        let campaign_id = new_campaign_id();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        if let Some(mandate) = mandate_id.and_then(non_empty) {
            require_mandate_in_tenant(&conn, mandate, tenant)?;
        }
        conn.execute(
            "INSERT INTO campaigns (
                 campaign_id, tenant_id, title, mandate_id, lead_agent_id,
                 status, workspace, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'backlog', ?6, ?7, ?7)",
            params![
                campaign_id,
                tenant,
                title.trim(),
                mandate_id.and_then(non_empty),
                lead_agent_id.and_then(non_empty),
                workspace.and_then(non_empty),
                now,
            ],
        )?;
        Ok(campaign_id)
    }

    pub fn get_campaign(&self, campaign_id: &str) -> Result<Option<Campaign>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_CAMPAIGN, params![campaign_id], row_to_campaign)
            .optional()?;
        Ok(row)
    }

    /// Tenant-scoped campaign read.
    pub fn get_campaign_for_tenant(
        &self,
        campaign_id: &str,
        tenant: &str,
    ) -> Result<Option<Campaign>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT campaign_id, tenant_id, title, mandate_id, lead_agent_id,
                        status, workspace, billing_code, created_at, updated_at
                 FROM campaigns WHERE campaign_id = ?1 AND tenant_id = ?2",
                params![campaign_id, normalize_tenant(tenant)],
                row_to_campaign,
            )
            .optional()?;
        Ok(row)
    }

    /// List a tenant's campaigns, newest first. `mandate_filter`
    /// narrows to one mandate's workstreams when set.
    pub fn list_campaigns(
        &self,
        tenant: &str,
        mandate_filter: Option<&str>,
    ) -> Result<Vec<Campaign>, SpineStoreError> {
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let tenant = normalize_tenant(tenant);
        let (sql, with_mandate) = match mandate_filter.and_then(non_empty) {
            Some(_) => (
                "SELECT campaign_id, tenant_id, title, mandate_id, lead_agent_id,
                        status, workspace, billing_code, created_at, updated_at
                 FROM campaigns WHERE tenant_id = ?1 AND mandate_id = ?2
                 ORDER BY created_at DESC",
                true,
            ),
            None => (
                "SELECT campaign_id, tenant_id, title, mandate_id, lead_agent_id,
                        status, workspace, billing_code, created_at, updated_at
                 FROM campaigns WHERE tenant_id = ?1 ORDER BY created_at DESC",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<Campaign> = if with_mandate {
            stmt.query_map(params![tenant, mandate_filter.unwrap()], row_to_campaign)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map(params![tenant], row_to_campaign)?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// PHASE 5 (companion): Campaigns whose title contains `query`
    /// (case-insensitive substring, LIKE wildcards escaped),
    /// tenant-scoped, newest first. Empty `query` → empty result.
    pub fn search_campaigns(
        &self,
        tenant: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Campaign>, SpineStoreError> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let tenant = normalize_tenant(tenant);
        let escaped = q
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let lim = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT campaign_id, tenant_id, title, mandate_id, lead_agent_id,
                    status, workspace, billing_code, created_at, updated_at
             FROM campaigns
             WHERE tenant_id = ?1 AND title LIKE ?2 ESCAPE '\\'
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows: Vec<Campaign> = stmt
            .query_map(params![tenant, pattern, lim], row_to_campaign)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Update one writable campaign field. Writable: `status`,
    /// `title`, `mandate_id`, `lead_agent_id`, `workspace`, `billing_code`.
    pub fn update_campaign_field(
        &self,
        campaign_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), SpineStoreError> {
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| SpineStoreError::Lock)?;
        let changed = match field {
            "status" => {
                if !is_campaign_status(value) {
                    return Err(SpineStoreError::BadInput(format!(
                        "campaign status '{value}' not in \
                         backlog/planned/in_progress/completed/cancelled"
                    )));
                }
                conn.execute(
                    "UPDATE campaigns SET status=?1, updated_at=?2 WHERE campaign_id=?3",
                    params![value, now, campaign_id],
                )?
            }
            "title" => {
                if value.trim().is_empty() {
                    return Err(SpineStoreError::BadInput("title required".into()));
                }
                conn.execute(
                    "UPDATE campaigns SET title=?1, updated_at=?2 WHERE campaign_id=?3",
                    params![value.trim(), now, campaign_id],
                )?
            }
            "mandate_id" => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    conn.execute(
                        "UPDATE campaigns SET mandate_id=NULL, updated_at=?1 WHERE campaign_id=?2",
                        params![now, campaign_id],
                    )?
                } else {
                    let tenant: Option<String> = conn
                        .query_row(
                            "SELECT tenant_id FROM campaigns WHERE campaign_id=?1",
                            params![campaign_id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let Some(tenant) = tenant else {
                        return Err(SpineStoreError::NotFound(campaign_id.into()));
                    };
                    require_mandate_in_tenant(&conn, trimmed, &tenant)?;
                    conn.execute(
                        "UPDATE campaigns SET mandate_id=?1, updated_at=?2 WHERE campaign_id=?3",
                        params![trimmed, now, campaign_id],
                    )?
                }
            }
            "lead_agent_id" => conn.execute(
                "UPDATE campaigns SET lead_agent_id=?1, updated_at=?2 WHERE campaign_id=?3",
                params![non_empty(value), now, campaign_id],
            )?,
            "workspace" => conn.execute(
                "UPDATE campaigns SET workspace=?1, updated_at=?2 WHERE campaign_id=?3",
                params![non_empty(value), now, campaign_id],
            )?,
            // OBJECT-LEVEL billing code (company-model §6.6). Empty clears it.
            "billing_code" => {
                let t = value.trim();
                let stored: Option<&str> = if t.is_empty() { None } else { Some(t) };
                conn.execute(
                    "UPDATE campaigns SET billing_code=?1, updated_at=?2 WHERE campaign_id=?3",
                    params![stored, now, campaign_id],
                )?
            }
            other => {
                return Err(SpineStoreError::BadInput(format!(
                    "unknown campaign field '{other}'"
                )));
            }
        };
        if changed == 0 {
            return Err(SpineStoreError::NotFound(campaign_id.into()));
        }
        Ok(())
    }
}

/// The SpineStore is the OBJECT-LEVEL billing-code source for run
/// stamping (company-model §6.6). Given a Brief's Guild (`tenant`) and
/// its Campaign/Mandate link ids, it resolves the effective object code
/// with precedence **Campaign-own → Mandate-own → Guild-own**, all
/// WITHIN `tenant`. Tenant-safe by construction: `get_campaign_for_tenant`
/// / `get_mandate_for_tenant` only return an object that belongs to
/// `tenant`, and the Guild fallback reads the Brief's OWN Guild — so a
/// stray or cross-Guild link id resolves to nothing and can never leak
/// another company's code.
impl crate::nodes::coordinator::ObjectBillingResolver for SpineStore {
    fn object_billing_code(
        &self,
        tenant: &str,
        mandate_id: Option<&str>,
        campaign_id: Option<&str>,
    ) -> Option<String> {
        // Campaign-own code first (the most specific object the Brief links).
        if let Some(cid) = campaign_id.and_then(non_empty)
            && let Ok(Some(c)) = self.get_campaign_for_tenant(cid, tenant)
            && let Some(code) = c.billing_code.as_deref().and_then(non_empty)
        {
            return Some(code.to_string());
        }
        // Then the linked Mandate's code.
        if let Some(mid) = mandate_id.and_then(non_empty)
            && let Ok(Some(m)) = self.get_mandate_for_tenant(mid, tenant)
            && let Some(code) = m.billing_code.as_deref().and_then(non_empty)
        {
            return Some(code.to_string());
        }
        // Finally the Guild-wide default code (the Brief's OWN Guild).
        if let Ok(Some(g)) = self.get_guild(tenant)
            && let Some(code) = g.billing_code.as_deref().and_then(non_empty)
        {
            return Some(code.to_string());
        }
        None
    }
}

// ── schema + helpers ──────────────────────────────────────

const SELECT_MANDATE: &str = "SELECT mandate_id, tenant_id, title, description, owner_agent_id,
        status, parent_mandate_id, billing_code, created_at, updated_at
 FROM mandates WHERE mandate_id = ?1";

const SELECT_CAMPAIGN: &str = "SELECT campaign_id, tenant_id, title, mandate_id, lead_agent_id,
        status, workspace, billing_code, created_at, updated_at
 FROM campaigns WHERE campaign_id = ?1";

fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS mandates (
             mandate_id        TEXT PRIMARY KEY,
             tenant_id      TEXT NOT NULL DEFAULT 'default',
             title          TEXT NOT NULL,
             description    TEXT NOT NULL DEFAULT '',
             owner_agent_id TEXT,
             status         TEXT NOT NULL DEFAULT 'planned',
             parent_mandate_id TEXT,
             created_at     INTEGER NOT NULL,
             updated_at     INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS mandates_tenant ON mandates(tenant_id);
         CREATE INDEX IF NOT EXISTS mandates_parent ON mandates(parent_mandate_id);

         CREATE TABLE IF NOT EXISTS campaigns (
             campaign_id     TEXT PRIMARY KEY,
             tenant_id      TEXT NOT NULL DEFAULT 'default',
             title          TEXT NOT NULL,
             mandate_id        TEXT,
             lead_agent_id  TEXT,
             status         TEXT NOT NULL DEFAULT 'backlog',
             workspace      TEXT,
             created_at     INTEGER NOT NULL,
             updated_at     INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS campaigns_tenant ON campaigns(tenant_id);
         CREATE INDEX IF NOT EXISTS campaigns_mandate ON campaigns(mandate_id);

         CREATE TABLE IF NOT EXISTS guilds (
             tenant_id    TEXT PRIMARY KEY,
             display_name TEXT NOT NULL,
             created_at   INTEGER NOT NULL,
             updated_at   INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS mandate_strategy (
             mandate_id TEXT PRIMARY KEY,
             status     TEXT NOT NULL DEFAULT 'proposed',
             doc        TEXT NOT NULL DEFAULT '',
             updated_at INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS mandate_team_plans (
             plan_id        TEXT PRIMARY KEY,
             tenant_id      TEXT NOT NULL DEFAULT 'default',
             mandate_id     TEXT NOT NULL,
             actor_id       TEXT NOT NULL DEFAULT '',
             description    TEXT NOT NULL DEFAULT '',
             proposed_roles TEXT NOT NULL DEFAULT '[]',
             pending_hires  TEXT NOT NULL DEFAULT '[]',
             clearance_ids  TEXT NOT NULL DEFAULT '[]',
             denials        TEXT NOT NULL DEFAULT '[]',
             next_steps     TEXT NOT NULL DEFAULT '[]',
             status         TEXT NOT NULL DEFAULT 'planned',
             created_at     INTEGER NOT NULL,
             updated_at     INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS mandate_team_plans_latest
             ON mandate_team_plans(tenant_id, mandate_id, created_at);

         CREATE TABLE IF NOT EXISTS mandate_orchestration_runs (
             run_id            TEXT PRIMARY KEY,
             tenant_id         TEXT NOT NULL DEFAULT 'default',
             mandate_id        TEXT NOT NULL,
             mode              TEXT NOT NULL DEFAULT 'plan_only',
             dry_run           INTEGER NOT NULL DEFAULT 0,
             input_signature   TEXT NOT NULL DEFAULT '',
             status            TEXT NOT NULL DEFAULT 'planned',
             created_brief_ids TEXT NOT NULL DEFAULT '[]',
             existing_brief_ids TEXT NOT NULL DEFAULT '[]',
             assigned_brief_ids TEXT NOT NULL DEFAULT '[]',
             skipped           TEXT NOT NULL DEFAULT '[]',
             source_markers    TEXT NOT NULL DEFAULT '[]',
             blockers          TEXT NOT NULL DEFAULT '[]',
             next_actions      TEXT NOT NULL DEFAULT '[]',
             created_at        INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS mandate_orchestration_runs_latest
             ON mandate_orchestration_runs(tenant_id, mandate_id, created_at);

         CREATE TABLE IF NOT EXISTS prime_proposals (
             proposal_id       TEXT PRIMARY KEY,
             tenant_id         TEXT NOT NULL DEFAULT 'default',
             proposer_id       TEXT NOT NULL DEFAULT '',
             message           TEXT NOT NULL DEFAULT '',
             proposal_json     TEXT NOT NULL DEFAULT '{}',
             status            TEXT NOT NULL DEFAULT 'proposed',
             mandate_id        TEXT NOT NULL DEFAULT '',
             created_brief_ids TEXT NOT NULL DEFAULT '[]',
             created_at        INTEGER NOT NULL,
             updated_at        INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS prime_proposals_tenant
             ON prime_proposals(tenant_id, created_at);

         CREATE TABLE IF NOT EXISTS runtime_settings (
             tenant_id  TEXT NOT NULL,
             key        TEXT NOT NULL,
             value      TEXT NOT NULL,
             updated_at INTEGER NOT NULL,
             updated_by TEXT NOT NULL DEFAULT '',
             PRIMARY KEY (tenant_id, key)
         );
         CREATE INDEX IF NOT EXISTS runtime_settings_key
             ON runtime_settings(key);",
    )?;
    // Defensive additive column: a Guild's monthly Allowance (cents).
    // Tolerates a guilds table created before this column existed.
    let _ = conn.execute(
        "ALTER TABLE guilds ADD COLUMN monthly_allowance_cents INTEGER",
        [],
    );
    // Defensive additive OBJECT-LEVEL billing-code columns (company-model
    // §6.6) — tolerate spine tables created before these columns existed.
    // Each runs independently; the row readers SELECT the column, so on a
    // pre-existing DB the ALTER must succeed (or already exist). NULL =
    // unset (no object-level code → falls through to the next precedence).
    for alter in [
        "ALTER TABLE mandates ADD COLUMN billing_code TEXT",
        "ALTER TABLE campaigns ADD COLUMN billing_code TEXT",
        "ALTER TABLE guilds ADD COLUMN billing_code TEXT",
    ] {
        let _ = conn.execute(alter, []);
    }
    // Defensive additive columns on orchestration runs — tolerate a table
    // created before stable source markers existed (company-model §4.6).
    // Each runs independently so a pre-existing column on one does not
    // abort the rest; the run row reader supplies '[]' for any that are
    // still absent via the column DEFAULT.
    for alter in [
        "ALTER TABLE mandate_orchestration_runs ADD COLUMN existing_brief_ids TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE mandate_orchestration_runs ADD COLUMN skipped TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE mandate_orchestration_runs ADD COLUMN source_markers TEXT NOT NULL DEFAULT '[]'",
    ] {
        let _ = conn.execute(alter, []);
    }
    Ok(())
}

/// Confirm `mandate_id` exists and belongs to `tenant`, else a
/// `BadInput` so a cross-tenant or dangling link is rejected.
fn require_mandate_in_tenant(
    conn: &Connection,
    mandate_id: &str,
    tenant: &str,
) -> Result<(), SpineStoreError> {
    let ok = conn
        .query_row(
            "SELECT 1 FROM mandates WHERE mandate_id = ?1 AND tenant_id = ?2",
            params![mandate_id, tenant],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if ok {
        Ok(())
    } else {
        Err(SpineStoreError::BadInput(format!(
            "mandate '{mandate_id}' is not a known mandate in this company"
        )))
    }
}

/// Would setting `mandate_id`'s parent to `new_parent` create a
/// cycle? True if `mandate_id` is an ancestor of `new_parent`
/// (walking up from `new_parent` reaches `mandate_id`). Depth-capped.
fn creates_mandate_cycle(
    conn: &Connection,
    mandate_id: &str,
    new_parent: &str,
) -> Result<bool, SpineStoreError> {
    const MAX_DEPTH: usize = 1024;
    let mut current = new_parent.to_string();
    for _ in 0..MAX_DEPTH {
        if current == mandate_id {
            return Ok(true);
        }
        let parent: Option<String> = conn
            .query_row(
                "SELECT parent_mandate_id FROM mandates WHERE mandate_id = ?1",
                params![current],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        match parent {
            Some(p) => current = p,
            None => return Ok(false),
        }
    }
    // Hit the depth cap → treat as a cycle, conservatively.
    Ok(true)
}

fn row_to_mandate(r: &rusqlite::Row) -> rusqlite::Result<Mandate> {
    Ok(Mandate {
        mandate_id: r.get(0)?,
        tenant_id: r.get(1)?,
        title: r.get(2)?,
        description: r.get(3)?,
        owner_agent_id: r.get(4)?,
        status: r.get(5)?,
        parent_mandate_id: r.get(6)?,
        billing_code: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

fn row_to_campaign(r: &rusqlite::Row) -> rusqlite::Result<Campaign> {
    Ok(Campaign {
        campaign_id: r.get(0)?,
        tenant_id: r.get(1)?,
        title: r.get(2)?,
        mandate_id: r.get(3)?,
        lead_agent_id: r.get(4)?,
        status: r.get(5)?,
        workspace: r.get(6)?,
        billing_code: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

fn normalize_tenant(tenant_id: &str) -> &str {
    if tenant_id.trim().is_empty() {
        "default"
    } else {
        tenant_id
    }
}

fn non_empty(s: &str) -> Option<&str> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t) }
}

/// Interpret a stored runtime-setting value as a bool. The bool setter writes
/// the canonical `1`/`0`, but this also accepts the same truthy spellings the
/// env switches use (`1`/`true`/`yes`/`on`, case-insensitive, trimmed) so a
/// hand-written value reads consistently; everything else is false.
fn runtime_truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn row_to_team_plan(r: &rusqlite::Row) -> rusqlite::Result<TeamPlan> {
    Ok(TeamPlan {
        plan_id: r.get(0)?,
        tenant_id: r.get(1)?,
        mandate_id: r.get(2)?,
        actor_id: r.get(3)?,
        description: r.get(4)?,
        proposed_roles: r.get(5)?,
        pending_hires: r.get(6)?,
        clearance_ids: r.get(7)?,
        denials: r.get(8)?,
        next_steps: r.get(9)?,
        status: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

fn new_mandate_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("mandate_{}", hex::encode(bytes))
}

fn new_plan_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("plan_{}", hex::encode(bytes))
}

fn new_run_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("orun_{}", hex::encode(bytes))
}

fn row_to_orchestration_run(r: &rusqlite::Row) -> rusqlite::Result<OrchestrationRun> {
    Ok(OrchestrationRun {
        run_id: r.get(0)?,
        tenant_id: r.get(1)?,
        mandate_id: r.get(2)?,
        mode: r.get(3)?,
        dry_run: r.get::<_, i64>(4)? != 0,
        input_signature: r.get(5)?,
        status: r.get(6)?,
        created_brief_ids: r.get(7)?,
        existing_brief_ids: r.get(8)?,
        assigned_brief_ids: r.get(9)?,
        skipped: r.get(10)?,
        source_markers: r.get(11)?,
        blockers: r.get(12)?,
        next_actions: r.get(13)?,
        created_at: r.get(14)?,
    })
}

fn new_campaign_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("camp_{}", hex::encode(bytes))
}

fn new_proposal_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("prop_{}", hex::encode(bytes))
}

fn row_to_prime_proposal(r: &rusqlite::Row) -> rusqlite::Result<PrimeProposalRow> {
    Ok(PrimeProposalRow {
        proposal_id: r.get(0)?,
        tenant_id: r.get(1)?,
        proposer_id: r.get(2)?,
        message: r.get(3)?,
        proposal_json: r.get(4)?,
        status: r.get(5)?,
        mandate_id: r.get(6)?,
        created_brief_ids: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
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
    use crate::nodes::coordinator::ObjectBillingResolver;

    fn store() -> SpineStore {
        SpineStore::in_memory().unwrap()
    }

    #[test]
    fn runtime_setting_persists_and_reads_bool() {
        let s = store();
        // Unset reads as None (no existence leak — a never-written value).
        assert_eq!(s.get_runtime_setting("acme", "k").unwrap(), None);
        assert_eq!(s.get_runtime_setting_bool("acme", "k").unwrap(), None);

        // Set bool true → canonical "1" stored, reads back true.
        s.set_runtime_setting_bool("acme", "k", true, "operator")
            .unwrap();
        assert_eq!(
            s.get_runtime_setting("acme", "k").unwrap().as_deref(),
            Some("1")
        );
        assert_eq!(s.get_runtime_setting_bool("acme", "k").unwrap(), Some(true));

        // Flip to false → "0", reads back false (NOT None — explicitly off).
        s.set_runtime_setting_bool("acme", "k", false, "operator")
            .unwrap();
        assert_eq!(
            s.get_runtime_setting_bool("acme", "k").unwrap(),
            Some(false)
        );

        // A hand-written truthy spelling is still read as true.
        s.set_runtime_setting("acme", "k", "ON", "cli").unwrap();
        assert_eq!(s.get_runtime_setting_bool("acme", "k").unwrap(), Some(true));
    }

    #[test]
    fn runtime_setting_is_tenant_isolated() {
        let s = store();
        s.set_runtime_setting_bool("acme", "flag", true, "operator")
            .unwrap();
        // Another Guild never sees acme's value.
        assert_eq!(s.get_runtime_setting_bool("globex", "flag").unwrap(), None);
        assert_eq!(
            s.get_runtime_setting_bool("acme", "flag").unwrap(),
            Some(true)
        );
    }

    #[test]
    fn list_tenants_with_runtime_bool_returns_only_truthy() {
        let s = store();
        s.set_runtime_setting_bool("acme", "flag", true, "op")
            .unwrap();
        s.set_runtime_setting_bool("globex", "flag", false, "op")
            .unwrap();
        s.set_runtime_setting_bool("initech", "flag", true, "op")
            .unwrap();
        // A different key never bleeds into this key's enabled set.
        s.set_runtime_setting_bool("acme", "other", true, "op")
            .unwrap();

        let enabled = s.list_tenants_with_runtime_bool("flag").unwrap();
        assert_eq!(enabled, vec!["acme".to_string(), "initech".to_string()]);

        // Flipping initech off removes it from the set.
        s.set_runtime_setting_bool("initech", "flag", false, "op")
            .unwrap();
        assert_eq!(
            s.list_tenants_with_runtime_bool("flag").unwrap(),
            vec!["acme".to_string()]
        );
    }

    #[test]
    fn object_billing_code_set_reads_and_resolves_with_precedence() {
        let s = store();
        // Empty: no object carries a code.
        assert_eq!(s.object_billing_code("acme", None, None), None);
        // Guild-level default.
        s.set_guild_billing_code("acme", Some("G")).unwrap();
        assert_eq!(
            s.get_guild("acme")
                .unwrap()
                .unwrap()
                .billing_code
                .as_deref(),
            Some("G")
        );
        assert_eq!(
            s.object_billing_code("acme", None, None).as_deref(),
            Some("G")
        );
        // Mandate code beats the Guild default.
        let m = s.create_mandate("acme", "M", "", None, None).unwrap();
        s.update_mandate_field(&m, "billing_code", "M-CODE")
            .unwrap();
        assert_eq!(
            s.get_mandate(&m).unwrap().unwrap().billing_code.as_deref(),
            Some("M-CODE")
        );
        assert_eq!(
            s.object_billing_code("acme", Some(&m), None).as_deref(),
            Some("M-CODE")
        );
        // Campaign code beats the Mandate code.
        let c = s
            .create_campaign("acme", "C", Some(&m), None, None)
            .unwrap();
        s.update_campaign_field(&c, "billing_code", "C-CODE")
            .unwrap();
        assert_eq!(
            s.get_campaign(&c).unwrap().unwrap().billing_code.as_deref(),
            Some("C-CODE")
        );
        assert_eq!(
            s.object_billing_code("acme", Some(&m), Some(&c)).as_deref(),
            Some("C-CODE")
        );
        // Clearing a code (empty value) falls through to the next level.
        s.update_campaign_field(&c, "billing_code", "").unwrap();
        assert_eq!(
            s.object_billing_code("acme", Some(&m), Some(&c)).as_deref(),
            Some("M-CODE")
        );
    }

    #[test]
    fn object_billing_code_is_tenant_scoped() {
        let s = store();
        // Codes live in tenant `b`.
        s.set_guild_billing_code("b", Some("B-G")).unwrap();
        let mb = s.create_mandate("b", "MB", "", None, None).unwrap();
        s.update_mandate_field(&mb, "billing_code", "B-M").unwrap();
        let cb = s.create_campaign("b", "CB", Some(&mb), None, None).unwrap();
        s.update_campaign_field(&cb, "billing_code", "B-C").unwrap();
        // A caller in tenant `a` passing tenant `b`'s ids gets nothing — the
        // ids resolve to None outside their Guild, and tenant `a` has no Guild
        // code of its own.
        assert_eq!(s.object_billing_code("a", Some(&mb), Some(&cb)), None);
        // The same ids DO resolve under tenant `b`.
        assert_eq!(
            s.object_billing_code("b", Some(&mb), Some(&cb)).as_deref(),
            Some("B-C")
        );
    }

    #[test]
    fn guild_name_upserts_and_reads_per_tenant() {
        let s = store();
        assert!(s.get_guild("acme").unwrap().is_none());
        s.set_guild_name("acme", "Acme Inc").unwrap();
        assert_eq!(
            s.get_guild("acme").unwrap().unwrap().display_name,
            "Acme Inc"
        );
        // Upsert updates in place.
        s.set_guild_name("acme", "Acme Corp").unwrap();
        assert_eq!(
            s.get_guild("acme").unwrap().unwrap().display_name,
            "Acme Corp"
        );
        // Empty name rejected.
        assert!(s.set_guild_name("acme", "  ").is_err());
        // A different tenant is a different Guild.
        assert!(s.get_guild("other").unwrap().is_none());

        // Allowance: set / clear / validate; setting it on an
        // unnamed Guild creates one named after the tenant.
        s.set_guild_allowance("acme", Some(50_000)).unwrap();
        assert_eq!(
            s.get_guild("acme")
                .unwrap()
                .unwrap()
                .monthly_allowance_cents,
            Some(50_000)
        );
        s.set_guild_allowance("acme", None).unwrap();
        assert_eq!(
            s.get_guild("acme")
                .unwrap()
                .unwrap()
                .monthly_allowance_cents,
            None
        );
        assert!(s.set_guild_allowance("acme", Some(-1)).is_err());
        s.set_guild_allowance("fresh", Some(100)).unwrap();
        let g = s.get_guild("fresh").unwrap().unwrap();
        assert_eq!(g.display_name, "fresh");
        assert_eq!(g.monthly_allowance_cents, Some(100));
    }

    #[test]
    fn search_mandates_matches_title_substring_per_tenant() {
        let s = store();
        let auth = s
            .create_mandate("acme", "Ship auth rewrite", "", None, None)
            .unwrap();
        let login = s
            .create_mandate("acme", "Fix auth login bug", "", None, None)
            .unwrap();
        s.create_mandate("acme", "Billing revamp", "", None, None)
            .unwrap();
        // Another tenant's matching mandate must not appear.
        s.create_mandate("other", "Auth for other co", "", None, None)
            .unwrap();

        let ids: std::collections::HashSet<String> = s
            .search_mandates("acme", "auth", 50)
            .unwrap()
            .into_iter()
            .map(|m| m.mandate_id)
            .collect();
        assert!(ids.contains(&auth) && ids.contains(&login));
        assert_eq!(ids.len(), 2);
        assert!(s.search_mandates("acme", "  ", 50).unwrap().is_empty());
    }

    #[test]
    fn search_campaigns_matches_title_substring_per_tenant() {
        let s = store();
        let m = s.create_mandate("acme", "M", "", None, None).unwrap();
        let a = s
            .create_campaign("acme", "Auth rewrite", Some(&m), None, None)
            .unwrap();
        let b = s
            .create_campaign("acme", "Auth hardening", Some(&m), None, None)
            .unwrap();
        s.create_campaign("acme", "Billing", Some(&m), None, None)
            .unwrap();

        let ids: std::collections::HashSet<String> = s
            .search_campaigns("acme", "auth", 50)
            .unwrap()
            .into_iter()
            .map(|c| c.campaign_id)
            .collect();
        assert!(ids.contains(&a) && ids.contains(&b));
        assert_eq!(ids.len(), 2);
        assert!(s.search_campaigns("other", "auth", 50).unwrap().is_empty());
    }

    #[test]
    fn mandate_tree_bundles_children_and_campaigns() {
        let s = store();
        let root = s.create_mandate("acme", "Company", "", None, None).unwrap();
        let sub = s
            .create_mandate("acme", "Q1", "", None, Some(&root))
            .unwrap();
        s.create_campaign("acme", "Auth", Some(&root), None, None)
            .unwrap();
        s.create_campaign("acme", "Billing", Some(&root), None, None)
            .unwrap();

        let tree = s.mandate_tree("acme", &root).unwrap().unwrap();
        assert_eq!(tree.mandate.mandate_id, root);
        assert_eq!(tree.child_mandates.len(), 1);
        assert_eq!(tree.child_mandates[0].mandate_id, sub);
        assert_eq!(tree.campaigns.len(), 2);

        // Cross-tenant / unknown → None.
        assert!(s.mandate_tree("other", &root).unwrap().is_none());
        assert!(s.mandate_tree("acme", "mandate_nope").unwrap().is_none());
    }

    #[test]
    fn list_child_mandates_returns_direct_children_per_tenant() {
        let s = store();
        let root = s.create_mandate("acme", "Company", "", None, None).unwrap();
        let c1 = s
            .create_mandate("acme", "Q1", "", None, Some(&root))
            .unwrap();
        let c2 = s
            .create_mandate("acme", "Q2", "", None, Some(&root))
            .unwrap();
        // A grandchild is NOT a direct child of root.
        let _g = s
            .create_mandate("acme", "Q1-a", "", None, Some(&c1))
            .unwrap();

        let kids: Vec<String> = s
            .list_child_mandates("acme", &root)
            .unwrap()
            .into_iter()
            .map(|m| m.mandate_id)
            .collect();
        assert_eq!(kids.len(), 2);
        assert!(kids.contains(&c1) && kids.contains(&c2));
        // c1 has exactly one child (the grandchild).
        assert_eq!(s.list_child_mandates("acme", &c1).unwrap().len(), 1);
        // Cross-tenant isolation.
        assert!(s.list_child_mandates("other", &root).unwrap().is_empty());
    }

    #[test]
    fn guild_counts_summarize_the_spine_per_tenant() {
        let s = store();
        let m1 = s.create_mandate("acme", "Ship v1", "", None, None).unwrap();
        let _m2 = s.create_mandate("acme", "Grow", "", None, None).unwrap();
        s.update_mandate_field(&m1, "status", "active").unwrap();
        let c1 = s
            .create_campaign("acme", "Auth", Some(&m1), None, None)
            .unwrap();
        let _c2 = s
            .create_campaign("acme", "Billing", Some(&m1), None, None)
            .unwrap();
        s.update_campaign_field(&c1, "status", "in_progress")
            .unwrap();
        // A different Guild's spine is counted separately.
        s.create_mandate("other", "X", "", None, None).unwrap();

        let c = s.guild_counts("acme").unwrap();
        assert_eq!(c.mandates_total, 2);
        assert_eq!(c.mandates_active, 1);
        assert_eq!(c.campaigns_total, 2);
        assert_eq!(c.campaigns_active, 1);

        let o = s.guild_counts("other").unwrap();
        assert_eq!(o.mandates_total, 1);
        assert_eq!(o.campaigns_total, 0);
    }

    #[test]
    fn strategy_gate_enforces_approval() {
        let s = store();
        let m = s.create_mandate("t", "Ship v1", "", None, None).unwrap();
        // No strategy → not approved.
        assert_eq!(s.strategy_status("t", &m).unwrap(), None);
        assert!(!s.strategy_approved("t", &m).unwrap());

        // Propose → proposed, still not approved.
        s.propose_strategy("t", &m, "1. hire 2. build").unwrap();
        assert_eq!(
            s.strategy_status("t", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!s.strategy_approved("t", &m).unwrap());

        // Approve → the gate opens.
        s.approve_strategy("t", &m).unwrap();
        assert!(s.strategy_approved("t", &m).unwrap());
        // Can't approve again (no longer proposed).
        assert!(s.approve_strategy("t", &m).is_err());

        // Reject path.
        let m2 = s.create_mandate("t", "Other", "", None, None).unwrap();
        s.propose_strategy("t", &m2, "plan").unwrap();
        s.reject_strategy("t", &m2).unwrap();
        assert_eq!(
            s.strategy_status("t", &m2).unwrap().as_deref(),
            Some("rejected")
        );
        assert!(!s.strategy_approved("t", &m2).unwrap());
    }

    #[test]
    fn strategy_actions_are_tenant_guarded() {
        let s = store();
        let m = s.create_mandate("acme", "Ship v1", "", None, None).unwrap();
        // Another Guild cannot propose / approve / reject / read the
        // strategy of acme's Mandate.
        assert!(matches!(
            s.propose_strategy("other", &m, "steal"),
            Err(SpineStoreError::BadInput(_))
        ));
        // acme proposes legitimately.
        s.propose_strategy("acme", &m, "plan").unwrap();
        assert!(matches!(
            s.approve_strategy("other", &m),
            Err(SpineStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.reject_strategy("other", &m),
            Err(SpineStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.strategy_status("other", &m),
            Err(SpineStoreError::BadInput(_))
        ));
        // Still only acme can act.
        s.approve_strategy("acme", &m).unwrap();
        assert!(s.strategy_approved("acme", &m).unwrap());
    }

    #[test]
    fn mandate_create_get_round_trips() {
        let s = store();
        let id = s
            .create_mandate("acme", "Ship v1", "the big one", Some("agt_ceo"), None)
            .unwrap();
        let g = s.get_mandate(&id).unwrap().unwrap();
        assert_eq!(g.title, "Ship v1");
        assert_eq!(g.description, "the big one");
        assert_eq!(g.owner_agent_id.as_deref(), Some("agt_ceo"));
        assert_eq!(g.status, "planned");
        assert_eq!(g.parent_mandate_id, None);
        assert_eq!(g.tenant_id, "acme");
    }

    #[test]
    fn mandate_status_is_validated() {
        let s = store();
        let id = s.create_mandate("t", "G", "", None, None).unwrap();
        assert!(s.update_mandate_field(&id, "status", "active").is_ok());
        assert_eq!(s.get_mandate(&id).unwrap().unwrap().status, "active");
        assert!(s.update_mandate_field(&id, "status", "bogus").is_err());
    }

    #[test]
    fn mandate_nesting_validates_parent_and_rejects_cycles() {
        let s = store();
        let parent = s.create_mandate("t", "Parent", "", None, None).unwrap();
        let child = s
            .create_mandate("t", "Child", "", None, Some(&parent))
            .unwrap();
        assert_eq!(
            s.get_mandate(&child)
                .unwrap()
                .unwrap()
                .parent_mandate_id
                .as_deref(),
            Some(parent.as_str())
        );
        // A mandate cannot parent itself.
        assert!(
            s.update_mandate_field(&child, "parent_mandate_id", &child)
                .is_err()
        );
        // Making the parent report to the child = a cycle → rejected.
        assert!(
            s.update_mandate_field(&parent, "parent_mandate_id", &child)
                .is_err()
        );
        // Unknown parent → rejected.
        assert!(s.create_mandate("t", "X", "", None, Some("nope")).is_err());
        // Cross-tenant parent → rejected.
        assert!(
            s.create_mandate("other", "Y", "", None, Some(&parent))
                .is_err()
        );
    }

    #[test]
    fn campaign_links_to_mandate_in_same_tenant_only() {
        let s = store();
        let mandate = s.create_mandate("acme", "G", "", None, None).unwrap();
        let proj = s
            .create_campaign(
                "acme",
                "Auth rewrite",
                Some(&mandate),
                Some("agt_lead"),
                None,
            )
            .unwrap();
        let p = s.get_campaign(&proj).unwrap().unwrap();
        assert_eq!(p.title, "Auth rewrite");
        assert_eq!(p.mandate_id.as_deref(), Some(mandate.as_str()));
        assert_eq!(p.lead_agent_id.as_deref(), Some("agt_lead"));
        assert_eq!(p.status, "backlog");
        // A campaign in another tenant cannot link this mandate.
        assert!(
            s.create_campaign("other", "P", Some(&mandate), None, None)
                .is_err()
        );
    }

    #[test]
    fn lists_are_tenant_scoped_and_filterable() {
        let s = store();
        let g_a = s.create_mandate("a", "GA", "", None, None).unwrap();
        s.update_mandate_field(&g_a, "status", "active").unwrap();
        s.create_mandate("a", "GA2", "", None, None).unwrap();
        s.create_mandate("b", "GB", "", None, None).unwrap();

        // Tenant isolation on lists.
        assert_eq!(s.list_mandates("a", None).unwrap().len(), 2);
        assert_eq!(s.list_mandates("b", None).unwrap().len(), 1);
        // Status filter.
        let active = s.list_mandates("a", Some("active")).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].mandate_id, g_a);

        // Cross-tenant reads are blocked.
        assert!(s.get_mandate_for_tenant(&g_a, "b").unwrap().is_none());
        assert!(s.get_mandate_for_tenant(&g_a, "a").unwrap().is_some());

        // Campaign mandate-filter.
        let mandate = s.create_mandate("a", "GP", "", None, None).unwrap();
        let p1 = s
            .create_campaign("a", "P1", Some(&mandate), None, None)
            .unwrap();
        s.create_campaign("a", "P2", None, None, None).unwrap();
        let under_mandate = s.list_campaigns("a", Some(&mandate)).unwrap();
        assert_eq!(under_mandate.len(), 1);
        assert_eq!(under_mandate[0].campaign_id, p1);
        assert_eq!(s.list_campaigns("a", None).unwrap().len(), 2);
    }

    #[test]
    fn team_plan_persist_latest_and_tenant_isolation() {
        let s = store();
        let m = s.create_mandate("a", "Ship", "", None, None).unwrap();
        // None until planned.
        assert!(s.latest_team_plan("a", &m).unwrap().is_none());
        let plan_id = s
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "a",
                mandate_id: &m,
                actor_id: "operator",
                description: "grow",
                proposed_roles_json: "[\"planner\"]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[\"do x\"]",
                status: "planned",
            })
            .unwrap();
        // Latest reads it back, tenant A only.
        let latest = s.latest_team_plan("a", &m).unwrap().unwrap();
        assert_eq!(latest.plan_id, plan_id);
        assert_eq!(latest.status, "planned");
        assert_eq!(
            latest.to_json()["proposed_roles"],
            serde_json::json!(["planner"])
        );
        // Tenant B cannot read tenant A's plan.
        assert!(s.latest_team_plan("b", &m).unwrap().is_none());
        // A second plan supersedes (latest = newest).
        let p2 = s
            .record_team_plan(&TeamPlanRecord {
                tenant_id: "a",
                mandate_id: &m,
                actor_id: "operator",
                description: "grow again",
                proposed_roles_json: "[]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "staffing",
            })
            .unwrap();
        assert_eq!(s.latest_team_plan("a", &m).unwrap().unwrap().plan_id, p2);
        // Recording against another tenant's mandate is rejected.
        assert!(matches!(
            s.record_team_plan(&TeamPlanRecord {
                tenant_id: "b",
                mandate_id: &m,
                actor_id: "x",
                description: "",
                proposed_roles_json: "[]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "planned",
            }),
            Err(SpineStoreError::BadInput(_))
        ));
    }

    fn orun(mandate_id: &str, tenant: &str, status: &str) -> OrchestrationRunRecord<'static> {
        // leak the borrowed ids into 'static for the test's brevity
        let mandate = Box::leak(mandate_id.to_string().into_boxed_str());
        let tenant = Box::leak(tenant.to_string().into_boxed_str());
        let status = Box::leak(status.to_string().into_boxed_str());
        OrchestrationRunRecord {
            tenant_id: tenant,
            mandate_id: mandate,
            mode: "assign_ready",
            dry_run: false,
            input_signature: "sig-1",
            status,
            created_brief_ids_json: "[\"t1\",\"t2\"]",
            existing_brief_ids_json: "[\"t0\"]",
            assigned_brief_ids_json: "[\"t2\"]",
            skipped_json: "[]",
            source_markers_json: "[\"mandate:m:parent\"]",
            blockers_json: "[]",
            next_actions_json: "[\"review\"]",
        }
    }

    #[test]
    fn orchestration_run_latest_list_and_tenant_isolation() {
        let s = store();
        let m = s.create_mandate("a", "Ship", "", None, None).unwrap();
        // None until any run.
        assert!(s.latest_orchestration_run("a", &m).unwrap().is_none());
        assert!(s.list_orchestration_runs("a", &m, 10).unwrap().is_empty());
        // Persist + read back.
        let run_id = s
            .record_orchestration_run(&orun(&m, "a", "created"))
            .unwrap();
        let latest = s.latest_orchestration_run("a", &m).unwrap().unwrap();
        assert_eq!(latest.run_id, run_id);
        assert_eq!(latest.status, "created");
        assert_eq!(latest.mode, "assign_ready");
        assert_eq!(
            latest.to_json()["created_brief_ids"],
            serde_json::json!(["t1", "t2"])
        );
        assert_eq!(
            latest.to_json()["assigned_brief_ids"],
            serde_json::json!(["t2"])
        );
        // Reused/existing, skipped and source markers round-trip too.
        assert_eq!(
            latest.to_json()["existing_brief_ids"],
            serde_json::json!(["t0"])
        );
        assert_eq!(latest.to_json()["skipped"], serde_json::json!([]));
        assert_eq!(
            latest.to_json()["source_markers"],
            serde_json::json!(["mandate:m:parent"])
        );
        // Tenant B cannot read tenant A's run.
        assert!(s.latest_orchestration_run("b", &m).unwrap().is_none());
        assert!(s.list_orchestration_runs("b", &m, 10).unwrap().is_empty());
        // A second run supersedes the latest; the list keeps both.
        let r2 = s
            .record_orchestration_run(&orun(&m, "a", "assigned"))
            .unwrap();
        assert_eq!(
            s.latest_orchestration_run("a", &m).unwrap().unwrap().run_id,
            r2
        );
        assert_eq!(s.list_orchestration_runs("a", &m, 10).unwrap().len(), 2);
        // Recording against another tenant's mandate is rejected.
        assert!(matches!(
            s.record_orchestration_run(&orun(&m, "b", "created")),
            Err(SpineStoreError::BadInput(_))
        ));
    }
}
