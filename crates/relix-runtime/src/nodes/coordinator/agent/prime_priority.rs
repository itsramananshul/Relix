//! Prime Executive Prioritization v1 — constrained model ORDERING over the
//! EXISTING governed candidate queue (company-model §5.4/§8.2 — the Action Center
//! "next governed step"; §12.5/§12.5A — the Prime planner / model-assisted seam,
//! here applied to *which* already-legal candidate a bounded tick spends its
//! action budget on first).
//!
//! **THE MODEL IS NOT THE PERMISSION SYSTEM.** This module lets an opt-in model
//! choose only the ORDER in which the autonomous loop attempts a set of
//! candidate actions the deterministic classifier has ALREADY computed as legal /
//! attemptable under the existing gates. It cannot invent a candidate, cannot add
//! an action to the menu, cannot widen the allowed action of any candidate, and
//! cannot bypass any gate: every action it reorders still flows through the same
//! governed handlers, standing authority, budget gates, claims, adapter probes,
//! and tenant isolation in `prime_driver`, and only the already-attemptable
//! candidates are ever offered. The model's only powers are to reorder the offered
//! candidate keys, return an EMPTY order to hold the whole queue this tick, and
//! attach a short reason. Any malformed / out-of-set / duplicate / overlong /
//! unavailable output degrades to the deterministic discovery order with an honest
//! mode.
//!
//! This module is PURE and dependency-light (summaries → prompt → parse), so the
//! prompt builder and the validator are fully unit-tested without a mesh or a
//! provider. The live mesh `ai.chat` wiring (the shared [`super::prime_deliberation::PrimeAiDecider`])
//! and the deterministic fallback that bounds it live in `prime_driver`.

use serde_json::Value;

/// Hard cap on the number of candidates offered to the model in one tick. The
/// discovery queue is already bounded by `discover_cap`; this caps the menu so a
/// large Guild never produces an unbounded prompt. Candidates beyond the cap are
/// still driven deterministically — they are simply not reordered.
pub const MAX_PRIORITY_CANDIDATES: usize = 24;
/// Hard cap on the prompt we hand the model — bounds cost and keeps the request
/// tight (bounded candidate summaries only, never a repo / file / secret dump).
pub const MAX_PRIORITY_PROMPT_CHARS: usize = 4000;
/// Hard cap on the raw model output we will even attempt to parse. A larger blob
/// is rejected outright (→ deterministic fallback) rather than parsed.
pub const MAX_PRIORITY_OUTPUT_CHARS: usize = 4000;
/// Hard cap on the model's free-text `reason` (chars). An overlong reason is
/// rejected (→ fallback), never silently truncated into the record.
pub const MAX_PRIORITY_REASON_CHARS: usize = 240;

/// How a single tick's candidate ORDER was actually chosen — surfaced on every
/// tick record so the operator sees whether the queue order was model-picked or
/// deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimePriorityMode {
    /// LLM prioritization is off, or there were fewer than two attemptable
    /// candidates to order: the deterministic discovery order alone decided.
    DeterministicOnly,
    /// The model returned a valid, in-set, duplicate-free order (possibly empty =
    /// hold) that was honoured.
    LlmUsed,
    /// The model answered but its output was malformed / out-of-set / duplicate /
    /// overlong, so the deterministic discovery order was used instead.
    Fallback,
    /// The model could not be reached (no decider / mesh / AI peer, or the call
    /// failed), so the deterministic discovery order was used.
    Unavailable,
}

impl PrimePriorityMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PrimePriorityMode::DeterministicOnly => "deterministic_only",
            PrimePriorityMode::LlmUsed => "llm_used",
            PrimePriorityMode::Fallback => "fallback",
            PrimePriorityMode::Unavailable => "unavailable",
        }
    }
}

