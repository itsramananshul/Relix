//! YAML flow frontend — operator-friendly alternative to SOL syntax.
//!
//! A `.yml` / `.yaml` file is parsed into a list of typed steps
//! ([`YamlStep`]), then **lowered to SOL source text** that runs
//! through the existing SOL compile pipeline (`lexer → parser →
//! analyzer → bytecode`). Output is byte-identical to a
//! hand-written `.sol` file expressing the same flow — no new
//! VM, no new opcodes, no new dispatcher.
//!
//! This keeps the runtime completely unchanged: every YAML
//! construct picks an existing SOL feature to lower to. New
//! YAML steps only need a new lowering branch; they cannot add
//! new runtime semantics.
//!
//! ## Supported steps
//!
//! | YAML step | Lowered SOL |
//! |---|---|
//! | `let: { name, type, value }` | `let name: type = value;` |
//! | `call: { peer, method, arg, assign? }` | `remote_call(peer, method, arg)` (with optional assignment) |
//! | `stream: { peer, method, arg, assign? }` | `remote_call_stream(...)` (same shape) |
//! | `result: "<value>"` | `return value;` |
//! | `print: "<value>"` | `print(value);` |
//! | `if: { condition, then, else? }` | `if cond { ... } else { ... }` |
//! | `loop: { times: N, steps }` | a fresh-named integer counter + `while` |
//! | `loop: { for_each: x, in: list_var, steps }` | `for x in list_var { ... }` |
//! | `try: { steps, catch: { kind, steps } }` | `try { ... } catch <kind> { ... }` |
//!
//! ## Error mapping
//!
//! - YAML-level parse errors (malformed YAML) come from
//!   `serde_yaml` and carry a line/column locator.
//! - Schema errors (missing required field, wrong type at a
//!   field, unknown step name) carry a 1-based step path so
//!   operators can find the offending block.
//! - Compile errors from the underlying SOL pipeline indicate
//!   a bug in the YAML lowerer — they should never escape in
//!   production.
//!
//! See `docs/sol-language-reference.md` for the SOL syntax the
//! lowerings produce.

use std::fmt::Write;
use std::path::Path;

use serde_yaml::{Mapping, Value};

use crate::sol::bytecode::Inst;

/// YAML frontend error. Carries enough information for a
/// human operator to fix the file without reading the
/// compiler source.
#[derive(Debug, thiserror::Error)]
pub enum YamlFlowError {
    /// Structural YAML parse error (malformed YAML). Comes
    /// straight from `serde_yaml`; the message already
    /// includes the location.
    #[error("yaml parse error at line {line}, column {column}: {message}")]
    Parse {
        line: usize,
        column: usize,
        message: String,
    },

    /// Schema or lowering-time semantic error. The YAML is
    /// well-formed but violates the documented schema (e.g.
    /// `loop` step with neither `times` nor `for_each`, `let`
    /// with an unsupported `type`, unknown step name).
    ///
    /// `line` / `column` are 1-based positions of the
    /// offending step in the original YAML source. They are
    /// `0` when the YAML wasn't parsed from a positionable
    /// source (e.g. tests that build a `YamlFlow` directly).
    /// `path` carries the step-path locator
    /// (`step 2 → catch.step 1`) as additional context.
    #[error("at line {line}, column {column} ({path}): {message}")]
    Semantic {
        path: String,
        message: String,
        line: usize,
        column: usize,
    },

    /// The lowering produced SOL source that the SOL compiler
    /// rejected. This is a YAML-lowerer bug — operator
    /// shouldn't see it. The error includes the SOL message
    /// AND the lowered source so we can debug from the failure.
    #[error("yaml lowering produced invalid SOL ({sol_error}); lowered source:\n{lowered_source}")]
    Lower {
        sol_error: String,
        lowered_source: String,
    },

    /// File I/O failure when [`compile_path`] is called.
    #[error("yaml flow: read {path}: {cause}")]
    Io { path: String, cause: String },
}

// ──────────────────────────── YAML AST ──────────────────────────

/// Top-level YAML flow: just a sequence of steps.
#[derive(Debug)]
pub struct YamlFlow {
    /// The ordered list of steps executed when the flow runs.
    /// May be empty (a no-op flow that returns the empty
    /// string).
    pub steps: Vec<YamlStep>,
}

/// One step in a YAML flow. Each step is a one-key map; the
/// key names the step type, the value is the step's config.
#[derive(Debug)]
pub enum YamlStep {
    /// Declare a local variable.
    Let(LetStep),
    /// Unary `remote_call`.
    Call(CallStep),
    /// Streaming `remote_call_stream`.
    Stream(CallStep),
    /// Set the flow result.
    Result(String),
    /// Side-effect print.
    Print(String),
    /// Conditional branching.
    If(IfStep),
    /// Bounded iteration.
    Loop(LoopStep),
    /// Wrap a block in error handling.
    Try(TryStep),
}

/// `let` step config. The `value` is held as a raw
/// [`serde_yaml::Value`] so it can carry a native YAML sequence
/// (for `type: list`), a native YAML mapping (for `type: map`),
/// or a scalar (for the four scalar types). The lowerer
/// validates the value shape against the declared type and
/// recursively stringifies nested structures into SOL literal
/// syntax.
#[derive(Debug)]
pub struct LetStep {
    pub name: String,
    pub var_type: String,
    pub value: Value,
}

/// `call` / `stream` step config.
#[derive(Debug)]
pub struct CallStep {
    pub peer: String,
    pub method: String,
    pub arg: String,
    pub assign: Option<String>,
}

/// `if` step config.
#[derive(Debug)]
pub struct IfStep {
    pub condition: String,
    pub then: Vec<YamlStep>,
    pub r#else: Vec<YamlStep>,
}

/// `loop` step config. Exactly one of `times` or
/// `for_each` (plus `in`) must be set.
#[derive(Debug)]
pub struct LoopStep {
    pub times: Option<u32>,
    pub for_each: Option<String>,
    pub in_list: Option<String>,
    pub steps: Vec<YamlStep>,
}

