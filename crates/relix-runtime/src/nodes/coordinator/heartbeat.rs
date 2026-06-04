//! The **heartbeat / assignment loop** core (Phase 3).
//!
//! This module holds the *pure, testable* selection-and-claim step
//! of the loop, decoupled from the actual outbound dispatch (which
//! wakes an Operative to run a Brief over the AI / delegation
//! path). One tick:
//!
//!   1. read the ready Briefs (`brief.ready` — assigned, active,
//!      unblocked, unclaimed), priority-ordered;
//!   2. atomically **Claim** each for its own assignee, so exactly
//!      one tick / coordinator instance ever dispatches a given
//!      Brief (single-owner);
//!   3. return the Briefs we won — *those* are the ones to
//!      dispatch this tick.
//!
//! The caller does the heavy part (running the agent) with the
//! returned batch, then heartbeats / releases the Claim. Keeping
//! the claim core here means the loop's correctness is unit-tested
//! without standing up the outbound mesh path.

use std::sync::Arc;

use super::{CoordinatorError, TaskStore, brief};
use crate::rig::bridge::BridgeTokenStore;
use crate::rig::{Rig, RigOutcome, RigRunRequest};

/// The default lease a dispatch tick takes on a claimed Brief. The
/// dispatcher must heartbeat (`TaskStore::heartbeat_claim`) before
/// this elapses, or the Brief becomes reclaimable (so a crashed
/// dispatcher's work is picked up by the next tick).
pub const DEFAULT_DISPATCH_LEASE_SECS: i64 = 300;

/// Bridge-back methods a Rig may use during one Shift. Keep this
/// list narrow: it is the difference between "agent can report work
/// on its Brief" and "leaked token can mutate the whole company."
pub const BRIDGE_BACK_SHIFT_METHODS: &[&str] = &[
    "brief.comment",
    "brief.subbrief",
    "brief.dossier_add",
    "brief.set_snags",
    "brief.claim_holder",
    "brief.clearance_request",
];

/// Run one selection-and-claim tick over the ready Briefs.
///
/// For each Brief ready to work, atomically claim it for its
/// assignee and collect the ones we won — the Briefs to dispatch
/// this tick. Briefs already held by a live Claim are skipped
/// (another tick / coordinator owns them). Briefs with no assignee
/// are skipped defensively (the readiness query already requires
/// one).
///
/// `batch` caps how many ready Briefs we consider; `lease_secs` is
/// the Claim lease length. Pure over the store — no outbound I/O.
pub fn claim_ready_batch(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
    let ready = store.list_ready_briefs(batch)?;
    let mut claimed = Vec::with_capacity(ready.len());
    for card in ready {
        let Some(assignee) = card.assignee_agent_id.as_deref() else {
            continue;
        };
        let execution_run_id = format!("shift_{}", uuid::Uuid::new_v4());
        if store.claim_brief_for_run(
            &card.task_id,
            assignee,
            lease_secs,
            Some(&execution_run_id),
        )? {
            claimed.push(card);
        }
    }
    Ok(claimed)
}

/// What one dispatched Brief produced this tick.
#[derive(Clone, Debug)]
pub struct DispatchRecord {
    /// The Brief that was dispatched.
    pub brief_id: String,
    /// The Rig that ran it (empty if none resolved).
    pub rig: String,
    /// The Rig's outcome.
    pub outcome: RigOutcome,
}

/// Verdict of the per-Brief Allowance / budget admission gate
/// (relix-company-model §3.6 "Budgets" + §5.2D autonomy/budget): the
/// company operating system must not keep dispatching work when the
/// assigned Operative is over its hard budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetAdmission {
    /// The Operative may run this Brief.
    Allow,
    /// The Operative is over budget / hard-stopped. `reason` is the
    /// operator-facing explanation chronicled on the Brief.
    Refuse { reason: String },
}

/// One US cent expressed in micro-USD (the metrics cost unit).
pub const MICROS_PER_CENT: u64 = 10_000;

