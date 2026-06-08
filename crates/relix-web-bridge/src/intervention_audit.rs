//! Bridge-side operator intervention audit ring.
//!
//! Every mutating operator-facing HTTP call (retry, recover,
//! cancel, provider CRUD, telegram save, etc.) appends an
//! entry to a bounded in-memory ring and — when a writable
//! path is configured — to a JSONL file on disk.
//!
//! The point is to give operators a single answer to the
//! question "who did what, when?" without grepping through
//! tracing logs. The bridge has no HTTP auth in alpha, so
//! "who" is the remote socket address (best-effort);
//! everything else is the action + target + outcome.
//!
//! Not to be confused with the per-node admission audit
//! (`relix_core::audit::AuditLog`), which records every
//! responder-side RPC including non-operator chat traffic.
//! This ring is bridge-local and operator-action-specific.
//!
//! Persistence: best-effort append. A failed disk write does
//! NOT block the underlying action — the ring entry still
//! lands in memory and a structured warn line hits the
//! tracing log.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default ring capacity. Tuned for small operator teams —
/// a busy hour of provider tweaks + retries lands well under
/// this and the file persistence keeps anything beyond it.
const DEFAULT_RING_CAP: usize = 500;

/// One operator-initiated intervention.
///
/// JSON shape is stable; additions only. Existing field types
/// must NOT change since the JSONL file is append-only and we
/// promise to keep older entries readable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterventionEntry {
    /// Monotonic per-process sequence. Resets on bridge
    /// restart. JSONL on disk retains the higher-resolution
    /// `ts` field for cross-restart ordering.
    pub seq: i64,
    /// Wall-clock unix seconds.
    pub ts: i64,
    /// Best-effort actor identification. The bridge has no
    /// HTTP auth in alpha, so this is typically the remote
    /// socket address (`127.0.0.1:54321`) or `"local"` when
    /// the call originated from within the process (a future
    /// background task, etc.). NEVER an API key or token.
    pub actor: String,
    /// Stable verb. One of:
    /// `retry` / `recover` / `cancel` / `provider_save` /
    /// `provider_delete` / `provider_enabled_set` /
    /// `provider_make_default` / `provider_test` /
    /// `telegram_save` / `telegram_test` /
    /// `task_note` (M60 onwards) / `compact_events_planned`.
    pub action: String,
    /// What the action was scoped to. Examples: a 32-hex
    /// task_id for `retry`/`cancel`; a provider name like
    /// `openai` for `provider_*`; `"telegram"` for the
    /// channel; `"all"` for global scans like recovery.
    pub target: String,
    /// One of `ok` / `refused` / `error`. `refused` covers
    /// outcomes like a retry that was rejected by the bridge
    /// guard (non-retryable failure class) — distinct from
    /// `error` (the action couldn't run at all).
    pub outcome: String,
    /// Short human-readable detail. Bridge-supplied, never
    /// echoed user input or upstream body — see redact rules
    /// at each call site. Capped at DETAIL_CAP bytes.
    pub detail: String,
    /// M68: bridge-generated correlation id for this
    /// intervention. Surfaces in chronicle events (when the
    /// underlying coord capability accepts a correlation_id
    /// arg) so operators can join the audit entry to the
    /// resulting `task.paused` / `task.operator_note` / etc.
    /// event. 16 hex chars (64 bits of entropy from OsRng) —
    /// short enough to scan in a log but wide enough to
    /// avoid collisions during normal operator activity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub correlation_id: String,
}

/// Hard cap on detail length to keep ring memory + JSONL
/// lines reasonable. Anything longer is truncated with a
/// `…[truncated]` suffix.
const DETAIL_CAP: usize = 240;

#[derive(Debug, Default)]
struct Inner {
    /// Newest-first ring of recent entries.
    events: VecDeque<InterventionEntry>,
    /// Monotonic sequence counter. Increments per record.
    seq: i64,
}

