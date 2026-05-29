//! RELIX-7.29 PART 3 — LLM-driven Belief State Tracking.
//!
//! This is the §7.29 Component 3 design: instead of the
//! operator-curated structured BeliefStore (the pre-rebuild
//! `reasoning::BeliefStore`), the AI handler asks a *small*
//! belief model — defaulting to the same provider with a
//! cheap model — to extract per-session beliefs from the
//! running conversation. The flow:
//!
//! 1. Before each `ai.chat`, the handler reads the current
//!    belief block for `(subject_id, session_id)` and
//!    prepends it to the system prompt as
//!    `[Current beliefs about this conversation]\n<bullets>`.
//! 2. After `ai.chat` returns to the caller, a *non-blocking*
//!    `tokio::spawn` fires the belief-update prompt against
//!    the configured belief model. The model is asked to
//!    return a JSON array of `{ text, confidence }` items.
//!    Items below `min_confidence_to_retain` are dropped; the
//!    list is truncated to `max_beliefs`.
//! 3. The store is keyed by `(subject_id, session_id)` and
//!    persists for the controller's lifetime. Operators read
//!    it via the `belief.get` cap and the
//!    `GET /v1/belief/:session_id` bridge endpoint; they
//!    clear it via `belief.reset` and
//!    `POST /v1/belief/:session_id` (with `action=reset`).
//!
//! The store is intentionally process-local: spec calls for a
//! Layer 4 memory record stamped with the tag `belief_state`,
//! and the in-memory ring is the functional equivalent for
//! controller-scoped state. Persisting it across restarts is a
//! follow-up that plugs in via the existing memory peer (out
//! of scope for the §7.29 closure).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::provider::ChatInput;

/// `[ai.belief_state]` config block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BeliefStateConfig {
    /// Master switch. `false` (the default) keeps the AI
    /// handler byte-identical to its pre-belief behaviour.
    #[serde(default)]
    pub enabled: bool,
    /// Optional provider name override. When unset, the
    /// belief update runs against the *same* provider as
    /// `ai.chat`. Operators with two providers (e.g. a cheap
    /// belief model and an expensive chat model) can split.
    #[serde(default)]
    pub belief_model: Option<String>,
    /// Belief model id. Empty means "let the provider pick
    /// its default cheap model".
    #[serde(default)]
    pub belief_model_name: String,
    /// Maximum number of beliefs to retain per session.
    /// Default 10.
    #[serde(default = "default_max_beliefs")]
    pub max_beliefs: usize,
    /// Confidence floor — beliefs with `confidence <` this
    /// value are dropped on every update. Default 0.55.
    #[serde(default = "default_min_confidence_to_retain")]
    pub min_confidence_to_retain: f32,
    /// When `true` (the default), `handle_chat` prepends the
    /// belief block to the system prompt. Operators disable
    /// this to keep beliefs visible via the cap surface
    /// without coupling them into the model context.
    #[serde(default = "default_inject_into_prompt")]
    pub inject_into_prompt: bool,
}

impl Default for BeliefStateConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            belief_model: None,
            belief_model_name: String::new(),
            max_beliefs: default_max_beliefs(),
            min_confidence_to_retain: default_min_confidence_to_retain(),
            inject_into_prompt: default_inject_into_prompt(),
        }
    }
}

fn default_max_beliefs() -> usize {
    10
}

fn default_min_confidence_to_retain() -> f32 {
    0.55
}

fn default_inject_into_prompt() -> bool {
    true
}

/// One per-session belief — what the model thinks it knows
/// about the conversation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Belief {
    /// One short sentence describing the belief.
    pub text: String,
    /// Belief model's self-reported confidence in `[0, 1]`.
    pub confidence: f32,
}

/// Composite key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SessionKey {
    subject_id: String,
    session_id: String,
}

/// Process-local belief tracker. Cheap to clone (one
/// `Arc<Mutex<HashMap>>`). The AI handler shares one instance
/// across every `ai.chat` invocation; the coordinator caps
/// share the same instance so reads + resets see the same
/// store the handler writes to.
#[derive(Clone, Default)]
pub struct BeliefStateTracker {
    inner: Arc<Mutex<HashMap<SessionKey, Vec<Belief>>>>,
    cfg: BeliefStateConfig,
}

