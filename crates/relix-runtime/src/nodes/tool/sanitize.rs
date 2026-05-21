//! PHASE H5 — Tool argument sanitization + self-repair.
//!
//! Hermes runs every tool-call argument through a sanitation pass
//! (`_sanitize_tool_call_arguments`) before dispatching, on the
//! premise that "the model often emits 95%-valid JSON and the
//! remaining 5% should be repaired rather than failing the
//! capability call." Relix's tool node has historically returned
//! `INVALID_ARGS` on the slightest malformedness; this module
//! ships the analogous repair layer.
//!
//! ## Design
//!
//! Pure deterministic function:
//!
//! ```ignore
//! let SanitizationResult { value, actions, severity } =
//!     sanitize_json_args(raw_input, &SanitizeConfig::default());
//! ```
//!
//! Input: a `&str` the model emitted as JSON. Output: a fresh
//! `serde_json::Value` (when recovery succeeded) plus a list of
//! every repair / strip / coerce performed. The actions list
//! lets callers emit a `tool.args_sanitized` chronicle event with
//! a faithful record of what changed.
//!
//! ## Repair heuristics
//!
//! Applied in order, each gated on a config knob:
//!
//! 1. **Trim surrounding whitespace + control chars.** Models
//!    sometimes wrap JSON in stray newlines or BOM bytes.
//! 2. **Strip Markdown code-fence wrappers.** Bodies like
//!    ` ```json\n{...}\n``` ` are unwrapped to the inner block.
//! 3. **Coerce trailing commas.** A single trailing comma before
//!    `]` or `}` is stripped; chains of consecutive trailing
//!    commas are NOT repaired (would mask a model bug).
//! 4. **Single → double quotes** for string keys / values, but
//!    only when the result would still parse as valid JSON. The
//!    rule is conservative: if the input parses successfully
//!    after the swap AND the original failed, take the swap; if
//!    the swap fails, abort the heuristic.
//! 5. **Best-effort UTF-8 control-char strip** (everything below
//!    0x20 that's not `\t` `\n` `\r`).
//!
//! After heuristics, the result is parsed as JSON. If parsing
//! still fails the sanitizer returns `SanitizationSeverity::Failed`
//! with the original input echoed so the caller can degrade to
//! the existing INVALID_ARGS path.
//!
//! ## Recursive payload protection
//!
//! Two caps applied to the parsed value:
//!
//! - **Depth cap** (default 32). Any nested object/array beyond
//!   the cap is replaced with a string `"[truncated:depth]"`;
//!   the truncation is recorded as an action.
//! - **Field-count cap** (default 256 keys per object, 1024
//!   elements per array). Excess elements are dropped; the
//!   drop count is recorded.
//!
//! These protect the runtime from a model that emits a 1 MB
//! recursive blob by accident.
//!
//! ## Dangerous-field strip
//!
//! Fields whose names case-insensitively match an opt-in deny
//! list are stripped from objects and recorded. Default deny
//! list: `__proto__`, `constructor`, `prototype` (prototype-
//! pollution avoidance for any downstream JS-shaped consumer).
//! Operators can extend via [`SanitizeConfig::extra_deny_keys`].
//!
//! ## Scalar coercion
//!
//! Two coercions:
//!
//! - `"true" / "false"` → bool when the parent declares an
//!   expected bool. This module doesn't know expected types,
//!   so this coercion is OPT-IN per call.
//! - Numeric strings → numbers under the same opt-in.
//!
//! Both record a coercion action.
//!
//! ## What this does NOT do
//!
//! - **No schema validation.** The sanitizer doesn't know what
//!   shape a given capability expects. Schema enforcement is a
//!   later milestone (PHASE H7 capability metadata).
//! - **No LLM call.** Pure pattern matching + deterministic
//!   transformations. Same posture as the H8 redactor.
//! - **No mutation of the input.** All outputs are fresh
//!   allocations.

use serde_json::Value;