/// `try` step config. Carries one OR many catch clauses —
/// SOL supports multiple catches per try, and the YAML format
/// now does too. The `catch:` field accepts either a single
/// mapping (single-catch shorthand, kept for backwards
/// compatibility) or a sequence of mappings (multi-catch
/// form), each with its own `kind` and `steps`.
#[derive(Debug)]
pub struct TryStep {
    pub steps: Vec<YamlStep>,
    /// Catch clauses in source order. Always at least one —
    /// `parse_try` rejects an empty list.
    pub catches: Vec<CatchStep>,
}

/// One catch clause inside a `try`.
#[derive(Debug)]
pub struct CatchStep {
    pub kind: String,
    pub steps: Vec<YamlStep>,
}

// ──────────────────────────── public API ────────────────────────

/// Compile a YAML flow source string to SOL bytecode the VM
/// can execute directly. Output is byte-identical to compiling
/// the equivalent `.sol` file.
///
/// This entry point also scans the source text to build a
/// step-line index, so schema errors raised during parsing or
/// lowering carry the real (line, column) of the offending
/// step in the original YAML — operators don't have to count
/// dashes to find it.
pub fn compile_source(yaml_source: &str) -> Result<Vec<Inst>, YamlFlowError> {
    let root: Value = serde_yaml::from_str(yaml_source).map_err(parse_error_from_serde)?;
    let index = std::sync::Arc::new(StepLineIndex::build(yaml_source));
    let flow = parse_flow_with_index(&root, index.clone())?;
    let lowered = lower_to_sol_with_index(&flow, index)?;
    crate::sol::compile_source(&lowered).map_err(|e| YamlFlowError::Lower {
        sol_error: e,
        lowered_source: lowered,
    })
}

/// Compile a YAML flow file at the given path. Convenience
/// wrapper around [`compile_source`].
pub fn compile_path(path: &Path) -> Result<Vec<Inst>, YamlFlowError> {
    let source = std::fs::read_to_string(path).map_err(|e| YamlFlowError::Io {
        path: path.display().to_string(),
        cause: e.to_string(),
    })?;
    compile_source(&source)
}

/// Lower a parsed [`YamlFlow`] to SOL source text. Exposed for
/// tests and tooling that wants to inspect the emitted SOL
/// without compiling.
///
/// **Scoping**: every variable name introduced by a `let` step
/// or a `call.assign` / `stream.assign` field is hoisted to the
/// outer function scope on a pre-pass, with a zero value
/// matching its declared type. The original `let` then becomes
/// a re-assignment inside whatever nested SOL block it lived
/// in. This makes the natural YAML pattern
///
/// ```yaml
/// - try:
///     steps: [{ call: {..., assign: reply} }]
///     catch: { kind: any, steps: [{ let: {name: reply, ...} }] }
/// - result: "{{reply}}"
/// ```
///
/// work — `reply` is visible to the final `result` step even
/// though SOL would otherwise have scoped it to the try / catch
/// bodies.
pub fn lower_to_sol(flow: &YamlFlow) -> Result<String, YamlFlowError> {
    lower_to_sol_with_index(flow, std::sync::Arc::new(StepLineIndex::empty()))
}

/// Same as [`lower_to_sol`] but takes a pre-built step-line
/// index so emitted errors carry real source positions.
/// Internal — `compile_source` calls this; external callers
/// use the index-free [`lower_to_sol`].
fn lower_to_sol_with_index(
    flow: &YamlFlow,
    index: std::sync::Arc<StepLineIndex>,
) -> Result<String, YamlFlowError> {
    let root_path = StepPath::root_with_index(index);
    let hoisted = collect_hoisted_decls(&flow.steps, &root_path)?;

    let mut ctx = Lowerer::new();
    ctx.emit("function start() -> str {\n");
    ctx.indent += 1;

    // Emit hoisted declarations at function entry with the
    // canonical zero value for the declared type. The
    // lowerer tracks `declared` so subsequent `let` / `assign`
    // emit re-assignments instead of fresh `let`s.
    for (name, ty) in &hoisted {
        ctx.indented(&format!(
            "let {name}: {} = {};\n",
            ty.as_sol(),
            ty.zero_lit()
        ));
        ctx.declared.insert(name.clone());
    }

    for (i, step) in flow.steps.iter().enumerate() {
        ctx.lower_step(step, &root_path.child(i))?;
    }
    if !ctx.has_explicit_result {
        ctx.indented("return \"\";\n");
    }
    ctx.indent -= 1;
    ctx.emit("}\n");
    Ok(ctx.out)
}

/// Walk the YAML AST and collect every variable that needs to
/// be hoisted to the function's outermost scope. Records each
/// name with its declared type. Conflicting types for the same
/// name surface as a schema error.
///
/// Excluded from hoisting: for-each loop variables (intrinsic
/// to the SOL `for` statement) and synthesised loop counters
/// (their names are gensym'd so they cannot collide).
fn collect_hoisted_decls(
    steps: &[YamlStep],
    path: &StepPath,
) -> Result<Vec<(String, LetType)>, YamlFlowError> {
    let mut decls: Vec<(String, LetType)> = Vec::new();
    let mut seen: std::collections::HashMap<String, LetType> = std::collections::HashMap::new();
    collect_steps(steps, path, &mut decls, &mut seen)?;
    Ok(decls)
}

