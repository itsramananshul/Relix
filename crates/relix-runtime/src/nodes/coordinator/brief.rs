//! **Brief** lifecycle logic — the product-spine state machine
//! layered over the coordinator Task ledger.
//!
//! A Brief (the evolved Task — see `docs/relix-lexicon.md`) has,
//! in addition to its execution `status`, a **board status**: the
//! column it sits in on the operator's board. These are separate
//! axes — execution status is "what the runtime is doing"; board
//! status is "where the work sits in the human's workflow."
//!
//! This module is pure logic (no I/O), so the transition rules
//! are testable in isolation and called from wherever the
//! coordinator writes `board_status`.

use serde::{Deserialize, Serialize};

/// A **Dossier** — a durable artifact attached to a Brief (the
/// "Document" in the lexicon): a plan, a design, a note, a
/// deliverable. Append-only and versioned by id, so the artifact
/// trail of a Brief is auditable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dossier {
    pub doc_id: String,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A lightweight Dossier listing row (metadata only, no body) for
/// the artifacts panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierMeta {
    pub doc_id: String,
    pub kind: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// The product-spine fields of a Brief (the columns layered onto
/// the Task ledger): who it's assigned to, where it sits on the
/// board, its priority, and what it links *up* to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefFields {
    pub task_id: String,
    /// The human identifier (e.g. `REL-42`) per
    /// relix-execution-and-issue-design §1.2; `None` for a Task that
    /// was never materialized as a Brief.
    pub human_ref: Option<String>,
    pub assignee_agent_id: Option<String>,
    pub board_status: String,
    pub priority: String,
    /// The Operative/Lead responsible for review before the Brief
    /// can enter `in_review`.
    pub reviewer_agent_id: Option<String>,
    pub mandate_id: Option<String>,
    pub campaign_id: Option<String>,
}

/// A Brief as it appears on the board — a compact card with its
/// title, column, priority, assignee, and spine links. The row
/// shape behind the board view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefCard {
    pub task_id: String,
    pub title: String,
    pub board_status: String,
    pub priority: String,
    pub assignee_agent_id: Option<String>,
    pub mandate_id: Option<String>,
    pub campaign_id: Option<String>,
}

/// The current Claim (lease) holder on a Brief — the Operative that
/// has checked it out for a run, with the lease expiry (unix secs).
/// `None` on the Brief detail when no live Claim is held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimInfo {
    pub agent_id: String,
    pub expires_at: i64,
}

/// One Chronicle event in the Brief detail's recent-events tail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChronicleEntry {
    pub event_id: i64,
    pub ts: i64,
    pub event_type: String,
    pub payload: String,
}

/// A compact Chronicle summary embedded in the Brief detail: the
/// total event count plus the newest few entries. The full,
/// paginated timeline stays on `GET /v1/spine/briefs/:id/events`
/// (and the live thread on `…/thread`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChronicleSummary {
    /// Total Chronicle events recorded on this Brief.
    pub total: i64,
    /// The newest `recent` entries (newest first), bounded.
    pub recent: Vec<ChronicleEntry>,
}

/// A bounded summary of a Brief's most recent Shift (run), embedded in the
/// Brief detail so the operator sees the execution state without a second
/// fetch. The full run record + transcript live on `GET /v1/runs/:id`; the
/// per-Brief Shift history on `GET /v1/spine/briefs/:id/runs`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatestRun {
    pub run_id: String,
    /// The adapter (Rig) that ran it.
    pub rig: String,
    /// `running` while in flight, then a terminal state: `done` / `failed` /
    /// `continued` / `cancelled` / `interrupted` (stale-run recovery), or
    /// `refused` (a durable pre-run refusal — see `refusal_reason`).
    pub status: String,
    /// What triggered it: `manual` / `heartbeat` / `scheduled`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    pub started_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<i64>,
    /// The Rig's result/reason — already secret-redacted, and bounded to a
    /// short snippet here (full text on the run detail).
    pub summary: String,
    /// Operator review: `pending_review` / `accepted` / `rejected`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    /// Safe-apply state: `not_applicable` / `blocked` / `ready` / `applied` /
    /// `failed` / `conflicted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apply_status: Option<String>,
    /// When `status == "refused"`: the machine reason a run never started —
    /// `unassigned` / `no_adapter` / `adapter_unavailable` / `workspace_error`
    /// / `workspace_context_error` / `over_allowance` (autonomous Allowance
    /// hard-stop). `None` for runs that actually executed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal_reason: Option<String>,
    /// Changed-file count this run produced.
    pub artifact_count: i64,
    /// Total Shifts (runs) recorded on this Brief.
    pub total_runs: i64,
}

