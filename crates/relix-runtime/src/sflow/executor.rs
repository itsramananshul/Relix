//! AST-walking executor for the .sflow DSL.
//!
//! The executor takes a parsed [`Program`] and a [`RemoteCallDispatcher`]
//! (the same trait the SOL VM uses), plus an optional chronicle writer
//! that the host wires up to a per-flow event log. It walks the AST
//! synchronously — the parent runs it inside `tokio::task::spawn_blocking`
//! just like the SOL VM, so `dispatcher.remote_call` can block on libp2p.
//!
//! Behavioural contract:
//! - Capability `step`s call the dispatcher; on success the result is
//!   captured in the last-result slot AND under any step name.
//! - Step failure cascades into the nearest enclosing `try` block whose
//!   `catch` matches the error kind; otherwise it aborts the flow and
//!   sets [`ExecOutcome::error`] to a structured cause.
//! - Loop iteration is capped at `max_loop_iters`; on overshoot the
//!   executor writes `sol.loop_limit_hit` to the chronicle and breaks.
//! - At most 50 unique variable names per execution. The 51st `set`
//!   aborts with a runtime error (same posture as the cron-store
//!   sanity caps).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;

use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError};

use super::parser::{Atom, Catch, CatchKind, CmpOp, Condition, Expr, Program, Stmt};

/// F5/F7: typed value carried in `vars`. The pre-list/map
/// executor stored everything as `String`; this enum keeps the
/// rich shape so built-ins (`list_*`, `map_*`) can operate on
/// the structured data, while string-context reads (step args,
/// `${...}` interpolation, conditions) stringify the value
/// deterministically.
///
/// Stringification format mirrors the encodings operators
/// typically write by hand when wiring pipe-delimited capability
/// payloads — pipe-separated for lists, `k=v;` for maps. This
/// is documented in `docs/sol-language.md` so flows that
/// interleave structured values and bare-string steps can
/// reason about the wire layout.
#[derive(Clone, Debug)]
pub enum SflowValue {
    String(String),
    List(Vec<String>),
    Map(Vec<(String, String)>),
}

impl SflowValue {
    /// Return the string representation used in step args,
    /// interpolation, conditions, and the chronicle log.
    /// Lists become `a|b|c`; maps become `k1=v1;k2=v2`.
    pub fn to_display(&self) -> String {
        match self {
            SflowValue::String(s) => s.clone(),
            SflowValue::List(items) => items.join("|"),
            SflowValue::Map(pairs) => pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(";"),
        }
    }
}

impl Default for SflowValue {
    fn default() -> Self {
        SflowValue::String(String::new())
    }
}

/// Per-execution variable cap. Matches `crates/relix-runtime/src/sflow/`
/// docstring + `docs/sol.md`. Exceeded → runtime error.
pub const MAX_VARS: usize = 50;
/// Default iteration cap. Overridable via [`Executor::with_max_loop_iters`].
pub const DEFAULT_MAX_LOOP_ITERS: u64 = 100;
/// `sol.sleep <n>` is clamped to this many seconds.
pub const MAX_SLEEP_SECS: u64 = 30;

/// Sink for chronicle events. Implementations write to wherever the host
/// records observability data (per-flow event log, task chronicle, stdout).
pub trait ChronicleSink: Send + Sync {
    fn write(&self, kind: &str, payload: &str);
}

/// A no-op chronicle sink — used by tests and stand-alone executions.
pub struct NullChronicle;
impl ChronicleSink for NullChronicle {
    fn write(&self, _: &str, _: &str) {}
}

/// In-memory chronicle sink that accumulates events for assertions.
/// Lives behind the executor so test code can introspect what was written.
#[derive(Default)]
pub struct VecChronicle {
    inner: std::sync::Mutex<Vec<(String, String)>>,
}
impl VecChronicle {
    pub fn entries(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().clone()
    }
    pub fn kinds(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}
impl ChronicleSink for VecChronicle {
    fn write(&self, kind: &str, payload: &str) {
        self.inner
            .lock()
            .unwrap()
            .push((kind.to_string(), payload.to_string()));
    }
}

/// Per-step bookkeeping. The executor keeps two indexes: the last step's
/// `(status, result)` as the implicit `status` / `result` atoms, plus a
/// by-name map for `step.<name>.status` / `step.<name>.result` lookups.
#[derive(Clone, Debug, Default)]
struct StepRecord {
    status: String,
    result: String,
}

/// Final outcome of executing a program. `error` is populated when the
/// flow halted due to an uncaught error.
#[derive(Clone, Debug, Default)]
pub struct ExecOutcome {
    /// Final flow result: explicit `return`, `sol.set_result`, or the
    /// last step's result if the flow falls off the end.
    pub result: String,
    /// On uncaught error: the structured cause. `None` on success.
    pub error: Option<RuntimeError>,
}

/// Runtime error from the executor. Carries enough for the host to
/// classify the failure on the task ledger.
#[derive(Clone, Debug)]
pub struct RuntimeError {
    /// Catch-kind equivalent for the underlying failure (used by the
    /// host to populate `failure_class`).
    pub kind: CatchKind,
    /// Stable error-kind value (mirrors `relix_core::types::error_kinds`,
    /// or `0` for sflow-local errors).
    pub error_kind: u32,
    /// Human-readable cause. For uncaught errors outside a try block the
    /// host stamps `failure_class = sol_uncaught_error`.
    pub message: String,
    /// 1-indexed source line of the failing statement, when known.
    pub line: usize,
}

impl RuntimeError {
    fn local(line: usize, message: impl Into<String>) -> Self {
        Self {
            kind: CatchKind::Any,
            error_kind: 0,
            message: message.into(),
            line,
        }
    }
}

/// Build an executor. Calls go through the dispatcher; events through the
/// chronicle sink. Both are optional in spirit but must be supplied — the
/// no-op sinks suffice when the host has no log wired.
pub struct Executor {
    dispatcher: Arc<dyn RemoteCallDispatcher>,
    chronicle: Arc<dyn ChronicleSink>,
    max_loop_iters: u64,
}

impl Executor {
    pub fn new(
        dispatcher: Arc<dyn RemoteCallDispatcher>,
        chronicle: Arc<dyn ChronicleSink>,
    ) -> Self {
        Self {
            dispatcher,
            chronicle,
            max_loop_iters: DEFAULT_MAX_LOOP_ITERS,
        }
    }

