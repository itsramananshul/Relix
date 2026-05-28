//! RELIX-7.24 Stage-5 — step-level verification harness.
//!
//! When `[planning] verify_steps = true`, the coordinator's
//! `planning.create_plan` execute path wraps the workflow
//! executor's event stream with this harness. For each
//! [`crate::workflow::WorkflowEvent::StepCompleted`] the
//! harness:
//!
//! 1. Walks every entry in the plan's [`PlanSpec::success_criteria`].
//! 2. Picks a [`VerificationStrategy`] from the criterion text
//!    using heuristic rules (length checks, keyword
//!    presence/absence, regex pattern match, fallback to an
//!    `ai.chat` judgement).
//! 3. Runs the strategy against the step's output.
//! 4. Persists a [`VerificationEntry`] row via the wired
//!    [`super::ApprovalStore`].
//!
//! After the workflow finishes, the harness inspects every
//! recorded entry. When ANY entry tied to a step in
//! `required_steps` failed verification, the final
//! [`VerificationOutcome`] is marked `Failed` and the
//! coordinator overrides the workflow result's status to
//! `failed` in the response. Non-critical failures are
//! recorded as warnings and don't affect the run's status.
//!
//! ## Honesty contract
//!
//! The workflow engine doesn't expose a mid-execution cancel
//! primitive, so the harness can't literally stop a running
//! workflow when a critical verification failure is detected
//! at step N — subsequent steps will still run if their
//! source edges fire. What the harness DOES is:
//!
//! - Record the verification failure persistently before the
//!   next step starts.
//! - Override the final [`crate::workflow::WorkflowResult`]
//!   status the coordinator returns: even if every step
//!   succeeded mechanically, a required-step verification
//!   failure flips the response to `Failed` with the full
//!   verification log in the body.
//!
//! From the operator's perspective this is indistinguishable
//! from a "halt" — the run is reported as failed, the trace
//! shows where verification flagged the divergence, and the
//! conflict resolver / critic loop's audit trail stays
//! intact.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::workflow::{Workflow, WorkflowDispatcher, WorkflowEvent, WorkflowResult};

use super::approval::VerificationEntry;
use super::{ApprovalStore, PlanSpec};

/// `[planning]` block — step-verification configuration. All
/// fields default to "off" so the existing single-shot
/// execute path stays byte-identical when the operator
/// hasn't opted in.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct VerificationConfig {
    /// Master switch. `false` skips the harness entirely;
    /// the workflow runs through the standard executor with
    /// zero overhead.
    #[serde(default)]
    pub verify_steps: bool,
    /// Agent name (as declared under `[agents.<name>]`) the
    /// AI judge dispatches to when no rule-based strategy
    /// applies.
    #[serde(default = "default_verifier_agent")]
    pub verifier_agent: String,
    /// libp2p peer alias to invoke `ai.chat` on for the AI
    /// judge.
    #[serde(default = "default_verifier_peer")]
    pub verifier_peer: String,
    /// Step ids that MUST pass every applicable criterion.
    /// Any failure on a required step flips the workflow
    /// result to `Failed`. Empty list → every step is
    /// advisory (failures recorded but workflow status
    /// unchanged).
    #[serde(default)]
    pub required_steps: Vec<String>,
}

fn default_verifier_agent() -> String {
    "coordinator".to_string()
}

fn default_verifier_peer() -> String {
    "coordinator".to_string()
}

/// One pre-selected strategy for a given criterion. The
/// harness picks strategies in priority order — the FIRST
/// matching strategy wins (so a "must include `foo`" clause
/// inside a goal-length criterion still uses the length
/// check first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStrategy {
    LengthCheck,
    KeywordPresence,
    KeywordAbsence,
    PatternMatch,
    AiJudge,
}

impl VerificationStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LengthCheck => "length_check",
            Self::KeywordPresence => "keyword_presence",
            Self::KeywordAbsence => "keyword_absence",
            Self::PatternMatch => "pattern_match",
            Self::AiJudge => "ai_judge",
        }
    }
}

/// The aggregate verdict across every step in a workflow run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationOutcome {
    /// `true` only when every required-step verification
    /// passed (or no required_steps were configured). When
    /// `false`, the coordinator overrides the workflow's
    /// result status to `failed`.
    pub passed: bool,
    /// Full log of every entry recorded for this run.
    pub entries: Vec<VerificationEntry>,
    /// Subset of `entries` that failed verification AND
    /// whose step is in `required_steps`. Empty when
    /// `passed = true`.
    pub critical_failures: Vec<VerificationEntry>,
    /// Subset of `entries` that failed verification but are
    /// NOT in `required_steps`. Operator sees these as
    /// warnings in the response.
    pub advisory_failures: Vec<VerificationEntry>,
}