fn collect_steps(
    steps: &[YamlStep],
    path: &StepPath,
    decls: &mut Vec<(String, LetType)>,
    seen: &mut std::collections::HashMap<String, LetType>,
) -> Result<(), YamlFlowError> {
    for (i, step) in steps.iter().enumerate() {
        let sp = path.child(i);
        match step {
            YamlStep::Let(s) => {
                let ty = validate_let_type(&s.var_type, &sp)?;
                record_decl(s.name.clone(), ty, &sp, decls, seen)?;
            }
            YamlStep::Call(s) | YamlStep::Stream(s) => {
                if let Some(name) = s.assign.as_ref() {
                    record_decl(name.clone(), LetType::Str, &sp, decls, seen)?;
                }
            }
            YamlStep::If(s) => {
                collect_steps(&s.then, &sp.named("then"), decls, seen)?;
                collect_steps(&s.r#else, &sp.named("else"), decls, seen)?;
            }
            YamlStep::Loop(s) => {
                // The for-each loop variable is NOT hoisted —
                // SOL declares it inside the `for` body. The
                // synthesised counter for counted loops is
                // gensym'd at lowering time so it also doesn't
                // appear here.
                collect_steps(&s.steps, &sp.named("loop"), decls, seen)?;
            }
            YamlStep::Try(s) => {
                collect_steps(&s.steps, &sp.named("try"), decls, seen)?;
                for (clause_index, c) in s.catches.iter().enumerate() {
                    let label = if s.catches.len() == 1 {
                        sp.named("catch")
                    } else {
                        sp.named(&format!("catch[{}]", clause_index))
                    };
                    collect_steps(&c.steps, &label, decls, seen)?;
                }
            }
            YamlStep::Result(_) | YamlStep::Print(_) => {}
        }
    }
    Ok(())
}

fn record_decl(
    name: String,
    ty: LetType,
    path: &StepPath,
    decls: &mut Vec<(String, LetType)>,
    seen: &mut std::collections::HashMap<String, LetType>,
) -> Result<(), YamlFlowError> {
    validate_ident(&name, "variable name", path)?;
    if let Some(existing) = seen.get(&name) {
        if existing.as_sol() != ty.as_sol() {
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: format!(
                    "variable `{name}` declared with conflicting types: first `{}`, later `{}`",
                    existing.as_sol(),
                    ty.as_sol()
                ),
            });
        }
        // Same type — ignore (multiple `let` of the same
        // name is allowed and lowers to re-assignment).
        return Ok(());
    }
    seen.insert(name.clone(), ty.clone());
    decls.push((name, ty));
    Ok(())
}

// ──────────────────────────── YAML → typed AST ──────────────────

/// Parse the root [`serde_yaml::Value`] into a typed
/// [`YamlFlow`]. Walks the value tree explicitly so each
/// schema error can carry a 1-based step path the operator
/// will recognise.
fn parse_flow(root: &Value) -> Result<YamlFlow, YamlFlowError> {
    parse_flow_with_index(root, std::sync::Arc::new(StepLineIndex::empty()))
}

/// Same as [`parse_flow`] but takes a pre-built step-line
/// index so emitted errors carry real source positions.
/// Internal — `compile_source` calls this; external callers
/// use the index-free [`parse_flow`].
fn parse_flow_with_index(
    root: &Value,
    index: std::sync::Arc<StepLineIndex>,
) -> Result<YamlFlow, YamlFlowError> {
    let root_path = StepPath::root_with_index(index);
    let root_map = expect_mapping(root, &root_path, "root")?;
    // Top-level keys: only `steps` is recognised today. An
    // unknown top-level key is treated as a clear schema error
    // so a typo doesn't silently skip a section.
    for (k, _) in root_map.iter() {
        match k.as_str() {
            Some("steps") => {}
            Some(other) => {
                return Err(YamlFlowError::Semantic {
                    line: 0,
                    column: 0,
                    path: "<root>".to_string(),
                    message: format!("unknown top-level key `{other}` — only `steps` is supported"),
                });
            }
            None => {
                return Err(YamlFlowError::Semantic {
                    line: 0,
                    column: 0,
                    path: "<root>".to_string(),
                    message: "top-level keys must be strings".to_string(),
                });
            }
        }
    }
    let steps_value = root_map.get(Value::String("steps".into())).cloned();
    let steps = match steps_value {
        Some(Value::Sequence(seq)) => parse_step_list(&seq, &root_path)?,
        Some(Value::Null) | None => Vec::new(),
        Some(_other) => {
            return Err(YamlFlowError::Semantic {
                line: 0,
                column: 0,
                path: "<root>".to_string(),
                message: "`steps` must be a sequence".to_string(),
            });
        }
    };
    Ok(YamlFlow { steps })
}

fn parse_step_list(seq: &[Value], parent: &StepPath) -> Result<Vec<YamlStep>, YamlFlowError> {
    seq.iter()
        .enumerate()
        .map(|(i, v)| parse_step(v, &parent.child(i)))
        .collect()
}

fn parse_step(value: &Value, path: &StepPath) -> Result<YamlStep, YamlFlowError> {
    let map = expect_mapping(value, path, "step")?;
    if map.len() != 1 {
        return Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "each step must be a single-key map (one of: let, call, stream, result, print, if, loop, try); got {} keys",
                map.len()
            ),
        });
    }
    let (k, body) = map.iter().next().expect("len == 1");
    let tag = k.as_str().ok_or_else(|| YamlFlowError::Semantic {
        line: path.location().0,
        column: path.location().1,
        path: path.render(),
        message: "step tag must be a string".to_string(),
    })?;
    match tag {
        "let" => Ok(YamlStep::Let(parse_let(body, path)?)),
        "call" => Ok(YamlStep::Call(parse_call(body, path)?)),
        "stream" => Ok(YamlStep::Stream(parse_call(body, path)?)),
        "result" => Ok(YamlStep::Result(expect_string_value(body, path, "result")?)),
        "print" => Ok(YamlStep::Print(expect_string_value(body, path, "print")?)),
        "if" => Ok(YamlStep::If(parse_if(body, path)?)),
        "loop" => Ok(YamlStep::Loop(parse_loop(body, path)?)),
        "try" => Ok(YamlStep::Try(parse_try(body, path)?)),
        other => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "unknown step type `{other}` — expected one of: let, call, stream, result, print, if, loop, try"
            ),
        }),
    }
}

fn parse_let(value: &Value, path: &StepPath) -> Result<LetStep, YamlFlowError> {
    let map = expect_mapping(value, path, "let body")?;
    deny_unknown_fields(map, path, "let", &["name", "type", "value"])?;
    let value_node = map
        .get(Value::String("value".into()))
        .cloned()
        .ok_or_else(|| YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: "missing required field `value`".to_string(),
        })?;
    Ok(LetStep {
        name: required_string(map, "name", path)?,
        var_type: required_string(map, "type", path)?,
        value: value_node,
    })
}

