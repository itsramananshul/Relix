//! Behavioral drift detection — checks whether a long-running
//! task's recent activity still aligns with its original goal.
//!
//! Drift is a known failure mode for autonomous agents: a task
//! starts as "draft a release announcement", N steps later the
//! agent is grepping the codebase for unrelated TODOs. The
//! detector compares the embedding of the original goal text
//! to the embedding of a short summary of the recent
//! chronicle events. When cosine similarity drops below the
//! configured threshold, the wrapper raises a structured
//! `drift_detected` event so the operator (or an automatic
//! policy) can intervene.
//!
//! ## Honest scope
//!
//! - This module ships the **detector primitive** plus the
//!   chronicle-summarisation helper. The detector is pure
//!   logic — no I/O, no embeddings — so tests can exercise it
//!   deterministically.
//! - The full coordinator integration (per-task counters,
//!   embedding RPC, pause-on-drift) lands as the wrapper
//!   reaches across nodes. Today the coordinator opts in
//!   through [`DriftConfig`] and logs the drift verdict to
//!   the chronicle when an embedding dispatcher becomes
//!   available; that wiring is documented in
//!   `controller_runtime.rs`.

use std::sync::Arc;

use serde::Deserialize;

/// Trait the coordinator's drift hook calls to embed the
/// goal text + recent-activity summary. Implementations
/// hand off to the AI node's `ai.embed` capability over the
/// mesh; tests inject stubs. `None` from `embed` means the
/// dispatcher could not produce a vector (provider doesn't
/// support embeddings, request failed, etc.) — the drift
/// hook treats that as a silent skip.
#[async_trait::async_trait]
pub trait DriftEmbedDispatcher: Send + Sync {
    async fn embed(&self, text: &str) -> Option<Vec<f32>>;
}

/// Type alias for the optional dispatcher Arc the coordinator
/// holds. Cheap to clone; absence means the drift hook records
/// only the textual summary without a similarity score.
pub type DriftEmbedDispatcherHandle = Option<Arc<dyn DriftEmbedDispatcher>>;

/// Process-wide OnceCell for the production drift embedder.
/// Wrapped as `Arc<OnceCell<...>>` so the controller can wire
/// a real `MeshDriftEmbedDispatcher` after startup (once the
/// `rpc::Client` is up and the AI peer alias is configured).
/// Empty cell == no embedder == similarity recorded as `none`.
pub type DriftEmbedDispatcherCell = Arc<tokio::sync::OnceCell<Arc<dyn DriftEmbedDispatcher>>>;

/// Mesh-backed [`DriftEmbedDispatcher`] that dials the AI peer
/// and calls `ai.embed` for a single text. Returns `None` on
/// any transport / decode error so the drift hook silently
/// skips the cosine computation rather than panicking.
///
/// Wire format mirror of the AI node's `handle_embed`:
/// request `model|text`, response `model|<b64 LE f32 packed>\n`.
pub struct MeshDriftEmbedDispatcher {
    mesh: crate::manifest::MeshClient,
    peer_alias: String,
    identity: relix_core::bundle::Bundle,
    deadline_secs: i64,
}

impl MeshDriftEmbedDispatcher {
    pub fn new(
        mesh: crate::manifest::MeshClient,
        peer_alias: String,
        identity: relix_core::bundle::Bundle,
        deadline_secs: i64,
    ) -> Self {
        Self {
            mesh,
            peer_alias,
            identity,
            deadline_secs,
        }
    }
}

#[async_trait::async_trait]
impl DriftEmbedDispatcher for MeshDriftEmbedDispatcher {
    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        use crate::dispatch::{build_request, decode_response};
        use crate::transport::envelope::ResponseResult;
        // AI node wire format: `model|text`. Empty model lets
        // the provider pick its default embedding model.
        let arg = format!("|{text}");
        let envelope = build_request(
            "ai.embed",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = match self.mesh.call(&self.peer_alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    peer = %self.peer_alias,
                    error = %e,
                    "drift embed: ai.embed call failed (silent skip)"
                );
                return None;
            }
        };
        let resp = decode_response(&resp_bytes).ok()?;
        let body_vec = match resp.res {
            ResponseResult::Ok(b) => b.to_vec(),
            ResponseResult::Err(env) => {
                tracing::debug!(
                    peer = %self.peer_alias,
                    cause = %env.cause,
                    "drift embed: ai.embed responder err (silent skip)"
                );
                return None;
            }
            ResponseResult::StreamHandle(_) => return None,
        };
        let body = std::str::from_utf8(&body_vec).ok()?.trim();
        // `<model>|<b64_vec_0>|<b64_vec_1>|...` — single text
        // means a single trailing vector field.
        let mut parts = body.split('|');
        let _model = parts.next()?;
        let b64 = parts.next()?;
        decode_embedding_b64(b64)
    }
}

