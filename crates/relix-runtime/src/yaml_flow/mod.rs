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
//! | `try: { steps, catch: { kind, steps } }` or `catch: [ ... ]` | `try { ... } catch <kind> { ... }` |
//!
//! ## Error mapping
//!
//! - YAML-level parse errors (malformed YAML) come from the
//!   `saphyr` parser and carry an exact line/column locator.
//! - Schema errors (missing required field, wrong type at a
//!   field, unknown step name) carry the **real** line/column
//!   of the offending node — saphyr's annotated tree
//!   ([`saphyr::MarkedYamlOwned`]) has a [`Span`] on every
//!   node, so nested errors point at the nested node, not the
//!   outer step.
//! - `Lower` errors indicate a YAML-frontend bug — they
//!   include the lowered SOL plus the most recently lowered
//!   step's path so the bug report is actionable.
//! - `Io` errors include the file path.
//!
//! See `docs/sol-language-reference.md` for the SOL syntax the
//! lowerings produce. See `docs/yaml-flow-reference.md` for the
//! operator-facing YAML reference.

use std::fmt::Write;
use std::path::Path;

use saphyr::{LoadableYamlNode, MarkedYamlOwned, ScalarOwned, YamlDataOwned};

use crate::sol::bytecode::Inst;

/// Convenience alias for saphyr's annotated YAML node.
type Node = MarkedYamlOwned;

/// Extract the 1-based `(line, column)` of a node from its
/// saphyr span. Scalar nodes have a span set directly by the
/// loader. Mappings and sequences in `saphyr 0.0.6`'s
/// `MarkedYamlOwned` get a default (0, 0) span — the
/// `LoadableYamlNode::with_start_marker` default impl is a
/// no-op for the owned variant. Fall back to the first child's
/// span so collections still report a useful position.
fn node_pos(n: &Node) -> (usize, usize) {
    let direct = (n.span.start.line(), n.span.start.col());
    if direct.0 > 0 {
        return direct;
    }
    match &n.data {
        YamlDataOwned::Mapping(m) => {
            if let Some((k, _)) = m.iter().next() {
                let from_key = (k.span.start.line(), k.span.start.col());
                if from_key.0 > 0 {
                    return from_key;
                }
            }
            direct
        }
        YamlDataOwned::Sequence(seq) => {
            if let Some(first) = seq.first() {
                let from_first = (first.span.start.line(), first.span.start.col());
                if from_first.0 > 0 {
                    return from_first;
                }
            }
            direct
        }
        YamlDataOwned::Tagged(_, inner) => node_pos(inner),
        _ => direct,
    }
}

/// YAML frontend error. Carries enough information for a
/// human operator to fix the file without reading the
/// compiler source.
#[derive(Debug, thiserror::Error)]
pub enum YamlFlowError {
    /// Structural YAML parse error (malformed YAML). Comes
    /// from saphyr; the `(line, column)` is the exact
    /// position of the offending token.
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
    /// `line` / `column` are the 1-based source position of
    /// the **offending node** as recorded by saphyr — nested
    /// errors point at the nested node, not the outer step.
    /// `path` carries a step-path locator
    /// (`step 2 → catch.step 1`) as additional context.
    #[error("at line {line}, column {column} ({path}): {message}")]
    Semantic {
        path: String,
        message: String,
        line: usize,
        column: usize,
    },

    /// The lowering produced SOL source that the SOL compiler
    /// rejected. This is a YAML-lowerer bug — operators
    /// shouldn't see it in production. The error includes
    /// the SOL message, the lowered source, AND the step
    /// path of the last step successfully lowered so the
    /// bug report names a starting point.
    #[error(
        "yaml lowering produced invalid SOL ({sol_error}); last lowered step: {step_context}; lowered source:\n{lowered_source}"
    )]
    Lower {
        sol_error: String,
        step_context: String,
        lowered_source: String,
    },

    /// File I/O failure when [`compile_path`] is called.
    /// Includes the full file path the bridge / CLI tried
    /// to read so the operator sees exactly which file
    /// failed to open.
    #[error("yaml flow: failed to read `{path}`: {cause}")]
    Io { path: String, cause: String },

    /// SEC PART 3: a user-supplied `if.condition` string
    /// (or any other field interpolated raw into SOL source)
    /// contained characters outside the allowlist for SOL
    /// boolean predicates. Pre-fix path emitted the raw
    /// string verbatim into the lowered SOL source, allowing
    /// arbitrary statements to be smuggled in. The allowlist
    /// is `^[A-Za-z0-9_\.\s\(\)\!\=\<\>\&\|]+$` — the exact
    /// character set of legitimate SOL boolean predicates
    /// (identifiers, member access, parentheses, comparison
    /// + logical operators, whitespace).
    #[error(
        "at {path}: invalid condition `{value}` — only [A-Za-z0-9_\\.\\s\\(\\)\\!\\=\\<\\>\\&\\|] characters are allowed (SOL boolean predicate grammar)"
    )]
    InvalidCondition { path: String, value: String },

    /// SEC PART 3: a user-supplied scalar value that would be
    /// interpolated raw into SOL source (`int`/`bool`/`float`
    /// `let.value` strings, or string-typed `list`/`map`
    /// fallback literals) failed its grammar-specific
    /// allowlist. Same posture as `InvalidCondition` but for
    /// scalars whose grammar is narrower than predicate.
    #[error("at {path}: invalid {what} value `{value}` — fails the SOL {what} grammar allowlist")]
    InvalidScalar {
        path: String,
        what: &'static str,
        value: String,
    },

    /// CORR PART 1: an operator-supplied YAML file is bigger
    /// than [`MAX_YAML_FILE_BYTES`]. Pre-fix path called
    /// `std::fs::read_to_string` with no size cap, so a
    /// malicious / accidental gigabyte-sized .yml could
    /// allocate unbounded memory. This error is raised at the
    /// boundary BEFORE the YAML parser is asked to do any work.
    #[error("yaml flow: `{path}` is {size_bytes} bytes; max is {max_bytes}")]
    FileTooLarge {
        path: String,
        size_bytes: u64,
        max_bytes: u64,
    },

    /// CORR PART 1: the YAML document nests deeper than
    /// [`MAX_YAML_NESTING_DEPTH`]. Pre-fix path let the parser
    /// recurse for as long as the heap held out, which a
    /// malicious flow could weaponise into stack exhaustion or
    /// O(n) memory blowup. The depth bound is honest about
    /// what real operator flows look like (well under 20).
    #[error("yaml flow: nesting depth {depth} exceeds max {max_depth} at {path}")]
    NestingTooDeep {
        path: String,
        depth: usize,
        max_depth: usize,
    },
}

