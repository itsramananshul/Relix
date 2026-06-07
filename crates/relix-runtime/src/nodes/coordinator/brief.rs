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
///
/// The authoring/revision/fork metadata (`author`, `revision_of_doc_id`,
/// `forked_from_doc_id`, `revision_number`) is **additive** (§1.8): legacy
/// rows and rows written by the original `brief.dossier_add` / plan-package
/// paths carry `None` for the first three; `revision_number` is always
/// derived in the read (the 1-based position of this row among the same
/// Brief+kind, oldest first), so every Dossier — old or new — has one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dossier {
    pub doc_id: String,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Who authored this revision (`brief.dossier_author`); `None` for legacy
    /// / `brief.dossier_add` / plan-package rows.
    #[serde(default)]
    pub author: Option<String>,
    /// The immediate prior revision this row supersedes via the optimistic-lock
    /// `revise` path; `None` for a first revision or an explicit fork.
    #[serde(default)]
    pub revision_of_doc_id: Option<String>,
    /// The base Dossier this row was explicitly **forked** from; `None` unless
    /// authored with `mode = fork`.
    #[serde(default)]
    pub forked_from_doc_id: Option<String>,
    /// Derived: this row's 1-based revision number within its Brief+kind
    /// (oldest = 1). Computed in the read, never stored, so it stays correct
    /// for legacy rows too.
    #[serde(default)]
    pub revision_number: i64,
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
    /// Who authored this revision; `None` for legacy / non-authored rows.
    #[serde(default)]
    pub author: Option<String>,
    /// The immediate prior revision this supersedes (revise path); `None`
    /// otherwise.
    #[serde(default)]
    pub revision_of_doc_id: Option<String>,
    /// The base Dossier this was forked from; `None` unless forked.
    #[serde(default)]
    pub forked_from_doc_id: Option<String>,
    /// Derived 1-based revision number within the Brief+kind (oldest = 1).
    #[serde(default)]
    pub revision_number: i64,
}

/// How a `brief.dossier_author` write relates to existing revisions (§1.8):
/// `Revise` writes the next linear revision under an optimistic lock (or the
/// first revision of a kind); `Fork` explicitly branches a new line from a
/// base Dossier even if a newer revision has since landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DossierAuthorMode {
    Revise,
    Fork,
}

/// The result of a successful `brief.dossier_author` write (§1.8): the
/// new append-only Dossier row plus its lineage. Returned to the operator /
/// bridge so the editor can show "revision N of <kind>" and keep an
/// `expected_latest_doc_id` for the next optimistic save.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierAuthored {
    pub doc_id: String,
    pub task_id: String,
    pub kind: String,
    pub title: String,
    pub author: Option<String>,
    /// `create` (first revision of a kind), `revise` (superseded a prior
    /// revision under the optimistic lock), or `fork` (branched from a base).
    pub mode: String,
    /// The 1-based revision number of the new row within its Brief+kind.
    pub revision_number: i64,
    /// The prior revision this row supersedes (revise path); `None` for a
    /// first `create` or a `fork`.
    pub revision_of_doc_id: Option<String>,
    /// The base Dossier this row forked from (`fork` only).
    pub forked_from_doc_id: Option<String>,
}

/// A **stale-lock refusal** from `brief.dossier_author` (§1.8): the
/// optimistic-concurrency check failed because the caller's
/// `expected_latest_doc_id` no longer matches the current latest revision of
/// that Brief+kind (a newer revision landed first). **Nothing is written** —
/// the caller must reload (or explicitly `fork`). The `stale` discriminant is
/// what the bridge inspects to map this onto an honest HTTP `409`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierStale {
    /// Always `true` — the wire discriminant the bridge keys its 409 on.
    pub stale: bool,
    pub kind: String,
    /// What the caller asserted was the latest revision.
    pub expected_latest_doc_id: Option<String>,
    /// What the latest revision actually is right now.
    pub current_latest_doc_id: Option<String>,
}