/// Decode a base64-encoded little-endian f32 packed vector
/// (the wire format used by `ai.embed`).
fn decode_embedding_b64(b64: &str) -> Option<Vec<f32>> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    if raw.len() % 4 != 0 {
        return None;
    }
    Some(
        raw.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// Default cosine-similarity floor below which we declare
/// drift. Empirically picked to catch "the agent wandered to
/// a different topic" without firing on legitimate
/// exploration.
pub const DEFAULT_DRIFT_THRESHOLD: f32 = 0.65;

/// Default number of chronicle steps between drift checks.
/// 10 strikes the balance between catching drift early and
/// not paying for an embedding RPC on every task update.
pub const DEFAULT_CHECK_EVERY_N: u32 = 10;

/// Maximum number of events the summariser folds into the
/// "recent activity" string. Cap exists so the embedding
/// model never sees a wall of chronicle history — only the
/// last few steps.
pub const MAX_SUMMARY_EVENTS: usize = 16;

/// Action the coordinator takes when drift is detected.
/// Default `warn` so opt-in operators see drift in the
/// chronicle without any blast radius.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DriftAction {
    #[default]
    Warn,
    Pause,
    Stop,
}

impl DriftAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Pause => "pause",
            Self::Stop => "stop",
        }
    }
}

/// `[guardrails.drift]` config block. Absent / `enabled =
/// false` means the detector primitive still works for tests
/// but the coordinator's task.update path skips the check.
#[derive(Clone, Debug, Deserialize)]
pub struct DriftConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_threshold")]
    pub threshold: f32,
    #[serde(default = "default_check_every_n")]
    pub check_every_n: u32,
    #[serde(default)]
    pub action: DriftAction,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_threshold(),
            check_every_n: default_check_every_n(),
            action: DriftAction::default(),
        }
    }
}

fn default_threshold() -> f32 {
    DEFAULT_DRIFT_THRESHOLD
}

fn default_check_every_n() -> u32 {
    DEFAULT_CHECK_EVERY_N
}

/// Lightweight view of one chronicle event — just enough for
/// the summariser. The coordinator's full `TaskEvent` carries
/// more (event_id, ts, attempt_id, …); we keep the shape
/// narrow so the detector doesn't take a dependency on the
/// coordinator's data layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChronicleEvent {
    pub event_type: String,
    pub payload: String,
}

impl ChronicleEvent {
    pub fn new(event_type: impl Into<String>, payload: impl Into<String>) -> Self {
        Self {
            event_type: event_type.into(),
            payload: payload.into(),
        }
    }
}

/// Pure-logic drift detector. Cheap to clone (three primitive
/// fields).
#[derive(Clone, Debug)]
pub struct DriftDetector {
    threshold: f32,
    check_every_n: u32,
}

impl DriftDetector {
    pub fn new(threshold: f32, check_every_n: u32) -> Self {
        Self {
            threshold,
            check_every_n: check_every_n.max(1),
        }
    }

    pub fn from_config(cfg: &DriftConfig) -> Self {
        Self::new(cfg.threshold, cfg.check_every_n)
    }

    /// `true` when the agent is drifting — i.e. cosine
    /// similarity between goal + recent-activity embeddings
    /// has dropped BELOW the configured threshold.
    pub fn is_drifting(&self, goal_embedding: &[f32], recent_embedding: &[f32]) -> bool {
        cosine_similarity(goal_embedding, recent_embedding) < self.threshold
    }

    /// Format a short text summary of the N most recent
    /// chronicle events. `None` when fewer than
    /// `check_every_n` events exist — the detector needs at
    /// least a window's worth of activity before it can
    /// reasonably say "the agent has drifted."
    pub fn summarise_recent_events(&self, events: &[ChronicleEvent]) -> Option<String> {
        if events.len() < self.check_every_n as usize {
            return None;
        }
        let start = events.len().saturating_sub(MAX_SUMMARY_EVENTS);
        let mut out = String::new();
        for (i, e) in events[start..].iter().enumerate() {
            let payload = e.payload.trim();
            let trimmed_payload = if payload.len() > 200 {
                // Snap 200 down to a char boundary so multi-byte
                // payload text isn't sliced mid-codepoint (panics).
                let mut cut = 200;
                while cut > 0 && !payload.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &payload[..cut])
            } else {
                payload.to_string()
            };
            if trimmed_payload.is_empty() {
                out.push_str(&format!("{}. {}\n", i + 1, e.event_type));
            } else {
                out.push_str(&format!(
                    "{}. {}: {}\n",
                    i + 1,
                    e.event_type,
                    trimmed_payload
                ));
            }
        }
        Some(out)
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn check_every_n(&self) -> u32 {
        self.check_every_n
    }
}

