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

use super::{CoordinatorError, TaskStore, brief};

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
}