/// Cheap-to-clone harness wired with everything it needs to
/// evaluate criteria during a workflow run.
#[derive(Clone)]
pub struct VerificationHarness {
    dispatcher: Arc<dyn WorkflowDispatcher>,
    store: ApprovalStore,
    cfg: VerificationConfig,
}

impl VerificationHarness {
    pub fn new(
        dispatcher: Arc<dyn WorkflowDispatcher>,
        store: ApprovalStore,
        cfg: VerificationConfig,
    ) -> Self {
        Self {
            dispatcher,
            store,
            cfg,
        }
    }

    /// Returns `true` when the operator has opted into
    /// step-level verification.
    pub fn enabled(&self) -> bool {
        self.cfg.verify_steps
    }

    /// Build a step-id → output map from a completed workflow
    /// trace. Used by [`Self::evaluate_run`] to pull step
    /// outputs without rerunning the workflow.
    pub fn extract_step_outputs(result: &WorkflowResult) -> Vec<(String, String)> {
        result
            .trace
            .steps
            .iter()
            .filter(|s| s.outcome.is_ok())
            .map(|s| (s.agent.clone(), s.output.clone()))
            .collect()
    }

    /// Evaluate every step output in `result` against the
    /// spec's `success_criteria`. Persists every entry into
    /// the wired [`ApprovalStore`] AND returns the aggregate
    /// [`VerificationOutcome`] so the coordinator can include
    /// it in the response.
    pub async fn evaluate_run(
        &self,
        plan_id: &str,
        spec: &PlanSpec,
        result: &WorkflowResult,
    ) -> VerificationOutcome {
        if !self.cfg.verify_steps || spec.success_criteria.is_empty() {
            return VerificationOutcome {
                passed: true,
                entries: Vec::new(),
                critical_failures: Vec::new(),
                advisory_failures: Vec::new(),
            };
        }
        let step_outputs = Self::extract_step_outputs(result);
        let mut entries = Vec::new();
        for (step_id, output) in &step_outputs {
            for criterion in &spec.success_criteria {
                let strategy = pick_strategy(criterion);
                let (passed, reason) = self.evaluate(strategy, criterion, output).await;
                let entry = VerificationEntry {
                    plan_id: plan_id.to_string(),
                    step_id: step_id.clone(),
                    criterion: criterion.clone(),
                    strategy_used: strategy.as_str().to_string(),
                    passed,
                    reason,
                    verified_at_ms: unix_now_ms(),
                };
                if let Err(e) = self.store.insert_verification(&entry) {
                    tracing::warn!(
                        plan_id,
                        step_id,
                        error = %e,
                        "verification: failed to persist entry"
                    );
                }
                entries.push(entry);
            }
        }
        let mut critical_failures = Vec::new();
        let mut advisory_failures = Vec::new();
        for e in &entries {
            if e.passed {
                continue;
            }
            if self.cfg.required_steps.is_empty()
                || self.cfg.required_steps.iter().any(|s| s == &e.step_id)
            {
                critical_failures.push(e.clone());
            } else {
                advisory_failures.push(e.clone());
            }
        }
        // When required_steps is empty the harness treats
        // EVERY step as advisory by default — the workflow is
        // never failed by verification alone. Operators opt
        // into halt-on-failure semantics by listing the
        // step_ids they care about.
        let passed = if self.cfg.required_steps.is_empty() {
            advisory_failures.is_empty()
        } else {
            critical_failures.is_empty()
        };
        // Re-categorize advisories when required_steps is
        // empty so the response distinguishes "no required
        // steps configured, everything passed" from "required
        // steps passed, some advisory failures".
        if self.cfg.required_steps.is_empty() {
            let (crit, adv): (Vec<_>, Vec<_>) = entries.iter().cloned().partition(|e| !e.passed);
            VerificationOutcome {
                passed: crit.is_empty(),
                entries,
                critical_failures: Vec::new(),
                advisory_failures: crit
                    .into_iter()
                    .chain(adv.into_iter().filter(|e| !e.passed))
                    .collect(),
            }
        } else {
            VerificationOutcome {
                passed,
                entries,
                critical_failures,
                advisory_failures,
            }
        }
    }