fn parse_call(value: &Value, path: &StepPath) -> Result<CallStep, YamlFlowError> {
    let map = expect_mapping(value, path, "call/stream body")?;
    deny_unknown_fields(
        map,
        path,
        "call/stream",
        &["peer", "method", "arg", "assign"],
    )?;
    Ok(CallStep {
        peer: required_string(map, "peer", path)?,
        method: required_string(map, "method", path)?,
        arg: required_string(map, "arg", path)?,
        assign: optional_string(map, "assign", path)?,
    })
}

fn parse_if(value: &Value, path: &StepPath) -> Result<IfStep, YamlFlowError> {
    let map = expect_mapping(value, path, "if body")?;
    deny_unknown_fields(map, path, "if", &["condition", "then", "else"])?;
    let condition = required_string(map, "condition", path)?;
    let then_seq = required_sequence(map, "then", path)?;
    let then = parse_step_list(&then_seq, &path.named("then"))?;
    let r#else = match map.get(Value::String("else".into())) {
        Some(Value::Sequence(seq)) => parse_step_list(seq, &path.named("else"))?,
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: "if.else must be a sequence of steps".to_string(),
            });
        }
    };
    Ok(IfStep {
        condition,
        then,
        r#else,
    })
}

fn parse_loop(value: &Value, path: &StepPath) -> Result<LoopStep, YamlFlowError> {
    let map = expect_mapping(value, path, "loop body")?;
    deny_unknown_fields(map, path, "loop", &["times", "for_each", "in", "steps"])?;
    let times = match map.get(Value::String("times".into())) {
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v) if v <= u32::MAX as u64 => Some(v as u32),
            _ => {
                return Err(YamlFlowError::Semantic {
                    line: path.location().0,
                    column: path.location().1,
                    path: path.render(),
                    message: format!("loop.times must be a non-negative integer (got `{n}`)"),
                });
            }
        },
        Some(Value::String(s)) => {
            // Tolerate `"3"` as well as `3` — operators
            // sometimes quote everything.
            match s.parse::<u32>() {
                Ok(v) => Some(v),
                Err(_) => {
                    return Err(YamlFlowError::Semantic {
                        line: path.location().0,
                        column: path.location().1,
                        path: path.render(),
                        message: format!("loop.times must be a non-negative integer (got `{s}`)"),
                    });
                }
            }
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: "loop.times must be an integer".to_string(),
            });
        }
    };
    let for_each = optional_string(map, "for_each", path)?;
    let in_list = optional_string(map, "in", path)?;
    let steps_seq = required_sequence(map, "steps", path)?;
    let steps = parse_step_list(&steps_seq, &path.named("loop"))?;
    Ok(LoopStep {
        times,
        for_each,
        in_list,
        steps,
    })
}

fn parse_try(value: &Value, path: &StepPath) -> Result<TryStep, YamlFlowError> {
    let map = expect_mapping(value, path, "try body")?;
    deny_unknown_fields(map, path, "try", &["steps", "catch"])?;
    let steps_seq = required_sequence(map, "steps", path)?;
    let steps = parse_step_list(&steps_seq, &path.named("try"))?;
    let catch_value =
        map.get(Value::String("catch".into()))
            .ok_or_else(|| YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: "try step missing required field `catch`".to_string(),
            })?;
    // `catch` accepts either a single mapping (single-catch
    // shorthand) or a sequence of mappings (multi-catch).
    // The multi-catch form lets operators handle timeout
    // differently from policy_denied, etc., without dropping
    // to SOL.
    let catches = match catch_value {
        Value::Mapping(_) => vec![parse_catch_clause(catch_value, path, 0)?],
        Value::Sequence(seq) => {
            if seq.is_empty() {
                return Err(YamlFlowError::Semantic {
                    line: path.location().0,
                    column: path.location().1,
                    path: path.render(),
                    message: "try.catch sequence must contain at least one clause".to_string(),
                });
            }
            seq.iter()
                .enumerate()
                .map(|(i, v)| parse_catch_clause(v, path, i))
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => {
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "try.catch must be a mapping (single catch) or a sequence of mappings (multi-catch)"
                        .to_string(),
            });
        }
    };
    Ok(TryStep { steps, catches })
}

fn parse_catch_clause(
    value: &Value,
    parent: &StepPath,
    clause_index: usize,
) -> Result<CatchStep, YamlFlowError> {
    let label = if clause_index == 0 && matches!(value, Value::Mapping(_)) {
        // Single-catch shorthand path — keep the locator as
        // simply `catch` so the message looks the same as
        // pre-multi-catch flows.
        parent.named("catch")
    } else {
        parent.named(&format!("catch[{}]", clause_index))
    };
    let map = expect_mapping(value, &label, "catch clause")?;
    deny_unknown_fields(map, &label, "catch", &["kind", "steps"])?;
    let kind = required_string(map, "kind", &label)?;
    let steps_seq = required_sequence(map, "steps", &label)?;
    let steps = parse_step_list(&steps_seq, &label)?;
    Ok(CatchStep { kind, steps })
}

// ──────────────────────────── parsing helpers ──────────────────

fn expect_mapping<'v>(
    value: &'v Value,
    path: &StepPath,
    what: &str,
) -> Result<&'v Mapping, YamlFlowError> {
    match value {
        Value::Mapping(m) => Ok(m),
        Value::Null => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("{what} is empty — expected a mapping with required fields"),
        }),
        _ => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("{what} must be a mapping"),
        }),
    }
}

fn expect_string_value(
    value: &Value,
    path: &StepPath,
    what: &str,
) -> Result<String, YamlFlowError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("{what} value is empty"),
        }),
        _ => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("{what} value must be a scalar (string / number / bool)"),
        }),
    }
}

fn required_string(map: &Mapping, key: &str, path: &StepPath) -> Result<String, YamlFlowError> {
    match map.get(Value::String(key.into())) {
        Some(v) => match v {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: format!("field `{key}` must be a scalar string"),
            }),
        },
        None => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("missing required field `{key}`"),
        }),
    }
}