/// CORR PART 1: hard ceiling on the size of a YAML flow file
/// the bridge / runtime will read into memory. 10 MiB —
/// orders of magnitude bigger than any realistic operator
/// flow and small enough that an attacker can't pin the
/// process by pointing it at `/dev/zero` (or, on Windows, a
/// huge log file).
pub const MAX_YAML_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// CORR PART 1: hard ceiling on YAML nesting depth.
/// Operator flows realistically nest 3–5 levels (a try wraps
/// a loop wraps a few calls). 20 is well past that; far short
/// of the depth a hostile document would need to weaponise
/// recursive lowering into stack exhaustion.
pub const MAX_YAML_NESTING_DEPTH: usize = 20;

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
/// [`MarkedYamlOwned`] so it can carry a native YAML sequence
/// (for `type: list`), a native YAML mapping (for `type: map`),
/// or a scalar (for the four scalar types). The lowerer
/// validates the value shape against the declared type and
/// recursively stringifies nested structures into SOL literal
/// syntax. Spans on the marked node feed the line / column of
/// any semantic error raised during lowering.
#[derive(Debug)]
pub struct LetStep {
    pub name: String,
    pub var_type: String,
    pub value: Node,
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
/// does too. The `catch:` field accepts either a single
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
/// the equivalent `.sol` file. Schema errors carry the real
/// (line, column) of the offending YAML node — saphyr's
/// annotated parser feeds the position straight through every
/// helper.
pub fn compile_source(yaml_source: &str) -> Result<Vec<Inst>, YamlFlowError> {
    let docs = Node::load_from_str(yaml_source).map_err(parse_error_from_scan)?;
    let root = match docs.first() {
        Some(n) => n.clone(),
        None => Node::from(YamlDataOwned::BadValue),
    };
    // CORR PART 1: reject pathologically nested documents
    // BEFORE the lowering pass walks them. The check is a
    // bounded recursion of its own that bails out the moment
    // depth exceeds the limit (so a hostile document doesn't
    // pay full O(N) traversal cost), and feeds the path of
    // the offending node into the error so operators can fix
    // it.
    check_depth_node(&root, 1, MAX_YAML_NESTING_DEPTH, "$")?;
    let flow = parse_flow(&root)?;
    let mut last_step_context = "<before any step>".to_string();
    let lowered = lower_to_sol_inner(&flow, &mut last_step_context)?;
    crate::sol::compile_source(&lowered).map_err(|e| YamlFlowError::Lower {
        sol_error: e,
        step_context: last_step_context,
        lowered_source: lowered,
    })
}

/// CORR PART 1: depth-bounded walk of a saphyr node. Returns
/// `Err(NestingTooDeep)` the instant `depth` would exceed
/// `max_depth`; this avoids running through the full tree of
/// an adversarial document.
fn check_depth_node(
    n: &Node,
    depth: usize,
    max_depth: usize,
    path: &str,
) -> Result<(), YamlFlowError> {
    if depth > max_depth {
        return Err(YamlFlowError::NestingTooDeep {
            path: path.to_string(),
            depth,
            max_depth,
        });
    }
    match &n.data {
        YamlDataOwned::Mapping(m) => {
            for (k, v) in m {
                let key_str = match &k.data {
                    YamlDataOwned::Value(ScalarOwned::String(s)) => s.clone(),
                    _ => "<non-string-key>".to_string(),
                };
                let child_path = if path == "$" {
                    format!("$.{key_str}")
                } else {
                    format!("{path}.{key_str}")
                };
                check_depth_node(v, depth + 1, max_depth, &child_path)?;
            }
        }
        YamlDataOwned::Sequence(seq) => {
            for (i, v) in seq.iter().enumerate() {
                let child_path = format!("{path}[{i}]");
                check_depth_node(v, depth + 1, max_depth, &child_path)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Async wrapper around [`compile_path`] that moves the
/// blocking file read onto the tokio blocking pool so async
/// callers (the flow runner, the bridge) never stall a runtime
/// worker on disk I/O.
pub async fn compile_path_async(path: std::path::PathBuf) -> Result<Vec<Inst>, YamlFlowError> {
    let path_for_err = path.clone();
    tokio::task::spawn_blocking(move || compile_path(&path))
        .await
        .map_err(|e| YamlFlowError::Io {
            path: path_for_err.display().to_string(),
            cause: format!("spawn_blocking join: {e}"),
        })?
}

/// Compile a YAML flow file at the given path. Convenience
/// wrapper around [`compile_source`].
pub fn compile_path(path: &Path) -> Result<Vec<Inst>, YamlFlowError> {
    // CORR PART 1: size-cap BEFORE reading into memory. We
    // stat the file first; a metadata failure falls through to
    // a normal Io error (same surface as before). A file that
    // exceeds the cap is rejected loudly with the exact size
    // so the operator knows what they pointed us at.
    let meta = std::fs::metadata(path).map_err(|e| YamlFlowError::Io {
        path: path.display().to_string(),
        cause: e.to_string(),
    })?;
    let size = meta.len();
    if size > MAX_YAML_FILE_BYTES {
        return Err(YamlFlowError::FileTooLarge {
            path: path.display().to_string(),
            size_bytes: size,
            max_bytes: MAX_YAML_FILE_BYTES,
        });
    }
    let source = std::fs::read_to_string(path).map_err(|e| YamlFlowError::Io {
        path: path.display().to_string(),
        cause: e.to_string(),
    })?;
    // Defence-in-depth: even if the on-disk size was within
    // the cap, the materialised UTF-8 string may have grown
    // (BOM stripping etc.). Re-check.
    if source.len() as u64 > MAX_YAML_FILE_BYTES {
        return Err(YamlFlowError::FileTooLarge {
            path: path.display().to_string(),
            size_bytes: source.len() as u64,
            max_bytes: MAX_YAML_FILE_BYTES,
        });
    }
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
    let mut last = String::from("<unused>");
    lower_to_sol_inner(flow, &mut last)
}

fn lower_to_sol_inner(
    flow: &YamlFlow,
    last_step_context: &mut String,
) -> Result<String, YamlFlowError> {
    let root_path = StepPath::root();
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
        let step_path = root_path.child(i);
        *last_step_context = step_path.render();
        ctx.lower_step(step, &step_path)?;
    }
    if !ctx.has_explicit_result {
        ctx.indented("return \"\";\n");
    }
    ctx.indent -= 1;
    ctx.emit("}\n");
    Ok(ctx.out)
}

// ──────────────────────────── YAML → typed AST ──────────────────

/// Parse a saphyr-loaded root node into a typed [`YamlFlow`].
/// Walks the value tree explicitly so each schema error can
/// carry both a step-path locator AND the real line/column of
/// the offending node.
pub fn parse_flow(root: &Node) -> Result<YamlFlow, YamlFlowError> {
    let root_path = StepPath::root();
    expect_mapping(root, &root_path, "root")?;
    let root_map = root.data.as_mapping().expect("validated as mapping");
    // Top-level keys: only `steps` is recognised today. An
    // unknown top-level key is treated as a clear schema
    // error so a typo doesn't silently skip a section.
    for (key_node, _) in root_map.iter() {
        let name = scalar_as_string(key_node).ok_or_else(|| {
            let (line, column) = node_pos(key_node);
            YamlFlowError::Semantic {
                line,
                column,
                path: "<root>".to_string(),
                message: "top-level keys must be strings".to_string(),
            }
        })?;
        if name != "steps" {
            let (line, column) = node_pos(key_node);
            return Err(YamlFlowError::Semantic {
                line,
                column,
                path: "<root>".to_string(),
                message: format!("unknown top-level key `{name}` — only `steps` is supported"),
            });
        }
    }
    let steps_node = root.data.as_mapping_get("steps");
    let steps = match steps_node {
        Some(n) => match &n.data {
            YamlDataOwned::Sequence(seq) => parse_step_list(seq, &root_path)?,
            YamlDataOwned::Value(ScalarOwned::Null) => Vec::new(),
            _ => {
                let (line, column) = node_pos(n);
                return Err(YamlFlowError::Semantic {
                    line,
                    column,
                    path: "<root>".to_string(),
                    message: "`steps` must be a sequence".to_string(),
                });
            }
        },
        None => Vec::new(),
    };
    Ok(YamlFlow { steps })
}

fn parse_step_list(seq: &[Node], parent: &StepPath) -> Result<Vec<YamlStep>, YamlFlowError> {
    seq.iter()
        .enumerate()
        .map(|(i, v)| parse_step(v, &parent.child(i)))
        .collect()
}

fn parse_step(value: &Node, path: &StepPath) -> Result<YamlStep, YamlFlowError> {
    expect_mapping(value, path, "step")?;
    let len = mapping_len(value);
    if len != 1 {
        let (line, column) = node_pos(value);
        return Err(YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: format!(
                "each step must be a single-key map (one of: let, call, stream, result, print, if, loop, try); got {len} keys"
            ),
        });
    }
    let map = value.data.as_mapping().expect("validated as mapping");
    let (k, body) = map.iter().next().expect("len == 1");
    let tag = scalar_as_string(k).ok_or_else(|| {
        let (line, column) = node_pos(k);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: "step tag must be a string".to_string(),
        }
    })?;
    match tag.as_str() {
        "let" => Ok(YamlStep::Let(parse_let(body, path)?)),
        "call" => Ok(YamlStep::Call(parse_call(body, path)?)),
        "stream" => Ok(YamlStep::Stream(parse_call(body, path)?)),
        "result" => Ok(YamlStep::Result(expect_string_value(body, path, "result")?)),
        "print" => Ok(YamlStep::Print(expect_string_value(body, path, "print")?)),
        "if" => Ok(YamlStep::If(parse_if(body, path)?)),
        "loop" => Ok(YamlStep::Loop(parse_loop(body, path)?)),
        "try" => Ok(YamlStep::Try(parse_try(body, path)?)),
        other => {
            // Point at the step's tag (key) node, which gives
            // the line of the bad step name.
            let (line, column) = node_pos(k);
            Err(YamlFlowError::Semantic {
                line,
                column,
                path: path.render(),
                message: format!(
                    "unknown step type `{other}` — expected one of: let, call, stream, result, print, if, loop, try"
                ),
            })
        }
    }
}

fn parse_let(value: &Node, path: &StepPath) -> Result<LetStep, YamlFlowError> {
    expect_mapping(value, path, "let body")?;
    deny_unknown_fields(value, path, "let", &["name", "type", "value"])?;
    let value_node = value.data.as_mapping_get("value").cloned().ok_or_else(|| {
        let (line, column) = node_pos(value);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: "missing required field `value`".to_string(),
        }
    })?;
    Ok(LetStep {
        name: required_string(value, "name", path)?,
        var_type: required_string(value, "type", path)?,
        value: value_node,
    })
}

fn parse_call(value: &Node, path: &StepPath) -> Result<CallStep, YamlFlowError> {
    expect_mapping(value, path, "call/stream body")?;
    deny_unknown_fields(
        value,
        path,
        "call/stream",
        &["peer", "method", "arg", "assign"],
    )?;
    Ok(CallStep {
        peer: required_string(value, "peer", path)?,
        method: required_string(value, "method", path)?,
        arg: required_string(value, "arg", path)?,
        assign: optional_string(value, "assign", path)?,
    })
}

fn parse_if(value: &Node, path: &StepPath) -> Result<IfStep, YamlFlowError> {
    expect_mapping(value, path, "if body")?;
    deny_unknown_fields(value, path, "if", &["condition", "then", "else"])?;
    let condition = required_string(value, "condition", path)?;
    let then_node = required_node(value, "then", path)?;
    let then_seq = expect_sequence(then_node, &path.named("then"), "then")?;
    let then = parse_step_list(then_seq, &path.named("then"))?;
    let r#else = match value.data.as_mapping_get("else") {
        Some(n) => match &n.data {
            YamlDataOwned::Sequence(seq) => parse_step_list(seq, &path.named("else"))?,
            YamlDataOwned::Value(ScalarOwned::Null) => Vec::new(),
            _ => {
                let (line, column) = node_pos(n);
                return Err(YamlFlowError::Semantic {
                    line,
                    column,
                    path: path.render(),
                    message: "if.else must be a sequence of steps".to_string(),
                });
            }
        },
        None => Vec::new(),
    };
    Ok(IfStep {
        condition,
        then,
        r#else,
    })
}