impl BeliefStateTracker {
    pub fn new(cfg: BeliefStateConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            cfg,
        }
    }

    /// Operator's `[ai.belief_state]` settings.
    pub fn config(&self) -> &BeliefStateConfig {
        &self.cfg
    }

    /// `true` when the tracker is enabled.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Read the current beliefs for `(subject_id, session_id)`.
    /// Returns an empty Vec when no beliefs have been recorded.
    pub fn get(&self, subject_id: &str, session_id: &str) -> Vec<Belief> {
        let key = SessionKey {
            subject_id: subject_id.to_string(),
            session_id: session_id.to_string(),
        };
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.get(&key).cloned().unwrap_or_default()
    }

    /// Replace the belief list. The list is filtered by
    /// `min_confidence_to_retain` and truncated to
    /// `max_beliefs` before being stored.
    pub fn set(&self, subject_id: &str, session_id: &str, mut beliefs: Vec<Belief>) {
        beliefs.retain(|b| b.confidence >= self.cfg.min_confidence_to_retain);
        beliefs.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        beliefs.truncate(self.cfg.max_beliefs);
        let key = SessionKey {
            subject_id: subject_id.to_string(),
            session_id: session_id.to_string(),
        };
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.insert(key, beliefs);
    }

    /// Clear the belief list for `(subject_id, session_id)`.
    /// Returns `true` when an entry existed.
    pub fn reset(&self, subject_id: &str, session_id: &str) -> bool {
        let key = SessionKey {
            subject_id: subject_id.to_string(),
            session_id: session_id.to_string(),
        };
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&key).is_some()
    }

    /// Number of `(subject, session)` entries currently held —
    /// dashboards use this to track per-controller memory use.
    pub fn len(&self) -> usize {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Build the system-prompt prefix from a belief list. Returns
/// an empty string when the list is empty.
pub fn format_for_system_prompt(beliefs: &[Belief]) -> String {
    if beliefs.is_empty() {
        return String::new();
    }
    let mut out = String::from("[Current beliefs about this conversation]\n");
    for b in beliefs {
        out.push_str("- ");
        out.push_str(b.text.trim());
        out.push_str(&format!(" (confidence: {:.2})\n", b.confidence));
    }
    out.push('\n');
    out
}

/// Build the structured prompt the belief model sees. Asks
/// for a JSON array of `{ text, confidence }` items.
pub fn build_update_prompt(
    existing: &[Belief],
    user_message: &str,
    assistant_reply: &str,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(
        "You are a belief-state tracker. Read the conversation turn and \
         return an updated JSON array of beliefs about this conversation.\n\n",
    );
    out.push_str("Existing beliefs:\n");
    if existing.is_empty() {
        out.push_str("(none)\n");
    } else {
        for b in existing {
            out.push_str(&format!(
                "- {} (confidence: {:.2})\n",
                b.text.trim(),
                b.confidence
            ));
        }
    }
    out.push_str("\nLatest user message:\n");
    out.push_str(user_message.trim());
    out.push_str("\n\nLatest assistant reply:\n");
    out.push_str(assistant_reply.trim());
    out.push_str(
        "\n\nReturn ONLY a JSON array. Each item must have:\n\
         - text: one short sentence (string)\n\
         - confidence: number in [0, 1]\n\
         Do not include code fences, prose, or trailing text — only the JSON \
         array. Example: [{\"text\": \"user is debugging a Rust build\", \
         \"confidence\": 0.82}]",
    );
    out
}

/// Parse the belief model's JSON response.
pub fn parse_update_response(raw: &str) -> Result<Vec<Belief>, ParseError> {
    let trimmed = trim_json_fences(raw);
    let items: Vec<Belief> =
        serde_json::from_str(&trimmed).map_err(|e| ParseError::Decode(e.to_string()))?;
    Ok(items
        .into_iter()
        .filter(|b| (0.0..=1.0).contains(&b.confidence) && !b.text.trim().is_empty())
        .collect())
}

/// Errors from [`parse_update_response`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("belief decode: {0}")]
    Decode(String),
}

