//! **Tradecraft** — the self-improvement loop, and **the Keeper**
//! (Pillar 1, the closed learning loop transplanted from Hermes).
//!
//! Operatives sharpen their **Tradecraft** by turning experience
//! into reusable **Knacks** (skills). The **Keeper** is the janitor
//! that keeps that library healthy: it ages Knacks on a
//! *usage-timestamp clock* (active → stale → archived), and — the
//! load-bearing safety rule — it **never deletes, only archives**,
//! and **only touches what the agent itself made** (`created_by =
//! "agent"`). A user-authored, bundled, or hub Knack, or a pinned
//! one, is left alone forever.
//!
//! This module is the Keeper's pure decision core; wiring it to a
//! real Knack store + the autonomous-creation trigger + the
//! post-response nudge are the layers above it.

/// Where a Knack sits on the usage clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnackState {
    /// In active rotation.
    Active,
    /// Unused long enough to be a candidate for consolidation, but
    /// still available.
    Stale,
    /// Aged out of rotation. **Never deleted** — recoverable.
    Archived,
}

/// The metadata the Keeper reasons over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnackMeta {
    /// Unix seconds of the Knack's last use.
    pub last_used_at: i64,
    /// Provenance — `agent` / `user` / `bundled` / `hub`. Only
    /// `agent` is auto-managed.
    pub created_by: String,
    /// Operator-pinned Knacks are exempt from aging.
    pub pinned: bool,
}

/// Default: stale after 30 days unused, archived after 90.
pub const DEFAULT_STALE_AFTER_SECS: i64 = 30 * 86_400;
pub const DEFAULT_ARCHIVE_AFTER_SECS: i64 = 90 * 86_400;

/// Is a Knack auto-managed by the Keeper? Only unpinned,
/// agent-created Knacks are — the provenance gate that stops the
/// Keeper from ever touching what a human (or a bundle/hub)
/// authored.
pub fn is_auto_managed(meta: &KnackMeta) -> bool {
    !meta.pinned && meta.created_by == "agent"
}

/// Decide a Knack's state on the usage clock. Knacks that aren't
/// auto-managed (user/bundled/hub-authored, or pinned) are always
/// reported `Active` — the Keeper leaves them be.
pub fn curate(meta: &KnackMeta, now: i64, stale_after: i64, archive_after: i64) -> KnackState {
    if !is_auto_managed(meta) {
        return KnackState::Active;
    }
    let idle = now.saturating_sub(meta.last_used_at);
    if idle >= archive_after {
        KnackState::Archived
    } else if idle >= stale_after {
        KnackState::Stale
    } else {
        KnackState::Active
    }
}

/// Convenience over [`curate`] with the default 30/90-day clock.
pub fn curate_default(meta: &KnackMeta, now: i64) -> KnackState {
    curate(
        meta,
        now,
        DEFAULT_STALE_AFTER_SECS,
        DEFAULT_ARCHIVE_AFTER_SECS,
    )
}

/// One Knack the Keeper looks at in a batch sweep: its id, the
/// metadata it reasons over, and the Knack's *current* stored state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnackRecord {
    pub id: String,
    pub meta: KnackMeta,
    pub state: KnackState,
}

/// The Keeper's batch decision: the Knacks that should *transition*,
/// grouped by their new state. Only Knacks whose target state
/// differs from their current state appear — applying an empty plan
/// is a no-op. The Keeper both ages (active→stale→archived) and
/// heals (`to_reactivate`: a Knack used again climbs back to Active).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CurationPlan {
    pub to_stale: Vec<String>,
    pub to_archive: Vec<String>,
    pub to_reactivate: Vec<String>,
}

impl CurationPlan {
    pub fn is_empty(&self) -> bool {
        self.to_stale.is_empty() && self.to_archive.is_empty() && self.to_reactivate.is_empty()
    }

    /// Total number of Knacks that change state under this plan.
    pub fn changes(&self) -> usize {
        self.to_stale.len() + self.to_archive.len() + self.to_reactivate.len()
    }
}

