//! RELIX-7.29 PART 4 — Judge Model.
//!
//! §7.29 Component 4. After the AI handler has produced a
//! response AND self-consistency / belief updates have run,
//! the judge fires on a tightly-gated subset of calls:
//!
//! - `[ai.judge] enabled = true`
//! - The final confidence dropped below `judge_threshold`
//!   (default 0.6)
//! - The response carries a tool call OR a structured-output
//!   marker (JSON / TOML / YAML fenced block, or a leading `{`
//!   / `[`)
//! - The session has at least 2 prior turns
//!
//! When all four are true, the handler dispatches a second
//! provider call (`generate_reply`) against the configured
//! `judge_model_name`, capped at `max_judge_latency_ms`. The
//! judge model is asked five questions and returns a JSON
//! object:
//!
//! ```json
//! {
//!   "answers_question": "yes" | "no" | "partial",
//!   "action_is_safe": "yes" | "no" | "needs_review",
//!   "factual_errors": ["..."],
//!   "overconfident": true | false,
//!   "verdict": "proceed" | "modify" | "block"
//! }
//! ```
//!
//! The judge's verdict is recorded in a process-local ring
//! buffer (default 256 entries) surfaced by the
//! `judge.recent_verdicts` + `judge.stats` coordinator caps.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::provider::ChatInput;

/// `[ai.judge]` config block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JudgeConfig {
    /// Master switch. `false` (the default) keeps the AI
    /// handler byte-identical to its pre-judge behaviour.
    #[serde(default)]
    pub enabled: bool,
    /// Optional provider name override. When unset, the
    /// judge call runs against the same provider as `ai.chat`.
    #[serde(default)]
    pub judge_model: Option<String>,
    /// Judge model id. Empty means "let the provider pick its
    /// default cheap model".
    #[serde(default)]
    pub judge_model_name: String,
    /// Confidence ceiling — when the final confidence is at-
    /// or-above this value the judge does NOT fire. Default
    /// 0.6.
    #[serde(default = "default_judge_threshold")]
    pub judge_threshold: f32,
    /// Hard timeout in ms on the judge call. Default 6000ms.
    /// Exceeding this returns a synthetic `proceed` verdict
    /// with a timeout note so the handler doesn't stall.
    #[serde(default = "default_max_judge_latency_ms")]
    pub max_judge_latency_ms: u64,
    /// Ring buffer depth for the `judge.recent_verdicts` cap.
    /// Default 256.
    #[serde(default = "default_ring_size")]
    pub recent_buffer_size: usize,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            judge_model: None,
            judge_model_name: String::new(),
            judge_threshold: default_judge_threshold(),
            max_judge_latency_ms: default_max_judge_latency_ms(),
            recent_buffer_size: default_ring_size(),
        }
    }
}

fn default_judge_threshold() -> f32 {
    0.6
}

fn default_max_judge_latency_ms() -> u64 {
    6_000
}

fn default_ring_size() -> usize {
    256
}

/// The judge's final verdict. The handler honours `Modify` by
/// surfacing the judge's notes alongside the original
/// response; `Block` causes the handler to return an error
/// outcome (the operator's fallback engine can then escalate).
/// `Proceed` is the default for the activation gate not being
/// met OR a timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JudgeAction {
    Proceed,
    Modify,
    Block,
}

impl JudgeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            JudgeAction::Proceed => "proceed",
            JudgeAction::Modify => "modify",
            JudgeAction::Block => "block",
        }
    }
}

/// Three-state Likert used by `answers_question` +
/// `action_is_safe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeStance {
    Yes,
    No,
    Partial,
    NeedsReview,
}

/// Parsed judge response.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// Did the response answer the user's question?
    pub answers_question: JudgeStance,
    /// Is the proposed action safe?
    pub action_is_safe: JudgeStance,
    /// List of suspected factual errors (one short phrase
    /// each). Empty when the judge sees none.
    #[serde(default)]
    pub factual_errors: Vec<String>,
    /// Whether the assistant was overconfident.
    pub overconfident: bool,
    /// Final action the dispatcher must honour.
    pub verdict: JudgeAction,
}

impl JudgeVerdict {
    /// Default "the judge did not run" verdict — used when
    /// the activation gate fails or the call times out.
    pub fn proceed_default(reason: &'static str) -> Self {
        Self {
            answers_question: JudgeStance::Yes,
            action_is_safe: JudgeStance::Yes,
            factual_errors: vec![],
            overconfident: false,
            verdict: JudgeAction::Proceed,
        }
        .with_reason(reason)
    }

    fn with_reason(self, _reason: &'static str) -> Self {
        // Reason is currently logged but not stored on the
        // verdict struct; the dispatcher emits it via
        // `tracing::info!` when this default is constructed.
        self
    }
}

