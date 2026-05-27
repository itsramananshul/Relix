//! Per-model price table for AI cost estimation — RELIX-7.11.
//!
//! Stores prompt + completion prices per 1000 tokens as
//! micro-USD integers (`$0.000_001` units). Lookups are
//! case-insensitive on the model name; an unknown model
//! returns `None` so the collector records the row without a
//! cost rather than fabricating one.
//!
//! Defaults are sourced from publicly-documented November-2026
//! prices for the most common provider models. Operators can
//! override the table via the `[metrics.prices]` controller
//! TOML section (one entry per model).

use std::collections::HashMap;

use serde::Deserialize;

/// Per-1k-token price in micro-USD (`$0.000_001` units).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct ModelPrice {
    /// Cost of 1000 prompt (input) tokens in micro-USD.
    pub prompt_per_1k_micros: u64,
    /// Cost of 1000 completion (output) tokens in micro-USD.
    pub completion_per_1k_micros: u64,
}

impl ModelPrice {
    pub fn new(prompt: u64, completion: u64) -> Self {
        Self {
            prompt_per_1k_micros: prompt,
            completion_per_1k_micros: completion,
        }
    }
}

/// Model → price lookup. Cheap to clone — the map is small.
#[derive(Clone, Debug, Default)]
pub struct PriceTable {
    by_model: HashMap<String, ModelPrice>,
}

impl PriceTable {
    /// Empty table — every cost lookup returns `None`. Used by
    /// tests that don't care about cost.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Populated default table. Values reflect public list
    /// prices at the time the channel shipped; operators
    /// should override via config when running with negotiated
    /// rates.
    pub fn with_defaults() -> Self {
        let mut t = HashMap::new();
        // OpenAI list prices (USD per 1M tokens at time of
        // writing; we store as micros per 1k).
        t.insert(
            "gpt-4o".into(),
            ModelPrice::new(2_500, 10_000), // $2.50 / $10.00 per 1M
        );
        t.insert(
            "gpt-4o-mini".into(),
            ModelPrice::new(150, 600), // $0.15 / $0.60 per 1M
        );
        t.insert("gpt-4-turbo".into(), ModelPrice::new(10_000, 30_000));
        t.insert("gpt-3.5-turbo".into(), ModelPrice::new(500, 1_500));
        // Anthropic list prices.
        t.insert(
            "claude-opus-4".into(),
            ModelPrice::new(15_000, 75_000), // $15 / $75 per 1M
        );
        t.insert(
            "claude-sonnet-4".into(),
            ModelPrice::new(3_000, 15_000), // $3 / $15 per 1M
        );
        t.insert("claude-haiku-4".into(), ModelPrice::new(250, 1_250));
        // Google Gemini.
        t.insert("gemini-2.5-pro".into(), ModelPrice::new(1_250, 5_000));
        t.insert("gemini-2.5-flash".into(), ModelPrice::new(75, 300));
        // Local mock model — free.
        t.insert("mock".into(), ModelPrice::new(0, 0));
        Self { by_model: t }
    }

    /// Insert / override one model's price.
    pub fn set(&mut self, model: impl Into<String>, price: ModelPrice) {
        self.by_model
            .insert(model.into().to_ascii_lowercase(), price);
    }

    /// Look up the price for `model` (case-insensitive).
    pub fn get(&self, model: &str) -> Option<ModelPrice> {
        let key = model.to_ascii_lowercase();
        if let Some(p) = self.by_model.get(&key) {
            return Some(*p);
        }
        // Common variant: model with a `:tag` or `-revision`
        // suffix (e.g. `claude-sonnet-4-20250619`). Try the
        // longest known prefix.
        let mut best: Option<(usize, ModelPrice)> = None;
        for (k, p) in &self.by_model {
            if key.starts_with(k) {
                let len = k.len();
                if best.as_ref().is_none_or(|(b, _)| len > *b) {
                    best = Some((len, *p));
                }
            }
        }
        best.map(|(_, p)| p)
    }