fn parse_loop(value: &Node, path: &StepPath) -> Result<LoopStep, YamlFlowError> {
    expect_mapping(value, path, "loop body")?;
    deny_unknown_fields(value, path, "loop", &["times", "for_each", "in", "steps"])?;
    let times = match value.data.as_mapping_get("times") {
        Some(n) => match &n.data {
            YamlDataOwned::Value(ScalarOwned::Integer(v)) => {
                if *v < 0 || *v > u32::MAX as i64 {
                    let (line, column) = node_pos(n);
                    return Err(YamlFlowError::Semantic {
                        line,
                        column,
                        path: path.render(),
                        message: format!("loop.times must fit in u32 (got `{v}`)"),
                    });
                }
                Some(*v as u32)
            }
            YamlDataOwned::Value(ScalarOwned::String(s)) => match s.parse::<u32>() {
                Ok(v) => Some(v),
                Err(_) => {
                    let (line, column) = node_pos(n);
                    return Err(YamlFlowError::Semantic {
                        line,
                        column,
                        path: path.render(),
                        message: format!("loop.times must be a non-negative integer (got `{s}`)"),
                    });
                }
            },
            YamlDataOwned::Value(ScalarOwned::Null) => None,
            _ => {
                let (line, column) = node_pos(n);
                return Err(YamlFlowError::Semantic {
                    line,
                    column,
                    path: path.render(),
                    message: "loop.times must be an integer".to_string(),
                });
            }
        },
        None => None,
    };
    let for_each = optional_string(value, "for_each", path)?;
    let in_list = optional_string(value, "in", path)?;
    let steps_node = required_node(value, "steps", path)?;
    let steps_seq = expect_sequence(steps_node, &path.named("loop"), "loop.steps")?;
    let steps = parse_step_list(steps_seq, &path.named("loop"))?;
    Ok(LoopStep {
        times,
        for_each,
        in_list,
        steps,
    })
}