/// Activation gate. Returns `true` when every spec'd
/// condition is satisfied.
pub fn should_invoke(
    cfg: &JudgeConfig,
    final_confidence: f32,
    response_text: &str,
    prior_session_turns: u32,
) -> bool {
    if !cfg.enabled {
        return false;
    }
    if final_confidence >= cfg.judge_threshold {
        return false;
    }
    if prior_session_turns < 2 {
        return false;
    }
    has_tool_call_or_structured_marker(response_text)
}

/// Heuristic: `true` when the response contains a tool-call
/// marker OR a structured-output marker. The judge's
/// activation gate cares about responses where the *action*
/// matters more than the phrasing — a paragraph of free text
/// gets a pass.
pub fn has_tool_call_or_structured_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Common tool-call openers used by the existing planner.
    const TOOL_OPENERS: &[&str] = &[
        "tool_call",
        "function_call",
        "<tool_call>",
        "[tool]",
        "tool:",
        "action:",
        "action_input:",
    ];
    if TOOL_OPENERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // Fenced structured output.
    if lower.contains("```json") || lower.contains("```toml") || lower.contains("```yaml") {
        return true;
    }
    // Leading JSON / array.
    let trimmed = text.trim_start();
    matches!(trimmed.chars().next(), Some('{') | Some('['))
}

/// Build the structured judge prompt.
pub fn build_judge_prompt(user_message: &str, response_text: &str) -> String {
    let mut out = String::with_capacity(512);
    out.push_str(
        "You are an impartial Judge model. Read the user's question and the \
         assistant's response, then answer the following five questions and \
         return a single JSON object.\n\n",
    );
    out.push_str("User question:\n");
    out.push_str(user_message.trim());
    out.push_str("\n\nAssistant response:\n");
    out.push_str(response_text.trim());
    out.push_str(
        "\n\nReturn ONLY the JSON object. The shape MUST be:\n\
         {\n\
         \"answers_question\": \"yes\" | \"no\" | \"partial\",\n\
         \"action_is_safe\": \"yes\" | \"no\" | \"needs_review\",\n\
         \"factual_errors\": [\"...\"],\n\
         \"overconfident\": true | false,\n\
         \"verdict\": \"proceed\" | \"modify\" | \"block\"\n\
         }\n\
         No code fences. No prose. JSON only.",
    );
    out
}

/// Parse the judge model's raw text into a [`JudgeVerdict`].
pub fn parse_judge_response(raw: &str) -> Result<JudgeVerdict, ParseError> {
    let trimmed = trim_json_fences(raw);
    serde_json::from_str(&trimmed).map_err(|e| ParseError::Decode(e.to_string()))
}

/// Errors from [`parse_judge_response`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("judge decode: {0}")]
    Decode(String),
}

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

/// Build the [`ChatInput`] for the judge call.
pub fn build_judge_input(
    cfg: &JudgeConfig,
    session_id: &str,
    user_message: &str,
    response_text: &str,
) -> ChatInput {
    ChatInput {
        session_id: format!("{session_id}::judge"),
        prompt: build_judge_prompt(user_message, response_text),
        history: String::new(),
        model: cfg.judge_model_name.clone(),
        system_prompt: Some("You are an impartial Judge model. Be concise.".to_string()),
        ..ChatInput::default()
    }
}

/// One recorded verdict — surfaced by the
/// `judge.recent_verdicts` cap.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerdictRecord {
    /// Caller agent.
    pub agent: String,
    /// Session id.
    pub session_id: String,
    /// Wall-clock time as milliseconds since the unix epoch.
    pub timestamp_ms: i64,
    /// Final confidence at the time the judge fired.
    pub final_confidence: f32,
    /// Whether the call timed out at `max_judge_latency_ms`.
    pub timed_out: bool,
    /// The parsed verdict — or a synthetic `proceed` when the
    /// judge could not be parsed / timed out.
    pub verdict: JudgeVerdict,
}

/// Process-local ring buffer + counters surfaced by the
/// `judge.*` coordinator caps.
#[derive(Clone)]
pub struct JudgeRecorder {
    inner: Arc<Mutex<JudgeRecorderInner>>,
}

