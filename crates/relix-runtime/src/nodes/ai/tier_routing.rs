//! §7.29 Part 1 — `[ai.routing]` config + tier-aware provider /
//! model resolution.
//!
//! The complexity classifier turns a request into a
//! [`super::complexity::ComplexityTier`]. This module turns
//! that tier into a `(provider_name, model_id)` pair the AI
//! handler can dispatch with, honouring:
//!
//! - Operator config (`[ai.routing.tiers]`).
//! - Health fallback: if the configured tier provider is
//!   unhealthy or absent, fall back to the next tier UP
//!   (Simple → Medium → Complex). If all tiers are unhealthy,
//!   fall back to the controller's default provider.
//! - Disabled / absent config: the resolver returns
//!   `RoutingDecision::Unrouted` and the AI handler dispatches
//!   with its existing default behaviour.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::complexity::{ComplexityScore, ComplexityTier};
use super::provider::ChatProvider;
use super::router::{HealthAwareRouter, ProviderHealth};

/// `[ai.routing]` config block.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct RoutingConfig {
    /// When `false` (the default), the resolver is a no-op and
    /// the AI handler dispatches with its existing default.
    #[serde(default)]
    pub enabled: bool,
    /// `[ai.routing.tiers]` — per-tier provider + model
    /// mappings. Tier slots not configured fall back to the
    /// default model on the default provider.
    #[serde(default)]
    pub tiers: TierMap,
}

/// `[ai.routing.tiers]` body.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct TierMap {
    /// Tier 1 (Simple) — cheapest fastest model.
    #[serde(default)]
    pub simple: Option<TierTarget>,
    /// Tier 2 (Medium) — balanced model.
    #[serde(default)]
    pub medium: Option<TierTarget>,
    /// Tier 3 (Complex) — most capable model.
    #[serde(default)]
    pub complex: Option<TierTarget>,
}

impl TierMap {
    /// Read the configured target for `tier`. `None` means
    /// "use the default provider + default model".
    pub fn target_for(&self, tier: ComplexityTier) -> Option<&TierTarget> {
        match tier {
            ComplexityTier::Simple => self.simple.as_ref(),
            ComplexityTier::Medium => self.medium.as_ref(),
            ComplexityTier::Complex => self.complex.as_ref(),
        }
    }
}

/// One tier's `(provider, model)` mapping.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct TierTarget {
    /// Provider name. Must match an entry under `[ai.providers.<name>]`
    /// OR the controller's active provider — the registry is
    /// fail-soft and accepts both.
    pub provider: String,
    /// Model id (provider-specific).
    pub model: String,
}

/// What the resolver decided to do for one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// Which provider name to dispatch on. `None` means "use
    /// the AI handler's default provider".
    pub provider: Option<String>,
    /// Which model id to send. `None` means "use the default".
    pub model: Option<String>,
    /// The tier the request was classified as.
    pub tier: ComplexityTier,
    /// `true` when the chosen target was the result of a
    /// health-fallback step rather than the operator's literal
    /// per-tier mapping.
    pub fell_back: bool,
    /// One-sentence operator-readable rationale.
    pub reasoning: String,
}

impl RoutingDecision {
    /// The "unrouted" decision: smart routing is disabled or
    /// no override applies. The caller dispatches with its
    /// existing default behaviour.
    pub fn unrouted(tier: ComplexityTier) -> Self {
        Self {
            provider: None,
            model: None,
            tier,
            fell_back: false,
            reasoning: "ai.routing disabled".to_string(),
        }
    }
}

/// Cheap-to-clone bundle of providers indexed by name. The AI
/// controller builds this once at startup from `[ai.providers]`;
/// the resolver looks up provider handles by name on every
/// call.
#[derive(Clone, Default)]
pub struct ProviderRegistry {
    inner: Arc<HashMap<String, Arc<dyn ChatProvider>>>,
}

