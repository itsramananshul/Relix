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

/// A **thread interaction** — an answerable card the agent (or
/// companion) raises on a Brief's thread (relix-execution-and-issue-
/// design §1.9; relix-dashboard-design §7). The slice covers three
/// kinds: `ask` (an open question for the operator to answer),
/// `confirm` (a yes/no gate, e.g. plan approval), and `suggest_tasks`
/// (an Operative proposes a bounded list of child Briefs; the operator
/// accepts — materializing them as real Sub-briefs — or rejects). The
/// lifecycle is `open → resolved | rejected`: a `confirm` answered yes
/// resolves, a no rejects; an `ask` always resolves with the answer
/// text; a `suggest_tasks` resolves on accept (children created) and
/// rejects on decline. A response is recorded once (idempotent), and
/// both the opening and the response are also written to the Brief's
/// Chronicle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interaction {
    pub interaction_id: String,
    pub task_id: String,
    /// `ask` | `confirm` | `suggest_tasks`.
    pub kind: String,
    pub prompt: String,
    /// Optional answer choices (radio/checkbox for `ask`); empty for a
    /// plain `confirm`.
    pub choices: Vec<String>,
    /// Who raised the card (the Operative, the companion, or a human).
    pub author: String,
    /// `open` | `resolved` | `rejected`.
    pub status: String,
    /// The operator's answer (the chosen option, free text, or yes/no
    /// note); `None` while still `open`. For an accepted `suggest_tasks`
    /// card this carries the comma-joined ids of the child Briefs created.
    pub response: Option<String>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
    /// Who answered it.
    pub resolved_by: Option<String>,
    /// The structured proposal for a `suggest_tasks` card (the proposed
    /// child Briefs); `None` for `ask`/`confirm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
}

/// One proposed child Brief inside a `suggest_tasks` interaction (§1.9):
/// a title, optionally a priority, and an optional simple **dependency
/// order** (`after`). Accepting the proposal materializes each child as a
/// real Sub-brief that inherits the parent's safe spine context
/// (Mandate/Campaign/reviewer; see [`super::TaskStore::respond_suggestion`]);
/// assignment still stays deferred (it runs governance gating).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSpec {
    pub title: String,
    /// `low` | `normal` | `high` | `urgent`; `None` opens at the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Optional intra-proposal dependency: the **0-based index of an
    /// earlier sibling** (§1.6 — a backward-only edge, so the graph is
    /// acyclic by construction) that this child depends on. On accept it
    /// becomes a Snag (`blocked_on`): the referenced sibling must reach
    /// `done` before this child is unblocked. [`normalize_proposal`]
    /// remaps it across any dropped (empty-title) children and refuses a
    /// forward / self / out-of-range / dropped-target reference at open
    /// time, so accept never has to fail half-way. `None` = no dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<usize>,
}

/// The bounded, sanitized proposal an Operative attaches to a
/// `suggest_tasks` card: a one-line summary plus the proposed child
/// Briefs. Normalized + size-capped on the way in (see
/// [`normalize_proposal`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub summary: String,
    pub children: Vec<ChildSpec>,
}

/// Hard caps on a `suggest_tasks` proposal — the proposal is bounded
/// and sanitized so a card can never carry an unbounded / oversized
/// payload (no file/path execution, no arbitrary giant JSON).
pub const MAX_SUGGESTED_CHILDREN: usize = 20;
/// Max length of a child-Brief title (chars). Longer titles are
/// truncated, not refused.
pub const MAX_CHILD_TITLE_LEN: usize = 200;
/// Max length of the proposal summary (chars). Longer is truncated.
pub const MAX_PROPOSAL_SUMMARY_LEN: usize = 500;

