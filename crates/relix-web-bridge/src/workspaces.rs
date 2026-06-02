//! Durable execution workspace leases.
//!
//! This is the bridge-facing product object that ties a task/run
//! to a concrete filesystem or sandbox target before the runtime
//! grows full provisioning/teardown execution. Leases are persisted
//! as JSON so a bridge restart does not erase ownership, cleanup
//! state, or failure reasons.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Path as AxumPath, State},
    http::StatusCode,
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

static MEMORY_STORE: OnceLock<Mutex<WorkspaceLeaseStore>> = OnceLock::new();

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceLeaseStatus {
    Active,
    ProvisionFailed,
    Released,
    CleanupFailed,
}

const WORKSPACE_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceLease {
    pub lease_id: String,
    pub tenant_id: String,
    pub workspace_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub owner_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provision_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teardown_command: Option<String>,
    pub cleanup_status: WorkspaceLeaseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub released_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateWorkspaceLeaseRequest {
    pub workspace_path: String,
    pub owner_agent: String,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub sandbox_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub provision_command: Option<String>,
    #[serde(default)]
    pub teardown_command: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReleaseWorkspaceLeaseRequest {
    #[serde(default)]
    pub failure_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceLeaseStore {
    path: Option<PathBuf>,
    leases: BTreeMap<String, WorkspaceLease>,
}

impl WorkspaceLeaseStore {
    pub fn new(path: Option<PathBuf>) -> Result<Self, String> {
        let leases = match path.as_ref() {
            Some(path) if path.exists() => read_leases(path)?,
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create workspace lease dir: {e}"))?;
                }
                BTreeMap::new()
            }
            None => BTreeMap::new(),
        };
        Ok(Self { path, leases })
    }

    pub fn list(&self, tenant_id: &str) -> Vec<WorkspaceLease> {
        self.leases
            .values()
            .filter(|lease| lease.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    pub fn get(&self, tenant_id: &str, lease_id: &str) -> Option<WorkspaceLease> {
        self.leases
            .get(lease_id)
            .filter(|lease| lease.tenant_id == tenant_id)
            .cloned()
    }

    pub fn get_active(&self, tenant_id: &str, lease_id: &str) -> Result<WorkspaceLease, String> {
        let lease = self
            .get(tenant_id, lease_id)
            .ok_or_else(|| format!("workspace lease not found: {lease_id}"))?;
        if lease.cleanup_status != WorkspaceLeaseStatus::Active {
            return Err(format!("workspace lease is not active: {lease_id}"));
        }
        Ok(lease)
    }

    pub fn create(
        &mut self,
        tenant_id: &str,
        req: CreateWorkspaceLeaseRequest,
    ) -> Result<WorkspaceLease, String> {
        let tenant_id = clean_required(tenant_id, "tenant_id")?;
        let workspace_path = clean_required(&req.workspace_path, "workspace_path")?;
        let owner_agent = clean_required(&req.owner_agent, "owner_agent")?;
        let now = now_ms();
        let lease = WorkspaceLease {
            lease_id: new_lease_id(),
            tenant_id,
            workspace_path,
            git_branch: clean_optional(req.git_branch),
            sandbox_id: clean_optional(req.sandbox_id),
            task_id: clean_optional(req.task_id),
            run_id: clean_optional(req.run_id),
            owner_agent,
            provision_command: clean_optional(req.provision_command),
            teardown_command: clean_optional(req.teardown_command),
            cleanup_status: WorkspaceLeaseStatus::Active,
            failure_reason: None,
            created_at_ms: now,
            updated_at_ms: now,
            released_at_ms: None,
        };
        self.leases.insert(lease.lease_id.clone(), lease.clone());
        self.persist()?;
        Ok(lease)
    }

    pub fn release(
        &mut self,
        tenant_id: &str,
        lease_id: &str,
        failure_reason: Option<String>,
    ) -> Result<WorkspaceLease, String> {
        let tenant_id = clean_required(tenant_id, "tenant_id")?;
        let now = now_ms();
        let lease = self
            .leases
            .get_mut(lease_id)
            .filter(|lease| lease.tenant_id == tenant_id)
            .ok_or_else(|| format!("workspace lease not found: {lease_id}"))?;
        lease.cleanup_status = if failure_reason
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            WorkspaceLeaseStatus::CleanupFailed
        } else {
            WorkspaceLeaseStatus::Released
        };
        lease.failure_reason = clean_optional(failure_reason);
        lease.updated_at_ms = now;
        lease.released_at_ms = Some(now);
        let out = lease.clone();
        self.persist()?;
        Ok(out)
    }

    pub fn mark_provision_failed(
        &mut self,
        tenant_id: &str,
        lease_id: &str,
        failure_reason: String,
    ) -> Result<WorkspaceLease, String> {
        let tenant_id = clean_required(tenant_id, "tenant_id")?;
        let now = now_ms();
        let lease = self
            .leases
            .get_mut(lease_id)
            .filter(|lease| lease.tenant_id == tenant_id)
            .ok_or_else(|| format!("workspace lease not found: {lease_id}"))?;
        lease.cleanup_status = WorkspaceLeaseStatus::ProvisionFailed;
        lease.failure_reason = Some(failure_reason);
        lease.updated_at_ms = now;
        let out = lease.clone();
        self.persist()?;
        Ok(out)
    }

    /// Phase 4 — bind the active durable task (and optional run) onto
    /// a lease so the workspace reflects which work is currently using
    /// it. Only mutates an active lease and bumps `updated_at_ms`.
    /// A missing run id explicitly clears any prior run binding so a
    /// reused lease cannot show a new task with a stale old run.
    pub fn bind_active_run(
        &mut self,
        tenant_id: &str,
        lease_id: &str,
        task_id: &str,
        run_id: Option<&str>,
    ) -> Result<WorkspaceLease, String> {
        let tenant_id = clean_required(tenant_id, "tenant_id")?;
        let task_id = clean_required(task_id, "task_id")?;
        let now = now_ms();
        let lease = self
            .leases
            .get_mut(lease_id)
            .filter(|lease| lease.tenant_id == tenant_id)
            .ok_or_else(|| format!("workspace lease not found: {lease_id}"))?;
        if lease.cleanup_status != WorkspaceLeaseStatus::Active {
            return Err(format!("workspace lease is not active: {lease_id}"));
        }
        lease.task_id = Some(task_id);
        lease.run_id = run_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        lease.updated_at_ms = now;
        let out = lease.clone();
        self.persist()?;
        Ok(out)
    }

    fn persist(&self) -> Result<(), String> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create lease dir: {e}"))?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&self.leases)
            .map_err(|e| format!("encode workspace leases: {e}"))?;
        std::fs::write(&tmp, body).map_err(|e| format!("write workspace lease temp: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("replace workspace lease file: {e}"))?;
        Ok(())
    }
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<WorkspaceLease>>, ErrorReply> {
    let tenant_id = tenant_id();
    with_store(&state, |store| Ok(store.list(&tenant_id)))
        .map(Json)
        .map_err(internal)
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateWorkspaceLeaseRequest>,
) -> Result<Json<WorkspaceLease>, ErrorReply> {
    let tenant_id = tenant_id();
    let lease = with_store(&state, |store| store.create(&tenant_id, req)).map_err(bad)?;
    if let Err(e) = crate::activity::append_workspace_activity(
        state.cfg.transport.data_dir.as_deref(),
        &lease,
        "workspace.create",
        "ok",
        "workspace lease created",
    ) {
        tracing::warn!(error = %e, lease_id = %lease.lease_id, "activity ledger: workspace create append failed");
    }
    if let Some(command) = lease.provision_command.clone() {
        match run_workspace_command(&lease, &command) {
            Ok(detail) => {
                if let Err(e) = crate::activity::append_workspace_activity(
                    state.cfg.transport.data_dir.as_deref(),
                    &lease,
                    "workspace.provision",
                    "ok",
                    detail,
                ) {
                    tracing::warn!(error = %e, lease_id = %lease.lease_id, "activity ledger: workspace provision append failed");
                }
            }
            Err(e) => {
                let failed = with_store(&state, |store| {
                    store.mark_provision_failed(&tenant_id, &lease.lease_id, e.clone())
                })
                .map_err(internal)?;
                if let Err(activity_err) = crate::activity::append_workspace_activity(
                    state.cfg.transport.data_dir.as_deref(),
                    &failed,
                    "workspace.provision",
                    "failed",
                    e.clone(),
                ) {
                    tracing::warn!(error = %activity_err, lease_id = %failed.lease_id, "activity ledger: workspace provision failure append failed");
                }
                return Err(bad(format!("workspace provision failed: {e}")));
            }
        }
    }
    Ok(Json(lease))
}

pub async fn get(
    State(state): State<AppState>,
    AxumPath(lease_id): AxumPath<String>,
) -> Result<Json<WorkspaceLease>, ErrorReply> {
    let tenant_id = tenant_id();
    with_store(&state, |store| {
        store
            .get(&tenant_id, &lease_id)
            .ok_or_else(|| format!("workspace lease not found: {lease_id}"))
    })
    .map(Json)
    .map_err(not_found)
}

/// Resolve a workspace lease for the tenant currently attached to
/// the request scope. Execution paths use this instead of trusting a
/// caller-supplied workspace path; workspace-scoped approvals must be
/// tied to a durable lease the tenant actually owns.
pub fn resolve_active_lease_for_current_tenant(
    state: &AppState,
    lease_id: &str,
) -> Result<WorkspaceLease, String> {
    let lease_id = clean_required(lease_id, "workspace_lease_id")?;
    let tenant_id = tenant_id();
    with_store(state, |store| store.get_active(&tenant_id, &lease_id))
}

/// Phase 4 — bind a created task (and optional run) onto a workspace
/// lease the current tenant owns, recording a durable activity row.
///
/// Execution paths call this AFTER `task.create` so the workspace's
/// "active run" reflects the work currently using it. The lease is
/// resolved against the verified per-request tenant, so a caller can
/// only bind tasks onto their own leases.
pub fn bind_active_run_for_current_tenant(
    state: &AppState,
    lease_id: &str,
    task_id: &str,
    run_id: Option<&str>,
) -> Result<WorkspaceLease, String> {
    let lease_id = clean_required(lease_id, "workspace_lease_id")?;
    let tenant_id = tenant_id();
    let lease = with_store(state, |store| {
        store.bind_active_run(&tenant_id, &lease_id, task_id, run_id)
    })?;
    if let Err(e) = crate::activity::append_workspace_activity(
        state.cfg.transport.data_dir.as_deref(),
        &lease,
        "workspace.bind_run",
        "ok",
        format!("task_id={task_id}"),
    ) {
        tracing::warn!(error = %e, lease_id = %lease.lease_id, "activity ledger: workspace bind_run append failed");
    }
    Ok(lease)
}

pub async fn release(
    State(state): State<AppState>,
    AxumPath(lease_id): AxumPath<String>,
    Json(req): Json<ReleaseWorkspaceLeaseRequest>,
) -> Result<Json<WorkspaceLease>, ErrorReply> {
    let tenant_id = tenant_id();
    let active =
        with_store(&state, |store| store.get_active(&tenant_id, &lease_id)).map_err(|e| {
            if e.contains("not found") {
                not_found(e)
            } else {
                bad(e)
            }
        })?;
    let mut failure_reason = req.failure_reason;
    if let Some(command) = active.teardown_command.clone() {
        match run_workspace_command(&active, &command) {
            Ok(detail) => {
                if let Err(e) = crate::activity::append_workspace_activity(
                    state.cfg.transport.data_dir.as_deref(),
                    &active,
                    "workspace.teardown",
                    "ok",
                    detail,
                ) {
                    tracing::warn!(error = %e, lease_id = %active.lease_id, "activity ledger: workspace teardown append failed");
                }
            }
            Err(e) => {
                if let Err(activity_err) = crate::activity::append_workspace_activity(
                    state.cfg.transport.data_dir.as_deref(),
                    &active,
                    "workspace.teardown",
                    "failed",
                    e.clone(),
                ) {
                    tracing::warn!(error = %activity_err, lease_id = %active.lease_id, "activity ledger: workspace teardown failure append failed");
                }
                failure_reason = Some(match failure_reason {
                    Some(existing) if !existing.trim().is_empty() => {
                        format!("{}; teardown command failed: {e}", existing.trim())
                    }
                    _ => format!("teardown command failed: {e}"),
                });
            }
        }
    }
    let lease = with_store(&state, |store| {
        store.release(&tenant_id, &lease_id, failure_reason)
    })
    .map_err(|e| {
        if e.contains("not found") {
            not_found(e)
        } else {
            bad(e)
        }
    })?;
    let decision = match lease.cleanup_status {
        WorkspaceLeaseStatus::ProvisionFailed => "provision_failed",
        WorkspaceLeaseStatus::CleanupFailed => "cleanup_failed",
        WorkspaceLeaseStatus::Released => "ok",
        WorkspaceLeaseStatus::Active => "active",
    };
    if let Err(e) = crate::activity::append_workspace_activity(
        state.cfg.transport.data_dir.as_deref(),
        &lease,
        "workspace.release",
        decision,
        lease
            .failure_reason
            .clone()
            .unwrap_or_else(|| "workspace lease released".into()),
    ) {
        tracing::warn!(error = %e, lease_id = %lease.lease_id, "activity ledger: workspace release append failed");
    }
    Ok(Json(lease))
}

fn with_store<T>(
    state: &AppState,
    f: impl FnOnce(&mut WorkspaceLeaseStore) -> Result<T, String>,
) -> Result<T, String> {
    if let Some(data_dir) = state.cfg.transport.data_dir.as_ref() {
        let mut store = WorkspaceLeaseStore::new(Some(data_dir.join("bridge-workspaces.json")))?;
        return f(&mut store);
    }
    let store = MEMORY_STORE.get_or_init(|| {
        Mutex::new(WorkspaceLeaseStore::new(None).expect("in-memory workspace store"))
    });
    let mut guard = store
        .lock()
        .map_err(|_| "workspace lease store lock poisoned".to_string())?;
    f(&mut guard)
}

type ErrorReply = (StatusCode, Json<ApiError>);

fn bad(error: impl Into<String>) -> ErrorReply {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: error.into(),
        }),
    )
}

fn not_found(error: impl Into<String>) -> ErrorReply {
    (
        StatusCode::NOT_FOUND,
        Json(ApiError {
            error: error.into(),
        }),
    )
}

fn internal(error: impl Into<String>) -> ErrorReply {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: error.into(),
        }),
    )
}