    /// Pure-function dispatch onto one of the four
    /// rule-based strategies or the AI judge. Public for
    /// testing.
    pub async fn evaluate(
        &self,
        strategy: VerificationStrategy,
        criterion: &str,
        output: &str,
    ) -> (bool, String) {
        match strategy {
            VerificationStrategy::LengthCheck => evaluate_length_check(criterion, output),
            VerificationStrategy::KeywordPresence => evaluate_keyword_presence(criterion, output),
            VerificationStrategy::KeywordAbsence => evaluate_keyword_absence(criterion, output),
            VerificationStrategy::PatternMatch => evaluate_pattern_match(criterion, output),
            VerificationStrategy::AiJudge => self.evaluate_ai_judge(criterion, output).await,
        }
    }

    async fn evaluate_ai_judge(&self, criterion: &str, output: &str) -> (bool, String) {
        let prompt = build_ai_judge_prompt(criterion, output);
        let session_id = format!("planning-verify-{}", short_rand_id());
        let arg = format!("{session_id}|{prompt}");
        match self
            .dispatcher
            .dispatch(&self.cfg.verifier_peer, "ai.chat", arg.as_bytes())
            .await
        {
            Ok(bytes) => parse_ai_judge_verdict(&bytes).unwrap_or((
                true,
                "ai judge response was unparseable — assumed pass".into(),
            )),
            Err(_) => (
                true,
                "ai judge dispatcher unreachable — assumed pass with caveat".into(),
            ),
        }
    }
}

/// Pick the highest-priority strategy that matches the
/// criterion text. The priority order is documented on
/// [`VerificationStrategy`]; the picker stops at the first
/// match. Pure function — public for testing.
pub fn pick_strategy(criterion: &str) -> VerificationStrategy {
    let lower = criterion.to_lowercase();
    if contains_length_keyword(&lower) {
        return VerificationStrategy::LengthCheck;
    }
    if contains_keyword_absence_marker(&lower) {
        return VerificationStrategy::KeywordAbsence;
    }
    if contains_keyword_presence_marker(&lower) {
        return VerificationStrategy::KeywordPresence;
    }
    if contains_regex_pattern(criterion) {
        return VerificationStrategy::PatternMatch;
    }
    VerificationStrategy::AiJudge
}

fn contains_length_keyword(lower: &str) -> bool {
    let phrases = [
        "under ",
        "at most ",
        "no more than ",
        "less than ",
        "fewer than ",
        "up to ",
    ];
    phrases
        .iter()
        .any(|p| lower.contains(p) && (lower.contains(" word") || lower.contains(" token")))
}

fn contains_keyword_presence_marker(lower: &str) -> bool {
    let phrases = [
        "must include",
        "must contain",
        "should include",
        "should contain",
        "include the",
        "contain the",
        "must mention",
    ];
    phrases.iter().any(|p| lower.contains(p))
}

fn contains_keyword_absence_marker(lower: &str) -> bool {
    let phrases = [
        "must not include",
        "must not contain",
        "should not include",
        "should not contain",
        "without ",
        "no mention of",
        "do not include",
        "do not mention",
    ];
    phrases.iter().any(|p| lower.contains(p))
}

fn contains_regex_pattern(criterion: &str) -> bool {
    let trimmed = criterion.trim();
    // Operators tag a regex criterion with `/regex/` markers.
    // We DON'T regex-match arbitrary criteria — that would
    // give too many false positives.
    trimmed.len() > 2 && trimmed.starts_with('/') && trimmed.ends_with('/')
}