/// One bounded, secret-free candidate summary offered to the prioritization
/// model. Built from a candidate's already-computed `NextStep` plus a little safe
/// metadata — never any secret, credential, token, or large free-text dump. The
/// `key` is a stable, opaque per-tick handle (`cand-1`, `cand-2`, …); the model
/// may only reorder these keys, and the parser rejects any key it was not
/// offered.
#[derive(Debug, Clone)]
pub struct PrimePriorityCandidate {
    pub key: String,
    pub tenant: String,
    /// `proposal` or `mandate`.
    pub target_kind: String,
    pub target_id: String,
    pub mandate_id: Option<String>,
    /// The classified next-step phase (`needs_team_plan` / `ready_to_start` / …).
    pub phase: String,
    /// The ONE governed action the deterministic classifier would attempt for
    /// this candidate — fixed; the model may only reorder, never change it.
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

/// The validated prioritization decision: an ordered list of offered candidate
/// keys (empty = hold the whole queue this tick) + a short, sanitized reason.
#[derive(Debug, Clone)]
pub struct PrimePriorityOrder {
    /// The offered keys in the model's chosen priority order. A subset is allowed
    /// (the caller appends any un-listed offered candidates in deterministic order
    /// AFTER these). EMPTY means HOLD — take no action this tick.
    pub order: Vec<String>,
    pub reason: String,
}

/// Replace pipe + non-whitespace control chars (keep `\n`/`\t`) so a snippet is
/// safe inside any wire/log form.
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

/// Strip a single leading/trailing markdown code fence (```json … ``` or ``` …
/// ```) if present, returning the inner body. Leaves un-fenced input untouched.
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    rest.trim()
        .strip_suffix("```")
        .map_or(rest.trim(), str::trim)
}

/// Build the bounded, sanitized prioritization prompt. PURE + unit-tested. The
/// model is instructed to rank the offered candidate KEYS most-important-first as
/// strict JSON `{"order":["…"],"reason":"…"}`, or to return `{"order":[],…}` to
/// hold the whole queue this tick, and never to invent a key or an action. Because
/// the coordinator re-validates + re-gates everything (the model only reorders
/// already-legal candidates), the prompt only needs to steer — it is never
/// trusted.
pub fn build_priority_prompt(candidates: &[PrimePriorityCandidate]) -> String {
    let mut lines = String::new();
    for c in candidates {
        let mandate = c.mandate_id.as_deref().unwrap_or("(none)");
        let strategy = c.strategy_status.as_deref().unwrap_or("none");
        lines.push_str(&format!(
            "- key: {key} | kind: {kind} | phase: {phase} | action: {action} | mandate: {mandate} | \
strategy: {strategy} | briefs total/ready/unassigned/running/needs_review/blocked: \
{total}/{ready}/{unassigned}/{running}/{needs_review}/{blocked} | missing_roles/pending_hires/pending_clearances: \
{missing}/{hires}/{clearances} | why: {reason}\n",
            key = c.key,
            kind = c.target_kind,
            phase = c.phase,
            action = c.computed_action,
            mandate = mandate,
            strategy = strategy,
            total = c.total_briefs,
            ready = c.ready,
            unassigned = c.unassigned,
            running = c.running,
            needs_review = c.needs_review,
            blocked = c.blocked,
            missing = c.missing_roles,
            hires = c.pending_hires,
            clearances = c.pending_clearances,
            reason = c.reason,
        ));
    }
    let raw = format!(
        "You are Prime, a company planning lead. The system has already computed the single legal \
next governed action for each work item below; every action listed is permitted. Your ONLY job is \
to choose the ORDER in which the autonomous loop should spend its limited action budget this tick — \
most important first. You may NOT invent items, keys, or actions, and you may NOT change any item's \
action.\n\
Rules:\n\
- Respond with ONLY a single JSON object, no prose, no code fence.\n\
- Shape: {{\"order\":[\"<key>\",\"<key>\"...],\"reason\":\"<one short sentence>\"}}.\n\
- Use ONLY the keys listed below; do not repeat a key; you may list a subset.\n\
- To take NO action this tick (hold the whole queue), return {{\"order\":[],\"reason\":\"<why hold>\"}}.\n\n\
Candidates (all already legal):\n\
{lines}",
        lines = lines,
    );
    let cleaned = sanitize_inline(&raw);
    cleaned.chars().take(MAX_PRIORITY_PROMPT_CHARS).collect()
}

