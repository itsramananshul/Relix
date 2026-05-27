//! Shared metric types for RELIX-7.11.
//!
//! `InvocationMetric` is the canonical per-call row that lands
//! in the SQLite store. The dispatch bridge constructs one of
//! these for every dispatched capability and hands it to the
//! collector.
//!
//! `AiUsageHint` is the optional sidecar AI handlers fire when
//! they have token-count / model information that wasn't
//! available to the bridge dispatcher. The collector merges the
//! hint into the matching invocation row before persisting.

use serde::{Deserialize, Serialize};

use relix_core::types::RequestId;

/// One capability invocation as observed by the dispatch
/// bridge. Every field except `token_count` / `cost_micros` /
/// `model` is filled in at the dispatch site; the AI-specific
/// fields are filled in by [`AiUsageHint`] when present.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationMetric {
    /// Caller's friendly identity name (`IdentityBundle::name`)
    /// — the agent name. Empty when admission step 5 ran the
    /// unverified-identity path (capability-denied before
    /// identity validation succeeded).
    pub agent_name: String,
    /// Peer alias the metric was recorded on. Empty when the
    /// recorder didn't have an alias handy (e.g. test
    /// harnesses). Set by the controller wiring at boot to
    /// `cfg.controller.name`.
    pub peer_alias: String,
    /// Capability method (e.g. `ai.chat`, `task.create`).
    pub method: String,
    /// Wall-clock timestamp in milliseconds since the unix
    /// epoch — millisecond resolution gives the dashboard a
    /// useful x-axis without doubling the storage cost.
    pub timestamp_ms: i64,
    /// Per-call latency in milliseconds (handler dispatch →
    /// outcome). Does NOT include admission / policy / audit
    /// overhead, matching the dispatch-stats latency captured
    /// alongside.
    pub latency_ms: u64,
    /// True iff the handler returned `HandlerOutcome::Ok`.
    pub success: bool,
    /// Error-kind string (from `relix_core::types::error_kinds`)
    /// when `success == false`. `None` for successful calls
    /// and for policy-denied / unknown-method outcomes — the
    /// metric layer only sees handler outcomes.
    pub error_kind: Option<String>,
    /// Token count when the call was an AI invocation with
    /// provider-reported usage. `None` for non-AI calls.
    pub token_count: Option<u64>,
    /// Estimated cost in micro-USD (`$0.000001` units).
    /// Computed from `token_count` × the configured price per
    /// 1k tokens for the model. `None` when the model isn't in
    /// the price table or the token count is missing.
    pub cost_micros: Option<u64>,
    /// Bytes of the encoded request args observed by the
    /// dispatch site.
    pub input_bytes: usize,
    /// Bytes of the encoded response body for successful calls;
    /// zero for failures.
    pub output_bytes: usize,
    /// Model identifier when the call was AI-bearing
    /// (`"gpt-4o-mini"`, `"claude-sonnet-4"`, …).
    pub model: Option<String>,
    /// Original `RequestId`. Carried through the collector's
    /// in-memory join cache so an [`AiUsageHint`] arriving
    /// before the dispatcher's record can be matched. The
    /// store doesn't persist this — it's a transient join key.
    #[serde(skip_serializing, default)]
    pub request_id: Option<RequestId>,
}

impl InvocationMetric {
    /// Merge an `AiUsageHint` into this metric. Used by the
    /// collector when an AI handler fires a usage hint before
    /// the dispatch records the base metric.
    pub fn enrich_with_hint(&mut self, hint: &AiUsageHint, prices: &super::pricing::PriceTable) {
        let total = hint.prompt_tokens as u64 + hint.completion_tokens as u64;
        if total > 0 {
            self.token_count = Some(total);
        }
        if !hint.model.is_empty() {
            self.model = Some(hint.model.clone());
        }
        if let (Some(_), Some(model)) = (self.token_count, self.model.as_ref())
            && let Some(cost) = prices.estimate_cost_micros(
                model,
                hint.prompt_tokens as u64,
                hint.completion_tokens as u64,
            )
        {
            self.cost_micros = Some(cost);
        }
    }
}

/// Optional usage hint emitted by an AI handler when it has a
/// token-count + model that the dispatch site can't see. The
/// collector keeps a bounded join cache keyed by `request_id`
/// and merges any matching hint into the invocation row at
/// write time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiUsageHint {
    pub request_id: RequestId,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid() -> RequestId {
        RequestId([7u8; 16])
    }

    fn base() -> InvocationMetric {
        InvocationMetric {
            agent_name: "alice".into(),
            peer_alias: "coord".into(),
            method: "ai.chat".into(),
            timestamp_ms: 100,
            latency_ms: 50,
            success: true,
            error_kind: None,
            token_count: None,
            cost_micros: None,
            input_bytes: 32,
            output_bytes: 96,
            model: None,
            request_id: Some(rid()),
        }
    }

    #[test]
    fn enrich_attaches_token_count_and_model() {
        let mut m = base();
        let hint = AiUsageHint {
            request_id: rid(),
            prompt_tokens: 30,
            completion_tokens: 70,
            model: "gpt-4o-mini".into(),
        };
        let prices = super::super::pricing::PriceTable::with_defaults();
        m.enrich_with_hint(&hint, &prices);
        assert_eq!(m.token_count, Some(100));
        assert_eq!(m.model.as_deref(), Some("gpt-4o-mini"));
        assert!(m.cost_micros.is_some());
    }

    #[test]
    fn enrich_with_empty_model_keeps_metric_model() {
        let mut m = base();
        m.model = Some("preset-model".into());
        let hint = AiUsageHint {
            request_id: rid(),
            prompt_tokens: 10,
            completion_tokens: 20,
            model: String::new(),
        };
        let prices = super::super::pricing::PriceTable::with_defaults();
        m.enrich_with_hint(&hint, &prices);
        assert_eq!(m.model.as_deref(), Some("preset-model"));
        assert_eq!(m.token_count, Some(30));
    }

    #[test]
    fn enrich_with_zero_tokens_leaves_token_count_unchanged() {
        let mut m = base();
        let hint = AiUsageHint {
            request_id: rid(),
            prompt_tokens: 0,
            completion_tokens: 0,
            model: "mock".into(),
        };
        let prices = super::super::pricing::PriceTable::with_defaults();
        m.enrich_with_hint(&hint, &prices);
        assert!(m.token_count.is_none());
        assert_eq!(m.model.as_deref(), Some("mock"));
    }
}