/// Severity of the sanitation outcome. Drives how the caller
/// surfaces the result: dashboard badge, chronicle event level,
/// etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SanitizationSeverity {
    /// Input parsed cleanly with zero repair actions.
    Clean,
    /// One or more cosmetic actions applied (whitespace trim,
    /// code-fence strip). The shape is unchanged.
    Cosmetic,
    /// Structural repair: trailing-comma fix, quote swap, etc.
    /// The shape is unchanged but the input was technically
    /// invalid JSON.
    Structural,
    /// Truncation / strip applied due to a cap or deny rule.
    /// The shape was modified; the caller should know.
    Truncated,
    /// Parsing failed even after every repair. Caller should
    /// fall back to the existing INVALID_ARGS path.
    Failed,
}

impl SanitizationSeverity {
    /// Short stable label for tracing fields + dashboard text.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Cosmetic => "cosmetic",
            Self::Structural => "structural",
            Self::Truncated => "truncated",
            Self::Failed => "failed",
        }
    }
}

/// One repair / strip / coerce action taken by the sanitizer.
/// Returned as an ordered list so callers can record a faithful
/// trace of what changed.
#[derive(Debug, Clone, PartialEq)]
pub enum SanitizationAction {
    /// Stripped leading/trailing whitespace + control chars.
    TrimmedSurroundingWhitespace,
    /// Removed Markdown code-fence wrappers (```json ... ```).
    StrippedCodeFence,
    /// Removed a trailing comma before `]` or `}`.
    StrippedTrailingComma,
    /// Swapped single quotes for double quotes around string
    /// keys / values.
    SwappedQuoteStyle,
    /// Removed in-band C0 control characters (not \t \n \r).
    StrippedControlChars { count: usize },
    /// Pruned an object key that matched the deny list.
    StrippedDeniedKey { key: String, path: String },
    /// Replaced a nested object/array with `[truncated:depth]`.
    TruncatedDepth { path: String, depth_at: usize },
    /// Dropped extra fields beyond the per-object cap.
    DroppedExcessKeys { path: String, dropped: usize },
    /// Dropped extra elements beyond the per-array cap.
    DroppedExcessElements { path: String, dropped: usize },
    /// Coerced a string scalar into a bool.
    CoercedStringToBool { path: String, value: bool },
    /// Coerced a string scalar into a number.
    CoercedStringToNumber { path: String, value: f64 },
    /// Parsing failed; original input echoed verbatim.
    ParseFailed { error: String },
}

impl SanitizationAction {
    /// Short stable label for the action kind (no payload).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TrimmedSurroundingWhitespace => "trimmed-whitespace",
            Self::StrippedCodeFence => "stripped-code-fence",
            Self::StrippedTrailingComma => "stripped-trailing-comma",
            Self::SwappedQuoteStyle => "swapped-quote-style",
            Self::StrippedControlChars { .. } => "stripped-control-chars",
            Self::StrippedDeniedKey { .. } => "stripped-denied-key",
            Self::TruncatedDepth { .. } => "truncated-depth",
            Self::DroppedExcessKeys { .. } => "dropped-excess-keys",
            Self::DroppedExcessElements { .. } => "dropped-excess-elements",
            Self::CoercedStringToBool { .. } => "coerced-string-to-bool",
            Self::CoercedStringToNumber { .. } => "coerced-string-to-number",
            Self::ParseFailed { .. } => "parse-failed",
        }
    }
}

/// The full sanitation outcome.
#[derive(Debug, Clone)]
pub struct SanitizationResult {
    /// Best-effort sanitised JSON value. `None` when severity is
    /// [`SanitizationSeverity::Failed`].
    pub value: Option<Value>,
    /// Every action the sanitizer took, in order.
    pub actions: Vec<SanitizationAction>,
    /// Highest-severity bucket reached.
    pub severity: SanitizationSeverity,
}

/// Knobs the caller can tune. Defaults are conservative —
/// "repair the obvious mistakes, leave everything else alone."
#[derive(Debug, Clone)]
pub struct SanitizeConfig {
    /// Maximum object/array depth before truncation kicks in.
    pub max_depth: usize,
    /// Maximum keys per object before excess keys are dropped.
    pub max_keys_per_object: usize,
    /// Maximum elements per array before excess are dropped.
    pub max_elements_per_array: usize,
    /// Maximum raw input bytes the sanitizer will scan.
    /// Anything bigger is rejected up front (severity=Failed
    /// with a ParseFailed action), avoiding pathological CPU
    /// spend on a runaway model output.
    pub max_input_bytes: usize,
    /// Lowercase key names to strip from any object. Defaults
    /// to the prototype-pollution short list.
    pub deny_keys: Vec<String>,
    /// Extra deny keys appended to the defaults.
    pub extra_deny_keys: Vec<String>,
    /// Enable opt-in string→bool / string→number coercion.
    /// Off by default — callers that know their schema turn it on.
    pub coerce_scalars: bool,
    /// Enable opt-in single-quote → double-quote rewriting.
    /// Off by default because the rewriter is heuristic and
    /// can occasionally produce malformed output that wouldn't
    /// have parsed anyway.
    pub swap_quote_style: bool,
}