/// Shared lock-protected ring + file handle.
///
/// Cloneable via `Arc` — every axum handler grabs a clone
/// from `AppState`.
#[derive(Debug)]
pub struct InterventionAudit {
    inner: RwLock<Inner>,
    cap: usize,
    /// Optional JSONL file the ring also appends to. `None` in
    /// tests / when no `data_dir` is configured.
    file_path: Option<PathBuf>,
    /// Serializes file writes so concurrent records don't
    /// interleave. Cheap — file appends are rare relative to
    /// chat traffic.
    file_lock: Mutex<()>,
}

impl InterventionAudit {
    pub fn new(file_path: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner::default()),
            cap: DEFAULT_RING_CAP,
            file_path,
            file_lock: Mutex::new(()),
        })
    }

    /// Append a new intervention entry without a correlation
    /// id. Lands in the in-memory ring; best-effort writes to
    /// disk when a `file_path` is set. Production HTTP
    /// handlers prefer [`Self::record_with_id`] so the audit
    /// row and any resulting chronicle event share an id.
    ///
    /// Kept exported because future internal background
    /// tasks (recovery scans, etc.) may want a
    /// no-correlation path. Currently only exercised by
    /// unit tests; `#[allow(dead_code)]` keeps clippy quiet.
    #[allow(dead_code)]
    pub fn record(
        &self,
        actor: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
        outcome: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.record_with_id(actor, action, target, outcome, detail, String::new());
    }

    /// Append with a pre-minted correlation id (M68). The id
    /// becomes part of the audit entry + is expected to also
    /// land in any chronicle event the underlying coord
    /// capability emits, so the two surfaces can be joined
    /// post-hoc.
    pub fn record_with_id(
        &self,
        actor: impl Into<String>,
        action: impl Into<String>,
        target: impl Into<String>,
        outcome: impl Into<String>,
        detail: impl Into<String>,
        correlation_id: impl Into<String>,
    ) {
        let ts = unix_secs();
        // H9: redact known-shape secrets before persisting. Same
        // boundary discipline as chronicle writes (H8) — the
        // audit log is operator-readable forever, so a leak here
        // is just as bad as one in the chronicle. Clamp AFTER
        // redaction so the redaction marker is never truncated
        // mid-token.
        let detail = clamp_detail(relix_core::redact::redact_secrets(&detail.into()));
        let actor = clamp_detail(actor.into());
        let entry = {
            let mut g = self.inner.write().unwrap_or_else(|e| {
                tracing::warn!("intervention write lock poisoned; recovering inner state");
                e.into_inner()
            });
            g.seq += 1;
            let entry = InterventionEntry {
                seq: g.seq,
                ts,
                actor,
                action: action.into(),
                target: target.into(),
                outcome: outcome.into(),
                detail,
                correlation_id: correlation_id.into(),
            };
            g.events.push_front(entry.clone());
            while g.events.len() > self.cap {
                g.events.pop_back();
            }
            entry
        };
        self.append_to_file(&entry);
    }
}

/// Mint a 16-hex correlation id from OsRng (M68). Short
/// enough to scan in a log; 64 bits of entropy avoid
/// collisions during normal operator activity.
pub fn new_correlation_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(16);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