struct JudgeRecorderInner {
    recent: VecDeque<VerdictRecord>,
    capacity: usize,
    proceed_count: u64,
    modify_count: u64,
    block_count: u64,
    timeout_count: u64,
    per_agent: HashMap<String, AgentStat>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct AgentStat {
    pub proceed: u64,
    pub modify: u64,
    pub block: u64,
    pub timeout: u64,
}

impl JudgeRecorder {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(JudgeRecorderInner {
                recent: VecDeque::with_capacity(capacity.max(1)),
                capacity: capacity.max(1),
                proceed_count: 0,
                modify_count: 0,
                block_count: 0,
                timeout_count: 0,
                per_agent: HashMap::new(),
            })),
        }
    }

    pub fn record(&self, rec: VerdictRecord) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let cap = g.capacity;
        if g.recent.len() == cap {
            g.recent.pop_front();
        }
        if rec.timed_out {
            g.timeout_count += 1;
        }
        match rec.verdict.verdict {
            JudgeAction::Proceed => g.proceed_count += 1,
            JudgeAction::Modify => g.modify_count += 1,
            JudgeAction::Block => g.block_count += 1,
        }
        let stat = g.per_agent.entry(rec.agent.clone()).or_default();
        if rec.timed_out {
            stat.timeout += 1;
        }
        match rec.verdict.verdict {
            JudgeAction::Proceed => stat.proceed += 1,
            JudgeAction::Modify => stat.modify += 1,
            JudgeAction::Block => stat.block += 1,
        }
        g.recent.push_back(rec);
    }

    pub fn recent(&self, limit: usize) -> Vec<VerdictRecord> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let l = limit.min(g.recent.len());
        g.recent.iter().rev().take(l).cloned().collect()
    }

    pub fn stats(&self) -> JudgeStatsSnapshot {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        JudgeStatsSnapshot {
            proceed_count: g.proceed_count,
            modify_count: g.modify_count,
            block_count: g.block_count,
            timeout_count: g.timeout_count,
            recent_buffered: g.recent.len() as u64,
            capacity: g.capacity as u64,
            per_agent: g
                .per_agent
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

impl Default for JudgeRecorder {
    fn default() -> Self {
        Self::new(default_ring_size())
    }
}

/// JSON-serialisable stats view returned by `judge.stats`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct JudgeStatsSnapshot {
    pub proceed_count: u64,
    pub modify_count: u64,
    pub block_count: u64,
    pub timeout_count: u64,
    pub recent_buffered: u64,
    pub capacity: u64,
    #[serde(default)]
    pub per_agent: HashMap<String, AgentStat>,
}

/// Public helper the AI handler uses: returns the timeout
/// `Duration` derived from config.
pub fn timeout_for(cfg: &JudgeConfig) -> Duration {
    Duration::from_millis(cfg.max_judge_latency_ms)
}

/// Per-session turn counter — used by the activation gate.
#[derive(Clone, Default)]
pub struct SessionTurnCounter {
    inner: Arc<Mutex<HashMap<String, u32>>>,
}

impl SessionTurnCounter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment and return the *prior* turn count for `session`.
    /// The handler calls this once per `ai.chat`; the returned
    /// value represents how many prior turns this session has
    /// accumulated, which feeds the judge activation gate.
    pub fn bump(&self, session: &str) -> u32 {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let v = g.entry(session.to_string()).or_insert(0);
        let prior = *v;
        *v = v.saturating_add(1);
        prior
    }

    pub fn reset(&self, session: &str) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(session).is_some()
    }
}

/// `judge.recent_verdicts` + `judge.stats` coordinator caps.
pub mod caps {
    use std::sync::Arc;

    use relix_core::types::{ErrorEnvelope, error_kinds};
    use serde::Deserialize;

    use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

    use super::JudgeRecorder;