fn parse_try(value: &Node, path: &StepPath) -> Result<TryStep, YamlFlowError> {
    expect_mapping(value, path, "try body")?;
    deny_unknown_fields(value, path, "try", &["steps", "catch"])?;
    let steps_node = required_node(value, "steps", path)?;
    let steps_seq = expect_sequence(steps_node, &path.named("try"), "try.steps")?;
    let steps = parse_step_list(steps_seq, &path.named("try"))?;
    let catch_value = required_node(value, "catch", path).map_err(|_| {
        let (line, column) = node_pos(value);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: "try step missing required field `catch`".to_string(),
        }
    })?;
    // `catch` accepts either a single mapping (single-catch
    // shorthand) or a sequence of mappings (multi-catch).
    let catches = match &catch_value.data {
        YamlDataOwned::Mapping(_) => vec![parse_catch_clause(catch_value, path, 0, true)?],
        YamlDataOwned::Sequence(seq) => {
            if seq.is_empty() {
                let (line, column) = node_pos(catch_value);
                return Err(YamlFlowError::Semantic {
                    line,
                    column,
                    path: path.render(),
                    message: "try.catch sequence must contain at least one clause".to_string(),
                });
            }
            seq.iter()
                .enumerate()
                .map(|(i, v)| parse_catch_clause(v, path, i, false))
                .collect::<Result<Vec<_>, _>>()?
        }
        _ => {
            let (line, column) = node_pos(catch_value);
            return Err(YamlFlowError::Semantic {
                line,
                column,
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
    value: &Node,
    parent: &StepPath,
    clause_index: usize,
    is_single_shorthand: bool,
) -> Result<CatchStep, YamlFlowError> {
    let label = if is_single_shorthand {
        parent.named("catch")
    } else {
        parent.named(&format!("catch[{}]", clause_index))
    };
    expect_mapping(value, &label, "catch clause")?;
    deny_unknown_fields(value, &label, "catch", &["kind", "steps"])?;
    let kind = required_string(value, "kind", &label)?;
    let steps_node = required_node(value, "steps", &label)?;
    let steps_seq = expect_sequence(steps_node, &label, "catch.steps")?;
    let steps = parse_step_list(steps_seq, &label)?;
    Ok(CatchStep { kind, steps })
}

// ──────────────────────────── parsing helpers ──────────────────

/// Validate that `value` is a mapping. Returns `Ok(())` on
/// success; callers re-extract the mapping via
/// `value.data.as_mapping()` when they need to iterate.
/// (Naming the mapping's concrete `LinkedHashMap<Node, Node>`
/// type from outside saphyr requires a transitive hashlink
/// dep — we keep it out by not naming the type.)
fn expect_mapping(value: &Node, path: &StepPath, what: &str) -> Result<(), YamlFlowError> {
    match &value.data {
        YamlDataOwned::Mapping(_) => Ok(()),
        YamlDataOwned::Value(ScalarOwned::Null) => {
            let (line, column) = node_pos(value);
            Err(YamlFlowError::Semantic {
                line,
                column,
                path: path.render(),
                message: format!("{what} is empty — expected a mapping with required fields"),
            })
        }
        _ => {
            let (line, column) = node_pos(value);
            Err(YamlFlowError::Semantic {
                line,
                column,
                path: path.render(),
                message: format!("{what} must be a mapping"),
            })
        }
    }
}

/// Mapping length — convenience around `data.as_mapping()`.
fn mapping_len(value: &Node) -> usize {
    value.data.as_mapping().map_or(0, |m| m.len())
}

fn expect_sequence<'v>(
    value: &'v Node,
    path: &StepPath,
    what: &str,
) -> Result<&'v Vec<Node>, YamlFlowError> {
    match &value.data {
        YamlDataOwned::Sequence(seq) => Ok(seq),
        _ => {
            let (line, column) = node_pos(value);
            Err(YamlFlowError::Semantic {
                line,
                column,
                path: path.render(),
                message: format!("{what} must be a sequence"),
            })
        }
    }
}

fn expect_string_value(value: &Node, path: &StepPath, what: &str) -> Result<String, YamlFlowError> {
    scalar_as_string(value).ok_or_else(|| {
        let (line, column) = node_pos(value);
        let kind = describe_node_kind(value);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: format!("{what} value must be a scalar (string / number / bool); got {kind}"),
        }
    })
}