fn optional_string(
    map: &Mapping,
    key: &str,
    path: &StepPath,
) -> Result<Option<String>, YamlFlowError> {
    match map.get(Value::String(key.into())) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("field `{key}` must be a scalar string"),
        }),
    }
}

fn required_sequence(
    map: &Mapping,
    key: &str,
    path: &StepPath,
) -> Result<Vec<Value>, YamlFlowError> {
    match map.get(Value::String(key.into())) {
        Some(Value::Sequence(seq)) => Ok(seq.clone()),
        Some(_) => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("field `{key}` must be a sequence"),
        }),
        None => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("missing required field `{key}`"),
        }),
    }
}

fn deny_unknown_fields(
    map: &Mapping,
    path: &StepPath,
    step: &str,
    allowed: &[&str],
) -> Result<(), YamlFlowError> {
    for (k, _) in map.iter() {
        match k.as_str() {
            Some(name) if allowed.contains(&name) => {}
            Some(name) => {
                return Err(YamlFlowError::Semantic {
                    line: path.location().0,
                    column: path.location().1,
                    path: path.render(),
                    message: format!(
                        "unknown `{step}` field `{name}` (allowed: {})",
                        allowed.join(", ")
                    ),
                });
            }
            None => {
                return Err(YamlFlowError::Semantic {
                    line: path.location().0,
                    column: path.location().1,
                    path: path.render(),
                    message: format!("`{step}` field names must be strings"),
                });
            }
        }
    }
    Ok(())
}

// ──────────────────────────── line index ───────────────────────

/// Source-text scan that records the (line, column) of each
/// top-level step in a YAML flow. Used to attach real source
/// positions to schema errors so the operator sees something
/// like `line 14, column 5` instead of just a step path.
///
/// The scan handles standard block-style YAML:
///
/// ```yaml
/// steps:
///   - let: ...           # step 1, line N
///       ...
///   - call: ...          # step 2, line M
///       ...
/// ```
///
/// Flow-style (`steps: [{...}, {...}]`) is not located —
/// every step gets `(0, 0)`. Operators authoring with the
/// inline form are presumed to be power users who can find
/// the offending step from the message alone.
#[derive(Debug, Default)]
struct StepLineIndex {
    /// 1-based (line, column) for each top-level step. The
    /// column is the column of the `- ` dash character.
    lines: Vec<(usize, usize)>,
}

impl StepLineIndex {
    /// Empty index — every lookup returns `(0, 0)`. Used by
    /// the public test entry points (`parse_flow`,
    /// `lower_to_sol`) that don't have a source string to
    /// scan.
    fn empty() -> Self {
        Self { lines: Vec::new() }
    }

    /// Scan a YAML source string and record line numbers for
    /// every top-level step. Forgiving: lines that aren't
    /// part of the `steps:` section are ignored, comments and
    /// blank lines are skipped, an unusual indent falls back
    /// to (0, 0) without crashing. The indent of `steps:` is
    /// taken from wherever it appears in the file — operator
    /// files typically have it at column 0, but YAML embedded
    /// in test fixtures or doc strings may sit at deeper
    /// indents.
    fn build(source: &str) -> Self {
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut steps_key_indent: Option<usize> = None;
        let mut steps_dash_indent: Option<usize> = None;

        for (idx, raw_line) in source.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = raw_line.trim_start();
            let indent = raw_line.len() - trimmed.len();

            // Skip blank lines and pure comments.
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            if steps_key_indent.is_none() {
                if trimmed.starts_with("steps:") {
                    steps_key_indent = Some(indent);
                }
                continue;
            }
            let steps_ki = steps_key_indent.unwrap();

            // Inside the steps section. A line starting with
            // `- ` (or bare `-` on its own) at an indent
            // greater than the `steps:` key's indent
            // introduces a new step at the first such depth
            // observed (subsequent same-indent dashes are
            // siblings).
            let is_dash = trimmed.starts_with("- ") || trimmed == "-";
            if is_dash && indent > steps_ki {
                if steps_dash_indent.is_none() {
                    steps_dash_indent = Some(indent);
                }
                if Some(indent) == steps_dash_indent {
                    lines.push((line_no, indent + 1));
                }
                continue;
            }

            // A sibling key at the steps-key indent ends the
            // steps section (e.g. another root-level key
            // declared after `steps`).
            if indent <= steps_ki && !is_dash {
                break;
            }
        }

        Self { lines }
    }

    /// Look up the (line, column) of the top-level step
    /// referenced by `path`. Returns `(0, 0)` when the path
    /// has no `step N` segment or when the index is empty.
    fn lookup(&self, path: &[String]) -> (usize, usize) {
        for seg in path {
            if let Some(rest) = seg.strip_prefix("step ")
                && let Ok(n) = rest.parse::<usize>()
                && n >= 1
            {
                return self.lines.get(n - 1).copied().unwrap_or((0, 0));
            }
        }
        (0, 0)
    }
}

// ──────────────────────────── lowerer ───────────────────────────

/// Path through the YAML tree, used for step-located error
/// messages. `step 2 → then.step 1 → catch.step 3` etc. The
/// path also carries the source-line index so error sites
/// can compute the real (line, column) of the offending step
/// without threading the index through every helper
/// individually.
#[derive(Clone, Debug)]
struct StepPath {
    segments: Vec<String>,
    index: std::sync::Arc<StepLineIndex>,
}

impl StepPath {
    fn root_with_index(index: std::sync::Arc<StepLineIndex>) -> Self {
        Self {
            segments: Vec::new(),
            index,
        }
    }
    fn child(&self, idx: usize) -> Self {
        let mut s = self.segments.clone();
        s.push(format!("step {}", idx + 1));
        Self {
            segments: s,
            index: self.index.clone(),
        }
    }
    fn named(&self, name: &str) -> Self {
        let mut s = self.segments.clone();
        s.push(name.to_string());
        Self {
            segments: s,
            index: self.index.clone(),
        }
    }
    fn render(&self) -> String {
        if self.segments.is_empty() {
            "<root>".to_string()
        } else {
            self.segments.join(" → ")
        }
    }
    /// Look up the (line, column) for the leading top-level
    /// step in this path. Returns `(0, 0)` when no index was
    /// supplied or the path doesn't reference a top-level
    /// step.
    fn location(&self) -> (usize, usize) {
        self.index.lookup(&self.segments)
    }
}

