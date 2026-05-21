//! PH-ROUTER1 / PH-ROUTER2 — Provider router scaffold + health-aware impl.
//!
//! Pure groundwork for the future smart router that will pick
//! among multiple configured AI providers based on health,
//! cost, latency, and request shape. Today the AI node uses
//! one configured provider per `[ai] provider = "..."` entry;
//! [`NoopRouter`] preserves that behaviour exactly while
//! exposing the [`ProviderRouter`] trait + [`RouteDecision`]
//! envelope so future smart routers slot in without API
//! churn.
//!
//! ## Why ship the scaffold before the smart router?
//!
//! - **Stable contract**: every future router-aware caller can
//!   start consuming [`RouteDecision`] today (zero-info struct
//!   for the no-op path) and grow into the richer fields as
//!   the smart router lands.
//! - **Operator visibility**: a future capability surface like
//!   `ai.route_explain` can return the most recent
//!   `RouteDecision` to operators wanting to know "why did the
//!   bridge pick provider X for that call?". Even with the
//!   no-op router that surface ships a meaningful answer
//!   ("only one provider configured").
//! - **Honest scope**: this module does NOT mutate live AI
//!   routing today. Adding a `Router` instance to the AI node
//!   is a separate follow-up milestone.
//!
//! ## What this does NOT do
//!
//! - No retry / fallback logic. The router picks ONE provider;
//!   retry orchestration belongs to the caller (or a future
//!   milestone above this layer).
//! - No live scoring. The trait accepts `ChatInput` so a
//!   future scorer can inspect request shape, but the no-op
//!   path never reads it.
//! - No state. Routers are constructed per-call today; future
//!   smart routers may grow internal state (rolling-window
//!   counters, cached health) but the trait stays object-safe
//!   so callers don't pin themselves to a specific impl.

use super::ChatInput;

/// The router's typed answer to "which provider should serve
/// this call?". Returned even when only one candidate is
/// configured (the no-op case) so callers can log decisions
/// uniformly.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDecision {
    /// Provider name the router chose. Always populated.
    pub chosen: String,
    /// Every candidate the router considered, in ranking order
    /// (best-first). For the no-op router this is exactly the
    /// caller-supplied candidates list with `score = 1.0`.
    pub candidates: Vec<RouteCandidate>,
    /// One-sentence operator-readable rationale. The no-op
    /// router uses "no-op single-provider mode (only candidate
    /// available)" or "no-op single-provider mode (first of N
    /// candidates)" so log scrapers can distinguish them.
    pub reasoning: String,
    /// Wall-clock unix seconds at which the decision was made.
    pub chosen_at: i64,
}

/// One row of [`RouteDecision::candidates`]. Future smart
/// routers populate `score`, `eligibility`, and `why` with
/// real signal; the no-op path leaves them at trivial defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCandidate {
    /// Provider name.
    pub name: String,
    /// 0.0..1.0 score, higher = better. No-op router: 1.0 for
    /// the chosen candidate, 0.5 for everyone else (preserves
    /// ordering signal without claiming real scoring).
    pub score: f32,
    /// `"eligible"`, `"ineligible"`, `"unknown"`. No-op router
    /// always returns `"eligible"`.
    pub eligibility: String,
    /// Short rationale specific to this candidate. No-op:
    /// `"first in caller-supplied list"` for chosen,
    /// `"considered but unranked"` for others.
    pub why: String,
}

/// Provider-router contract. Implementors decide which
/// provider out of a caller-supplied candidate list serves a
/// given request. The default [`NoopRouter`] picks the first
/// candidate and tags every other with `score = 0.5` so the
/// decision envelope still surfaces the full candidate set.
pub trait ProviderRouter: Send + Sync {
    /// Short stable name (used in tracing fields + dashboard
    /// badges). Lowercase + kebab-case.
    fn name(&self) -> &'static str;

    /// Pick one provider out of `candidates`. `candidates`
    /// must be non-empty; routers may panic on empty input
    /// since callers are responsible for filtering out
    /// disabled / quarantined providers upstream.
    fn pick(&self, input: &ChatInput, candidates: &[String]) -> RouteDecision;
}

