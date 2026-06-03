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
        if store.claim_brief(&card.task_id, assignee, lease_secs)? {
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
///     it isn't re-dispatched forever), a `Continue` / retryable
///     failure stays `in_progress` for the next tick;
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
    let claimed = claim_ready_batch(store, batch, lease_secs)?;
    let mut records = Vec::with_capacity(claimed.len());
    for card in claimed {
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
                    .map(|bt| bt.mint(&card.task_id, &assignee, "", lease_secs))
                    .unwrap_or_default();
                let req = RigRunRequest::new(
                    &card.task_id,
                    assignee,
                    String::new(),
                    build_prompt(&card),
                )
                .with_bridge_token(&token);
                let outcome = rig.run(&req);
                if let Some(bt) = bridge_tokens {
                    if !token.is_empty() {
                        bt.revoke(&token);
                    }
                }
                // Advance the board by outcome. The Brief is now
                // `in_progress` (we either moved it or it already
                // was), so both transitions below are legal.
                match &outcome {
                    // Done → review for a human / supervisor.
                    RigOutcome::Done { .. } => {
                        store.set_board_status(&card.task_id, "in_review")?;
                    }
                    // Unrecoverable failure → park in `blocked` for
                    // attention rather than re-dispatching it forever,
                    // and chronicle WHY so the Desk shows the reason.
                    RigOutcome::Failed {
                        retryable: false,
                        reason,
                    } => {
                        store.set_board_status(&card.task_id, "blocked")?;
                        let _ =
                            store.append_event(&card.task_id, "brief.dispatch_failed", reason);
                    }
                    // Continue / retryable failure → leave it
                    // `in_progress`; the next tick (or a continuation)
                    // picks it back up.
                    _ => {}
                }
                DispatchRecord {
                    brief_id: card.task_id.clone(),
                    rig: rig.name().to_string(),
                    outcome,
                }
            }
            None => DispatchRecord {
                brief_id: card.task_id.clone(),
                rig: String::new(),
                outcome: RigOutcome::Failed {
                    reason: "no Rig configured and no Guild default".to_string(),
                    retryable: false,
                },
            },
        };
        // Always release the Claim after the tick.
        if let Some(assignee) = card.assignee_agent_id.as_deref() {
            store.release_claim(&card.task_id, assignee)?;
        }
        records.push(record);
    }
    Ok(records)
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
            .create("u", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.set_board_status(&unassigned, "todo").unwrap();

        // Blocked: ready query excludes it.
        let blocked = ready_brief(&s, "blocked", "agt_y");
        let blocker = s
            .create("blk", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
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
            RigOutcome::Failed { retryable: false, .. }
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
    fn dispatch_batch_mints_and_revokes_a_bridge_token_per_run() {
        use std::sync::{Arc, Mutex};

        // A Rig that records the bridge token it was handed.
        struct RecordingRig(Arc<Mutex<String>>);
        impl Rig for RecordingRig {
            fn name(&self) -> &str {
                "recorder"
            }
            fn run(&self, req: &RigRunRequest) -> RigOutcome {
                *self.0.lock().unwrap() = req.bridge_token.clone();
                RigOutcome::Done {
                    summary: "ok".to_string(),
                }
            }
        }

        let s = store();
        let _a = ready_brief(&s, "a", "agt_a");
        let tokens = BridgeTokenStore::new();
        let seen = Arc::new(Mutex::new(String::new()));
        let rig: Arc<dyn Rig> = Arc::new(RecordingRig(seen.clone()));

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
        // …and revoked when the Shift ended.
        assert!(tokens.is_empty(), "token should be revoked after the run");
    }
}