/// SOL source builder + lowering state.
struct Lowerer {
    out: String,
    indent: usize,
    /// Names declared so far at any scope. Used to decide
    /// whether a `call.assign` should emit `let name: str =
    /// ...` (first use) or `name = ...` (re-assignment).
    declared: std::collections::HashSet<String>,
    /// Monotonically-increasing counter for synthesised loop
    /// counter variables (`__yaml_loop_i_0`, `__yaml_loop_i_1`,
    /// ...) so two top-level counted loops don't collide on
    /// the same name.
    loop_counter: usize,
    /// Set when any step lowers to a top-level `return ...;`
    /// so the function epilogue knows whether to append a
    /// default `return "";`.
    has_explicit_result: bool,
}

impl Lowerer {
    fn new() -> Self {
        Self {
            out: String::new(),
            indent: 0,
            declared: std::collections::HashSet::new(),
            loop_counter: 0,
            has_explicit_result: false,
        }
    }

    fn emit(&mut self, s: &str) {
        self.out.push_str(s);
    }

    fn indented(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
    }

    fn next_loop_var(&mut self) -> String {
        let n = self.loop_counter;
        self.loop_counter += 1;
        format!("__yaml_loop_i_{n}")
    }

    fn lower_step(&mut self, step: &YamlStep, path: &StepPath) -> Result<(), YamlFlowError> {
        match step {
            YamlStep::Let(s) => self.lower_let(s, path),
            YamlStep::Call(s) => self.lower_call(s, "remote_call", path),
            YamlStep::Stream(s) => self.lower_call(s, "remote_call_stream", path),
            YamlStep::Result(value) => {
                let lit = sol_string_literal(value, path)?;
                self.indented(&format!("return {lit};\n"));
                self.has_explicit_result = true;
                Ok(())
            }
            YamlStep::Print(value) => {
                let lit = sol_string_literal(value, path)?;
                self.indented(&format!("print({lit});\n"));
                Ok(())
            }
            YamlStep::If(s) => self.lower_if(s, path),
            YamlStep::Loop(s) => self.lower_loop(s, path),
            YamlStep::Try(s) => self.lower_try(s, path),
        }
    }

    fn lower_let(&mut self, s: &LetStep, path: &StepPath) -> Result<(), YamlFlowError> {
        validate_ident(&s.name, "let.name", path)?;
        let ty = validate_let_type(&s.var_type, path)?;
        let rhs = lower_let_value(&ty, &s.value, path)?;
        // Every name introduced by `let` or `call.assign` is
        // hoisted to the function's outer scope by
        // `collect_hoisted_decls`. So the FIRST encounter at
        // lowering time still needs to emit a re-assignment —
        // the outer declaration already exists.
        if self.declared.contains(&s.name) {
            self.indented(&format!("{} = {};\n", s.name, rhs));
        } else {
            self.indented(&format!("let {}: {} = {};\n", s.name, ty.as_sol(), rhs));
            self.declared.insert(s.name.clone());
        }
        Ok(())
    }

    fn lower_call(
        &mut self,
        s: &CallStep,
        builtin: &str,
        path: &StepPath,
    ) -> Result<(), YamlFlowError> {
        let peer = sol_string_literal(&s.peer, path)?;
        let method = sol_string_literal(&s.method, path)?;
        let arg = sol_string_literal(&s.arg, path)?;
        let invocation = format!("{builtin}({peer}, {method}, {arg})");

        if let Some(assign) = s.assign.as_deref() {
            validate_ident(assign, "call.assign", path)?;
            if self.declared.contains(assign) {
                self.indented(&format!("{assign} = {invocation};\n"));
            } else {
                self.indented(&format!("let {assign}: str = {invocation};\n"));
                self.declared.insert(assign.to_string());
            }
        } else {
            self.indented(&format!("{invocation};\n"));
        }
        Ok(())
    }