/// A **locked-document write refusal** from `brief.dossier_author` (§1.8
/// document locking): the logical Dossier (this Brief + `kind`) is held under
/// an explicit lock by a *different* subject, so the write is refused with
/// **nothing written** — a locked document is never silently overwritten. The
/// `locked` discriminant is what the bridge inspects to map this onto an honest
/// HTTP `409`. The lock owner can still author normally (it owns the lock);
/// another author must wait for an `unlock` (the owner releases it).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierLocked {
    /// Always `true` — the wire discriminant the bridge keys its 409 on.
    pub locked: bool,
    pub kind: String,
    /// The subject that currently holds the lock on this kind.
    pub locked_by: String,
}

/// The outcome of [`super::TaskStore::author_dossier`]: a written revision, a
/// no-write stale-lock refusal, or a no-write locked-document refusal. All are
/// `Ok` (the capability succeeded in *deciding*); only `Authored` mutates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DossierAuthorOutcome {
    Authored(DossierAuthored),
    Stale(DossierStale),
    Locked(DossierLocked),
}

/// The outcome of [`super::TaskStore::author_prime_dossier`] — the
/// Prime-governed, lock-aware, idempotent Dossier-authoring wrapper used by the
/// company orchestration/strategy paths (relix-company-model §12.5;
/// relix-execution-and-issue-design §1.8). It composes the append-only
/// [`super::TaskStore::author_dossier`] write with a pre-check that (a) never
/// appends a duplicate when Prime already authored the kind and (b) never
/// clobbers a human/editor (or legacy author-less) revision. Every variant is
/// `Ok` (the helper succeeded in *deciding*); only `Authored` mutates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrimeDossierOutcome {
    /// A fresh, Prime-owned first revision of this kind was written through the
    /// governed `author_dossier` path.
    Authored(DossierAuthored),
    /// Prime already owns the latest revision of this kind — left untouched, so
    /// a rerun of `mandate.orchestrate` never appends a duplicate row.
    AlreadyPresent { kind: String, doc_id: String },
    /// The logical Dossier (this Brief + kind) is locked by a *different*
    /// subject — `author_dossier` refused the write, nothing was overwritten.
    LockedByOther { kind: String, locked_by: String },
    /// The latest revision of this kind was authored by a non-Prime subject (a
    /// human/editor, or a legacy author-less `add_dossier` row) — preserved, not
    /// clobbered.
    SkippedHumanOwned {
        kind: String,
        author: Option<String>,
    },
    /// A concurrent revision landed first (optimistic-lock stale) — nothing was
    /// written.
    Stale { kind: String },
}

impl PrimeDossierOutcome {
    /// A short, stable label for the persisted/returned note (tests assert on
    /// it). One of `authored` / `already_present` / `locked_by_other` /
    /// `skipped_human_owned` / `stale`.
    pub fn label(&self) -> &'static str {
        match self {
            PrimeDossierOutcome::Authored(_) => "authored",
            PrimeDossierOutcome::AlreadyPresent { .. } => "already_present",
            PrimeDossierOutcome::LockedByOther { .. } => "locked_by_other",
            PrimeDossierOutcome::SkippedHumanOwned { .. } => "skipped_human_owned",
            PrimeDossierOutcome::Stale { .. } => "stale",
        }
    }
}

/// A **lock** held on a logical Dossier (a Brief's document `kind`, e.g.
/// `plan`) — §1.8 document locking. While a lock exists, only `locked_by` may
/// author a new revision of that `kind`; any other author's write is refused
/// (no silent overwrite). The lock is per `(Brief, kind)` and tenant-scoped via
/// the owning Brief, exactly like Dossiers. **Owner-or-nobody:** only the owner
/// may `unlock` (a conservative, auditable contract — there is no operator
/// force-unlock in this v1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierLock {
    pub task_id: String,
    pub kind: String,
    pub locked_by: String,
    pub locked_at: i64,
    /// Optional human reason the document was locked (bounded); omitted when
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A **lock/unlock conflict refusal** (§1.8): the caller tried to lock or
/// unlock a Dossier `kind` that is currently locked by a *different* subject.
/// Nothing is changed. The `conflict` discriminant is what the bridge inspects
/// to map this onto an honest HTTP `409`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierLockConflict {
    /// Always `true` — the wire discriminant the bridge keys its 409 on.
    pub conflict: bool,
    pub kind: String,
    /// The subject that currently holds the lock.
    pub locked_by: String,
}