/// No-op router. Preserves the current single-provider
/// behaviour exactly: pick the first candidate. Multiple
/// candidates are surfaced in the decision envelope but the
/// router doesn't actually score them.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopRouter;

impl NoopRouter {
    pub fn new() -> Self {
        Self
    }
}

impl ProviderRouter for NoopRouter {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn pick(&self, _input: &ChatInput, candidates: &[String]) -> RouteDecision {
        assert!(
            !candidates.is_empty(),
            "ProviderRouter::pick called with empty candidates"
        );
        let chosen = candidates[0].clone();
        let chosen_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut rows: Vec<RouteCandidate> = Vec::with_capacity(candidates.len());
        for (i, name) in candidates.iter().enumerate() {
            if i == 0 {
                rows.push(RouteCandidate {
                    name: name.clone(),
                    score: 1.0,
                    eligibility: "eligible".to_string(),
                    why: "first in caller-supplied list".to_string(),
                });
            } else {
                rows.push(RouteCandidate {
                    name: name.clone(),
                    score: 0.5,
                    eligibility: "eligible".to_string(),
                    why: "considered but unranked (noop router)".to_string(),
                });
            }
        }
        let reasoning = if candidates.len() == 1 {
            "no-op single-provider mode (only candidate available)".to_string()
        } else {
            format!(
                "no-op single-provider mode (first of {} candidates)",
                candidates.len()
            )
        };
        RouteDecision {
            chosen,
            candidates: rows,
            reasoning,
            chosen_at,
        }
    }
}

// ─────────────────────────── HealthAwareRouter ───────────────────────────

/// PH-ROUTER2: per-provider health snapshot the router consumes
/// to filter / rank candidates. Mirrors the shape the bridge's
/// `/v1/providers/health` endpoint already projects so callers
/// can build the snapshot from cached state without inventing a
/// new wire format.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderHealth {
    /// Lower-case provider name, matching `ChatInput.provider`.
    pub name: String,
    /// `true` when the bridge has the provider in cooldown
    /// (either operator-quarantine or auto-cooldown from a
    /// rate-limit storm). Cooldown providers are excluded
    /// from routing entirely.
    pub in_cooldown: bool,
    /// `true` when the operator explicitly quarantined this
    /// provider. Tracked separately from `in_cooldown` so the
    /// router can surface a different `why` string.
    pub operator_quarantined: bool,
    /// Rolling 5-minute rate-limit hit count from the bridge's
    /// observation ring. Pure observability; routers may use
    /// it as a tie-breaker but a non-zero count does NOT
    /// exclude a provider on its own.
    pub rate_limit_hits_5min: u64,
    /// Lifetime success counter from the bridge.
    pub success_count: u64,
    /// Lifetime fail counter from the bridge.
    pub failure_count: u64,
}

impl ProviderHealth {
    /// 0.0..1.0 success ratio. Returns 1.0 for a fresh provider
    /// with no recorded calls (don't penalise the unknown).
    pub fn success_ratio(&self) -> f32 {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            return 1.0;
        }
        self.success_count as f32 / total as f32
    }
}

/// PH-ROUTER2: router that filters out unhealthy providers and
/// ranks the survivors by recent reliability. Designed to be
/// drop-in for [`NoopRouter`]: the same `pick(input, candidates)`
/// signature, no panics on healthy input, and a one-sentence
/// `reasoning` field operators can read in the dashboard.
///
/// Filtering posture (fail-closed):
/// - in_cooldown providers are excluded.
/// - If filtering would leave the candidate list empty, the
///   router falls back to picking the highest-success-ratio
///   candidate from the ORIGINAL list — operators see a
///   "all candidates unhealthy" reasoning string so the choice
///   is honest about being a forced fallback.
///
/// Scoring (within the eligible set):
/// - score = success_ratio (0.0..1.0)
/// - tie-break by lower `rate_limit_hits_5min`
/// - final tie-break by candidate position (stable sort)
pub struct HealthAwareRouter {
    snapshot: Vec<ProviderHealth>,
}