impl Default for SanitizeConfig {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_keys_per_object: 256,
            max_elements_per_array: 1024,
            max_input_bytes: 256 * 1024,
            deny_keys: vec!["__proto__".into(), "constructor".into(), "prototype".into()],
            extra_deny_keys: Vec::new(),
            coerce_scalars: false,
            swap_quote_style: false,
        }
    }
}

/// Sanitize a JSON-shaped argument string. See module docs.
pub fn sanitize_json_args(input: &str, cfg: &SanitizeConfig) -> SanitizationResult {
    let mut actions = Vec::new();

    // Up-front size cap. Avoids spending CPU on a runaway
    // payload that we'd reject downstream anyway.
    if input.len() > cfg.max_input_bytes {
        actions.push(SanitizationAction::ParseFailed {
            error: format!(
                "input size {} bytes exceeds max_input_bytes={}",
                input.len(),
                cfg.max_input_bytes
            ),
        });
        return SanitizationResult {
            value: None,
            actions,
            severity: SanitizationSeverity::Failed,
        };
    }

    // 1. Trim surrounding whitespace + control chars.
    let trimmed = input.trim_matches(|c: char| c.is_whitespace() || c.is_control());
    if trimmed.len() != input.len() {
        actions.push(SanitizationAction::TrimmedSurroundingWhitespace);
    }
    let work = trimmed.to_string();

    // 2. Strip Markdown code fence.
    let (work, fenced) = strip_code_fence(&work);
    if fenced {
        actions.push(SanitizationAction::StrippedCodeFence);
    }

    // 3. Optional quote swap.
    let mut try_swap_quotes = cfg.swap_quote_style;

    // 4. Try parse; if it works, we're done (Clean unless we
    //    already applied cosmetic actions).
    let (parsed, parse_actions) = parse_with_repair(&work, &mut try_swap_quotes);
    actions.extend(parse_actions);

    let Some(parsed) = parsed else {
        // Pin a ParseFailed marker so caller has the rust serde error.
        // The list already contains any cosmetic actions taken; the
        // severity is Failed because we couldn't recover.
        return SanitizationResult {
            value: None,
            actions,
            severity: SanitizationSeverity::Failed,
        };
    };

    // 5. Walk the parsed tree applying caps + deny + coercion.
    let mut deny: Vec<String> = cfg
        .deny_keys
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    deny.extend(cfg.extra_deny_keys.iter().map(|s| s.to_ascii_lowercase()));
    let walked = walk(parsed, "$", 0, cfg, &deny, cfg.coerce_scalars, &mut actions);

    let severity = compute_severity(&actions);
    SanitizationResult {
        value: Some(walked),
        actions,
        severity,
    }
}

// ─────────────────────────── Heuristic helpers ───────────────────────────

/// Detect and strip Markdown code-fence wrappers like
/// ` ```json\n{...}\n``` ` or ` ```\n[...]\n``` `. Returns the
/// inner body + a bool indicating whether we stripped.
fn strip_code_fence(input: &str) -> (String, bool) {
    let t = input.trim();
    if !t.starts_with("```") {
        return (input.to_string(), false);
    }
    // Find newline after the opening fence.
    let after_open = match t.find('\n') {
        Some(i) => &t[i + 1..],
        None => return (input.to_string(), false),
    };
    // Find closing fence.
    let close_idx = match after_open.rfind("```") {
        Some(i) => i,
        None => return (input.to_string(), false),
    };
    let inner = &after_open[..close_idx];
    (inner.trim().to_string(), true)
}