/// Pure per-Operative monthly Allowance verdict.
///
/// - `allowance_cents`: the Operative's configured monthly cap
///   (`AgentProfile.monthly_allowance_cents`); `None` = no per-agent
///   Allowance, so this gate allows.
/// - `spend_micros`: the Operative's spend over the window, in
///   micro-USD (from the metrics ledger).
///
/// A cap of `0` (or negative) is an explicit **hard-stop** — the
/// Operative is budgeted to nothing and must not run. A positive cap
/// refuses once spend reaches it. (1 cent = [`MICROS_PER_CENT`]
/// micro-USD.)
pub fn allowance_admits(allowance_cents: Option<i64>, spend_micros: u64) -> BudgetAdmission {
    match allowance_cents {
        None => BudgetAdmission::Allow,
        Some(c) if c <= 0 => BudgetAdmission::Refuse {
            reason: "allowance=0 (hard-stopped)".to_string(),
        },
        Some(c) => {
            let cap_micros = (c as u64).saturating_mul(MICROS_PER_CENT);
            if spend_micros >= cap_micros {
                BudgetAdmission::Refuse {
                    reason: format!(
                        "over monthly allowance (used {spend_micros}u >= cap {cap_micros}u)"
                    ),
                }
            } else {
                BudgetAdmission::Allow
            }
        }
    }
}

/// Run one full dispatch tick: claim the ready Briefs, run each on
/// its Rig, advance the board, and release the Claim.
///
/// For each claimed Brief:
///   - resolve its Rig via `resolve_rig` (the assignee's chosen
///     backend, or the Guild default — the caller owns that lookup
///     so this stays decoupled from the agent store);
///   - if it has a Rig: move `todo → in_progress` (work has
///     started), run the Rig with the prompt from `build_prompt`,
///     then advance by outcome — `Done` → `in_review`, an
///     unrecoverable `Failed` (`retryable: false`) → `blocked` (so
///     it isn't re-dispatched forever), a `Continue` stays
///     `in_progress` and chronicles its note for the next Shift, a
///     retryable failure stays `in_progress` for the next tick;
///   - if no Rig resolves: record a `Failed` outcome and leave the
///     board untouched (nothing ran — it re-appears next tick / the
///     Desk surfaces it);
///   - always release the Claim afterwards, so a continuation or
///     the next tick can pick the Brief up.
///
/// The board transitions are always valid by construction (the
/// ready set is `todo`/`in_progress`), so they propagate real DB
/// errors but never an illegal-transition error.
pub fn dispatch_batch<R, P>(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
    bridge_tokens: Option<&BridgeTokenStore>,
    resolve_rig: R,
    build_prompt: P,
) -> Result<Vec<DispatchRecord>, CoordinatorError>
where
    R: Fn(&brief::BriefCard) -> Option<Arc<dyn Rig>>,
    P: Fn(&brief::BriefCard) -> String,
{
    dispatch_batch_with_policy(
        store,
        batch,
        lease_secs,
        bridge_tokens,
        |_| true,
        |_| 20,
        // No budget gate for the simple wrapper (tests / old callers).
        |_| BudgetAdmission::Allow,
        resolve_rig,
        build_prompt,
    )
}

