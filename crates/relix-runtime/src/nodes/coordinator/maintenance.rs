//! Operator maintenance: a bounded storage summary over the run-workspace
//! root + a SAFE prune of old run workspaces (and, optionally, the verbose
//! log rows of pruned runs).
//!
//! Safety contract (this is destructive code, so the rules are explicit):
//!   * Never follows symlinks (sizing or deleting).
//!   * Only ever operates on DIRECT children of the configured workspace
//!     root, each named like a generated `run_<uuid>` (validated).
//!   * Refuses a shallow / filesystem-root workspace root.
//!   * Never deletes a workspace whose run is still `running`.
//!   * Bounded: caps the directory count + total files walked so a huge or
//!     pathological tree can't stall the operator console.
//!   * Dry-run is the default at the API layer; a real delete is explicit.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

/// Cap on workspace directories listed under the root (defensive bound).
const MAX_WORKSPACES: usize = 5000;
/// Global file budget for sizing all workspaces in one scan.
const MAX_FILES_SCANNED: u64 = 200_000;

/// Default age threshold (days): a workspace newer than this is "recent"
/// and kept.
pub const DEFAULT_PRUNE_OLDER_THAN_DAYS: u64 = 7;
/// Default count of newest workspaces always kept regardless of age.
pub const DEFAULT_PRUNE_KEEP_LATEST: usize = 10;

/// One run workspace directory under the root.
#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceEntry {
    pub run_id: String,
    pub bytes: u64,
    pub files: u64,
    /// Last-modified time (unix secs) of the workspace directory.
    pub modified: i64,
}

/// Aggregate view of every run workspace under the root.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WorkspaceScan {
    pub root: String,
    /// False when the root directory does not exist yet (graceful).
    pub exists: bool,
    pub count: usize,
    pub total_bytes: u64,
    pub oldest: Option<i64>,
    pub newest: Option<i64>,
    /// True if the scan hit a bound (dir / file cap) — figures are a floor.
    pub truncated: bool,
    /// Per-directory detail — used by prune; not serialized into the
    /// summary (keeps the payload small).
    #[serde(skip)]
    pub entries: Vec<WorkspaceEntry>,
}

/// Sum the regular-file bytes + count under `dir`, skipping symlinks and
/// stopping when the shared `budget` is exhausted.
fn dir_size_bounded(dir: &Path, budget: &mut u64) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for ent in rd.flatten() {
            if *budget == 0 {
                return (bytes, files);
            }
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(ent.path());
            } else if ft.is_file() {
                bytes = bytes.saturating_add(ent.metadata().map(|m| m.len()).unwrap_or(0));
                files += 1;
                *budget -= 1;
            }
        }
    }
    (bytes, files)
}