/// Try to parse `input` as JSON; on failure apply cheap fix-ups
/// (trailing-comma strip, optional quote swap) and try again.
/// Records each action that actually changed something.
fn parse_with_repair(
    input: &str,
    try_swap_quotes: &mut bool,
) -> (Option<Value>, Vec<SanitizationAction>) {
    let mut actions = Vec::new();
    // Try-1: parse the input as-is.
    if let Ok(v) = serde_json::from_str(input) {
        return (Some(v), actions);
    }

    // Try-2: strip control chars (not \t \n \r) and retry.
    let (cleaned, stripped) = strip_control_chars(input);
    if stripped > 0 {
        actions.push(SanitizationAction::StrippedControlChars { count: stripped });
    }
    if let Ok(v) = serde_json::from_str(&cleaned) {
        return (Some(v), actions);
    }

    // Try-3: strip a single trailing comma before `]` or `}`.
    let (decomma, removed) = strip_trailing_commas(&cleaned);
    if removed > 0 {
        actions.push(SanitizationAction::StrippedTrailingComma);
    }
    if let Ok(v) = serde_json::from_str(&decomma) {
        return (Some(v), actions);
    }

    // Try-4: optional single-quote → double-quote swap.
    if *try_swap_quotes {
        let swapped = swap_single_quotes(&decomma);
        if swapped != decomma
            && let Ok(v) = serde_json::from_str(&swapped)
        {
            actions.push(SanitizationAction::SwappedQuoteStyle);
            return (Some(v), actions);
        }
        // Mark we tried.
        *try_swap_quotes = false;
    }

    // Give up. Record the final parse error against the most
    // aggressively-cleaned text.
    let err = match serde_json::from_str::<Value>(&decomma) {
        Err(e) => e.to_string(),
        Ok(_) => "unknown".to_string(),
    };
    actions.push(SanitizationAction::ParseFailed { error: err });
    (None, actions)
}

fn strip_control_chars(input: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len());
    let mut removed = 0;
    for c in input.chars() {
        if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' {
            removed += 1;
            continue;
        }
        out.push(c);
    }
    (out, removed)
}

/// Remove a single trailing comma immediately before `]` or `}`.
/// Only handles a single isolated case per `]` / `}` so we don't
/// mask wider corruption.
fn strip_trailing_commas(input: &str) -> (String, usize) {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut removed = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            out.push(b);
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_string = true;
            out.push(b);
            i += 1;
            continue;
        }
        if b == b',' {
            // Look ahead, skipping whitespace, for `]` or `}`.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'}') {
                removed += 1;
                // Skip the comma; the trailing whitespace + closer
                // will be written naturally as we resume.
                i += 1;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    (String::from_utf8(out).unwrap_or_default(), removed)
}

/// Conservative single-quote → double-quote swap. We rewrite
/// `'...'` to `"..."` only when the inner content contains no
/// `"`. If any swap target would be ambiguous we leave the input
/// alone. This is the kind of heuristic that fails closed:
/// downstream `serde_json::from_str` is the source of truth.
fn swap_single_quotes(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_dq = false;
    let mut escape = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_dq {
            out.push(b as char);
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_dq = false;
            }
            i += 1;
            continue;
        }
        if b == b'"' {
            in_dq = true;
            out.push('"');
            i += 1;
            continue;
        }
        if b == b'\'' {
            // Find matching close single quote.
            let close = (i + 1..bytes.len()).find(|&k| bytes[k] == b'\'');
            if let Some(c) = close {
                let inner = &bytes[i + 1..c];
                if !inner.contains(&b'"') {
                    out.push('"');
                    out.extend(inner.iter().map(|&b| b as char));
                    out.push('"');
                    i = c + 1;
                    continue;
                }
            }
        }
        out.push(b as char);
        i += 1;
    }
    out
}

// ─────────────────────────── Tree walker ───────────────────────────