fn required_node<'v>(
    parent: &'v Node,
    key: &str,
    path: &StepPath,
) -> Result<&'v Node, YamlFlowError> {
    parent.data.as_mapping_get(key).ok_or_else(|| {
        let (line, column) = node_pos(parent);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: format!("missing required field `{key}`"),
        }
    })
}

fn required_string(parent: &Node, key: &str, path: &StepPath) -> Result<String, YamlFlowError> {
    let n = required_node(parent, key, path)?;
    scalar_as_string(n).ok_or_else(|| {
        let (line, column) = node_pos(n);
        YamlFlowError::Semantic {
            line,
            column,
            path: path.render(),
            message: format!("field `{key}` must be a scalar string"),
        }
    })
}

fn optional_string(
    parent: &Node,
    key: &str,
    path: &StepPath,
) -> Result<Option<String>, YamlFlowError> {
    match parent.data.as_mapping_get(key) {
        Some(n) => match &n.data {
            YamlDataOwned::Value(ScalarOwned::Null) => Ok(None),
            _ => scalar_as_string(n).map(Some).ok_or_else(|| {
                let (line, column) = node_pos(n);
                YamlFlowError::Semantic {
                    line,
                    column,
                    path: path.render(),
                    message: format!("field `{key}` must be a scalar string"),
                }
            }),
        },
        None => Ok(None),
    }
}

fn deny_unknown_fields(
    parent: &Node,
    path: &StepPath,
    step: &str,
    allowed: &[&str],
) -> Result<(), YamlFlowError> {
    let map = match parent.data.as_mapping() {
        Some(m) => m,
        None => return Ok(()),
    };
    for (k, _) in map.iter() {
        let name = match scalar_as_string(k) {
            Some(s) => s,
            None => {
                let (line, column) = node_pos(k);
                return Err(YamlFlowError::Semantic {
                    line,
                    column,
                    path: path.render(),
                    message: format!("`{step}` field names must be strings"),
                });
            }
        };
        if !allowed.contains(&name.as_str()) {
            let (line, column) = node_pos(k);
            return Err(YamlFlowError::Semantic {
                line,
                column,
                path: path.render(),
                message: format!(
                    "unknown `{step}` field `{name}` (allowed: {})",
                    allowed.join(", ")
                ),
            });
        }
    }
    Ok(())
}

/// Coerce a scalar node into its string representation.
/// Strings stay as-is; numbers / bools / null stringify to
/// their YAML text. Non-scalar nodes return `None`.
fn scalar_as_string(node: &Node) -> Option<String> {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::String(s)) => Some(s.clone()),
        YamlDataOwned::Value(ScalarOwned::Integer(i)) => Some(i.to_string()),
        YamlDataOwned::Value(ScalarOwned::Boolean(b)) => Some(b.to_string()),
        YamlDataOwned::Value(ScalarOwned::FloatingPoint(f)) => Some(f.0.to_string()),
        YamlDataOwned::Value(ScalarOwned::Null) => Some(String::new()),
        YamlDataOwned::Representation(s, _, _) => Some(s.clone()),
        _ => None,
    }
}

fn describe_node_kind(node: &Node) -> &'static str {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::String(_)) => "string",
        YamlDataOwned::Value(ScalarOwned::Integer(_)) => "integer",
        YamlDataOwned::Value(ScalarOwned::Boolean(_)) => "bool",
        YamlDataOwned::Value(ScalarOwned::FloatingPoint(_)) => "float",
        YamlDataOwned::Value(ScalarOwned::Null) => "null",
        YamlDataOwned::Sequence(_) => "sequence",
        YamlDataOwned::Mapping(_) => "mapping",
        YamlDataOwned::Tagged(_, _) => "tagged",
        YamlDataOwned::Alias(_) => "alias",
        YamlDataOwned::Representation(_, _, _) => "scalar",
        YamlDataOwned::BadValue => "bad-value",
    }
}

// ──────────────────────────── lowerer ───────────────────────────

/// Path through the YAML tree, used for step-located error
/// messages. `step 2 → then.step 1 → catch.step 3` etc. Line
/// and column on the error come from the offending node's
/// saphyr span, not from the path — see [`YamlFlowError::Semantic`].
#[derive(Clone, Debug)]
struct StepPath {
    segments: Vec<String>,
}

