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
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
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
    Released,
    CleanupFailed,
}

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
    with_store(&state, |store| store.create(&tenant_id, req))
        .map(Json)
        .map_err(bad)
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

pub async fn release(
    State(state): State<AppState>,
    AxumPath(lease_id): AxumPath<String>,
    Json(req): Json<ReleaseWorkspaceLeaseRequest>,
) -> Result<Json<WorkspaceLease>, ErrorReply> {
    let tenant_id = tenant_id();
    with_store(&state, |store| {
        store.release(&tenant_id, &lease_id, req.failure_reason)
    })
    .map(Json)
    .map_err(|e| {
        if e.contains("not found") {
            not_found(e)
        } else {
            bad(e)
        }
    })
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
}