/// The outcome of [`super::TaskStore::lock_dossier`]: the active lock now held
/// by the caller, or a conflict refusal when another subject holds it. Both are
/// `Ok` (the capability decided); only `Locked` mutates. A re-lock by the same
/// owner is idempotent and updates the reason.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DossierLockOutcome {
    Locked(DossierLock),
    Conflict(DossierLockConflict),
}

/// The result of a successful [`super::TaskStore::unlock_dossier`] (§1.8): the
/// lock on `kind` is released (or was already absent — unlock is idempotent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DossierUnlocked {
    /// Always `true` — the document `kind` is now unlocked.
    pub unlocked: bool,
    pub kind: String,
    /// The subject that held the lock that was just released; `None` when the
    /// kind was not locked (an idempotent no-op unlock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previously_locked_by: Option<String>,
}

/// The outcome of [`super::TaskStore::unlock_dossier`]: the released lock, or a
/// conflict refusal when a *different* subject holds it (only the owner may
/// unlock in this v1). Both are `Ok`; only `Unlocked` mutates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DossierUnlockOutcome {
    Unlocked(DossierUnlocked),
    Conflict(DossierLockConflict),
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
    /// `open` | `resolved` | `rejected` | `cancelled` | `expired`. A card is
    /// `cancelled` when an operator closes it without answering
    /// (`brief.interaction_cancel`); `expired` when a bound plan confirm went
    /// stale (the plan changed under it).
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
    /// **Approval-bound plan confirm** (relix-execution-and-issue-design
    /// §1.8): when this `confirm` card was opened against a specific Dossier
    /// revision, the exact bound Dossier id (which IS the revision — Dossiers
    /// are immutable, append-only rows). The accept path re-checks that the
    /// latest Dossier of [`Self::bound_doc_kind`] is still this id; if the plan
    /// changed since opening, the accept is refused as **stale** and the card
    /// expires (it must never resolve as *approved* against a superseded plan).
    /// `None` for a plain `ask`/`confirm`/`suggest_tasks` (those behave exactly
    /// as before — no binding, no staleness check, no supersede-on-comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_doc_id: Option<String>,
    /// The Dossier kind this confirm is bound to (e.g. `plan`); `None` when
    /// unbound. Paired with [`Self::bound_doc_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_doc_kind: Option<String>,
    /// **Plan package** (relix-execution-and-issue-design §1.7/§1.8/§3.1): when
    /// this `confirm` card was opened as part of a *plan package* (a `plan`
    /// Dossier + a `suggest_tasks` proposal + this approval-bound confirm, all
    /// created together by `brief.plan_package_open`), the exact `suggest_tasks`
    /// interaction id this confirm is linked to. Accepting this confirm
    /// **materializes that exact proposal** through the existing resumable,
    /// exactly-once decomposition ledger; rejecting it closes the linked
    /// proposal without creating children. `None` for a plain
    /// `ask`/`confirm`/`suggest_tasks` and for a standalone
    /// `brief.plan_confirm_open` confirm (those behave exactly as before — no
    /// linked proposal, answered through `brief.interaction_respond`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_interaction_id: Option<String>,
}