/// Strip a leading/trailing ```json fence the belief model
/// might emit despite the prompt.
fn trim_json_fences(s: &str) -> String {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        t = rest.trim_start();
    } else if let Some(rest) = t.strip_prefix("```") {
        t = rest.trim_start();
    }
    if let Some(rest) = t.strip_suffix("```") {
        t = rest.trim_end();
    }
    t.to_string()
}

/// `belief.get` + `belief.reset` coordinator caps.
pub mod caps {
    use std::sync::Arc;

    use relix_core::types::{ErrorEnvelope, error_kinds};
    use serde::Deserialize;

    use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

    use super::BeliefStateTracker;

    /// Wire `belief.get` + `belief.reset` onto `bridge`.
    pub fn register(bridge: &mut DispatchBridge, tracker: BeliefStateTracker) {
        {
            let tracker = tracker.clone();
            bridge.register(
                "belief.get",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let tracker = tracker.clone();
                    async move { handle_get(&tracker, &ctx) }
                })),
            );
        }
        {
            bridge.register(
                "belief.reset",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let tracker = tracker.clone();
                    async move { handle_reset(&tracker, &ctx) }
                })),
            );
        }
    }

    #[derive(Debug, Deserialize, Default)]
    struct BeliefArgs {
        #[serde(default)]
        subject_id: String,
        #[serde(default)]
        session_id: String,
    }

    fn handle_get(tracker: &BeliefStateTracker, ctx: &InvocationCtx) -> HandlerOutcome {
        let args = match decode_args(ctx) {
            Ok(a) => a,
            Err(out) => return out,
        };
        let subject = effective_subject(&args, ctx);
        if args.session_id.trim().is_empty() {
            return invalid("session_id is required");
        }
        let beliefs = tracker.get(&subject, &args.session_id);
        let body = serde_json::json!({
            "subject_id": subject,
            "session_id": args.session_id,
            "beliefs": beliefs,
            "enabled": tracker.enabled(),
        });
        ok_json(&body)
    }

    fn handle_reset(tracker: &BeliefStateTracker, ctx: &InvocationCtx) -> HandlerOutcome {
        let args = match decode_args(ctx) {
            Ok(a) => a,
            Err(out) => return out,
        };
        let subject = effective_subject(&args, ctx);
        if args.session_id.trim().is_empty() {
            return invalid("session_id is required");
        }
        let cleared = tracker.reset(&subject, &args.session_id);
        let body = serde_json::json!({
            "subject_id": subject,
            "session_id": args.session_id,
            "cleared": cleared,
        });
        ok_json(&body)
    }

    fn decode_args(ctx: &InvocationCtx) -> Result<BeliefArgs, HandlerOutcome> {
        if ctx.args.is_empty() {
            return Ok(BeliefArgs::default());
        }
        serde_json::from_slice(&ctx.args).map_err(|e| invalid(&format!("belief: decode args: {e}")))
    }

    fn effective_subject(args: &BeliefArgs, ctx: &InvocationCtx) -> String {
        if !args.subject_id.trim().is_empty() {
            return args.subject_id.clone();
        }
        ctx.caller.subject_id.to_string()
    }

    fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
        match serde_json::to_vec(value) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("belief: encode response: {e}"),
                retry_hint: 0,
                retry_after: None,
            }),
        }
    }

    fn invalid(msg: &str) -> HandlerOutcome {
        HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: msg.to_string(),
            retry_hint: 0,
            retry_after: None,
        })
    }
}