    pub fn with_max_loop_iters(mut self, cap: u64) -> Self {
        self.max_loop_iters = cap.max(1);
        self
    }

    pub fn run(&self, program: &Program) -> ExecOutcome {
        let mut state = ExecState::default();
        match self.exec_block(&program.stmts, &mut state) {
            Ok(BlockFlow::Continue) | Ok(BlockFlow::Return) => ExecOutcome {
                result: state.flow_result,
                error: None,
            },
            Err(err) => {
                self.chronicle.write(
                    "sol.flow_failed",
                    &format!("kind={} message={}", err.kind.as_str(), err.message),
                );
                ExecOutcome {
                    result: state.flow_result,
                    error: Some(err),
                }
            }
        }
    }

    // ---- Block / statement execution --------------------------------

    fn exec_block(&self, stmts: &[Stmt], state: &mut ExecState) -> Result<BlockFlow, RuntimeError> {
        for stmt in stmts {
            match self.exec_stmt(stmt, state)? {
                BlockFlow::Continue => {}
                BlockFlow::Return => return Ok(BlockFlow::Return),
            }
        }
        Ok(BlockFlow::Continue)
    }

    fn exec_stmt(&self, stmt: &Stmt, state: &mut ExecState) -> Result<BlockFlow, RuntimeError> {
        match stmt {
            Stmt::Step {
                name,
                peer,
                wire_method,
                arg,
                line,
            } => {
                self.exec_step(name.as_deref(), peer, wire_method, arg, *line, state)?;
                Ok(BlockFlow::Continue)
            }
            Stmt::Set { name, value, line } => {
                // F5/F7: use the typed resolver so list / map
                // literals + built-in calls preserve their
                // structured form in the var store. Plain
                // string literals still stringify the same way
                // they always did.
                let resolved = state.resolve_value(value, *line)?;
                state.set_var(name, resolved, *line)?;
                Ok(BlockFlow::Continue)
            }
            Stmt::If {
                branches,
                else_body,
                line,
            } => self.exec_if(branches, else_body.as_deref(), *line, state),
            Stmt::LoopTimes { count, body, line } => {
                self.exec_loop_times(*count, body, *line, state)
            }
            Stmt::While { cond, body, line } => self.exec_while(cond, body, *line, state, false),
            Stmt::Until { cond, body, line } => self.exec_while(cond, body, *line, state, true),
            Stmt::Try {
                body,
                catches,
                line,
            } => self.exec_try(body, catches, *line, state),
            Stmt::Rethrow { line } => {
                let Some(err) = state.current_error.clone() else {
                    return Err(RuntimeError::local(
                        *line,
                        "rethrow outside of a catch block",
                    ));
                };
                Err(err)
            }
            Stmt::Return { value, line } => {
                if let Some(v) = value {
                    state.flow_result = state.resolve(v, *line)?;
                } else if state.flow_result.is_empty() {
                    state.flow_result = state.last_step.result.clone();
                }
                Ok(BlockFlow::Return)
            }
            Stmt::SolLog { message, line } => {
                let m = state.resolve(message, *line)?;
                self.chronicle.write("sol.log", &m);
                Ok(BlockFlow::Continue)
            }
            Stmt::SolSleep { secs, .. } => {
                let s = (*secs).min(MAX_SLEEP_SECS);
                std::thread::sleep(Duration::from_secs(s));
                Ok(BlockFlow::Continue)
            }
            Stmt::SolAssert { cond, line } => {
                if !state.eval(cond, *line)? {
                    return Err(RuntimeError::local(
                        *line,
                        "sol.assert: condition was false",
                    ));
                }
                Ok(BlockFlow::Continue)
            }
            Stmt::SolSetResult { value, line } => {
                let v = state.resolve(value, *line)?;
                state.flow_result = v;
                Ok(BlockFlow::Continue)
            }
        }
    }

    fn exec_step(
        &self,
        name: Option<&str>,
        peer: &str,
        wire_method: &str,
        arg: &Expr,
        line: usize,
        state: &mut ExecState,
    ) -> Result<(), RuntimeError> {
        let interpolated = state.resolve(arg, line)?;
        let label = name.unwrap_or("(unnamed)");
        self.chronicle.write(
            "sol.step_start",
            &format!("step={label} peer={peer} method={wire_method} line={line}"),
        );
        let res = self
            .dispatcher
            .remote_call(peer, wire_method, interpolated.as_bytes());
        match res {
            Ok(bytes) => {
                let body = String::from_utf8(bytes).unwrap_or_else(|e| {
                    format!("<binary response: {} bytes; {}>", e.as_bytes().len(), e)
                });
                state.last_step = StepRecord {
                    status: "completed".into(),
                    result: body.clone(),
                };
                if let Some(n) = name {
                    state.named_steps.insert(
                        n.to_string(),
                        StepRecord {
                            status: "completed".into(),
                            result: body.clone(),
                        },
                    );
                }
                self.chronicle.write(
                    "sol.step_done",
                    &format!(
                        "step={label} peer={peer} method={wire_method} status=completed bytes={}",
                        body.len()
                    ),
                );
                Ok(())
            }
            Err(err) => {
                let kind = classify_remote_error(&err);
                let runtime_err = RuntimeError {
                    kind,
                    error_kind: err.kind,
                    message: err.cause.clone(),
                    line,
                };
                state.last_step = StepRecord {
                    status: "failed".into(),
                    result: err.cause.clone(),
                };
                if let Some(n) = name {
                    state.named_steps.insert(
                        n.to_string(),
                        StepRecord {
                            status: "failed".into(),
                            result: err.cause.clone(),
                        },
                    );
                }
                self.chronicle.write(
                    "sol.step_done",
                    &format!(
                        "step={label} peer={peer} method={wire_method} status=failed kind={} cause={}",
                        kind.as_str(),
                        err.cause
                    ),
                );
                Err(runtime_err)
            }
        }
    }