/// One proposed child Brief inside a `suggest_tasks` interaction (§1.9):
/// a title, optionally a priority, an optional simple **dependency
/// order** (`after`), and an optional **explicit assignee hint** (by
/// Operative id or by role). Accepting the proposal materializes each
/// child as a real Sub-brief that inherits the parent's safe spine
/// context (Mandate/Campaign/reviewer; see
/// [`super::TaskStore::respond_suggestion`]). The parent's assignee is
/// **never** inherited; assignment happens only when a child carries an
/// explicit hint, and only after that hint passes the existing
/// assign-Key gate (same-Guild, active) — otherwise the child opens
/// unassigned, exactly as before.
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
    /// Optional **explicit assignee hint by Operative id** (§1.9, model A
    /// — precise). When set, accepting the proposal validates the id
    /// (same-Guild, active) through the existing assign-Key gate and
    /// assigns the materialized child to it; an unknown / cross-Guild /
    /// inactive id, or an id the accepter's assign-Key forbids, refuses
    /// the **whole** accept *before* any child is created (never a partial
    /// materialization). Mutually exclusive with [`Self::assignee_role`]
    /// (both set is rejected at open). `None` = the child opens unassigned
    /// (the default — the parent's assignee is never inherited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_agent_id: Option<String>,
    /// Optional **explicit assignee hint by role** (§1.9, model B —
    /// friendly). On accept the role is resolved to the **oldest active
    /// same-role Operative in the Guild** (deterministic) and assigned
    /// through the same gate; no active match refuses the whole accept
    /// before any child is created. Mutually exclusive with
    /// [`Self::assignee_agent_id`]. `None` = unassigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee_role: Option<String>,
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

/// The three artifacts a **plan package** creates together
/// (relix-execution-and-issue-design §1.7/§1.8/§3.1): an immutable `plan`
/// Dossier revision, the structured `suggest_tasks` proposal, and the
/// approval-bound `confirm` linked to BOTH. Returned by
/// [`super::TaskStore::open_plan_package`]. Accepting the confirm materializes
/// the linked proposal through the resumable, exactly-once decomposition
/// ledger; rejecting it closes the proposal without creating children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPackage {
    /// The immutable `plan` Dossier revision the confirm is bound to (its id IS
    /// the revision — Dossiers are append-only rows).
    pub plan_doc_id: String,
    /// The `suggest_tasks` interaction carrying the proposal the confirm gates.
    pub suggestion_id: String,
    /// The approval-bound `confirm` interaction linked to the plan revision and
    /// the proposal.
    pub confirm_id: String,
}

/// The honest, typed outcome of answering a plan-package confirm
/// (relix-execution-and-issue-design §1.7/§1.8). It carries *what actually
/// happened* so a caller never has to infer approval from a bare 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanConfirmResult {
    /// One of:
    /// - `approved` — the confirm resolved and the linked proposal materialized
    ///   on this call;
    /// - `already_approved` — idempotent duplicate accept: the children already
    ///   existed and the SAME ids are returned (never duplicated);
    /// - `rejected` — the confirm and its still-open linked proposal were both
    ///   closed, no children created;
    /// - `rejected_proposal_already_closed` — the confirm was rejected, but the
    ///   linked proposal had already materialized/closed and was left intact.
    pub outcome: String,
    /// The linked `suggest_tasks` interaction id.
    pub suggestion_id: String,
    /// The child Brief ids created by materializing the linked proposal (empty
    /// on reject). On a duplicate accept these are the SAME ids — never doubled.
    pub created: Vec<String>,
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
/// Max length of an assignee hint (Operative id or role) on a proposed
/// child (chars). Over-cap is a hard error, not truncated — an over-long
/// id/role would never resolve and must not bloat the card.
pub const MAX_ASSIGNEE_HINT_LEN: usize = 128;

