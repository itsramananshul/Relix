//! Prime Deliberation v1 — constrained model deliberation over the EXISTING
//! governed action menu (company-model §12.5/§12.5A — the Prime planner /
//! model-assisted seam, here applied to the autonomous loop's choice within a
//! tick).
//!
//! **THE MODEL IS NOT THE PERMISSION SYSTEM.** This module lets an opt-in model
//! choose ONLY between the single legal next governed action the deterministic
//! classifier already computed for a candidate and `none` (hold this tick). It
//! cannot invent an action, cannot pick an action outside the candidate's
//! allowed set, and cannot bypass any gate: every action it confirms still flows
//! through the existing governed handlers + standing authority + budget gates +
//! claims + adapter probes + tenant isolation in `prime_driver`. The model's
//! only powers are (a) decline the deterministic action this tick (`none`) and
//! (b) attach a short reason. Any malformed / disallowed / unavailable output
//! degrades to the deterministic behaviour with an honest mode.
//!
//! This module is PURE and dependency-light (snapshot → prompt → parse), so the
//! schema/prompt/validator are fully unit-tested without a mesh or a provider.
//! The live mesh `ai.chat` wiring + the [`PrimeAiDecider`] abstraction the loop
//! consumes live in `prime_driver`.

use serde_json::Value;

/// The sentinel "take no governed action this tick" choice. Always legal.
pub const ACTION_NONE: &str = "none";

/// Every action the deliberation layer may ever encounter. The model can only
/// pick one of these AND only when it is in the candidate's computed allowed set
/// (which is always `[<computed action>, "none"]`). A value outside this list is
/// rejected as an unknown/invented action.
pub const KNOWN_ACTIONS: &[&str] = &[
    ACTION_NONE,
    "approve",
    "propose_strategy",
    "approve_strategy",
    "create_team_plan",
    "orchestrate_assign_ready",
    "start",
    "start_mandate",
    "hire_approve",
    "clearance_approve",
    "review_accept",
    "apply_run",
];

/// Hard cap on a model reason we will accept (chars). An overlong reason is
/// rejected (→ deterministic fallback), never silently truncated into the
/// record.
pub const MAX_REASON_CHARS: usize = 240;
/// Hard cap on the prompt we hand the model — bounds cost and keeps the request
/// tight (snapshot only, never a repo / secret dump).
pub const MAX_PROMPT_CHARS: usize = 2000;
/// Hard cap on the raw model output we will even attempt to parse. A larger blob
/// is rejected outright (→ fallback) rather than parsed.
pub const MAX_MODEL_OUTPUT_CHARS: usize = 4000;

/// How a single tick's action choice was actually made — surfaced on the tick
/// record so the operator sees the provenance instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimeDeliberationMode {
    /// LLM deliberation is off (or there was no governed action to choose): the
    /// deterministic classifier alone decided.
    DeterministicOnly,
    /// The model returned a valid, allowed choice that was honoured (either the
    /// computed action or `none`).
    LlmUsed,
    /// The model answered but its output was malformed / disallowed / unsafe, so
    /// the deterministic behaviour was used instead.
    Fallback,
    /// The model could not be reached (no decider / mesh / AI peer, or the call
    /// failed), so the deterministic behaviour was used.
    Unavailable,
}

impl PrimeDeliberationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PrimeDeliberationMode::DeterministicOnly => "deterministic_only",
            PrimeDeliberationMode::LlmUsed => "llm_used",
            PrimeDeliberationMode::Fallback => "fallback",
            PrimeDeliberationMode::Unavailable => "unavailable",
        }
    }
}

/// The bounded, secret-free snapshot the model deliberates over. Built from the
/// already-computed `NextStep` plus a little safe candidate metadata — never any
/// secret, credential, token, or large free-text dump.
#[derive(Debug, Clone)]
pub struct PrimeDeliberationInput {
    pub tenant: String,
    /// `proposal` or `mandate`.
    pub target_kind: String,
    pub target_id: String,
    pub mandate_id: Option<String>,
    /// The classified next-step phase (`needs_team_plan` / `ready_to_start` / …).
    pub phase: String,
    /// The ONE governed action the deterministic classifier would take — the
    /// only positive choice offered to the model.
    pub computed_action: String,
    /// The deterministic step's short, secret-free reason text.
    pub reason: String,
    pub strategy_status: Option<String>,
    pub total_briefs: i64,
    pub ready: i64,
    pub unassigned: i64,
    pub running: i64,
    pub needs_review: i64,
    pub blocked: i64,
    pub missing_roles: usize,
    pub pending_hires: usize,
    pub pending_clearances: usize,
}