fn read_leases(path: &Path) -> Result<BTreeMap<String, WorkspaceLease>, String> {
    let body = std::fs::read(path).map_err(|e| format!("read workspace leases: {e}"))?;
    if body.is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_slice(&body).map_err(|e| format!("decode workspace leases: {e}"))
}

fn clean_required(value: &str, name: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(format!("{name} required"))
    } else {
        Ok(trimmed.to_string())
    }
}

fn clean_optional(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn run_workspace_command(lease: &WorkspaceLease, command: &str) -> Result<String, String> {
    let command = command.trim();
    if command.is_empty() {
        return Ok("workspace command empty; skipped".into());
    }
    let cwd = command_cwd(&lease.workspace_path)?;
    let mut cmd = shell_command(command);
    cmd.current_dir(&cwd)
        .env("RELIX_WORKSPACE_PATH", &lease.workspace_path)
        .env("RELIX_WORKSPACE_LEASE_ID", &lease.lease_id)
        .env("RELIX_TENANT_ID", &lease.tenant_id)
        .env("RELIX_OWNER_AGENT", &lease.owner_agent)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn workspace command in {}: {e}", cwd.display()))?;
    let deadline = Instant::now() + WORKSPACE_COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "command timed out after {}s",
                    WORKSPACE_COMMAND_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => return Err(format!("wait workspace command: {e}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("collect workspace command output: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = truncate_detail(format!(
        "exit={}; cwd={}; stdout={}; stderr={}",
        output.status.code().unwrap_or(-1),
        cwd.display(),
        stdout.trim(),
        stderr.trim()
    ));
    if output.status.success() {
        Ok(detail)
    } else {
        Err(detail)
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    }
}

fn command_cwd(workspace_path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(workspace_path);
    if path.is_dir() {
        return Ok(path);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("create workspace parent: {e}"))?;
        return Ok(parent.to_path_buf());
    }
    std::env::current_dir().map_err(|e| format!("resolve current dir: {e}"))
}

fn truncate_detail(mut value: String) -> String {
    const MAX: usize = 2048;
    if value.len() > MAX {
        value.truncate(MAX);
        value.push_str("...");
    }
    value
}

fn tenant_id() -> String {
    crate::tenant::current_tenant_or_none().unwrap_or_else(|| "default".into())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_lease_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(36);
    s.push_str("wsl_");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_get_and_release_workspace_lease() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let lease = store
            .create(
                "tenant-a",
                CreateWorkspaceLeaseRequest {
                    workspace_path: "D:/work/repo".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: Some("codex/test".into()),
                    sandbox_id: None,
                    task_id: Some("task-1".into()),
                    run_id: Some("run-1".into()),
                    provision_command: None,
                    teardown_command: Some("git worktree remove".into()),
                },
            )
            .unwrap();
        assert_eq!(lease.cleanup_status, WorkspaceLeaseStatus::Active);
        assert_eq!(
            store
                .get("tenant-a", &lease.lease_id)
                .unwrap()
                .task_id
                .as_deref(),
            Some("task-1")
        );
        assert!(store.get("tenant-b", &lease.lease_id).is_none());

        let released = store
            .release("tenant-a", &lease.lease_id, None)
            .expect("release");
        assert_eq!(released.cleanup_status, WorkspaceLeaseStatus::Released);
        assert!(released.released_at_ms.is_some());
    }

    #[test]
    fn bind_active_run_stamps_task_onto_active_lease() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let lease = store
            .create(
                "tenant-a",
                CreateWorkspaceLeaseRequest {
                    workspace_path: "/tmp/repo".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: None,
                    sandbox_id: None,
                    task_id: None,
                    run_id: None,
                    provision_command: None,
                    teardown_command: None,
                },
            )
            .unwrap();
        assert!(lease.task_id.is_none());

        let bound = store
            .bind_active_run("tenant-a", &lease.lease_id, "task-9", Some("run-9"))
            .expect("bind");
        assert_eq!(bound.task_id.as_deref(), Some("task-9"));
        assert_eq!(bound.run_id.as_deref(), Some("run-9"));
        assert!(bound.updated_at_ms >= lease.updated_at_ms);

        // Rebinding without a run clears the old run id instead of
        // leaving a misleading task=new/run=old pair on the lease.
        let rebound = store
            .bind_active_run("tenant-a", &lease.lease_id, "task-10", None)
            .expect("rebind");
        assert_eq!(rebound.task_id.as_deref(), Some("task-10"));
        assert!(rebound.run_id.is_none());

        // Cross-tenant bind is rejected: the lease belongs to tenant-a.
        assert!(
            store
                .bind_active_run("tenant-b", &lease.lease_id, "task-x", None)
                .is_err()
        );

        // Released leases can no longer be bound.
        store.release("tenant-a", &lease.lease_id, None).unwrap();
        assert!(
            store
                .bind_active_run("tenant-a", &lease.lease_id, "task-11", None)
                .is_err()
        );
    }

    #[test]
    fn release_with_failure_reason_marks_cleanup_failed() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let lease = store
            .create(
                "default",
                CreateWorkspaceLeaseRequest {
                    workspace_path: "/tmp/repo".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: None,
                    sandbox_id: None,
                    task_id: None,
                    run_id: None,
                    provision_command: None,
                    teardown_command: None,
                },
            )
            .unwrap();
        let failed = store
            .release("default", &lease.lease_id, Some("teardown exited 1".into()))
            .unwrap();
        assert_eq!(failed.cleanup_status, WorkspaceLeaseStatus::CleanupFailed);
        assert_eq!(failed.failure_reason.as_deref(), Some("teardown exited 1"));
    }

    #[test]
    fn mark_provision_failed_makes_lease_inactive() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let lease = store
            .create(
                "default",
                CreateWorkspaceLeaseRequest {
                    workspace_path: "/tmp/repo".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: None,
                    sandbox_id: None,
                    task_id: None,
                    run_id: None,
                    provision_command: None,
                    teardown_command: None,
                },
            )
            .unwrap();
        let failed = store
            .mark_provision_failed("default", &lease.lease_id, "provision exited 7".into())
            .unwrap();
        assert_eq!(failed.cleanup_status, WorkspaceLeaseStatus::ProvisionFailed);
        assert_eq!(failed.failure_reason.as_deref(), Some("provision exited 7"));
        assert!(store.get_active("default", &lease.lease_id).is_err());
    }

    #[test]
    fn workspace_command_reports_success_and_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let lease = WorkspaceLease {
            lease_id: "wsl_test".into(),
            tenant_id: "default".into(),
            workspace_path: tmp.path().join("repo").display().to_string(),
            git_branch: None,
            sandbox_id: None,
            task_id: None,
            run_id: None,
            owner_agent: "agt-1".into(),
            provision_command: None,
            teardown_command: None,
            cleanup_status: WorkspaceLeaseStatus::Active,
            failure_reason: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: None,
        };

        let ok = run_workspace_command(&lease, "echo relix-workspace").unwrap();
        assert!(ok.contains("exit=0"));
        assert!(ok.contains("relix-workspace"));

        let err = run_workspace_command(&lease, "exit 7").unwrap_err();
        assert!(err.contains("exit=7"));
    }

    #[test]
    fn create_rejects_missing_owner_and_path() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let err = store
            .create(
                "default",
                CreateWorkspaceLeaseRequest {
                    workspace_path: " ".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: None,
                    sandbox_id: None,
                    task_id: None,
                    run_id: None,
                    provision_command: None,
                    teardown_command: None,
                },
            )
            .unwrap_err();
        assert_eq!(err, "workspace_path required");
    }

    #[test]
    fn inactive_lease_is_not_execution_bindable() {
        let mut store = WorkspaceLeaseStore::new(None).unwrap();
        let lease = store
            .create(
                "default",
                CreateWorkspaceLeaseRequest {
                    workspace_path: "/tmp/repo".into(),
                    owner_agent: "agt-1".into(),
                    git_branch: None,
                    sandbox_id: None,
                    task_id: None,
                    run_id: None,
                    provision_command: None,
                    teardown_command: None,
                },
            )
            .unwrap();
        let released = store.release("default", &lease.lease_id, None).unwrap();
        assert_eq!(released.cleanup_status, WorkspaceLeaseStatus::Released);
        let err = store.get_active("default", &lease.lease_id).unwrap_err();
        assert_eq!(
            err,
            format!("workspace lease is not active: {}", lease.lease_id)
        );
    }
}
