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

/// Marker `method` on an `approval_requests` row meaning "activate the
/// pending hire named by this row's `agent_id` when approved" — the
/// route-differentiated spawn Clearance (company-model §5.2A). The
/// decide hop in the handlers matches on this exact string.
pub const SPAWN_CLEARANCE_METHOD: &str = "agent.activate_hire";

/// Capability category recorded on a spawn Clearance row.
pub const SPAWN_CLEARANCE_CATEGORY: &str = "agents:create";

/// Lifetime of a spawn Clearance before it auto-expires (7 days). A
/// hire awaiting greenlight can reasonably wait days, unlike a
/// mid-Shift action Clearance.
pub const SPAWN_CLEARANCE_TTL_SECS: i64 = 7 * 86_400;

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
    /// DEFERRED 2: explicit allow-list of operator subject ids
    /// (hex [`relix_core::types::NodeId`] strings) authorised
    /// to record decisions on approvals minted for this agent.
    /// Empty ⇒ role-based fallback (only `operator` / `admin`
    /// roles may decide); defends against agent-self-approval
    /// for channel-originated approvals.
    pub authorized_approvers: Vec<String>,
    pub approval_timeout_secs: i64,
    /// SEC PART 1 (agent-gate default-deny): explicit profile
    /// label. The only operator-meaningful value today is
    /// `"allow-all"` — when set, the admission gate bypasses
    /// every categorical / surface / risk / approval check and
    /// admits with `matched_rule = "allow_all_profile"`. The
    /// bypass is audited as such. `None` (the default) leaves
    /// the gate's standard checks in force.
    #[serde(default)]
    pub profile: Option<String>,
    /// PHASE 0 (org tree): the single agent this one reports to —
    /// its boss. `None` for the apex (the CEO reports to the
    /// Board/operator, which is not an agent row). This one link
    /// turns the flat agent list into the org tree: walking it
    /// *up* gives the escalation chain, *down* gives a manager's
    /// subtree. Nullable; existing rows read NULL until set.
    #[serde(default)]
    pub reports_to: Option<String>,
    /// PILLAR 2 (Rig): which agent backend powers this Operative —
    /// `echo` / `hermes` / `claude` / `codex` / a remote, resolved
    /// against the Rig registry at dispatch. `None` = the Guild
    /// default Rig. Nullable; existing rows read NULL.
    #[serde(default)]
    pub rig: Option<String>,
    /// PILLAR 2 / governance: this Operative's monthly **Allowance**
    /// (budget) in cents. `None` = no per-agent cap. Nullable.
    #[serde(default)]
    pub monthly_allowance_cents: Option<i64>,
    /// Runtime Key: max live Brief runs this Operative may own at
    /// once. Paperclip allows high parallelism; Relix makes it a
    /// per-agent control. Clamped 1..=50 at write time.
    #[serde(default = "default_max_concurrent_runs")]
    pub max_concurrent_runs: i64,
    /// Runtime Key: may the scheduled heartbeat wake this Operative?
    #[serde(default = "default_true")]
    pub wake_on_timer: bool,
    /// Runtime Key: may assignment/comment/manual triggers wake this
    /// Operative on demand?
    #[serde(default = "default_true")]
    pub wake_on_demand: bool,
    // ── Org/Work Keys (company-model §5.2) ──────────────────
    /// Org Key: may this Operative spawn/hire other Operatives?
    /// Default-deny (§5.1). **Enforced** on agent-originated hires.
    #[serde(default)]
    pub can_spawn_agents: bool,
    /// Org Key: how a permitted spawn is routed — `direct` (the actor
    /// originates the pending hire) / `lead` / `founder` (route the
    /// hire up for greenlight). Default `founder` (safest).
    #[serde(default = "default_spawn_route")]
    pub spawn_route: String,
    /// Work Key: may this Operative assign Briefs to other Operatives?
    /// Default-deny. **Enforced** on agent-originated assignment.
    #[serde(default)]
    pub can_assign_work: bool,
    /// Work Key: scope of `can_assign_work` — `any` / `branch` (only
    /// the actor's Branch) / `specific` (`assign_allowed_agents`).
    /// Default `specific` (narrowest).
    #[serde(default = "default_assign_scope")]
    pub assign_scope: String,
    /// Work Key: explicit assignee allowlist used when
    /// `assign_scope = specific`.
    #[serde(default)]
    pub assign_allowed_agents: Vec<String>,
    /// Org Key: may this Operative reassign/override work owned by
    /// others? **Enforced** on Brief management mutations.
    #[serde(default)]
    pub can_manage_work: bool,
    /// Org Key: scope of `can_manage_work` — `any` / `branch` (only the
    /// actor's Branch) / `specific` (`manage_allowed_agents`). Default
    /// `specific` (narrowest).
    #[serde(default = "default_manage_scope")]
    pub manage_scope: String,
    /// Org Key: explicit managed-owner allowlist used when
    /// `manage_scope = specific`.
    #[serde(default)]
    pub manage_allowed_agents: Vec<String>,
    /// Org Key: may this Operative edit other Operatives' config
    /// (profile fields / Keys / scopes)? **Enforced** on `agent.update`
    /// / `agent.delete`.
    #[serde(default)]
    pub can_configure_agents: bool,
    /// Org Key: scope of `can_configure_agents` — `any` / `branch` /
    /// `specific` (`configure_allowed_agents`) / `none`. Default `none`.
    #[serde(default = "default_configure_scope")]
    pub configure_scope: String,
    /// Org Key: explicit target allowlist used when
    /// `configure_scope = specific`.
    #[serde(default)]
    pub configure_allowed_agents: Vec<String>,
    /// Capability Key: stored credential ids this Operative may have
    /// injected. **Enforced** for non-operator credential reads:
    /// empty = deny-by-default, otherwise exact-id match.
    #[serde(default)]
    pub secret_allowlist: Vec<String>,
    /// The Operative's **charter** — markdown instruction bundle
    /// (company-model §4.5). Operator-authored trusted text; surfaced
    /// in the profile read and composed into the agent's prompt.
    #[serde(default)]
    pub instruction_bundle: String,
    /// Adapter preference (`relix-agent-adapters.md` §3.2/§3.3/§7,
    /// `relix-dashboard-design.md` §9 "model lane"): the model this
    /// Operative would prefer its Rig run on (e.g. `claude-sonnet-4`,
    /// `gpt-5-codex`). Free text, nullable; empty clears. **STORED
    /// PREFERENCE ONLY** — the [`crate::rig::RigRunRequest`] contract
    /// carries no per-run model override, so adapter execution does not
    /// consume this yet. Surfaced + editable as a future adapter hint.
    #[serde(default)]
    pub model_preference: Option<String>,
    /// Adapter preference: the reasoning/effort tier
    /// (`minimal`/`low`/`medium`/`high`; Codex's `model_reasoning_effort`
    /// knob, adapters §3.3). Constrained to that set at write time;
    /// nullable, empty clears. **STORED PREFERENCE ONLY** — see
    /// [`Self::model_preference`].
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_max_concurrent_runs() -> i64 {
    20
}

fn default_true() -> bool {
    true
}

fn default_spawn_route() -> String {
    "founder".to_string()
}

fn default_assign_scope() -> String {
    "specific".to_string()
}

fn default_configure_scope() -> String {
    "none".to_string()
}

fn default_manage_scope() -> String {
    "specific".to_string()
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
    /// DEFERRED 2: mirror of [`AgentProfile::authorized_approvers`]
    /// — surfaced on the gate view so the bridge's
    /// `RequireApproval` flow can stamp the list on the new
    /// approval row without a second profile lookup.
    pub authorized_approvers: Vec<String>,
    pub approval_timeout_secs: i64,
    /// SEC PART 1: agent-gate explicit-bypass profile. See
    /// [`AgentProfile::profile`].
    pub profile: Option<String>,
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
            authorized_approvers: p.authorized_approvers.clone(),
            approval_timeout_secs: p.approval_timeout_secs,
            profile: p.profile.clone(),
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
    /// DEFERRED 3: terminal status used by the boot-time
    /// migration in [`AgentStore::migrate_legacy_opaque_tokens`].
    /// A pre-SEC-PART-A approval that carried a random opaque
    /// `approval_token` is flipped here at startup because the
    /// new admission gate cannot verify the legacy token format.
    /// The waiting agent receives a clear "retry" error on its
    /// next `coord.approval.poll` call.
    LegacyTokenExpired,
}

impl ApprovalStatus {
    pub fn as_wire(&self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
            ApprovalStatus::Expired => "expired",
            ApprovalStatus::Consumed => "consumed",
            ApprovalStatus::LegacyTokenExpired => "legacy_token_expired",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            "consumed" => Some(Self::Consumed),
            "legacy_token_expired" => Some(Self::LegacyTokenExpired),
            _ => None,
        }
    }
}

/// SEC PART A — metadata returned by [`AgentStore::decide_approval`]
/// when an approval is approved. Carries the fields the cap
/// handler needs to mint a structured signed
/// [`crate::approval::ApprovalToken`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecidedApprovalMetadata {
    /// Same as `ApprovalRecord::approval_id`. The token binds
    /// to this id and the SQLite blocklist row is keyed off it.
    pub approval_id: String,
    /// Subject id (NodeId hex) the approval was filed for —
    /// the token binds to this so agent A cannot replay agent
    /// B's token.
    pub subject_id: String,
    /// Capability method the approval was filed for — the
    /// token binds to this so a token for `tool.web_read` does
    /// not admit `tool.terminal`.
    pub method: String,
    /// Optional task correlation id from the approval row.
    /// Used as the token's `session_id` binding so the
    /// admission audit can correlate the approval with the
    /// task it was filed against.
    pub task_id: Option<String>,
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
    /// DEFERRED 2: subject-id allow-list of operators
    /// authorised to record a decision on this row. Stamped
    /// from the agent profile at `create_approval` time. Empty
    /// ⇒ `coord.approval.decide` falls back to the OPERATOR
    /// role check.
    pub authorized_approvers: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingApproval {
    pub standing_id: String,
    pub agent_id: String,
    pub match_category: String,
    pub match_path_glob: Option<String>,
    pub scope_kind: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub method_prefix: Option<String>,
    pub workspace_path_glob: Option<String>,
    pub expires_at: i64,
    pub granted_by: String,
    pub max_calls: Option<i64>,
    pub calls_used: i64,
    pub max_cost_micros: Option<i64>,
    pub cost_used_micros: i64,
    pub note: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingApprovalCreate<'a> {
    pub agent_id: &'a str,
    pub match_category: &'a str,
    pub match_path_glob: Option<&'a str>,
    pub scope_kind: Option<&'a str>,
    pub task_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub method_prefix: Option<&'a str>,
    pub workspace_path_glob: Option<&'a str>,
    pub expires_at: i64,
    pub granted_by: &'a str,
    pub max_calls: Option<i64>,
    pub max_cost_micros: Option<i64>,
    pub note: &'a str,
    pub tenant_id: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StandingApprovalMatch<'a> {
    pub agent_id: &'a str,
    pub category: &'a str,
    pub method: &'a str,
    pub task_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub workspace_path: Option<&'a str>,
    pub tenant_id: Option<&'a str>,
    pub estimated_cost_micros: i64,
    pub now: i64,
}

/// NOT-DONE 2: one row in the `startup_tasks` ledger. Tracks
/// once-per-DB migration passes that need to survive process
/// interruption mid-run. `completed_at_ms = None` means the
/// pass is still in flight (or was interrupted); the
/// background runner uses `last_processed_id` as a resume
/// cursor in that case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupTaskRow {
    /// Stable key identifying the migration pass (e.g.
    /// `"legacy_token_orphaned_task_fail"`).
    pub task_name: String,
    /// Unix ms when the pass last started.
    pub started_at_ms: i64,
    /// Unix ms when the pass completed, or `None` if it is
    /// still in flight / was interrupted.
    pub completed_at_ms: Option<i64>,
    /// Count of rows processed so far. On a clean completion
    /// this equals the total population size; on a resume it
    /// represents the high-water mark before the cursor was
    /// advanced past `last_processed_id`.
    pub rows_processed: i64,
    /// Highest-sorted id the pass touched. The background
    /// runner resumes by listing rows STRICTLY after this
    /// value (lexicographic order). `None` when the pass has
    /// not yet touched any row.
    pub last_processed_id: Option<String>,
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

/// The result of [`AgentStore::approve_hire_with_rig`]: what Rig the
/// now-active Operative ends up bound to (if any), and whether *this*
/// call set it. A `rig: None` outcome means the Operative is active but
/// still **un-runnable** until a Rig is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveHireOutcome {
    /// `true` when this approval call wrote the Rig (vs. it was already set
    /// or left unset).
    pub rig_set: bool,
    /// The Rig bound to the Operative after approval, if any.
    pub rig: Option<String>,
}

pub struct AgentStore {
    conn: Arc<Mutex<Connection>>,
}

impl AgentStore {
    pub fn open(path: &Path) -> Result<Self, AgentStoreError> {
        Self::open_with_clock(path, &relix_core::clock::SystemClock)
    }