impl InterventionAudit {
    /// Snapshot the recent entries newest-first, optionally
    /// limited to those after a sequence cursor. `since=0`
    /// returns up to `limit` of the newest entries.
    pub fn since(&self, since: i64, limit: usize) -> (Vec<InterventionEntry>, i64) {
        let g = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("intervention read lock poisoned; recovering inner state");
            e.into_inner()
        });
        let mut out: Vec<InterventionEntry> = g
            .events
            .iter()
            .take_while(|e| e.seq > since)
            .take(limit)
            .cloned()
            .collect();
        // VecDeque is newest-first; we already have that
        // order. Caller renders top-down.
        let cursor = out.first().map(|e| e.seq).unwrap_or(g.seq);
        out.truncate(limit);
        // Shrink_to_fit avoids retaining oversize allocs.
        out.shrink_to_fit();
        (out, cursor)
    }

    /// Hard ring snapshot (for tests + diagnostics).
    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<InterventionEntry> {
        let g = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("intervention read lock poisoned; recovering inner state");
            e.into_inner()
        });
        g.events.iter().cloned().collect()
    }

    fn append_to_file(&self, entry: &InterventionEntry) {
        let Some(path) = self.file_path.as_ref() else {
            return;
        };
        // Serialize the entry. Failures here are bugs — the
        // type round-trips serde — but we never panic at
        // runtime.
        let Ok(mut line) = serde_json::to_string(entry) else {
            tracing::warn!(
                seq = entry.seq,
                action = %entry.action,
                "intervention audit: serialize failed"
            );
            return;
        };
        line.push('\n');
        if let Err(e) = crate::activity::append_intervention_activity(path, entry) {
            tracing::warn!(
                path = %path.display(),
                err = %e,
                "activity ledger: intervention mirror failed"
            );
        }
        let _guard = self.file_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Lazy parent dir creation: the bridge's data dir
        // might not exist yet on first run.
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write as _;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut f) => {
                if let Err(e) = f.write_all(line.as_bytes()) {
                    tracing::warn!(
                        path = %path.display(),
                        err = %e,
                        "intervention audit: write failed"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    err = %e,
                    "intervention audit: open failed"
                );
            }
        }
    }
}

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn clamp_detail(mut s: String) -> String {
    if s.len() <= DETAIL_CAP {
        return s;
    }
    // Truncate on a char boundary to stay valid UTF-8.
    let mut end = DETAIL_CAP;
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("…[truncated]");
    s
}

// ── HTTP surface ───────────────────────────────────────────