    fn lower_if(&mut self, s: &IfStep, path: &StepPath) -> Result<(), YamlFlowError> {
        self.indented(&format!("if {} {{\n", s.condition.trim()));
        self.indent += 1;
        for (i, step) in s.then.iter().enumerate() {
            self.lower_step(step, &path.named("then").child(i))?;
        }
        self.indent -= 1;
        if !s.r#else.is_empty() {
            self.indented("} else {\n");
            self.indent += 1;
            for (i, step) in s.r#else.iter().enumerate() {
                self.lower_step(step, &path.named("else").child(i))?;
            }
            self.indent -= 1;
            self.indented("}\n");
        } else {
            self.indented("}\n");
        }
        Ok(())
    }

    fn lower_loop(&mut self, s: &LoopStep, path: &StepPath) -> Result<(), YamlFlowError> {
        match (s.times.as_ref(), s.for_each.as_deref(), s.in_list.as_deref()) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "loop step must set EITHER `times` (counted) OR `for_each` + `in` (collection), not both"
                        .to_string(),
            }),
            (Some(n), None, None) => self.lower_counted_loop(*n, &s.steps, path),
            (None, Some(name), Some(list_var)) => {
                self.lower_for_each_loop(name, list_var, &s.steps, path)
            }
            (None, Some(_), None) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "loop step has `for_each` but no `in` — set `in: <list_var>` to name the list"
                        .to_string(),
            }),
            (None, None, Some(_)) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "loop step has `in` but no `for_each` — set `for_each: <name>` for the loop variable"
                        .to_string(),
            }),
            (None, None, None) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "loop step must set EITHER `times: <N>` (counted) OR `for_each: <name>` + `in: <list_var>` (collection)"
                        .to_string(),
            }),
        }
    }

    fn lower_counted_loop(
        &mut self,
        n: u32,
        steps: &[YamlStep],
        path: &StepPath,
    ) -> Result<(), YamlFlowError> {
        let counter = self.next_loop_var();
        // Open a nested block so the counter goes out of
        // scope after the loop completes — important if two
        // counted loops sit side by side at the top level.
        self.indented("{\n");
        self.indent += 1;
        self.indented(&format!("let {counter}: int = 0;\n"));
        self.indented(&format!("while {counter} < {n} {{\n"));
        self.indent += 1;
        for (i, step) in steps.iter().enumerate() {
            self.lower_step(step, &path.named("loop").child(i))?;
        }
        self.indented(&format!("{counter} = {counter} + 1;\n"));
        self.indent -= 1;
        self.indented("}\n");
        self.indent -= 1;
        self.indented("}\n");
        Ok(())
    }

    fn lower_for_each_loop(
        &mut self,
        name: &str,
        list_var: &str,
        steps: &[YamlStep],
        path: &StepPath,
    ) -> Result<(), YamlFlowError> {
        validate_ident(name, "loop.for_each", path)?;
        validate_ident(list_var, "loop.in", path)?;
        self.indented(&format!("for {name} in {list_var} {{\n"));
        self.indent += 1;
        let added = self.declared.insert(name.to_string());
        for (i, step) in steps.iter().enumerate() {
            self.lower_step(step, &path.named("loop").child(i))?;
        }
        if added {
            self.declared.remove(name);
        }
        self.indent -= 1;
        self.indented("}\n");
        Ok(())
    }

    fn lower_try(&mut self, s: &TryStep, path: &StepPath) -> Result<(), YamlFlowError> {
        if s.catches.is_empty() {
            // Defensive — `parse_try` enforces non-empty
            // already; this guards against future direct
            // AST construction.
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: "try step has no catch clauses".to_string(),
            });
        }
        for c in &s.catches {
            validate_catch_kind(&c.kind, path)?;
        }
        self.indented("try {\n");
        self.indent += 1;
        for (i, step) in s.steps.iter().enumerate() {
            self.lower_step(step, &path.named("try").child(i))?;
        }
        self.indent -= 1;
        // Emit one SOL `catch <kind> { ... }` per clause, in
        // source order. The SOL parser accepts the
        // `} catch ... {` chain — see sol/parser.rs:try_stmt.
        for (clause_index, catch) in s.catches.iter().enumerate() {
            let catch_path = if s.catches.len() == 1 {
                path.named("catch")
            } else {
                path.named(&format!("catch[{}]", clause_index))
            };
            self.indented(&format!("}} catch {} {{\n", catch.kind));
            self.indent += 1;
            for (i, step) in catch.steps.iter().enumerate() {
                self.lower_step(step, &catch_path.child(i))?;
            }
            self.indent -= 1;
        }
        self.indented("}\n");
        Ok(())
    }
}

// ──────────────────────────── helpers ───────────────────────────

#[derive(Clone, Debug)]
enum LetType {
    Int,
    Str,
    Bool,
    Float,
    List,
    Map,
}

impl LetType {
    fn as_sol(&self) -> &'static str {
        match self {
            LetType::Int => "int",
            LetType::Str => "str",
            LetType::Bool => "bool",
            LetType::Float => "float",
            LetType::List => "list",
            LetType::Map => "map",
        }
    }

    /// The canonical zero / default value used for the
    /// hoisted declaration emitted at function entry. The
    /// operator's `let` step inside the flow will overwrite
    /// it via re-assignment before any user code reads it.
    fn zero_lit(&self) -> &'static str {
        match self {
            LetType::Int => "0",
            LetType::Str => "\"\"",
            LetType::Bool => "false",
            LetType::Float => "0.0",
            LetType::List => "[]",
            LetType::Map => "{}",
        }
    }
}

fn validate_let_type(ty: &str, path: &StepPath) -> Result<LetType, YamlFlowError> {
    match ty {
        "int" => Ok(LetType::Int),
        "str" => Ok(LetType::Str),
        "bool" => Ok(LetType::Bool),
        "float" => Ok(LetType::Float),
        "list" => Ok(LetType::List),
        "map" => Ok(LetType::Map),
        other => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "let.type `{other}` is not supported — expected one of: int, str, bool, float, list, map"
            ),
        }),
    }
}

fn validate_ident(name: &str, what: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    if name.is_empty() {
        return Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("{what} is empty"),
        });
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "{what} `{name}` is not a valid SOL identifier (must start with letter or underscore)"
            ),
        });
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message: format!(
                    "{what} `{name}` contains invalid character `{c}` (only letters, digits, underscore allowed)"
                ),
            });
        }
    }
    Ok(())
}

fn validate_catch_kind(kind: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    match kind {
        "any" | "timeout" | "mesh_error" | "policy_denied" | "responder_error" => Ok(()),
        other => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "catch.kind `{other}` is not a recognised SOL kind — expected one of: any, timeout, mesh_error, policy_denied, responder_error"
            ),
        }),
    }
}