/// Policy-aware dispatch tick used by the live controller. The
/// default [`dispatch_batch`] keeps tests and old callers simple;
/// this variant lets production wiring enforce per-agent runtime
/// Keys before queueing timer wakes and before claiming queued runs.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_batch_with_policy<R, P, A, C, B>(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
    bridge_tokens: Option<&BridgeTokenStore>,
    allow_timer_wakeup: A,
    max_running_for_agent: C,
    admit_budget: B,
    resolve_rig: R,
    build_prompt: P,
) -> Result<Vec<DispatchRecord>, CoordinatorError>
where
    R: Fn(&brief::BriefCard) -> Option<Arc<dyn Rig>>,
    P: Fn(&brief::BriefCard) -> String,
    A: Fn(&brief::BriefCard) -> bool,
    C: FnMut(&str) -> i64,
    B: Fn(&brief::BriefCard) -> BudgetAdmission,
{
    let ready = store.list_ready_briefs(batch)?;
    for card in &ready {
        let Some(assignee) = card.assignee_agent_id.as_deref() else {
            continue;
        };
        if allow_timer_wakeup(card) {
            let _ =
                store.request_brief_wakeup(&card.task_id, assignee, "timer", "heartbeat", None)?;
        }
    }
    let claimed = store.claim_queued_wakeups_with_caps(batch, lease_secs, max_running_for_agent)?;
    let mut records = Vec::with_capacity(claimed.len());
    for claimed_wake in claimed {
        let card = claimed_wake.card;
        let wakeup_id = claimed_wake.wakeup.wakeup_id;
        // PHASE 4 (Allowance hard-stop, relix-company-model §3.6/§5.2D):
        // before running the Brief, check the assigned Operative is
        // within budget. If over budget / hard-stopped, do NOT run it
        // and do NOT silently skip — park it in `blocked` (visible to
        // the operator), chronicle WHY, finish the wakeup, and release
        // the Claim so the lease is not leaked.
        if let BudgetAdmission::Refuse { reason } = admit_budget(&card) {
            // `todo -> blocked` is illegal; mirror the dispatch path's
            // `todo -> in_progress -> blocked` so the park is valid.
            if card.board_status == "todo" {
                store.set_board_status(&card.task_id, "in_progress")?;
            }
            store.set_board_status(&card.task_id, "blocked")?;
            let _ = store.append_event(&card.task_id, "brief.budget_refused", &reason);
            let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
            if let Some(assignee) = card.assignee_agent_id.as_deref() {
                store.release_claim(&card.task_id, assignee)?;
            }
            records.push(DispatchRecord {
                brief_id: card.task_id.clone(),
                rig: String::new(),
                outcome: RigOutcome::Failed {
                    reason,
                    retryable: false,
                },
            });
            continue;
        }
        let record = match resolve_rig(&card) {
            Some(rig) => {
                // Work starts: todo → in_progress.
                if card.board_status == "todo" {
                    store.set_board_status(&card.task_id, "in_progress")?;
                }
                let assignee = card.assignee_agent_id.clone().unwrap_or_default();
                // Mint a scoped bridge-back token for this Shift so the
                // agent can call Relix back; it dies with the Shift.
                let token = bridge_tokens
                    .map(|bt| {
                        bt.mint_scoped(
                            &card.task_id,
                            &assignee,
                            "",
                            lease_secs,
                            BRIDGE_BACK_SHIFT_METHODS
                                .iter()
                                .map(|m| (*m).to_string())
                                .collect(),
                        )
                    })
                    .unwrap_or_default();
                let req =
                    RigRunRequest::new(&card.task_id, assignee, String::new(), build_prompt(&card))
                        .with_bridge_token(&token)
                        .with_context(brief_context(&card));
                let outcome = rig.run(&req);
                if let Some(bt) = bridge_tokens
                    && !token.is_empty()
                {
                    bt.revoke(&token);
                }
                // Advance the board by outcome. The Brief is now
                // `in_progress` (we either moved it or it already
                // was), so both transitions below are legal.
                match &outcome {
                    // Done → review for a human / supervisor, and
                    // chronicle the result summary so the reviewer
                    // sees what the Shift produced.
                    RigOutcome::Done { summary } => {
                        match store.set_board_status(&card.task_id, "in_review") {
                            Ok(_) => {
                                let _ = store.finish_wakeup(&wakeup_id, "completed", Some(summary));
                                let _ =
                                    store.append_event(&card.task_id, "brief.shift_done", summary);
                            }
                            Err(CoordinatorError::Invalid(reason))
                                if reason.contains("reviewer required") =>
                            {
                                store.set_board_status(&card.task_id, "blocked")?;
                                let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
                                let _ = store.append_event(
                                    &card.task_id,
                                    "brief.dispatch_failed",
                                    &format!("reviewer required before review: {summary}"),
                                );
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    // Unrecoverable failure → park in `blocked` for
                    // attention rather than re-dispatching it forever,
                    // and chronicle WHY so the Desk shows the reason.
                    RigOutcome::Failed {
                        retryable: false,
                        reason,
                    } => {
                        store.set_board_status(&card.task_id, "blocked")?;
                        let _ = store.finish_wakeup(&wakeup_id, "failed", Some(reason));
                        let _ = store.append_event(&card.task_id, "brief.dispatch_failed", reason);
                    }
                    // Durable yield → stay `in_progress` and
                    // chronicle the note so the NEXT Shift resumes
                    // with the continuation context.
                    RigOutcome::Continue { note } => {
                        let _ = store.finish_wakeup(&wakeup_id, "continued", Some(note));
                        let _ = store.append_event(&card.task_id, "brief.continued", note);
                    }
                    // Retryable failure → leave it `in_progress`; the
                    // next tick picks it back up.
                    RigOutcome::Failed {
                        retryable: true,
                        reason,
                    } => {
                        let _ = store.finish_wakeup(&wakeup_id, "failed", Some(reason));
                    }
                }
                DispatchRecord {
                    brief_id: card.task_id.clone(),
                    rig: rig.name().to_string(),
                    outcome,
                }
            }
            None => {
                let reason = "no Rig configured and no Guild default".to_string();
                let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
                DispatchRecord {
                    brief_id: card.task_id.clone(),
                    rig: String::new(),
                    outcome: RigOutcome::Failed {
                        reason,
                        retryable: false,
                    },
                }
            }
        };
        // Always release the Claim after the tick.
        if let Some(assignee) = card.assignee_agent_id.as_deref() {
            store.release_claim(&card.task_id, assignee)?;
        }
        records.push(record);
    }
    Ok(records)
}

/// Structured result of a manual, synchronous **run** of one Brief —
/// the dashboard "Start / Run" path (`brief.run`). Unlike the timer
/// loop, this runs immediately and reports a clear outcome, including
/// the adapter-unavailable states (so the UI never fakes a run).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunReport {
    pub brief_id: String,
    /// `done` / `failed` / `continued` — real run outcomes; or
    /// `not_found` / `unassigned` / `no_adapter` / `adapter_unavailable`
    /// / `already_running` — pre-run refusals (no command was spawned).
    pub status: String,
    /// The adapter (Rig) that ran it, empty when none resolved.
    pub rig: String,
    /// Result summary (Done) or reason (Failed / refusal). Already
    /// secret-redacted by the Rig before it reaches here.
    pub summary: String,
    /// Install hint when the adapter is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}

impl RunReport {
    fn refuse(brief_id: &str, status: &str, summary: impl Into<String>) -> Self {
        Self {
            brief_id: brief_id.to_string(),
            status: status.to_string(),
            rig: String::new(),
            summary: summary.into(),
            install_hint: None,
        }
    }
}

/// Run ONE Brief synchronously through its Operative's Rig — the manual
/// "Start" action. Resolves the adapter, refuses clearly when it is
/// unavailable (never spawns), claims the Brief to block a duplicate
/// concurrent run, runs the Rig, advances the board, and chronicles the
/// result (tagged with the adapter name) exactly like the timer loop.
#[allow(clippy::too_many_arguments)]
pub fn run_brief_now(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
) -> Result<RunReport, CoordinatorError> {
    let Some(card) = store.brief_card(brief_id)? else {
        return Ok(RunReport::refuse(brief_id, "not_found", "brief not found"));
    };
    let Some(assignee) = card.assignee_agent_id.clone() else {
        return Ok(RunReport::refuse(
            brief_id,
            "unassigned",
            "assign an Operative before running",
        ));
    };
    let Some(rig) = registry.resolve(preferred_rig) else {
        return Ok(RunReport::refuse(
            brief_id,
            "no_adapter",
            "the Operative has no Rig and no Guild default is configured",
        ));
    };
    // Live availability probe — never spawn an adapter that isn't there.
    let probe = rig.probe();
    if probe.status != "available" {
        return Ok(RunReport {
            brief_id: brief_id.to_string(),
            status: "adapter_unavailable".to_string(),
            rig: rig.name().to_string(),
            summary: probe.detail,
            install_hint: probe.install_hint,
        });
    }
    // Single-owner: claim the Brief so a duplicate concurrent run can't
    // start. A live claim by another run → refuse.
    let run_id = format!("run_{}", uuid::Uuid::new_v4());
    if !store.claim_brief_for_run(&card.task_id, &assignee, lease_secs, Some(&run_id))? {
        return Ok(RunReport {
            brief_id: brief_id.to_string(),
            status: "already_running".to_string(),
            rig: rig.name().to_string(),
            summary: "another run holds the Claim on this Brief".to_string(),
            install_hint: None,
        });
    }
    let rig_name = rig.name().to_string();
    if card.board_status == "todo" {
        store.set_board_status(&card.task_id, "in_progress")?;
    }
    let _ = store.append_event(
        &card.task_id,
        "brief.run_started",
        &format!("[{rig_name}] run {run_id}"),
    );
    // Scoped per-run bridge-back token (dies with the run).
    let token = bridge_tokens
        .map(|bt| {
            bt.mint_scoped(
                &card.task_id,
                &assignee,
                "",
                lease_secs,
                BRIDGE_BACK_SHIFT_METHODS
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect(),
            )
        })
        .unwrap_or_default();
    let req = RigRunRequest::new(&card.task_id, &assignee, String::new(), prompt)
        .with_bridge_token(&token)
        .with_context(brief_context(&card));
    let outcome = rig.run(&req);
    if let Some(bt) = bridge_tokens
        && !token.is_empty()
    {
        bt.revoke(&token);
    }
    let (status, summary) = match &outcome {
        RigOutcome::Done { summary } => {
            // Done → in_review (best-effort; a missing reviewer parks it).
            if store.set_board_status(&card.task_id, "in_review").is_err() {
                let _ = store.set_board_status(&card.task_id, "blocked");
            }
            let _ = store.append_event(
                &card.task_id,
                "brief.shift_done",
                &format!("[{rig_name}] {summary}"),
            );
            ("done", summary.clone())
        }
        RigOutcome::Failed {
            retryable: false,
            reason,
        } => {
            let _ = store.set_board_status(&card.task_id, "blocked");
            let _ = store.append_event(
                &card.task_id,
                "brief.dispatch_failed",
                &format!("[{rig_name}] {reason}"),
            );
            ("failed", reason.clone())
        }
        RigOutcome::Failed {
            retryable: true,
            reason,
        } => {
            let _ = store.append_event(
                &card.task_id,
                "brief.dispatch_failed",
                &format!("[{rig_name}] {reason}"),
            );
            ("failed", reason.clone())
        }
        RigOutcome::Continue { note } => {
            let _ = store.append_event(
                &card.task_id,
                "brief.continued",
                &format!("[{rig_name}] {note}"),
            );
            ("continued", note.clone())
        }
    };
    let _ = store.release_claim(&card.task_id, &assignee);
    Ok(RunReport {
        brief_id: card.task_id,
        status: status.to_string(),
        rig: rig_name,
        summary,
        install_hint: None,
    })
}

/// Build the opaque `context` string handed to the Rig: where the
/// Brief sits on the spine (priority + Mandate/Campaign links), so
/// the agent backend knows the work's place in the company without
/// a separate lookup.
fn brief_context(card: &brief::BriefCard) -> String {
    let mut parts = vec![format!("priority={}", card.priority)];
    if let Some(m) = &card.mandate_id {
        parts.push(format!("mandate={m}"));
    }
    if let Some(c) = &card.campaign_id {
        parts.push(format!("campaign={c}"));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::coordinator::RetryPolicy;

    fn store() -> TaskStore {
        TaskStore::in_memory().unwrap()
    }

    fn ready_brief(s: &TaskStore, title: &str, assignee: &str) -> String {
        let id = s
            .create(
                title,
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_brief_field(&id, "assignee", assignee).unwrap();
        s.set_brief_field(&id, "reviewer", "reviewer_1").unwrap();
        s.set_board_status(&id, "todo").unwrap();
        id
    }

    #[test]
    fn claim_ready_batch_claims_each_ready_brief_once_for_its_assignee() {
        let s = store();
        let a = ready_brief(&s, "a", "agt_a");
        let b = ready_brief(&s, "b", "agt_b");

        // First tick claims both, for their respective assignees.
        let first = claim_ready_batch(&s, 50, 300).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(s.claim_holder(&a).unwrap().unwrap().0, "agt_a");
        assert_eq!(s.claim_holder(&b).unwrap().unwrap().0, "agt_b");

        // Second tick wins nothing — both are held by a live Claim.
        assert!(claim_ready_batch(&s, 50, 300).unwrap().is_empty());
    }

    #[test]
    fn claim_ready_batch_skips_unassigned_blocked_and_done() {
        let s = store();
        let live = ready_brief(&s, "live", "agt_x");

        // Unassigned: not ready, never dispatched.
        let unassigned = s
            .create(
                "u",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_board_status(&unassigned, "todo").unwrap();

        // Blocked: ready query excludes it.
        let blocked = ready_brief(&s, "blocked", "agt_y");
        let blocker = s
            .create(
                "blk",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.add_snag(&blocked, &blocker).unwrap();

        let dispatched: Vec<String> = claim_ready_batch(&s, 50, 300)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(dispatched.contains(&live));
        assert!(!dispatched.contains(&unassigned));
        assert!(!dispatched.contains(&blocked));
    }

    #[test]
    fn an_expired_lease_lets_the_next_tick_reclaim() {
        let s = store();
        let id = ready_brief(&s, "a", "agt_a");
        assert_eq!(claim_ready_batch(&s, 50, 300).unwrap().len(), 1);

        // Backdate the lease into the past — the dispatcher "crashed".
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET claim_expires_at = 100 WHERE task_id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        }
        // The next tick reclaims and re-dispatches it.
        let again: Vec<String> = claim_ready_batch(&s, 50, 300)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(again.contains(&id));
    }

    #[test]
    fn dispatch_batch_runs_each_brief_on_its_rig_and_advances_the_board() {
        use crate::rig::RigRegistry;
        let s = store();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "write docs", "agt_a"); // starts in todo

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].brief_id, a);
        assert_eq!(records[0].rig, "echo");
        assert!(matches!(records[0].outcome, RigOutcome::Done { .. }));

        // Board advanced todo → in_progress → in_review; Claim released.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_review"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        // The Shift result was chronicled for the reviewer.
        let done = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.shift_done"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(done.len(), 1);
        assert!(
            done[0].payload.contains("write docs"),
            "got {:?}",
            done[0].payload
        );
        // No longer ready, so a second tick does nothing.
        assert!(s.list_ready_briefs(50).unwrap().is_empty());
        assert!(
            dispatch_batch(
                &s,
                50,
                300,
                None,
                |_: &brief::BriefCard| reg.get("echo"),
                |c: &brief::BriefCard| c.title.clone(),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn dispatch_policy_can_disable_timer_wake_for_ready_brief() {
        use crate::rig::RigRegistry;
        let s = store();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "do not wake", "agt_a");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| false,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert!(records.is_empty());
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("todo"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        assert!(s.list_brief_wakeups(&a, 10).unwrap().is_empty());
    }

    #[test]
    fn dispatch_policy_honors_per_agent_concurrency_cap() {
        use crate::rig::RigRegistry;
        let s = store();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "a", "agt_a");
        let b = ready_brief(&s, "b", "agt_a");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |agent| if agent == "agt_a" { 1 } else { 20 },
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let done_id = records[0].brief_id.clone();
        let queued_id = if done_id == a { b } else { a };
        assert_eq!(
            s.board_status(&done_id).unwrap().as_deref(),
            Some("in_review")
        );
        assert_eq!(s.board_status(&queued_id).unwrap().as_deref(), Some("todo"));
        let queued_rows = s.list_brief_wakeups(&queued_id, 10).unwrap();
        assert_eq!(queued_rows.len(), 1);
        assert_eq!(queued_rows[0].status, "queued");
    }

    // ── Allowance / budget hard-stop (company-model §3.6/§5.2D) ──

    #[test]
    fn allowance_admits_pure_verdicts() {
        // No per-agent Allowance configured → always allowed.
        assert_eq!(allowance_admits(None, 999_999_999), BudgetAdmission::Allow);
        // Explicit zero (or negative) Allowance → hard-stop regardless
        // of spend (even with zero recorded spend).
        assert!(matches!(
            allowance_admits(Some(0), 0),
            BudgetAdmission::Refuse { .. }
        ));
        assert!(matches!(
            allowance_admits(Some(-5), 0),
            BudgetAdmission::Refuse { .. }
        ));
        // Positive cap: 100 cents = 1_000_000 micro-USD.
        // Under the cap → allowed.
        assert_eq!(allowance_admits(Some(100), 999_999), BudgetAdmission::Allow);
        // At/over the cap → refused.
        assert!(matches!(
            allowance_admits(Some(100), 1_000_000),
            BudgetAdmission::Refuse { .. }
        ));
        assert!(matches!(
            allowance_admits(Some(100), 5_000_000),
            BudgetAdmission::Refuse { .. }
        ));
    }

    #[test]
    fn over_budget_operative_is_refused_parked_and_chronicled() {
        use crate::rig::RigRegistry;
        let s = store();
        let reg = RigRegistry::with_builtins();
        let refused = ready_brief(&s, "refused work", "agt_broke");
        let allowed = ready_brief(&s, "allowed work", "agt_ok");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            // Refuse only the over-budget Operative; mirror the live
            // closure's payload shape.
            |card: &brief::BriefCard| {
                if card.assignee_agent_id.as_deref() == Some("agt_broke") {
                    BudgetAdmission::Refuse {
                        reason: "budget_refused: agent_id=agt_broke allowance=0c used=0u \
                                 reason=allowance=0 (hard-stopped)"
                            .to_string(),
                    }
                } else {
                    BudgetAdmission::Allow
                }
            },
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        // The refused Brief did NOT run: it's parked in `blocked`
        // (visible to the operator), never reaching `in_review`.
        assert_eq!(
            s.board_status(&refused).unwrap().as_deref(),
            Some("blocked")
        );
        // It was NOT silently skipped — a chronicle event explains why.
        let refusal = s
            .query_events(
                &refused,
                0,
                50,
                Some("brief.budget_refused"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(refusal.len(), 1);
        assert!(
            refusal[0].payload.contains("budget_refused")
                && refusal[0].payload.contains("agt_broke"),
            "got {:?}",
            refusal[0].payload
        );
        // The Claim lease is released (not leaked) and the wakeup
        // closed as failed.
        assert!(s.claim_holder(&refused).unwrap().is_none());
        assert!(records.iter().any(|r| r.brief_id == refused
            && matches!(
                r.outcome,
                RigOutcome::Failed {
                    retryable: false,
                    ..
                }
            )));

        // The under-budget Operative still dispatches normally.
        assert_eq!(
            s.board_status(&allowed).unwrap().as_deref(),
            Some("in_review")
        );
    }

    #[test]
    fn dispatch_batch_fails_a_brief_with_no_rig_and_leaves_the_board() {
        let s = store();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| None,
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].outcome, RigOutcome::Failed { .. }));
        // Nothing ran → board untouched (still todo); Claim released.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("todo"));
        assert!(s.claim_holder(&a).unwrap().is_none());
    }

    #[test]
    fn dispatch_batch_parks_an_unrecoverable_failure_in_blocked() {
        // A Rig that always fails non-retryably.
        struct DeadRig;
        impl Rig for DeadRig {
            fn name(&self) -> &str {
                "dead"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Failed {
                    reason: "boom".to_string(),
                    retryable: false,
                }
            }
        }
        let s = store();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(DeadRig);

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].outcome,
            RigOutcome::Failed {
                retryable: false,
                ..
            }
        ));
        // Started then failed unrecoverably → parked in blocked,
        // Claim released, and no longer ready (won't re-dispatch).
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("blocked"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        assert!(s.list_ready_briefs(50).unwrap().is_empty());
        // The reason is chronicled so the Desk can show why.
        let events = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.dispatch_failed"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, "boom");
    }

    #[test]
    fn dispatch_batch_leaves_a_retryable_failure_in_progress() {
        struct FlakyRig;
        impl Rig for FlakyRig {
            fn name(&self) -> &str {
                "flaky"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Failed {
                    reason: "transient".to_string(),
                    retryable: true,
                }
            }
        }
        let s = store();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(FlakyRig);
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        // Retryable → stays in_progress so the next tick retries it.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_progress"));
    }

    #[test]
    fn dispatch_batch_chronicles_a_continue_note_and_stays_in_progress() {
        struct YieldRig;
        impl Rig for YieldRig {
            fn name(&self) -> &str {
                "yield"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Continue {
                    note: "waiting on review".to_string(),
                }
            }
        }
        let s = store();
        let a = ready_brief(&s, "a", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(YieldRig);
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        // Stays in_progress (resumable) and the note is chronicled.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_progress"));
        let events = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.continued"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload, "waiting on review");
    }

    #[test]
    fn dispatch_batch_hands_the_rig_the_brief_spine_context() {
        use std::sync::{Arc, Mutex};

        struct CtxRig(Arc<Mutex<String>>);
        impl Rig for CtxRig {
            fn name(&self) -> &str {
                "ctx"
            }
            fn run(&self, req: &RigRunRequest) -> RigOutcome {
                *self.0.lock().unwrap() = req.context.clone();
                RigOutcome::Done {
                    summary: "ok".to_string(),
                }
            }
        }

        let s = store();
        let a = ready_brief(&s, "a", "agt_a");
        s.set_brief_field(&a, "priority", "high").unwrap();
        s.set_brief_field(&a, "mandate", "mandate_x").unwrap();
        s.set_brief_field(&a, "campaign", "camp_y").unwrap();

        let seen = Arc::new(Mutex::new(String::new()));
        let rig: Arc<dyn Rig> = Arc::new(CtxRig(seen.clone()));
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        let ctx = seen.lock().unwrap().clone();
        assert!(ctx.contains("priority=high"), "ctx: {ctx}");
        assert!(ctx.contains("mandate=mandate_x"), "ctx: {ctx}");
        assert!(ctx.contains("campaign=camp_y"), "ctx: {ctx}");
    }

    #[test]
    fn dispatch_batch_mints_and_revokes_a_bridge_token_per_run() {
        use std::sync::{Arc, Mutex};

        // A Rig that records the bridge token it was handed.
        struct RecordingRig {
            token: Arc<Mutex<String>>,
            allowed: Arc<Mutex<bool>>,
            denied: Arc<Mutex<bool>>,
            tokens: BridgeTokenStore,
        }
        impl Rig for RecordingRig {
            fn name(&self) -> &str {
                "recorder"
            }
            fn run(&self, req: &RigRunRequest) -> RigOutcome {
                *self.token.lock().unwrap() = req.bridge_token.clone();
                *self.allowed.lock().unwrap() = self.tokens.authorize_method(
                    &req.bridge_token,
                    &req.brief_id,
                    &req.agent_id,
                    "brief.comment",
                );
                *self.denied.lock().unwrap() = !self.tokens.authorize_method(
                    &req.bridge_token,
                    &req.brief_id,
                    &req.agent_id,
                    "agent.delete",
                );
                RigOutcome::Done {
                    summary: "ok".to_string(),
                }
            }
        }

        let s = store();
        let _a = ready_brief(&s, "a", "agt_a");
        let tokens = BridgeTokenStore::new();
        let seen = Arc::new(Mutex::new(String::new()));
        let allowed = Arc::new(Mutex::new(false));
        let denied = Arc::new(Mutex::new(false));
        let rig: Arc<dyn Rig> = Arc::new(RecordingRig {
            token: seen.clone(),
            allowed: allowed.clone(),
            denied: denied.clone(),
            tokens: tokens.clone(),
        });

        let records = dispatch_batch(
            &s,
            50,
            300,
            Some(&tokens),
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);

        // A token was minted and handed to the Rig during the run…
        let handed = seen.lock().unwrap().clone();
        assert!(handed.starts_with("brt_"), "got: {handed:?}");
        assert!(
            *allowed.lock().unwrap(),
            "token must permit run-scoped bridge-back methods"
        );
        assert!(
            *denied.lock().unwrap(),
            "token must deny unrelated bridge methods"
        );
        // …and revoked when the Shift ended.
        assert!(tokens.is_empty(), "token should be revoked after the run");
    }

    // ── Synchronous run_brief_now (the dashboard "Start") ────────

    fn echo_registry() -> crate::rig::RigRegistry {
        crate::rig::RigRegistry::with_builtins().with_default("echo")
    }

    #[test]
    fn run_brief_now_runs_on_echo_and_moves_to_review() {
        let s = store();
        let id = ready_brief(&s, "Write the readme", "agt_a");
        let reg = echo_registry();
        let report =
            run_brief_now(&s, &reg, None, 300, &id, Some("echo"), "do the work".into()).unwrap();
        assert_eq!(report.status, "done", "got: {report:?}");
        assert_eq!(report.rig, "echo");
        assert!(report.summary.contains("echo:"));
        // The board advanced to review and the run was chronicled.
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("in_review"));
        let kinds: Vec<String> = s
            .list_events_after(&id, 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.event_type)
            .collect();
        assert!(kinds.iter().any(|k| k == "brief.run_started"));
        assert!(kinds.iter().any(|k| k == "brief.shift_done"));
        // The Claim is released after the run.
        assert!(s.claim_holder(&id).unwrap().is_none());
    }

    #[test]
    fn run_brief_now_refuses_unassigned() {
        let s = store();
        let id = s
            .create("u", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        let report = run_brief_now(&s, &echo_registry(), None, 300, &id, Some("echo"), "x".into())
            .unwrap();
        assert_eq!(report.status, "unassigned");
    }

    #[test]
    fn run_brief_now_reports_no_adapter_when_none_resolves() {
        let s = store();
        let id = ready_brief(&s, "t", "agt_a");
        let empty = crate::rig::RigRegistry::new(); // no default, no rigs
        let report = run_brief_now(&s, &empty, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "no_adapter");
    }

    #[test]
    fn run_brief_now_reports_adapter_unavailable_without_spawning() {
        let s = store();
        let id = ready_brief(&s, "t", "agt_a");
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(
            crate::rig::ProcessRig::new(
                "ghost",
                "definitely-not-installed-relix-adapter-xyzzy",
                vec![],
            )
            .with_install_hint("install the ghost adapter"),
        ));
        reg.set_default(Some("ghost".to_string()));
        let report = run_brief_now(&s, &reg, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "adapter_unavailable", "got: {report:?}");
        assert_eq!(report.rig, "ghost");
        assert_eq!(report.install_hint.as_deref(), Some("install the ghost adapter"));
        // It must NOT have moved the board (no run happened).
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("todo"));
    }

    #[test]
    fn run_brief_now_reports_not_found_for_unknown_brief() {
        let s = store();
        let report =
            run_brief_now(&s, &echo_registry(), None, 300, "nope", Some("echo"), "x".into())
                .unwrap();
        assert_eq!(report.status, "not_found");
    }
}
