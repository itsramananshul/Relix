//! Durable product-spine activity ledger.
//!
//! This is the bridge-level operator view of "what happened?"
//! across otherwise scattered surfaces. It intentionally starts
//! as append-only JSONL so it can be written by bridge handlers
//! without taking a dependency on SQLite or the coordinator task
//! database. The schema is additive-only.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};

use crate::{config::AppState, intervention_audit::InterventionEntry, workspaces::WorkspaceLease};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityEntry {
    pub activity_id: String,
    pub ts_ms: i64,
    pub source: String,
    pub actor: String,
    pub tenant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub action: String,
    pub target: String,
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<i64>,
    pub detail: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ActivityQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActivityRecentResponse {
    pub items: Vec<ActivityEntry>,
    pub count: usize,
    pub durable: bool,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

type ErrorReply = (StatusCode, Json<ApiError>);

pub async fn recent(
    State(state): State<AppState>,
    Query(q): Query<ActivityQuery>,
) -> Result<Json<ActivityRecentResponse>, ErrorReply> {
    let Some(path) = activity_path_for_state(&state) else {
        return Ok(Json(ActivityRecentResponse {
            items: Vec::new(),
            count: 0,
            durable: false,
        }));
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let items = read_recent(&path, &q, limit).map_err(internal)?;
    let count = items.len();
    Ok(Json(ActivityRecentResponse {
        items,
        count,
        durable: true,
    }))
}

pub fn append_workspace_activity(
    data_dir: Option<&Path>,
    lease: &WorkspaceLease,
    action: &str,
    decision: &str,
    detail: impl Into<String>,
) -> Result<(), String> {
    let Some(path) = data_dir.map(activity_path_for_data_dir) else {
        return Ok(());
    };
    append_entry(
        &path,
        &ActivityEntry {
            activity_id: new_activity_id(),
            ts_ms: now_ms(),
            source: "workspace".into(),
            actor: lease.owner_agent.clone(),
            tenant_id: lease.tenant_id.clone(),
            task_id: lease.task_id.clone(),
            run_id: lease.run_id.clone(),
            action: action.into(),
            target: lease.lease_id.clone(),
            decision: decision.into(),
            method: None,
            approval_id: None,
            policy_result: None,
            cost_micros: None,
            detail: detail.into(),
        },
    )
}

pub fn append_intervention_activity(
    intervention_path: &Path,
    entry: &InterventionEntry,
) -> Result<(), String> {
    let path = activity_path_from_intervention_path(intervention_path);
    append_entry(&path, &activity_from_intervention(entry))
}

pub fn append_approval_activity(
    data_dir: Option<&Path>,
    tenant_id: &str,
    actor: &str,
    approval_id: &str,
    decision: &str,
    task_id: Option<&str>,
    detail: impl Into<String>,
) -> Result<(), String> {
    let Some(path) = data_dir.map(activity_path_for_data_dir) else {
        return Ok(());
    };
    append_entry(
        &path,
        &ActivityEntry {
            activity_id: new_activity_id(),
            ts_ms: now_ms(),
            source: "approval".into(),
            actor: actor.into(),
            tenant_id: tenant_id.into(),
            task_id: task_id.map(str::to_string),
            run_id: None,
            action: "approval.record_decision".into(),
            target: approval_id.into(),
            decision: decision.into(),
            method: Some("approval.record_decision".into()),
            approval_id: Some(approval_id.into()),
            policy_result: None,
            cost_micros: None,
            detail: detail.into(),
        },
    )
}

pub fn activity_path_from_intervention_path(path: &Path) -> PathBuf {
    path.with_file_name("bridge-activity.jsonl")
}

fn activity_path_for_state(state: &AppState) -> Option<PathBuf> {
    state
        .cfg
        .transport
        .data_dir
        .as_ref()
        .map(|d| activity_path_for_data_dir(d))
}

fn activity_path_for_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bridge-activity.jsonl")
}

fn activity_from_intervention(entry: &InterventionEntry) -> ActivityEntry {
    ActivityEntry {
        activity_id: if entry.correlation_id.is_empty() {
            new_activity_id()
        } else {
            format!("act_{}", entry.correlation_id)
        },
        ts_ms: entry.ts.saturating_mul(1000),
        source: "intervention".into(),
        actor: entry.actor.clone(),
        tenant_id: "default".into(),
        task_id: extract_task_id(&entry.target),
        run_id: None,
        action: entry.action.clone(),
        target: entry.target.clone(),
        decision: entry.outcome.clone(),
        method: None,
        approval_id: None,
        policy_result: None,
        cost_micros: None,
        detail: entry.detail.clone(),
    }
}

fn append_entry(path: &Path, entry: &ActivityEntry) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("create activity dir: {e}"))?;
    }
    let mut line =
        serde_json::to_string(entry).map_err(|e| format!("encode activity entry: {e}"))?;
    line.push('\n');
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("open activity ledger: {e}"))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("write activity ledger: {e}"))
}