    fn exec_if(
        &self,
        branches: &[(Condition, Vec<Stmt>)],
        else_body: Option<&[Stmt]>,
        line: usize,
        state: &mut ExecState,
    ) -> Result<BlockFlow, RuntimeError> {
        for (i, (cond, body)) in branches.iter().enumerate() {
            if state.eval(cond, line)? {
                let label = if i == 0 { "if" } else { "elif" };
                self.chronicle
                    .write("sol.condition_branch", &format!("taken={label} index={i}"));
                return self.exec_block(body, state);
            }
        }
        if let Some(body) = else_body {
            self.chronicle.write("sol.condition_branch", "taken=else");
            return self.exec_block(body, state);
        }
        self.chronicle.write("sol.condition_branch", "taken=none");
        Ok(BlockFlow::Continue)
    }

    fn exec_loop_times(
        &self,
        count: u64,
        body: &[Stmt],
        line: usize,
        state: &mut ExecState,
    ) -> Result<BlockFlow, RuntimeError> {
        let mut iter = 0u64;
        let cap = self.max_loop_iters;
        let target = count.min(cap);
        if count > cap {
            self.chronicle.write(
                "sol.loop_limit_hit",
                &format!("requested={count} cap={cap} line={line}"),
            );
        }
        while iter < target {
            let prev = state.loop_iter.replace(iter);
            self.chronicle.write(
                "sol.loop_iter",
                &format!("iter={iter} kind=times line={line}"),
            );
            let res = self.exec_block(body, state);
            state.loop_iter = prev;
            match res? {
                BlockFlow::Return => return Ok(BlockFlow::Return),
                BlockFlow::Continue => {}
            }
            iter += 1;
        }
        Ok(BlockFlow::Continue)
    }

    fn exec_while(
        &self,
        cond: &Condition,
        body: &[Stmt],
        line: usize,
        state: &mut ExecState,
        invert: bool,
    ) -> Result<BlockFlow, RuntimeError> {
        let mut iter = 0u64;
        let cap = self.max_loop_iters;
        loop {
            if iter >= cap {
                self.chronicle.write(
                    "sol.loop_limit_hit",
                    &format!(
                        "cap={cap} kind={} line={line}",
                        if invert { "until" } else { "while" }
                    ),
                );
                break;
            }
            let mut truth = state.eval(cond, line)?;
            if invert {
                truth = !truth;
            }
            if !truth {
                break;
            }
            let prev = state.loop_iter.replace(iter);
            self.chronicle.write(
                "sol.loop_iter",
                &format!(
                    "iter={iter} kind={} line={line}",
                    if invert { "until" } else { "while" }
                ),
            );
            let res = self.exec_block(body, state);
            state.loop_iter = prev;
            match res? {
                BlockFlow::Return => return Ok(BlockFlow::Return),
                BlockFlow::Continue => {}
            }
            iter += 1;
        }
        Ok(BlockFlow::Continue)
    }

    fn exec_try(
        &self,
        body: &[Stmt],
        catches: &[Catch],
        line: usize,
        state: &mut ExecState,
    ) -> Result<BlockFlow, RuntimeError> {
        match self.exec_block(body, state) {
            Ok(flow) => Ok(flow),
            Err(err) => {
                let Some(matching) = pick_catch(catches, err.kind) else {
                    // No matching handler — propagate.
                    return Err(err);
                };
                self.chronicle.write(
                    "sol.error_caught",
                    &format!(
                        "kind={} catch={} line={} cause={}",
                        err.kind.as_str(),
                        matching.kind.as_str(),
                        line,
                        err.message
                    ),
                );
                let prev_err = state.current_error.replace(err.clone());
                state.set_internal("error.kind", err.kind.as_str().to_string());
                state.set_internal("error.message", err.message.clone());
                let res = self.exec_block(&matching.body, state);
                state.current_error = prev_err;
                state.clear_internal("error.kind");
                state.clear_internal("error.message");
                res
            }
        }
    }
}

fn pick_catch(catches: &[Catch], kind: CatchKind) -> Option<&Catch> {
    if let Some(c) = catches.iter().find(|c| c.kind == kind) {
        return Some(c);
    }
    catches.iter().find(|c| c.kind == CatchKind::Any)
}

fn classify_remote_error(err: &RemoteCallError) -> CatchKind {
    use relix_core::types::error_kinds::*;
    match err.kind {
        TIMEOUT | APPROVAL_TIMEOUT => CatchKind::Timeout,
        TRANSPORT | PEER_UNREACHABLE | 0 => CatchKind::MeshError,
        POLICY_DENIED | APPROVAL_DENIED | APPROVAL_REQUIRED => CatchKind::PolicyDenied,
        _ => CatchKind::ResponderError,
    }
}

#[derive(Clone, Copy)]
enum BlockFlow {
    Continue,
    Return,
}

#[derive(Default)]
struct ExecState {
    /// F5/F7: typed variable store. The previous executor held
    /// `HashMap<String, String>`; expanding to `SflowValue`
    /// lets `list_*` / `map_*` built-ins operate on rich values
    /// while string-context reads stringify via
    /// `SflowValue::to_display`.
    vars: HashMap<String, SflowValue>,
    /// Internal `error.kind` / `error.message` injected by the executor
    /// for the duration of a catch block. Kept separate from user-defined
    /// vars so they don't count against [`MAX_VARS`].
    internal: HashMap<String, String>,
    named_steps: HashMap<String, StepRecord>,
    last_step: StepRecord,
    loop_iter: Option<u64>,
    flow_result: String,
    /// The error a catch block is handling — used to make `rethrow` work.
    current_error: Option<RuntimeError>,
}

impl ExecState {
    fn set_var(&mut self, name: &str, value: SflowValue, line: usize) -> Result<(), RuntimeError> {
        if !self.vars.contains_key(name) && self.vars.len() >= MAX_VARS {
            return Err(RuntimeError::local(
                line,
                format!("variable cap exceeded ({MAX_VARS} max per flow)"),
            ));
        }
        self.vars.insert(name.to_string(), value);
        Ok(())
    }