/// Build the [`ChatInput`] for the belief update call. The AI
/// handler hands this to its provider (or to a separate
/// belief-model provider when wired) to run the update.
pub fn build_update_input(
    cfg: &BeliefStateConfig,
    session_id: &str,
    existing: &[Belief],
    user_message: &str,
    assistant_reply: &str,
) -> ChatInput {
    ChatInput {
        session_id: format!("{session_id}::belief"),
        prompt: build_update_prompt(existing, user_message, assistant_reply),
        history: String::new(),
        model: cfg.belief_model_name.clone(),
        system_prompt: Some(
            "You are an impartial belief-state tracker. Be concise and conservative.".to_string(),
        ),
        ..ChatInput::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let cfg = BeliefStateConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.max_beliefs, 10);
        assert!(cfg.inject_into_prompt);
    }

    #[test]
    fn set_filters_out_low_confidence_and_truncates() {
        let cfg = BeliefStateConfig {
            enabled: true,
            max_beliefs: 2,
            min_confidence_to_retain: 0.6,
            ..Default::default()
        };
        let t = BeliefStateTracker::new(cfg);
        t.set(
            "subj",
            "sess",
            vec![
                Belief {
                    text: "alpha".into(),
                    confidence: 0.9,
                },
                Belief {
                    text: "beta".into(),
                    confidence: 0.4, // dropped
                },
                Belief {
                    text: "gamma".into(),
                    confidence: 0.7,
                },
                Belief {
                    text: "delta".into(),
                    confidence: 0.65,
                },
            ],
        );
        let got = t.get("subj", "sess");
        assert_eq!(got.len(), 2, "got {got:?}");
        // Sorted by confidence desc — alpha > gamma.
        assert_eq!(got[0].text, "alpha");
        assert_eq!(got[1].text, "gamma");
    }

    #[test]
    fn reset_returns_false_when_missing_and_true_when_present() {
        let t = BeliefStateTracker::new(BeliefStateConfig {
            enabled: true,
            min_confidence_to_retain: 0.0,
            ..Default::default()
        });
        assert!(!t.reset("a", "b"));
        t.set(
            "a",
            "b",
            vec![Belief {
                text: "x".into(),
                confidence: 0.9,
            }],
        );
        assert!(t.reset("a", "b"));
        assert!(t.get("a", "b").is_empty());
    }

    #[test]
    fn format_for_system_prompt_skips_when_empty() {
        assert!(format_for_system_prompt(&[]).is_empty());
    }

    #[test]
    fn format_for_system_prompt_emits_bullet_list_with_confidence() {
        let s = format_for_system_prompt(&[
            Belief {
                text: "user wants Rust help".into(),
                confidence: 0.82,
            },
            Belief {
                text: "build is failing on linker".into(),
                confidence: 0.71,
            },
        ]);
        assert!(s.starts_with("[Current beliefs about this conversation]\n"));
        assert!(s.contains("user wants Rust help"));
        assert!(s.contains("0.82"));
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn build_update_prompt_lists_existing_beliefs_or_none() {
        let p = build_update_prompt(&[], "hi", "hello");
        assert!(p.contains("(none)"));
        let p = build_update_prompt(
            &[Belief {
                text: "x".into(),
                confidence: 0.9,
            }],
            "hi",
            "hello",
        );
        assert!(p.contains("- x (confidence: 0.90)"));
    }

    #[test]
    fn parse_update_response_handles_bare_array() {
        let body = r#"[{"text": "user is curious", "confidence": 0.8}]"#;
        let items = parse_update_response(body).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "user is curious");
    }

    #[test]
    fn parse_update_response_handles_fenced_array() {
        let body = "```json\n[{\"text\": \"a\", \"confidence\": 0.7}]\n```";
        let items = parse_update_response(body).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn parse_update_response_drops_out_of_range_or_empty_text() {
        let body = r#"[
            {"text": "valid", "confidence": 0.8},
            {"text": "", "confidence": 0.9},
            {"text": "too high", "confidence": 1.5},
            {"text": "neg", "confidence": -0.1}
        ]"#;
        let items = parse_update_response(body).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "valid");
    }

    #[test]
    fn parse_update_response_rejects_garbage() {
        assert!(parse_update_response("not json").is_err());
    }

    #[test]
    fn build_update_input_targets_isolated_belief_session() {
        let cfg = BeliefStateConfig {
            enabled: true,
            belief_model_name: "cheap-model".into(),
            ..Default::default()
        };
        let input = build_update_input(&cfg, "sess1", &[], "user", "assistant");
        assert_eq!(input.session_id, "sess1::belief");
        assert_eq!(input.model, "cheap-model");
        assert!(input.system_prompt.is_some());
    }
}