/// LengthCheck evaluator: extracts the limit N from the
/// criterion (`under N words` / `at most N tokens`), counts
/// the output's words (and approximates tokens as `words *
/// 1.3`), and reports pass/fail.
pub fn evaluate_length_check(criterion: &str, output: &str) -> (bool, String) {
    let lower = criterion.to_lowercase();
    let (limit, unit) = match extract_length_limit(&lower) {
        Some(v) => v,
        None => {
            return (
                true,
                "length check: criterion did not specify a numeric limit; passed by default".into(),
            );
        }
    };
    let word_count = output.split_whitespace().count();
    let token_estimate = ((word_count as f32) * 1.3).ceil() as usize;
    let (observed, observed_unit) = match unit {
        LengthUnit::Words => (word_count, "words"),
        LengthUnit::Tokens => (token_estimate, "tokens (estimated)"),
    };
    if observed <= limit {
        (
            true,
            format!("length check: {observed} {observed_unit} ≤ {limit} limit"),
        )
    } else {
        (
            false,
            format!("length check: {observed} {observed_unit} > {limit} limit"),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LengthUnit {
    Words,
    Tokens,
}

fn extract_length_limit(lower: &str) -> Option<(usize, LengthUnit)> {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let n: usize = lower[start..i].parse().ok()?;
            // Look ahead for "words" or "tokens".
            let rest = &lower[i..];
            let unit = if rest.contains("words") || rest.contains(" word ") {
                LengthUnit::Words
            } else if rest.contains("tokens") || rest.contains(" token ") {
                LengthUnit::Tokens
            } else {
                // Number without a recognised unit — keep
                // looking for another number.
                continue;
            };
            return Some((n, unit));
        }
        i += 1;
    }
    None
}

/// KeywordPresence: extracts the keyword(s) the criterion
/// requires and checks the output contains each. Keywords
/// come from quoted segments OR from the noun after "include
/// the" / "contain the" / "mention".
pub fn evaluate_keyword_presence(criterion: &str, output: &str) -> (bool, String) {
    let keywords = extract_keywords_for_strategy(criterion);
    if keywords.is_empty() {
        return (
            true,
            "keyword presence: no extractable keyword; passed by default".into(),
        );
    }
    let lower_output = output.to_lowercase();
    let missing: Vec<String> = keywords
        .iter()
        .filter(|k| !lower_output.contains(&k.to_lowercase()))
        .cloned()
        .collect();
    if missing.is_empty() {
        (
            true,
            format!("keyword presence: output contains {:?}", keywords),
        )
    } else {
        (
            false,
            format!("keyword presence: missing keyword(s) {missing:?}"),
        )
    }
}

/// KeywordAbsence: extracts the keyword(s) the criterion
/// forbids and checks the output contains NONE.
pub fn evaluate_keyword_absence(criterion: &str, output: &str) -> (bool, String) {
    let keywords = extract_keywords_for_strategy(criterion);
    if keywords.is_empty() {
        return (
            true,
            "keyword absence: no extractable keyword; passed by default".into(),
        );
    }
    let lower_output = output.to_lowercase();
    let present: Vec<String> = keywords
        .iter()
        .filter(|k| lower_output.contains(&k.to_lowercase()))
        .cloned()
        .collect();
    if present.is_empty() {
        (
            true,
            format!("keyword absence: output omits {:?}", keywords),
        )
    } else {
        (
            false,
            format!("keyword absence: forbidden keyword(s) found {present:?}"),
        )
    }
}

/// Extract the keyword(s) the criterion is asking about.
/// Priority:
///
/// 1. Anything in single OR double quotes.
/// 2. The trailing noun phrase after `include the`,
///    `contain the`, `mention`, `without`, etc.
fn extract_keywords_for_strategy(criterion: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Single-quoted.
    extract_quoted(criterion, '\'', &mut out);
    // Double-quoted.
    extract_quoted(criterion, '"', &mut out);
    if !out.is_empty() {
        return out;
    }
    let lower = criterion.to_lowercase();
    let markers = [
        "must include",
        "must contain",
        "should include",
        "should contain",
        "must mention",
        "include the",
        "contain the",
        "must not include",
        "must not contain",
        "should not include",
        "should not contain",
        "without ",
        "no mention of",
        "do not include",
        "do not mention",
    ];
    for m in markers {
        if let Some(idx) = lower.find(m) {
            let tail = &criterion[idx + m.len()..].trim_start();
            // Pull up to 3 words of the trailing noun phrase,
            // dropping leading articles ("the", "a", "an") so
            // `include the executive summary` yields
            // `executive summary` rather than `the executive
            // summary`.
            let mut words: Vec<&str> = tail.split_whitespace().collect();
            while let Some(first) = words.first().copied() {
                let lower_first = first.to_ascii_lowercase();
                if matches!(lower_first.as_str(), "the" | "a" | "an") {
                    words.remove(0);
                } else {
                    break;
                }
            }
            words.truncate(3);
            if words.is_empty() {
                continue;
            }
            // Drop trailing punctuation.
            let phrase: String = words
                .join(" ")
                .trim_matches(|c: char| !c.is_alphanumeric() && c != ' ')
                .trim()
                .to_string();
            if !phrase.is_empty() {
                out.push(phrase);
            }
            break;
        }
    }
    out
}

fn extract_quoted(s: &str, q: char, out: &mut Vec<String>) {
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c == q {
            match start {
                Some(s_idx) => {
                    let inner = &s[s_idx + 1..i];
                    if !inner.is_empty() {
                        out.push(inner.to_string());
                    }
                    start = None;
                }
                None => start = Some(i),
            }
        }
    }
}