/// Validate + parse a raw model reply into a [`PrimePriorityOrder`] constrained to
/// `offered_keys`. STRICT: rejects empty / overlong output, non-object JSON, a
/// missing / non-array `order`, a non-string / unknown (not offered) / duplicate
/// key, and a non-string / overlong / control-char reason. An EMPTY `order` array
/// is ACCEPTED and means HOLD (take no action this tick). On any rejection the
/// caller falls back to the deterministic discovery order. PURE + unit-tested.
pub fn parse_priority_order(
    raw: &str,
    offered_keys: &[String],
) -> Result<PrimePriorityOrder, String> {
    if raw.chars().count() > MAX_PRIORITY_OUTPUT_CHARS {
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
    let order_val = obj
        .get("order")
        .ok_or_else(|| "missing `order`".to_string())?;
    let arr = match order_val {
        Value::Array(a) => a,
        _ => return Err("`order` must be an array".to_string()),
    };
    if arr.len() > offered_keys.len() {
        return Err("`order` lists more keys than were offered".to_string());
    }
    let mut order: Vec<String> = Vec::with_capacity(arr.len());
    for elem in arr {
        let key = match elem {
            Value::String(s) => s.trim().to_string(),
            _ => return Err("`order` contains a non-string key".to_string()),
        };
        if !offered_keys.iter().any(|k| k == &key) {
            return Err(format!("unknown key `{key}` not in the offered set"));
        }
        if order.iter().any(|k| k == &key) {
            return Err(format!("duplicate key `{key}`"));
        }
        order.push(key);
    }
    // Reason is optional; when present it must be a clean, bounded string.
    let reason = match obj.get("reason") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => {
            if s.chars().count() > MAX_PRIORITY_REASON_CHARS {
                return Err("reason too long".to_string());
            }
            if s.chars().any(|c| c.is_control() && c != ' ') {
                return Err("reason contains control characters".to_string());
            }
            sanitize_inline(s.trim())
        }
        Some(_) => return Err("`reason` must be a string".to_string()),
    };
    Ok(PrimePriorityOrder { order, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(key: &str, action: &str) -> PrimePriorityCandidate {
        PrimePriorityCandidate {
            key: key.into(),
            tenant: "default".into(),
            target_kind: "mandate".into(),
            target_id: format!("m-{key}"),
            mandate_id: Some(format!("m-{key}")),
            phase: "needs_team_plan".into(),
            computed_action: action.into(),
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

    fn keys(ks: &[&str]) -> Vec<String> {
        ks.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn accepts_a_valid_reordered_subset() {
        let d = parse_priority_order(
            r#"{"order":["cand-2","cand-1"],"reason":"ship the proposal first"}"#,
            &keys(&["cand-1", "cand-2"]),
        )
        .expect("valid reorder accepted");
        assert_eq!(d.order, vec!["cand-2".to_string(), "cand-1".to_string()]);
        assert_eq!(d.reason, "ship the proposal first");
    }

    #[test]
    fn accepts_a_strict_subset() {
        let d = parse_priority_order(
            r#"{"order":["cand-2"]}"#,
            &keys(&["cand-1", "cand-2", "cand-3"]),
        )
        .expect("a subset is allowed");
        assert_eq!(d.order, vec!["cand-2".to_string()]);
        assert!(d.reason.is_empty());
    }

    #[test]
    fn accepts_empty_order_as_hold() {
        let d = parse_priority_order(
            r#"{"order":[],"reason":"hold for human review"}"#,
            &keys(&["cand-1", "cand-2"]),
        )
        .expect("empty order is a valid hold");
        assert!(d.order.is_empty());
        assert_eq!(d.reason, "hold for human review");
    }

    #[test]
    fn rejects_unknown_key() {
        let e = parse_priority_order(r#"{"order":["cand-9"]}"#, &keys(&["cand-1", "cand-2"]))
            .unwrap_err();
        assert!(e.contains("unknown key"), "got: {e}");
    }

    #[test]
    fn rejects_duplicate_key() {
        let e = parse_priority_order(
            r#"{"order":["cand-1","cand-1"]}"#,
            &keys(&["cand-1", "cand-2"]),
        )
        .unwrap_err();
        assert!(e.contains("duplicate key"), "got: {e}");
    }

    #[test]
    fn rejects_prose() {
        let e = parse_priority_order(
            "Sure! I'd run cand-2 first, then cand-1.",
            &keys(&["cand-1", "cand-2"]),
        )
        .unwrap_err();
        assert!(e.contains("malformed JSON"), "got: {e}");
    }

    #[test]
    fn rejects_array_top_level() {
        let e = parse_priority_order(r#"["cand-1"]"#, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("array"), "got: {e}");
    }

    #[test]
    fn rejects_missing_order() {
        let e = parse_priority_order(r#"{"reason":"x"}"#, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("missing `order`"), "got: {e}");
    }

    #[test]
    fn rejects_non_array_order() {
        let e = parse_priority_order(r#"{"order":"cand-1"}"#, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("must be an array"), "got: {e}");
    }

    #[test]
    fn rejects_non_string_key() {
        let e = parse_priority_order(r#"{"order":[3]}"#, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("non-string key"), "got: {e}");
    }

    #[test]
    fn rejects_too_many_keys() {
        let e = parse_priority_order(
            r#"{"order":["cand-1","cand-2","cand-3"]}"#,
            &keys(&["cand-1", "cand-2"]),
        )
        .unwrap_err();
        assert!(e.contains("more keys than were offered"), "got: {e}");
    }

    #[test]
    fn rejects_overlong_output() {
        let raw = "x".repeat(MAX_PRIORITY_OUTPUT_CHARS + 1);
        let e = parse_priority_order(&raw, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("too long"), "got: {e}");
    }

    #[test]
    fn rejects_overlong_reason() {
        let long = "x".repeat(MAX_PRIORITY_REASON_CHARS + 1);
        let raw = format!(r#"{{"order":["cand-1"],"reason":"{long}"}}"#);
        let e = parse_priority_order(&raw, &keys(&["cand-1"])).unwrap_err();
        assert!(e.contains("reason too long"), "got: {e}");
    }

    #[test]
    fn strips_code_fence_and_parses() {
        let raw = "```json\n{\"order\":[\"cand-1\"],\"reason\":\"ok\"}\n```";
        let d = parse_priority_order(raw, &keys(&["cand-1", "cand-2"]))
            .expect("fenced JSON is stripped + parsed");
        assert_eq!(d.order, vec!["cand-1".to_string()]);
    }

    #[test]
    fn prompt_is_bounded_pipe_free_and_lists_keys() {
        let cands = vec![
            cand("cand-1", "create_team_plan"),
            cand("cand-2", "approve"),
        ];
        let p = build_priority_prompt(&cands);
        assert!(p.chars().count() <= MAX_PRIORITY_PROMPT_CHARS);
        assert!(!p.contains('|'), "prompt must be pipe-free");
        assert!(p.contains("cand-1"));
        assert!(p.contains("cand-2"));
        assert!(p.contains("order"));
        assert!(p.contains("JSON"));
    }

    #[test]
    fn prompt_is_clamped_for_many_candidates() {
        let cands: Vec<_> = (0..200)
            .map(|i| cand(&format!("cand-{i}"), "create_team_plan"))
            .collect();
        let p = build_priority_prompt(&cands);
        assert!(p.chars().count() <= MAX_PRIORITY_PROMPT_CHARS);
    }

    #[test]
    fn mode_strings_are_stable() {
        assert_eq!(
            PrimePriorityMode::DeterministicOnly.as_str(),
            "deterministic_only"
        );
        assert_eq!(PrimePriorityMode::LlmUsed.as_str(), "llm_used");
        assert_eq!(PrimePriorityMode::Fallback.as_str(), "fallback");
        assert_eq!(PrimePriorityMode::Unavailable.as_str(), "unavailable");
    }
}