impl ProviderRegistry {
    /// Build from an `(name → provider)` map.
    pub fn new(map: HashMap<String, Arc<dyn ChatProvider>>) -> Self {
        Self {
            inner: Arc::new(map),
        }
    }

    /// Look up a provider by name. Case-insensitive.
    pub fn get(&self, name: &str) -> Option<Arc<dyn ChatProvider>> {
        self.inner
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    /// Every provider name the registry knows about.
    pub fn names(&self) -> Vec<String> {
        self.inner.keys().cloned().collect()
    }

    /// Number of providers registered.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` when nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Tier-aware routing resolver.
#[derive(Clone)]
pub struct TierRouter {
    cfg: RoutingConfig,
    registry: ProviderRegistry,
    /// Health snapshot keyed by provider name. Refreshed at
    /// whatever cadence the AI controller deems fit.
    health: Arc<HashMap<String, ProviderHealth>>,
}

impl Default for TierRouter {
    fn default() -> Self {
        // The default router is disabled — handy for test
        // call sites that need a placeholder argument.
        Self::new(
            RoutingConfig::default(),
            ProviderRegistry::default(),
            Vec::new(),
        )
    }
}

impl TierRouter {
    /// New router. The health snapshot may be empty — a
    /// missing entry is treated as "fresh / healthy".
    pub fn new(
        cfg: RoutingConfig,
        registry: ProviderRegistry,
        health: Vec<ProviderHealth>,
    ) -> Self {
        let mut map: HashMap<String, ProviderHealth> = HashMap::with_capacity(health.len());
        for h in health {
            map.insert(h.name.clone(), h);
        }
        Self {
            cfg,
            registry,
            health: Arc::new(map),
        }
    }

    /// `true` when `[ai.routing] enabled = true`.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Borrow the underlying registry — used by the AI handler
    /// to look up the resolved provider by name.
    pub fn registry(&self) -> &ProviderRegistry {
        &self.registry
    }

    /// Borrow the routing config.
    pub fn config(&self) -> &RoutingConfig {
        &self.cfg
    }

    /// Decide which provider + model to dispatch on. The result
    /// is honoured by the AI handler; callers pass the
    /// [`ComplexityScore`] from the classifier.
    pub fn resolve(&self, score: &ComplexityScore) -> RoutingDecision {
        if !self.cfg.enabled {
            return RoutingDecision::unrouted(score.tier);
        }

        // Walk Simple → Medium → Complex starting at the
        // classified tier. At each step, check if the configured
        // target is reachable AND healthy. Stop on the first
        // viable match. If none, return unrouted (AI handler
        // falls back to its default).
        let mut tier = score.tier;
        let mut fell_back = false;
        let mut visited: Vec<ComplexityTier> = vec![tier];
        loop {
            match self.cfg.tiers.target_for(tier) {
                Some(t) => {
                    let healthy = self.is_provider_healthy(&t.provider);
                    let in_registry = self.registry.get(&t.provider).is_some();
                    if healthy && (in_registry || self.registry.is_empty()) {
                        let reasoning = if fell_back {
                            format!(
                                "tier {} unavailable; routing as {} → {} on {}",
                                score.tier.as_str(),
                                tier.as_str(),
                                t.model,
                                t.provider,
                            )
                        } else {
                            format!("routed {} → {} on {}", tier.as_str(), t.model, t.provider,)
                        };
                        return RoutingDecision {
                            provider: Some(t.provider.clone()),
                            model: Some(t.model.clone()),
                            tier: score.tier,
                            fell_back,
                            reasoning,
                        };
                    }
                    fell_back = true;
                }
                None => {
                    fell_back = true;
                }
            }

            let next = tier.next_up();
            if next == tier || visited.contains(&next) {
                // Walk terminated. Fall through to the AI
                // handler's default.
                return RoutingDecision {
                    provider: None,
                    model: None,
                    tier: score.tier,
                    fell_back: true,
                    reasoning: format!(
                        "every tier unhealthy or unconfigured (visited {:?}); using default",
                        visited.iter().map(|t| t.as_str()).collect::<Vec<_>>()
                    ),
                };
            }
            tier = next;
            visited.push(tier);
        }
    }

    fn is_provider_healthy(&self, name: &str) -> bool {
        let Some(h) = self.health.get(name) else {
            // Unknown providers are treated as fresh / healthy
            // (matches `HealthAwareRouter`).
            return true;
        };
        !h.in_cooldown
    }
}

/// Build a registry from a HashMap. Convenience for tests.
#[cfg(test)]
pub(crate) fn registry_from(pairs: Vec<(&str, Arc<dyn ChatProvider>)>) -> ProviderRegistry {
    let map: HashMap<String, Arc<dyn ChatProvider>> =
        pairs.into_iter().map(|(n, p)| (n.to_string(), p)).collect();
    ProviderRegistry::new(map)
}

/// GAP-22-Feature-2-style hook: provide a [`HealthAwareRouter`]
/// view over the same health snapshot so future commits can
/// blend tier routing + health-based candidate ranking. Not
/// used by the tier resolver itself — the tier resolver picks
/// per-tier providers directly.
pub fn health_router_from_snapshot(snapshot: &[ProviderHealth]) -> HealthAwareRouter {
    HealthAwareRouter::new(snapshot.to_vec())
}

/// Convenience: derive a `tier_for_routing` string the metrics
/// row stores so dashboards can slice cost by tier.
pub fn metrics_tier_label(tier: ComplexityTier) -> &'static str {
    tier.as_str()
}

/// Re-export for callers that just want the trait-bounded view.
pub use super::router::ProviderRouter as _ProviderRouterReexport;

/// `routing.explain` coordinator cap — classify a message,
/// resolve the tier, and return both the score and the
/// decision as JSON. Used by:
///
/// - the bridge endpoint `POST /v1/routing/explain`,
/// - the CLI `relix routing explain --message "..."`,
/// - operator dashboards that want to dry-run the resolver
///   without paying the provider call.
///
/// Arg encoding (JSON):
/// ```json
/// { "message": "hello", "session_turns": 0 }
/// ```
pub mod caps {
    use std::sync::Arc;