fn walk(
    v: Value,
    path: &str,
    depth: usize,
    cfg: &SanitizeConfig,
    deny: &[String],
    coerce: bool,
    actions: &mut Vec<SanitizationAction>,
) -> Value {
    if depth >= cfg.max_depth {
        actions.push(SanitizationAction::TruncatedDepth {
            path: path.to_string(),
            depth_at: depth,
        });
        return Value::String("[truncated:depth]".into());
    }
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut dropped_excess = 0usize;
            let mut keys_seen = 0usize;
            for (k, val) in map.into_iter() {
                if deny.iter().any(|d| d == &k.to_ascii_lowercase()) {
                    actions.push(SanitizationAction::StrippedDeniedKey {
                        key: k,
                        path: path.to_string(),
                    });
                    continue;
                }
                if keys_seen >= cfg.max_keys_per_object {
                    dropped_excess += 1;
                    continue;
                }
                keys_seen += 1;
                let child_path = if path == "$" {
                    format!("${}", json_pointer_segment(&k))
                } else {
                    format!("{path}{}", json_pointer_segment(&k))
                };
                let walked = walk(val, &child_path, depth + 1, cfg, deny, coerce, actions);
                out.insert(k, walked);
            }
            if dropped_excess > 0 {
                actions.push(SanitizationAction::DroppedExcessKeys {
                    path: path.to_string(),
                    dropped: dropped_excess,
                });
            }
            Value::Object(out)
        }
        Value::Array(arr) => {
            let total = arr.len();
            let take = total.min(cfg.max_elements_per_array);
            let mut iter = arr.into_iter();
            let mut out = Vec::with_capacity(take);
            for i in 0..take {
                let val = iter.next().expect("bounded by take");
                let child_path = format!("{path}[{i}]");
                out.push(walk(
                    val,
                    &child_path,
                    depth + 1,
                    cfg,
                    deny,
                    coerce,
                    actions,
                ));
            }
            if total > take {
                actions.push(SanitizationAction::DroppedExcessElements {
                    path: path.to_string(),
                    dropped: total - take,
                });
            }
            Value::Array(out)
        }
        Value::String(s) if coerce => {
            // String→bool / string→number opt-in coercion.
            let lower = s.to_ascii_lowercase();
            if lower == "true" {
                actions.push(SanitizationAction::CoercedStringToBool {
                    path: path.to_string(),
                    value: true,
                });
                return Value::Bool(true);
            }
            if lower == "false" {
                actions.push(SanitizationAction::CoercedStringToBool {
                    path: path.to_string(),
                    value: false,
                });
                return Value::Bool(false);
            }
            if let Ok(n) = s.parse::<f64>()
                && n.is_finite()
                && let Some(num) = serde_json::Number::from_f64(n)
            {
                actions.push(SanitizationAction::CoercedStringToNumber {
                    path: path.to_string(),
                    value: n,
                });
                return Value::Number(num);
            }
            Value::String(s)
        }
        other => other,
    }
}

/// Produce a JSON-pointer-style segment for `key`. Keeps the
/// path readable in actions for the dashboard.
fn json_pointer_segment(key: &str) -> String {
    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        format!(".{key}")
    } else {
        format!("[\"{}\"]", key.replace('"', "\\\""))
    }
}

fn compute_severity(actions: &[SanitizationAction]) -> SanitizationSeverity {
    let mut sev = SanitizationSeverity::Clean;
    for a in actions {
        let bump = match a {
            SanitizationAction::TrimmedSurroundingWhitespace
            | SanitizationAction::StrippedCodeFence => SanitizationSeverity::Cosmetic,
            SanitizationAction::StrippedTrailingComma
            | SanitizationAction::SwappedQuoteStyle
            | SanitizationAction::StrippedControlChars { .. }
            | SanitizationAction::CoercedStringToBool { .. }
            | SanitizationAction::CoercedStringToNumber { .. } => SanitizationSeverity::Structural,
            SanitizationAction::StrippedDeniedKey { .. }
            | SanitizationAction::TruncatedDepth { .. }
            | SanitizationAction::DroppedExcessKeys { .. }
            | SanitizationAction::DroppedExcessElements { .. } => SanitizationSeverity::Truncated,
            SanitizationAction::ParseFailed { .. } => SanitizationSeverity::Failed,
        };
        if severity_rank(bump) > severity_rank(sev) {
            sev = bump;
        }
    }
    sev
}

