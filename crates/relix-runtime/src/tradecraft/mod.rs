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