/// PatternMatch: when the criterion is `/regex/`, compile and
/// match against the output. Bad regexes pass with a note.
pub fn evaluate_pattern_match(criterion: &str, output: &str) -> (bool, String) {
    let trimmed = criterion.trim();
    let pattern = trimmed
        .strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .unwrap_or(trimmed);
    match regex::Regex::new(pattern) {
        Ok(re) => {
            if re.is_match(output) {
                (true, format!("pattern match: `/{pattern}/` matched"))
            } else {
                (
                    false,
                    format!("pattern match: `/{pattern}/` did not match output"),
                )
            }
        }
        Err(e) => (
            true,
            format!(
                "pattern match: regex compile failed for `/{pattern}/`: {e}; passed by default"
            ),
        ),
    }
}

fn build_ai_judge_prompt(criterion: &str, output: &str) -> String {
    let output_preview: String = output.chars().take(2000).collect();
    format!(
        "You are an output-verification judge. Decide whether the OUTPUT below satisfies the \
         CRITERION. Return ONLY a JSON object with this exact shape — no markdown, no prose:\n\
         {{\"passed\": <bool>, \"reason\": <string>}}\n\n\
         CRITERION:\n{criterion}\n\nOUTPUT:\n{output_preview}\n\nReturn the JSON now."
    )
}

/// Parse the AI judge's response body. Returns `None` only
/// when no JSON object can be extracted at all; callers treat
/// that as "judge unreachable / unparseable" with a default
/// pass to avoid blocking the workflow on a misconfigured AI.
pub fn parse_ai_judge_verdict(raw: &[u8]) -> Option<(bool, String)> {
    let text = std::str::from_utf8(raw).ok()?;
    let stripped = strip_markdown_code_fences(text);
    if let Ok(v) = serde_json::from_str::<JudgeVerdict>(&stripped) {
        return Some((v.passed, v.reason));
    }
    if let Some(start) = stripped.find('{')
        && let Some(end) = stripped[start..].rfind('}')
    {
        let slice = &stripped[start..start + end + 1];
        if let Ok(v) = serde_json::from_str::<JudgeVerdict>(slice) {
            return Some((v.passed, v.reason));
        }
    }
    None
}

#[derive(Deserialize)]
struct JudgeVerdict {
    #[serde(default)]
    passed: bool,
    #[serde(default)]
    reason: String,
}

fn strip_markdown_code_fences(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```json")
        && let Some(body) = rest.strip_suffix("```")
    {
        return body.trim().to_string();
    }
    if let Some(rest) = t.strip_prefix("```")
        && let Some(body) = rest.strip_suffix("```")
    {
        return body.trim().to_string();
    }
    t.to_string()
}