/// Trim + length-check an optional assignee hint (id or role); empty ⇒
/// `None`, over-cap ⇒ hard error (`what` names the kind for the message).
fn normalize_hint(value: Option<&str>, what: &str) -> Result<Option<String>, String> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) if s.chars().count() > MAX_ASSIGNEE_HINT_LEN => Err(format!(
            "{what} hint too long ({} chars); the limit is {MAX_ASSIGNEE_HINT_LEN}",
            s.chars().count()
        )),
        Some(s) => Ok(Some(s.to_string())),
    }
}

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
    let summary: String = summary
        .trim()
        .chars()
        .take(MAX_PROPOSAL_SUMMARY_LEN)
        .collect();
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
        // Assignee hints (§1.9): an id OR a role, never both. Trimmed +
        // length-capped here; resolved + assign-Key gated only at accept
        // (an id/role can name an Operative hired after this card opened).
        let assignee_agent_id = normalize_hint(c.assignee_agent_id.as_deref(), "assignee")?;
        let assignee_role = normalize_hint(c.assignee_role.as_deref(), "role")?;
        if assignee_agent_id.is_some() && assignee_role.is_some() {
            return Err(format!(
                "task '{title}' sets both an assignee and a role — choose one, not both"
            ));
        }
        old_to_new.push(Some(kept.len()));
        // `after` is carried through pass 1 untouched (still an *original*
        // index); pass 2 validates + remaps it once all positions are known.
        kept.push(ChildSpec {
            title,
            priority,
            after: c.after,
            assignee_agent_id,
            assignee_role,
        });
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
    Ok(Proposal {
        summary,
        children: norm,
    })
}

/// A canonical, hashable fingerprint of a `suggest_tasks` proposal's
/// **materialization-affecting** content (relix-execution-and-issue-design
/// §1.7 — "a hash of the requested children, normalized so cosmetic
/// differences don't matter"). Two proposals that would create the *same*
/// children — same titles, priorities, `after` order, and assignee hints —
/// produce the same fingerprint regardless of JSON key order, whitespace, or
/// the human-facing `summary` (which never affects what gets created).
///
/// Used by the durable decomposition claim to detect a **plan fork**: a
/// second accept of the same card whose proposal hashes differently is
/// refused (you can't fork the plan under one accepted identity), while the
/// same fingerprint resumes / no-ops.
///
/// Input is expected to be an already-[`normalize_proposal`]d `Proposal`
/// (the form stored on the card), so the encoding below is the canonical
/// one — but the function is total over any `Proposal`.
///
/// Encoding: a unit/record-separated stream of the per-child
/// materialization fields in order, then BLAKE3-hashed (the repo's local
/// hash convention — `hex(blake3(bytes))`, as in `db.rs`). The child *count*
/// is included so a truncated/extended plan can never collide with a prefix.
pub fn proposal_fingerprint(proposal: &Proposal) -> String {
    // Field separator \u{1f} (unit sep) and record separator \u{1e} are
    // control chars that cannot appear in a normalized title (titles are
    // trimmed plain text), so the encoding is unambiguous.
    let mut buf = String::new();
    buf.push_str(&proposal.children.len().to_string());
    for c in &proposal.children {
        buf.push('\u{1e}');
        buf.push_str(&c.title);
        buf.push('\u{1f}');
        buf.push_str(c.priority.as_deref().unwrap_or(""));
        buf.push('\u{1f}');
        if let Some(a) = c.after {
            buf.push_str(&a.to_string());
        }
        buf.push('\u{1f}');
        buf.push_str(c.assignee_agent_id.as_deref().unwrap_or(""));
        buf.push('\u{1f}');
        buf.push_str(c.assignee_role.as_deref().unwrap_or(""));
    }
    hex::encode(blake3::hash(buf.as_bytes()).as_bytes())
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
    /// The Brief's billing code — cross-team cost attribution
    /// (relix-company-model §3.4/§6.6). `None` when unset; runs inherit it
    /// (or the nearest ancestor Sub-brief's) at run start. Omitted from the
    /// wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_code: Option<String>,
}

/// One billing-code bucket in a Brief-tree cost rollup
/// (relix-company-model §6.6). `billing_code` is empty (`""`) for runs with
/// no attribution. `cost_micros` is micro-USD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BillingCodeCost {
    pub billing_code: String,
    pub run_count: i64,
    pub cost_micros: i64,
}