impl StepPath {
    fn root() -> Self {
        Self {
            segments: Vec::new(),
        }
    }
    fn child(&self, idx: usize) -> Self {
        let mut s = self.segments.clone();
        s.push(format!("step {}", idx + 1));
        Self { segments: s }
    }
    fn named(&self, name: &str) -> Self {
        let mut s = self.segments.clone();
        s.push(name.to_string());
        Self { segments: s }
    }
    fn render(&self) -> String {
        if self.segments.is_empty() {
            "<root>".to_string()
        } else {
            self.segments.join(" → ")
        }
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
                let lit = sol_string_literal(value, path, None)?;
                self.indented(&format!("return {lit};\n"));
                self.has_explicit_result = true;
                Ok(())
            }
            YamlStep::Print(value) => {
                let lit = sol_string_literal(value, path, None)?;
                self.indented(&format!("print({lit});\n"));
                Ok(())
            }
            YamlStep::If(s) => self.lower_if(s, path),
            YamlStep::Loop(s) => self.lower_loop(s, path),
            YamlStep::Try(s) => self.lower_try(s, path),
        }
    }

    fn lower_let(&mut self, s: &LetStep, path: &StepPath) -> Result<(), YamlFlowError> {
        validate_ident(&s.name, "let.name", path, None)?;
        let ty = validate_let_type(&s.var_type, path, Some(&s.value))?;
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
        let peer = sol_string_literal(&s.peer, path, None)?;
        let method = sol_string_literal(&s.method, path, None)?;
        let arg = sol_string_literal(&s.arg, path, None)?;
        let invocation = format!("{builtin}({peer}, {method}, {arg})");

        if let Some(assign) = s.assign.as_deref() {
            validate_ident(assign, "call.assign", path, None)?;
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
        // SEC PART 3: validate the user-supplied condition
        // against the strict SOL-predicate allowlist BEFORE
        // splicing it into the lowered SOL source. Without
        // this check, an attacker who controlled the YAML
        // could smuggle arbitrary statements through
        // `if: { condition: "...; remote_call(...)" }`.
        validate_condition(&s.condition, path)?;
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
        match (
            s.times.as_ref(),
            s.for_each.as_deref(),
            s.in_list.as_deref(),
        ) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(semantic_at_path(
                path,
                "loop step must set EITHER `times` (counted) OR `for_each` + `in` (collection), not both",
            )),
            (Some(n), None, None) => self.lower_counted_loop(*n, &s.steps, path),
            (None, Some(name), Some(list_var)) => {
                self.lower_for_each_loop(name, list_var, &s.steps, path)
            }
            (None, Some(_), None) => Err(semantic_at_path(
                path,
                "loop step has `for_each` but no `in` — set `in: <list_var>` to name the list",
            )),
            (None, None, Some(_)) => Err(semantic_at_path(
                path,
                "loop step has `in` but no `for_each` — set `for_each: <name>` for the loop variable",
            )),
            (None, None, None) => Err(semantic_at_path(
                path,
                "loop step must set EITHER `times: <N>` (counted) OR `for_each: <name>` + `in: <list_var>` (collection)",
            )),
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
        validate_ident(name, "loop.for_each", path, None)?;
        validate_ident(list_var, "loop.in", path, None)?;
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
            return Err(semantic_at_path(path, "try step has no catch clauses"));
        }
        for c in &s.catches {
            validate_catch_kind(&c.kind, path, None)?;
        }
        self.indented("try {\n");
        self.indent += 1;
        for (i, step) in s.steps.iter().enumerate() {
            self.lower_step(step, &path.named("try").child(i))?;
        }
        self.indent -= 1;
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

// ──────────────────────────── hoisting ──────────────────────────

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
                let ty = validate_let_type(&s.var_type, &sp, Some(&s.value))?;
                record_decl(s.name.clone(), ty, &sp, decls, seen, Some(&s.value))?;
            }
            YamlStep::Call(s) | YamlStep::Stream(s) => {
                if let Some(name) = s.assign.as_ref() {
                    record_decl(name.clone(), LetType::Str, &sp, decls, seen, None)?;
                }
            }
            YamlStep::If(s) => {
                collect_steps(&s.then, &sp.named("then"), decls, seen)?;
                collect_steps(&s.r#else, &sp.named("else"), decls, seen)?;
            }
            YamlStep::Loop(s) => {
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
    locate: Option<&Node>,
) -> Result<(), YamlFlowError> {
    validate_ident(&name, "variable name", path, locate)?;
    if let Some(existing) = seen.get(&name) {
        if existing.as_sol() != ty.as_sol() {
            return Err(semantic_at_path_or_node(
                path,
                locate,
                format!(
                    "variable `{name}` declared with conflicting types: first `{}`, later `{}`",
                    existing.as_sol(),
                    ty.as_sol()
                ),
            ));
        }
        return Ok(());
    }
    seen.insert(name.clone(), ty.clone());
    decls.push((name, ty));
    Ok(())
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

fn validate_let_type(
    ty: &str,
    path: &StepPath,
    locate: Option<&Node>,
) -> Result<LetType, YamlFlowError> {
    match ty {
        "int" => Ok(LetType::Int),
        "str" => Ok(LetType::Str),
        "bool" => Ok(LetType::Bool),
        "float" => Ok(LetType::Float),
        "list" => Ok(LetType::List),
        "map" => Ok(LetType::Map),
        other => Err(semantic_at_path_or_node(
            path,
            locate,
            format!(
                "let.type `{other}` is not supported — expected one of: int, str, bool, float, list, map"
            ),
        )),
    }
}

fn validate_ident(
    name: &str,
    what: &str,
    path: &StepPath,
    locate: Option<&Node>,
) -> Result<(), YamlFlowError> {
    if name.is_empty() {
        return Err(semantic_at_path_or_node(
            path,
            locate,
            format!("{what} is empty"),
        ));
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' {
        return Err(semantic_at_path_or_node(
            path,
            locate,
            format!(
                "{what} `{name}` is not a valid SOL identifier (must start with letter or underscore)"
            ),
        ));
    }
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' {
            return Err(semantic_at_path_or_node(
                path,
                locate,
                format!(
                    "{what} `{name}` contains invalid character `{c}` (only letters, digits, underscore allowed)"
                ),
            ));
        }
    }
    Ok(())
}

fn validate_catch_kind(
    kind: &str,
    path: &StepPath,
    locate: Option<&Node>,
) -> Result<(), YamlFlowError> {
    match kind {
        "any" | "timeout" | "mesh_error" | "policy_denied" | "responder_error" => Ok(()),
        other => Err(semantic_at_path_or_node(
            path,
            locate,
            format!(
                "catch.kind `{other}` is not a recognised SOL kind — expected one of: any, timeout, mesh_error, policy_denied, responder_error"
            ),
        )),
    }
}