    /// Estimate the cost (in micro-USD) of a call to `model`
    /// with the given prompt + completion token counts. Returns
    /// `None` when the model is unknown.
    pub fn estimate_cost_micros(
        &self,
        model: &str,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> Option<u64> {
        let p = self.get(model)?;
        // (tokens * price_per_1k_micros) / 1000 — integer math,
        // saturating on overflow.
        let prompt_cost = prompt_tokens.saturating_mul(p.prompt_per_1k_micros) / 1000;
        let completion_cost = completion_tokens.saturating_mul(p.completion_per_1k_micros) / 1000;
        Some(prompt_cost.saturating_add(completion_cost))
    }

    /// Number of known models. Used by status / debug surfaces.
    pub fn len(&self) -> usize {
        self.by_model.len()
    }

    /// True iff the table has no entries.
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }
}

/// `[metrics.prices]` TOML projection — a flat map of model →
/// `{ prompt_per_1k_micros, completion_per_1k_micros }`.
/// Merged onto the default table at controller boot.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct PriceTableConfig {
    #[serde(flatten)]
    pub entries: std::collections::BTreeMap<String, ModelPrice>,
}

impl PriceTableConfig {
    /// Project the config into a runtime table, starting from
    /// the defaults and overriding per-model.
    pub fn into_table(self) -> PriceTable {
        let mut t = PriceTable::with_defaults();
        for (k, v) in self.entries {
            t.set(k, v);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_common_models() {
        let t = PriceTable::with_defaults();
        assert!(t.get("gpt-4o-mini").is_some());
        assert!(t.get("claude-sonnet-4").is_some());
        assert!(t.get("gemini-2.5-flash").is_some());
    }

    #[test]
    fn lookup_is_case_insensitive() {
        let t = PriceTable::with_defaults();
        assert_eq!(t.get("GPT-4o-mini"), t.get("gpt-4o-mini"));
    }

    #[test]
    fn lookup_falls_back_to_longest_prefix_match() {
        let t = PriceTable::with_defaults();
        // Dated revision of an Anthropic model should match
        // the base `claude-sonnet-4` row.
        let dated = t.get("claude-sonnet-4-20250619").unwrap();
        let base = t.get("claude-sonnet-4").unwrap();
        assert_eq!(dated, base);
    }

    #[test]
    fn unknown_model_returns_none() {
        let t = PriceTable::with_defaults();
        assert!(t.get("totally-fake-model-9000").is_none());
    }

    #[test]
    fn estimate_cost_computes_integer_math() {
        // gpt-4o-mini: $0.15 prompt / $0.60 completion per 1M.
        // 1k prompt + 2k completion → 0.15c + 1.2c = 1.35c = 13500 micros.
        let t = PriceTable::with_defaults();
        let cost = t.estimate_cost_micros("gpt-4o-mini", 1000, 2000).unwrap();
        assert_eq!(cost, 1350);
    }

    #[test]
    fn estimate_cost_for_unknown_model_is_none() {
        let t = PriceTable::with_defaults();
        assert!(t.estimate_cost_micros("nope", 100, 100).is_none());
    }

    #[test]
    fn estimate_cost_zero_tokens_zero_cost() {
        let t = PriceTable::with_defaults();
        assert_eq!(t.estimate_cost_micros("gpt-4o-mini", 0, 0), Some(0));
    }

    #[test]
    fn config_overrides_defaults_for_named_model() {
        let cfg = PriceTableConfig {
            entries: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("gpt-4o-mini".into(), ModelPrice::new(100, 200));
                m
            },
        };
        let t = cfg.into_table();
        let p = t.get("gpt-4o-mini").unwrap();
        assert_eq!(p.prompt_per_1k_micros, 100);
        assert_eq!(p.completion_per_1k_micros, 200);
        // Other defaults still present.
        assert!(t.get("claude-sonnet-4").is_some());
    }

    #[test]
    fn empty_table_returns_none_on_every_lookup() {
        let t = PriceTable::empty();
        assert!(t.is_empty());
        assert!(t.get("gpt-4o").is_none());
    }
}
