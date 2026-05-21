//! PH-ROUTER1 — Provider router scaffold.
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
}