    fn set_internal(&mut self, name: &str, value: String) {
        self.internal.insert(name.to_string(), value);
    }

    fn clear_internal(&mut self, name: &str) {
        self.internal.remove(name);
    }

    /// String-context resolve. Preserves the contract of the
    /// pre-F5 executor: every step arg, every interpolation,
    /// every condition compares against a `String`.
    fn resolve(&self, expr: &Expr, line: usize) -> Result<String, RuntimeError> {
        Ok(self.resolve_value(expr, line)?.to_display())
    }

    /// Typed resolve. Returns the rich `SflowValue` so that
    /// `set x = [...]` and list/map built-ins can carry the
    /// structured shape through to the var store.
    fn resolve_value(&self, expr: &Expr, line: usize) -> Result<SflowValue, RuntimeError> {
        Ok(match expr {
            Expr::Literal(s) => SflowValue::String(self.interpolate(s, line)?),
            Expr::LastResult => SflowValue::String(self.last_step.result.clone()),
            Expr::Var(name) => self.vars.get(name).cloned().unwrap_or_default(),
            Expr::StepResult(name) => SflowValue::String(
                self.named_steps
                    .get(name)
                    .map(|s| s.result.clone())
                    .unwrap_or_default(),
            ),
            Expr::ListLit(elements) => {
                let mut out: Vec<String> = Vec::with_capacity(elements.len());
                for e in elements {
                    out.push(self.resolve(e, line)?);
                }
                SflowValue::List(out)
            }
            Expr::MapLit(pairs) => {
                let mut out: Vec<(String, String)> = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let value = self.resolve(v, line)?;
                    // De-dup keys with last-write-wins so the
                    // structure mirrors how `map_set` updates
                    // existing keys.
                    if let Some(existing) = out.iter_mut().find(|(ek, _)| ek == k) {
                        existing.1 = value;
                    } else {
                        out.push((k.clone(), value));
                    }
                }
                SflowValue::Map(out)
            }
            Expr::Call(name, args) => self.eval_builtin(name, args, line)?,
        })
    }