impl PrimeDeliberationInput {
    /// The legal action set the model must choose from: the computed action and
    /// `none`. The validator rejects anything else.
    pub fn allowed_actions(&self) -> Vec<String> {
        let mut v = vec![self.computed_action.clone()];
        if self.computed_action != ACTION_NONE {
            v.push(ACTION_NONE.to_string());
        }
        v
    }
}

/// The validated decision: a legal action (one of the allowed set) + a short,
/// sanitized reason.
#[derive(Debug, Clone)]
pub struct PrimeDeliberationDecision {
    pub action: String,
    pub reason: String,
}

/// The optional AI decision provider the autonomous loop consults. Synchronous
/// (the loop runs inside `spawn_blocking`); the live impl in `prime_driver`
/// bridges to the async mesh `ai.chat` call. Returns the model's raw reply text,
/// or `Err(reason)` when the model is unavailable. Implementors MUST NOT take any
/// governed action — they only return text for [`parse_prime_decision`] to vet.
pub trait PrimeAiDecider: Send + Sync {
    fn deliberate(&self, prompt: &str) -> Result<String, String>;
}

/// Replace pipe + control chars (keep ordinary whitespace) so a snippet is safe
/// in any wire/log form.
fn sanitize_inline(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '|' {
                '/'
            } else if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Build the bounded, sanitized deliberation prompt. PURE + unit-tested. The
/// model is instructed to return STRICT JSON `{"action":"…","reason":"…"}`,
/// choosing EXACTLY ONE action from the allowed list (which always includes
/// `none`), to explain briefly, and never to invent a tool. Because the
/// coordinator re-validates and re-gates everything, the prompt only needs to
/// steer — it is never trusted.
pub fn build_prime_deliberation_prompt(input: &PrimeDeliberationInput) -> String {
    let allowed = input.allowed_actions().join(", ");
    let strategy = input.strategy_status.as_deref().unwrap_or("none");
    let mandate = input.mandate_id.as_deref().unwrap_or("(none)");
    let raw = format!(
        "You are Prime, a company planning lead, deciding the next governed step for ONE work \
item. You may ONLY choose one action from this exact allowed list, or \"none\" to hold this \
tick. Do NOT invent actions or tools. The system has already computed the single legal next \
action; your job is to confirm it or hold.\n\
Allowed actions: [{allowed}]\n\
Respond with ONLY a single JSON object, no prose, no code fence:\n\
{{\"action\":\"<one of the allowed actions>\",\"reason\":\"<one short sentence>\"}}\n\n\
Work item:\n\
- kind: {kind}\n\
- id: {id}\n\
- mandate: {mandate}\n\
- phase: {phase}\n\
- computed next action: {action}\n\
- strategy status: {strategy}\n\
- briefs total/ready/unassigned/running/needs_review/blocked: {total}/{ready}/{unassigned}/{running}/{needs_review}/{blocked}\n\
- missing roles / pending hires / pending clearances: {missing}/{hires}/{clearances}\n\
- system reason: {reason}\n",
        allowed = allowed,
        kind = input.target_kind,
        id = input.target_id,
        mandate = mandate,
        phase = input.phase,
        action = input.computed_action,
        strategy = strategy,
        total = input.total_briefs,
        ready = input.ready,
        unassigned = input.unassigned,
        running = input.running,
        needs_review = input.needs_review,
        blocked = input.blocked,
        missing = input.missing_roles,
        hires = input.pending_hires,
        clearances = input.pending_clearances,
        reason = input.reason,
    );
    let cleaned = sanitize_inline(&raw);
    cleaned.chars().take(MAX_PROMPT_CHARS).collect()
}

/// Strip a single leading/trailing markdown code fence (```json … ``` or ``` …
/// ```) if present, returning the inner body. Leaves un-fenced input untouched.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    // Drop the optional language tag on the opening fence line.
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    rest.trim()
        .strip_suffix("```")
        .map_or(rest.trim(), str::trim)
}