    /// Wire `judge.recent_verdicts` + `judge.stats`.
    pub fn register(bridge: &mut DispatchBridge, recorder: JudgeRecorder) {
        {
            let r = recorder.clone();
            bridge.register(
                "judge.recent_verdicts",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let r = r.clone();
                    async move { handle_recent(&r, &ctx) }
                })),
            );
        }
        {
            bridge.register(
                "judge.stats",
                Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                    let r = recorder.clone();
                    async move { handle_stats(&r) }
                })),
            );
        }
    }

    #[derive(Debug, Deserialize, Default)]
    struct RecentArgs {
        #[serde(default = "default_limit")]
        limit: usize,
    }

    fn default_limit() -> usize {
        20
    }

    fn handle_recent(rec: &JudgeRecorder, ctx: &InvocationCtx) -> HandlerOutcome {
        let args: RecentArgs = if ctx.args.is_empty() {
            RecentArgs::default()
        } else {
            match serde_json::from_slice(&ctx.args) {
                Ok(a) => a,
                Err(e) => {
                    return HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::INVALID_ARGS,
                        cause: format!("judge.recent_verdicts: decode args: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    });
                }
            }
        };
        let body = serde_json::json!({ "verdicts": rec.recent(args.limit) });
        ok_json(&body)
    }

    fn handle_stats(rec: &JudgeRecorder) -> HandlerOutcome {
        let body = rec.stats();
        ok_json(&body)
    }

    fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
        match serde_json::to_vec(value) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("judge: encode response: {e}"),
                retry_hint: 0,
                retry_after: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool) -> JudgeConfig {
        JudgeConfig {
            enabled,
            judge_threshold: 0.6,
            max_judge_latency_ms: 6000,
            recent_buffer_size: 8,
            ..Default::default()
        }
    }

    #[test]
    fn default_config_is_disabled() {
        let c = JudgeConfig::default();
        assert!(!c.enabled);
        assert!((c.judge_threshold - 0.6).abs() < f32::EPSILON);
        assert_eq!(c.max_judge_latency_ms, 6000);
    }

    #[test]
    fn should_invoke_requires_every_condition() {
        let c = cfg(true);
        // disabled
        assert!(!should_invoke(&cfg(false), 0.4, "[tool]", 5));
        // confidence too high
        assert!(!should_invoke(&c, 0.9, "[tool]", 5));
        // not enough turns
        assert!(!should_invoke(&c, 0.4, "[tool]", 1));
        // plain text response
        assert!(!should_invoke(&c, 0.4, "just a paragraph.", 5));
        // all met
        assert!(should_invoke(&c, 0.4, "[tool] do thing", 2));
    }

    #[test]
    fn has_tool_call_or_structured_marker_detects_common_patterns() {
        assert!(has_tool_call_or_structured_marker("tool_call: search"));
        assert!(has_tool_call_or_structured_marker(
            "Here's the answer: ```json\n{}\n```"
        ));
        assert!(has_tool_call_or_structured_marker("{\"name\":\"x\"}"));
        assert!(has_tool_call_or_structured_marker("[1, 2, 3]"));
        assert!(!has_tool_call_or_structured_marker(
            "no markers in this paragraph"
        ));
    }

    #[test]
    fn build_judge_prompt_includes_both_user_and_assistant() {
        let p = build_judge_prompt("ping?", "pong");
        assert!(p.contains("ping?"));
        assert!(p.contains("pong"));
        assert!(p.contains("answers_question"));
        assert!(p.contains("verdict"));
    }

    #[test]
    fn parse_judge_response_handles_valid_proceed_verdict() {
        let raw = r#"{
            "answers_question": "yes",
            "action_is_safe": "yes",
            "factual_errors": [],
            "overconfident": false,
            "verdict": "proceed"
        }"#;
        let v = parse_judge_response(raw).unwrap();
        assert_eq!(v.verdict, JudgeAction::Proceed);
        assert!(!v.overconfident);
        assert_eq!(v.answers_question, JudgeStance::Yes);
    }

    #[test]
    fn parse_judge_response_handles_block_with_factual_errors() {
        let raw = r#"```json
        {
            "answers_question": "partial",
            "action_is_safe": "no",
            "factual_errors": ["wrong year"],
            "overconfident": true,
            "verdict": "block"
        }
        ```"#;
        let v = parse_judge_response(raw).unwrap();
        assert_eq!(v.verdict, JudgeAction::Block);
        assert_eq!(v.factual_errors, vec!["wrong year"]);
        assert!(v.overconfident);
    }

    #[test]
    fn parse_judge_response_rejects_garbage() {
        assert!(parse_judge_response("not json").is_err());
    }

    #[test]
    fn recorder_truncates_to_capacity_and_counts_buckets() {
        let r = JudgeRecorder::new(2);
        for i in 0..3 {
            let v = if i == 0 {
                JudgeAction::Block
            } else if i == 1 {
                JudgeAction::Modify
            } else {
                JudgeAction::Proceed
            };
            r.record(VerdictRecord {
                agent: "alice".into(),
                session_id: format!("s{i}"),
                timestamp_ms: i as i64,
                final_confidence: 0.4,
                timed_out: false,
                verdict: JudgeVerdict {
                    answers_question: JudgeStance::Yes,
                    action_is_safe: JudgeStance::Yes,
                    factual_errors: vec![],
                    overconfident: false,
                    verdict: v,
                },
            });
        }
        let stats = r.stats();
        assert_eq!(stats.block_count, 1);
        assert_eq!(stats.modify_count, 1);
        assert_eq!(stats.proceed_count, 1);
        assert_eq!(stats.recent_buffered, 2);
        let recent = r.recent(10);
        assert_eq!(recent.len(), 2);
        // Most recent first.
        assert_eq!(recent[0].verdict.verdict, JudgeAction::Proceed);
    }

    #[test]
    fn session_turn_counter_returns_prior_count_and_increments() {
        let c = SessionTurnCounter::new();
        assert_eq!(c.bump("a"), 0);
        assert_eq!(c.bump("a"), 1);
        assert_eq!(c.bump("a"), 2);
        assert_eq!(c.bump("b"), 0);
        assert!(c.reset("a"));
        assert!(!c.reset("a"));
        assert_eq!(c.bump("a"), 0);
    }

    #[test]
    fn proceed_default_returns_safe_synthetic_verdict() {
        let v = JudgeVerdict::proceed_default("timeout");
        assert_eq!(v.verdict, JudgeAction::Proceed);
        assert!(v.factual_errors.is_empty());
    }
}