impl HealthAwareRouter {
    /// Build a router from a health snapshot. Callers refresh
    /// the snapshot at whatever cadence makes sense (typically
    /// per-request via a cached call to `/v1/providers/health`).
    pub fn new(snapshot: Vec<ProviderHealth>) -> Self {
        Self { snapshot }
    }

    fn health_for(&self, name: &str) -> Option<&ProviderHealth> {
        self.snapshot
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
    }
}

impl ProviderRouter for HealthAwareRouter {
    fn name(&self) -> &'static str {
        "health-aware"
    }

    fn pick(&self, _input: &ChatInput, candidates: &[String]) -> RouteDecision {
        assert!(
            !candidates.is_empty(),
            "ProviderRouter::pick called with empty candidates"
        );
        let chosen_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // 1. Score every caller-supplied candidate. Missing
        //    health rows are treated as "fresh / unknown"
        //    (success_ratio 1.0, no rate limits, eligible).
        let mut rows: Vec<(String, f32, u64, bool, bool, &'static str)> =
            Vec::with_capacity(candidates.len());
        for name in candidates {
            let (score, rl5, in_cd, op_q, why) = match self.health_for(name) {
                Some(h) if h.in_cooldown && h.operator_quarantined => (
                    h.success_ratio(),
                    h.rate_limit_hits_5min,
                    true,
                    true,
                    "operator-quarantined",
                ),
                Some(h) if h.in_cooldown => (
                    h.success_ratio(),
                    h.rate_limit_hits_5min,
                    true,
                    false,
                    "auto-cooldown",
                ),
                Some(h) => (
                    h.success_ratio(),
                    h.rate_limit_hits_5min,
                    false,
                    false,
                    "healthy",
                ),
                None => (1.0, 0, false, false, "no-health-snapshot"),
            };
            rows.push((name.clone(), score, rl5, in_cd, op_q, why));
        }

        // 2. Partition healthy vs unhealthy preserving input
        //    order so the position tie-break is meaningful.
        let mut healthy: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| if !r.3 { Some(i) } else { None })
            .collect();
        // 3. Rank healthy candidates: higher score first, then
        //    lower rate-limit hit count.
        healthy.sort_by(|&a, &b| {
            let ra = &rows[a];
            let rb = &rows[b];
            rb.1.partial_cmp(&ra.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(ra.2.cmp(&rb.2))
        });

        let (chosen_idx, fallback) = if let Some(&i) = healthy.first() {
            (i, false)
        } else {
            // All candidates unhealthy. Fall back to the
            // best-success-ratio across the original list so
            // we don't panic — callers see the reasoning.
            let mut all_by_score: Vec<usize> = (0..rows.len()).collect();
            all_by_score.sort_by(|&a, &b| {
                rows[b]
                    .1
                    .partial_cmp(&rows[a].1)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            (all_by_score[0], true)
        };

        // 4. Build the candidate envelope. Chosen candidate gets
        //    the actual score; the rest get a halved score to
        //    preserve ordering signal without claiming they were
        //    picked.
        let chosen_name = rows[chosen_idx].0.clone();
        let chosen_score = rows[chosen_idx].1;
        let mut envelope: Vec<RouteCandidate> = Vec::with_capacity(rows.len());
        for (idx, r) in rows.iter().enumerate() {
            let eligibility = if r.3 { "ineligible" } else { "eligible" };
            let why = if idx == chosen_idx {
                format!("chosen ({} success_ratio={:.2})", r.5, r.1)
            } else {
                format!("considered ({} success_ratio={:.2})", r.5, r.1)
            };
            let score = if idx == chosen_idx { r.1 } else { r.1 * 0.5 };
            envelope.push(RouteCandidate {
                name: r.0.clone(),
                score,
                eligibility: eligibility.to_string(),
                why,
            });
        }

        let reasoning = if fallback {
            format!(
                "all {} candidates unhealthy; falling back to highest-success-ratio \
                 (chosen={} success_ratio={:.2})",
                rows.len(),
                chosen_name,
                chosen_score
            )
        } else if rows.len() == 1 {
            format!(
                "health-aware single-candidate mode (chosen={} success_ratio={:.2})",
                chosen_name, chosen_score
            )
        } else {
            format!(
                "health-aware: chose {} (success_ratio={:.2}) from {} eligible of {} total",
                chosen_name,
                chosen_score,
                healthy.len(),
                rows.len(),
            )
        };

        RouteDecision {
            chosen: chosen_name,
            candidates: envelope,
            reasoning,
            chosen_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ChatInput {
        ChatInput {
            session_id: "s".into(),
            prompt: "hello".into(),
            ..Default::default()
        }
    }

    #[test]
    fn noop_router_picks_first_candidate() {
        let r = NoopRouter::new();
        let d = r.pick(&input(), &["openai".into()]);
        assert_eq!(d.chosen, "openai");
        assert_eq!(d.candidates.len(), 1);
        assert!((d.candidates[0].score - 1.0).abs() < 1e-6);
        assert!(d.reasoning.contains("only candidate"));
    }

    #[test]
    fn noop_router_surfaces_all_candidates_with_chosen_first() {
        let r = NoopRouter::new();
        let d = r.pick(
            &input(),
            &["openai".into(), "anthropic".into(), "mock".into()],
        );
        assert_eq!(d.chosen, "openai");
        assert_eq!(d.candidates.len(), 3);
        assert_eq!(d.candidates[0].name, "openai");
        assert!((d.candidates[0].score - 1.0).abs() < 1e-6);
        assert!((d.candidates[1].score - 0.5).abs() < 1e-6);
        assert!((d.candidates[2].score - 0.5).abs() < 1e-6);
        for c in &d.candidates {
            assert_eq!(c.eligibility, "eligible");
        }
        assert!(d.reasoning.contains("first of 3"));
    }

    #[test]
    fn noop_router_stamps_chosen_at() {
        let r = NoopRouter::new();
        let d = r.pick(&input(), &["mock".into()]);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        // chosen_at should be within a few seconds of now.
        assert!((d.chosen_at - now).abs() < 5);
    }

    #[test]
    fn noop_router_name_is_stable_kebab_case() {
        assert_eq!(NoopRouter.name(), "noop");
    }

    #[test]
    #[should_panic(expected = "empty candidates")]
    fn noop_router_panics_on_empty_candidates() {
        // Callers MUST filter out disabled providers upstream;
        // the router itself doesn't try to recover from "no
        // candidates" because that's a programmer error, not
        // an operator one.
        let r = NoopRouter::new();
        let _ = r.pick(&input(), &[]);
    }

    #[test]
    fn route_decision_is_object_safe_via_trait_object() {
        // PH-ROUTER1 contract: ProviderRouter must be
        // dyn-compatible so callers can hold an Arc<dyn ...>
        // and swap routers at runtime. The compiler enforces
        // this; the test exists to fail fast if a future
        // method addition breaks object-safety.
        let r: Box<dyn ProviderRouter> = Box::new(NoopRouter::new());
        let d = r.pick(&input(), &["mock".into()]);
        assert_eq!(d.chosen, "mock");
    }

    // ── PH-ROUTER2: HealthAwareRouter ──────────────────────────────

    fn health(
        name: &str,
        in_cooldown: bool,
        op_q: bool,
        ok: u64,
        fail: u64,
        rl5: u64,
    ) -> ProviderHealth {
        ProviderHealth {
            name: name.into(),
            in_cooldown,
            operator_quarantined: op_q,
            rate_limit_hits_5min: rl5,
            success_count: ok,
            failure_count: fail,
        }
    }

    #[test]
    fn provider_health_success_ratio_with_no_calls_is_one() {
        let h = health("openai", false, false, 0, 0, 0);
        assert!((h.success_ratio() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn provider_health_success_ratio_basic() {
        let h = health("openai", false, false, 9, 1, 0);
        assert!((h.success_ratio() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn health_aware_router_name_is_stable_kebab_case() {
        let r = HealthAwareRouter::new(vec![]);
        assert_eq!(r.name(), "health-aware");
    }

    #[test]
    fn health_aware_router_picks_only_healthy_candidate() {
        let r = HealthAwareRouter::new(vec![
            health("anthropic", true, false, 10, 10, 50),
            health("openai", false, false, 8, 2, 0),
        ]);
        let d = r.pick(&input(), &["anthropic".into(), "openai".into()]);
        assert_eq!(d.chosen, "openai");
        assert!(d.reasoning.contains("1 eligible of 2 total"));
        let chosen_row = d.candidates.iter().find(|c| c.name == "openai").unwrap();
        assert_eq!(chosen_row.eligibility, "eligible");
        let other = d.candidates.iter().find(|c| c.name == "anthropic").unwrap();
        assert_eq!(other.eligibility, "ineligible");
    }

    #[test]
    fn health_aware_router_prefers_higher_success_ratio() {
        // Both healthy; openai has 0.9 ratio, anthropic 0.5.
        let r = HealthAwareRouter::new(vec![
            health("anthropic", false, false, 5, 5, 0),
            health("openai", false, false, 9, 1, 0),
        ]);
        let d = r.pick(&input(), &["anthropic".into(), "openai".into()]);
        assert_eq!(d.chosen, "openai");
        assert!(d.reasoning.contains("chose openai"));
    }

    #[test]
    fn health_aware_router_breaks_tie_with_rate_limit_count() {
        let r = HealthAwareRouter::new(vec![
            health("anthropic", false, false, 9, 1, 10), // 0.9 ratio, 10 rl hits
            health("openai", false, false, 9, 1, 0),     // 0.9 ratio, 0 rl hits
        ]);
        let d = r.pick(&input(), &["anthropic".into(), "openai".into()]);
        assert_eq!(d.chosen, "openai");
    }

    #[test]
    fn health_aware_router_treats_unknown_provider_as_fresh() {
        // No health rows at all — every candidate is "unknown",
        // success_ratio 1.0, eligible. Picks first (stable).
        let r = HealthAwareRouter::new(vec![]);
        let d = r.pick(&input(), &["openai".into(), "anthropic".into()]);
        assert_eq!(d.chosen, "openai");
        assert!(d.reasoning.contains("2 eligible of 2"));
        for c in &d.candidates {
            assert_eq!(c.eligibility, "eligible");
        }
    }

    #[test]
    fn health_aware_router_falls_back_when_all_unhealthy() {
        let r = HealthAwareRouter::new(vec![
            health("anthropic", true, false, 5, 5, 0), // 0.5 ratio, cooldown
            health("openai", true, true, 9, 1, 0),     // 0.9 ratio, op-q
        ]);
        let d = r.pick(&input(), &["anthropic".into(), "openai".into()]);
        // Best success ratio wins on fallback.
        assert_eq!(d.chosen, "openai");
        assert!(d.reasoning.contains("all 2 candidates unhealthy"));
        // BOTH rows should still be marked ineligible in the envelope.
        for c in &d.candidates {
            assert_eq!(c.eligibility, "ineligible");
        }
    }

    #[test]
    fn health_aware_router_distinguishes_cooldown_vs_quarantine_in_why() {
        let r = HealthAwareRouter::new(vec![
            health("anthropic", true, true, 5, 5, 0),
            health("openai", false, false, 9, 1, 0),
        ]);
        let d = r.pick(&input(), &["anthropic".into(), "openai".into()]);
        let q = d.candidates.iter().find(|c| c.name == "anthropic").unwrap();
        assert!(q.why.contains("operator-quarantined"));
    }

    #[test]
    fn health_aware_router_is_object_safe() {
        let r: Box<dyn ProviderRouter> = Box::new(HealthAwareRouter::new(vec![]));
        let d = r.pick(&input(), &["mock".into()]);
        assert_eq!(d.chosen, "mock");
    }

    #[test]
    #[should_panic(expected = "empty candidates")]
    fn health_aware_router_panics_on_empty_candidates() {
        let r = HealthAwareRouter::new(vec![]);
        let _ = r.pick(&input(), &[]);
    }
}