#[derive(Debug, serde::Deserialize, Default)]
pub struct RecentQuery {
    /// Sequence cursor — return entries strictly newer than
    /// this `seq`. `0` (or omitted) returns the most recent
    /// `limit` entries.
    #[serde(default)]
    pub since: Option<i64>,
    /// Cap. Bounded by the ring capacity; defaults to 100.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Serialize)]
pub struct RecentResponse {
    pub items: Vec<InterventionEntry>,
    /// `seq` of the newest item returned. Pass back as `?since=`
    /// on the next request to walk forward in time. When the
    /// page is empty, this echoes the ring's current high-water
    /// mark so the cursor doesn't reset on idle polling.
    pub next_cursor: i64,
}

/// `GET /v1/intervention/recent?since=N&limit=N` — newest-first
/// snapshot of recent operator interventions. Read-only.
pub async fn recent(
    axum::extract::State(state): axum::extract::State<crate::config::AppState>,
    axum::extract::Query(q): axum::extract::Query<RecentQuery>,
) -> axum::Json<RecentResponse> {
    let limit = q.limit.unwrap_or(100).min(DEFAULT_RING_CAP);
    let (items, cursor) = state.intervention_audit.since(q.since.unwrap_or(0), limit);
    axum::Json(RecentResponse {
        items,
        next_cursor: cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_with_id_round_trips_correlation_id_through_disk() {
        // M68: the correlation_id field persists through the
        // in-memory ring AND through the JSONL file so a
        // post-mortem grep can join an audit entry to its
        // chronicle counterpart.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("intervention.jsonl");
        let a = InterventionAudit::new(Some(path.clone()));
        let corr = new_correlation_id();
        assert_eq!(corr.len(), 16);
        assert!(corr.chars().all(|c| c.is_ascii_hexdigit()));
        a.record_with_id(
            "anon",
            "pause",
            "task-1",
            "ok",
            "running→paused",
            corr.clone(),
        );
        // In-memory check.
        let snap = a.snapshot();
        assert_eq!(snap[0].correlation_id, corr);
        // Disk check — same id round-trips.
        let body = std::fs::read_to_string(&path).expect("read jsonl");
        let line = body.lines().next().expect("at least one line");
        let parsed: InterventionEntry = serde_json::from_str(line).expect("parse");
        assert_eq!(parsed.correlation_id, corr);
    }

    #[test]
    fn record_redacts_secrets_in_detail_before_persist() {
        // H9: a pasted API key in the detail field must NOT land
        // in the in-memory ring OR the on-disk JSONL. Same posture
        // as the chronicle write boundary (H8). Operators who
        // grep their audit log a year later shouldn't see live
        // secrets.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("intervention.jsonl");
        let a = InterventionAudit::new(Some(path.clone()));
        // Assemble the fake secret-shaped token at runtime so no full
        // provider-key-shaped literal sits in source.
        let fake_key = ["sk", "-abcdef0123456789ABCDEF0123456789AAAA"].concat();
        a.record(
            "anon",
            "provider_test",
            "openai",
            "ok",
            format!("tried {fake_key}"),
        );
        let snap = a.snapshot();
        assert!(
            !snap[0].detail.contains("sk-abcdef"),
            "raw key leaked into ring: {}",
            snap[0].detail
        );
        assert!(snap[0].detail.contains("[REDACTED:OPENAI_KEY]"));
        // Disk file must be redacted too.
        let body = std::fs::read_to_string(&path).expect("read jsonl");
        assert!(
            !body.contains("sk-abcdef"),
            "raw key leaked to disk: {body}"
        );
        assert!(body.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn record_appends_in_newest_first_order() {
        let a = InterventionAudit::new(None);
        a.record("a1", "retry", "task-1", "ok", "first");
        a.record("a2", "retry", "task-2", "ok", "second");
        let snap = a.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].target, "task-2");
        assert_eq!(snap[1].target, "task-1");
        assert!(snap[0].seq > snap[1].seq);
    }

    #[test]
    fn ring_caps_at_default_capacity() {
        let a = InterventionAudit::new(None);
        for i in 0..(DEFAULT_RING_CAP + 50) {
            a.record("anon", "retry", format!("task-{i}"), "ok", "");
        }
        assert_eq!(a.snapshot().len(), DEFAULT_RING_CAP);
    }

    #[test]
    fn since_returns_only_entries_after_cursor() {
        let a = InterventionAudit::new(None);
        a.record("anon", "recover", "all", "ok", "0 recovered");
        a.record("anon", "retry", "task-1", "ok", "");
        let (page, cursor) = a.since(0, 10);
        assert_eq!(page.len(), 2);
        // Newest first — the second record is at index 0.
        assert_eq!(page[0].action, "retry");
        a.record("anon", "cancel", "task-1", "ok", "");
        let (page, _next_cursor) = a.since(cursor, 10);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].action, "cancel");
    }

    #[test]
    fn detail_truncates_long_payloads_on_char_boundary() {
        let a = InterventionAudit::new(None);
        // Build a string with a multibyte char crossing the
        // DETAIL_CAP boundary to verify we don't slice mid-
        // codepoint.
        let mut long = String::new();
        while long.len() < DETAIL_CAP - 1 {
            long.push('a');
        }
        long.push('ñ'); // 2-byte UTF-8 char straddling cap
        long.push_str("trailing-bytes");
        a.record("anon", "provider_save", "openai", "ok", long);
        let snap = a.snapshot();
        assert!(snap[0].detail.len() <= DETAIL_CAP + "…[truncated]".len());
        assert!(snap[0].detail.ends_with("…[truncated]"));
        // Standard UTF-8 invariant: char_indices walks
        // without panicking.
        for _ in snap[0].detail.char_indices() {}
    }

    #[test]
    fn file_persistence_writes_one_jsonl_line_per_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("intervention.jsonl");
        let a = InterventionAudit::new(Some(path.clone()));
        a.record("anon", "retry", "task-1", "ok", "first");
        a.record("anon", "retry", "task-2", "refused", "non-retryable");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "expected one JSONL line per record");
        // Each line must round-trip back to the typed entry.
        let parsed: InterventionEntry = serde_json::from_str(lines[0]).expect("parse line 1");
        assert_eq!(parsed.target, "task-1");
        assert_eq!(parsed.outcome, "ok");
    }

    #[test]
    fn record_never_panics_when_file_path_unwritable() {
        // Path inside a nonexistent root that we can't create
        // because the leading segment is invalid on every
        // platform. We expect the in-memory record to still
        // succeed and the disk write to silently warn-and-
        // skip.
        let path = if cfg!(windows) {
            PathBuf::from("Z:\\definitely-not-a-real-drive\\intervention.jsonl")
        } else {
            PathBuf::from("/proc/0/cant-write-here.jsonl")
        };
        let a = InterventionAudit::new(Some(path));
        a.record("anon", "retry", "task-1", "ok", "should still memo");
        assert_eq!(a.snapshot().len(), 1);
    }
}