fn read_recent(path: &Path, q: &ActivityQuery, limit: usize) -> Result<Vec<ActivityEntry>, String> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read activity ledger: {e}")),
    };
    let tenant = q.tenant_id.as_deref().and_then(non_empty);
    let source = q.source.as_deref().and_then(non_empty);
    let task_id = q.task_id.as_deref().and_then(non_empty);
    let mut entries = Vec::new();
    for line in body.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<ActivityEntry>(line) else {
            continue;
        };
        if tenant.is_some_and(|t| entry.tenant_id != t) {
            continue;
        }
        if source.is_some_and(|s| entry.source != s) {
            continue;
        }
        if task_id.is_some_and(|t| entry.task_id.as_deref() != Some(t)) {
            continue;
        }
        entries.push(entry);
        if entries.len() >= limit {
            break;
        }
    }
    Ok(entries)
}

fn extract_task_id(target: &str) -> Option<String> {
    let value = target.trim();
    if value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(value.to_string())
    } else {
        None
    }
}

fn non_empty(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_activity_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(36);
    s.push_str("act_");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn internal(error: impl Into<String>) -> ErrorReply {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: error.into(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::WorkspaceLeaseStatus;

    fn lease(task_id: Option<&str>) -> WorkspaceLease {
        WorkspaceLease {
            lease_id: "wsl_1".into(),
            tenant_id: "tenant-a".into(),
            workspace_path: "/repo".into(),
            git_branch: Some("codex/x".into()),
            sandbox_id: None,
            task_id: task_id.map(str::to_string),
            run_id: Some("run-1".into()),
            owner_agent: "agt-1".into(),
            provision_command: None,
            teardown_command: None,
            cleanup_status: WorkspaceLeaseStatus::Active,
            failure_reason: None,
            created_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: None,
        }
    }

    #[test]
    fn workspace_activity_appends_and_reads_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        append_workspace_activity(
            Some(tmp.path()),
            &lease(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            "workspace.create",
            "ok",
            "created",
        )
        .unwrap();
        append_workspace_activity(
            Some(tmp.path()),
            &lease(Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")),
            "workspace.release",
            "ok",
            "released",
        )
        .unwrap();

        let items = read_recent(
            &activity_path_for_data_dir(tmp.path()),
            &ActivityQuery::default(),
            10,
        )
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].action, "workspace.release");
        assert_eq!(items[1].action, "workspace.create");
    }

    #[test]
    fn recent_filters_by_tenant_source_and_task() {
        let tmp = tempfile::tempdir().unwrap();
        append_workspace_activity(
            Some(tmp.path()),
            &lease(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            "workspace.create",
            "ok",
            "created",
        )
        .unwrap();
        let q = ActivityQuery {
            limit: None,
            tenant_id: Some("tenant-a".into()),
            source: Some("workspace".into()),
            task_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        };
        let items = read_recent(&activity_path_for_data_dir(tmp.path()), &q, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn intervention_activity_uses_correlation_id_and_task_target() {
        let entry = InterventionEntry {
            seq: 1,
            ts: 7,
            actor: "operator".into(),
            action: "retry".into(),
            target: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            outcome: "ok".into(),
            detail: "accepted".into(),
            correlation_id: "0123456789abcdef".into(),
        };
        let activity = activity_from_intervention(&entry);
        assert_eq!(activity.activity_id, "act_0123456789abcdef");
        assert_eq!(activity.ts_ms, 7000);
        assert_eq!(
            activity.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    #[test]
    fn approval_activity_records_decision_and_approval_id() {
        let tmp = tempfile::tempdir().unwrap();
        append_approval_activity(
            Some(tmp.path()),
            "tenant-a",
            "operator",
            "apr-1",
            "approved",
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "approved from dashboard",
        )
        .unwrap();

        let q = ActivityQuery {
            limit: None,
            tenant_id: Some("tenant-a".into()),
            source: Some("approval".into()),
            task_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        };
        let items = read_recent(&activity_path_for_data_dir(tmp.path()), &q, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].action, "approval.record_decision");
        assert_eq!(items[0].approval_id.as_deref(), Some("apr-1"));
        assert_eq!(items[0].method.as_deref(), Some("approval.record_decision"));
        assert_eq!(items[0].decision, "approved");
    }
}