    /// NOT-DONE 1: constructor that takes an explicit clock so
    /// the boot-time legacy-token migration's `decided_at`
    /// stamp is deterministic under test. Production callers
    /// use [`Self::open`] which threads
    /// [`relix_core::clock::SystemClock`].
    pub fn open_with_clock(
        path: &Path,
        clock: &dyn relix_core::clock::Clock,
    ) -> Result<Self, AgentStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AgentStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "agent_store");
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        // DEFERRED 3: flip pre-SEC-PART-A opaque-token rows
        // before the cap surface is wired so the agent's next
        // poll sees the terminal state immediately.
        let migrated = migrate_legacy_opaque_tokens(&conn, clock.now_ms())?;
        if migrated > 0 {
            tracing::warn!(
                count = migrated,
                "approval: found {migrated} pending approvals with legacy \
                 opaque tokens; flipped to `legacy_token_expired`. Agents \
                 waiting on them will need to retry to mint a fresh \
                 structured token."
            );
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// DEFERRED 3 test-only helper. Inserts a legacy-shaped row
    /// (status=pending|approved + non-NULL opaque
    /// `approval_token`) so test code can exercise the
    /// migration path without bypassing the public API. Hidden
    /// behind `#[cfg(test)]` so it can never accidentally
    /// re-introduce the legacy write path into production.
    #[cfg(test)]
    pub(crate) fn seed_legacy_token_row_for_test(
        &self,
        approval_id: &str,
        status: &str,
        token: &str,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO approval_requests (
                 approval_id, agent_id, subject_id, method, capability_category,
                 args_redacted_hash, reason, approver_groups,
                 requested_at, expires_at, status, task_id, approval_token,
                 authorized_approvers
             ) VALUES (?1, 'a', 's', 'm', 'c', '', '', '[]',
                       0, 9999999999, ?2, NULL, ?3, '[]')",
            params![approval_id, status, token],
        )?;
        Ok(())
    }

    /// NOT-DONE 3 test scaffold. Inserts a pre-SEC-PART-A
    /// approval row (status `pending`, non-NULL `approval_token`,
    /// linked to `task_id`) that the boot-time
    /// `migrate_legacy_opaque_tokens` pass will target. The
    /// post-SEC `create_approval` path no longer writes the
    /// legacy `approval_token` column, so this helper is the
    /// ONLY public way to construct the exact shape the
    /// migration is designed to flip.
    ///
    /// `#[doc(hidden)] pub` so the relix-web-bridge integration
    /// test (which sits in a sibling crate and CANNOT depend on
    /// `rusqlite` — bridge invariant) can seed without raw SQL.
    /// Never call this from production code: it bypasses every
    /// caller-side validation (capability category, requested_at,
    /// expires_at sanity, etc.) and writes a row in a shape that
    /// only exists in legacy databases.
    #[doc(hidden)]
    pub fn force_insert_legacy_pending_approval_for_test(
        &self,
        approval_id: &str,
        task_id: &str,
        opaque_token: &str,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO approval_requests (
                 approval_id, agent_id, subject_id, method, capability_category,
                 args_redacted_hash, reason, approver_groups,
                 requested_at, expires_at, status, task_id, approval_token,
                 authorized_approvers
             ) VALUES (?1, 'agt-legacy', 'subj-op', 'tool.web_read',
                       'external_api:read', '', 'legacy pending', '[]',
                       1700000000, 9999999999, 'pending', ?2, ?3, '[]')",
            params![approval_id, task_id, opaque_token],
        )?;
        Ok(())
    }

    /// DEFERRED 3 test-only helper. Re-runs the legacy-token
    /// migration on the underlying connection. Used by the cap
    /// handler tests to seed a row + migrate + observe the
    /// agent-visible signal without exposing the raw connection.
    #[cfg(test)]
    pub(crate) fn run_legacy_token_migration_for_test(&self) -> Result<usize, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let now_ms = <relix_core::clock::SystemClock as relix_core::clock::Clock>::now_ms(
            &relix_core::clock::SystemClock,
        );
        migrate_legacy_opaque_tokens(&conn, now_ms).map_err(AgentStoreError::Db)
    }

    /// DEFERRED B test-only helper. Stamps a task_id on an
    /// existing approval row without going through the normal
    /// create-approval path. Used by the controller-side
    /// `fail_tasks_orphaned_by_legacy_token_migration` tests to
    /// link a seeded legacy row to a TaskStore-managed task
    /// without re-implementing the entire `create_approval`
    /// signature.
    #[cfg(test)]
    pub(crate) fn force_set_task_id_for_test(
        &self,
        approval_id: &str,
        task_id: &str,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "UPDATE approval_requests SET task_id = ?1 WHERE approval_id = ?2",
            params![task_id, approval_id],
        )?;
        Ok(())
    }

    pub fn in_memory() -> Result<Self, AgentStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        // DEFERRED 3: in-memory stores start fresh, so the
        // migration is a no-op — but we run it anyway so tests
        // that seed legacy rows manually then re-open the
        // store see consistent behaviour with the on-disk path.
        let _ = migrate_legacy_opaque_tokens(
            &conn,
            <relix_core::clock::SystemClock as relix_core::clock::Clock>::now_ms(
                &relix_core::clock::SystemClock,
            ),
        )?;
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
        // GROUP 6: caller's VERIFIED tenant (from InvocationCtx).
        tenant_id: &str,
    ) -> Result<String, AgentStoreError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
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
                 approval_required_categories, authorized_approvers,
                 approval_timeout_secs,
                 created_at, updated_at, tenant_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, '[]',
                       ?9, '[]', '[]', '[]', '[]', ?10, '[]', 86400, ?11, ?11, ?12)",
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
                tenant,
            ],
        )?;
        Ok(agent_id)
    }

    /// Idempotently provision the operator-console agent profile for
    /// a verified `subject_id` (the bridge/dashboard identity). The
    /// fail-closed agent gate denies any caller without a profile, so
    /// the operator console needs one to read Tasks/Workflows. We use
    /// the `allow-all` profile — the system's designated operator
    /// profile (see `agent_gate::PROFILE_ALLOW_ALL`) — which passes the
    /// gate's categorical checks WITHOUT weakening the gate: the call
    /// is still verified, audited, and recorded as an explicit
    /// `allow_all_profile` allow-rule. No-op when a profile for the
    /// subject already exists. Returns true when a row was inserted.
    pub fn ensure_operator_console_profile(
        &self,
        subject_id: &str,
        tenant_id: &str,
    ) -> Result<bool, AgentStoreError> {
        if subject_id.trim().is_empty() {
            return Err(AgentStoreError::BadInput("subject_id required".into()));
        }
        if self.get_by_subject(subject_id)?.is_some() {
            return Ok(false);
        }
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let now = unix_now();
        let agent_id = new_agent_id("operator");
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO agent_profiles (
                 agent_id, name, role, title, department, team,
                 created_by, status, subject_id, surface_allowlist,
                 risk_ceiling, allow_categories, deny_categories,
                 allow_sensitivity_tags, deny_sensitivity_tags,
                 approval_required_categories, authorized_approvers,
                 approval_timeout_secs, created_at, updated_at, tenant_id, profile
             ) VALUES (?1, 'operator-console', 'operator', 'Operator Console',
                       'ops', 'ops', 'relix-boot', 'active', ?2, '[]',
                       'critical', '[]', '[]', '[]', '[]', '[]', '[]', 86400,
                       ?3, ?3, ?4, 'allow-all')",
            params![agent_id, subject_id, now, tenant],
        )?;
        Ok(true)
    }

    /// First-run owner bootstrap: grant the seeded operator-console
    /// profile the full Org/Work Keys (assign/manage/spawn/configure =
    /// `any`) so the dashboard owner — the Founder/Board acting through
    /// the console identity — can assign Briefs to Operatives, manage
    /// their work, and spawn the first team. Idempotent + self-healing:
    /// run every boot, it UPDATEs only the console profile (matched by
    /// `subject_id` AND the `allow-all` infra profile) so it never
    /// touches a normal Operative. No-op (Ok(false)) when no console
    /// profile exists for the subject. This does NOT weaken the
    /// admission gate — those Keys are a separate enforcement axis and
    /// only ever apply to the trusted boot-seeded console identity.
    pub fn grant_console_authority(
        &self,
        subject_id: &str,
        tenant_id: &str,
    ) -> Result<bool, AgentStoreError> {
        if subject_id.trim().is_empty() {
            return Err(AgentStoreError::BadInput("subject_id required".into()));
        }
        let tenant = norm_tenant(tenant_id);
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = conn.execute(
            "UPDATE agent_profiles
             SET can_spawn_agents = 1, spawn_route = 'direct',
                 can_assign_work = 1, assign_scope = 'any',
                 can_manage_work = 1, manage_scope = 'any',
                 can_configure_agents = 1, configure_scope = 'any',
                 updated_at = ?3
             WHERE subject_id = ?1 AND tenant_id = ?2 AND profile = 'allow-all'",
            params![subject_id, tenant, now],
        )?;
        Ok(changed > 0)
    }

    /// First-run company bootstrap (company-model: the Founder is the
    /// apex Operative). Idempotently ensure exactly **one** Founder
    /// Operative exists in `tenant`. Atomic: the existence check and the
    /// insert happen under one connection lock, so two concurrent
    /// bootstrap calls cannot both create a Founder. Returns
    /// `(agent_id, created)` — `created=false` when a Founder already
    /// existed (no duplicate). The Founder is `active`, carries the full
    /// Org/Work Keys (so it can stand up the first team), reports to
    /// nobody (the apex), and runs on `rig` (defaulting to `echo`).
    pub fn ensure_founder(
        &self,
        name: &str,
        rig: &str,
        created_by: &str,
        tenant_id: &str,
    ) -> Result<(String, bool), AgentStoreError> {
        let name = {
            let n = name.trim();
            if n.is_empty() { "Founder" } else { n }
        };
        let rig = {
            let r = rig.trim();
            if r.is_empty() { "echo" } else { r }
        };
        let created_by = {
            let c = created_by.trim();
            if c.is_empty() { "relix-boot" } else { c }
        };
        let tenant = norm_tenant(tenant_id).to_string();
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        // Atomic existence check under the same lock as the insert.
        let existing: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM agent_profiles
                 WHERE role = 'founder' AND tenant_id = ?1
                 ORDER BY created_at ASC LIMIT 1",
                params![tenant],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok((id, false));
        }
        let agent_id = new_agent_id("founder");
        let subject_id = format!("founder-{}", new_agent_id("subject"));
        conn.execute(
            "INSERT INTO agent_profiles (
                 agent_id, name, role, title, department, team,
                 created_by, status, subject_id, risk_ceiling,
                 created_at, updated_at, tenant_id, rig,
                 can_spawn_agents, spawn_route,
                 can_assign_work, assign_scope,
                 can_manage_work, manage_scope,
                 can_configure_agents, configure_scope
             ) VALUES (?1, ?2, 'founder', 'Founder', 'Company', 'Leadership',
                       ?3, 'active', ?4, 'high', ?5, ?5, ?6, ?7,
                       1, 'direct', 1, 'any', 1, 'any', 1, 'any')",
            params![agent_id, name, created_by, subject_id, now, tenant, rig],
        )?;
        Ok((agent_id, true))
    }

    /// Marker `created_by` for owner-provisioned safe-local starter crew
    /// (company-model §12.6). A stable, non-caller value so idempotency and
    /// audit can recognise a starter Operative regardless of which owner
    /// identity ran the bootstrap.
    pub const STARTER_CREATED_BY: &'static str = "relix-starter";

    /// First-run starter crew (company-model §12.6). Idempotently ensure
    /// **one active** safe-local starter Operative for `role` exists in
    /// `tenant`, bound to `rig` (the built-in `echo` by default). Atomic: the
    /// existence check (by the [`Self::STARTER_CREATED_BY`] marker + canonical
    /// `role`) and the insert happen under one connection lock, so two
    /// concurrent bootstraps cannot both create the same starter. Returns
    /// `(agent_id, created)` — `created=false` when a matching starter already
    /// existed (no duplicate). The starter is a plain **worker**: `active`, low
    /// risk ceiling, and NO org/work Keys (it cannot spawn or assign) — the
    /// Board provisions it directly only because this is its sovereign
    /// first-run action (§5.4). `name`/`title` are the operator-facing labels
    /// the caller has already marked local/safe.
    pub fn ensure_starter_operative(
        &self,
        role: &str,
        name: &str,
        title: &str,
        rig: &str,
        tenant_id: &str,
    ) -> Result<(String, bool), AgentStoreError> {
        let role = role.trim();
        let name = name.trim();
        let title = title.trim();
        if role.is_empty() || name.is_empty() || title.is_empty() {
            return Err(AgentStoreError::BadInput(
                "role / name / title required".into(),
            ));
        }
        let rig = {
            let r = rig.trim();
            if r.is_empty() { "echo" } else { r }
        };
        let tenant = norm_tenant(tenant_id).to_string();
        let now = unix_now();
        let approval_required = serde_json::to_string(&default_approval_categories())
            .map_err(|e| AgentStoreError::Json(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        // Atomic existence check under the same lock as the insert: an active
        // starter for this role already covers it.
        let existing: Option<String> = conn
            .query_row(
                "SELECT agent_id FROM agent_profiles
                 WHERE created_by = ?1 AND role = ?2 AND tenant_id = ?3
                   AND status = 'active'
                 ORDER BY created_at ASC LIMIT 1",
                params![Self::STARTER_CREATED_BY, role, tenant],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok((id, false));
        }
        let agent_id = new_agent_id(role);
        let subject_id = format!("starter-{}", new_agent_id("subject"));
        conn.execute(
            "INSERT INTO agent_profiles (
                 agent_id, name, role, title, department, team,
                 created_by, status, subject_id, surface_allowlist,
                 risk_ceiling, allow_categories, deny_categories,
                 allow_sensitivity_tags, deny_sensitivity_tags,
                 approval_required_categories, authorized_approvers,
                 approval_timeout_secs, created_at, updated_at, tenant_id, rig
             ) VALUES (?1, ?2, ?3, ?4, 'Starter Crew', 'Local/Safe',
                       ?5, 'active', ?6, '[]',
                       'low', '[]', '[]', '[]', '[]', ?7, '[]', 86400,
                       ?8, ?8, ?9, ?10)",
            params![
                agent_id,
                name,
                role,
                title,
                Self::STARTER_CREATED_BY,
                subject_id,
                approval_required,
                now,
                tenant,
                rig,
            ],
        )?;
        Ok((agent_id, true))
    }

    /// First-run status read: the tenant's Founder, or `None` if the
    /// company has not been initialised yet.
    pub fn find_founder(&self, tenant_id: &str) -> Result<Option<AgentProfile>, AgentStoreError> {
        let tenant = norm_tenant(tenant_id);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(
                &format!(
                    "{} AND role = 'founder' ORDER BY created_at ASC LIMIT 1",
                    SELECT_AGENTS_BY_TENANT
                ),
                params![tenant],
                row_to_agent,
            )
            .optional()?;
        Ok(row)
    }

    /// The Crew roster: every real Operative in `tenant`, newest first.
    /// **Excludes** the infra operator-console profile (`allow-all`) so
    /// the dashboard Crew shows only assignable Operatives (the Founder
    /// + hires), never the hidden console identity.
    pub fn list_operatives_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<AgentProfile>, AgentStoreError> {
        let tenant = norm_tenant(tenant_id);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(&format!(
            "{} AND COALESCE(profile, '') != 'allow-all' ORDER BY created_at DESC",
            SELECT_AGENTS_BY_TENANT
        ))?;
        let rows: Vec<AgentProfile> = stmt
            .query_map(params![tenant], row_to_agent)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// GROUP 6: tenant-scoped lookup by AIC subject — returns the
    /// profile ONLY when it belongs to `tenant`, so a caller
    /// scoped to tenant A cannot read tenant B's agent profile.
    pub fn get_by_subject_for_tenant(
        &self,
        subject_id: &str,
        tenant: &str,
    ) -> Result<Option<AgentProfile>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT agent_id, name, role, title, department, team,
                        created_by, status, subject_id, surface_allowlist,
                        risk_ceiling, allow_categories, deny_categories,
                        allow_sensitivity_tags, deny_sensitivity_tags,
                        approval_required_categories, authorized_approvers,
                        approval_timeout_secs,
                        created_at, updated_at, profile, reports_to, rig,
                        monthly_allowance_cents, max_concurrent_runs,
                        wake_on_timer, wake_on_demand,
                        can_spawn_agents, spawn_route, can_assign_work, assign_scope,
                        assign_allowed_agents, can_manage_work, can_configure_agents,
                        configure_scope, secret_allowlist, instruction_bundle,
                        manage_scope, manage_allowed_agents, configure_allowed_agents,
                        model_preference, reasoning_effort
                 FROM agent_profiles WHERE subject_id = ?1 AND tenant_id = ?2",
                params![subject_id, tenant],
                row_to_agent,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_agent(&self, agent_id: &str) -> Result<Option<AgentProfile>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_AGENT, params![agent_id], row_to_agent)
            .optional()?;
        Ok(row)
    }

    /// GROUP 6 (tenant isolation): read one Operative profile by
    /// `agent_id` ONLY when it belongs to `tenant`. The product-facing
    /// `agent.get` / `agent.keys` routes use this so a known agent_id
    /// from one Guild cannot read another Guild's Operative.
    pub fn get_agent_for_tenant(
        &self,
        agent_id: &str,
        tenant: &str,
    ) -> Result<Option<AgentProfile>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(SELECT_AGENT_FOR_TENANT, params![agent_id, t], row_to_agent)
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
                        approval_required_categories, authorized_approvers,
                        approval_timeout_secs,
                        created_at, updated_at, profile, reports_to, rig,
                        monthly_allowance_cents, max_concurrent_runs,
                        wake_on_timer, wake_on_demand,
                        can_spawn_agents, spawn_route, can_assign_work, assign_scope,
                        assign_allowed_agents, can_manage_work, can_configure_agents,
                        configure_scope, secret_allowlist, instruction_bundle,
                        manage_scope, manage_allowed_agents, configure_allowed_agents,
                        model_preference, reasoning_effort
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

    /// PHASE 0 (org tree): the agents that directly report to
    /// `manager_id` — one level down the hierarchy. Powers the
    /// org-chart children view. Ordered oldest-first for stable
    /// rendering.
    pub fn list_direct_reports(
        &self,
        manager_id: &str,
    ) -> Result<Vec<AgentSnapshot>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, name, role, status, subject_id
             FROM agent_profiles WHERE reports_to = ?1
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<AgentSnapshot> = stmt
            .query_map(params![manager_id], |r| {
                Ok(AgentSnapshot {
                    agent_id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    status: r.get(3)?,
                    subject_id: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 5 (staffing): the **active** Operatives whose `role`
    /// matches — the companion's "who can I assign this to" lookup.
    /// Returns agent_ids, creation order. Suspended/disabled/pending
    /// Operatives are excluded (only assignable ones).
    pub fn list_by_role(&self, role: &str) -> Result<Vec<String>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_profiles
             WHERE role = ?1 AND status = 'active'
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<String> = stmt
            .query_map(params![role], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 2 (org tree): an Operative's **peers** — the other
    /// Operatives reporting to the same Lead (excludes the agent
    /// itself). Empty for an apex with no Lead set. The "my team"
    /// sibling row in the org chart.
    pub fn list_peers(&self, agent_id: &str) -> Result<Vec<String>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let boss: Option<String> = conn
            .query_row(
                "SELECT reports_to FROM agent_profiles WHERE agent_id=?1",
                params![agent_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(boss) = boss else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_profiles
             WHERE reports_to = ?1 AND agent_id != ?2
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<String> = stmt
            .query_map(params![boss, agent_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    // ── GROUP 6: tenant-scoped org-tree reads ─────────────
    // Each mirrors the unscoped read above but adds `tenant_id = ?`
    // so a known agent_id from another Guild yields nothing.

    /// Tenant-scoped [`Self::list_direct_reports`].
    pub fn list_direct_reports_for_tenant(
        &self,
        manager_id: &str,
        tenant: &str,
    ) -> Result<Vec<AgentSnapshot>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT agent_id, name, role, status, subject_id
             FROM agent_profiles WHERE reports_to = ?1 AND tenant_id = ?2
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<AgentSnapshot> = stmt
            .query_map(params![manager_id, t], |r| {
                Ok(AgentSnapshot {
                    agent_id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    status: r.get(3)?,
                    subject_id: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Tenant-scoped [`Self::list_by_role`].
    pub fn list_by_role_for_tenant(
        &self,
        role: &str,
        tenant: &str,
    ) -> Result<Vec<String>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_profiles
             WHERE role = ?1 AND status = 'active' AND tenant_id = ?2
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<String> = stmt
            .query_map(params![role, t], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// All `active` Operatives in `tenant`, **oldest first** (`created_at`
    /// then insertion order). Used to adopt an already-active same-role crew
    /// member before filing a new hire (company-model §12.5A/§12.5B), so the
    /// Company reuses the crew it already has. Tenant-scoped: it never returns
    /// another Company's crew. The ordering is deterministic so a caller can
    /// pick "the oldest match" reproducibly.
    pub fn list_active_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<AgentProfile>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let sql = format!(
            "{SELECT_AGENTS_BY_TENANT} AND status = 'active' ORDER BY created_at ASC, rowid ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![t], row_to_agent)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Tenant-scoped [`Self::list_peers`].
    pub fn list_peers_for_tenant(
        &self,
        agent_id: &str,
        tenant: &str,
    ) -> Result<Vec<String>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let boss: Option<String> = conn
            .query_row(
                "SELECT reports_to FROM agent_profiles WHERE agent_id=?1 AND tenant_id=?2",
                params![agent_id, t],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(boss) = boss else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_profiles
             WHERE reports_to = ?1 AND agent_id != ?2 AND tenant_id = ?3
             ORDER BY created_at ASC",
        )?;
        let rows: Vec<String> = stmt
            .query_map(params![boss, agent_id, t], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 0 (org tree): every agent at or below `manager_id` —
    /// the manager's *subtree*, as agent_ids (excluding the
    /// manager itself). This is the scope unit for delegated
    /// authority ("this planner may only assign work to agents
    /// under it"). Breadth-first with a visited guard and a hard
    /// node cap, so a malformed cycle can never spin forever.
    pub fn manager_subtree(&self, manager_id: &str) -> Result<Vec<String>, AgentStoreError> {
        const MAX_NODES: usize = 10_000;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare("SELECT agent_id FROM agent_profiles WHERE reports_to = ?1")?;
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(manager_id.to_string());
        let mut frontier: Vec<String> = vec![manager_id.to_string()];
        while let Some(node) = frontier.pop() {
            if out.len() >= MAX_NODES {
                break;
            }
            let children: Vec<String> = stmt
                .query_map(params![node], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            for child in children {
                if seen.insert(child.clone()) {
                    out.push(child.clone());
                    frontier.push(child);
                }
            }
        }
        Ok(out)
    }

    /// PHASE 0 (org tree): the escalation path *up* from `agent_id`
    /// to the apex — the chain of bosses, nearest first. Stops at
    /// the apex (an agent with no boss) or a missing link. A
    /// visited guard + depth cap bound a malformed cycle.
    pub fn chain_of_command(&self, agent_id: &str) -> Result<Vec<String>, AgentStoreError> {
        const MAX_DEPTH: usize = 1024;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare("SELECT reports_to FROM agent_profiles WHERE agent_id = ?1")?;
        let mut chain: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(agent_id.to_string());
        let mut current = agent_id.to_string();
        for _ in 0..MAX_DEPTH {
            let boss: Option<String> = stmt
                .query_row(params![current], |r| r.get::<_, Option<String>>(0))
                .optional()?
                .flatten();
            let Some(boss) = boss else { break };
            if !seen.insert(boss.clone()) {
                break; // cycle guard
            }
            chain.push(boss.clone());
            current = boss;
        }
        Ok(chain)
    }

    /// PHASE 2/3 authority: may `manager` act on `target`? True when
    /// `target` is in `manager`'s Branch (subtree) — the delegated-
    /// authority scope ("a planner may only assign to agents under
    /// it"). A manager does not manage itself.
    pub fn manages(&self, manager_id: &str, target_id: &str) -> Result<bool, AgentStoreError> {
        if manager_id == target_id {
            return Ok(false);
        }
        let subtree = self.manager_subtree(manager_id)?;
        Ok(subtree.iter().any(|id| id == target_id))
    }

    /// Tenant-scoped [`Self::manager_subtree`] — the BFS only follows
    /// `reports_to` edges within `tenant`, so a Branch never leaks an
    /// agent_id from another Guild.
    pub fn manager_subtree_for_tenant(
        &self,
        manager_id: &str,
        tenant: &str,
    ) -> Result<Vec<String>, AgentStoreError> {
        const MAX_NODES: usize = 10_000;
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT agent_id FROM agent_profiles WHERE reports_to = ?1 AND tenant_id = ?2",
        )?;
        let mut out: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(manager_id.to_string());
        let mut frontier: Vec<String> = vec![manager_id.to_string()];
        while let Some(node) = frontier.pop() {
            if out.len() >= MAX_NODES {
                break;
            }
            let children: Vec<String> = stmt
                .query_map(params![node, t], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            for child in children {
                if seen.insert(child.clone()) {
                    out.push(child.clone());
                    frontier.push(child);
                }
            }
        }
        Ok(out)
    }

    /// Tenant-scoped [`Self::chain_of_command`].
    pub fn chain_of_command_for_tenant(
        &self,
        agent_id: &str,
        tenant: &str,
    ) -> Result<Vec<String>, AgentStoreError> {
        const MAX_DEPTH: usize = 1024;
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT reports_to FROM agent_profiles WHERE agent_id = ?1 AND tenant_id = ?2",
        )?;
        let mut chain: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        seen.insert(agent_id.to_string());
        let mut current = agent_id.to_string();
        for _ in 0..MAX_DEPTH {
            let boss: Option<String> = stmt
                .query_row(params![current, t], |r| r.get::<_, Option<String>>(0))
                .optional()?
                .flatten();
            let Some(boss) = boss else { break };
            if !seen.insert(boss.clone()) {
                break;
            }
            chain.push(boss.clone());
            current = boss;
        }
        Ok(chain)
    }

    /// Tenant-scoped [`Self::manages`] — the delegated-authority check
    /// used by the assign-Key gate, scoped so cross-tenant agent_ids
    /// never resolve as "in Branch".
    pub fn manages_for_tenant(
        &self,
        manager_id: &str,
        target_id: &str,
        tenant: &str,
    ) -> Result<bool, AgentStoreError> {
        if manager_id == target_id {
            return Ok(false);
        }
        let subtree = self.manager_subtree_for_tenant(manager_id, tenant)?;
        Ok(subtree.iter().any(|id| id == target_id))
    }

    /// PHASE 5 (companion / Roster): Operative counts by status —
    /// the Roster-at-a-glance (active / pending / suspended /
    /// disabled).
    pub fn status_counts(&self) -> Result<Vec<(String, i64)>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT status, COUNT(*) FROM agent_profiles GROUP BY status ORDER BY status",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// PHASE 4 (Allowance oversight): the total monthly Allowance
    /// committed across the *active* roster, in cents. NULL
    /// allowances count as 0. The Founder compares this against the
    /// Guild Allowance (`guild.get`) to read commitment vs budget.
    pub fn committed_allowance_cents(&self) -> Result<i64, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(monthly_allowance_cents), 0)
             FROM agent_profiles WHERE status = 'active'",
            [],
            |r| r.get(0),
        )?;
        Ok(total)
    }

    /// GROUP 6 (tenant isolation): committed Allowance for ONE Guild's
    /// active roster. The unscoped variant sums across every tenant,
    /// which would leak another Guild's spend commitment — the
    /// product `agent.allowance_committed` route uses this.
    pub fn committed_allowance_cents_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<i64, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(monthly_allowance_cents), 0)
             FROM agent_profiles WHERE status = 'active' AND tenant_id = ?1",
            params![t],
            |r| r.get(0),
        )?;
        Ok(total)
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
            | "approval_required_categories"
            | "authorized_approvers" => {
                // Accept either a JSON array or a comma-separated
                // list; normalise to JSON for storage.
                let json = normalise_string_list(value)
                    .map_err(|e| AgentStoreError::BadInput(format!("{field}: {e}")))?;
                let sql = format!(
                    "UPDATE agent_profiles SET {field}=?1, updated_at=?2 WHERE agent_id=?3"
                );
                conn.execute(&sql, params![json, now, agent_id])?
            }
            "profile" => {
                // SEC PART 1: only `allow-all` and `""`/NULL
                // are valid. Reject any other label so an
                // operator can't introduce a new permissive
                // profile name that the gate doesn't know
                // about (which would fall through to the
                // default-deny path silently).
                let trimmed = value.trim();
                let stored: Option<&str> = if trimmed.is_empty() {
                    None
                } else if trimmed == "allow-all" {
                    Some("allow-all")
                } else {
                    return Err(AgentStoreError::BadInput(format!(
                        "profile '{trimmed}' not recognised (only 'allow-all' or empty are valid)"
                    )));
                };
                conn.execute(
                    "UPDATE agent_profiles SET profile=?1, updated_at=?2 WHERE agent_id=?3",
                    params![stored, now, agent_id],
                )?
            }
            "reports_to" => {
                // PHASE 0 (org tree): set or clear this agent's
                // boss. Empty value clears it (apex / no boss). A
                // non-empty value must reference an existing agent
                // and must not be the agent itself (no self-report).
                // Deeper cycle detection lands with the org-chart
                // surface in Phase 2.
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    conn.execute(
                        "UPDATE agent_profiles SET reports_to=NULL, updated_at=?1
                         WHERE agent_id=?2",
                        params![now, agent_id],
                    )?
                } else {
                    if trimmed == agent_id {
                        return Err(AgentStoreError::BadInput(
                            "an agent cannot report to itself".into(),
                        ));
                    }
                    let boss_exists = conn
                        .query_row(
                            "SELECT 1 FROM agent_profiles WHERE agent_id=?1",
                            params![trimmed],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    if !boss_exists {
                        return Err(AgentStoreError::BadInput(format!(
                            "reports_to target '{trimmed}' is not a known agent"
                        )));
                    }
                    // Cycle guard: walk up the chain from the
                    // prospective boss; if we reach this agent, the
                    // edge would close a loop in the org tree. Done
                    // inline (we already hold the lock) so it can't
                    // deadlock against `manages`/`chain_of_command`.
                    const MAX_CHAIN_DEPTH: u32 = 10_000;
                    let mut cursor = trimmed.to_string();
                    let mut depth = 0u32;
                    loop {
                        if cursor == agent_id {
                            return Err(AgentStoreError::BadInput(
                                "reports_to would create a cycle in the org tree".into(),
                            ));
                        }
                        depth += 1;
                        if depth > MAX_CHAIN_DEPTH {
                            break; // pre-existing corrupt cycle — don't hang
                        }
                        let next: Option<String> = conn
                            .query_row(
                                "SELECT reports_to FROM agent_profiles WHERE agent_id=?1",
                                params![cursor],
                                |r| r.get::<_, Option<String>>(0),
                            )
                            .optional()?
                            .flatten();
                        match next {
                            Some(b) => cursor = b,
                            None => break, // reached an apex — no cycle
                        }
                    }
                    conn.execute(
                        "UPDATE agent_profiles SET reports_to=?1, updated_at=?2
                         WHERE agent_id=?3",
                        params![trimmed, now, agent_id],
                    )?
                }
            }
            "rig" => {
                // PILLAR 2 (Rig): set or clear the backend that
                // powers this Operative. Empty clears it (use the
                // Guild default). Any non-empty name is accepted;
                // the dispatcher resolves it against the Rig
                // registry and falls back to the default if unknown.
                let trimmed = value.trim();
                let stored: Option<&str> = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                conn.execute(
                    "UPDATE agent_profiles SET rig=?1, updated_at=?2 WHERE agent_id=?3",
                    params![stored, now, agent_id],
                )?
            }
            "allowance" => {
                // Governance: this Operative's monthly Allowance in
                // cents. Empty clears the cap; negative / non-integer
                // are rejected.
                let trimmed = value.trim();
                let stored: Option<i64> = if trimmed.is_empty() {
                    None
                } else {
                    match trimmed.parse::<i64>() {
                        Ok(c) if c >= 0 => Some(c),
                        Ok(_) => {
                            return Err(AgentStoreError::BadInput("allowance must be >= 0".into()));
                        }
                        Err(_) => {
                            return Err(AgentStoreError::BadInput(format!(
                                "allowance not an integer: {trimmed}"
                            )));
                        }
                    }
                };
                conn.execute(
                    "UPDATE agent_profiles SET monthly_allowance_cents=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![stored, now, agent_id],
                )?
            }
            "max_concurrent_runs" | "concurrency" => {
                let trimmed = value.trim();
                let slots = match trimmed.parse::<i64>() {
                    Ok(n) if (1..=50).contains(&n) => n,
                    Ok(_) => {
                        return Err(AgentStoreError::BadInput(
                            "max_concurrent_runs must be between 1 and 50".into(),
                        ));
                    }
                    Err(_) => {
                        return Err(AgentStoreError::BadInput(format!(
                            "max_concurrent_runs not an integer: {trimmed}"
                        )));
                    }
                };
                conn.execute(
                    "UPDATE agent_profiles SET max_concurrent_runs=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![slots, now, agent_id],
                )?
            }
            "wake_on_timer" | "timer_wake" => {
                let flag = parse_bool_key(value, "wake_on_timer")?;
                conn.execute(
                    "UPDATE agent_profiles SET wake_on_timer=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![if flag { 1 } else { 0 }, now, agent_id],
                )?
            }
            "wake_on_demand" | "on_demand_wake" => {
                let flag = parse_bool_key(value, "wake_on_demand")?;
                conn.execute(
                    "UPDATE agent_profiles SET wake_on_demand=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![if flag { 1 } else { 0 }, now, agent_id],
                )?
            }
            // ── Org/Work Keys (company-model §5.2) ────────────
            "can_spawn_agents" | "can_manage_work" | "can_configure_agents" => {
                let flag = parse_bool_key(value, field)?;
                let sql = format!(
                    "UPDATE agent_profiles SET {field}=?1, updated_at=?2 WHERE agent_id=?3"
                );
                conn.execute(&sql, params![if flag { 1 } else { 0 }, now, agent_id])?
            }
            "can_assign_work" => {
                let flag = parse_bool_key(value, "can_assign_work")?;
                conn.execute(
                    "UPDATE agent_profiles SET can_assign_work=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![if flag { 1 } else { 0 }, now, agent_id],
                )?
            }
            "spawn_route" => {
                let v = value.trim();
                if !super::keys::SPAWN_ROUTES.contains(&v) {
                    return Err(AgentStoreError::BadInput(format!(
                        "spawn_route '{v}' not in direct/lead/founder"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET spawn_route=?1, updated_at=?2 WHERE agent_id=?3",
                    params![v, now, agent_id],
                )?
            }
            "assign_scope" => {
                let v = value.trim();
                if !super::keys::ASSIGN_SCOPES.contains(&v) {
                    return Err(AgentStoreError::BadInput(format!(
                        "assign_scope '{v}' not in any/branch/specific"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET assign_scope=?1, updated_at=?2 WHERE agent_id=?3",
                    params![v, now, agent_id],
                )?
            }
            "configure_scope" => {
                let v = value.trim();
                if !super::keys::CONFIGURE_SCOPES.contains(&v) {
                    return Err(AgentStoreError::BadInput(format!(
                        "configure_scope '{v}' not in any/branch/specific/none"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET configure_scope=?1, updated_at=?2 WHERE agent_id=?3",
                    params![v, now, agent_id],
                )?
            }
            "manage_scope" => {
                let v = value.trim();
                if !super::keys::MANAGE_SCOPES.contains(&v) {
                    return Err(AgentStoreError::BadInput(format!(
                        "manage_scope '{v}' not in any/branch/specific"
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET manage_scope=?1, updated_at=?2 WHERE agent_id=?3",
                    params![v, now, agent_id],
                )?
            }
            "assign_allowed_agents"
            | "secret_allowlist"
            | "manage_allowed_agents"
            | "configure_allowed_agents" => {
                let json = normalise_string_list(value)
                    .map_err(|e| AgentStoreError::BadInput(format!("{field}: {e}")))?;
                let sql = format!(
                    "UPDATE agent_profiles SET {field}=?1, updated_at=?2 WHERE agent_id=?3"
                );
                conn.execute(&sql, params![json, now, agent_id])?
            }
            "instruction_bundle" | "charter" => {
                // Charter markdown (company-model §4.5). Stored verbatim
                // but length-capped so a runaway bundle can't bloat the
                // row; the gate never executes this — it is context only.
                const MAX_BUNDLE: usize = 64 * 1024;
                if value.len() > MAX_BUNDLE {
                    return Err(AgentStoreError::BadInput(format!(
                        "instruction_bundle too large ({} bytes; max {MAX_BUNDLE})",
                        value.len()
                    )));
                }
                conn.execute(
                    "UPDATE agent_profiles SET instruction_bundle=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![value, now, agent_id],
                )?
            }
            "model_preference" | "model" => {
                // Adapter preference (relix-agent-adapters.md §3.2/§3.3/§7).
                // Free-text model name; empty clears. Length-capped so a
                // runaway value can't bloat the row. STORED PREFERENCE ONLY —
                // adapter execution does not consume it (no per-run model
                // override on the Rig run contract).
                const MAX_MODEL: usize = 128;
                let trimmed = value.trim();
                if trimmed.len() > MAX_MODEL {
                    return Err(AgentStoreError::BadInput(format!(
                        "model_preference too long ({} chars; max {MAX_MODEL})",
                        trimmed.len()
                    )));
                }
                if contains_agent_wire_delimiter(trimmed) {
                    return Err(AgentStoreError::BadInput(
                        "model_preference cannot contain pipe, tab, or newline characters".into(),
                    ));
                }
                let stored: Option<&str> = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                conn.execute(
                    "UPDATE agent_profiles SET model_preference=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![stored, now, agent_id],
                )?
            }
            "reasoning_effort" | "effort" => {
                // Adapter preference: reasoning/effort tier. Empty clears;
                // otherwise constrained to the safe set. STORED PREFERENCE
                // ONLY — see `model_preference`.
                let trimmed = value.trim();
                let stored: Option<&str> = if trimmed.is_empty() {
                    None
                } else if is_known_effort(trimmed) {
                    Some(trimmed)
                } else {
                    return Err(AgentStoreError::BadInput(format!(
                        "reasoning_effort '{trimmed}' not in minimal/low/medium/high"
                    )));
                };
                conn.execute(
                    "UPDATE agent_profiles SET reasoning_effort=?1, updated_at=?2
                     WHERE agent_id=?3",
                    params![stored, now, agent_id],
                )?
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

    /// GROUP 6 (tenant isolation): edit one field ONLY when the
    /// Operative belongs to `tenant`. `agent_id` is a globally-unique
    /// PRIMARY KEY, so an ownership check fully isolates the write:
    /// once we confirm the row is this Guild's, mutating by agent_id
    /// cannot touch another Guild. Returns `NotFound` (never an
    /// "exists but wrong tenant" leak) when the row is not visible.
    pub fn update_agent_field_for_tenant(
        &self,
        agent_id: &str,
        tenant: &str,
        field: &str,
        value: &str,
    ) -> Result<(), AgentStoreError> {
        if self.get_agent_for_tenant(agent_id, tenant)?.is_none() {
            return Err(AgentStoreError::NotFound(agent_id.to_string()));
        }
        self.update_agent_field(agent_id, field, value)
    }

    /// GROUP 6 (tenant isolation): soft-delete ONLY when the Operative
    /// belongs to `tenant`. Same ownership-check rationale as
    /// [`Self::update_agent_field_for_tenant`].
    pub fn soft_delete_for_tenant(
        &self,
        agent_id: &str,
        tenant: &str,
    ) -> Result<(), AgentStoreError> {
        if self.get_agent_for_tenant(agent_id, tenant)?.is_none() {
            return Err(AgentStoreError::NotFound(agent_id.to_string()));
        }
        self.soft_delete_agent(agent_id)
    }

    // ── PHASE 4: hire flow ────────────────────────────────

    /// Request a hire: mint a new Operative in `pending` status. A
    /// pending Operative appears in the Roster but is **inert** —
    /// the fail-closed agent gate denies any non-`active` caller —
    /// so a CEO-spawned hire can't act until the Founder approves.
    #[allow(clippy::too_many_arguments)]
    pub fn request_hire(
        &self,
        name: &str,
        role: &str,
        title: &str,
        department: &str,
        team: &str,
        created_by: &str,
        subject_id: &str,
        risk_ceiling: &str,
        tenant_id: &str,
    ) -> Result<String, AgentStoreError> {
        let agent_id = self.create_agent(
            name,
            role,
            title,
            department,
            team,
            created_by,
            subject_id,
            risk_ceiling,
            tenant_id,
        )?;
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "UPDATE agent_profiles SET status='pending', updated_at=?1 WHERE agent_id=?2",
            params![now, agent_id],
        )?;
        Ok(agent_id)
    }

    /// Approve a pending hire (pending → active). Errors if the
    /// Operative isn't pending (so an already-active agent can't be
    /// "approved" into existence).
    pub fn approve_hire(&self, agent_id: &str) -> Result<(), AgentStoreError> {
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = conn.execute(
            "UPDATE agent_profiles SET status='active', updated_at=?1
             WHERE agent_id=?2 AND status='pending'",
            params![now, agent_id],
        )?;
        if changed == 0 {
            return Err(AgentStoreError::NotFound(agent_id.into()));
        }
        Ok(())
    }

    /// Approve a pending hire **and** (optionally) bind its Rig in one
    /// atomic, tenant-scoped step (company-model §12.6). This is what lets
    /// `agent.approve_hire` produce an *immediately runnable* Operative: a
    /// freshly-filed Prime hire has no Rig, so without setting one here the
    /// approved Operative would activate un-runnable (the dispatcher resolves
    /// an empty Rig to the Guild default, which is unset by default) and need a
    /// separate `agent.update {rig}` PATCH.
    ///
    /// Semantics:
    /// - **Tenant-scoped, no existence leak.** A hire that does not exist *in
    ///   `tenant`* (absent or another Guild's) returns `NotFound` — identical
    ///   to a truly-missing id, so a cross-tenant caller cannot probe.
    /// - **Pending-only.** An already-active (or disabled) Operative returns
    ///   `BadInput` and is left untouched — a duplicate approval is safe and
    ///   **never clobbers** an existing Rig (the second call refuses before any
    ///   write). Mirrors [`Self::approve_hire`].
    /// - **No-clobber.** A `rig` is written only when one is supplied *and* the
    ///   Operative currently has none; an already-set Rig is preserved (the
    ///   explicit Rig-change path is `agent.update {rig}`, gated separately).
    /// - **Atomic.** The status flip and the Rig write happen under the same
    ///   connection lock in a single `UPDATE` guarded by `status='pending'`, so
    ///   there is no window where the Operative is active-but-unrigged because
    ///   of this call.
    ///
    /// The caller is responsible for validating `rig` against the known-Rig
    /// allowlist *before* calling (see `rig::is_known_rig`); this method stores
    /// whatever non-empty name it is given.
    pub fn approve_hire_with_rig(
        &self,
        agent_id: &str,
        rig: Option<&str>,
        tenant: &str,
    ) -> Result<ApproveHireOutcome, AgentStoreError> {
        let now = unix_now();
        let t = norm_tenant(tenant).to_string();
        let rig = rig.map(str::trim).filter(|s| !s.is_empty());
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        // Tenant-scoped read of the current status + Rig. A row outside the
        // caller's Guild reads as absent (no existence leak).
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT status, rig FROM agent_profiles
                 WHERE agent_id = ?1 AND tenant_id = ?2",
                params![agent_id, t],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        let (status, existing_rig) = match row {
            Some(v) => v,
            None => return Err(AgentStoreError::NotFound(agent_id.into())),
        };
        if status != "pending" {
            return Err(AgentStoreError::BadInput(format!(
                "hire is not pending (status={status}); duplicate approval is a no-op"
            )));
        }
        let existing_rig = existing_rig.filter(|s| !s.trim().is_empty());
        // Set the Rig only when supplied AND none is already bound (no clobber).
        let rig_to_set: Option<&str> = match (rig, existing_rig.as_deref()) {
            (Some(r), None) => Some(r),
            _ => None,
        };
        let changed = if let Some(r) = rig_to_set {
            conn.execute(
                "UPDATE agent_profiles SET status='active', rig=?1, updated_at=?2
                 WHERE agent_id=?3 AND tenant_id=?4 AND status='pending'",
                params![r, now, agent_id, t],
            )?
        } else {
            conn.execute(
                "UPDATE agent_profiles SET status='active', updated_at=?1
                 WHERE agent_id=?2 AND tenant_id=?3 AND status='pending'",
                params![now, agent_id, t],
            )?
        };
        if changed == 0 {
            // Lost the pending→active race (concurrent approval) — treat as a
            // safe duplicate, never a partial write.
            return Err(AgentStoreError::BadInput(
                "hire is no longer pending; duplicate approval is a no-op".into(),
            ));
        }
        let final_rig = rig_to_set.map(|s| s.to_string()).or(existing_rig);
        Ok(ApproveHireOutcome {
            rig_set: rig_to_set.is_some(),
            rig: final_rig,
        })
    }

    /// Reject a pending hire (pending → disabled, terminal).
    pub fn reject_hire(&self, agent_id: &str) -> Result<(), AgentStoreError> {
        let now = unix_now();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let changed = conn.execute(
            "UPDATE agent_profiles SET status='disabled', updated_at=?1
             WHERE agent_id=?2 AND status='pending'",
            params![now, agent_id],
        )?;
        if changed == 0 {
            return Err(AgentStoreError::NotFound(agent_id.into()));
        }
        Ok(())
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
        // DEFERRED 2: stamp the operator-allow-list onto the
        // row at create time. Sourced by the bridge from
        // `AgentProfile::authorized_approvers`. Empty ⇒
        // `coord.approval.decide` falls back to the role check.
        authorized_approvers: &[String],
        // GROUP 6: caller's VERIFIED tenant (from InvocationCtx).
        tenant_id: &str,
    ) -> Result<String, AgentStoreError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let now = unix_now();
        let approval_id = new_approval_id();
        let groups_json = serde_json::to_string(approver_groups)
            .map_err(|e| AgentStoreError::Json(e.to_string()))?;
        let approvers_json = serde_json::to_string(authorized_approvers)
            .map_err(|e| AgentStoreError::Json(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO approval_requests (
                 approval_id, agent_id, subject_id, method, capability_category,
                 args_redacted_hash, reason, approver_groups,
                 requested_at, expires_at, status, task_id, authorized_approvers, tenant_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11, ?12, ?13)",
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
                approvers_json,
                tenant,
            ],
        )?;
        Ok(approval_id)
    }

    /// Create a **typed spawn Clearance** linked to a pending hire
    /// (company-model §5.2A, route=lead/founder). The row's
    /// `method = SPAWN_CLEARANCE_METHOD` and `agent_id = hire_agent_id`
    /// are the marker [`crate::nodes::coordinator::agent::handlers`]'
    /// decide hop reads to *activate* the hire on approve (or disable
    /// it on reject). `approver_subjects` widens the decider set to the
    /// actor's Lead (route=lead); operator/admin can always decide.
    pub fn create_spawn_clearance(
        &self,
        hire_agent_id: &str,
        hire_subject_id: &str,
        reason: &str,
        approver_subjects: &[String],
        tenant: &str,
    ) -> Result<String, AgentStoreError> {
        let expires_at = unix_now() + SPAWN_CLEARANCE_TTL_SECS;
        self.create_approval(
            hire_agent_id,
            hire_subject_id,
            SPAWN_CLEARANCE_METHOD,
            SPAWN_CLEARANCE_CATEGORY,
            "spawn",
            reason,
            &[],
            None,
            expires_at,
            approver_subjects,
            tenant,
        )
    }

    /// GROUP 6: tenant-scoped approval lookup — returns the row
    /// ONLY when it belongs to `tenant`, so a caller scoped to
    /// tenant A cannot read tenant B's approval request by id.
    pub fn get_approval_for_tenant(
        &self,
        approval_id: &str,
        tenant: &str,
    ) -> Result<Option<String>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row: Option<String> = conn
            .query_row(
                "SELECT status FROM approval_requests WHERE approval_id = ?1 AND tenant_id = ?2",
                params![approval_id, tenant],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row)
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

    /// GROUP 6 (tenant isolation): the full approval record, ONLY when
    /// it belongs to `tenant`. The product `coord.approval.get` /
    /// `coord.approval.decide` paths use this so a known approval_id
    /// from another Guild cannot be read or decided.
    pub fn get_approval_record_for_tenant(
        &self,
        approval_id: &str,
        tenant: &str,
    ) -> Result<Option<ApprovalRecord>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let sql = format!("{SELECT_APPROVAL} AND tenant_id = ?2");
        let row = conn
            .query_row(&sql, params![approval_id, t], row_to_approval)
            .optional()?;
        Ok(row)
    }

    /// SEC PART A: approve or reject a pending approval.
    /// Returns `Some(DecidedApprovalMetadata)` when the
    /// decision is `Approved` so the caller can mint a
    /// structured [`crate::approval::ApprovalToken`] from the
    /// returned metadata. Returns `Ok(None)` on `Rejected`.
    /// Refuses to act on a terminal status.
    ///
    /// The legacy random `approval_token` column is left
    /// untouched (NULL on new rows) — the structured token's
    /// HMAC signature is the proof, and the consumption
    /// blocklist lives in [`approval_token_blocklist`]. The
    /// admission gate calls
    /// [`Self::try_consume_token_atomic`] on the token's
    /// blocklist key, NOT on the `approval_token` column.
    pub fn decide_approval(
        &self,
        approval_id: &str,
        decision: ApprovalStatus,
        decided_by: &str,
        note: &str,
    ) -> Result<Option<DecidedApprovalMetadata>, AgentStoreError> {
        if !matches!(
            decision,
            ApprovalStatus::Approved | ApprovalStatus::Rejected
        ) {
            return Err(AgentStoreError::BadInput(
                "decide accepts only Approved/Rejected".into(),
            ));
        }
        let now = unix_now();
        let mut conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, Option<String>)> = tx
            .query_row(
                "SELECT status, subject_id, method, task_id
                 FROM approval_requests WHERE approval_id = ?1",
                params![approval_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let (status_str, subject_id, method, task_id) = match row {
            None => return Err(AgentStoreError::NotFound(approval_id.into())),
            Some(t) => t,
        };
        if status_str != "pending" {
            return Err(AgentStoreError::BadInput(format!(
                "approval is {status_str}, not pending"
            )));
        }
        let changed = tx.execute(
            "UPDATE approval_requests SET
                 status = ?1,
                 decided_at = ?2,
                 decided_by = ?3,
                 decision_note = ?4
             WHERE approval_id = ?5 AND status = 'pending'",
            params![decision.as_wire(), now, decided_by, note, approval_id,],
        )?;
        // The `status = 'pending'` guard defends against a
        // concurrent decide() that ran between our SELECT and
        // UPDATE. Both transactions can't both UPDATE; the
        // BEGIN IMMEDIATE acquires the reserved lock so this
        // is the safety belt for a hypothetical future
        // refactor that drops the lock.
        if changed == 0 {
            return Err(AgentStoreError::BadInput(format!(
                "approval {approval_id} was decided concurrently"
            )));
        }
        tx.commit()?;
        Ok(if decision == ApprovalStatus::Approved {
            Some(DecidedApprovalMetadata {
                approval_id: approval_id.into(),
                subject_id,
                method,
                task_id,
            })
        } else {
            None
        })
    }

    /// SEC PART A: atomically claim a one-shot
    /// [`ApprovalToken`](crate::approval::ApprovalToken) blocklist
    /// row. Returns `Ok(true)` when this call was the FIRST to
    /// consume `token_id`; returns `Ok(false)` when the token
    /// was already consumed (PRIMARY KEY collision).
    ///
    /// Wrapped in a single `BEGIN IMMEDIATE` transaction so
    /// two concurrent admission paths cannot both see "not yet
    /// consumed". SQLite's `BEGIN IMMEDIATE` acquires the
    /// reserved lock up front; the contender blocks until the
    /// winner commits, at which point its INSERT sees the
    /// existing row and the UNIQUE constraint fires.
    ///
    /// `approval_id` is stored alongside so operators can
    /// `SELECT * FROM approval_token_blocklist WHERE
    /// approval_id = ?` for audit forensics.
    pub fn try_consume_token_atomic(
        &self,
        token_id: &str,
        approval_id: &str,
        consumed_at_ms: i64,
    ) -> Result<bool, AgentStoreError> {
        let mut conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // INSERT OR IGNORE: the PRIMARY KEY collision on
        // re-use translates to a 0-row change. The transaction
        // commits either way — operator history of repeat
        // attempts is not interesting, only "did this attempt
        // win the race?".
        let changes = tx.execute(
            "INSERT OR IGNORE INTO approval_token_blocklist (token_id, approval_id, consumed_at)
             VALUES (?1, ?2, ?3)",
            params![token_id, approval_id, consumed_at_ms],
        )?;
        tx.commit()?;
        Ok(changes > 0)
    }

    /// Read the audit row for a consumed token. Returns
    /// `Ok(Some(consumed_at_ms))` when the token has been
    /// consumed, `Ok(None)` otherwise. Used by the bridge's
    /// debug surface only — the admission gate does not
    /// consult this on the hot path.
    pub fn token_blocklist_consumed_at(
        &self,
        token_id: &str,
    ) -> Result<Option<i64>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row: Option<i64> = conn
            .query_row(
                "SELECT consumed_at FROM approval_token_blocklist WHERE token_id = ?1",
                params![token_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row)
    }

    /// DEFERRED B: list the `task_id`s of every approval in
    /// `legacy_token_expired` status that carries a non-NULL
    /// task_id. The controller startup calls this after the
    /// boot-time legacy-token migration so any task that was
    /// parked in `awaiting_input` by a since-migrated approval
    /// can be transitioned to `failed`.
    ///
    /// Idempotent: re-running returns the SAME list (the rows
    /// stay in `legacy_token_expired` permanently). The
    /// controller-side wrapper is the layer that filters out
    /// tasks that are already terminal.
    pub fn list_legacy_token_expired_task_ids(&self) -> Result<Vec<String>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT task_id FROM approval_requests
             WHERE status = 'legacy_token_expired'
               AND task_id IS NOT NULL",
        )?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// NOT-DONE 2: cursor-aware variant of
    /// [`Self::list_legacy_token_expired_task_ids`] that
    /// returns `(approval_id, task_id)` pairs ordered ASC by
    /// `approval_id` and starting STRICTLY after `cursor`. An
    /// empty cursor returns every row. Used by the resumable
    /// background-task migration pass.
    ///
    /// Ascending-by-id ordering makes the cursor stable across
    /// process restarts: SQLite's PRIMARY KEY scan walks
    /// `approval_id` lexicographically, so a partial pass
    /// always resumes from the next id past the last one it
    /// recorded.
    pub fn list_legacy_token_expired_after(
        &self,
        cursor: &str,
    ) -> Result<Vec<(String, String)>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT approval_id, task_id FROM approval_requests
             WHERE status = 'legacy_token_expired'
               AND task_id IS NOT NULL
               AND approval_id > ?1
             ORDER BY approval_id ASC",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map(params![cursor], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    // ── NOT-DONE 2: startup_tasks ledger ─────────────────

    /// Read the persisted state for one startup task. Returns
    /// `None` when the task name has never been recorded.
    pub fn startup_task_get(
        &self,
        task_name: &str,
    ) -> Result<Option<StartupTaskRow>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT task_name, started_at_ms, completed_at_ms,
                        rows_processed, last_processed_id
                 FROM startup_tasks WHERE task_name = ?1",
                params![task_name],
                |r| {
                    Ok(StartupTaskRow {
                        task_name: r.get(0)?,
                        started_at_ms: r.get(1)?,
                        completed_at_ms: r.get(2)?,
                        rows_processed: r.get(3)?,
                        last_processed_id: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Convenience: `true` iff a row with `completed_at_ms IS
    /// NOT NULL` exists for `task_name`. The background pass
    /// short-circuits on this so a successful prior run is not
    /// re-executed.
    pub fn startup_task_is_complete(&self, task_name: &str) -> Result<bool, AgentStoreError> {
        Ok(self
            .startup_task_get(task_name)?
            .and_then(|r| r.completed_at_ms)
            .is_some())
    }

    /// Insert OR update the startup-task row to mark the pass
    /// as "running". Idempotent: a re-run after interruption
    /// preserves the cursor + rows_processed columns by using
    /// `INSERT … ON CONFLICT DO UPDATE` that only refreshes
    /// `started_at_ms`.
    pub fn startup_task_begin(
        &self,
        task_name: &str,
        started_at_ms: i64,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO startup_tasks (task_name, started_at_ms)
             VALUES (?1, ?2)
             ON CONFLICT(task_name) DO UPDATE
                SET started_at_ms = excluded.started_at_ms",
            params![task_name, started_at_ms],
        )?;
        Ok(())
    }

    /// Persist progress mid-pass: update the cursor +
    /// `rows_processed` so an interrupted process resumes from
    /// the right place on next boot.
    pub fn startup_task_record_progress(
        &self,
        task_name: &str,
        last_processed_id: &str,
        rows_processed: i64,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "UPDATE startup_tasks
             SET last_processed_id = ?1, rows_processed = ?2
             WHERE task_name = ?3",
            params![last_processed_id, rows_processed, task_name],
        )?;
        Ok(())
    }

    /// Mark the pass complete. Subsequent
    /// [`Self::startup_task_is_complete`] calls return `true`.
    pub fn startup_task_complete(
        &self,
        task_name: &str,
        completed_at_ms: i64,
        rows_processed: i64,
    ) -> Result<(), AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "UPDATE startup_tasks
             SET completed_at_ms = ?1, rows_processed = ?2
             WHERE task_name = ?3",
            params![completed_at_ms, rows_processed, task_name],
        )?;
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
                    task_id, approval_token, authorized_approvers
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

    /// GROUP 6 (tenant isolation): pending approvals for ONE Guild —
    /// the product `coord.approval.pending` / `/v1/spine/clearances`
    /// read uses this so the Desk only surfaces this Guild's
    /// Clearances.
    pub fn list_pending_approvals_for_tenant(
        &self,
        limit: usize,
        tenant: &str,
    ) -> Result<Vec<ApprovalRecord>, AgentStoreError> {
        let t = norm_tenant(tenant);
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let cap = limit.clamp(1, 500);
        let mut stmt = conn.prepare(
            "SELECT approval_id, agent_id, subject_id, method, capability_category,
                    args_redacted_hash, reason, approver_groups,
                    requested_at, expires_at, status,
                    decided_at, decided_by, decision_note,
                    task_id, approval_token, authorized_approvers
             FROM approval_requests
             WHERE status = 'pending' AND tenant_id = ?1
             ORDER BY requested_at ASC
             LIMIT ?2",
        )?;
        let rows: Vec<ApprovalRecord> = stmt
            .query_map(params![t, cap as i64], row_to_approval)?
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

    #[allow(clippy::too_many_arguments)]
    pub fn create_standing(
        &self,
        agent_id: &str,
        match_category: &str,
        match_path_glob: Option<&str>,
        expires_at: i64,
        granted_by: &str,
        note: &str,
        // GROUP 6: caller's VERIFIED tenant (from InvocationCtx).
        tenant_id: &str,
    ) -> Result<String, AgentStoreError> {
        self.create_scoped_standing(StandingApprovalCreate {
            agent_id,
            match_category,
            match_path_glob,
            scope_kind: None,
            task_id: None,
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            expires_at,
            granted_by,
            max_calls: None,
            max_cost_micros: None,
            note,
            tenant_id,
        })
    }

    pub fn create_scoped_standing(
        &self,
        input: StandingApprovalCreate<'_>,
    ) -> Result<String, AgentStoreError> {
        if input.agent_id.trim().is_empty() || input.match_category.trim().is_empty() {
            return Err(AgentStoreError::BadInput(
                "agent_id and match_category required".into(),
            ));
        }
        let scope_kind = normalize_standing_scope(input.scope_kind);
        validate_standing_scope(
            &scope_kind,
            input.task_id,
            input.session_id,
            input.method_prefix,
            input.match_path_glob.or(input.workspace_path_glob),
        )?;
        if input.max_calls.is_some_and(|n| n <= 0) {
            return Err(AgentStoreError::BadInput(
                "standing approval max_calls must be positive".into(),
            ));
        }
        if input.max_cost_micros.is_some_and(|n| n <= 0) {
            return Err(AgentStoreError::BadInput(
                "standing approval max_cost_micros must be positive".into(),
            ));
        }
        let tenant = if input.tenant_id.trim().is_empty() {
            "default"
        } else {
            input.tenant_id
        };
        let now = unix_now();
        let standing_id = new_standing_id();
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        conn.execute(
            "INSERT INTO standing_approvals (
                 standing_id, agent_id, match_category, match_path_glob,
                 scope_kind, task_id, session_id, method_prefix, workspace_path_glob,
                 expires_at, granted_by, max_calls, calls_used, max_cost_micros,
                 cost_used_micros, note, created_at, tenant_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13, 0, ?14, ?15, ?16)",
            params![
                standing_id,
                input.agent_id,
                input.match_category,
                input.match_path_glob,
                scope_kind,
                input.task_id.and_then(non_empty_opt),
                input.session_id.and_then(non_empty_opt),
                input.method_prefix.and_then(non_empty_opt),
                input.workspace_path_glob.and_then(non_empty_opt),
                input.expires_at,
                input.granted_by,
                input.max_calls,
                input.max_cost_micros,
                input.note,
                now,
                tenant,
            ],
        )?;
        Ok(standing_id)
    }

    /// GROUP 6: tenant-scoped count of standing approvals for an
    /// agent — proves cross-tenant denial in SQL.
    pub fn count_standing_for_tenant(
        &self,
        tenant: &str,
        agent_id: &str,
    ) -> Result<u64, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM standing_approvals WHERE tenant_id = ?1 AND agent_id = ?2",
            params![tenant, agent_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn list_standing(&self, agent_id: &str) -> Result<Vec<StandingApproval>, AgentStoreError> {
        self.list_standing_for_tenant(agent_id, "default")
    }

    pub fn list_standing_for_tenant(
        &self,
        agent_id: &str,
        tenant_id: &str,
    ) -> Result<Vec<StandingApproval>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let tenant = non_empty_opt(tenant_id).unwrap_or("default");
        let mut stmt = conn.prepare(
            "SELECT standing_id, agent_id, match_category, match_path_glob,
                    scope_kind, task_id, session_id, method_prefix, workspace_path_glob,
                    expires_at, granted_by, max_calls, calls_used, max_cost_micros,
                    cost_used_micros, note, created_at
             FROM standing_approvals WHERE tenant_id = ?1 AND agent_id = ?2
             ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map(params![tenant, agent_id], |r| {
                Ok(StandingApproval {
                    standing_id: r.get(0)?,
                    agent_id: r.get(1)?,
                    match_category: r.get(2)?,
                    match_path_glob: r.get(3)?,
                    scope_kind: r.get(4)?,
                    task_id: r.get(5)?,
                    session_id: r.get(6)?,
                    method_prefix: r.get(7)?,
                    workspace_path_glob: r.get(8)?,
                    expires_at: r.get(9)?,
                    granted_by: r.get(10)?,
                    max_calls: r.get(11)?,
                    calls_used: r.get(12)?,
                    max_cost_micros: r.get(13)?,
                    cost_used_micros: r.get(14)?,
                    note: r.get(15)?,
                    created_at: r.get(16)?,
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
        self.has_active_standing_for(StandingApprovalMatch {
            agent_id,
            category,
            method: "",
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: None,
            estimated_cost_micros: 0,
            now,
        })
    }

    pub fn has_active_standing_for(
        &self,
        input: StandingApprovalMatch<'_>,
    ) -> Result<bool, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let tenant = input.tenant_id.and_then(non_empty_opt).unwrap_or("default");
        let mut stmt = conn.prepare(
            "SELECT standing_id, match_path_glob, scope_kind, task_id, session_id,
                    method_prefix, workspace_path_glob
             FROM standing_approvals
             WHERE tenant_id = ?1 AND agent_id = ?2 AND match_category = ?3 AND expires_at > ?4
               AND (max_calls IS NULL OR calls_used < max_calls)
               AND (max_cost_micros IS NULL OR cost_used_micros + ?5 <= max_cost_micros)",
        )?;
        let rows = stmt
            .query_map(
                params![
                    tenant,
                    input.agent_id,
                    input.category,
                    input.now,
                    input.estimated_cost_micros.max(0)
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().any(
            |(
                _,
                path_glob,
                scope_kind,
                task_id,
                session_id,
                method_prefix,
                workspace_path_glob,
            )| {
                standing_scope_matches(
                    &scope_kind,
                    StandingScopeRow {
                        path_glob: path_glob.as_deref(),
                        task_id: task_id.as_deref(),
                        session_id: session_id.as_deref(),
                        method_prefix: method_prefix.as_deref(),
                        workspace_path_glob: workspace_path_glob.as_deref(),
                    },
                    &input,
                )
            },
        ))
    }

    pub fn consume_active_standing_for(
        &self,
        input: StandingApprovalMatch<'_>,
    ) -> Result<Option<String>, AgentStoreError> {
        let conn = self.conn.lock().map_err(|_| AgentStoreError::Lock)?;
        let tenant = input.tenant_id.and_then(non_empty_opt).unwrap_or("default");
        let candidates = {
            let mut stmt = conn.prepare(
                "SELECT standing_id, match_path_glob, scope_kind, task_id, session_id,
                        method_prefix, workspace_path_glob, max_calls, max_cost_micros
                 FROM standing_approvals
                 WHERE tenant_id = ?1 AND agent_id = ?2 AND match_category = ?3 AND expires_at > ?4
                   AND (max_calls IS NULL OR calls_used < max_calls)
                   AND (max_cost_micros IS NULL OR cost_used_micros + ?5 <= max_cost_micros)
                 ORDER BY created_at ASC, standing_id ASC",
            )?;
            stmt.query_map(
                params![
                    tenant,
                    input.agent_id,
                    input.category,
                    input.now,
                    input.estimated_cost_micros.max(0)
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (
            standing_id,
            path_glob,
            scope_kind,
            task_id,
            session_id,
            method_prefix,
            workspace_path_glob,
            max_calls,
            max_cost_micros,
        ) in candidates
        {
            if !standing_scope_matches(
                &scope_kind,
                StandingScopeRow {
                    path_glob: path_glob.as_deref(),
                    task_id: task_id.as_deref(),
                    session_id: session_id.as_deref(),
                    method_prefix: method_prefix.as_deref(),
                    workspace_path_glob: workspace_path_glob.as_deref(),
                },
                &input,
            ) {
                continue;
            }
            if max_calls.is_none() && max_cost_micros.is_none() {
                return Ok(Some(standing_id));
            }
            let changed = conn.execute(
                "UPDATE standing_approvals
                 SET calls_used = calls_used + CASE WHEN max_calls IS NULL THEN 0 ELSE 1 END,
                     cost_used_micros = cost_used_micros + CASE WHEN max_cost_micros IS NULL THEN 0 ELSE ?2 END
                 WHERE standing_id = ?1
                   AND (max_calls IS NULL OR calls_used < max_calls)
                   AND (max_cost_micros IS NULL OR cost_used_micros + ?2 <= max_cost_micros)",
                params![standing_id, input.estimated_cost_micros.max(0)],
            )?;
            if changed > 0 {
                return Ok(Some(standing_id));
            }
        }
        Ok(None)
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
        approval_required_categories, authorized_approvers,
        approval_timeout_secs,
        created_at, updated_at, profile, reports_to, rig, monthly_allowance_cents,
        max_concurrent_runs, wake_on_timer, wake_on_demand,
        can_spawn_agents, spawn_route, can_assign_work, assign_scope,
        assign_allowed_agents, can_manage_work, can_configure_agents,
        configure_scope, secret_allowlist, instruction_bundle,
        manage_scope, manage_allowed_agents, configure_allowed_agents,
        model_preference, reasoning_effort
 FROM agent_profiles WHERE agent_id = ?1";

/// GROUP 6 (tenant isolation): the agent-id read, additionally scoped
/// to a verified tenant so a known `agent_id` cannot reach another
/// Guild's Operative. Same columns/order as [`SELECT_AGENT`].
const SELECT_AGENT_FOR_TENANT: &str = "SELECT agent_id, name, role, title, department, team,
        created_by, status, subject_id, surface_allowlist,
        risk_ceiling, allow_categories, deny_categories,
        allow_sensitivity_tags, deny_sensitivity_tags,
        approval_required_categories, authorized_approvers,
        approval_timeout_secs,
        created_at, updated_at, profile, reports_to, rig, monthly_allowance_cents,
        max_concurrent_runs, wake_on_timer, wake_on_demand,
        can_spawn_agents, spawn_route, can_assign_work, assign_scope,
        assign_allowed_agents, can_manage_work, can_configure_agents,
        configure_scope, secret_allowlist, instruction_bundle,
        manage_scope, manage_allowed_agents, configure_allowed_agents,
        model_preference, reasoning_effort
 FROM agent_profiles WHERE agent_id = ?1 AND tenant_id = ?2";

/// Full-profile column list scoped to a tenant, with an **open**
/// trailing predicate the caller appends to (`AND role = …` /
/// `AND COALESCE(profile,'') != …` / `ORDER BY …`). Same columns/order
/// as [`SELECT_AGENT`] so [`row_to_agent`] maps it. The single bound
/// parameter is the tenant id.
const SELECT_AGENTS_BY_TENANT: &str = "SELECT agent_id, name, role, title, department, team,
        created_by, status, subject_id, surface_allowlist,
        risk_ceiling, allow_categories, deny_categories,
        allow_sensitivity_tags, deny_sensitivity_tags,
        approval_required_categories, authorized_approvers,
        approval_timeout_secs,
        created_at, updated_at, profile, reports_to, rig, monthly_allowance_cents,
        max_concurrent_runs, wake_on_timer, wake_on_demand,
        can_spawn_agents, spawn_route, can_assign_work, assign_scope,
        assign_allowed_agents, can_manage_work, can_configure_agents,
        configure_scope, secret_allowlist, instruction_bundle,
        manage_scope, manage_allowed_agents, configure_allowed_agents,
        model_preference, reasoning_effort
 FROM agent_profiles WHERE tenant_id = ?1";

const SELECT_APPROVAL: &str =
    "SELECT approval_id, agent_id, subject_id, method, capability_category,
        args_redacted_hash, reason, approver_groups,
        requested_at, expires_at, status,
        decided_at, decided_by, decision_note,
        task_id, approval_token, authorized_approvers
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
             authorized_approvers TEXT NOT NULL DEFAULT '[]',
             approval_timeout_secs INTEGER NOT NULL DEFAULT 86400,
             created_at      INTEGER NOT NULL,
             updated_at      INTEGER NOT NULL,
             reports_to      TEXT,
             rig             TEXT,
             monthly_allowance_cents INTEGER,
             max_concurrent_runs INTEGER NOT NULL DEFAULT 20,
             wake_on_timer INTEGER NOT NULL DEFAULT 1,
             wake_on_demand INTEGER NOT NULL DEFAULT 1,
             can_spawn_agents INTEGER NOT NULL DEFAULT 0,
             spawn_route TEXT NOT NULL DEFAULT 'founder',
             can_assign_work INTEGER NOT NULL DEFAULT 0,
             assign_scope TEXT NOT NULL DEFAULT 'specific',
             assign_allowed_agents TEXT NOT NULL DEFAULT '[]',
             can_manage_work INTEGER NOT NULL DEFAULT 0,
             can_configure_agents INTEGER NOT NULL DEFAULT 0,
             configure_scope TEXT NOT NULL DEFAULT 'none',
             secret_allowlist TEXT NOT NULL DEFAULT '[]',
             instruction_bundle TEXT NOT NULL DEFAULT '',
             manage_scope TEXT NOT NULL DEFAULT 'specific',
             manage_allowed_agents TEXT NOT NULL DEFAULT '[]',
             configure_allowed_agents TEXT NOT NULL DEFAULT '[]',
             model_preference TEXT,
             reasoning_effort TEXT
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
             approval_token  TEXT UNIQUE,
             authorized_approvers TEXT NOT NULL DEFAULT '[]'
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
             scope_kind      TEXT NOT NULL DEFAULT 'agent_category',
             task_id         TEXT,
             session_id      TEXT,
             method_prefix   TEXT,
             workspace_path_glob TEXT,
             expires_at      INTEGER NOT NULL,
             granted_by      TEXT NOT NULL,
             max_calls       INTEGER,
             calls_used      INTEGER NOT NULL DEFAULT 0,
             max_cost_micros INTEGER,
             cost_used_micros INTEGER NOT NULL DEFAULT 0,
             note            TEXT NOT NULL DEFAULT '',
             created_at      INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS standing_approvals_agent
             ON standing_approvals(agent_id, match_category, expires_at);

         -- SEC PART A: one-shot approval-token consumption
         -- blocklist. The admission gate atomically INSERTs a
         -- row here when it admits a token-bearing call; the
         -- PRIMARY KEY constraint makes a concurrent re-use a
         -- guaranteed UNIQUE-violation, so two requests with
         -- the same token cannot both pass.
         CREATE TABLE IF NOT EXISTS approval_token_blocklist (
             token_id     TEXT PRIMARY KEY,
             approval_id  TEXT NOT NULL,
             consumed_at  INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS approval_token_blocklist_approval
             ON approval_token_blocklist(approval_id);

         -- NOT-DONE 2: startup-task ledger. Tracks once-per-DB
         -- migration passes that need to survive process
         -- interruption mid-run. Background tasks consult
         -- `completed_at_ms` to skip; the `last_processed_id`
         -- column is the cursor a partially-completed pass
         -- resumes from on next boot.
         CREATE TABLE IF NOT EXISTS startup_tasks (
             task_name TEXT PRIMARY KEY,
             started_at_ms INTEGER NOT NULL,
             completed_at_ms INTEGER,
             rows_processed INTEGER NOT NULL DEFAULT 0,
             last_processed_id TEXT
         );",
    )?;
    // DEFERRED 2: agent_profiles + approval_requests both gain
    // `authorized_approvers TEXT NOT NULL DEFAULT '[]'`. The
    // CREATE TABLE clauses above are no-ops on existing
    // databases — we have to ALTER the table in that case. The
    // helper is idempotent: it inspects `PRAGMA table_info` and
    // skips when the column already exists. The column is the
    // only DEFERRED 2-introduced piece operators can have on
    // disk before the upgrade.
    ensure_column(
        conn,
        "agent_profiles",
        "authorized_approvers",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "approval_requests",
        "authorized_approvers",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    // SEC PART 1: agent-gate default-deny — `profile` column
    // for explicit "allow-all" bypass. Nullable so existing
    // rows continue to flow through the categorical checks
    // (which now default-deny when no profile exists).
    ensure_column(conn, "agent_profiles", "profile", "TEXT")?;
    // PHASE 0 (org tree): `reports_to` — the single boss link that
    // turns the flat agent list into the org hierarchy. Nullable;
    // existing rows read NULL (no boss) until an operator/CEO sets
    // it via `agent.update agent_id|reports_to|<boss_agent_id>`.
    ensure_column(conn, "agent_profiles", "reports_to", "TEXT")?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS agent_profiles_reports_to ON agent_profiles(reports_to)",
        [],
    )?;
    // PILLAR 2 (Rig): the agent backend that powers an Operative.
    ensure_column(conn, "agent_profiles", "rig", "TEXT")?;
    // Governance: per-Operative monthly Allowance (budget, cents).
    ensure_column(conn, "agent_profiles", "monthly_allowance_cents", "INTEGER")?;
    // Runtime Keys: per-agent wake controls and concurrency slots.
    ensure_column(
        conn,
        "agent_profiles",
        "max_concurrent_runs",
        "INTEGER NOT NULL DEFAULT 20",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "wake_on_timer",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "wake_on_demand",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    // Org/Work Keys (company-model §5.2). Default-deny booleans; the
    // scope/route text columns default to their safest value so an
    // existing row never silently widens. Idempotent via ensure_column.
    ensure_column(
        conn,
        "agent_profiles",
        "can_spawn_agents",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "spawn_route",
        "TEXT NOT NULL DEFAULT 'founder'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "can_assign_work",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "assign_scope",
        "TEXT NOT NULL DEFAULT 'specific'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "assign_allowed_agents",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "can_manage_work",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "can_configure_agents",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "configure_scope",
        "TEXT NOT NULL DEFAULT 'none'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "secret_allowlist",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "instruction_bundle",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    // Manage/Configure scope Keys (company-model §5.2A/§5.3).
    ensure_column(
        conn,
        "agent_profiles",
        "manage_scope",
        "TEXT NOT NULL DEFAULT 'specific'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "manage_allowed_agents",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    ensure_column(
        conn,
        "agent_profiles",
        "configure_allowed_agents",
        "TEXT NOT NULL DEFAULT '[]'",
    )?;
    // Adapter preferences (relix-agent-adapters.md §3.2/§3.3/§7;
    // relix-dashboard-design.md §9). Nullable, no default so existing
    // rows read NULL (no preference). STORED PREFERENCE ONLY — adapter
    // execution does not consume these yet (the Rig run contract carries
    // no per-run model override).
    ensure_column(conn, "agent_profiles", "model_preference", "TEXT")?;
    ensure_column(conn, "agent_profiles", "reasoning_effort", "TEXT")?;
    // GROUP 6: tenant isolation. Add `tenant_id` to the per-caller
    // agent/approval tables. Idempotent (ensure_column probes
    // PRAGMA); existing rows default to the reserved 'default'
    // tenant so single-tenant deployments keep reading their data.
    // (`startup_tasks` = node-local migration ledger and
    // `approval_token_blocklist` = global one-shot-token uniqueness
    // guard are tenant-neutral and intentionally excluded.)
    for tbl in ["agent_profiles", "approval_requests", "standing_approvals"] {
        ensure_column(conn, tbl, "tenant_id", "TEXT NOT NULL DEFAULT 'default'")?;
    }
    ensure_column(
        conn,
        "standing_approvals",
        "scope_kind",
        "TEXT NOT NULL DEFAULT 'agent_category'",
    )?;
    ensure_column(conn, "standing_approvals", "task_id", "TEXT")?;
    ensure_column(conn, "standing_approvals", "session_id", "TEXT")?;
    ensure_column(conn, "standing_approvals", "method_prefix", "TEXT")?;
    ensure_column(conn, "standing_approvals", "workspace_path_glob", "TEXT")?;
    ensure_column(conn, "standing_approvals", "max_calls", "INTEGER")?;
    ensure_column(
        conn,
        "standing_approvals",
        "calls_used",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(conn, "standing_approvals", "max_cost_micros", "INTEGER")?;
    ensure_column(
        conn,
        "standing_approvals",
        "cost_used_micros",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS agent_profiles_tenant ON agent_profiles(tenant_id);\
         CREATE INDEX IF NOT EXISTS approval_requests_tenant ON approval_requests(tenant_id);\
         CREATE INDEX IF NOT EXISTS standing_approvals_tenant ON standing_approvals(tenant_id);\
         CREATE INDEX IF NOT EXISTS standing_approvals_scope ON standing_approvals(tenant_id, agent_id, match_category, scope_kind, expires_at);",
    )?;
    Ok(())
}

/// DEFERRED 3: flip every pre-SEC-PART-A approval row whose
/// `approval_token` column is non-NULL AND whose status is
/// still in a state that an agent could be polling for
/// (`pending` or `approved`) to the new
/// `legacy_token_expired` terminal status.
///
/// The pre-SEC-PART-A `decide_approval` minted a random opaque
/// string into `approval_token`. The new admission gate
/// requires the structured + HMAC-signed
/// [`crate::approval::ApprovalToken`] wire format; an opaque
/// random string fails parse with `MalformedEncoding`. Without
/// this migration the agent's next admission attempt would
/// fail with a confusing decode error; with the migration the
/// operator-side cap layer can return a clear
/// `legacy_token_expired` signal AND a `decision_note` that
/// tells the operator what happened.
///
/// Returns the number of rows flipped. Idempotent — re-runs
/// match no rows because the only states the SQL targets are
/// `pending` / `approved`, and the migration moves rows out of
/// both.
///
/// NOT-DONE 1: `now_ms` is sourced by the caller from a
/// [`relix_core::clock::Clock`] so the `decided_at` stamp is
/// deterministic under test.
pub(crate) fn migrate_legacy_opaque_tokens(
    conn: &Connection,
    now_ms: i64,
) -> Result<usize, rusqlite::Error> {
    // `decided_at` on `approval_requests` is unix seconds (not
    // millis) — preserve the column's existing precision.
    let now_secs = now_ms / 1_000;
    let changed = conn.execute(
        "UPDATE approval_requests
         SET status = 'legacy_token_expired',
             decided_at = ?1,
             decision_note = 'legacy_token_expired: opaque approval_token \
                              from a pre-SEC-PART-A deployment cannot be \
                              verified by the new HMAC-signed token gate. \
                              Retry to mint a fresh structured token.'
         WHERE approval_token IS NOT NULL
           AND status IN ('pending', 'approved')",
        params![now_secs],
    )?;
    Ok(changed)
}

/// Add `column` (with `column_decl`) to `table` if it does not
/// already exist. Idempotent. Lives in this module because
/// agent_store is the only consumer; the §7.30
/// `ApprovalRequestStore` has its own private copy.
fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    column_decl: &str,
) -> Result<(), rusqlite::Error> {
    let sql_check = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql_check)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }
    drop(rows);
    drop(stmt);
    let sql_alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_decl}");
    conn.execute(&sql_alter, [])?;
    Ok(())
}

fn non_empty_opt(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// GROUP 6 (tenant isolation): normalise a tenant id, mapping
/// empty/blank to the reserved `default` tenant — the same convention
/// the write paths use, so reads and writes agree on scoping.
fn norm_tenant(tenant: &str) -> &str {
    let t = tenant.trim();
    if t.is_empty() { "default" } else { t }
}

fn parse_bool_key(value: &str, field: &str) -> Result<bool, AgentStoreError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(AgentStoreError::BadInput(format!(
            "{field} must be boolean (true/false), got `{other}`"
        ))),
    }
}

fn normalize_standing_scope(scope_kind: Option<&str>) -> String {
    match scope_kind.and_then(non_empty_opt) {
        Some("task") => "task".into(),
        Some("session") => "session".into(),
        Some("method_prefix") | Some("capability_family") => "method_prefix".into(),
        Some("workspace_path") => "workspace_path".into(),
        Some("agent_category") | None => "agent_category".into(),
        Some(other) => other.to_string(),
    }
}

fn validate_standing_scope(
    scope_kind: &str,
    task_id: Option<&str>,
    session_id: Option<&str>,
    method_prefix: Option<&str>,
    path_glob: Option<&str>,
) -> Result<(), AgentStoreError> {
    match scope_kind {
        "agent_category" => Ok(()),
        "task" if task_id.and_then(non_empty_opt).is_some() => Ok(()),
        "session" if session_id.and_then(non_empty_opt).is_some() => Ok(()),
        "method_prefix" if method_prefix.and_then(non_empty_opt).is_some() => Ok(()),
        "workspace_path" if path_glob.and_then(non_empty_opt).is_some() => Ok(()),
        "task" => Err(AgentStoreError::BadInput(
            "task scoped standing approval requires task_id".into(),
        )),
        "session" => Err(AgentStoreError::BadInput(
            "session scoped standing approval requires session_id".into(),
        )),
        "method_prefix" => Err(AgentStoreError::BadInput(
            "method_prefix scoped standing approval requires method_prefix".into(),
        )),
        "workspace_path" => Err(AgentStoreError::BadInput(
            "workspace_path scoped standing approval requires path_glob or workspace_path_glob"
                .into(),
        )),
        other => Err(AgentStoreError::BadInput(format!(
            "unsupported standing approval scope_kind: {other}"
        ))),
    }
}

struct StandingScopeRow<'a> {
    path_glob: Option<&'a str>,
    task_id: Option<&'a str>,
    session_id: Option<&'a str>,
    method_prefix: Option<&'a str>,
    workspace_path_glob: Option<&'a str>,
}

fn standing_scope_matches(
    scope_kind: &str,
    row: StandingScopeRow<'_>,
    input: &StandingApprovalMatch<'_>,
) -> bool {
    match scope_kind {
        "agent_category" => workspace_matches(row.path_glob, input.workspace_path),
        "task" => {
            row.task_id == input.task_id && workspace_matches(row.path_glob, input.workspace_path)
        }
        "session" => {
            row.session_id == input.session_id
                && workspace_matches(row.path_glob, input.workspace_path)
        }
        "method_prefix" => {
            row.method_prefix
                .is_some_and(|prefix| input.method.starts_with(prefix))
                && workspace_matches(row.path_glob, input.workspace_path)
        }
        "workspace_path" => workspace_matches(
            row.workspace_path_glob.or(row.path_glob),
            input.workspace_path,
        ),
        _ => false,
    }
}

fn workspace_matches(pattern: Option<&str>, workspace_path: Option<&str>) -> bool {
    let Some(pattern) = pattern.and_then(non_empty_opt) else {
        return true;
    };
    let Some(path) = workspace_path.and_then(non_empty_opt) else {
        return false;
    };
    if let Some(prefix) = pattern.strip_suffix('*') {
        path.starts_with(prefix)
    } else {
        path == pattern
    }
}

fn row_to_agent(r: &rusqlite::Row) -> rusqlite::Result<AgentProfile> {
    let surface_allowlist: String = r.get(9)?;
    let allow_categories: String = r.get(11)?;
    let deny_categories: String = r.get(12)?;
    let allow_sensitivity_tags: String = r.get(13)?;
    let deny_sensitivity_tags: String = r.get(14)?;
    let approval_required_categories: String = r.get(15)?;
    let authorized_approvers: String = r.get(16)?;
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
        authorized_approvers: parse_json_list(&authorized_approvers),
        approval_timeout_secs: r.get(17)?,
        created_at: r.get(18)?,
        updated_at: r.get(19)?,
        // SEC PART 1: nullable. The ALTER TABLE adds it as
        // a no-default column, so existing rows read NULL
        // → None and stay subject to the standard checks.
        profile: r.get::<_, Option<String>>(20)?,
        // PHASE 0 (org tree): appended as the last SELECT column
        // so every existing positional index above is unchanged.
        reports_to: r.get::<_, Option<String>>(21)?,
        rig: r.get::<_, Option<String>>(22)?,
        monthly_allowance_cents: r.get::<_, Option<i64>>(23)?,
        max_concurrent_runs: r.get::<_, i64>(24)?,
        wake_on_timer: r.get::<_, i64>(25)? != 0,
        wake_on_demand: r.get::<_, i64>(26)? != 0,
        // Org/Work Keys appended last so positional indices above
        // stay stable for existing rows.
        can_spawn_agents: r.get::<_, i64>(27)? != 0,
        spawn_route: r.get::<_, String>(28)?,
        can_assign_work: r.get::<_, i64>(29)? != 0,
        assign_scope: r.get::<_, String>(30)?,
        assign_allowed_agents: parse_json_list(&r.get::<_, String>(31)?),
        can_manage_work: r.get::<_, i64>(32)? != 0,
        can_configure_agents: r.get::<_, i64>(33)? != 0,
        configure_scope: r.get::<_, String>(34)?,
        secret_allowlist: parse_json_list(&r.get::<_, String>(35)?),
        instruction_bundle: r.get::<_, String>(36)?,
        // Appended last (indices 37+) so existing positional indices
        // above stay stable for rows written before these columns.
        manage_scope: r.get::<_, String>(37)?,
        manage_allowed_agents: parse_json_list(&r.get::<_, String>(38)?),
        configure_allowed_agents: parse_json_list(&r.get::<_, String>(39)?),
        // Adapter preferences appended last (indices 40+) so existing
        // positional indices above stay stable for rows written before
        // these columns. Nullable → None on a legacy/unset row.
        model_preference: r.get::<_, Option<String>>(40)?,
        reasoning_effort: r.get::<_, Option<String>>(41)?,
    })
}

fn row_to_approval(r: &rusqlite::Row) -> rusqlite::Result<ApprovalRecord> {
    let groups_json: String = r.get(7)?;
    let status_str: String = r.get(10)?;
    let status = ApprovalStatus::parse(&status_str).unwrap_or(ApprovalStatus::Pending);
    let authorized_approvers_json: String = r.get(16)?;
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
        authorized_approvers: parse_json_list(&authorized_approvers_json),
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

/// The safe set for the adapter `reasoning_effort` preference
/// (relix-agent-adapters.md §3.3 — Codex's `model_reasoning_effort`
/// knob). Constrained at write time so a stored value is always one an
/// adapter could later map; STORED PREFERENCE ONLY today.
fn is_known_effort(s: &str) -> bool {
    matches!(s, "minimal" | "low" | "medium" | "high")
}

fn contains_agent_wire_delimiter(s: &str) -> bool {
    s.contains(['|', '\n', '\r', '\t'])
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

    // ── First-run Founder bootstrap ──────────────────────

    #[test]
    fn ensure_founder_is_idempotent_and_never_duplicates() {
        let s = store();
        let (id1, created1) = s.ensure_founder("Ada", "echo", "owner", "default").unwrap();
        assert!(created1);
        // Second call (different name) returns the SAME founder, created=false.
        let (id2, created2) = s
            .ensure_founder("Other", "claude", "owner", "default")
            .unwrap();
        assert!(!created2);
        assert_eq!(id1, id2);
        // Exactly one founder; the original name/rig are preserved.
        let f = s.find_founder("default").unwrap().unwrap();
        assert_eq!(f.name, "Ada");
        assert_eq!(f.rig.as_deref(), Some("echo"));
        assert_eq!(f.role, "founder");
        assert_eq!(s.list_operatives_for_tenant("default").unwrap().len(), 1);
    }

    #[test]
    fn ensure_founder_defaults_name_and_rig() {
        let s = store();
        let (id, _) = s.ensure_founder("  ", "  ", "  ", "default").unwrap();
        let f = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(f.name, "Founder");
        assert_eq!(f.rig.as_deref(), Some("echo"));
    }

    // ── Adapter model preferences (stored preference only) ────

    #[test]
    fn model_preference_defaults_none_then_set_read_and_clear() {
        let s = store();
        let id = s
            .create_agent(
                "A", "engineer", "Eng", "rd", "core", "op", "subj-mp", "low", "default",
            )
            .unwrap();
        // Fresh row: no preferences stored.
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.model_preference, None);
        assert_eq!(p.reasoning_effort, None);

        // Set both (also covers the read path round-trip).
        s.update_agent_field(&id, "model_preference", "claude-sonnet-4")
            .unwrap();
        s.update_agent_field(&id, "reasoning_effort", "high")
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.model_preference.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(p.reasoning_effort.as_deref(), Some("high"));

        // Empty string CLEARS each field back to NULL/None.
        s.update_agent_field(&id, "model_preference", "  ").unwrap();
        s.update_agent_field(&id, "reasoning_effort", "").unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.model_preference, None);
        assert_eq!(p.reasoning_effort, None);
    }

    #[test]
    fn reasoning_effort_rejects_unknown_tier() {
        let s = store();
        let id = s
            .create_agent(
                "A", "engineer", "Eng", "rd", "core", "op", "subj-re", "low", "default",
            )
            .unwrap();
        let err = s
            .update_agent_field(&id, "reasoning_effort", "turbo")
            .unwrap_err();
        assert!(matches!(err, AgentStoreError::BadInput(_)));
        // The bad write left the field untouched (still None).
        assert_eq!(s.get_agent(&id).unwrap().unwrap().reasoning_effort, None);
    }

    #[test]
    fn model_preference_aliases_and_length_cap() {
        let s = store();
        let id = s
            .create_agent(
                "A", "engineer", "Eng", "rd", "core", "op", "subj-al", "low", "default",
            )
            .unwrap();
        // `model` / `effort` are accepted aliases for the canonical names.
        s.update_agent_field(&id, "model", "gpt-5-codex").unwrap();
        s.update_agent_field(&id, "effort", "minimal").unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.model_preference.as_deref(), Some("gpt-5-codex"));
        assert_eq!(p.reasoning_effort.as_deref(), Some("minimal"));
        // An over-long model name is rejected.
        let long = "m".repeat(200);
        assert!(matches!(
            s.update_agent_field(&id, "model_preference", &long),
            Err(AgentStoreError::BadInput(_))
        ));
    }

    #[test]
    fn model_preference_rejects_agent_wire_delimiters() {
        let s = store();
        let id = s
            .create_agent(
                "A",
                "engineer",
                "Eng",
                "rd",
                "core",
                "op",
                "subj-wire",
                "low",
                "default",
            )
            .unwrap();

        for bad in [
            "claude|sonnet",
            "claude\nsonnet",
            "claude\rsonnet",
            "claude\tsonnet",
        ] {
            assert!(
                matches!(
                    s.update_agent_field(&id, "model_preference", bad),
                    Err(AgentStoreError::BadInput(_))
                ),
                "bad model preference was accepted: {bad:?}"
            );
        }
        assert_eq!(s.get_agent(&id).unwrap().unwrap().model_preference, None);
    }

    #[test]
    fn ensure_starter_operative_is_idempotent_active_and_rigged() {
        let s = store();
        let (id1, c1) = s
            .ensure_starter_operative(
                "engineer",
                "Starter Engineer (local · echo)",
                "Starter Engineer",
                "echo",
                "default",
            )
            .unwrap();
        assert!(c1);
        let p = s.get_agent(&id1).unwrap().unwrap();
        assert_eq!(p.status, "active");
        assert_eq!(p.rig.as_deref(), Some("echo"));
        assert_eq!(p.created_by, AgentStore::STARTER_CREATED_BY);
        assert!(
            !p.can_spawn_agents && !p.can_assign_work,
            "a starter is a worker"
        );
        // Re-ensuring the same role returns the same Operative (no duplicate).
        let (id2, c2) = s
            .ensure_starter_operative("engineer", "X", "Y", "echo", "default")
            .unwrap();
        assert_eq!(id1, id2);
        assert!(!c2);
        assert_eq!(s.list_operatives_for_tenant("default").unwrap().len(), 1);
    }

    #[test]
    fn ensure_starter_operative_is_per_tenant() {
        let s = store();
        s.ensure_starter_operative("engineer", "Eng A", "Eng", "echo", "guild-a")
            .unwrap();
        assert_eq!(s.list_operatives_for_tenant("guild-a").unwrap().len(), 1);
        assert_eq!(s.list_operatives_for_tenant("guild-b").unwrap().len(), 0);
    }

    #[test]
    fn founder_is_per_tenant() {
        let s = store();
        s.ensure_founder("Ada", "echo", "owner", "guild-a").unwrap();
        // Another Guild has no founder until its own bootstrap.
        assert!(s.find_founder("guild-b").unwrap().is_none());
        let (_, created) = s.ensure_founder("Bo", "echo", "owner", "guild-b").unwrap();
        assert!(created);
        assert_eq!(s.find_founder("guild-a").unwrap().unwrap().name, "Ada");
        assert_eq!(s.find_founder("guild-b").unwrap().unwrap().name, "Bo");
    }

    #[test]
    fn grant_console_authority_upgrades_only_the_console_profile() {
        let s = store();
        s.ensure_operator_console_profile("console-subj", "default")
            .unwrap();
        // A normal Operative that must NOT be touched by the grant.
        let normal = s
            .create_agent(
                "Eng", "engineer", "Engineer", "eng", "core", "owner", "eng-subj", "medium",
                "default",
            )
            .unwrap();
        let granted = s
            .grant_console_authority("console-subj", "default")
            .unwrap();
        assert!(granted);
        let console = s.get_by_subject("console-subj").unwrap().unwrap();
        assert!(console.can_assign_work && console.assign_scope == "any");
        assert!(console.can_manage_work && console.manage_scope == "any");
        assert!(console.can_spawn_agents && console.can_configure_agents);
        // The normal Operative's Keys are unchanged (still default-deny).
        let eng = s.get_agent(&normal).unwrap().unwrap();
        assert!(!eng.can_assign_work);
        assert!(!eng.can_manage_work);
        // No console profile for an unknown subject → no-op.
        assert!(!s.grant_console_authority("nobody", "default").unwrap());
    }

    // ── PHASE 0: org tree (reports_to) ───────────────────

    #[test]
    fn phase0_reports_to_sets_clears_and_validates() {
        let s = store();
        let boss = s
            .create_agent(
                "CEO",
                "ceo",
                "Chief",
                "exec",
                "exec",
                "operator",
                "subj-boss",
                "high",
                "default",
            )
            .unwrap();
        let report = s
            .create_agent(
                "Eng",
                "engineer",
                "Engineer",
                "eng",
                "eng",
                "operator",
                "subj-report",
                "medium",
                "default",
            )
            .unwrap();

        // Fresh agents have no boss (apex / unset).
        assert_eq!(s.get_agent(&report).unwrap().unwrap().reports_to, None);

        // Setting the boss link creates the org-tree edge.
        s.update_agent_field(&report, "reports_to", &boss).unwrap();
        assert_eq!(
            s.get_agent(&report).unwrap().unwrap().reports_to,
            Some(boss.clone())
        );

        // An agent cannot report to itself.
        assert!(
            s.update_agent_field(&report, "reports_to", &report)
                .is_err()
        );

        // An unknown boss is rejected — no dangling edges.
        assert!(
            s.update_agent_field(&report, "reports_to", "nope-not-an-agent")
                .is_err()
        );

        // The previously-set valid edge survived the rejected writes.
        assert_eq!(
            s.get_agent(&report).unwrap().unwrap().reports_to,
            Some(boss)
        );

        // Empty value clears the link back to apex / no boss.
        s.update_agent_field(&report, "reports_to", "").unwrap();
        assert_eq!(s.get_agent(&report).unwrap().unwrap().reports_to, None);
    }

    #[test]
    fn phase0_org_tree_queries_walk_up_and_down() {
        let s = store();
        // CEO → planner → {worker1, worker2}
        let ceo = s
            .create_agent(
                "CEO", "ceo", "Chief", "exec", "exec", "op", "subj-ceo", "high", "default",
            )
            .unwrap();
        let planner = s
            .create_agent(
                "Plan",
                "planner",
                "Planner",
                "eng",
                "eng",
                "op",
                "subj-plan",
                "medium",
                "default",
            )
            .unwrap();
        let w1 = s
            .create_agent(
                "W1", "worker", "Worker", "eng", "eng", "op", "subj-w1", "low", "default",
            )
            .unwrap();
        let w2 = s
            .create_agent(
                "W2", "worker", "Worker", "eng", "eng", "op", "subj-w2", "low", "default",
            )
            .unwrap();
        s.update_agent_field(&planner, "reports_to", &ceo).unwrap();
        s.update_agent_field(&w1, "reports_to", &planner).unwrap();
        s.update_agent_field(&w2, "reports_to", &planner).unwrap();

        // Down one level.
        let ceo_reports: Vec<String> = s
            .list_direct_reports(&ceo)
            .unwrap()
            .into_iter()
            .map(|a| a.agent_id)
            .collect();
        assert_eq!(ceo_reports, vec![planner.clone()]);
        let planner_reports: Vec<String> = s
            .list_direct_reports(&planner)
            .unwrap()
            .into_iter()
            .map(|a| a.agent_id)
            .collect();
        assert_eq!(planner_reports.len(), 2);
        assert!(planner_reports.contains(&w1) && planner_reports.contains(&w2));

        // The whole subtree under the CEO is everyone but the CEO.
        let subtree = s.manager_subtree(&ceo).unwrap();
        assert_eq!(subtree.len(), 3);
        for id in [&planner, &w1, &w2] {
            assert!(subtree.contains(id));
        }
        assert!(!subtree.contains(&ceo));

        // Escalation path up from a worker: planner, then CEO.
        assert_eq!(
            s.chain_of_command(&w1).unwrap(),
            vec![planner.clone(), ceo.clone()]
        );
        // The apex escalates to nobody.
        assert!(s.chain_of_command(&ceo).unwrap().is_empty());
    }

    #[test]
    fn list_by_role_returns_active_matches_only() {
        let s = store();
        let e1 = s
            .create_agent(
                "E1", "engineer", "E", "e", "e", "op", "subj-br1", "low", "default",
            )
            .unwrap();
        let e2 = s
            .create_agent(
                "E2", "engineer", "E", "e", "e", "op", "subj-br2", "low", "default",
            )
            .unwrap();
        let _d = s
            .create_agent(
                "D", "designer", "D", "e", "e", "op", "subj-br3", "low", "default",
            )
            .unwrap();
        // A pending engineer is not assignable.
        s.request_hire(
            "E3", "engineer", "E", "e", "e", "op", "subj-br4", "low", "default",
        )
        .unwrap();
        // A suspended engineer is excluded.
        s.update_agent_field(&e2, "status", "suspended").unwrap();

        let engineers = s.list_by_role("engineer").unwrap();
        assert_eq!(engineers, vec![e1]);
        assert!(s.list_by_role("manager").unwrap().is_empty());
    }

    #[test]
    fn list_active_for_tenant_is_oldest_first_active_only_and_isolated() {
        let s = store();
        let e1 = s
            .create_agent(
                "E1", "engineer", "E", "e", "e", "op", "subj-a1", "low", "default",
            )
            .unwrap();
        let e2 = s
            .create_agent(
                "E2", "engineer", "E", "e", "e", "op", "subj-a2", "low", "default",
            )
            .unwrap();
        // A pending hire is not active.
        s.request_hire(
            "E3", "engineer", "E", "e", "e", "op", "subj-a3", "low", "default",
        )
        .unwrap();
        // A crew member in another tenant must never appear.
        s.create_agent(
            "X", "engineer", "E", "e", "e", "op", "subj-x", "low", "tenant-b",
        )
        .unwrap();

        let ids: Vec<String> = s
            .list_active_for_tenant("default")
            .unwrap()
            .into_iter()
            .map(|p| p.agent_id)
            .collect();
        // Oldest-first (insertion order), active only, tenant-scoped.
        assert_eq!(ids, vec![e1, e2]);
        // tenant-b sees only its own active crew.
        assert_eq!(s.list_active_for_tenant("tenant-b").unwrap().len(), 1);
    }

    #[test]
    fn list_peers_returns_same_lead_siblings() {
        let s = store();
        let ceo = s
            .create_agent(
                "CEO", "ceo", "C", "x", "x", "op", "subj-pr0", "high", "default",
            )
            .unwrap();
        let a = s
            .create_agent(
                "A", "eng", "A", "e", "e", "op", "subj-pr1", "low", "default",
            )
            .unwrap();
        let b = s
            .create_agent(
                "B", "eng", "B", "e", "e", "op", "subj-pr2", "low", "default",
            )
            .unwrap();
        let c = s
            .create_agent(
                "C", "eng", "C", "e", "e", "op", "subj-pr3", "low", "default",
            )
            .unwrap();
        // a, b, c all report to ceo.
        for x in [&a, &b, &c] {
            s.update_agent_field(x, "reports_to", &ceo).unwrap();
        }

        let peers = s.list_peers(&a).unwrap();
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&b) && peers.contains(&c));
        assert!(!peers.contains(&a), "excludes self");
        // The apex has no Lead → no peers.
        assert!(s.list_peers(&ceo).unwrap().is_empty());
    }

    #[test]
    fn reports_to_rejects_cycles() {
        let s = store();
        let ceo = s
            .create_agent(
                "CEO", "ceo", "C", "x", "x", "op", "subj-cy1", "high", "default",
            )
            .unwrap();
        let lead = s
            .create_agent(
                "L", "lead", "L", "e", "e", "op", "subj-cy2", "medium", "default",
            )
            .unwrap();
        let ic = s
            .create_agent(
                "IC", "worker", "I", "e", "e", "op", "subj-cy3", "low", "default",
            )
            .unwrap();
        // ceo <- lead <- ic
        s.update_agent_field(&lead, "reports_to", &ceo).unwrap();
        s.update_agent_field(&ic, "reports_to", &lead).unwrap();

        // Direct 2-cycle: ceo can't report to its own report.
        assert!(matches!(
            s.update_agent_field(&ceo, "reports_to", &lead),
            Err(AgentStoreError::BadInput(_))
        ));
        // Deep cycle: ceo can't report to a descendant further down.
        assert!(matches!(
            s.update_agent_field(&ceo, "reports_to", &ic),
            Err(AgentStoreError::BadInput(_))
        ));
        // The valid edges are untouched after the rejected writes.
        assert_eq!(s.get_agent(&ceo).unwrap().unwrap().reports_to, None);
        // A legal re-parent still works (ic moves under ceo directly).
        s.update_agent_field(&ic, "reports_to", &ceo).unwrap();
        assert_eq!(
            s.get_agent(&ic).unwrap().unwrap().reports_to.as_deref(),
            Some(ceo.as_str())
        );
    }

    #[test]
    fn manages_reflects_the_branch_subtree() {
        let s = store();
        let ceo = s
            .create_agent(
                "CEO", "ceo", "C", "x", "x", "op", "subj-mc", "high", "default",
            )
            .unwrap();
        let planner = s
            .create_agent(
                "P", "planner", "P", "e", "e", "op", "subj-mp", "medium", "default",
            )
            .unwrap();
        let worker = s
            .create_agent(
                "W", "worker", "W", "e", "e", "op", "subj-mw", "low", "default",
            )
            .unwrap();
        let outsider = s
            .create_agent(
                "O", "worker", "O", "e", "e", "op", "subj-mo", "low", "default",
            )
            .unwrap();
        s.update_agent_field(&planner, "reports_to", &ceo).unwrap();
        s.update_agent_field(&worker, "reports_to", &planner)
            .unwrap();

        assert!(s.manages(&ceo, &planner).unwrap());
        assert!(s.manages(&ceo, &worker).unwrap());
        assert!(s.manages(&planner, &worker).unwrap());
        assert!(!s.manages(&planner, &ceo).unwrap());
        assert!(!s.manages(&ceo, &outsider).unwrap());
        assert!(!s.manages(&ceo, &ceo).unwrap());
    }

    #[test]
    fn status_counts_summarize_the_roster() {
        let s = store();
        s.create_agent("a", "r", "t", "d", "t", "op", "s1", "low", "default")
            .unwrap();
        s.create_agent("b", "r", "t", "d", "t", "op", "s2", "low", "default")
            .unwrap();
        s.request_hire("c", "r", "t", "d", "t", "op", "s3", "low", "default")
            .unwrap();

        let map: std::collections::HashMap<String, i64> =
            s.status_counts().unwrap().into_iter().collect();
        assert_eq!(map.get("active"), Some(&2));
        assert_eq!(map.get("pending"), Some(&1));
        assert_eq!(map.values().sum::<i64>(), 3);
    }

    #[test]
    fn committed_allowance_sums_active_roster_only() {
        let s = store();
        let a = s
            .create_agent("a", "r", "t", "d", "t", "op", "s1", "low", "default")
            .unwrap();
        let b = s
            .create_agent("b", "r", "t", "d", "t", "op", "s2", "low", "default")
            .unwrap();
        // A pending hire's allowance must NOT count (inert headcount).
        let c = s
            .request_hire("c", "r", "t", "d", "t", "op", "s3", "low", "default")
            .unwrap();
        s.update_agent_field(&a, "allowance", "5000").unwrap();
        s.update_agent_field(&b, "allowance", "2500").unwrap();
        s.update_agent_field(&c, "allowance", "9999").unwrap();

        // b still has no allowance set on creation; a=5000, b=2500.
        assert_eq!(s.committed_allowance_cents().unwrap(), 7500);
    }

    // ── PHASE 4: hire flow ───────────────────────────────

    #[test]
    fn runtime_keys_default_update_and_validate() {
        let s = store();
        let id = s
            .create_agent(
                "runner",
                "worker",
                "W",
                "eng",
                "eng",
                "op",
                "subj-runner",
                "low",
                "default",
            )
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.max_concurrent_runs, 20);
        assert!(p.wake_on_timer);
        assert!(p.wake_on_demand);

        s.update_agent_field(&id, "max_concurrent_runs", "3")
            .unwrap();
        s.update_agent_field(&id, "wake_on_timer", "false").unwrap();
        s.update_agent_field(&id, "wake_on_demand", "off").unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.max_concurrent_runs, 3);
        assert!(!p.wake_on_timer);
        assert!(!p.wake_on_demand);

        assert!(
            s.update_agent_field(&id, "max_concurrent_runs", "0")
                .is_err()
        );
        assert!(
            s.update_agent_field(&id, "max_concurrent_runs", "51")
                .is_err()
        );
        assert!(s.update_agent_field(&id, "wake_on_timer", "maybe").is_err());
    }

    #[test]
    fn hire_flow_is_pending_until_approved() {
        let s = store();
        let id = s
            .request_hire(
                "Eng",
                "engineer",
                "E",
                "eng",
                "eng",
                "ceo",
                "subj-hire",
                "low",
                "default",
            )
            .unwrap();
        // A fresh hire is pending (inert — gate denies non-active).
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "pending");

        // Approve → active.
        s.approve_hire(&id).unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "active");
        // Can't "approve" again — it's no longer pending.
        assert!(s.approve_hire(&id).is_err());

        // Reject a fresh pending hire → disabled (terminal).
        let id2 = s
            .request_hire("X", "r", "t", "d", "t", "ceo", "subj-h2", "low", "default")
            .unwrap();
        s.reject_hire(&id2).unwrap();
        assert_eq!(s.get_agent(&id2).unwrap().unwrap().status, "disabled");
        // Rejecting a non-pending agent errors.
        assert!(s.reject_hire(&id).is_err());
    }

    #[test]
    fn approve_hire_with_rig_activates_runnable_and_is_no_clobber() {
        let s = store();
        let req = |subj: &str| {
            s.request_hire("Q", "qa", "QA", "qa", "qa", "ceo", subj, "low", "default")
                .unwrap()
        };

        // (a) Approve WITH a Rig → active + Rig bound in one atomic step. The
        // approved Operative is immediately runnable (no separate PATCH).
        let id = req("subj-a");
        let out = s
            .approve_hire_with_rig(&id, Some("echo"), "default")
            .unwrap();
        assert!(out.rig_set, "this call set the Rig");
        assert_eq!(out.rig.as_deref(), Some("echo"));
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.status, "active");
        assert_eq!(p.rig.as_deref(), Some("echo"), "Rig bound at approval");

        // (b) Duplicate approval (same Rig) is a safe no-op — it refuses
        // (already active) and never clobbers the Rig.
        let dup = s.approve_hire_with_rig(&id, Some("echo"), "default");
        assert!(matches!(dup, Err(AgentStoreError::BadInput(_))));
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().rig.as_deref(),
            Some("echo"),
            "duplicate approval left the Rig intact"
        );

        // (c) Duplicate approval with a CONFLICTING Rig also refuses and does
        // NOT silently clobber the existing Rig.
        let conflict = s.approve_hire_with_rig(&id, Some("claude"), "default");
        assert!(matches!(conflict, Err(AgentStoreError::BadInput(_))));
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().rig.as_deref(),
            Some("echo"),
            "conflicting re-approval must not clobber the bound Rig"
        );

        // (d) Approve WITHOUT a Rig → active but un-runnable (no Rig); the
        // outcome reports that honestly.
        let id2 = req("subj-d");
        let out2 = s.approve_hire_with_rig(&id2, None, "default").unwrap();
        assert!(!out2.rig_set);
        assert_eq!(out2.rig, None, "no Rig bound → not runnable");
        let p2 = s.get_agent(&id2).unwrap().unwrap();
        assert_eq!(p2.status, "active");
        assert_eq!(p2.rig, None);
    }

    #[test]
    fn approve_hire_with_rig_is_tenant_scoped_no_existence_leak() {
        let s = store();
        // A pending hire in the `acme` Guild.
        let id = s
            .request_hire("Q", "qa", "QA", "qa", "qa", "ceo", "subj-x", "low", "acme")
            .unwrap();
        // A caller in a DIFFERENT Guild cannot approve it — and the error is
        // `NotFound` (identical to a truly-missing id), so existence never leaks.
        let cross = s.approve_hire_with_rig(&id, Some("echo"), "other");
        assert!(matches!(cross, Err(AgentStoreError::NotFound(_))));
        // The hire is untouched (still pending) in its own Guild.
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "pending");
        // The owning Guild approves it normally.
        s.approve_hire_with_rig(&id, Some("echo"), "acme").unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "active");
    }

    // ── PILLAR 2: Rig (agent backend) ────────────────────

    #[test]
    fn pillar2_rig_field_sets_and_clears() {
        let s = store();
        let id = s
            .create_agent(
                "n", "engineer", "Eng", "eng", "eng", "op", "subj-rig", "low", "default",
            )
            .unwrap();
        // Default: no Rig (use the Guild default at dispatch).
        assert_eq!(s.get_agent(&id).unwrap().unwrap().rig, None);
        // Set a Rig.
        s.update_agent_field(&id, "rig", "claude").unwrap();
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().rig.as_deref(),
            Some("claude")
        );
        // Clear it back to the default.
        s.update_agent_field(&id, "rig", "").unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().rig, None);
    }

    #[test]
    fn agent_allowance_sets_clears_and_validates() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "op", "subj-allw", "low", "default")
            .unwrap();
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().monthly_allowance_cents,
            None
        );
        s.update_agent_field(&id, "allowance", "25000").unwrap();
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().monthly_allowance_cents,
            Some(25000)
        );
        s.update_agent_field(&id, "allowance", "").unwrap();
        assert_eq!(
            s.get_agent(&id).unwrap().unwrap().monthly_allowance_cents,
            None
        );
        assert!(s.update_agent_field(&id, "allowance", "-5").is_err());
        assert!(s.update_agent_field(&id, "allowance", "abc").is_err());
    }

    // ── agent CRUD ───────────────────────────────────────

    #[test]
    fn group6_approval_and_standing_reads_are_isolated_by_verified_tenant() {
        let s = store();
        // approval_requests: two tenants, tenant-scoped get.
        let a = s
            .create_approval(
                "ag",
                "subj",
                "m",
                "c",
                "",
                "r",
                &[],
                None,
                9_999_999_999,
                &[],
                "tenant-a",
            )
            .unwrap();
        let b = s
            .create_approval(
                "ag",
                "subj",
                "m",
                "c",
                "",
                "r",
                &[],
                None,
                9_999_999_999,
                &[],
                "tenant-b",
            )
            .unwrap();
        assert!(s.get_approval_for_tenant(&a, "tenant-a").unwrap().is_some());
        assert!(
            s.get_approval_for_tenant(&b, "tenant-a").unwrap().is_none(),
            "tenant A must not read tenant B's approval request"
        );
        // standing_approvals: two tenants for the same agent.
        s.create_standing(
            "shared-agent",
            "fetch",
            None,
            9_999_999_999,
            "op",
            "n",
            "tenant-a",
        )
        .unwrap();
        s.create_standing(
            "shared-agent",
            "fetch",
            None,
            9_999_999_999,
            "op",
            "n",
            "tenant-b",
        )
        .unwrap();
        assert_eq!(
            s.count_standing_for_tenant("tenant-a", "shared-agent")
                .unwrap(),
            1
        );
        assert_eq!(
            s.count_standing_for_tenant("tenant-b", "shared-agent")
                .unwrap(),
            1
        );
    }

    #[test]
    fn group6_agent_profile_reads_are_isolated_by_verified_tenant() {
        // An agent profile created under tenant A must not be
        // readable via the tenant-scoped lookup as tenant B,
        // even though the caller knows the subject_id.
        let s = store();
        s.create_agent(
            "A", "research", "t", "d", "tm", "creator", "subj-a", "medium", "tenant-a",
        )
        .unwrap();
        // Same tenant + subject → visible.
        assert!(
            s.get_by_subject_for_tenant("subj-a", "tenant-a")
                .unwrap()
                .is_some()
        );
        // Different tenant → NOT visible, even with the subject id.
        assert!(
            s.get_by_subject_for_tenant("subj-a", "tenant-b")
                .unwrap()
                .is_none(),
            "tenant B must not read tenant A's agent profile"
        );
    }

    #[test]
    fn tenant_scoped_agent_id_reads_writes_isolate_across_guilds() {
        // GROUP 6: a known agent_id from tenant A must not be readable,
        // updatable, or deletable as tenant B.
        let s = store();
        let id = s
            .create_agent(
                "A", "research", "t", "d", "tm", "creator", "subj-a", "medium", "tenant-a",
            )
            .unwrap();
        // Read: visible to A, invisible to B.
        assert!(s.get_agent_for_tenant(&id, "tenant-a").unwrap().is_some());
        assert!(
            s.get_agent_for_tenant(&id, "tenant-b").unwrap().is_none(),
            "tenant B must not read tenant A's Operative by agent_id"
        );
        // Update: B is refused (NotFound, not a wrong-tenant leak); A works.
        assert!(matches!(
            s.update_agent_field_for_tenant(&id, "tenant-b", "title", "pwned"),
            Err(AgentStoreError::NotFound(_))
        ));
        assert_eq!(s.get_agent(&id).unwrap().unwrap().title, "t");
        s.update_agent_field_for_tenant(&id, "tenant-a", "title", "lead")
            .unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().title, "lead");
        // Delete: B is refused; the Operative stays active.
        assert!(matches!(
            s.soft_delete_for_tenant(&id, "tenant-b"),
            Err(AgentStoreError::NotFound(_))
        ));
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "active");
        s.soft_delete_for_tenant(&id, "tenant-a").unwrap();
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "disabled");
    }

    #[test]
    fn tenant_scoped_branch_and_manages_do_not_cross_guilds() {
        // GROUP 6: the org tree is per-Guild. A manager + worker in
        // tenant A must not appear as a Branch / management edge for B.
        let s = store();
        let mgr = s
            .create_agent(
                "M", "planner", "t", "d", "tm", "creator", "subj-m", "medium", "tenant-a",
            )
            .unwrap();
        let worker = s
            .create_agent(
                "W", "engineer", "t", "d", "tm", "creator", "subj-w", "medium", "tenant-a",
            )
            .unwrap();
        s.update_agent_field_for_tenant(&worker, "tenant-a", "reports_to", &mgr)
            .unwrap();
        // Branch + manages resolve within tenant A only.
        assert_eq!(
            s.manager_subtree_for_tenant(&mgr, "tenant-a").unwrap(),
            vec![worker.clone()]
        );
        assert!(s.manages_for_tenant(&mgr, &worker, "tenant-a").unwrap());
        // From tenant B the Branch is empty and the edge does not exist.
        assert!(
            s.manager_subtree_for_tenant(&mgr, "tenant-b")
                .unwrap()
                .is_empty()
        );
        assert!(!s.manages_for_tenant(&mgr, &worker, "tenant-b").unwrap());
    }

    #[test]
    fn tenant_scoped_allowance_sums_only_that_guild() {
        // GROUP 6: committed Allowance must not leak another Guild's
        // spend commitment.
        let s = store();
        let a = s
            .create_agent(
                "A", "research", "t", "d", "tm", "creator", "subj-a", "medium", "tenant-a",
            )
            .unwrap();
        let b = s
            .create_agent(
                "B", "research", "t", "d", "tm", "creator", "subj-b", "medium", "tenant-b",
            )
            .unwrap();
        s.update_agent_field_for_tenant(&a, "tenant-a", "allowance", "1000")
            .unwrap();
        s.update_agent_field_for_tenant(&b, "tenant-b", "allowance", "9999")
            .unwrap();
        assert_eq!(
            s.committed_allowance_cents_for_tenant("tenant-a").unwrap(),
            1000
        );
        assert_eq!(
            s.committed_allowance_cents_for_tenant("tenant-b").unwrap(),
            9999
        );
    }

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
                "default",
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
        let r = s.create_agent("n", "r", "t", "d", "t", "c", "subj", "extreme", "default");
        assert!(matches!(r, Err(AgentStoreError::BadInput(_))));
    }

    #[test]
    fn new_org_keys_default_deny_and_round_trip_through_update() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj-keys", "low", "default")
            .unwrap();
        // Fresh Operative is default-deny on the org/work Keys (§5.1).
        let p = s.get_agent(&id).unwrap().unwrap();
        assert!(!p.can_spawn_agents);
        assert!(!p.can_assign_work);
        assert_eq!(p.spawn_route, "founder");
        assert_eq!(p.assign_scope, "specific");
        assert_eq!(p.configure_scope, "none");
        assert!(p.assign_allowed_agents.is_empty());
        assert!(p.instruction_bundle.is_empty());
        // The Founder grants Keys via agent.update.
        s.update_agent_field(&id, "can_spawn_agents", "true")
            .unwrap();
        s.update_agent_field(&id, "spawn_route", "direct").unwrap();
        s.update_agent_field(&id, "can_assign_work", "true")
            .unwrap();
        s.update_agent_field(&id, "assign_scope", "branch").unwrap();
        s.update_agent_field(&id, "assign_allowed_agents", "agt_a, agt_b")
            .unwrap();
        s.update_agent_field(&id, "charter", "# You lead.\nDelegate.")
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert!(p.can_spawn_agents);
        assert_eq!(p.spawn_route, "direct");
        assert!(p.can_assign_work);
        assert_eq!(p.assign_scope, "branch");
        assert_eq!(p.assign_allowed_agents, vec!["agt_a", "agt_b"]);
        assert_eq!(p.instruction_bundle, "# You lead.\nDelegate.");
    }

    #[test]
    fn manage_configure_keys_default_deny_and_round_trip() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj-mc", "low", "default")
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        // Defaults: deny + narrowest scopes + empty allowlists.
        assert!(!p.can_manage_work);
        assert!(!p.can_configure_agents);
        assert_eq!(p.manage_scope, "specific");
        assert_eq!(p.configure_scope, "none");
        assert!(p.manage_allowed_agents.is_empty());
        assert!(p.configure_allowed_agents.is_empty());
        // Grant + round-trip.
        s.update_agent_field(&id, "can_manage_work", "true")
            .unwrap();
        s.update_agent_field(&id, "manage_scope", "branch").unwrap();
        s.update_agent_field(&id, "manage_allowed_agents", "agt_m1, agt_m2")
            .unwrap();
        s.update_agent_field(&id, "can_configure_agents", "true")
            .unwrap();
        s.update_agent_field(&id, "configure_scope", "any").unwrap();
        s.update_agent_field(&id, "configure_allowed_agents", "agt_c1")
            .unwrap();
        let p = s.get_agent(&id).unwrap().unwrap();
        assert!(p.can_manage_work);
        assert_eq!(p.manage_scope, "branch");
        assert_eq!(p.manage_allowed_agents, vec!["agt_m1", "agt_m2"]);
        assert!(p.can_configure_agents);
        assert_eq!(p.configure_scope, "any");
        assert_eq!(p.configure_allowed_agents, vec!["agt_c1"]);
    }

    #[test]
    fn org_key_scope_values_are_validated() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj-val", "low", "default")
            .unwrap();
        assert!(matches!(
            s.update_agent_field(&id, "spawn_route", "sideways"),
            Err(AgentStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.update_agent_field(&id, "assign_scope", "everyone"),
            Err(AgentStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.update_agent_field(&id, "configure_scope", "world"),
            Err(AgentStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.update_agent_field(&id, "manage_scope", "everyone"),
            Err(AgentStoreError::BadInput(_))
        ));
        // `any` is now a valid configure_scope.
        s.update_agent_field(&id, "configure_scope", "any").unwrap();
    }

    #[test]
    fn get_by_subject_returns_the_profile() {
        let s = store();
        let id = s
            .create_agent("n", "r", "t", "d", "t", "c", "subj-x", "low", "default")
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
        s.create_agent("a", "r", "t", "d", "t", "c", "subj-1", "low", "default")
            .unwrap();
        s.create_agent("b", "r", "t", "d", "t", "c", "subj-2", "low", "default")
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
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium", "default")
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
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium", "default")
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
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium", "default")
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
            .create_agent("n", "r", "t", "d", "t", "c", "subj", "medium", "default")
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
                &[],
                "default",
            )
            .unwrap();
        let r = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(r.method, "tool.web_post");
        assert_eq!(r.status, ApprovalStatus::Pending);
        assert_eq!(r.task_id.as_deref(), Some("task-1"));
        assert!(r.approval_token.is_none());
    }

    #[test]
    fn decide_approved_returns_metadata_for_signed_token_mint() {
        // SEC PART A: decide_approval no longer mints a random
        // string token. It returns the metadata the cap handler
        // needs to mint a structured signed
        // `ApprovalToken`. The legacy `approval_token` column is
        // left NULL on new rows.
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
                Some("task-7"),
                unix_now() + 60,
                &[],
                "default",
            )
            .unwrap();
        let meta = s
            .decide_approval(&id, ApprovalStatus::Approved, "alice", "ok")
            .unwrap()
            .expect("approved -> Some(metadata)");
        assert_eq!(meta.approval_id, id);
        assert_eq!(meta.subject_id, "subj-1");
        assert_eq!(meta.method, "tool.x");
        assert_eq!(meta.task_id.as_deref(), Some("task-7"));
        // Legacy column is NOT written.
        let row = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Approved);
        assert!(row.approval_token.is_none());
    }

    #[test]
    fn token_blocklist_first_claim_wins_replay_loses() {
        // SEC PART A: replay defense lives on the
        // approval_token_blocklist (PRIMARY KEY on token_id).
        // The first atomic INSERT wins; the second sees the
        // existing row and returns false.
        let s = store();
        let claimed_first = s
            .try_consume_token_atomic("token-id-aaa", "apr-1", 100)
            .unwrap();
        assert!(claimed_first);
        let claimed_again = s
            .try_consume_token_atomic("token-id-aaa", "apr-1", 200)
            .unwrap();
        assert!(!claimed_again, "replay must lose");
        assert_eq!(
            s.token_blocklist_consumed_at("token-id-aaa")
                .unwrap()
                .unwrap(),
            100,
            "first-claim timestamp must NOT regress on replay"
        );
    }

    #[test]
    fn decide_rejected_returns_no_token() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                unix_now() + 60,
                &[],
                "default",
            )
            .unwrap();
        let meta = s
            .decide_approval(&id, ApprovalStatus::Rejected, "alice", "nope")
            .unwrap();
        assert!(meta.is_none());
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status,
            ApprovalStatus::Rejected
        );
    }

    #[test]
    fn decide_refuses_terminal_approval() {
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                unix_now() + 60,
                &[],
                "default",
            )
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
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                unix_now() + 60,
                &[],
                "default",
            )
            .unwrap();
        let b = s
            .create_approval(
                "b",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                unix_now() + 60,
                &[],
                "default",
            )
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
            .create_approval("a", "s", "m", "c", "", "", &[], None, 100, &[], "default")
            .unwrap();
        // expires_at = 100; query with now = 1000.
        let v = s.list_expired_pending(1000).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn mark_expired_flips_status() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, 100, &[], "default")
            .unwrap();
        s.mark_expired(&id).unwrap();
        assert_eq!(
            s.get_approval(&id).unwrap().unwrap().status,
            ApprovalStatus::Expired
        );
    }

    // ── NOT-DONE 1: migration runs with injected clock ───

    #[test]
    fn migration_stamps_decided_at_from_injected_clock() {
        // Seed a legacy pending row + migrate with a FakeClock
        // pinned to a fixed time. The stamped `decided_at`
        // (unix seconds, derived from the clock's `now_ms /
        // 1000`) must match what the FakeClock returned at
        // call time.
        let s = store();
        s.seed_legacy_token_row_for_test("leg-clock", "pending", "abc")
            .unwrap();
        // Pin to a clock value whose seconds equivalent is
        // distinct from `unix_now` so we can detect a
        // wall-clock fallback regression.
        let clock = relix_core::clock::FakeClock::new(2_700_000_000_000);
        let now_ms = <relix_core::clock::FakeClock as relix_core::clock::Clock>::now_ms(&clock);
        {
            let conn = s.conn.lock().unwrap();
            let n = migrate_legacy_opaque_tokens(&conn, now_ms).unwrap();
            assert_eq!(n, 1);
        }
        let r = s.get_approval("leg-clock").unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::LegacyTokenExpired);
        // FakeClock value 2_700_000_000_000 ms → 2_700_000_000
        // seconds.
        assert_eq!(
            r.decided_at,
            Some(2_700_000_000),
            "decided_at must come from FakeClock, not wall-clock"
        );
    }

    // ── DEFERRED 3: legacy opaque-token migration ────────

    #[test]
    fn migration_flips_pending_row_with_legacy_token() {
        let s = store();
        s.seed_legacy_token_row_for_test("leg-1", "pending", "deadbeefcafef00d")
            .unwrap();
        // open() already ran the migration on the empty store;
        // re-run via the public test helper now that the row
        // exists.
        let n = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(n, 1, "exactly one pending+token row migrated");
        let r = s.get_approval("leg-1").unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::LegacyTokenExpired);
        assert!(
            r.decision_note
                .as_deref()
                .unwrap_or("")
                .contains("legacy_token_expired"),
            "decision_note must name the failure mode: {:?}",
            r.decision_note
        );
    }

    #[test]
    fn migration_flips_approved_row_with_legacy_token() {
        // The common case: pre-PART-A `decide_approval` left the
        // row in `approved` status with a random token. After
        // upgrade the agent's wire token will never verify; the
        // migration flips the row so the agent sees a clear
        // signal next time.
        let s = store();
        s.seed_legacy_token_row_for_test("leg-2", "approved", "0a1b2c3d4e5f6789")
            .unwrap();
        let n = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(n, 1);
        let r = s.get_approval("leg-2").unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::LegacyTokenExpired);
    }

    #[test]
    fn migration_leaves_structured_token_rows_alone_when_token_is_null() {
        // SEC-PART-A-vintage rows have approval_token = NULL
        // (structured tokens live only on the wire). The
        // migration must NOT touch them.
        let s = store();
        let id = s
            .create_approval(
                "a",
                "s",
                "m",
                "c",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                &[],
                "default",
            )
            .unwrap();
        let n = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(n, 0, "no rows touched: pending row has no token");
        let r = s.get_approval(&id).unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::Pending);
    }

    #[test]
    fn migration_is_idempotent_under_repeat_run() {
        let s = store();
        s.seed_legacy_token_row_for_test("leg-3", "pending", "ffeeddcc")
            .unwrap();
        s.seed_legacy_token_row_for_test("leg-4", "approved", "aabbccdd")
            .unwrap();
        let first = s.run_legacy_token_migration_for_test().unwrap();
        let second = s.run_legacy_token_migration_for_test().unwrap();
        let third = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(first, 2, "first run flips both rows");
        assert_eq!(second, 0, "second run finds nothing");
        assert_eq!(third, 0, "third run finds nothing");
    }

    #[test]
    fn migration_does_not_touch_rejected_or_consumed_rows() {
        // `rejected` / `expired` / `consumed` / `legacy_token_expired`
        // are all terminal. Even a row with a stale token in those
        // states stays put — the operator's record of the past
        // decision must not be rewritten.
        let s = store();
        s.seed_legacy_token_row_for_test("leg-rej", "rejected", "abc")
            .unwrap();
        s.seed_legacy_token_row_for_test("leg-cons", "consumed", "def")
            .unwrap();
        s.seed_legacy_token_row_for_test("leg-exp", "expired", "ghi")
            .unwrap();
        let n = s.run_legacy_token_migration_for_test().unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            s.get_approval("leg-rej").unwrap().unwrap().status,
            ApprovalStatus::Rejected
        );
        assert_eq!(
            s.get_approval("leg-cons").unwrap().unwrap().status,
            ApprovalStatus::Consumed
        );
        assert_eq!(
            s.get_approval("leg-exp").unwrap().unwrap().status,
            ApprovalStatus::Expired
        );
    }

    #[test]
    fn approval_status_parse_round_trips_legacy_token_expired() {
        // Status enum invariants — `as_wire` must produce a
        // string `parse` re-accepts, both for the new
        // variant + every existing one. Locks the round-trip
        // contract.
        for s in [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Expired,
            ApprovalStatus::Consumed,
            ApprovalStatus::LegacyTokenExpired,
        ] {
            let wire = s.as_wire();
            let back = ApprovalStatus::parse(wire).expect("round-trip");
            assert_eq!(back, s, "wire = {wire:?}");
        }
        assert_eq!(
            ApprovalStatus::LegacyTokenExpired.as_wire(),
            "legacy_token_expired"
        );
    }

    // ── standing approvals ───────────────────────────────

    #[test]
    fn create_standing_then_has_active_returns_true() {
        let s = store();
        let _id = s
            .create_standing(
                "agt-1",
                "fs",
                None,
                unix_now() + 86400,
                "alice",
                "",
                "default",
            )
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
            .create_standing("agt-1", "fs", None, 100, "alice", "", "default")
            .unwrap();
        assert!(!s.has_active_standing("agt-1", "fs", 1000).unwrap());
    }

    #[test]
    fn revoke_standing_drops_the_row() {
        let s = store();
        let id = s
            .create_standing("agt-1", "fs", None, unix_now() + 60, "alice", "", "default")
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
        s.create_standing(
            "agt-1",
            "fs",
            None,
            unix_now() + 60,
            "alice",
            "n1",
            "default",
        )
        .unwrap();
        s.create_standing(
            "agt-1",
            "browser",
            None,
            unix_now() + 60,
            "alice",
            "n2",
            "default",
        )
        .unwrap();
        s.create_standing(
            "agt-2",
            "fs",
            None,
            unix_now() + 60,
            "alice",
            "n3",
            "default",
        )
        .unwrap();
        let v = s.list_standing("agt-1").unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn list_standing_for_tenant_does_not_cross_tenant_boundary() {
        let s = store();
        s.create_standing(
            "agt-1",
            "fs",
            None,
            unix_now() + 60,
            "alice",
            "tenant-a-row",
            "tenant-a",
        )
        .unwrap();
        s.create_standing(
            "agt-1",
            "browser",
            None,
            unix_now() + 60,
            "bob",
            "tenant-b-row",
            "tenant-b",
        )
        .unwrap();

        let a = s.list_standing_for_tenant("agt-1", "tenant-a").unwrap();
        let b = s.list_standing_for_tenant("agt-1", "tenant-b").unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].note, "tenant-a-row");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].note, "tenant-b-row");
    }

    #[test]
    fn task_scoped_standing_only_matches_the_bound_task() {
        let s = store();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: "agt-1",
            match_category: "fs",
            match_path_glob: None,
            scope_kind: Some("task"),
            task_id: Some("task-123"),
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            expires_at: unix_now() + 60,
            granted_by: "alice",
            max_calls: None,
            max_cost_micros: None,
            note: "single task",
            tenant_id: "default",
        })
        .unwrap();

        assert!(
            s.has_active_standing_for(StandingApprovalMatch {
                agent_id: "agt-1",
                category: "fs",
                method: "tool.fs.read",
                task_id: Some("task-123"),
                session_id: None,
                workspace_path: None,
                tenant_id: Some("default"),
                estimated_cost_micros: 0,
                now: unix_now(),
            })
            .unwrap()
        );
        assert!(
            !s.has_active_standing_for(StandingApprovalMatch {
                agent_id: "agt-1",
                category: "fs",
                method: "tool.fs.read",
                task_id: Some("task-999"),
                session_id: None,
                workspace_path: None,
                tenant_id: Some("default"),
                estimated_cost_micros: 0,
                now: unix_now(),
            })
            .unwrap()
        );
    }

    #[test]
    fn method_prefix_scoped_standing_is_not_a_category_wide_bypass() {
        let s = store();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: "agt-1",
            match_category: "browser",
            match_path_glob: None,
            scope_kind: Some("method_prefix"),
            task_id: None,
            session_id: None,
            method_prefix: Some("tool.web_read"),
            workspace_path_glob: None,
            expires_at: unix_now() + 60,
            granted_by: "alice",
            max_calls: None,
            max_cost_micros: None,
            note: "read-only browsing",
            tenant_id: "default",
        })
        .unwrap();

        assert!(
            s.has_active_standing_for(StandingApprovalMatch {
                agent_id: "agt-1",
                category: "browser",
                method: "tool.web_read",
                task_id: None,
                session_id: None,
                workspace_path: None,
                tenant_id: Some("default"),
                estimated_cost_micros: 0,
                now: unix_now(),
            })
            .unwrap()
        );
        assert!(
            !s.has_active_standing_for(StandingApprovalMatch {
                agent_id: "agt-1",
                category: "browser",
                method: "tool.web_submit_form",
                task_id: None,
                session_id: None,
                workspace_path: None,
                tenant_id: Some("default"),
                estimated_cost_micros: 0,
                now: unix_now(),
            })
            .unwrap()
        );
    }

    #[test]
    fn bounded_standing_approval_is_consumed_until_exhausted() {
        let s = store();
        let id = s
            .create_scoped_standing(StandingApprovalCreate {
                agent_id: "agt-1",
                match_category: "fs",
                match_path_glob: None,
                scope_kind: Some("agent_category"),
                task_id: None,
                session_id: None,
                method_prefix: None,
                workspace_path_glob: None,
                expires_at: unix_now() + 60,
                granted_by: "alice",
                max_calls: Some(2),
                max_cost_micros: None,
                note: "two calls",
                tenant_id: "default",
            })
            .unwrap();
        let input = || StandingApprovalMatch {
            agent_id: "agt-1",
            category: "fs",
            method: "tool.fs.read",
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: Some("default"),
            estimated_cost_micros: 0,
            now: unix_now(),
        };

        assert_eq!(
            s.consume_active_standing_for(input()).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            s.consume_active_standing_for(input()).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(s.consume_active_standing_for(input()).unwrap(), None);
        assert!(!s.has_active_standing_for(input()).unwrap());

        let rows = s.list_standing("agt-1").unwrap();
        assert_eq!(rows[0].max_calls, Some(2));
        assert_eq!(rows[0].calls_used, 2);
    }

    #[test]
    fn cost_bounded_standing_approval_is_consumed_until_budget_exhausted() {
        let s = store();
        let id = s
            .create_scoped_standing(StandingApprovalCreate {
                agent_id: "agt-1",
                match_category: "external_api:write",
                match_path_glob: None,
                scope_kind: Some("agent_category"),
                task_id: None,
                session_id: None,
                method_prefix: None,
                workspace_path_glob: None,
                expires_at: unix_now() + 60,
                granted_by: "alice",
                max_calls: None,
                max_cost_micros: Some(20_000),
                note: "two paid calls",
                tenant_id: "default",
            })
            .unwrap();
        let input = || StandingApprovalMatch {
            agent_id: "agt-1",
            category: "external_api:write",
            method: "tool.web_post",
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: Some("default"),
            estimated_cost_micros: 10_000,
            now: unix_now(),
        };

        assert_eq!(
            s.consume_active_standing_for(input()).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            s.consume_active_standing_for(input()).unwrap().as_deref(),
            Some(id.as_str())
        );
        assert_eq!(s.consume_active_standing_for(input()).unwrap(), None);
        assert!(!s.has_active_standing_for(input()).unwrap());

        let rows = s.list_standing("agt-1").unwrap();
        assert_eq!(rows[0].max_cost_micros, Some(20_000));
        assert_eq!(rows[0].cost_used_micros, 20_000);
        assert_eq!(rows[0].calls_used, 0);
    }
}
