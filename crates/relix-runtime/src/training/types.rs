//! RELIX-7.15 — shared types for the training data pipeline.
//!
//! Every AI agent interaction (a `system_prompt` + `user_message`
//! → `response` exchange, plus any tool calls made during the
//! response) gets a unique [`InteractionId`] and lands on a
//! [`InteractionRecord`] row.
//!
//! Records are immutable in the dispatcher's view; only the
//! `quality_score` + `exported` + `export_set` columns are
//! updated post-insert (by the quality scorer and export
//! engine respectively).

use serde::{Deserialize, Serialize};

/// Unique identifier for a recorded interaction. Wraps a hex
/// string (32 chars of randomness from
/// `relix_core::types::RequestId`) so the column type lines up
/// with the rest of the codebase.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InteractionId(pub String);

impl InteractionId {
    /// Mint a fresh id from a `RequestId`. Hex-encoded so it
    /// round-trips through any TEXT column without escaping.
    pub fn from_request(id: &relix_core::types::RequestId) -> Self {
        let mut s = String::with_capacity(32);
        for b in id.0.iter() {
            use std::fmt::Write;
            let _ = write!(&mut s, "{b:02x}");
        }
        Self(s)
    }

    /// Mint a fresh id from raw randomness. Used by tests + by
    /// the streaming-handler path where the dispatcher's
    /// `RequestId` is the natural source.
    pub fn new() -> Self {
        Self::from_request(&relix_core::types::RequestId::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for InteractionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for InteractionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One tool-call observation embedded inside an interaction.
/// Stored as a JSON-array element in
/// [`InteractionRecord::tool_calls`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool: String,
    pub input: String,
    pub output: String,
    pub success: bool,
    pub latency_ms: u64,
    /// Populated only when `success == false`. Mirrors the
    /// dispatch bridge's `ErrorEnvelope.kind` shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// One persisted agent interaction. Mirrors the SQL schema in
/// [`super::store`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionRecord {
    pub interaction_id: InteractionId,
    pub session_id: String,
    pub agent: String,
    pub model: String,
    pub provider: String,
    pub system_prompt: String,
    pub user_message: String,
    pub response: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallRecord>,
    /// Total (prompt + completion) tokens when reported by the
    /// provider; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    pub latency_ms: u64,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// Wall-clock unix-ms timestamp at which the interaction
    /// completed.
    pub recorded_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<f32>,
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_set: Option<String>,
    /// RELIX-7.15 PII step: `true` when the recorder applied
    /// the PII anonymizer to `system_prompt` / `user_message` /
    /// `response` / `tool_calls` BEFORE persisting this row.
    /// `false` rows are anonymized at export time as a safety
    /// net (covers rows recorded before anonymization was
    /// enabled). The default for backwards-compat is `false`.
    #[serde(default)]
    pub anonymized: bool,
}

impl InteractionRecord {
    /// Convenience constructor used by the AI handler — it
    /// builds the record from values it already has at
    /// completion time and lets the recorder add the wall-clock
    /// `recorded_at` if the caller passes `0`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        interaction_id: InteractionId,
        session_id: String,
        agent: String,
        model: String,
        provider: String,
        system_prompt: String,
        user_message: String,
        response: String,
        tool_calls: Vec<ToolCallRecord>,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
        latency_ms: u64,
        success: bool,
        error_kind: Option<String>,
        recorded_at: i64,
    ) -> Self {
        let token_count = match (prompt_tokens, completion_tokens) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        Self {
            interaction_id,
            session_id,
            agent,
            model,
            provider,
            system_prompt,
            user_message,
            response,
            tool_calls,
            token_count,
            prompt_tokens,
            completion_tokens,
            latency_ms,
            success,
            error_kind,
            recorded_at,
            quality_score: None,
            exported: false,
            export_set: None,
            anonymized: false,
        }
    }
}

/// Aggregate distribution returned by `training.stats`. Buckets
/// are 0.0..0.1, 0.1..0.2, ..., 0.9..1.0 — 10 buckets total.
/// The 1.0 cap rolls into the last bucket.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreDistribution {
    pub buckets: [u64; 10],
    /// Number of records with `quality_score IS NULL`.
    pub unscored: u64,
}

/// Aggregate per-agent / per-model counter pair used by
/// `training.stats`. Sorted descending by count by the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupedCount {
    pub label: String,
    pub count: u64,
}

/// Aggregate stats payload returned by `training.stats`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TrainingStats {
    pub total: u64,
    pub exported: u64,
    /// Mean of `quality_score` over rows where the column is
    /// non-null. `None` when no scored rows exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub average_quality_score: Option<f64>,
    pub score_distribution: ScoreDistribution,
    pub by_agent: Vec<GroupedCount>,
    pub by_model: Vec<GroupedCount>,
}

/// Light summary returned by `training.list_interactions` (the
/// full record body is not included so listings stay cheap).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InteractionSummary {
    pub interaction_id: InteractionId,
    pub session_id: String,
    pub agent: String,
    pub model: String,
    pub provider: String,
    pub latency_ms: u64,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_count: Option<u32>,
    pub recorded_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<f32>,
    pub exported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_set: Option<String>,
    /// First 80 chars of the user message — operators routinely
    /// want to eyeball "what was this?" without pulling the
    /// whole record.
    pub user_preview: String,
    /// Mirrors [`InteractionRecord::anonymized`] so list
    /// consumers can render the per-row redaction state without
    /// pulling the full record body.
    #[serde(default)]
    pub anonymized: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_id_from_request_is_lowercase_hex() {
        let req = relix_core::types::RequestId([0xab; 16]);
        let id = InteractionId::from_request(&req);
        assert_eq!(id.as_str(), &"ab".repeat(16));
        assert_eq!(id.as_str().len(), 32);
    }

    #[test]
    fn interaction_id_new_is_unique_per_call() {
        let a = InteractionId::new();
        let b = InteractionId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn new_record_computes_total_tokens() {
        let rec = InteractionRecord::new(
            InteractionId::new(),
            "sess".into(),
            "alice".into(),
            "gpt-4o-mini".into(),
            "openai".into(),
            "sys".into(),
            "u".into(),
            "r".into(),
            vec![],
            Some(30),
            Some(70),
            42,
            true,
            None,
            1_700_000_000_000,
        );
        assert_eq!(rec.token_count, Some(100));
        assert_eq!(rec.prompt_tokens, Some(30));
        assert!(!rec.exported);
        assert!(rec.quality_score.is_none());
    }

    #[test]
    fn new_record_skips_total_tokens_when_neither_side_known() {
        let rec = InteractionRecord::new(
            InteractionId::new(),
            "sess".into(),
            "alice".into(),
            "mock".into(),
            "mock".into(),
            "sys".into(),
            "u".into(),
            "r".into(),
            vec![],
            None,
            None,
            10,
            true,
            None,
            0,
        );
        assert!(rec.token_count.is_none());
    }
}