fn lower_let_value(ty: &LetType, value: &Node, path: &StepPath) -> Result<String, YamlFlowError> {
    match ty {
        LetType::Str => match &value.data {
            YamlDataOwned::Value(ScalarOwned::String(_))
            | YamlDataOwned::Value(ScalarOwned::Integer(_))
            | YamlDataOwned::Value(ScalarOwned::Boolean(_))
            | YamlDataOwned::Value(ScalarOwned::FloatingPoint(_))
            | YamlDataOwned::Value(ScalarOwned::Null)
            | YamlDataOwned::Representation(_, _, _) => {
                let s = scalar_as_string(value).unwrap_or_default();
                sol_string_literal(&s, path, Some(value))
            }
            YamlDataOwned::Sequence(_) => Err(semantic_at_node(
                path,
                value,
                "let.value is a YAML sequence but let.type is `str` — use `type: list` for sequence values",
            )),
            YamlDataOwned::Mapping(_) => Err(semantic_at_node(
                path,
                value,
                "let.value is a YAML mapping but let.type is `str` — use `type: map` for mapping values",
            )),
            YamlDataOwned::Tagged(_, inner) => lower_let_value(ty, inner, path),
            YamlDataOwned::Alias(_) | YamlDataOwned::BadValue => Err(semantic_at_node(
                path,
                value,
                "let.value is not a recognised YAML scalar",
            )),
        },
        LetType::Int | LetType::Bool | LetType::Float => require_scalar_unquoted(value, ty, path),
        LetType::List => match &value.data {
            YamlDataOwned::Sequence(_) => yaml_to_sol_expr(value, path),
            YamlDataOwned::Value(ScalarOwned::String(s)) => {
                // SEC PART 3: when the user provides a
                // string-typed SOL list literal, validate it
                // against the strict collection-allowlist
                // before splicing into the lowered source.
                validate_collection_literal(s, "list", path)?;
                Ok(s.clone())
            }
            YamlDataOwned::Value(ScalarOwned::Null) => Ok("[]".to_string()),
            YamlDataOwned::Mapping(_) => Err(semantic_at_node(
                path,
                value,
                "let.value is a YAML mapping but let.type is `list` — use `type: map` for mapping values",
            )),
            YamlDataOwned::Value(_) | YamlDataOwned::Representation(_, _, _) => {
                Err(semantic_at_node(
                    path,
                    value,
                    "let.value must be a sequence for type `list` (or a SOL list literal as a string)",
                ))
            }
            YamlDataOwned::Tagged(_, inner) => lower_let_value(ty, inner, path),
            YamlDataOwned::Alias(_) | YamlDataOwned::BadValue => Err(semantic_at_node(
                path,
                value,
                "let.value is not a recognised YAML node",
            )),
        },
        LetType::Map => match &value.data {
            YamlDataOwned::Mapping(_) => yaml_to_sol_expr(value, path),
            YamlDataOwned::Value(ScalarOwned::String(s)) => {
                // SEC PART 3: see LetType::List branch above.
                validate_collection_literal(s, "map", path)?;
                Ok(s.clone())
            }
            YamlDataOwned::Value(ScalarOwned::Null) => Ok("{}".to_string()),
            YamlDataOwned::Sequence(_) => Err(semantic_at_node(
                path,
                value,
                "let.value is a YAML sequence but let.type is `map` — use `type: list` for sequence values",
            )),
            YamlDataOwned::Value(_) | YamlDataOwned::Representation(_, _, _) => {
                Err(semantic_at_node(
                    path,
                    value,
                    "let.value must be a mapping for type `map` (or a SOL map literal as a string)",
                ))
            }
            YamlDataOwned::Tagged(_, inner) => lower_let_value(ty, inner, path),
            YamlDataOwned::Alias(_) | YamlDataOwned::BadValue => Err(semantic_at_node(
                path,
                value,
                "let.value is not a recognised YAML node",
            )),
        },
    }
}

fn require_scalar_unquoted(
    value: &Node,
    ty: &LetType,
    path: &StepPath,
) -> Result<String, YamlFlowError> {
    match &value.data {
        YamlDataOwned::Value(ScalarOwned::String(s)) => {
            // SEC PART 3: validate the user-supplied string
            // matches the SOL grammar for `ty` before
            // splicing it into the lowered source.
            match ty {
                LetType::Int => validate_int_literal(s, path)?,
                LetType::Float => validate_float_literal(s, path)?,
                LetType::Bool => validate_bool_literal(s, path)?,
                _ => {}
            }
            Ok(s.clone())
        }
        YamlDataOwned::Value(ScalarOwned::Integer(i)) => Ok(i.to_string()),
        YamlDataOwned::Value(ScalarOwned::FloatingPoint(f)) => Ok(f.0.to_string()),
        YamlDataOwned::Value(ScalarOwned::Boolean(b)) => Ok(b.to_string()),
        YamlDataOwned::Representation(s, _, _) => {
            // Representation carries the source's literal
            // text — apply the same grammar check the
            // String branch does.
            match ty {
                LetType::Int => validate_int_literal(s, path)?,
                LetType::Float => validate_float_literal(s, path)?,
                LetType::Bool => validate_bool_literal(s, path)?,
                _ => {}
            }
            Ok(s.clone())
        }
        YamlDataOwned::Sequence(_) => Err(semantic_at_node(
            path,
            value,
            format!(
                "let.value is a YAML sequence but let.type is `{}` — use `type: list` for sequence values",
                ty.as_sol()
            ),
        )),
        YamlDataOwned::Mapping(_) => Err(semantic_at_node(
            path,
            value,
            format!(
                "let.value is a YAML mapping but let.type is `{}` — use `type: map` for mapping values",
                ty.as_sol()
            ),
        )),
        YamlDataOwned::Value(ScalarOwned::Null) => Err(semantic_at_node(
            path,
            value,
            format!("let.value for type `{}` cannot be null", ty.as_sol()),
        )),
        YamlDataOwned::Tagged(_, inner) => require_scalar_unquoted(inner, ty, path),
        YamlDataOwned::Alias(_) | YamlDataOwned::BadValue => Err(semantic_at_node(
            path,
            value,
            "let.value is not a recognised YAML scalar",
        )),
    }
}