/// Validate + parse a raw model reply into a [`PrimeDeliberationDecision`]
/// constrained to `allowed`. STRICT: rejects empty / overlong output, non-object
/// JSON (arrays, scalars), malformed JSON, a missing/non-string `action`, an
/// unknown action, an action not in the allowed set, a non-string / overlong /
/// control-char reason. On any rejection the caller falls back to the
/// deterministic action. PURE + unit-tested.
pub fn parse_prime_decision(
    raw: &str,
    allowed: &[String],
) -> Result<PrimeDeliberationDecision, String> {
    if raw.chars().count() > MAX_MODEL_OUTPUT_CHARS {
        return Err("model output too long".to_string());
    }
    let body = strip_code_fence(raw);
    if body.is_empty() {
        return Err("empty model output".to_string());
    }
    let value: Value = serde_json::from_str(body).map_err(|e| format!("malformed JSON: {e}"))?;
    let obj = match &value {
        Value::Object(map) => map,
        Value::Array(_) => return Err("model output is an array, not an object".to_string()),
        _ => return Err("model output is not a JSON object".to_string()),
    };
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing or non-string `action`".to_string())?
        .trim()
        .to_string();
    if !KNOWN_ACTIONS.contains(&action.as_str()) {
        return Err(format!("unknown action `{action}`"));
    }
    if !allowed.iter().any(|a| a == &action) {
        return Err(format!("action `{action}` not in the allowed set"));
    }
    // Reason is optional; when present it must be a clean, bounded string.
    let reason = match obj.get("reason") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => {
            if s.chars().count() > MAX_REASON_CHARS {
                return Err("reason too long".to_string());
            }
            if s.chars().any(|c| c.is_control() && c != ' ') {
                return Err("reason contains control characters".to_string());
            }
            sanitize_inline(s.trim())
        }
        Some(_) => return Err("`reason` must be a string".to_string()),
    };
    Ok(PrimeDeliberationDecision { action, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(computed: &str) -> PrimeDeliberationInput {
        PrimeDeliberationInput {
            tenant: "default".into(),
            target_kind: "mandate".into(),
            target_id: "m1".into(),
            mandate_id: Some("m1".into()),
            phase: "needs_team_plan".into(),
            computed_action: computed.into(),
            reason: "no team plan yet".into(),
            strategy_status: Some("approved".into()),
            total_briefs: 0,
            ready: 0,
            unassigned: 0,
            running: 0,
            needs_review: 0,
            blocked: 0,
            missing_roles: 0,
            pending_hires: 0,
            pending_clearances: 0,
        }
    }

    fn allowed(computed: &str) -> Vec<String> {
        input(computed).allowed_actions()
    }

    #[test]
    fn accepts_a_valid_allowed_action() {
        let d = parse_prime_decision(
            r#"{"action":"create_team_plan","reason":"crew is ready to staff"}"#,
            &allowed("create_team_plan"),
        )
        .expect("valid allowed action accepted");
        assert_eq!(d.action, "create_team_plan");
        assert_eq!(d.reason, "crew is ready to staff");
    }

    #[test]
    fn accepts_none_to_hold() {
        let d = parse_prime_decision(
            r#"{"action":"none","reason":"hold for review"}"#,
            &allowed("create_team_plan"),
        )
        .expect("none is always allowed");
        assert_eq!(d.action, "none");
    }

    #[test]
    fn accepts_missing_reason_as_empty() {
        let d = parse_prime_decision(r#"{"action":"none"}"#, &allowed("orchestrate_assign_ready"))
            .unwrap();
        assert_eq!(d.action, "none");
        assert!(d.reason.is_empty());
    }

    #[test]
    fn rejects_unknown_action() {
        let e = parse_prime_decision(
            r#"{"action":"delete_everything","reason":"x"}"#,
            &allowed("create_team_plan"),
        )
        .unwrap_err();
        assert!(e.contains("unknown action"), "got: {e}");
    }

    #[test]
    fn rejects_known_but_disallowed_action() {
        // `start` is a known action but not in the allowed set for a
        // create_team_plan candidate.
        let e = parse_prime_decision(
            r#"{"action":"start","reason":"go"}"#,
            &allowed("create_team_plan"),
        )
        .unwrap_err();
        assert!(e.contains("not in the allowed set"), "got: {e}");
    }

    #[test]
    fn rejects_malformed_json() {
        let e = parse_prime_decision("not json at all", &allowed("create_team_plan")).unwrap_err();
        assert!(e.contains("malformed JSON"), "got: {e}");
    }

    #[test]
    fn rejects_array() {
        let e = parse_prime_decision(r#"["create_team_plan"]"#, &allowed("create_team_plan"))
            .unwrap_err();
        assert!(e.contains("array"), "got: {e}");
    }

    #[test]
    fn rejects_scalar() {
        let e = parse_prime_decision(r#""create_team_plan""#, &allowed("create_team_plan"))
            .unwrap_err();
        assert!(e.contains("not a JSON object"), "got: {e}");
    }

    #[test]
    fn strips_code_fence_and_parses() {
        let raw = "```json\n{\"action\":\"create_team_plan\",\"reason\":\"ok\"}\n```";
        let d = parse_prime_decision(raw, &allowed("create_team_plan"))
            .expect("fenced JSON is stripped + parsed");
        assert_eq!(d.action, "create_team_plan");
    }

    #[test]
    fn rejects_overlong_reason() {
        let long = "x".repeat(MAX_REASON_CHARS + 1);
        let raw = format!(r#"{{"action":"none","reason":"{long}"}}"#);
        let e = parse_prime_decision(&raw, &allowed("create_team_plan")).unwrap_err();
        assert!(e.contains("reason too long"), "got: {e}");
    }

    #[test]
    fn rejects_control_chars_in_reason() {
        // A raw newline inside the JSON string is a control char.
        let raw = "{\"action\":\"none\",\"reason\":\"line1\nline2\"}";
        let e = parse_prime_decision(raw, &allowed("create_team_plan")).unwrap_err();
        // Either serde rejects the literal control char, or our control-char
        // guard does — both are valid rejections (→ fallback). What must NOT
        // happen is acceptance.
        assert!(!e.is_empty());
        assert!(parse_prime_decision(raw, &allowed("create_team_plan")).is_err());
    }

    #[test]
    fn rejects_overlong_output() {
        let raw = "x".repeat(MAX_MODEL_OUTPUT_CHARS + 1);
        let e = parse_prime_decision(&raw, &allowed("create_team_plan")).unwrap_err();
        assert!(e.contains("too long"), "got: {e}");
    }

    #[test]
    fn rejects_non_string_action() {
        let e = parse_prime_decision(r#"{"action":3}"#, &allowed("create_team_plan")).unwrap_err();
        assert!(e.contains("`action`"), "got: {e}");
    }

    #[test]
    fn prompt_is_bounded_pipe_free_and_mentions_the_choices() {
        let p = build_prime_deliberation_prompt(&input("create_team_plan"));
        assert!(p.chars().count() <= MAX_PROMPT_CHARS);
        assert!(!p.contains('|'), "prompt must be pipe-free");
        assert!(p.contains("create_team_plan"));
        assert!(p.contains("none"));
        assert!(p.contains("JSON"));
    }

    #[test]
    fn prompt_is_clamped_for_a_huge_reason() {
        let mut i = input("create_team_plan");
        i.reason = "y".repeat(10_000);
        let p = build_prime_deliberation_prompt(&i);
        assert!(p.chars().count() <= MAX_PROMPT_CHARS);
    }

    #[test]
    fn mode_strings_are_stable() {
        assert_eq!(
            PrimeDeliberationMode::DeterministicOnly.as_str(),
            "deterministic_only"
        );
        assert_eq!(PrimeDeliberationMode::LlmUsed.as_str(), "llm_used");
        assert_eq!(PrimeDeliberationMode::Fallback.as_str(), "fallback");
        assert_eq!(PrimeDeliberationMode::Unavailable.as_str(), "unavailable");
    }
}