    /// Evaluate a built-in call. Returns the typed result
    /// (e.g. `list_len` is `SflowValue::String("3")` so that
    /// `set count = list_len(var.xs)` works in string
    /// contexts; an explicit integer type isn't needed because
    /// Sflow has no `int` value form).
    fn eval_builtin(
        &self,
        name: &str,
        args: &[Expr],
        line: usize,
    ) -> Result<SflowValue, RuntimeError> {
        match name {
            "list_len" => {
                expect_arity(name, args, 1, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let n = match v {
                    SflowValue::List(items) => items.len(),
                    SflowValue::String(s) if s.is_empty() => 0,
                    SflowValue::String(s) => s.split('|').count(),
                    SflowValue::Map(pairs) => pairs.len(),
                };
                Ok(SflowValue::String(n.to_string()))
            }
            "list_get" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let idx_str = self.resolve(&args[1], line)?;
                let Ok(idx) = idx_str.parse::<i64>() else {
                    return Err(RuntimeError::local(
                        line,
                        format!("list_get index must be an integer, got `{idx_str}`"),
                    ));
                };
                let items: Vec<String> = match v {
                    SflowValue::List(it) => it,
                    SflowValue::String(s) if s.is_empty() => Vec::new(),
                    SflowValue::String(s) => s.split('|').map(str::to_string).collect(),
                    SflowValue::Map(pairs) => {
                        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
                    }
                };
                let s = if idx < 0 || (idx as usize) >= items.len() {
                    String::new()
                } else {
                    items[idx as usize].clone()
                };
                Ok(SflowValue::String(s))
            }
            "list_push" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let val = self.resolve(&args[1], line)?;
                let mut items: Vec<String> = match v {
                    SflowValue::List(it) => it,
                    SflowValue::String(s) if s.is_empty() => Vec::new(),
                    SflowValue::String(s) => s.split('|').map(str::to_string).collect(),
                    SflowValue::Map(pairs) => {
                        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
                    }
                };
                items.push(val);
                Ok(SflowValue::List(items))
            }
            "list_contains" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let needle = self.resolve(&args[1], line)?;
                let items: Vec<String> = match v {
                    SflowValue::List(it) => it,
                    SflowValue::String(s) if s.is_empty() => Vec::new(),
                    SflowValue::String(s) => s.split('|').map(str::to_string).collect(),
                    SflowValue::Map(pairs) => {
                        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
                    }
                };
                Ok(SflowValue::String(
                    if items.iter().any(|x| x == &needle) {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                ))
            }
            "list_join" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let sep = self.resolve(&args[1], line)?;
                let items: Vec<String> = match v {
                    SflowValue::List(it) => it,
                    SflowValue::String(s) if s.is_empty() => Vec::new(),
                    SflowValue::String(s) => s.split('|').map(str::to_string).collect(),
                    SflowValue::Map(pairs) => {
                        pairs.iter().map(|(k, v)| format!("{k}={v}")).collect()
                    }
                };
                Ok(SflowValue::String(items.join(&sep)))
            }
            "list_split" => {
                expect_arity(name, args, 2, line)?;
                let src = self.resolve(&args[0], line)?;
                let sep = self.resolve(&args[1], line)?;
                let items: Vec<String> = if sep.is_empty() {
                    vec![src.clone()]
                } else {
                    src.split(&sep).map(str::to_string).collect()
                };
                Ok(SflowValue::List(items))
            }
            "map_get" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let pairs = map_pairs_from(v);
                let s = pairs
                    .into_iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v)
                    .unwrap_or_default();
                Ok(SflowValue::String(s))
            }
            "map_set" => {
                expect_arity(name, args, 3, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let val = self.resolve(&args[2], line)?;
                let mut pairs = map_pairs_from(v);
                if let Some(existing) = pairs.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = val;
                } else {
                    pairs.push((key, val));
                }
                Ok(SflowValue::Map(pairs))
            }
            "map_has" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let pairs = map_pairs_from(v);
                Ok(SflowValue::String(
                    if pairs.iter().any(|(k, _)| *k == key) {
                        "true"
                    } else {
                        "false"
                    }
                    .to_string(),
                ))
            }
            "map_keys" => {
                expect_arity(name, args, 1, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let pairs = map_pairs_from(v);
                Ok(SflowValue::List(
                    pairs.into_iter().map(|(k, _)| k).collect(),
                ))
            }
            "map_len" => {
                expect_arity(name, args, 1, line)?;
                let v = self.resolve_value(&args[0], line)?;
                Ok(SflowValue::String(map_pairs_from(v).len().to_string()))
            }
            "map_del" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let pairs = map_pairs_from(v)
                    .into_iter()
                    .filter(|(k, _)| *k != key)
                    .collect();
                Ok(SflowValue::Map(pairs))
            }
            _ => Err(RuntimeError::local(
                line,
                format!("unknown built-in `{name}`"),
            )),
        }
    }

    /// Expand `${…}` placeholders in a string literal. Unknown placeholders
    /// expand to the empty string; unmatched `$`s pass through verbatim.
    fn interpolate(&self, s: &str, _line: usize) -> Result<String, RuntimeError> {
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                let end = match s[i + 2..].find('}') {
                    Some(off) => i + 2 + off,
                    None => {
                        out.push_str(&s[i..]);
                        break;
                    }
                };
                let key = &s[i + 2..end];
                out.push_str(&self.lookup_placeholder(key));
                i = end + 1;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        Ok(out)
    }

    fn lookup_placeholder(&self, key: &str) -> String {
        match key {
            "loop.iter" => self.loop_iter.map(|i| i.to_string()).unwrap_or_default(),
            "result" => self.last_step.result.clone(),
            "status" => self.last_step.status.clone(),
            "error.kind" | "error.message" => self.internal.get(key).cloned().unwrap_or_default(),
            k if k.starts_with("var.") => self
                .vars
                .get(&k[4..])
                .map(SflowValue::to_display)
                .unwrap_or_default(),
            k if k.starts_with("step.") => {
                if let Some(rest) = k.strip_prefix("step.") {
                    if let Some(name) = rest.strip_suffix(".result") {
                        return self
                            .named_steps
                            .get(name)
                            .map(|s| s.result.clone())
                            .unwrap_or_default();
                    }
                    if let Some(name) = rest.strip_suffix(".status") {
                        return self
                            .named_steps
                            .get(name)
                            .map(|s| s.status.clone())
                            .unwrap_or_default();
                    }
                }
                String::new()
            }
            // bare `var_name` — treat as variable lookup for ergonomics
            k => self
                .vars
                .get(k)
                .map(SflowValue::to_display)
                .unwrap_or_default(),
        }
    }

    fn eval(&self, cond: &Condition, line: usize) -> Result<bool, RuntimeError> {
        Ok(match cond {
            Condition::True => true,
            Condition::False => false,
            Condition::And(a, b) => self.eval(a, line)? && self.eval(b, line)?,
            Condition::Or(a, b) => self.eval(a, line)? || self.eval(b, line)?,
            Condition::Not(c) => !self.eval(c, line)?,
            Condition::Exists(atom) => {
                let v = self.read_atom(atom);
                !v.is_empty()
            }
            Condition::Compare(atom, op, rhs) => {
                let lhs = self.read_atom(atom);
                match op {
                    CmpOp::Eq => lhs == *rhs,
                    CmpOp::Neq => lhs != *rhs,
                    CmpOp::Contains => lhs.contains(rhs.as_str()),
                    CmpOp::Matches => match Regex::new(rhs) {
                        Ok(re) => re.is_match(&lhs),
                        Err(e) => {
                            return Err(RuntimeError::local(
                                line,
                                format!("invalid regex `{rhs}`: {e}"),
                            ));
                        }
                    },
                }
            }
        })
    }

    fn read_atom(&self, atom: &Atom) -> String {
        match atom {
            Atom::Status => self.last_step.status.clone(),
            Atom::Result => self.last_step.result.clone(),
            Atom::Var(name) => self
                .vars
                .get(name)
                .map(SflowValue::to_display)
                .unwrap_or_default(),
            Atom::StepStatus(name) => self
                .named_steps
                .get(name)
                .map(|s| s.status.clone())
                .unwrap_or_default(),
            Atom::StepResult(name) => self
                .named_steps
                .get(name)
                .map(|s| s.result.clone())
                .unwrap_or_default(),
        }
    }
}