/// Sweep a batch of Knacks and report only the transitions. A
/// non-auto-managed Knack (user/bundled/hub-authored, or pinned)
/// always targets `Active` via [`curate`], so it can only ever be
/// *healed* out of an accidental stale/archived state, never aged
/// into one — the provenance gate holds at the batch level too.
pub fn plan_curation(
    records: &[KnackRecord],
    now: i64,
    stale_after: i64,
    archive_after: i64,
) -> CurationPlan {
    let mut plan = CurationPlan::default();
    for r in records {
        let target = curate(&r.meta, now, stale_after, archive_after);
        if target == r.state {
            continue;
        }
        match target {
            KnackState::Stale => plan.to_stale.push(r.id.clone()),
            KnackState::Archived => plan.to_archive.push(r.id.clone()),
            KnackState::Active => plan.to_reactivate.push(r.id.clone()),
        }
    }
    plan
}

/// Convenience over [`plan_curation`] with the default 30/90-day clock.
pub fn plan_curation_default(records: &[KnackRecord], now: i64) -> CurationPlan {
    plan_curation(
        records,
        now,
        DEFAULT_STALE_AFTER_SECS,
        DEFAULT_ARCHIVE_AFTER_SECS,
    )
}

/// An in-memory **Knack library** with the Keeper wired in: holds
/// Knacks (id → meta + state), advances their usage clock on a
/// sweep, and heals them on use. The persistent store layers the
/// same shape over SQLite later; this makes the closed loop
/// runnable + testable today. Cheap to clone (an `Arc` handle).
#[derive(Clone, Default)]
pub struct KnackLedger {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, KnackEntry>>>,
}

#[derive(Clone, Debug)]
struct KnackEntry {
    meta: KnackMeta,
    state: KnackState,
}