/// The full detail view of a Brief, assembled in one read: its
/// spine fields, title, both directions of its relation graph (each
/// tenant-filtered), its Dossiers, labels, due/pinned, blocked flag,
/// the current Claim holder, a wakeup count, a Chronicle summary, and the
/// latest Shift (run) summary. Saves the detail pane a fan-out of calls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefDetail {
    /// The Brief's human-facing title.
    pub title: String,
    pub fields: BriefFields,
    /// Downstream: the Sub-briefs spawned from this Brief (same Guild only).
    pub subbriefs: Vec<String>,
    /// Downstream: the Snags (blockers) on this Brief (same Guild only).
    pub snags: Vec<String>,
    /// Upstream: the Briefs this Brief blocks (who waits on it; same Guild).
    pub blocking: Vec<String>,
    /// Upstream: the parent Briefs that spawned this as a Sub-brief (same Guild).
    pub parents: Vec<String>,
    pub dossiers: Vec<DossierMeta>,
    /// The Brief's free-form labels.
    pub labels: Vec<String>,
    /// Pinned to the top of its board column.
    pub pinned: bool,
    /// Optional due date (unix secs); `None` when unset.
    pub due_at: Option<i64>,
    /// True when at least one Snag's blocker isn't `done`.
    pub blocked: bool,
    /// The current Claim/lease holder, if one is live.
    pub claim: Option<ClaimInfo>,
    /// How many wakeup-ledger rows this Brief has (full ledger on `…/wakeups`).
    pub wakeup_count: i64,
    /// Total Chronicle events + the newest few (full timeline on `…/events`).
    pub chronicle: ChronicleSummary,
    /// The Brief's most recent Shift (run) summary, or `None` when it has
    /// never run. Full history on `…/runs`; full run on `/v1/runs/:id`.
    pub latest_run: Option<LatestRun>,
}

/// The board columns a Brief can sit in.
///
/// `backlog → todo → in_progress → in_review → done` is the happy
/// path; `blocked` is a side state you can enter from / leave to
/// active work; `cancelled` is terminal.
pub const BOARD_STATUSES: &[&str] = &[
    "backlog",
    "todo",
    "in_progress",
    "in_review",
    "done",
    "blocked",
    "cancelled",
];

/// Priority levels for a Brief.
pub const PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];

pub fn is_board_status(s: &str) -> bool {
    BOARD_STATUSES.contains(&s)
}

pub fn is_priority(s: &str) -> bool {
    PRIORITIES.contains(&s)
}

/// Is moving a Brief's board status `from → to` a legal
/// transition? Idempotent (`from == to` is allowed as a no-op).
///
/// Rules:
/// - `cancelled` is terminal — nothing leaves it.
/// - any live (non-cancelled) Brief may be `cancelled`.
/// - `done` may be re-opened to `in_progress`.
/// - otherwise only adjacent workflow moves are allowed (you
///   can't, e.g., jump `backlog → done`).
pub fn board_transition_allowed(from: &str, to: &str) -> bool {
    if !is_board_status(from) || !is_board_status(to) {
        return false;
    }
    if from == to {
        return true; // idempotent no-op
    }
    if from == "cancelled" {
        return false; // terminal
    }
    if to == "cancelled" {
        return true; // anything live can be cancelled
    }
    matches!(
        (from, to),
        ("backlog", "todo")
            | ("backlog", "in_progress")
            | ("todo", "backlog")
            | ("todo", "in_progress")
            | ("in_progress", "todo")
            | ("in_progress", "in_review")
            | ("in_progress", "blocked")
            | ("in_review", "in_progress")
            | ("in_review", "done")
            | ("blocked", "in_progress")
            | ("blocked", "todo")
            | ("done", "in_progress") // re-open
    )
}

/// Convenience: the default board status a fresh Brief opens in.
pub const DEFAULT_BOARD_STATUS: &str = "backlog";
/// Convenience: the default priority a fresh Brief opens at.
pub const DEFAULT_PRIORITY: &str = "normal";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocab_validates() {
        assert!(is_board_status("in_progress"));
        assert!(!is_board_status("doing"));
        assert!(is_priority("urgent"));
        assert!(!is_priority("meh"));
    }

    #[test]
    fn happy_path_is_walkable() {
        assert!(board_transition_allowed("backlog", "todo"));
        assert!(board_transition_allowed("todo", "in_progress"));
        assert!(board_transition_allowed("in_progress", "in_review"));
        assert!(board_transition_allowed("in_review", "done"));
    }

    #[test]
    fn cannot_skip_columns() {
        assert!(!board_transition_allowed("backlog", "done"));
        assert!(!board_transition_allowed("todo", "in_review"));
        assert!(!board_transition_allowed("backlog", "in_review"));
    }

    #[test]
    fn blocked_is_a_reversible_side_state() {
        assert!(board_transition_allowed("in_progress", "blocked"));
        assert!(board_transition_allowed("blocked", "in_progress"));
        assert!(board_transition_allowed("blocked", "todo"));
    }

    #[test]
    fn anything_live_can_be_cancelled_but_cancel_is_terminal() {
        for s in [
            "backlog",
            "todo",
            "in_progress",
            "in_review",
            "done",
            "blocked",
        ] {
            assert!(
                board_transition_allowed(s, "cancelled"),
                "{s} should be cancellable"
            );
        }
        // Cancelled is terminal.
        for s in ["backlog", "todo", "in_progress", "done"] {
            assert!(
                !board_transition_allowed("cancelled", s),
                "cancelled → {s} must be rejected"
            );
        }
    }

    #[test]
    fn done_can_be_reopened_only_to_in_progress() {
        assert!(board_transition_allowed("done", "in_progress"));
        assert!(board_transition_allowed("done", "cancelled"));
        assert!(!board_transition_allowed("done", "todo"));
        assert!(!board_transition_allowed("done", "backlog"));
    }

    #[test]
    fn idempotent_self_transition_is_allowed() {
        for s in BOARD_STATUSES {
            assert!(
                board_transition_allowed(s, s),
                "{s} → {s} should be a no-op"
            );
        }
    }

    #[test]
    fn unknown_statuses_are_rejected() {
        assert!(!board_transition_allowed("backlog", "bogus"));
        assert!(!board_transition_allowed("bogus", "todo"));
    }
}