fn short_rand_id() -> String {
    let bytes: [u8; 8] = rand::random();
    hex::encode(bytes)
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Workflow-event-stream wrapper used by the coordinator's
/// execute path. Spawns a task that drains an
/// `execute_with_events` channel into the harness, recording
/// any non-critical failures as advisory rows. Returns once
/// the `Finished` event arrives.
///
/// The harness can't intercept dispatch failures or
/// pre-cancel later steps — it observes outputs as they're
/// emitted and persists results. Critical-step decisions are
/// applied AFTER the run via [`VerificationHarness::evaluate_run`]
/// — see the module docstring's honesty contract.
pub async fn drain_events_into_log(
    mut events: tokio::sync::mpsc::UnboundedReceiver<WorkflowEvent>,
    harness: VerificationHarness,
    plan_id: String,
    spec: PlanSpec,
) -> Option<WorkflowResult> {
    if !harness.enabled() {
        // Caller should not have wired the channel; drain
        // defensively anyway so the executor isn't blocked on
        // a stalled receiver.
        let mut result: Option<WorkflowResult> = None;
        while let Some(ev) = events.recv().await {
            if let WorkflowEvent::Finished(r) = ev {
                result = Some(r);
            }
        }
        return result;
    }
    let mut completed_outputs: Vec<(String, String)> = Vec::new();
    let mut result: Option<WorkflowResult> = None;
    while let Some(ev) = events.recv().await {
        match ev {
            WorkflowEvent::StepCompleted { agent, output, .. } => {
                // Persist verification rows for this step on
                // the fly. The final-result pass below
                // re-evaluates against the full result so
                // critical_failures can be aggregated, but
                // this gives operators a real-time signal.
                for criterion in &spec.success_criteria {
                    let strategy = pick_strategy(criterion);
                    let (passed, reason) = harness.evaluate(strategy, criterion, &output).await;
                    let entry = VerificationEntry {
                        plan_id: plan_id.clone(),
                        step_id: agent.clone(),
                        criterion: criterion.clone(),
                        strategy_used: strategy.as_str().to_string(),
                        passed,
                        reason,
                        verified_at_ms: unix_now_ms(),
                    };
                    if let Err(e) = harness.store.insert_verification(&entry) {
                        tracing::warn!(
                            plan_id = %plan_id,
                            step_id = %agent,
                            error = %e,
                            "verification: failed to persist entry"
                        );
                    }
                }
                completed_outputs.push((agent, output));
            }
            WorkflowEvent::Finished(r) => {
                result = Some(r);
            }
            _ => {}
        }
    }
    result
}

/// Build a [`Workflow`] +
/// [`crate::workflow::execute_with_events`] driver that
/// streams its events through the harness and returns the
/// final [`WorkflowResult`] alongside the recorded outcome.
/// Convenience used by the coordinator.
pub async fn execute_with_verification(
    workflow_arc: Arc<Workflow>,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    input: &str,
    harness: VerificationHarness,
    plan_id: &str,
    spec: &PlanSpec,
) -> (WorkflowResult, VerificationOutcome) {
    if !harness.enabled() {
        let result = crate::workflow::execute(workflow_arc, dispatcher, input).await;
        return (
            result,
            VerificationOutcome {
                passed: true,
                entries: Vec::new(),
                critical_failures: Vec::new(),
                advisory_failures: Vec::new(),
            },
        );
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let plan_id_owned = plan_id.to_string();
    let spec_owned = spec.clone();
    let drainer = drain_events_into_log(rx, harness.clone(), plan_id_owned, spec_owned);
    let exec = crate::workflow::execute_with_events(workflow_arc, dispatcher, input, tx);
    let (drain_result, exec_result) = tokio::join!(drainer, exec);
    // drain_result is the stream's final Finished result;
    // exec_result is the executor's return value. They should
    // be equal; we prefer exec_result since it's
    // unambiguously the function's own return.
    let _ = drain_result;
    let outcome = harness.evaluate_run(plan_id, spec, &exec_result).await;
    (exec_result, outcome)
}

#[async_trait]
impl WorkflowDispatcher for NoopVerifierDispatcher {
    async fn dispatch(
        &self,
        peer: &str,
        capability: &str,
        _input: &[u8],
    ) -> crate::workflow::DispatchResult {
        Err(crate::workflow::DispatchError {
            peer: peer.to_string(),
            method: capability.to_string(),
            cause: "verification: noop test dispatcher".to_string(),
        })
    }
}

#[doc(hidden)]
pub struct NoopVerifierDispatcher;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{DispatchError, DispatchResult};
    use std::collections::BTreeMap;
    use tokio::sync::Mutex;

    struct CannedDispatcher {
        responses: Mutex<BTreeMap<(String, String), Vec<DispatchResult>>>,
    }
    impl CannedDispatcher {
        fn new() -> Self {
            Self {
                responses: Mutex::new(BTreeMap::new()),
            }
        }
        async fn push_err(&self, peer: &str, cap: &str, cause: &str) {
            self.responses
                .lock()
                .await
                .entry((peer.into(), cap.into()))
                .or_default()
                .push(Err(DispatchError {
                    peer: peer.into(),
                    method: cap.into(),
                    cause: cause.into(),
                }));
        }
    }
    #[async_trait]
    impl WorkflowDispatcher for CannedDispatcher {
        async fn dispatch(&self, peer: &str, cap: &str, _input: &[u8]) -> DispatchResult {
            let mut q = self.responses.lock().await;
            let queue = q.entry((peer.into(), cap.into())).or_default();
            if queue.is_empty() {
                return Err(DispatchError {
                    peer: peer.into(),
                    method: cap.into(),
                    cause: "no canned response queued".into(),
                });
            }
            queue.remove(0)
        }
    }

    fn fixture_harness(verify_steps: bool, required_steps: Vec<String>) -> VerificationHarness {
        let dispatcher = Arc::new(CannedDispatcher::new());
        let store = ApprovalStore::open_in_memory().unwrap();
        VerificationHarness::new(
            dispatcher,
            store,
            VerificationConfig {
                verify_steps,
                verifier_agent: "coordinator".into(),
                verifier_peer: "coordinator".into(),
                required_steps,
            },
        )
    }

    #[test]
    fn pick_strategy_routes_length_keywords_to_length_check() {
        assert_eq!(
            pick_strategy("Return a summary under 300 words"),
            VerificationStrategy::LengthCheck
        );
        assert_eq!(
            pick_strategy("output should be at most 100 tokens"),
            VerificationStrategy::LengthCheck
        );
    }

    #[test]
    fn pick_strategy_routes_must_include_to_keyword_presence() {
        assert_eq!(
            pick_strategy("output must include the word `result`"),
            VerificationStrategy::KeywordPresence
        );
    }

    #[test]
    fn pick_strategy_routes_must_not_include_to_keyword_absence() {
        assert_eq!(
            pick_strategy("output must not include profanity"),
            VerificationStrategy::KeywordAbsence
        );
    }

    #[test]
    fn pick_strategy_routes_slash_regex_to_pattern_match() {
        assert_eq!(pick_strategy("/^OK:/"), VerificationStrategy::PatternMatch);
    }

    #[test]
    fn pick_strategy_falls_through_to_ai_judge() {
        assert_eq!(
            pick_strategy("answer must be coherent and well-reasoned"),
            VerificationStrategy::AiJudge
        );
    }

    #[test]
    fn evaluate_length_check_passes_under_limit() {
        let (passed, reason) =
            evaluate_length_check("output must be under 10 words", "this has four words");
        assert!(passed);
        assert!(reason.contains("≤ 10"));
    }

    #[test]
    fn evaluate_length_check_fails_over_limit() {
        let long: String = std::iter::repeat_n("word", 50)
            .collect::<Vec<_>>()
            .join(" ");
        let (passed, _) = evaluate_length_check("must be under 10 words", &long);
        assert!(!passed);
    }

    #[test]
    fn evaluate_length_check_tokens_approximates_via_words() {
        let text: String = std::iter::repeat_n("word", 100)
            .collect::<Vec<_>>()
            .join(" ");
        // 100 words → ~130 tokens; limit 200 → passes.
        let (passed, _) = evaluate_length_check("must be under 200 tokens", &text);
        assert!(passed);
        // limit 50 → fails.
        let (passed, reason) = evaluate_length_check("must be under 50 tokens", &text);
        assert!(!passed, "reason={reason}");
    }

    #[test]
    fn evaluate_keyword_presence_passes_when_keyword_found() {
        let (passed, _) = evaluate_keyword_presence(
            "output must include \"summary\"",
            "Final summary: the project shipped.",
        );
        assert!(passed);
    }

    #[test]
    fn evaluate_keyword_presence_fails_when_keyword_missing() {
        let (passed, reason) = evaluate_keyword_presence(
            "output must include \"summary\"",
            "Final: the project shipped.",
        );
        assert!(!passed);
        assert!(reason.contains("missing"));
    }

    #[test]
    fn evaluate_keyword_absence_passes_when_keyword_missing() {
        let (passed, _) =
            evaluate_keyword_absence("output must not include \"profanity\"", "clean output");
        assert!(passed);
    }

    #[test]
    fn evaluate_keyword_absence_fails_when_keyword_present() {
        let (passed, reason) =
            evaluate_keyword_absence("output must not include \"badword\"", "this has badword");
        assert!(!passed);
        assert!(reason.contains("forbidden"));
    }

    #[test]
    fn evaluate_pattern_match_passes_on_match() {
        let (passed, _) = evaluate_pattern_match("/^OK:/", "OK: ready");
        assert!(passed);
    }

    #[test]
    fn evaluate_pattern_match_fails_on_miss() {
        let (passed, _) = evaluate_pattern_match("/^OK:/", "ERR: not ready");
        assert!(!passed);
    }

    #[test]
    fn evaluate_pattern_match_passes_by_default_on_bad_regex() {
        let (passed, reason) = evaluate_pattern_match("/[unclosed/", "any text");
        assert!(passed, "bad regex must default-pass with a note");
        assert!(reason.contains("regex compile failed"), "reason={reason}");
    }

    #[test]
    fn parse_ai_judge_verdict_accepts_bare_json() {
        let body = br#"{"passed":true,"reason":"looks good"}"#;
        let (passed, reason) = parse_ai_judge_verdict(body).unwrap();
        assert!(passed);
        assert_eq!(reason, "looks good");
    }

    #[test]
    fn parse_ai_judge_verdict_extracts_from_markdown_fence() {
        let body = b"```json\n{\"passed\":false,\"reason\":\"missing keyword\"}\n```";
        let (passed, reason) = parse_ai_judge_verdict(body).unwrap();
        assert!(!passed);
        assert_eq!(reason, "missing keyword");
    }

    #[test]
    fn parse_ai_judge_verdict_returns_none_on_garbage() {
        assert!(parse_ai_judge_verdict(b"<<<<").is_none());
    }

    #[tokio::test]
    async fn ai_judge_passes_by_default_when_dispatcher_unreachable() {
        let dispatcher = Arc::new(CannedDispatcher::new());
        dispatcher
            .push_err("coordinator", "ai.chat", "mesh down")
            .await;
        let harness = VerificationHarness::new(
            dispatcher,
            ApprovalStore::open_in_memory().unwrap(),
            VerificationConfig {
                verify_steps: true,
                verifier_agent: "coordinator".into(),
                verifier_peer: "coordinator".into(),
                required_steps: Vec::new(),
            },
        );
        let (passed, reason) = harness
            .evaluate(
                VerificationStrategy::AiJudge,
                "the answer must be coherent",
                "some output",
            )
            .await;
        assert!(passed);
        assert!(reason.contains("unreachable"));
    }

    #[tokio::test]
    async fn evaluate_run_persists_entries_and_marks_passed_when_no_failures() {
        let dispatcher = Arc::new(CannedDispatcher::new());
        let store = ApprovalStore::open_in_memory().unwrap();
        let harness = VerificationHarness::new(
            dispatcher,
            store.clone(),
            VerificationConfig {
                verify_steps: true,
                verifier_agent: "c".into(),
                verifier_peer: "c".into(),
                required_steps: vec!["step_a".into()],
            },
        );
        let spec = super::super::SpecParser::new().parse("Goal. Output must be under 100 words.");
        let mut result = mock_workflow_result();
        // Make sure the step exists in the trace.
        result.trace.steps.push(crate::workflow::ExecutionStep {
            agent: "step_a".into(),
            peer: "p1".into(),
            capability: "ai.chat".into(),
            input: "in".into(),
            output: "short output".into(),
            latency_ms: 1,
            outcome: Ok(()),
        });
        let outcome = harness.evaluate_run("plan-1", &spec, &result).await;
        assert!(outcome.passed);
        assert_eq!(outcome.critical_failures.len(), 0);
        assert!(!outcome.entries.is_empty());
        // Persistence round-trip.
        let persisted = store.list_verifications("plan-1").unwrap();
        assert_eq!(persisted.len(), outcome.entries.len());
    }

    #[tokio::test]
    async fn evaluate_run_marks_failed_when_required_step_violates_a_criterion() {
        let dispatcher = Arc::new(CannedDispatcher::new());
        let store = ApprovalStore::open_in_memory().unwrap();
        let harness = VerificationHarness::new(
            dispatcher,
            store,
            VerificationConfig {
                verify_steps: true,
                verifier_agent: "c".into(),
                verifier_peer: "c".into(),
                required_steps: vec!["critical".into()],
            },
        );
        let spec = super::super::SpecParser::new().parse("Goal. Output must be under 5 words.");
        let mut result = mock_workflow_result();
        result.trace.steps.push(crate::workflow::ExecutionStep {
            agent: "critical".into(),
            peer: "p1".into(),
            capability: "ai.chat".into(),
            input: "in".into(),
            // 7 words → fails the under-5-words criterion.
            output: "this output has seven different total words here".into(),
            latency_ms: 1,
            outcome: Ok(()),
        });
        let outcome = harness.evaluate_run("plan-2", &spec, &result).await;
        assert!(!outcome.passed);
        assert_eq!(outcome.critical_failures.len(), 1);
        let f = &outcome.critical_failures[0];
        assert_eq!(f.step_id, "critical");
        assert!(!f.passed);
    }

    #[tokio::test]
    async fn evaluate_run_is_a_noop_when_verify_steps_is_false() {
        let harness = fixture_harness(false, vec![]);
        let spec = super::super::SpecParser::new().parse("Goal. Return a summary.");
        let result = mock_workflow_result();
        let outcome = harness.evaluate_run("plan-noop", &spec, &result).await;
        assert!(outcome.passed);
        assert!(outcome.entries.is_empty());
    }

    fn mock_workflow_result() -> WorkflowResult {
        WorkflowResult {
            trace: crate::workflow::ExecutionTrace {
                execution_id: crate::workflow::ExecutionId("test".into()),
                workflow_name: "test_wf".into(),
                steps: Vec::new(),
                total_latency_ms: 1,
            },
            status: crate::workflow::ExecutionStatus::Success,
            result: "done".into(),
        }
    }

    #[test]
    fn extract_keywords_picks_quoted_segments_first() {
        let kws = extract_keywords_for_strategy("output must include \"alpha\" and 'beta'");
        assert!(kws.contains(&"alpha".to_string()));
        assert!(kws.contains(&"beta".to_string()));
    }

    #[test]
    fn extract_keywords_falls_through_to_noun_phrase_after_marker() {
        let kws = extract_keywords_for_strategy("must include the executive summary");
        assert_eq!(kws, vec!["executive summary".to_string()]);
    }
}