/// Cosine similarity. Public so the coordinator hook can
/// reuse the same impl when scoring drift events without
/// pulling the whole detector through. Returns 0.0 on
/// mismatched lengths or zero-norm input.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(n: usize) -> Vec<ChronicleEvent> {
        (0..n)
            .map(|i| ChronicleEvent::new(format!("step.{i}"), format!("ran action {i}")))
            .collect()
    }

    #[test]
    fn is_drifting_below_threshold_returns_true() {
        let d = DriftDetector::new(0.7, 10);
        // Orthogonal vectors → cosine 0.0 < 0.7 → drift.
        assert!(d.is_drifting(&[1.0, 0.0], &[0.0, 1.0]));
    }

    #[test]
    fn is_drifting_above_threshold_returns_false() {
        let d = DriftDetector::new(0.7, 10);
        // Parallel vectors → cosine 1.0 >= 0.7 → no drift.
        assert!(!d.is_drifting(&[1.0, 0.0], &[1.0, 0.0]));
        // Near-parallel.
        assert!(!d.is_drifting(&[1.0, 0.1], &[1.0, 0.0]));
    }

    #[test]
    fn is_drifting_handles_zero_and_mismatched_vectors() {
        let d = DriftDetector::new(0.5, 10);
        // Mismatched length → cosine returns 0.0 → drift.
        assert!(d.is_drifting(&[1.0], &[1.0, 0.0]));
        // Zero vector → cosine returns 0.0 → drift.
        assert!(d.is_drifting(&[0.0, 0.0], &[1.0, 0.0]));
    }

    #[test]
    fn summarise_returns_none_when_not_enough_events() {
        let d = DriftDetector::new(0.7, 10);
        let few = events(5);
        assert!(d.summarise_recent_events(&few).is_none());
        // Exactly the window — still under because we need
        // strictly `>= check_every_n`. 10 items meets the bar.
        let exactly = events(10);
        assert!(d.summarise_recent_events(&exactly).is_some());
    }

    #[test]
    fn summarise_formats_event_type_and_payload() {
        let d = DriftDetector::new(0.7, 3);
        let evts = vec![
            ChronicleEvent::new("task.run", "fetched config"),
            ChronicleEvent::new("task.run", "parsed args"),
            ChronicleEvent::new("task.run", "called provider"),
        ];
        let summary = d.summarise_recent_events(&evts).unwrap();
        assert!(summary.contains("task.run: fetched config"));
        assert!(summary.contains("task.run: parsed args"));
        assert!(summary.contains("task.run: called provider"));
        // Lines are 1-indexed.
        assert!(summary.starts_with("1. "));
    }

    #[test]
    fn summarise_caps_at_max_summary_events() {
        let d = DriftDetector::new(0.7, 5);
        let many = events(50);
        let summary = d.summarise_recent_events(&many).unwrap();
        // The summary keeps only the last MAX_SUMMARY_EVENTS
        // lines — earlier events are dropped.
        assert!(!summary.contains("ran action 0"));
        assert!(summary.contains(&format!("ran action {}", 50 - 1)));
    }

    #[test]
    fn summarise_truncates_long_payloads() {
        let d = DriftDetector::new(0.7, 1);
        let big_payload = "x".repeat(500);
        let evts = vec![ChronicleEvent::new("task.run", big_payload)];
        let summary = d.summarise_recent_events(&evts).unwrap();
        assert!(summary.contains("…"));
        // Truncated payload + ellipsis fits in the cap.
        assert!(summary.len() < 500);
    }

    #[test]
    fn drift_config_defaults_to_disabled_with_documented_thresholds() {
        let cfg = DriftConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - DEFAULT_DRIFT_THRESHOLD).abs() < 1e-6);
        assert_eq!(cfg.check_every_n, DEFAULT_CHECK_EVERY_N);
        assert_eq!(cfg.action, DriftAction::Warn);
    }

    #[test]
    fn drift_action_parses_round_trip_via_serde() {
        let cfg: DriftConfig = toml::from_str(
            r#"
                enabled = true
                threshold = 0.5
                check_every_n = 3
                action = "pause"
            "#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.action, DriftAction::Pause);
        assert_eq!(cfg.action.as_str(), "pause");
    }

    #[test]
    fn decode_embedding_b64_round_trips_via_ai_embed_wire_format() {
        // The AI node packs each vector as little-endian f32
        // bytes then base64-STANDARD encodes. The drift
        // embedder MUST be the mirror.
        use base64::Engine;
        let vec_in = vec![0.1f32, -0.5, 1.25, 42.0];
        let mut raw: Vec<u8> = Vec::with_capacity(vec_in.len() * 4);
        for x in &vec_in {
            raw.extend_from_slice(&x.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let decoded = decode_embedding_b64(&b64).expect("decode succeeds");
        assert_eq!(decoded.len(), vec_in.len());
        for (a, b) in decoded.iter().zip(vec_in.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn decode_embedding_b64_returns_none_on_garbage_input() {
        assert!(decode_embedding_b64("!!not-base64!!").is_none());
        // Truncated bytes (not divisible by 4) → None.
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        assert!(decode_embedding_b64(&b64).is_none());
    }

    #[test]
    fn check_every_n_clamped_to_minimum_of_one() {
        let d = DriftDetector::new(0.7, 0);
        assert_eq!(d.check_every_n(), 1);
        // With check_every_n=1, a single event yields a
        // non-empty summary.
        let evts = vec![ChronicleEvent::new("step", "did one thing")];
        let summary = d.summarise_recent_events(&evts).unwrap();
        assert!(summary.contains("did one thing"));
    }
}