fn dir_modified_secs(ent: &std::fs::DirEntry) -> i64 {
    ent.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// List + size every run workspace directly under `root`. Bounded, skips
/// symlinked entries, and treats a missing root as an empty scan.
pub fn scan_run_workspaces(root: &Path) -> WorkspaceScan {
    let mut scan = WorkspaceScan {
        root: root.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let Ok(rd) = std::fs::read_dir(root) else {
        return scan; // missing / unreadable root → exists=false, empty
    };
    scan.exists = true;
    let mut budget = MAX_FILES_SCANNED;
    for ent in rd.flatten() {
        if scan.entries.len() >= MAX_WORKSPACES {
            scan.truncated = true;
            break;
        }
        let Ok(ft) = ent.file_type() else { continue };
        // Only real directories are run workspaces — never a symlink.
        if !ft.is_dir() || ft.is_symlink() {
            continue;
        }
        let run_id = ent.file_name().to_string_lossy().into_owned();
        let modified = dir_modified_secs(&ent);
        let (bytes, files) = dir_size_bounded(&ent.path(), &mut budget);
        scan.total_bytes = scan.total_bytes.saturating_add(bytes);
        scan.oldest = Some(scan.oldest.map_or(modified, |o| o.min(modified)));
        scan.newest = Some(scan.newest.map_or(modified, |n| n.max(modified)));
        scan.entries.push(WorkspaceEntry {
            run_id,
            bytes,
            files,
            modified,
        });
    }
    if budget == 0 {
        scan.truncated = true;
    }
    scan.count = scan.entries.len();
    scan
}

/// What pruning is allowed to remove this run.
#[derive(Clone, Debug)]
pub struct PrunePolicy {
    pub older_than_days: u64,
    pub keep_latest: usize,
    pub delete_workspaces: bool,
}

/// One workspace selected for deletion (would-delete under `dry_run`).
#[derive(Clone, Debug, Serialize)]
pub struct PruneItem {
    pub run_id: String,
    pub bytes: u64,
    pub modified: i64,
    pub age_days: i64,
}

/// The result of a prune pass — exactly what would be / was removed.
#[derive(Clone, Debug, Serialize)]
pub struct PruneReport {
    pub root: String,
    pub dry_run: bool,
    pub older_than_days: u64,
    pub keep_latest: usize,
    /// Workspaces selected for deletion.
    pub to_delete: Vec<PruneItem>,
    pub to_delete_bytes: u64,
    /// Workspaces kept because their run is still running.
    pub kept_running: usize,
    /// Workspaces kept as the newest `keep_latest`.
    pub kept_latest: usize,
    /// Workspaces kept because they are newer than the age cutoff.
    pub kept_recent: usize,
    pub deleted_workspaces: usize,
    pub deleted_bytes: u64,
    pub errors: Vec<String>,
}

/// A workspace root is safe to prune under iff it is a real directory with a
/// grandparent — so we can NEVER operate on a filesystem/drive root or a
/// top-level directory. Returns the canonical root.
pub fn validate_prune_root(root: &Path) -> Result<std::path::PathBuf, String> {
    if !root.is_dir() {
        return Err(format!(
            "workspace root is not a directory: {}",
            root.display()
        ));
    }
    let abs = std::fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve workspace root {}: {e}", root.display()))?;
    if abs.parent().and_then(|p| p.parent()).is_none() {
        return Err(format!(
            "refusing to prune a shallow / filesystem-root path: {}",
            abs.display()
        ));
    }
    Ok(abs)
}

/// Compute (and, unless `dry_run`, perform) a prune of old run workspaces.
/// `scan` is the listing from [`scan_run_workspaces`]; `running` is the set
/// of run_ids whose runs are still in flight (never deleted).
pub fn prune_run_workspaces(
    root: &Path,
    now_secs: i64,
    scan: &WorkspaceScan,
    running: &HashSet<String>,
    policy: &PrunePolicy,
    dry_run: bool,
) -> Result<PruneReport, String> {
    let root_canon = validate_prune_root(root)?;
    let mut report = PruneReport {
        root: root_canon.to_string_lossy().into_owned(),
        dry_run,
        older_than_days: policy.older_than_days,
        keep_latest: policy.keep_latest,
        to_delete: Vec::new(),
        to_delete_bytes: 0,
        kept_running: 0,
        kept_latest: 0,
        kept_recent: 0,
        deleted_workspaces: 0,
        deleted_bytes: 0,
        errors: Vec::new(),
    };
    // Newest-first so `keep_latest` protects the most recently used N.
    let mut entries = scan.entries.clone();
    entries.sort_by_key(|e| std::cmp::Reverse(e.modified));
    let cutoff = now_secs - (policy.older_than_days as i64).max(0) * 86_400;
    for (i, e) in entries.iter().enumerate() {
        if running.contains(&e.run_id) {
            report.kept_running += 1;
            continue;
        }
        if i < policy.keep_latest {
            report.kept_latest += 1;
            continue;
        }
        if e.modified > cutoff {
            report.kept_recent += 1;
            continue;
        }
        let age_days = (now_secs - e.modified).max(0) / 86_400;
        report.to_delete_bytes = report.to_delete_bytes.saturating_add(e.bytes);
        report.to_delete.push(PruneItem {
            run_id: e.run_id.clone(),
            bytes: e.bytes,
            modified: e.modified,
            age_days,
        });
    }
    if !dry_run && policy.delete_workspaces {
        for item in &report.to_delete {
            match delete_one_workspace(&root_canon, &item.run_id) {
                Ok(()) => {
                    report.deleted_workspaces += 1;
                    report.deleted_bytes = report.deleted_bytes.saturating_add(item.bytes);
                }
                Err(e) => report.errors.push(format!("{}: {e}", item.run_id)),
            }
        }
    }
    Ok(report)
}

/// Remove ONE workspace directory, with defense-in-depth: the run_id must be
/// a safe single segment, the target a DIRECT child of the (canonical) root,
/// a real directory, and not a symlink.
fn delete_one_workspace(root_canon: &Path, run_id: &str) -> Result<(), String> {
    if !crate::nodes::coordinator::heartbeat::run_id_is_safe(run_id) {
        return Err("unsafe run id".to_string());
    }
    let target = root_canon.join(run_id);
    if target.parent() != Some(root_canon) {
        return Err("not a direct child of the workspace root".to_string());
    }
    let md = std::fs::symlink_metadata(&target).map_err(|e| format!("stat: {e}"))?;
    if md.file_type().is_symlink() {
        return Err("refusing to delete a symlink".to_string());
    }
    if !md.is_dir() {
        return Err("not a directory".to_string());
    }
    std::fs::remove_dir_all(&target).map_err(|e| format!("remove: {e}"))
}

// ── Scheduled / autonomous cleanup ──────────────────────────────────────

/// Default scheduled-prune interval — once a day.
pub const DEFAULT_AUTOPRUNE_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Operator-controlled scheduled cleanup config, resolved from env.
/// DISABLED by default; even when enabled, DRY-RUN by default so an
/// operator must explicitly opt into a real delete.
#[derive(Clone, Debug, Serialize)]
pub struct AutopruneConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub older_than_days: u64,
    pub keep_latest: usize,
    pub delete_workspaces: bool,
    pub delete_events: bool,
    pub delete_artifacts: bool,
    pub dry_run: bool,
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => default,
    }
}
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Resolve the scheduled-cleanup config from `RELIX_MAINTENANCE_AUTOPRUNE_*`.
pub fn resolve_autoprune_config() -> AutopruneConfig {
    AutopruneConfig {
        enabled: env_bool("RELIX_MAINTENANCE_AUTOPRUNE_ENABLED", false),
        interval_secs: env_u64(
            "RELIX_MAINTENANCE_AUTOPRUNE_INTERVAL_SECS",
            DEFAULT_AUTOPRUNE_INTERVAL_SECS,
        )
        .max(60),
        older_than_days: env_u64(
            "RELIX_MAINTENANCE_AUTOPRUNE_OLDER_THAN_DAYS",
            DEFAULT_PRUNE_OLDER_THAN_DAYS,
        ),
        keep_latest: env_u64(
            "RELIX_MAINTENANCE_AUTOPRUNE_KEEP_LATEST",
            DEFAULT_PRUNE_KEEP_LATEST as u64,
        ) as usize,
        delete_workspaces: env_bool("RELIX_MAINTENANCE_AUTOPRUNE_DELETE_WORKSPACES", true),
        delete_events: env_bool("RELIX_MAINTENANCE_AUTOPRUNE_DELETE_EVENTS", false),
        delete_artifacts: env_bool("RELIX_MAINTENANCE_AUTOPRUNE_DELETE_ARTIFACTS", false),
        // Conservative: even when enabled, default to a dry-run.
        dry_run: env_bool("RELIX_MAINTENANCE_AUTOPRUNE_DRY_RUN", true),
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The full outcome of a prune executed through the store (the report plus
/// any log-row deletions, the durable status, and the audit row id).
#[derive(Clone, Debug)]
pub struct PruneOutcome {
    pub report: PruneReport,
    pub events_deleted: usize,
    pub artifacts_deleted: usize,
    pub status: &'static str,
    pub audit_id: i64,
}

/// Run a prune against the store's configured workspace root AND record a
/// durable audit row — for EVERY attempt, including dry-runs, refusals, and
/// failures. This is the single path both the manual API (`trigger="manual"`)
/// and scheduled cleanup (`trigger="scheduled"`) go through, so the audit is
/// always complete. The audit payload carries no secrets and is bounded.
#[allow(clippy::too_many_arguments)]
pub fn execute_prune(
    store: &super::TaskStore,
    trigger: &str,
    older_than_days: u64,
    keep_latest: usize,
    delete_workspaces: bool,
    delete_events: bool,
    delete_artifacts: bool,
    dry_run: bool,
) -> Result<PruneOutcome, String> {
    let root = store.run_workspace_root().to_path_buf();
    let root_str = root.to_string_lossy().into_owned();
    let now = now_secs();
    let scan = scan_run_workspaces(&root);
    let running = store.running_run_ids().unwrap_or_default();
    let policy = PrunePolicy {
        older_than_days,
        keep_latest,
        delete_workspaces,
    };
    let report = match prune_run_workspaces(&root, now, &scan, &running, &policy, dry_run) {
        Ok(r) => r,
        Err(e) => {
            // A refusal is still audited (status=refused).
            let _ = store.record_maintenance_audit(
                "prune",
                trigger,
                dry_run,
                Some(&root_str),
                0,
                0,
                0,
                0,
                "refused",
                Some(&e),
                None,
            );
            return Err(e);
        }
    };
    let (events_deleted, artifacts_deleted) = if !dry_run && (delete_events || delete_artifacts) {
        let ids: Vec<String> = report.to_delete.iter().map(|i| i.run_id.clone()).collect();
        store
            .prune_run_logs(&ids, delete_events, delete_artifacts)
            .unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let status = if report.errors.is_empty() {
        "ok"
    } else {
        "failed"
    };
    let note = if dry_run {
        format!(
            "dry-run: {} candidate(s), {} bytes",
            report.to_delete.len(),
            report.to_delete_bytes
        )
    } else if report.errors.is_empty() {
        format!(
            "deleted {} workspace(s), {} bytes",
            report.deleted_workspaces, report.deleted_bytes
        )
    } else {
        format!(
            "deleted {} of {}, {} error(s)",
            report.deleted_workspaces,
            report.to_delete.len(),
            report.errors.len()
        )
    };
    // Compact, secret-free payload (a sample of the run_ids + the keep tallies).
    let sample: Vec<&str> = report
        .to_delete
        .iter()
        .take(50)
        .map(|i| i.run_id.as_str())
        .collect();
    let payload = serde_json::json!({
        "older_than_days": older_than_days,
        "keep_latest": keep_latest,
        "delete_workspaces": delete_workspaces,
        "delete_events": delete_events,
        "delete_artifacts": delete_artifacts,
        "to_delete_total": report.to_delete.len(),
        "to_delete_sample": sample,
        "kept_running": report.kept_running,
        "kept_latest": report.kept_latest,
        "kept_recent": report.kept_recent,
    })
    .to_string();
    let audit_id = store
        .record_maintenance_audit(
            "prune",
            trigger,
            dry_run,
            Some(&report.root),
            report.deleted_workspaces as i64,
            report.deleted_bytes as i64,
            events_deleted as i64,
            artifacts_deleted as i64,
            status,
            Some(&note),
            Some(&payload),
        )
        .unwrap_or(0);
    Ok(PruneOutcome {
        report,
        events_deleted,
        artifacts_deleted,
        status,
        audit_id,
    })
}

/// One scheduled-cleanup tick: if autoprune is enabled, run a prune via
/// [`execute_prune`] (trigger `scheduled`) using the env-resolved policy.
/// Returns `Ok(None)` when disabled. Safe to call on a timer.
pub fn autoprune_tick(store: &super::TaskStore) -> Result<Option<PruneOutcome>, String> {
    let cfg = resolve_autoprune_config();
    if !cfg.enabled {
        return Ok(None);
    }
    execute_prune(
        store,
        "scheduled",
        cfg.older_than_days,
        cfg.keep_latest,
        cfg.delete_workspaces,
        cfg.delete_events,
        cfg.delete_artifacts,
        cfg.dry_run,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;

    fn mkfile(p: &Path, bytes: usize) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, vec![b'x'; bytes]).unwrap();
    }
    fn entry(run_id: &str, modified: i64) -> WorkspaceEntry {
        WorkspaceEntry {
            run_id: run_id.to_string(),
            bytes: 10,
            files: 1,
            modified,
        }
    }
    // Build a scan over `entries` (age decoupled from real disk mtimes, so
    // the prune logic is deterministic) but the dirs are still on disk so a
    // real delete actually removes them.
    fn scan_of(root: &Path, entries: Vec<WorkspaceEntry>) -> WorkspaceScan {
        WorkspaceScan {
            root: root.to_string_lossy().into_owned(),
            exists: true,
            count: entries.len(),
            total_bytes: entries.iter().map(|e| e.bytes).sum(),
            oldest: entries.iter().map(|e| e.modified).min(),
            newest: entries.iter().map(|e| e.modified).max(),
            truncated: false,
            entries,
        }
    }

    #[test]
    fn scan_missing_root_is_graceful() {
        let tmp = tempfile::tempdir().unwrap();
        let s = scan_run_workspaces(&tmp.path().join("nope"));
        assert!(!s.exists);
        assert_eq!(s.count, 0);
        assert_eq!(s.total_bytes, 0);
    }

    #[test]
    fn scan_counts_dirs_and_bytes_and_skips_loose_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        mkfile(&root.join("run_a").join("BRIEF.md"), 100);
        mkfile(&root.join("run_a").join("out.txt"), 50);
        mkfile(&root.join("run_b").join("note.txt"), 25);
        mkfile(&root.join("loose.txt"), 999); // a stray root file is NOT a workspace
        let s = scan_run_workspaces(root);
        assert!(s.exists);
        assert_eq!(s.count, 2, "two workspace dirs");
        assert_eq!(s.total_bytes, 175);
    }

    #[test]
    fn prune_dry_run_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        mkfile(&root.join("run_old").join("x"), 10);
        let now = 1_000 * DAY;
        let scan = scan_of(&root, vec![entry("run_old", now - 30 * DAY)]);
        let policy = PrunePolicy {
            older_than_days: 7,
            keep_latest: 0,
            delete_workspaces: true,
        };
        let report =
            prune_run_workspaces(&root, now, &scan, &HashSet::new(), &policy, true).unwrap();
        assert_eq!(report.to_delete.len(), 1, "old workspace is eligible");
        assert_eq!(report.deleted_workspaces, 0, "dry-run deletes nothing");
        assert!(
            root.join("run_old").exists(),
            "dir still present after dry-run"
        );
    }

    #[test]
    fn prune_refuses_shallow_root() {
        assert!(validate_prune_root(Path::new("/")).is_err());
    }

    #[test]
    fn prune_keeps_running_recent_and_deletes_only_old() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        for n in ["run_old", "run_live", "run_new"] {
            mkfile(&root.join(n).join("x"), 10);
        }
        let now = 1_000 * DAY;
        let scan = scan_of(
            &root,
            vec![
                entry("run_old", now - 30 * DAY),
                entry("run_live", now - 30 * DAY), // old but running
                entry("run_new", now - DAY),       // recent
            ],
        );
        let running: HashSet<String> = ["run_live".to_string()].into_iter().collect();
        let policy = PrunePolicy {
            older_than_days: 7,
            keep_latest: 0,
            delete_workspaces: true,
        };
        let report = prune_run_workspaces(&root, now, &scan, &running, &policy, false).unwrap();
        assert_eq!(report.deleted_workspaces, 1, "{report:?}");
        assert_eq!(report.kept_running, 1);
        assert_eq!(report.kept_recent, 1);
        assert!(!root.join("run_old").exists(), "old workspace removed");
        assert!(root.join("run_live").exists(), "running workspace kept");
        assert!(root.join("run_new").exists(), "recent workspace kept");
    }

    #[test]
    fn prune_keep_latest_protects_newest_even_if_old() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        for n in ["run_o1", "run_o2", "run_o3"] {
            mkfile(&root.join(n).join("x"), 10);
        }
        let now = 1_000 * DAY;
        let scan = scan_of(
            &root,
            vec![
                entry("run_o1", now - 30 * DAY), // oldest
                entry("run_o2", now - 29 * DAY),
                entry("run_o3", now - 28 * DAY), // newest (all > 7d old)
            ],
        );
        let policy = PrunePolicy {
            older_than_days: 7,
            keep_latest: 2,
            delete_workspaces: true,
        };
        let report =
            prune_run_workspaces(&root, now, &scan, &HashSet::new(), &policy, false).unwrap();
        assert_eq!(report.kept_latest, 2);
        assert_eq!(report.deleted_workspaces, 1);
        assert!(!root.join("run_o1").exists(), "oldest removed");
        assert!(root.join("run_o3").exists(), "newest kept");
    }

    // ── execute_prune + audit + autoprune (store-backed) ──

    fn store_with_root(root: &Path) -> crate::nodes::coordinator::TaskStore {
        let mut s = crate::nodes::coordinator::TaskStore::in_memory().unwrap();
        s.set_run_workspace_root(root.to_path_buf());
        s
    }

    #[test]
    fn execute_prune_dry_run_audits_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        mkfile(&root.join("run_one").join("x"), 10);
        let store = store_with_root(&root);
        // older_than_days=0 makes the fresh dir eligible (cutoff == now).
        let out = execute_prune(&store, "manual", 0, 0, true, false, false, true).unwrap();
        assert!(out.report.dry_run);
        assert_eq!(out.report.to_delete.len(), 1);
        assert_eq!(out.report.deleted_workspaces, 0, "dry-run deletes nothing");
        assert!(root.join("run_one").exists());
        let audit = store.list_maintenance_audit(10).unwrap();
        assert_eq!(audit.len(), 1, "dry-run is still audited");
        assert!(audit[0].dry_run);
        assert_eq!(audit[0].status, "ok");
        assert_eq!(audit[0].trigger, "manual");
    }

    #[test]
    fn execute_prune_real_delete_audits_and_removes_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        mkfile(&root.join("run_a").join("x"), 10);
        mkfile(&root.join("run_b").join("x"), 10);
        let store = store_with_root(&root);
        // keep_latest=1 protects one; the other is deleted.
        let out = execute_prune(&store, "manual", 0, 1, true, false, false, false).unwrap();
        assert!(!out.report.dry_run);
        assert_eq!(out.report.deleted_workspaces, 1);
        let audit = store.list_maintenance_audit(10).unwrap();
        assert_eq!(audit.len(), 1);
        assert!(!audit[0].dry_run);
        assert_eq!(audit[0].deleted_workspaces, 1);
        assert_eq!(audit[0].status, "ok");
    }

    #[test]
    fn execute_prune_refused_root_audits_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store_with_root(&tmp.path().join("does-not-exist"));
        let res = execute_prune(&store, "manual", 7, 0, true, false, false, true);
        assert!(res.is_err(), "a missing root is refused");
        let audit = store.list_maintenance_audit(10).unwrap();
        assert_eq!(audit.len(), 1, "a refusal is still audited");
        assert_eq!(audit[0].status, "refused");
    }

    #[test]
    fn audit_rows_are_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        std::fs::create_dir_all(&root).unwrap();
        let store = store_with_root(&root);
        for _ in 0..3 {
            execute_prune(&store, "manual", 7, 0, true, false, false, true).unwrap();
        }
        let audit = store.list_maintenance_audit(10).unwrap();
        assert_eq!(audit.len(), 3);
        assert!(
            audit[0].id > audit[1].id && audit[1].id > audit[2].id,
            "newest first"
        );
    }

    #[test]
    fn autoprune_config_defaults_disabled_and_dry_run() {
        // With no env set, scheduled cleanup is OFF and dry-run.
        let cfg = resolve_autoprune_config();
        assert!(!cfg.enabled, "disabled by default");
        assert!(cfg.dry_run, "dry-run by default even if it ran");
        assert!(
            cfg.delete_workspaces,
            "default action when a real run happens"
        );
    }

    #[test]
    fn autoprune_tick_disabled_returns_none_and_no_audit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("ws").join("runs");
        std::fs::create_dir_all(&root).unwrap();
        let store = store_with_root(&root);
        assert!(autoprune_tick(&store).unwrap().is_none());
        assert_eq!(store.list_maintenance_audit(10).unwrap().len(), 0);
    }
}