    use relix_core::types::{ErrorEnvelope, error_kinds};
    use serde::{Deserialize, Serialize};

    use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

    use super::super::complexity::{ComplexityClassifier, ComplexityScore};
    use super::{RoutingDecision, TierRouter};

    /// Wire the `routing.explain` cap onto `bridge`.
    pub fn register(bridge: &mut DispatchBridge, router: TierRouter) {
        let router_for_explain = router;
        bridge.register(
            "routing.explain",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let router = router_for_explain.clone();
                async move { handle_explain(&router, &ctx) }
            })),
        );
    }

    /// Request body for `routing.explain`.
    #[derive(Clone, Debug, Default, Deserialize)]
    pub struct ExplainRequest {
        /// The message text the operator wants to classify.
        #[serde(default)]
        pub message: String,
        /// Session turn count for the session-turn signal.
        /// Defaults to 0 when omitted.
        #[serde(default)]
        pub session_turns: u32,
    }

    /// Response body for `routing.explain`.
    #[derive(Clone, Debug, Serialize, Deserialize)]
    pub struct ExplainResponse {
        /// Score from the classifier — tier + numeric score +
        /// list of triggered signal names.
        pub score: ComplexityScore,
        /// What the resolver decided to do (provider + model
        /// override, fallback flag, rationale).
        pub decision: RoutingDecision,
        /// Whether `[ai.routing] enabled = true` at the moment
        /// of the call. When false, decision.provider /
        /// decision.model are always None.
        pub routing_enabled: bool,
    }

    fn handle_explain(router: &TierRouter, ctx: &InvocationCtx) -> HandlerOutcome {
        let req: ExplainRequest = if ctx.args.is_empty() {
            ExplainRequest::default()
        } else {
            match serde_json::from_slice(&ctx.args) {
                Ok(r) => r,
                Err(e) => {
                    return HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::INVALID_ARGS,
                        cause: format!("routing.explain: decode args: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    });
                }
            }
        };
        if req.message.trim().is_empty() {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: "routing.explain: message is required".into(),
                retry_hint: 0,
                retry_after: None,
            });
        }
        let score = ComplexityClassifier::new().classify(&req.message, req.session_turns);
        let decision = router.resolve(&score);
        let body = ExplainResponse {
            score,
            decision,
            routing_enabled: router.enabled(),
        };
        match serde_json::to_vec(&body) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("routing.explain: encode response: {e}"),
                retry_hint: 0,
                retry_after: None,
            }),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::dispatch::DispatchBridge;
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use relix_core::identity::VerifiedIdentity;
        use relix_core::policy::PolicyEngine;
        use relix_core::types::{NodeId, RequestId, TraceId};
        use tempfile::TempDir;

        fn fresh_bridge() -> (DispatchBridge, TempDir) {
            let dir = TempDir::new().unwrap();
            let org_root = SigningKey::generate(&mut OsRng);
            let responder = SigningKey::generate(&mut OsRng);
            let policy = PolicyEngine::permissive();
            let bridge = DispatchBridge::new(
                policy,
                org_root.verifying_key(),
                &dir.path().join("audit.log"),
                responder,
            )
            .unwrap();
            (bridge, dir)
        }

        fn ctx(args: Vec<u8>) -> InvocationCtx {
            InvocationCtx {
                caller: VerifiedIdentity {
                    subject_id: NodeId::from_pubkey(b"caller"),
                    name: "alice".into(),
                    org_id: NodeId::from_pubkey(b"org"),
                    groups: vec!["operators".into()],
                    role: "agent".into(),
                    clearance: "internal".into(),
                    bundle_id: [0; 32],
                },
                trace_id: TraceId::new(),
                request_id: RequestId::new(),
                tenant_id: None,
                args,
            }
        }

        #[test]
        fn routing_explain_returns_score_and_decision() {
            let router = TierRouter::default();
            let (mut bridge, _td) = fresh_bridge();
            register(&mut bridge, router.clone());
            let body = serde_json::json!({ "message": "hi", "session_turns": 0 });
            let outcome = handle_explain(&router, &ctx(serde_json::to_vec(&body).unwrap()));
            match outcome {
                HandlerOutcome::Ok(bytes) => {
                    let resp: ExplainResponse = serde_json::from_slice(&bytes).unwrap();
                    assert!(!resp.routing_enabled);
                    assert!(resp.decision.provider.is_none());
                    assert_eq!(resp.score.tier.as_str(), "simple");
                }
                HandlerOutcome::Err(e) => panic!("expected Ok, got Err: {:?}", e.kind),
            }
        }

        #[test]
        fn routing_explain_rejects_empty_message() {
            let router = TierRouter::default();
            let outcome = handle_explain(&router, &ctx(b"{}".to_vec()));
            match outcome {
                HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
                HandlerOutcome::Ok(_) => panic!("expected error"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::ai::complexity::{ComplexityClassifier, ComplexityTier};
    use crate::nodes::ai::provider::MockProvider;

    fn cfg(enabled: bool) -> RoutingConfig {
        RoutingConfig {
            enabled,
            tiers: TierMap {
                simple: Some(TierTarget {
                    provider: "ollama-small".into(),
                    model: "llama3.2:3b".into(),
                }),
                medium: Some(TierTarget {
                    provider: "ollama-mid".into(),
                    model: "llama3.1:8b".into(),
                }),
                complex: Some(TierTarget {
                    provider: "ollama-big".into(),
                    model: "llama3.1:70b".into(),
                }),
            },
        }
    }

    fn registry() -> ProviderRegistry {
        let mock: Arc<dyn ChatProvider> = Arc::new(MockProvider);
        registry_from(vec![
            ("ollama-small", mock.clone()),
            ("ollama-mid", mock.clone()),
            ("ollama-big", mock),
        ])
    }

    fn classify(message: &str, turns: u32) -> ComplexityScore {
        ComplexityClassifier::new().classify(message, turns)
    }

    fn health(name: &str, in_cooldown: bool) -> ProviderHealth {
        ProviderHealth {
            name: name.into(),
            in_cooldown,
            operator_quarantined: false,
            rate_limit_hits_5min: 0,
            success_count: 10,
            failure_count: 0,
        }
    }

    #[test]
    fn disabled_config_yields_unrouted_decision() {
        let r = TierRouter::new(cfg(false), registry(), vec![]);
        let d = r.resolve(&classify("hi", 0));
        assert!(d.provider.is_none());
        assert!(d.model.is_none());
        assert!(!d.fell_back);
        assert!(d.reasoning.contains("disabled"));
    }

    #[test]
    fn simple_request_resolves_to_simple_tier_target() {
        let r = TierRouter::new(cfg(true), registry(), vec![]);
        let d = r.resolve(&classify("hi", 0));
        assert_eq!(d.tier, ComplexityTier::Simple);
        assert_eq!(d.provider.as_deref(), Some("ollama-small"));
        assert_eq!(d.model.as_deref(), Some("llama3.2:3b"));
        assert!(!d.fell_back);
    }

    #[test]
    fn complex_request_resolves_to_complex_tier_target() {
        let r = TierRouter::new(cfg(true), registry(), vec![]);
        let prompt = "think carefully. architecture review of this design.";
        let d = r.resolve(&classify(prompt, 6));
        assert_eq!(d.tier, ComplexityTier::Complex);
        assert_eq!(d.provider.as_deref(), Some("ollama-big"));
        assert_eq!(d.model.as_deref(), Some("llama3.1:70b"));
    }

    #[test]
    fn unhealthy_tier_provider_falls_back_to_next_tier_up() {
        let r = TierRouter::new(
            cfg(true),
            registry(),
            vec![
                health("ollama-small", true), // cooldown
                health("ollama-mid", false),
                health("ollama-big", false),
            ],
        );
        let d = r.resolve(&classify("hi", 0));
        assert_eq!(d.tier, ComplexityTier::Simple); // classifier still says simple
        assert!(d.fell_back);
        assert_eq!(d.provider.as_deref(), Some("ollama-mid"));
        assert_eq!(d.model.as_deref(), Some("llama3.1:8b"));
        assert!(d.reasoning.contains("unavailable"));
    }

    #[test]
    fn cascading_fallback_walks_simple_to_medium_to_complex() {
        let r = TierRouter::new(
            cfg(true),
            registry(),
            vec![
                health("ollama-small", true),
                health("ollama-mid", true),
                health("ollama-big", false),
            ],
        );
        let d = r.resolve(&classify("hi", 0));
        assert!(d.fell_back);
        assert_eq!(d.provider.as_deref(), Some("ollama-big"));
    }

    #[test]
    fn all_tiers_unhealthy_falls_back_to_default() {
        let r = TierRouter::new(
            cfg(true),
            registry(),
            vec![
                health("ollama-small", true),
                health("ollama-mid", true),
                health("ollama-big", true),
            ],
        );
        let d = r.resolve(&classify("hi", 0));
        assert!(d.provider.is_none());
        assert!(d.model.is_none());
        assert!(d.fell_back);
        assert!(d.reasoning.contains("default"));
    }

    #[test]
    fn missing_tier_mapping_falls_back_to_next_tier() {
        let mut c = cfg(true);
        c.tiers.simple = None;
        let r = TierRouter::new(c, registry(), vec![]);
        let d = r.resolve(&classify("hi", 0));
        assert!(d.fell_back);
        assert_eq!(d.provider.as_deref(), Some("ollama-mid"));
    }
}