impl KnackLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a Knack, Active as of `now`. `created_by`
    /// drives the provenance gate — only `"agent"` Knacks are ever
    /// auto-managed.
    pub fn add(&self, id: impl Into<String>, created_by: impl Into<String>, now: i64) {
        let entry = KnackEntry {
            meta: KnackMeta {
                last_used_at: now,
                created_by: created_by.into(),
                pinned: false,
            },
            state: KnackState::Active,
        };
        self.lock().insert(id.into(), entry);
    }

    /// Record a use: refresh `last_used_at` and heal the Knack back
    /// to Active (a re-used Knack is never stale). No-op if absent.
    pub fn touch(&self, id: &str, now: i64) {
        if let Some(e) = self.lock().get_mut(id) {
            e.meta.last_used_at = now;
            e.state = KnackState::Active;
        }
    }

    /// Pin / unpin a Knack (pinned Knacks are exempt from aging).
    pub fn set_pinned(&self, id: &str, pinned: bool) {
        if let Some(e) = self.lock().get_mut(id) {
            e.meta.pinned = pinned;
        }
    }

    /// The Keeper's sweep: compute the transition plan over every
    /// Knack and apply it (ages idle agent-Knacks, heals re-used
    /// ones). Returns the plan that was applied.
    pub fn sweep(&self, now: i64, stale_after: i64, archive_after: i64) -> CurationPlan {
        let mut g = self.lock();
        let records: Vec<KnackRecord> = g
            .iter()
            .map(|(id, e)| KnackRecord {
                id: id.clone(),
                meta: e.meta.clone(),
                state: e.state,
            })
            .collect();
        let plan = plan_curation(&records, now, stale_after, archive_after);
        for id in &plan.to_stale {
            if let Some(e) = g.get_mut(id) {
                e.state = KnackState::Stale;
            }
        }
        for id in &plan.to_archive {
            if let Some(e) = g.get_mut(id) {
                e.state = KnackState::Archived;
            }
        }
        for id in &plan.to_reactivate {
            if let Some(e) = g.get_mut(id) {
                e.state = KnackState::Active;
            }
        }
        plan
    }

    /// Convenience: [`sweep`] with the default 30/90-day clock.
    pub fn sweep_default(&self, now: i64) -> CurationPlan {
        self.sweep(now, DEFAULT_STALE_AFTER_SECS, DEFAULT_ARCHIVE_AFTER_SECS)
    }

    /// A Knack's current state, if present.
    pub fn state(&self, id: &str) -> Option<KnackState> {
        self.lock().get(id).map(|e| e.state)
    }

    pub fn count_in(&self, state: KnackState) -> usize {
        self.lock().values().filter(|e| e.state == state).count()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, KnackEntry>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Default: a run that made this many tool calls is "hard enough"
/// to be worth reviewing for a new Knack.
pub const DEFAULT_KNACK_REVIEW_TOOL_THRESHOLD: u32 = 6;

/// Should a finished run trigger a background skill-creation review?
///
/// Tool-iteration intensity is the proxy for difficulty — a long
/// tool chain is exactly the multi-step work worth distilling into a
/// Knack. Fires only on a *successful* run (a failure has nothing
/// reusable to capture).
pub fn should_review_for_knack(tool_calls: u32, succeeded: bool, threshold: u32) -> bool {
    succeeded && tool_calls >= threshold
}

/// A counter-based **nudge** clock. The post-response Tradecraft /
/// memory review fires every `every` responses, so self-improvement
/// always runs *after* the reply and never competes with the task.
/// [`NudgeClock::tick`] returns `true` on the firing response.
#[derive(Clone, Debug)]
pub struct NudgeClock {
    every: u32,
    count: u32,
}

impl NudgeClock {
    pub fn new(every: u32) -> Self {
        Self {
            every: every.max(1),
            count: 0,
        }
    }

    /// Advance one response; returns `true` when the nudge fires.
    pub fn tick(&mut self) -> bool {
        self.count += 1;
        if self.count >= self.every {
            self.count = 0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_knack(last_used_at: i64) -> KnackMeta {
        KnackMeta {
            last_used_at,
            created_by: "agent".to_string(),
            pinned: false,
        }
    }

    const DAY: i64 = 86_400;

    #[test]
    fn agent_knack_ages_active_then_stale_then_archived() {
        let now = 100 * DAY;
        // Used today → Active.
        assert_eq!(curate_default(&agent_knack(now), now), KnackState::Active);
        // Idle 31 days → Stale.
        assert_eq!(
            curate_default(&agent_knack(now - 31 * DAY), now),
            KnackState::Stale
        );
        // Idle 91 days → Archived.
        assert_eq!(
            curate_default(&agent_knack(now - 91 * DAY), now),
            KnackState::Archived
        );
        // Exactly on the boundary counts as crossed.
        assert_eq!(
            curate_default(&agent_knack(now - 30 * DAY), now),
            KnackState::Stale
        );
        assert_eq!(
            curate_default(&agent_knack(now - 90 * DAY), now),
            KnackState::Archived
        );
    }

    #[test]
    fn provenance_gate_protects_non_agent_knacks() {
        let now = 1000 * DAY;
        for who in ["user", "bundled", "hub"] {
            let meta = KnackMeta {
                last_used_at: 0, // ancient
                created_by: who.to_string(),
                pinned: false,
            };
            assert!(!is_auto_managed(&meta));
            // Never aged, regardless of how long unused.
            assert_eq!(curate_default(&meta, now), KnackState::Active);
        }
    }

    #[test]
    fn pinned_agent_knacks_are_exempt() {
        let now = 1000 * DAY;
        let meta = KnackMeta {
            last_used_at: 0,
            created_by: "agent".to_string(),
            pinned: true,
        };
        assert!(!is_auto_managed(&meta));
        assert_eq!(curate_default(&meta, now), KnackState::Active);
    }

    #[test]
    fn plan_curation_reports_only_transitions_and_heals() {
        let now = 100 * DAY;
        let rec = |id: &str, idle_days: i64, state: KnackState| KnackRecord {
            id: id.to_string(),
            meta: agent_knack(now - idle_days * DAY),
            state,
        };
        let records = vec![
            // Fresh, already Active → no change.
            rec("a", 0, KnackState::Active),
            // Idle 31d but stored Active → to_stale.
            rec("b", 31, KnackState::Active),
            // Idle 91d but stored Stale → to_archive.
            rec("c", 91, KnackState::Stale),
            // Used again (idle 0) but stored Stale → heal to Active.
            rec("d", 0, KnackState::Stale),
            // Already Stale and still stale → no change.
            rec("e", 40, KnackState::Stale),
        ];
        let plan = plan_curation_default(&records, now);
        assert_eq!(plan.to_stale, vec!["b".to_string()]);
        assert_eq!(plan.to_archive, vec!["c".to_string()]);
        assert_eq!(plan.to_reactivate, vec!["d".to_string()]);
        assert_eq!(plan.changes(), 3);
        assert!(!plan.is_empty());
    }

    #[test]
    fn plan_curation_never_ages_protected_knacks() {
        let now = 1000 * DAY;
        // A user-authored Knack, ancient and stored Active.
        let user = KnackRecord {
            id: "u".to_string(),
            meta: KnackMeta {
                last_used_at: 0,
                created_by: "user".to_string(),
                pinned: false,
            },
            state: KnackState::Active,
        };
        let plan = plan_curation_default(&[user], now);
        assert!(plan.is_empty(), "protected Knack must never be aged");
    }

    #[test]
    fn knack_review_fires_on_hard_successful_runs_only() {
        let t = DEFAULT_KNACK_REVIEW_TOOL_THRESHOLD;
        // Hard + success → review.
        assert!(should_review_for_knack(t, true, t));
        assert!(should_review_for_knack(t + 5, true, t));
        // Below threshold → no review.
        assert!(!should_review_for_knack(t - 1, true, t));
        // Hard but failed → nothing to capture.
        assert!(!should_review_for_knack(t + 5, false, t));
    }

    #[test]
    fn knack_ledger_ages_heals_and_protects() {
        let now = 100 * DAY;
        let led = KnackLedger::new();
        led.add("k_agent", "agent", now - 31 * DAY); // idle 31d
        led.add("k_user", "user", now - 500 * DAY); // ancient, protected
        led.add("k_pin", "agent", now - 500 * DAY); // ancient but will pin
        led.set_pinned("k_pin", true);

        // First sweep: only the unpinned agent Knack ages to Stale.
        let plan = led.sweep_default(now);
        assert_eq!(plan.to_stale, vec!["k_agent".to_string()]);
        assert_eq!(led.state("k_agent"), Some(KnackState::Stale));
        assert_eq!(led.state("k_user"), Some(KnackState::Active));
        assert_eq!(led.state("k_pin"), Some(KnackState::Active));

        // Using the stale Knack heals it back to Active on next sweep.
        led.touch("k_agent", now);
        assert_eq!(led.state("k_agent"), Some(KnackState::Active));
        // A no-change sweep produces an empty plan.
        assert!(led.sweep_default(now).is_empty());

        // Long idle archives the agent Knack.
        let later = now + 200 * DAY;
        let plan2 = led.sweep_default(later);
        assert!(plan2.to_archive.contains(&"k_agent".to_string()));
        assert_eq!(led.count_in(KnackState::Archived), 1);
    }

    #[test]
    fn nudge_clock_fires_every_n_responses() {
        let mut clock = NudgeClock::new(3);
        assert!(!clock.tick()); // 1
        assert!(!clock.tick()); // 2
        assert!(clock.tick()); // 3 → fire
        assert!(!clock.tick()); // 4
        assert!(!clock.tick()); // 5
        assert!(clock.tick()); // 6 → fire

        // every=0 is clamped to 1 (fires every response).
        let mut each = NudgeClock::new(0);
        assert!(each.tick());
        assert!(each.tick());
    }
}