fn severity_rank(s: SanitizationSeverity) -> u8 {
    match s {
        SanitizationSeverity::Clean => 0,
        SanitizationSeverity::Cosmetic => 1,
        SanitizationSeverity::Structural => 2,
        SanitizationSeverity::Truncated => 3,
        SanitizationSeverity::Failed => 4,
    }
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> SanitizeConfig {
        SanitizeConfig::default()
    }

    #[test]
    fn clean_json_passthrough_no_actions() {
        let r = sanitize_json_args(r#"{"a":1,"b":"c"}"#, &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Clean);
        assert!(r.actions.is_empty());
        assert_eq!(r.value, Some(json!({"a": 1, "b": "c"})));
    }

    #[test]
    fn surrounding_whitespace_trimmed() {
        let r = sanitize_json_args("  \n\n{\"x\":1}\t  ", &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Cosmetic);
        assert!(matches!(
            r.actions[0],
            SanitizationAction::TrimmedSurroundingWhitespace
        ));
        assert_eq!(r.value, Some(json!({"x": 1})));
    }

    #[test]
    fn code_fence_stripped() {
        let r = sanitize_json_args("```json\n{\"x\":1}\n```", &cfg());
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::StrippedCodeFence))
        );
        assert_eq!(r.value, Some(json!({"x": 1})));
    }

    #[test]
    fn trailing_comma_in_object_stripped() {
        let r = sanitize_json_args(r#"{"a":1,"b":2,}"#, &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Structural);
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::StrippedTrailingComma))
        );
        assert_eq!(r.value, Some(json!({"a": 1, "b": 2})));
    }

    #[test]
    fn trailing_comma_in_array_stripped() {
        let r = sanitize_json_args(r#"[1,2,3,]"#, &cfg());
        assert_eq!(r.value, Some(json!([1, 2, 3])));
    }

    #[test]
    fn single_quote_swap_opt_in() {
        let mut c = cfg();
        c.swap_quote_style = true;
        let r = sanitize_json_args(r#"{'a':'b'}"#, &c);
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::SwappedQuoteStyle))
        );
        assert_eq!(r.value, Some(json!({"a": "b"})));
    }

    #[test]
    fn single_quote_left_alone_when_opt_out() {
        let r = sanitize_json_args(r#"{'a':'b'}"#, &cfg());
        // Default config doesn't swap → parse fails → Failed.
        assert_eq!(r.severity, SanitizationSeverity::Failed);
        assert!(r.value.is_none());
    }

    #[test]
    fn control_chars_stripped_then_parsed() {
        // A vertical-tab (0x0B) mid-object would normally make
        // serde_json choke. We strip it and parse.
        let r = sanitize_json_args("{\"a\":\u{000B}1}", &cfg());
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::StrippedControlChars { .. }))
        );
        assert_eq!(r.value, Some(json!({"a": 1})));
    }

    #[test]
    fn deny_keys_stripped() {
        let r = sanitize_json_args(r#"{"safe":1,"__proto__":{"bad":1}}"#, &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Truncated);
        let v = r.value.unwrap();
        assert!(v.get("__proto__").is_none());
        assert_eq!(v.get("safe"), Some(&json!(1)));
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::StrippedDeniedKey { .. }))
        );
    }

    #[test]
    fn deny_keys_case_insensitive() {
        let r = sanitize_json_args(r#"{"__PROTO__":1}"#, &cfg());
        assert!(r.value.unwrap().get("__PROTO__").is_none());
    }

    #[test]
    fn excess_keys_dropped() {
        let mut c = cfg();
        c.max_keys_per_object = 2;
        let r = sanitize_json_args(r#"{"a":1,"b":2,"c":3,"d":4}"#, &c);
        let v = r.value.unwrap();
        assert_eq!(v.as_object().unwrap().len(), 2);
        let dropped = r
            .actions
            .iter()
            .find_map(|a| match a {
                SanitizationAction::DroppedExcessKeys { dropped, .. } => Some(*dropped),
                _ => None,
            })
            .expect("excess-keys action missing");
        assert_eq!(dropped, 2);
    }

    #[test]
    fn excess_elements_dropped() {
        let mut c = cfg();
        c.max_elements_per_array = 3;
        let r = sanitize_json_args(r#"[1,2,3,4,5]"#, &c);
        assert_eq!(r.value.unwrap().as_array().unwrap().len(), 3);
    }

    #[test]
    fn depth_cap_truncates_deep_nesting() {
        let mut c = cfg();
        c.max_depth = 3;
        // {"a":{"b":{"c":{"d":1}}}} — depth 4 should be truncated.
        let r = sanitize_json_args(r#"{"a":{"b":{"c":{"d":1}}}}"#, &c);
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::TruncatedDepth { .. }))
        );
    }

    #[test]
    fn coerce_string_to_bool_opt_in() {
        let mut c = cfg();
        c.coerce_scalars = true;
        let r = sanitize_json_args(r#"{"enabled":"true","disabled":"false"}"#, &c);
        let v = r.value.unwrap();
        assert_eq!(v.get("enabled"), Some(&json!(true)));
        assert_eq!(v.get("disabled"), Some(&json!(false)));
    }

    #[test]
    fn coerce_string_to_number_opt_in() {
        let mut c = cfg();
        c.coerce_scalars = true;
        let r = sanitize_json_args(r#"{"count":"42"}"#, &c);
        assert_eq!(r.value.unwrap().get("count"), Some(&json!(42.0)));
    }

    #[test]
    fn coerce_disabled_by_default() {
        let r = sanitize_json_args(r#"{"enabled":"true"}"#, &cfg());
        assert_eq!(r.value.unwrap().get("enabled"), Some(&json!("true")));
    }

    #[test]
    fn oversize_input_rejected_up_front() {
        let mut c = cfg();
        c.max_input_bytes = 16;
        let big = "{\"a\":\"".to_string() + &"x".repeat(100) + "\"}";
        let r = sanitize_json_args(&big, &c);
        assert_eq!(r.severity, SanitizationSeverity::Failed);
        assert!(
            r.actions
                .iter()
                .any(|a| matches!(a, SanitizationAction::ParseFailed { .. }))
        );
    }

    #[test]
    fn malformed_after_all_repairs_returns_failed() {
        let r = sanitize_json_args(r#"{not at all valid"#, &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Failed);
        assert!(r.value.is_none());
    }

    #[test]
    fn severity_labels_are_stable() {
        for s in [
            SanitizationSeverity::Clean,
            SanitizationSeverity::Cosmetic,
            SanitizationSeverity::Structural,
            SanitizationSeverity::Truncated,
            SanitizationSeverity::Failed,
        ] {
            let l = s.label();
            assert!(!l.is_empty());
            assert!(l.chars().all(|c| c.is_ascii_lowercase()));
        }
    }

    #[test]
    fn action_kinds_are_stable_kebab_case() {
        // Sample one of each action kind via construction.
        let samples = [
            SanitizationAction::TrimmedSurroundingWhitespace,
            SanitizationAction::StrippedCodeFence,
            SanitizationAction::StrippedTrailingComma,
            SanitizationAction::SwappedQuoteStyle,
            SanitizationAction::StrippedControlChars { count: 1 },
            SanitizationAction::StrippedDeniedKey {
                key: "k".into(),
                path: "$".into(),
            },
            SanitizationAction::TruncatedDepth {
                path: "$.a".into(),
                depth_at: 5,
            },
            SanitizationAction::DroppedExcessKeys {
                path: "$".into(),
                dropped: 1,
            },
            SanitizationAction::DroppedExcessElements {
                path: "$".into(),
                dropped: 1,
            },
            SanitizationAction::CoercedStringToBool {
                path: "$.a".into(),
                value: true,
            },
            SanitizationAction::CoercedStringToNumber {
                path: "$.a".into(),
                value: 1.0,
            },
            SanitizationAction::ParseFailed { error: "x".into() },
        ];
        for a in samples {
            let k = a.kind();
            assert!(!k.is_empty());
            assert!(k.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
        }
    }

    #[test]
    fn json_pointer_segments_sanity() {
        assert_eq!(json_pointer_segment("foo"), ".foo");
        assert_eq!(json_pointer_segment("with space"), "[\"with space\"]");
    }

    #[test]
    fn cosmetic_plus_structural_yields_structural() {
        let r = sanitize_json_args("  {\"a\":1,}  ", &cfg());
        assert_eq!(r.severity, SanitizationSeverity::Structural);
        assert_eq!(r.value, Some(json!({"a": 1})));
    }

    #[test]
    fn truncation_overrides_lower_severity() {
        let mut c = cfg();
        c.max_keys_per_object = 1;
        let r = sanitize_json_args(r#"  {"a":1,"b":2,}  "#, &c);
        assert_eq!(r.severity, SanitizationSeverity::Truncated);
    }
}
