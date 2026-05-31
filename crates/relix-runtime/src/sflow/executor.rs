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
/// F11: list / map values themselves carry `SflowValue` so
/// nested structures survive across `set` boundaries — a list
/// of lists or a map of lists keeps its typed shape until it's
/// pulled into a string context, at which point `to_display`
/// recurses through the structure to produce a flat
/// pipe-or-semicolon encoded preview.
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
    List(Vec<SflowValue>),
    Map(Vec<(String, SflowValue)>),
}

impl SflowValue {
    /// Return the string representation used in step args,
    /// interpolation, conditions, and the chronicle log.
    /// Lists become `a|b|c`; maps become `k1=v1;k2=v2`.
    /// Nested values recurse through `to_display`, producing
    /// the same flat display the pre-F11 executor did for
    /// single-level structures.
    pub fn to_display(&self) -> String {
        match self {
            SflowValue::String(s) => s.clone(),
            SflowValue::List(items) => items
                .iter()
                .map(SflowValue::to_display)
                .collect::<Vec<_>>()
                .join("|"),
            SflowValue::Map(pairs) => pairs
                .iter()
                .map(|(k, v)| format!("{k}={}", v.to_display()))
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
    fn lock_poison_safe(&self) -> std::sync::MutexGuard<'_, Vec<(String, String)>> {
        self.inner.lock().unwrap_or_else(|e| {
            tracing::warn!("VecChronicle mutex poisoned; recovering inner state");
            e.into_inner()
        })
    }
    pub fn entries(&self) -> Vec<(String, String)> {
        self.lock_poison_safe().clone()
    }
    pub fn kinds(&self) -> Vec<String> {
        self.lock_poison_safe()
            .iter()
            .map(|(k, _)| k.clone())
            .collect()
    }
}
impl ChronicleSink for VecChronicle {
    fn write(&self, kind: &str, payload: &str) {
        self.lock_poison_safe()
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
            Stmt::For {
                var_name,
                iter,
                body,
                line,
            } => self.exec_for(var_name, iter, body, *line, state),
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
                // PART 1: prefer tokio's timer when the executor
                // is running inside a tokio runtime (production
                // path runs via `tokio::task::spawn_blocking`
                // from `flow_runner::run_sflow`, so a Handle is
                // reachable). Fall back to std::thread::sleep
                // for pure-sync test invocations.
                match tokio::runtime::Handle::try_current() {
                    Ok(h) => h.block_on(tokio::time::sleep(Duration::from_secs(s))),
                    Err(_) => std::thread::sleep(Duration::from_secs(s)),
                }
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

    /// F9: `for <ident> in <list> ... end`. Resolves the
    /// iterable to a list of strings (a `SflowValue::List`
    /// is iterated directly; a `String` is split on the
    /// canonical `|` separator the same way `list_*` builtins
    /// coerce). The loop variable is set once per iteration
    /// to the current element; the prior binding for that
    /// name (if any) is restored after the loop completes so
    /// the var doesn't leak. Same per-block iteration cap as
    /// `loop N times` / `while`; same `MAX_NESTING_DEPTH`.
    fn exec_for(
        &self,
        var_name: &str,
        iter: &Expr,
        body: &[Stmt],
        line: usize,
        state: &mut ExecState,
    ) -> Result<BlockFlow, RuntimeError> {
        let value = state.resolve_value(iter, line)?;
        // F11: iterate over the typed items so a list-of-lists
        // exposes each inner list to the loop body as a
        // SflowValue::List rather than its stringified form.
        let items = list_items_from(value);
        let cap = self.max_loop_iters;
        if items.len() as u64 > cap {
            self.chronicle.write(
                "sol.loop_limit_hit",
                &format!(
                    "requested={n} cap={cap} kind=for line={line}",
                    n = items.len()
                ),
            );
        }
        let prior = state.vars.remove(var_name);
        // Cap iterations the same way `loop N times` /
        // `while` do. `.take(cap)` is the clippy-preferred
        // shape over a manual counter (`explicit_counter_loop`).
        for (idx, elem) in items.into_iter().take(cap as usize).enumerate() {
            let prev_loop = state.loop_iter.replace(idx as u64);
            state.set_var(var_name, elem, line)?;
            self.chronicle
                .write("sol.loop_iter", &format!("iter={idx} kind=for line={line}"));
            let res = self.exec_block(body, state);
            state.loop_iter = prev_loop;
            match res? {
                BlockFlow::Return => {
                    // Restore the prior binding even on a
                    // return — the var still shouldn't leak
                    // upward into the caller's scope.
                    state.vars.remove(var_name);
                    if let Some(p) = prior {
                        state.vars.insert(var_name.to_string(), p);
                    }
                    return Ok(BlockFlow::Return);
                }
                BlockFlow::Continue => {}
            }
        }
        // Restore the prior binding (or remove the loop var
        // outright when it was unset before the loop).
        state.vars.remove(var_name);
        if let Some(p) = prior {
            state.vars.insert(var_name.to_string(), p);
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
                // F11: preserve nested typed values. Resolving
                // each element via `resolve_value` instead of
                // `resolve` (which would stringify) keeps a
                // list-of-lists or a list-of-maps usable by
                // downstream builtins.
                let mut out: Vec<SflowValue> = Vec::with_capacity(elements.len());
                for e in elements {
                    out.push(self.resolve_value(e, line)?);
                }
                SflowValue::List(out)
            }
            Expr::MapLit(pairs) => {
                let mut out: Vec<(String, SflowValue)> = Vec::with_capacity(pairs.len());
                for (k, v) in pairs {
                    let value = self.resolve_value(v, line)?;
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

    /// Evaluate a built-in call. Returns the typed result —
    /// `list_get` / `map_get` preserve the stored
    /// `SflowValue` (so a nested list survives across a
    /// builtin call boundary), while numeric / boolean
    /// results stringify since Sflow has no `int` / `bool`
    /// type.
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
                let n = list_items_from(v).len();
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
                let items = list_items_from(v);
                if idx < 0 || (idx as usize) >= items.len() {
                    Ok(SflowValue::String(String::new()))
                } else {
                    // F11: return the stored SflowValue so a
                    // nested list / map survives the read.
                    Ok(items.into_iter().nth(idx as usize).unwrap_or_default())
                }
            }
            "list_get_list" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let idx_str = self.resolve(&args[1], line)?;
                let Ok(idx) = idx_str.parse::<i64>() else {
                    return Err(RuntimeError::local(
                        line,
                        format!("list_get_list index must be an integer, got `{idx_str}`"),
                    ));
                };
                let items = list_items_from(v);
                if idx < 0 || (idx as usize) >= items.len() {
                    return Err(RuntimeError::local(
                        line,
                        format!(
                            "list_get_list: index {idx} out of bounds (len {})",
                            items.len()
                        ),
                    ));
                }
                let elem = items.into_iter().nth(idx as usize).unwrap_or_default();
                match elem {
                    SflowValue::List(_) => Ok(elem),
                    other => Err(RuntimeError::local(
                        line,
                        format!(
                            "list_get_list: element at index {idx} is not a list (it's `{}`)",
                            other.to_display()
                        ),
                    )),
                }
            }
            "list_push" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let val = self.resolve_value(&args[1], line)?;
                let mut items = list_items_from(v);
                items.push(val);
                Ok(SflowValue::List(items))
            }
            "list_contains" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let needle = self.resolve(&args[1], line)?;
                let items = list_items_from(v);
                let present = items.iter().any(|x| x.to_display() == needle);
                Ok(SflowValue::String(
                    if present { "true" } else { "false" }.to_string(),
                ))
            }
            "list_join" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let sep = self.resolve(&args[1], line)?;
                let items = list_items_from(v);
                let parts: Vec<String> = items.iter().map(SflowValue::to_display).collect();
                Ok(SflowValue::String(parts.join(&sep)))
            }
            "list_split" => {
                expect_arity(name, args, 2, line)?;
                let src = self.resolve(&args[0], line)?;
                let sep = self.resolve(&args[1], line)?;
                let items: Vec<SflowValue> = if sep.is_empty() {
                    vec![SflowValue::String(src)]
                } else {
                    src.split(&sep)
                        .map(|s| SflowValue::String(s.to_string()))
                        .collect()
                };
                Ok(SflowValue::List(items))
            }
            "map_get" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let pairs = map_pairs_from(v);
                Ok(pairs
                    .into_iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v)
                    .unwrap_or_default())
            }
            "map_get_map" => {
                expect_arity(name, args, 2, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let pairs = map_pairs_from(v);
                let Some(value) = pairs.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v) else {
                    return Err(RuntimeError::local(
                        line,
                        format!("map_get_map: key `{key}` not present"),
                    ));
                };
                match value {
                    SflowValue::Map(_) => Ok(value),
                    other => Err(RuntimeError::local(
                        line,
                        format!(
                            "map_get_map: value at `{key}` is not a map (it's `{}`)",
                            other.to_display()
                        ),
                    )),
                }
            }
            "map_set" => {
                expect_arity(name, args, 3, line)?;
                let v = self.resolve_value(&args[0], line)?;
                let key = self.resolve(&args[1], line)?;
                let val = self.resolve_value(&args[2], line)?;
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
                    pairs
                        .into_iter()
                        .map(|(k, _)| SflowValue::String(k))
                        .collect(),
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

/// F11: Coerce a SflowValue into the pair-list shape map
/// built-ins expect. A `Map` returns its pairs directly; a
/// `String` is parsed against the canonical `k1=v1;k2=v2`
/// encoding with each value wrapped as `SflowValue::String`.
/// Empty string → empty map; segments without `=` map to empty
/// `SflowValue::String("")` values. A `List` cannot be coerced
/// into a map and returns an empty pair list — built-ins like
/// `map_get` on a list var silently return `""` rather than
/// panicking, matching the SOL behaviour of `map_get(list, "k")
/// -> ""`.
fn map_pairs_from(v: SflowValue) -> Vec<(String, SflowValue)> {
    match v {
        SflowValue::Map(pairs) => pairs,
        SflowValue::String(s) if s.is_empty() => Vec::new(),
        SflowValue::String(s) => s
            .split(';')
            .map(|seg| match seg.split_once('=') {
                Some((k, v)) => (k.to_string(), SflowValue::String(v.to_string())),
                None => (seg.to_string(), SflowValue::String(String::new())),
            })
            .collect(),
        SflowValue::List(_) => Vec::new(),
    }
}

/// F11: coerce a SflowValue into a `Vec<SflowValue>` for the
/// list built-ins. A `List` returns its items directly. A
/// non-empty `String` splits on `|` and wraps each segment as
/// `SflowValue::String` (preserving the same coercion
/// behaviour the pre-F11 executor had). An empty `String`
/// returns an empty list. A `Map` lowers to a list of
/// `k=v` strings — same posture as the previous executor for
/// list_join over a map.
fn list_items_from(v: SflowValue) -> Vec<SflowValue> {
    match v {
        SflowValue::List(items) => items,
        SflowValue::String(s) if s.is_empty() => Vec::new(),
        SflowValue::String(s) => s
            .split('|')
            .map(|seg| SflowValue::String(seg.to_string()))
            .collect(),
        SflowValue::Map(pairs) => pairs
            .into_iter()
            .map(|(k, v)| SflowValue::String(format!("{k}={}", v.to_display())))
            .collect(),
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

    // ── F11: nested lists & maps ───────────────────────────

    #[test]
    fn nested_list_literal_preserves_inner_lists() {
        let src = r#"
            set xs = [["a", "b"], ["c", "d"]]
            sol.set_result list_len(var.xs)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.result, "2");
    }

    #[test]
    fn list_get_on_nested_returns_typed_inner_list() {
        // The disambiguating signal: pull the inner list
        // back out and check its length. If nested support
        // is real, list_len on a list-of-three returns 3.
        let src = r#"
            set xs = [["a", "b"], ["c", "d", "e"]]
            set inner1 = list_get(var.xs, "1")
            sol.set_result list_len(var.inner1)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.result, "3");
    }

    #[test]
    fn map_with_list_value_preserves_list_via_map_get() {
        let src = r#"
            set m = { "items": ["a", "b", "c"] }
            set inner = map_get(var.m, "items")
            sol.set_result list_len(var.inner)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "3");
    }

    #[test]
    fn nested_map_preserves_inner_map_via_map_get() {
        let src = r#"
            set m = { "outer": { "inner": "v" } }
            set inner = map_get(var.m, "outer")
            sol.set_result map_get(var.inner, "inner")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "v");
    }

    #[test]
    fn list_get_list_explicit_typed_accessor_succeeds() {
        let src = r#"
            set xs = [["a", "b"], ["c", "d"]]
            set inner = list_get_list(var.xs, "0")
            sol.set_result list_len(var.inner)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "2");
    }

    #[test]
    fn list_get_list_on_string_element_returns_runtime_error() {
        let src = r#"
            set xs = ["scalar"]
            set inner = list_get_list(var.xs, "0")
            sol.set_result "should-not-reach"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_some(), "expected runtime error");
        let err = outcome.error.expect("error set");
        assert!(
            err.message.contains("not a list"),
            "error must say 'not a list': {}",
            err.message
        );
    }

    #[test]
    fn map_get_map_explicit_typed_accessor_succeeds() {
        let src = r#"
            set m = { "outer": { "inner": "v" } }
            set inner = map_get_map(var.m, "outer")
            sol.set_result map_get(var.inner, "inner")
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "v");
    }

    #[test]
    fn map_get_map_on_string_value_returns_runtime_error() {
        let src = r#"
            set m = { "k": "string-val" }
            set inner = map_get_map(var.m, "k")
            sol.set_result "should-not-reach"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_some());
        let err = outcome.error.expect("error set");
        assert!(
            err.message.contains("not a map"),
            "error must say 'not a map': {}",
            err.message
        );
    }

    #[test]
    fn nested_list_display_format_is_flat_pipe_join() {
        // A list-of-lists [["a", "b"], ["c"]] displays
        // as "a|b|c" — the outer separator is `|` and
        // the inner separator is also `|`, so the result
        // is observationally a flat pipe-join.
        let src = r#"
            set xs = [["a", "b"], ["c"]]
            sol.set_result "${var.xs}"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "a|b|c");
    }

    // ── F9: for-in over lists ──────────────────────────────

    #[test]
    fn for_in_list_literal_iterates_all_elements_in_order() {
        let src = r#"
            set acc = ""
            for x in ["a", "b", "c"]
              set acc = "${acc}${x}"
            end
            sol.set_result var.acc
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        assert_eq!(outcome.result, "abc");
    }

    #[test]
    fn for_in_iterates_list_stored_in_variable() {
        let src = r#"
            set items = ["alpha", "beta"]
            set acc = ""
            for it in var.items
              set acc = "${acc}-${it}"
            end
            sol.set_result var.acc
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "-alpha-beta");
    }

    #[test]
    fn for_in_over_pipe_string_splits_on_pipe() {
        let src = r#"
            set raw = "x|y|z"
            set acc = ""
            for el in var.raw
              set acc = "${acc}.${el}"
            end
            sol.set_result var.acc
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, ".x.y.z");
    }

    #[test]
    fn for_in_loop_variable_does_not_leak_after_end() {
        // The loop var should NOT be visible after `end`.
        // We bind a sentinel `it = "outer"` first, run a
        // loop that overwrites `it`, then assert the outer
        // value is restored.
        let src = r#"
            set it = "outer"
            for it in ["inner-a", "inner-b"]
              sol.log "inside"
            end
            sol.set_result var.it
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "outer");
    }

    #[test]
    fn for_in_loop_variable_unset_when_no_prior_binding() {
        // No prior `set it` — after the loop, `var.it`
        // should be empty (the prior unset state).
        let src = r#"
            for it in ["a", "b"]
              sol.log "noop"
            end
            sol.set_result var.it
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "");
    }

    #[test]
    fn for_in_empty_list_runs_zero_iterations() {
        let src = r#"
            set acc = "untouched"
            for el in []
              set acc = "touched"
            end
            sol.set_result var.acc
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "untouched");
    }

    #[test]
    fn for_in_nested_loops_work_within_max_nesting_depth() {
        let src = r#"
            set acc = ""
            for outer in ["x", "y"]
              for inner in ["1", "2"]
                set acc = "${acc}${outer}${inner};"
              end
            end
            sol.set_result var.acc
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "x1;x2;y1;y2;");
    }

    #[test]
    fn for_in_interops_with_list_push_accumulator() {
        let src = r#"
            set acc = []
            for el in ["a", "b", "c"]
              set acc = list_push(var.acc, var.el)
            end
            sol.set_result list_len(var.acc)
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        assert_eq!(outcome.result, "3");
    }

    #[test]
    fn for_in_with_return_inside_body_propagates() {
        let src = r#"
            for el in ["a", "b", "c"]
              return var.el
            end
            sol.set_result "fell-through"
            return
        "#;
        let (outcome, _) = exec_no_dispatch(src);
        // Return inside the first iteration: result is the
        // first element.
        assert_eq!(outcome.result, "a");
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