/// Validate + normalize a `suggest_tasks` proposal (pure, so the
/// doc-specified bounds are unit-testable in isolation):
///
/// - the summary is trimmed and length-capped (truncated, not refused);
/// - each child title is trimmed and length-capped; empty titles are
///   dropped;
/// - a child priority, when present, must be a valid Brief priority
///   (an invalid one is a hard error — it would otherwise be silently
///   dropped at create time);
/// - an `after` dependency (§1.6), when present, must reference an
///   **earlier kept sibling** by its original index. It is remapped to
///   the post-drop position; a forward / self / out-of-range / dropped-
///   target reference is a hard error here (rejected at open time so the
///   accept path never half-creates an order it can't honour);
/// - the proposal must have at least one child and **no more than**
///   [`MAX_SUGGESTED_CHILDREN`] (over-cap is refused, never silently
///   truncated — the operator must see the full set they accept).
pub fn normalize_proposal(summary: &str, children: &[ChildSpec]) -> Result<Proposal, String> {
    let summary: String = summary.trim().chars().take(MAX_PROPOSAL_SUMMARY_LEN).collect();
    // Pass 1: trim titles + drop empties, remembering the original→kept
    // index mapping so an `after` (which names an *original* sibling
    // position) can be re-pointed after drops.
    let mut old_to_new: Vec<Option<usize>> = Vec::with_capacity(children.len());
    let mut kept: Vec<ChildSpec> = Vec::new();
    for c in children {
        let title: String = c.title.trim().chars().take(MAX_CHILD_TITLE_LEN).collect();
        if title.is_empty() {
            old_to_new.push(None); // dropped — nothing maps here
            continue;
        }
        let priority = match c.priority.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(p) if is_priority(p) => Some(p.to_string()),
            Some(p) => return Err(format!("priority '{p}' not in low/normal/high/urgent")),
        };
        old_to_new.push(Some(kept.len()));
        // `after` is carried through pass 1 untouched (still an *original*
        // index); pass 2 validates + remaps it once all positions are known.
        kept.push(ChildSpec { title, priority, after: c.after });
    }
    if kept.is_empty() {
        return Err("a suggestion needs at least one child task".to_string());
    }
    if kept.len() > MAX_SUGGESTED_CHILDREN {
        return Err(format!(
            "too many proposed tasks ({}); the limit is {MAX_SUGGESTED_CHILDREN}",
            kept.len()
        ));
    }
    // Pass 2: validate + remap each `after` to a backward-only kept index.
    let mut norm: Vec<ChildSpec> = Vec::with_capacity(kept.len());
    for (new_idx, mut spec) in kept.into_iter().enumerate() {
        if let Some(orig) = spec.after {
            if orig >= old_to_new.len() {
                return Err(format!(
                    "task #{new_idx} depends on out-of-range task #{orig}"
                ));
            }
            let mapped = old_to_new[orig].ok_or_else(|| {
                format!("task #{new_idx} depends on a dropped (empty) task #{orig}")
            })?;
            if mapped >= new_idx {
                return Err(format!(
                    "task #{new_idx} must depend on an earlier task (got #{mapped})"
                ));
            }
            spec.after = Some(mapped);
        }
        norm.push(spec);
    }
    Ok(Proposal { summary, children: norm })
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

    fn child(title: &str) -> ChildSpec {
        ChildSpec { title: title.into(), priority: None, after: None }
    }

    #[test]
    fn proposal_normalizes_and_drops_empty_titles() {
        let p = normalize_proposal(
            "  Break this down  ",
            &[child("First"), child("   "), child("  Second  ")],
        )
        .expect("valid");
        assert_eq!(p.summary, "Break this down");
        assert_eq!(p.children.len(), 2);
        assert_eq!(p.children[0].title, "First");
        assert_eq!(p.children[1].title, "Second");
    }

    #[test]
    fn proposal_requires_at_least_one_child() {
        assert!(normalize_proposal("s", &[]).is_err());
        assert!(normalize_proposal("s", &[child("   ")]).is_err());
    }

    #[test]
    fn proposal_rejects_over_cap() {
        let many: Vec<ChildSpec> = (0..=MAX_SUGGESTED_CHILDREN)
            .map(|i| child(&format!("t{i}")))
            .collect();
        assert!(normalize_proposal("s", &many).is_err());
        let ok: Vec<ChildSpec> = (0..MAX_SUGGESTED_CHILDREN)
            .map(|i| child(&format!("t{i}")))
            .collect();
        assert!(normalize_proposal("s", &ok).is_ok());
    }

    #[test]
    fn proposal_validates_priority_and_bounds_lengths() {
        assert!(
            normalize_proposal(
                "s",
                &[ChildSpec { title: "t".into(), priority: Some("urgent".into()), after: None }]
            )
            .is_ok()
        );
        assert!(
            normalize_proposal(
                "s",
                &[ChildSpec { title: "t".into(), priority: Some("meh".into()), after: None }]
            )
            .is_err()
        );
        let long_title = "x".repeat(MAX_CHILD_TITLE_LEN + 50);
        let long_summary = "y".repeat(MAX_PROPOSAL_SUMMARY_LEN + 50);
        let p = normalize_proposal(&long_summary, &[child(&long_title)]).expect("valid");
        assert_eq!(p.summary.chars().count(), MAX_PROPOSAL_SUMMARY_LEN);
        assert_eq!(p.children[0].title.chars().count(), MAX_CHILD_TITLE_LEN);
    }

    fn child_after(title: &str, after: Option<usize>) -> ChildSpec {
        ChildSpec { title: title.into(), priority: None, after }
    }

    #[test]
    fn proposal_keeps_a_valid_backward_after() {
        // child #1 depends on #0 — a legal backward edge.
        let p = normalize_proposal(
            "Plan",
            &[child_after("First", None), child_after("Second", Some(0))],
        )
        .expect("valid backward dependency");
        assert_eq!(p.children[0].after, None);
        assert_eq!(p.children[1].after, Some(0));
    }

    #[test]
    fn proposal_rejects_forward_self_and_out_of_range_after() {
        // Forward reference (#0 → #1) is refused.
        assert!(
            normalize_proposal(
                "p",
                &[child_after("A", Some(1)), child_after("B", None)]
            )
            .is_err()
        );
        // Self reference (#0 → #0) is refused.
        assert!(normalize_proposal("p", &[child_after("A", Some(0))]).is_err());
        // Out-of-range reference is refused.
        assert!(
            normalize_proposal(
                "p",
                &[child_after("A", None), child_after("B", Some(9))]
            )
            .is_err()
        );
    }

    #[test]
    fn proposal_after_remaps_across_dropped_children() {
        // Original indices: 0=A, 1="" (dropped), 2=C(after=0). After the
        // drop A→0, C→1, and `after=0` still points at A — a valid edge.
        let p = normalize_proposal(
            "p",
            &[
                child_after("A", None),
                child_after("   ", None),
                child_after("C", Some(0)),
            ],
        )
        .expect("after remaps over the dropped child");
        assert_eq!(p.children.len(), 2);
        assert_eq!(p.children[1].title, "C");
        assert_eq!(p.children[1].after, Some(0));
    }

    #[test]
    fn proposal_rejects_after_pointing_at_a_dropped_child() {
        // #2 (C) depends on original #1, which is dropped (empty) — refused.
        assert!(
            normalize_proposal(
                "p",
                &[
                    child_after("A", None),
                    child_after("   ", None),
                    child_after("C", Some(1)),
                ]
            )
            .is_err()
        );
    }
}