/// The cost of a Brief and its entire Sub-brief tree over a window
/// (relix-company-model §6.6 "Cost rollup & attribution"). Summed from the
/// durable `brief_runs` ledger (real run cost, never UI data) and tenant-safe
/// (only same-Guild Briefs in the tree contribute). Counts/cost exclude
/// pre-run `refused` rows (no adapter ran). `*_micros` are micro-USD.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BriefCostRollup {
    /// The root Brief the rollup is anchored at.
    pub brief_id: String,
    /// The Guild the rollup is scoped to.
    pub tenant_id: String,
    /// Window lower bound (unix SECONDS, inclusive — matches
    /// `brief_runs.started_at`).
    pub since_secs: i64,
    /// Window upper bound (unix SECONDS, exclusive).
    pub until_secs: i64,
    /// Number of Briefs in the tree (the root + its same-Guild descendants).
    pub brief_count: i64,
    /// Total runs across the whole tree (own + descendants) in the window.
    pub run_count: i64,
    /// Total cost across the whole tree in the window.
    pub cost_micros: i64,
    /// Just the root Brief's own runs.
    pub own_run_count: i64,
    pub own_cost_micros: i64,
    /// The descendant Sub-briefs' runs (= total − own).
    pub descendant_run_count: i64,
    pub descendant_cost_micros: i64,
    /// Cost grouped by each run's stamped billing code (sorted by code;
    /// `""` = unattributed).
    pub by_billing_code: Vec<BillingCodeCost>,
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
    /// The Brief's *unresolved* blockers (Snags whose blocker isn't yet
    /// `done`), as their human-ref where set else their id — same-Guild
    /// only, so the board can render a "Blocked by X" chip without opening
    /// the detail (relix-dashboard-design §6). Populated only by the board
    /// query; the other card lists leave it empty (and it is then omitted
    /// from the wire entirely).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
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
    /// Recovery DIAGNOSIS (execution-and-issue §3.3b) for a terminal / refused
    /// Shift: a stable failure-class bucket, a retryable verdict, a small
    /// operator-facing retry budget, and a recommended action + route. `None`
    /// on a `running` row / legacy rows. Informs operator decisions — NOT an
    /// autonomous retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_budget_remaining: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_route: Option<String>,
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
    /// Company-model §6.6 — this Brief's current **delegation depth**: the
    /// longest same-Guild `spawned` parent chain up to a root (a root is `0`,
    /// its Sub-brief `1`, …). Read-only visibility so a UI can show how deep a
    /// delegation cascade sits without inventing the value.
    pub delegation_depth: usize,
    /// Company-model §6.6 — the max delegation depth a new Sub-brief link may
    /// reach (the runaway backstop). A new Sub-brief under this Brief is
    /// refused once `delegation_depth + 1` would exceed this.
    pub max_delegation_depth: usize,
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
        ChildSpec {
            title: title.into(),
            priority: None,
            after: None,
            assignee_agent_id: None,
            assignee_role: None,
        }
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
                &[ChildSpec {
                    title: "t".into(),
                    priority: Some("urgent".into()),
                    after: None,
                    assignee_agent_id: None,
                    assignee_role: None,
                }]
            )
            .is_ok()
        );
        assert!(
            normalize_proposal(
                "s",
                &[ChildSpec {
                    title: "t".into(),
                    priority: Some("meh".into()),
                    after: None,
                    assignee_agent_id: None,
                    assignee_role: None,
                }]
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
        ChildSpec {
            title: title.into(),
            priority: None,
            after,
            assignee_agent_id: None,
            assignee_role: None,
        }
    }

    // Helper: a child carrying an explicit assignee hint (id or role).
    fn child_hint(title: &str, agent: Option<&str>, role: Option<&str>) -> ChildSpec {
        ChildSpec {
            title: title.into(),
            priority: None,
            after: None,
            assignee_agent_id: agent.map(str::to_string),
            assignee_role: role.map(str::to_string),
        }
    }

    #[test]
    fn proposal_carries_and_trims_assignee_hints() {
        let p = normalize_proposal(
            "s",
            &[
                child_hint("by id", Some("  agt_eng_1  "), None),
                child_hint("by role", None, Some("  engineer  ")),
                child("plain"),
            ],
        )
        .expect("valid");
        assert_eq!(
            p.children[0].assignee_agent_id.as_deref(),
            Some("agt_eng_1")
        );
        assert_eq!(p.children[0].assignee_role, None);
        assert_eq!(p.children[1].assignee_role.as_deref(), Some("engineer"));
        assert_eq!(p.children[1].assignee_agent_id, None);
        // A plain child keeps the unassigned default.
        assert_eq!(p.children[2].assignee_agent_id, None);
        assert_eq!(p.children[2].assignee_role, None);
    }

    #[test]
    fn proposal_rejects_both_assignee_and_role_on_one_child() {
        let out = normalize_proposal(
            "s",
            &[child_hint("conflict", Some("agt_eng_1"), Some("engineer"))],
        );
        assert!(
            out.is_err(),
            "an id AND a role on one child must be refused"
        );
    }

    #[test]
    fn proposal_rejects_over_long_assignee_hint() {
        let long = "x".repeat(MAX_ASSIGNEE_HINT_LEN + 1);
        assert!(normalize_proposal("s", &[child_hint("t", Some(&long), None)]).is_err());
        assert!(normalize_proposal("s", &[child_hint("t", None, Some(&long))]).is_err());
    }

    #[test]
    fn proposal_empty_assignee_hint_normalizes_to_none() {
        let p = normalize_proposal("s", &[child_hint("t", Some("   "), None)]).expect("valid");
        assert_eq!(p.children[0].assignee_agent_id, None);
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
            normalize_proposal("p", &[child_after("A", Some(1)), child_after("B", None)]).is_err()
        );
        // Self reference (#0 → #0) is refused.
        assert!(normalize_proposal("p", &[child_after("A", Some(0))]).is_err());
        // Out-of-range reference is refused.
        assert!(
            normalize_proposal("p", &[child_after("A", None), child_after("B", Some(9))]).is_err()
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
    fn fingerprint_ignores_cosmetic_summary_but_tracks_children() {
        // Same children, different summary → SAME fingerprint (summary is
        // cosmetic and never affects what gets materialized).
        let a = normalize_proposal("Plan A", &[child("First"), child("Second")]).unwrap();
        let b = normalize_proposal(
            "totally different wording",
            &[child("First"), child("Second")],
        )
        .unwrap();
        assert_eq!(proposal_fingerprint(&a), proposal_fingerprint(&b));
        // A different child set → DIFFERENT fingerprint.
        let c = normalize_proposal("Plan A", &[child("First"), child("Third")]).unwrap();
        assert_ne!(proposal_fingerprint(&a), proposal_fingerprint(&c));
        // A truncated plan must not collide with a prefix of the longer one.
        let d = normalize_proposal("Plan A", &[child("First")]).unwrap();
        assert_ne!(proposal_fingerprint(&a), proposal_fingerprint(&d));
    }

    #[test]
    fn fingerprint_tracks_priority_after_and_hints() {
        let base = normalize_proposal("p", &[child("A"), child("B")]).unwrap();
        // Priority change forks the fingerprint.
        let prio = normalize_proposal(
            "p",
            &[
                child("A"),
                ChildSpec {
                    title: "B".into(),
                    priority: Some("high".into()),
                    after: None,
                    assignee_agent_id: None,
                    assignee_role: None,
                },
            ],
        )
        .unwrap();
        assert_ne!(proposal_fingerprint(&base), proposal_fingerprint(&prio));
        // An `after` edge forks the fingerprint.
        let after =
            normalize_proposal("p", &[child_after("A", None), child_after("B", Some(0))]).unwrap();
        assert_ne!(proposal_fingerprint(&base), proposal_fingerprint(&after));
        // An assignee hint forks the fingerprint.
        let hinted =
            normalize_proposal("p", &[child("A"), child_hint("B", Some("agt_eng_1"), None)])
                .unwrap();
        assert_ne!(proposal_fingerprint(&base), proposal_fingerprint(&hinted));
        // Deterministic: hashing the same proposal twice is stable.
        assert_eq!(proposal_fingerprint(&base), proposal_fingerprint(&base));
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