/// Coerce a SflowValue into the pair-list shape map built-ins
/// expect. A `Map` returns its pairs directly; a `String` is
/// parsed against the canonical `k1=v1;k2=v2` encoding (empty
/// string → empty map; segments without `=` map to empty
/// values). A `List` cannot be coerced into a map and returns
/// an empty pair list — built-ins like `map_get` on a list var
/// silently return `""` rather than panicking, matching the
/// SOL behaviour of `map_get(list, "k") -> ""`.
fn map_pairs_from(v: SflowValue) -> Vec<(String, String)> {
    match v {
        SflowValue::Map(pairs) => pairs,
        SflowValue::String(s) if s.is_empty() => Vec::new(),
        SflowValue::String(s) => s
            .split(';')
            .map(|seg| match seg.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (seg.to_string(), String::new()),
            })
            .collect(),
        SflowValue::List(_) => Vec::new(),
    }
}

fn expect_arity(
    name: &str,
    args: &[Expr],
    expected: usize,
    line: usize,
) -> Result<(), RuntimeError> {
    if args.len() != expected {
        return Err(RuntimeError::local(
            line,
            format!(
                "{name}() takes {expected} argument{} but received {}",
                if expected == 1 { "" } else { "s" },
                args.len()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sflow::compile;
    use std::sync::Mutex;

    /// Dispatcher that returns scripted responses in order.
    struct ScriptedDispatcher {
        calls: Mutex<Vec<(String, String, Vec<u8>)>>,
        responses: Mutex<Vec<Result<Vec<u8>, RemoteCallError>>>,
    }
    impl ScriptedDispatcher {
        fn new(responses: Vec<Result<Vec<u8>, RemoteCallError>>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into_iter().rev().collect()),
            })
        }
        fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl RemoteCallDispatcher for ScriptedDispatcher {
        fn remote_call(
            &self,
            peer: &str,
            method: &str,
            arg: &[u8],
        ) -> Result<Vec<u8>, RemoteCallError> {
            self.calls
                .lock()
                .unwrap()
                .push((peer.to_string(), method.to_string(), arg.to_vec()));
            self.responses.lock().unwrap().pop().unwrap_or_else(|| {
                Err(RemoteCallError::local(peer, method, "no scripted response"))
            })
        }
    }

    /// Helper for tests that don't need a dispatcher — F5/F7
    /// list & map behaviour is verified by setting vars and
    /// inspecting flow_result, not by dispatching capabilities.
    fn exec_no_dispatch(src: &str) -> (ExecOutcome, Arc<VecChronicle>) {
        let prog = compile(src).unwrap();
        let dispatcher = ScriptedDispatcher::new(Vec::new());
        let chronicle: Arc<VecChronicle> = Arc::new(VecChronicle::default());
        let executor = Executor::new(dispatcher, chronicle.clone());
        let outcome = executor.run(&prog);
        (outcome, chronicle)
    }

    #[test]
    fn empty_list_literal_in_set_stores_empty_list() {
        let src = r#"
            set xs = []
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.result, "0");
    }

    #[test]
    fn three_element_list_literal_has_length_three() {
        let src = r#"
            set xs = ["a", "b", "c"]
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.result, "3");
    }

    #[test]
    fn list_get_returns_element_at_index() {
        let src = r#"
            set xs = ["alpha", "beta", "gamma"]
            sol.set_result list_get(var.xs, "1")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "beta");
    }

    #[test]
    fn list_get_out_of_bounds_returns_empty_string() {
        let src = r#"
            set xs = ["only"]
            sol.set_result list_get(var.xs, "99")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "");
    }

    #[test]
    fn list_push_returns_new_list_original_unchanged() {
        let src = r#"
            set xs = ["a", "b", "c"]
            set ys = list_push(var.xs, "d")
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "3", "original list must not be mutated");

        let src2 = r#"
            set xs = ["a", "b", "c"]
            set ys = list_push(var.xs, "d")
            sol.set_result list_len(var.ys)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src2);
        assert_eq!(outcome.result, "4");
    }

    #[test]
    fn list_contains_returns_true_for_present_value() {
        let src = r#"
            set xs = ["a", "b", "c"]
            sol.set_result list_contains(var.xs, "b")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "true");
    }

    #[test]
    fn list_contains_returns_false_for_absent_value() {
        let src = r#"
            set xs = ["a", "b"]
            sol.set_result list_contains(var.xs, "z")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "false");
    }

    #[test]
    fn list_join_produces_correct_string() {
        let src = r#"
            set xs = ["a", "b", "c"]
            sol.set_result list_join(var.xs, "-")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "a-b-c");
    }

    #[test]
    fn list_split_with_separator_returns_three_elements() {
        let src = r#"
            set xs = list_split("a|b|c", "|")
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "3");
    }

    #[test]
    fn list_split_empty_string_produces_single_element_list() {
        let src = r#"
            set xs = list_split("", "|")
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "1");
    }

    #[test]
    fn empty_map_literal_has_length_zero() {
        let src = r#"
            set m = {}
            sol.set_result map_len(var.m)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "0");
    }

    #[test]
    fn map_with_two_pairs_returns_correct_values() {
        let src = r#"
            set m = { "k1": "v1", "k2": "v2" }
            sol.set_result map_get(var.m, "k2")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "v2");
    }

    #[test]
    fn map_get_missing_key_returns_empty_string() {
        let src = r#"
            set m = { "k1": "v1" }
            sol.set_result map_get(var.m, "absent")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "");
    }

    #[test]
    fn map_has_returns_true_for_present_key() {
        let src = r#"
            set m = { "k1": "v1" }
            sol.set_result map_has(var.m, "k1")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "true");
    }

    #[test]
    fn map_set_returns_new_map_original_unchanged() {
        let src = r#"
            set m = { "k1": "v1" }
            set m2 = map_set(var.m, "k2", "v2")
            sol.set_result map_len(var.m)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "1");

        let src2 = r#"
            set m = { "k1": "v1" }
            set m2 = map_set(var.m, "k2", "v2")
            sol.set_result map_len(var.m2)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src2);
        assert_eq!(outcome.result, "2");
    }

    #[test]
    fn map_del_returns_new_map_with_key_removed() {
        let src = r#"
            set m = { "a": "1", "b": "2" }
            set m2 = map_del(var.m, "a")
            sol.set_result map_has(var.m2, "a")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "false");
    }

    #[test]
    fn map_keys_returns_keys_list_in_insertion_order() {
        let src = r#"
            set m = { "a": "1", "b": "2", "c": "3" }
            set ks = map_keys(var.m)
            sol.set_result list_get(var.ks, "0")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "a");
    }

    #[test]
    fn list_display_format_is_pipe_separated() {
        // Lists round-trip into step-arg / interpolation
        // contexts via `SflowValue::to_display`, which
        // pipe-joins. Verify this so flows authors know what
        // the wire format looks like.
        let src = r#"
            set xs = ["a", "b", "c"]
            sol.set_result "joined ${var.xs}"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "joined a|b|c");
    }

    #[test]
    fn map_display_format_is_semicolon_separated() {
        let src = r#"
            set m = { "k1": "v1", "k2": "v2" }
            sol.set_result "encoded ${var.m}"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "encoded k1=v1;k2=v2");
    }

    #[test]
    fn map_literal_value_can_carry_interpolation() {
        let src = r#"
            set name = "alice"
            set m = { "greeting": "hi ${var.name}" }
            sol.set_result map_get(var.m, "greeting")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "hi alice");
    }

    #[test]
    fn nested_map_set_chains_correctly() {
        let src = r#"
            set m = {}
            set m = map_set(var.m, "a", "1")
            set m = map_set(var.m, "b", "2")
            set m = map_set(var.m, "c", "3")
            sol.set_result map_len(var.m)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "3");
    }

    fn exec(
        src: &str,
        responses: Vec<Result<Vec<u8>, RemoteCallError>>,
    ) -> (ExecOutcome, Arc<VecChronicle>, Arc<ScriptedDispatcher>) {
        let prog = compile(src).expect("compile");
        let disp = ScriptedDispatcher::new(responses);
        let chr = Arc::new(VecChronicle::default());
        let exe = Executor::new(disp.clone(), chr.clone());
        let out = exe.run(&prog);
        (out, chr, disp)
    }

    #[test]
    fn if_true_branch_executes_false_branch_skips() {
        let src = "if true\nsol.set_result \"yes\"\nelse\nsol.set_result \"no\"\nend\n";
        let (out, _, _) = exec(src, vec![]);
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.result, "yes");
    }

    #[test]
    fn elif_branch_executes_when_if_false() {
        let src = "if false\nsol.set_result \"a\"\nelif true\nsol.set_result \"b\"\nelse\nsol.set_result \"c\"\nend\n";
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "b");
    }

    #[test]
    fn else_branch_executes_when_no_condition_matches() {
        let src = "if false\nsol.set_result \"a\"\nelif false\nsol.set_result \"b\"\nelse\nsol.set_result \"c\"\nend\n";
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "c");
    }

    #[test]
    fn loop_n_times_runs_exactly_n_times() {
        let src = "loop 3 times\nsol.log \"iter ${loop.iter}\"\nend\nsol.set_result \"done\"\n";
        let (out, chr, _) = exec(src, vec![]);
        assert!(out.error.is_none());
        let logs: Vec<String> = chr
            .entries()
            .into_iter()
            .filter_map(|(k, p)| if k == "sol.log" { Some(p) } else { None })
            .collect();
        assert_eq!(logs, vec!["iter 0", "iter 1", "iter 2"]);
        assert_eq!(out.result, "done");
    }

    #[test]
    fn loop_cap_triggers_chronicle_event_and_breaks() {
        let src = "loop 999999 times\nsol.log \"x\"\nend\n";
        let (out, chr, _) = exec(src, vec![]);
        assert!(out.error.is_none());
        let cap_hits = chr
            .kinds()
            .into_iter()
            .filter(|k| k == "sol.loop_limit_hit")
            .count();
        assert_eq!(cap_hits, 1);
        let logs = chr.kinds().into_iter().filter(|k| k == "sol.log").count();
        assert_eq!(logs as u64, DEFAULT_MAX_LOOP_ITERS);
    }

    #[test]
    fn while_exits_when_condition_false() {
        let src = concat!(
            "set count = \"0\"\n",
            "while var.count != \"3\"\n",
            "set count = \"3\"\n",
            "end\n",
            "sol.set_result var.count\n",
        );
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "3");
    }

    #[test]
    fn try_catches_simulated_responder_error() {
        let src = concat!(
            "try\n",
            "ai.chat \"hi\"\n",
            "sol.set_result \"shouldnt reach\"\n",
            "catch responder_error\n",
            "sol.set_result \"caught\"\n",
            "end\n",
        );
        let (out, chr, _) = exec(
            src,
            vec![Err(RemoteCallError {
                kind: 11,
                peer: "ai".into(),
                method: "ai.chat".into(),
                cause: "kaboom".into(),
            })],
        );
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.result, "caught");
        assert!(chr.kinds().iter().any(|k| k == "sol.error_caught"));
    }

    #[test]
    fn try_catches_any_when_kind_mismatched() {
        let src = concat!(
            "try\n",
            "ai.chat \"hi\"\n",
            "catch timeout\n",
            "sol.set_result \"timed_out\"\n",
            "catch any\n",
            "sol.set_result \"other\"\n",
            "end\n",
        );
        let (out, _, _) = exec(
            src,
            vec![Err(RemoteCallError {
                kind: 11,
                peer: "ai".into(),
                method: "ai.chat".into(),
                cause: "internal".into(),
            })],
        );
        assert_eq!(out.result, "other");
    }

    #[test]
    fn execution_continues_after_catch_end() {
        let src = concat!(
            "try\n",
            "ai.chat \"hi\"\n",
            "catch any\n",
            "sol.set_result \"caught\"\n",
            "end\n",
            "sol.log \"after\"\n",
        );
        let (out, chr, _) = exec(src, vec![Err(RemoteCallError::local("ai", "ai.chat", "x"))]);
        assert!(out.error.is_none());
        assert!(
            chr.entries()
                .iter()
                .any(|(k, p)| k == "sol.log" && p == "after")
        );
    }

    #[test]
    fn rethrow_propagates_to_outer_handler() {
        let src = concat!(
            "try\n",
            "try\n",
            "ai.chat \"hi\"\n",
            "catch any\n",
            "rethrow\n",
            "end\n",
            "catch any\n",
            "sol.set_result \"outer\"\n",
            "end\n",
        );
        let (out, _, _) = exec(src, vec![Err(RemoteCallError::local("ai", "ai.chat", "x"))]);
        assert_eq!(out.result, "outer");
    }

    #[test]
    fn var_interpolation_works_in_step_args() {
        let src = "set name = \"alice\"\nai.chat \"hi ${var.name}\"\n";
        let (_, _, disp) = exec(src, vec![Ok(b"ok".to_vec())]);
        let calls = disp.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(String::from_utf8_lossy(&calls[0].2), "hi alice");
    }

    #[test]
    fn loop_iter_placeholder_resolves_correctly() {
        let src = "loop 3 times\nsol.log \"i=${loop.iter}\"\nend\n";
        let (_, chr, _) = exec(src, vec![]);
        let logs: Vec<String> = chr
            .entries()
            .into_iter()
            .filter_map(|(k, p)| if k == "sol.log" { Some(p) } else { None })
            .collect();
        assert_eq!(logs, vec!["i=0", "i=1", "i=2"]);
    }

    #[test]
    fn set_var_eq_result_captures_last_step_result() {
        let src = "ai.chat \"hi\"\nset r = result\nsol.set_result var.r\n";
        let (out, _, _) = exec(src, vec![Ok(b"hello".to_vec())]);
        assert_eq!(out.result, "hello");
    }

    #[test]
    fn named_step_result_visible_in_condition() {
        let src = concat!(
            "step check: ai.ping \"x\"\n",
            "if step.check.result contains \"ok\"\n",
            "sol.set_result \"reachable\"\n",
            "else\n",
            "sol.set_result \"unreachable\"\n",
            "end\n",
        );
        let (out, _, _) = exec(src, vec![Ok(b"ok pong".to_vec())]);
        assert_eq!(out.result, "reachable");
    }

    #[test]
    fn return_exits_flow_with_value() {
        let src = "set x = \"early\"\nreturn var.x\nsol.set_result \"late\"\n";
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "early");
    }

    #[test]
    fn sol_log_writes_chronicle_event() {
        let src = "sol.log \"hello\"\n";
        let (_, chr, _) = exec(src, vec![]);
        assert!(
            chr.entries()
                .iter()
                .any(|(k, p)| k == "sol.log" && p == "hello")
        );
    }

    #[test]
    fn sol_assert_fails_flow_on_false_condition() {
        let src = "sol.assert false\nsol.set_result \"never\"\n";
        let (out, _, _) = exec(src, vec![]);
        assert!(out.error.is_some());
        assert!(out.error.as_ref().unwrap().message.contains("assert"));
    }

    #[test]
    fn sol_set_result_sets_flow_result() {
        let src = "sol.set_result \"chosen\"\nreturn\n";
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "chosen");
    }

    #[test]
    fn sol_sleep_pauses_briefly() {
        // 0s sleep just exercises the path without blocking the test.
        let src = "sol.sleep 0\nsol.set_result \"ok\"\n";
        let (out, _, _) = exec(src, vec![]);
        assert_eq!(out.result, "ok");
    }

    #[test]
    fn uncaught_error_outside_try_fails_flow() {
        let src = "ai.chat \"hi\"\nsol.set_result \"never\"\n";
        let (out, _, _) = exec(
            src,
            vec![Err(RemoteCallError::local("ai", "ai.chat", "boom"))],
        );
        assert!(out.error.is_some());
        let e = out.error.unwrap();
        assert!(e.message.contains("boom"));
    }

    #[test]
    fn var_cap_exceeded_fails_flow() {
        let mut src = String::new();
        for i in 0..51 {
            src.push_str(&format!("set v{i} = \"x\"\n"));
        }
        let (out, _, _) = exec(&src, vec![]);
        assert!(out.error.is_some());
        assert!(out.error.unwrap().message.contains("variable cap"));
    }
}