/// Lower the `value` of a `let` step into SOL source according
/// to the declared `type`. Native YAML sequences and mappings
/// are accepted directly for `list` / `map` types and
/// recursively stringified into SOL literal syntax. Scalars are
/// emitted verbatim (for int / bool / float) or as a SOL string
/// literal (for str). A value shape that doesn't match the
/// declared type surfaces a clear semantic error.
fn lower_let_value(ty: &LetType, value: &Value, path: &StepPath) -> Result<String, YamlFlowError> {
    match ty {
        LetType::Str => match value {
            Value::String(s) => sol_string_literal(s, path),
            Value::Number(n) => sol_string_literal(&n.to_string(), path),
            Value::Bool(b) => sol_string_literal(&b.to_string(), path),
            Value::Null => sol_string_literal("", path),
            Value::Sequence(_) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value is a YAML sequence but let.type is `str` — use `type: list` for sequence values"
                        .to_string(),
            }),
            Value::Mapping(_) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value is a YAML mapping but let.type is `str` — use `type: map` for mapping values"
                        .to_string(),
            }),
            Value::Tagged(t) => lower_let_value(ty, &t.value, path),
        },
        LetType::Int | LetType::Float => require_scalar_unquoted(value, ty, path),
        LetType::Bool => require_scalar_unquoted(value, ty, path),
        LetType::List => match value {
            // Native YAML sequence — recursively stringify
            // into a SOL `[a, b, c]` literal.
            Value::Sequence(_) => yaml_to_sol_list_or_map(value, path),
            // Backwards-compatible: a quoted string carrying
            // the SOL list literal verbatim. Still supported
            // so flows authored before native lists worked
            // keep compiling.
            Value::String(s) => Ok(s.clone()),
            Value::Mapping(_) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value is a YAML mapping but let.type is `list` — use `type: map` for mapping values"
                        .to_string(),
            }),
            Value::Number(_) | Value::Bool(_) | Value::Null => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value must be a sequence for type `list` (or a SOL list literal as a string)"
                        .to_string(),
            }),
            Value::Tagged(t) => lower_let_value(ty, &t.value, path),
        },
        LetType::Map => match value {
            Value::Mapping(_) => yaml_to_sol_list_or_map(value, path),
            Value::String(s) => Ok(s.clone()),
            Value::Sequence(_) => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value is a YAML sequence but let.type is `map` — use `type: list` for sequence values"
                        .to_string(),
            }),
            Value::Number(_) | Value::Bool(_) | Value::Null => Err(YamlFlowError::Semantic {
                line: path.location().0,
                column: path.location().1,
                path: path.render(),
                message:
                    "let.value must be a mapping for type `map` (or a SOL map literal as a string)"
                        .to_string(),
            }),
            Value::Tagged(t) => lower_let_value(ty, &t.value, path),
        },
    }
}

fn require_scalar_unquoted(
    value: &Value,
    ty: &LetType,
    path: &StepPath,
) -> Result<String, YamlFlowError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Sequence(_) => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "let.value is a YAML sequence but let.type is `{}` — use `type: list` for sequence values",
                ty.as_sol()
            ),
        }),
        Value::Mapping(_) => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!(
                "let.value is a YAML mapping but let.type is `{}` — use `type: map` for mapping values",
                ty.as_sol()
            ),
        }),
        Value::Null => Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message: format!("let.value for type `{}` cannot be null", ty.as_sol()),
        }),
        Value::Tagged(t) => require_scalar_unquoted(&t.value, ty, path),
    }
}

/// Recursively turn a YAML `Value` into the SOL expression that
/// produces the same logical value. Strings become SOL string
/// literals, numbers / bools / null stay verbatim, sequences
/// become `[a, b, c]` SOL lists, and mappings become
/// `{"k": v, ...}` SOL maps. Nested lists and maps are handled
/// by the recursion.
fn yaml_to_sol_expr(value: &Value, path: &StepPath) -> Result<String, YamlFlowError> {
    match value {
        Value::String(s) => sol_string_literal(s, path),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => sol_string_literal("", path),
        Value::Sequence(seq) => {
            let mut parts = Vec::with_capacity(seq.len());
            for v in seq {
                parts.push(yaml_to_sol_expr(v, path)?);
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        Value::Mapping(m) => {
            let mut parts = Vec::with_capacity(m.len());
            for (k, v) in m {
                let key_str = match k {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(YamlFlowError::Semantic {
                            line: path.location().0,
                            column: path.location().1,
                            path: path.render(),
                            message: "map keys must be scalar strings (SOL map literals only accept string-literal keys)"
                                .to_string(),
                        });
                    }
                };
                let key_lit = sol_string_literal(&key_str, path)?;
                let val_expr = yaml_to_sol_expr(v, path)?;
                parts.push(format!("{key_lit}: {val_expr}"));
            }
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        Value::Tagged(t) => yaml_to_sol_expr(&t.value, path),
    }
}

/// Shim: the public entry point exposed to the lowerer for
/// list / map values. Identical to `yaml_to_sol_expr` but
/// named for clarity at the call site.
fn yaml_to_sol_list_or_map(value: &Value, path: &StepPath) -> Result<String, YamlFlowError> {
    yaml_to_sol_expr(value, path)
}

/// SOL strings have no escape sequences (SIMP-016). A literal
/// `"` would prematurely close the SOL string. Emit the value
/// verbatim between two `"` characters, refusing anything that
/// would produce malformed SOL.
fn sol_string_literal(value: &str, path: &StepPath) -> Result<String, YamlFlowError> {
    if value.contains('"') {
        return Err(YamlFlowError::Semantic {
            line: path.location().0,
            column: path.location().1,
            path: path.render(),
            message:
                "string value contains a `\"` character; SOL has no escape sequences (SIMP-016) so quotes inside strings are unsupported"
                    .to_string(),
        });
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    out.push_str(value);
    out.push('"');
    Ok(out)
}

fn parse_error_from_serde(e: serde_yaml::Error) -> YamlFlowError {
    let location = e.location();
    let (line, column) = location.map(|l| (l.line(), l.column())).unwrap_or((0, 0));
    let message = e.to_string();
    YamlFlowError::Parse {
        line,
        column,
        message,
    }
}

/// Render the lowering of a small flow as a debug string,
/// useful for tooling that wants to preview the emitted SOL.
/// Returns the same source the underlying SOL compiler would
/// see. Errors short-circuit with a single-line summary.
#[allow(dead_code)]
pub(crate) fn debug_lower(yaml: &str) -> String {
    match serde_yaml::from_str::<Value>(yaml) {
        Ok(root) => match parse_flow(&root) {
            Ok(flow) => match lower_to_sol(&flow) {
                Ok(s) => s,
                Err(e) => {
                    let mut buf = String::new();
                    let _ = write!(buf, "/* lowering error: {e} */");
                    buf
                }
            },
            Err(e) => format!("/* parse error: {e} */"),
        },
        Err(e) => format!("/* yaml error: {e} */"),
    }
}

#[cfg(test)]
mod tests;