/// Recursively turn a YAML node into the SOL expression that
/// produces the same logical value. Strings become SOL string
/// literals, numbers / bools / null stay verbatim, sequences
/// become `[a, b, c]` SOL lists, and mappings become
/// `{"k": v, ...}` SOL maps. Nested lists and maps are handled
/// by the recursion.
fn yaml_to_sol_expr(value: &Node, path: &StepPath) -> Result<String, YamlFlowError> {
    match &value.data {
        YamlDataOwned::Value(ScalarOwned::String(s)) => sol_string_literal(s, path, Some(value)),
        YamlDataOwned::Value(ScalarOwned::Integer(i)) => Ok(i.to_string()),
        YamlDataOwned::Value(ScalarOwned::Boolean(b)) => Ok(b.to_string()),
        YamlDataOwned::Value(ScalarOwned::FloatingPoint(f)) => Ok(f.0.to_string()),
        YamlDataOwned::Value(ScalarOwned::Null) => sol_string_literal("", path, Some(value)),
        YamlDataOwned::Representation(s, _, _) => sol_string_literal(s, path, Some(value)),
        YamlDataOwned::Sequence(seq) => {
            let mut parts = Vec::with_capacity(seq.len());
            for v in seq {
                parts.push(yaml_to_sol_expr(v, path)?);
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        YamlDataOwned::Mapping(m) => {
            let mut parts = Vec::with_capacity(m.len());
            for (k, v) in m {
                let key_str = scalar_as_string(k).ok_or_else(|| {
                    semantic_at_node(
                        path,
                        k,
                        "map keys must be scalar strings (SOL map literals only accept string-literal keys)",
                    )
                })?;
                let key_lit = sol_string_literal(&key_str, path, Some(k))?;
                let val_expr = yaml_to_sol_expr(v, path)?;
                parts.push(format!("{key_lit}: {val_expr}"));
            }
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        YamlDataOwned::Tagged(_, inner) => yaml_to_sol_expr(inner, path),
        YamlDataOwned::Alias(_) | YamlDataOwned::BadValue => Err(semantic_at_node(
            path,
            value,
            "encountered a node the YAML frontend cannot lower",
        )),
    }
}

/// SEC PART 3: strict allowlist regexes for every
/// user-supplied SOL fragment that the lowerer would
/// otherwise interpolate verbatim. Compiled once via
/// `OnceLock` so the hot path stays allocation-free.
fn condition_allowlist() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| {
        // Rust `regex` crate forbids unnecessary `\` escapes
        // inside char classes; the prompt's `^[A-Za-z0-9_\.\s\(\)\!\=\<\>\&\|]+$`
        // is equivalent to this strictly-formed pattern.
        regex::Regex::new(r"^[A-Za-z0-9_.\s()!=<>&|]+$")
            .expect("condition allowlist regex is constant + tested at boot")
    })
}

fn int_allowlist() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| regex::Regex::new(r"^-?[0-9]+$").expect("int allowlist regex"))
}

fn float_allowlist() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| regex::Regex::new(r"^-?[0-9]+(\.[0-9]+)?$").expect("float allowlist regex"))
}

/// `^[\[\]\{\}\:\,\s\"A-Za-z0-9_\.\-]+$` — the chars that show
/// up in legitimate SOL list/map literals (brackets, braces,
/// colon, comma, quoted strings, scalars). Excludes `;`,
/// newlines, function-call parens, and operators that would
/// let a string-typed `let.value` smuggle statements into the
/// surrounding SOL source.
fn collection_allowlist() -> &'static regex::Regex {
    static RX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RX.get_or_init(|| {
        regex::Regex::new(r#"^[\[\]{}:,\s"A-Za-z0-9_.\-]+$"#).expect("collection allowlist regex")
    })
}

/// SEC PART 3: validate a user-supplied SOL boolean predicate
/// before interpolating it into the lowered source. Reject
/// anything outside the allowlist with
/// [`YamlFlowError::InvalidCondition`] so the SOL injection
/// path is closed even when the operator hand-edits the YAML.
fn validate_condition(value: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !condition_allowlist().is_match(trimmed) {
        return Err(YamlFlowError::InvalidCondition {
            path: path.render(),
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_int_literal(value: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    if !int_allowlist().is_match(value.trim()) {
        return Err(YamlFlowError::InvalidScalar {
            path: path.render(),
            what: "int",
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_bool_literal(value: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    match value.trim() {
        "true" | "false" => Ok(()),
        _ => Err(YamlFlowError::InvalidScalar {
            path: path.render(),
            what: "bool",
            value: value.to_string(),
        }),
    }
}

fn validate_float_literal(value: &str, path: &StepPath) -> Result<(), YamlFlowError> {
    if !float_allowlist().is_match(value.trim()) {
        return Err(YamlFlowError::InvalidScalar {
            path: path.render(),
            what: "float",
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_collection_literal(
    value: &str,
    what: &'static str,
    path: &StepPath,
) -> Result<(), YamlFlowError> {
    if !collection_allowlist().is_match(value) {
        return Err(YamlFlowError::InvalidScalar {
            path: path.render(),
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// SOL strings have no escape sequences (SIMP-016). A literal
/// `"` would prematurely close the SOL string. Emit the value
/// verbatim between two `"` characters, refusing anything that
/// would produce malformed SOL.
fn sol_string_literal(
    value: &str,
    path: &StepPath,
    locate: Option<&Node>,
) -> Result<String, YamlFlowError> {
    if value.contains('"') {
        return Err(semantic_at_path_or_node(
            path,
            locate,
            "string value contains a `\"` character; SOL has no escape sequences (SIMP-016) so quotes inside strings are unsupported",
        ));
    }
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    out.push_str(value);
    out.push('"');
    Ok(out)
}

// ──────────────────────────── error helpers ────────────────────

fn semantic_at_node(path: &StepPath, node: &Node, message: impl Into<String>) -> YamlFlowError {
    let (line, column) = node_pos(node);
    YamlFlowError::Semantic {
        line,
        column,
        path: path.render(),
        message: message.into(),
    }
}

fn semantic_at_path(path: &StepPath, message: impl Into<String>) -> YamlFlowError {
    YamlFlowError::Semantic {
        line: 0,
        column: 0,
        path: path.render(),
        message: message.into(),
    }
}

fn semantic_at_path_or_node(
    path: &StepPath,
    node: Option<&Node>,
    message: impl Into<String>,
) -> YamlFlowError {
    match node {
        Some(n) => semantic_at_node(path, n, message),
        None => semantic_at_path(path, message),
    }
}

fn parse_error_from_scan(e: saphyr::ScanError) -> YamlFlowError {
    let marker = e.marker();
    YamlFlowError::Parse {
        line: marker.line(),
        column: marker.col(),
        message: e.info().to_string(),
    }
}

/// Render the lowering of a small flow as a debug string,
/// useful for tooling that wants to preview the emitted SOL.
/// Returns the same source the underlying SOL compiler would
/// see. Errors short-circuit with a single-line summary.
#[allow(dead_code)]
pub(crate) fn debug_lower(yaml: &str) -> String {
    match Node::load_from_str(yaml) {
        Ok(docs) => match docs.first() {
            Some(root) => match parse_flow(root) {
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
            None => "/* empty document */".to_string(),
        },
        Err(e) => format!("/* yaml error: {e:?} */"),
    }
}

#[cfg(test)]
mod tests;
