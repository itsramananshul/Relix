# Flow languages

Relix ships **two** flow languages side-by-side:

| File ext | Language | When to reach for it |
|---|---|---|
| `.sol` | Rust-like SOL | Power users; existing chat flows; anything that wants typed locals and `{}` blocks |
| `.sflow` | Step-based Sflow | Operator-authored flows; error recovery via try/catch; loops with caps; lightweight conditional routing |
| `.yml` / `.yaml` | YAML flow | Operators who prefer YAML syntax; lowers to SOL and runs the same VM — see [`yaml-flow-reference.md`](yaml-flow-reference.md) |

The `flow_runner` dispatches on extension. Both languages share
the same `RemoteCallDispatcher`, the same per-flow event log,
the same `FlowRunResult` shape returned to the bridge / CLI.
Choose whichever fits the flow — there is no "wrong" answer for
a given use case.

`POST /v1/sol/validate { source, kind: "sflow" | "sol" }`
parse-checks either language without executing it; the
dashboard `#/sol` page calls it.

---

## 1. SOL (`.sol`) — Rust-like

A real little programming language ported verbatim from OpenPrem
(`crates/relix-runtime/src/sol/`):

- `function start() -> str { ... }` — single entry point per file.
- `let x: str = "literal";` — typed locals (`int`, `float`, `char`,
  `bool`, `str`, `list`, `map`, arrays, tuples, structs, enums).
- `if cond { … } else { … }`, `while cond { … }`, `for x in arr { … }`,
  `for x in lst { … }` — all compile to `Jump` / `JumpFalse` opcodes;
  the VM executes them.
- `"hi {{name}} bye"` — string interpolation. Markers expand to
  variable references at parse time (F1).
- `"a" + "b"` — string concatenation. Literals have no escape
  sequences (SIMP-016).
- `print(x)` — stdout for `flow-run`.
- `return x;`
- `remote_call("peer_or_capability_uri", "method", "arg") -> str` —
  the mesh primitive. Pipe-delimited args by convention.
- `try { … } catch <kind> { … } catch any { … }` — error recovery
  for failing `remote_call`s. Kinds: `any`, `timeout`,
  `mesh_error`, `policy_denied`, `responder_error`. Inside a
  catch body, `error_kind()` / `error_cause()` /
  `error_retry_hint()` expose the structured failure. `rethrow;`
  propagates to the next outer try-handler.
- `delegate goal G from P to T` and
  `send subject S body B from F to T` — soft-keyword sugar that
  lowers to `remote_call("coord", "delegate.spawn", …)` and
  `remote_call("coord", "msg.send", …)`. Both forms are
  expressions; the result is the child task id / message id
  (`str`).
- `last_confidence() -> float` — returns the confidence score
  (`[0.0, 1.0]`) of the most recently completed `remote_call` in
  this execution context. Returns `1.0` (neutral) before any call
  has completed. The score is stamped by the host's confidence
  scorer after each dispatch; reading it is a single atomic load.
  See `sol-language-reference.md §7.7` for scoring details.

### 1.1 List & map literals

```sol
let items: list = ["alpha", "beta", "gamma"];
let empty: list = [];
let config: map = { "model": "gpt-4o", "temperature": "0.2" };
let bare: map = {};
```

Lists are heterogeneous at the VM layer; in practice operators
use them as string lists. Maps are string-keyed; values are any
expression. Both literal forms are expressions and may appear
anywhere a value is expected (`let` RHS, function arg, delegate
goal, etc.).

Built-in surface (each lowers to a dedicated opcode; immutable
update semantics — `list_push` / `map_set` / `map_del` return
fresh objects):

| Function | Returns | Notes |
|---|---|---|
| `list_len(lst)` | `int` | |
| `list_get(lst, i)` | `str` | out-of-bounds → `""` |
| `list_push(lst, v)` | `list` | new list; original unchanged |
| `list_contains(lst, v)` | `bool` | string-compare elements |
| `list_join(lst, sep)` | `str` | |
| `list_split(s, sep)` | `list` | empty input → one empty elem |
| `map_get(m, k)` | `str` | missing key → `""` |
| `map_set(m, k, v)` | `map` | new map; original unchanged |
| `map_has(m, k)` | `bool` | |
| `map_keys(m)` | `list` | insertion order preserved |
| `map_len(m)` | `int` | |
| `map_del(m, k)` | `map` | new map; original unchanged |

`for x in lst { … }` iterates a list; each `x` is exposed to
the body as `str`.

A failed `remote_call` outside an enclosing `try` halts the VM
with `VM_ERROR_SENTINEL`. The dispatcher's structured error is
surfaced on `FlowRunResult::last_error` and the host classifies
it into a `FailureClass` for the task ledger.

### Worked example

```sol
function start() -> str {
    let session: str = "demo";
    let user_msg: str = "hello memory";

    remote_call("memory", "memory.write_turn", "demo|user|" + user_msg);
    let history: str = remote_call("memory", "memory.recent_for_session", "demo");
    let reply: str = remote_call("ai", "ai.chat", "demo|" + user_msg + "|" + history);
    remote_call("memory", "memory.write_turn", "demo|assistant|" + reply);

    return reply;
}
```

The bridge `chat_template.sol` substitutes `{{SESSION}}` and
`{{MESSAGE}}` at request time; the substitution validator rejects
`"`, `|`, and `\n` so the rendered literal cannot break out.

---

## 2. Sflow (`.sflow`) — step-based

A flat sequence of statements. No functions, no types, no
semicolons. Designed for operators to author and review without
needing to know Rust-style block syntax.

### 2.1 Basic structure

```sflow
step reply: ai.chat "basic-demo|hello"
return step.reply.result
```

Execution starts at the first statement. Falling off the end is
the same as `return` with the last step's result.

`ai.chat`'s arg is `session_id|prompt[|history]` per SIMP-016 —
a bare prompt is rejected with `INVALID_ARGS`. Every capability
defines its own arg format; check the responder before authoring
the call.

Comments use `// …` to end of line. Lines blank or containing
only a comment are skipped.

### 2.2 Capability steps

```sflow
step <name>: <peer>.<method> <arg>     // named — referenceable later
<peer>.<method> <arg>                  // unnamed
```

`<arg>` is one of: a double-quoted string literal (possibly with
`${…}` interpolations), the bareword `result`, `var.<name>`, or
`step.<other>.result`. The arg is interpolated before the
dispatcher sees it.

The result of a named step is captured under that name; both
the implicit `result` / `status` slots and a by-name map keep
the value around for later conditions and interpolations.

### 2.3 Variables

```sflow
set my_var = "literal value"
set my_var = result
set my_var = step.fetch.result
set my_var = var.other_var
set xs = ["alpha", "beta", "gamma"]               // list literal
set m = { "model": "gpt-4o", "temp": "0.2" }      // map literal
set count = list_len(var.xs)                       // built-in call
```

- Scoped to one execution. No cross-flow persistence.
- Max **50 variables** per flow. The 51st `set` fails the flow.
- Names: alphanumeric + underscore, max 32 chars, must start
  with a letter or underscore.
- Reference in args / conditions with `${my_var}` interpolation
  or the `var.my_var` bareword.
- Values can be `String` / `List` / `Map`. In string contexts
  (step args, `${…}` interpolation, conditions) lists
  stringify as `a|b|c`, maps as `k1=v1;k2=v2`.

### 2.3.1 List & map literals + built-ins

```sflow
set xs = ["alpha", "beta", "gamma"]
set empty = []
set m = { "k1": "v1", "k2": "v2" }
set bare = {}
```

Built-ins mirror the SOL surface — every `list_*` / `map_*`
returns a fresh value, never mutates the input binding.

| Function | Returns | Notes |
|---|---|---|
| `list_len(lst)` | int (as str) | `0` for empty |
| `list_get(lst, idx)` | str | out-of-bounds → `""` |
| `list_push(lst, v)` | list | new list |
| `list_contains(lst, v)` | `"true"` / `"false"` | string-compare |
| `list_join(lst, sep)` | str | |
| `list_split(s, sep)` | list | empty input → one empty elem |
| `map_get(m, k)` | str | missing key → `""` |
| `map_set(m, k, v)` | map | new map |
| `map_has(m, k)` | `"true"` / `"false"` | |
| `map_keys(m)` | list | insertion order |
| `map_len(m)` | int (as str) | |
| `map_del(m, k)` | map | new map |

Sflow has no integer / bool types so numeric / boolean results
are returned as their `str` form. `if list_contains(...) == "true"`
is the canonical pattern for branching on a boolean built-in
result.

Lists & maps can carry over into `step` args via the
stringification above:

```sflow
set parts = list_split("draft|finalize|publish", "|")
loop 3 times
  set i = "${loop.iter}"
  set name = list_get(var.parts, var.i)
  step do: tasks.update "task-id|status=${name}"
end
```

### 2.4 Conditional branching

```sflow
if status == "completed"
  <statements>
elif status != "failed"
  <statements>
else
  <statements>
end
```

Condition grammar:

| Form | Meaning |
|---|---|
| `status == "completed"` | Last step's status equals literal |
| `status != "failed"` | Last step's status not equal |
| `result contains "error"` | Last result contains substring |
| `result matches "^ERR.*"` | Last result matches Rust-regex |
| `var.my_var == "value"` | Variable equals literal |
| `var.my_var exists` | Variable is set and non-empty |
| `step.my_step.status == "completed"` | Named-step status |
| `step.my_step.result contains "ok"` | Named-step result substring |
| `true` / `false` | Literals |
| `<expr> and <expr>` | Logical and |
| `<expr> or <expr>` | Logical or |
| `not <expr>` | Logical not |

Nesting cap: **8 levels**. The parser rejects deeper trees at
parse time with the offending line number. Refactor into smaller
flows before chasing that limit.

### 2.5 Loops

```sflow
loop <N> times
  <statements>
end

while <condition>
  <statements>
end

until <condition>
  <statements>
end

for <var> in <list_var>
  <statements>
end
```

- `loop N times` runs the body 0..N-1 times. `${loop.iter}` is
  the 0-indexed iteration counter, available inside the body.
- `while` runs the body while the condition is true.
- `until` is sugar for `while not <condition>`.
- `for x in list_var` (F9) iterates over a list variable, binding each
  element to `x` in the body. The loop variable is restored to its
  prior binding (or removed if it was unset before the loop) after
  `end`. `${loop.iter}` is also available inside a `for` body.

**Iteration cap: 100 per loop** (configurable by the operator;
wired by the flow runner via `Executor::with_max_loop_iters`). When
hit, the executor writes `sol.loop_limit_hit` to the chronicle and
breaks out — never crashes, never hangs.

### 2.6 Error handling

```sflow
set prompt = "session|hello"

try
  step reply: ai.chat "${prompt}"
catch timeout
  sol.set_result "Taking too long. Please retry."
catch any
  sol.set_result "Something went wrong. Please retry."
end
```

`prompt` is set up-front so the snippet is self-contained — the
arg `${prompt}` interpolates to a valid SIMP-016 string
(`session_id|prompt[|history]`). A host-rendered template can
overwrite this seed before execution.

Error kinds:

| Kind | Triggers on |
|---|---|
| `timeout` | `TIMEOUT` / `APPROVAL_TIMEOUT` |
| `mesh_error` | `TRANSPORT` / `PEER_UNREACHABLE` / local dispatch errors |
| `policy_denied` | `POLICY_DENIED` / `APPROVAL_DENIED` / `APPROVAL_REQUIRED` |
| `responder_error` | Application errors from the responder (`RESPONDER_INTERNAL`, `INVALID_ARGS`, …) |
| `any` | Catches anything not handled above |

Catch ordering: the first matching `catch` runs; if none match
but `catch any` is present, that runs. Otherwise the error
propagates to the next enclosing `try` or fails the flow.

Inside a catch block, `${error.kind}` and `${error.message}` are
available as interpolations. `rethrow` re-raises the current
error to the next outer handler; at the top level, the flow
fails with the captured cause.

A `remote_call` failure outside any `try` block aborts the flow
and the host writes `failure_class = sol_uncaught_error` on the
task ledger.

### 2.7 Built-in steps

```sflow
sol.log "message ${var.x}"         // chronicle event sol.log
sol.sleep <seconds>                // clamped to 30s max
sol.assert <condition>             // fails flow if condition false
sol.set_result "value"             // sets the running flow result
sol.set_result var.name
```

None of these dispatch to the mesh. `sol.set_result` sets the
result that `return` (without a value) and the falls-off-the-end
case both return.

### 2.8 Return

```sflow
return                  // exit with current sol.set_result / last result
return "literal"        // exit with this string
return var.my_var       // exit with variable value
return step.x.result    // exit with a named step's result
```

### 2.9 Interpolation placeholders

Any string literal can carry `${…}` placeholders that the
executor expands at run time:

| Placeholder | Expands to |
|---|---|
| `${result}` | Last step's result |
| `${status}` | Last step's status (`completed` / `failed` / empty) |
| `${var.name}` | Variable `name`, or empty if unset |
| `${step.name.result}` | Named step's result |
| `${step.name.status}` | Named step's status |
| `${loop.iter}` | 0-indexed iteration (loop body only) |
| `${error.kind}` | Error kind, inside a catch block |
| `${error.message}` | Error cause, inside a catch block |

Unknown placeholders expand to the empty string. Unmatched `$`
characters pass through verbatim.

---

## 3. Choosing between SOL and Sflow

Use **SOL** when:
- You're extending the existing chat flows (`chat_template.sol`,
  `chat_with_tool.sol`). The bridge already renders them.
- You want typed locals, arrays, or struct-style data shaping.
- The flow has no recoverable failure modes — a failed
  `remote_call` *should* abort the flow.

Use **Sflow** when:
- You want try/catch / rethrow error recovery.
- The flow has conditional routing on a step's outcome
  (`if step.x.status == "completed"`).
- You need bounded iteration (`loop 5 times`).
- The flow is operator-authored and you want the parser to
  reject deep nesting / too-long variable names / unclosed
  blocks at validate time.

Both languages call the same `remote_call` pipeline, so a flow
can be translated either way without changing the responders.

---

## 4. Chronicle events

The executor (Sflow) and dispatcher (both languages) write
events to the per-flow event log on disk. SOL emits the canonical
`RemoteCallIssued` / `RemoteCallCompleted` / `RemoteCallFailed`
records. Sflow additionally emits these structured events,
multiplexed under `RemoteCallIssued` records with an
`event=<name>` payload prefix (the on-disk record format stays
signed and hash-chained — adding new `EventType` variants is a
Gate 2 concern).

| Event | When written |
|---|---|
| `sol.step_start` | Before each capability step is dispatched |
| `sol.step_done` | After each capability step (success or failure) |
| `sol.loop_iter` | At the start of each iteration of `loop` / `while` / `until` |
| `sol.condition_branch` | When an `if` / `elif` / `else` branch is taken (or `none` if no branch matched) |
| `sol.error_caught` | When a `catch` block fires |
| `sol.loop_limit_hit` | When a loop hits the iteration cap |
| `sol.log` | From the `sol.log` built-in |
| `sol.flow_failed` | When an uncaught error aborts the flow |

`relix-flow-inspect` prints these as UTF-8 payloads against the
canonical event records — no new tooling required.

---

## 5. Worked examples

> **SIMP-016 reminder:** all `ai.chat` calls must pass args as
> `session_id|prompt[|history]`. A bare prompt string is
> rejected by the responder with `INVALID_ARGS` (error kind 5,
> classified as `responder_error` for catch purposes). The
> examples below use distinct session ids per flow so they don't
> step on each other's memory traces.

### 5.1 `flows/chat_with_retry.sflow`

Chat with a fallback message when the AI peer times out or
fails for any other reason:

```sflow
set prompt = "sflow-retry-demo|hello"

try
  step reply: ai.chat "${prompt}"
  return step.reply.result
catch timeout
  sol.set_result "Taking too long. Please retry."
catch any
  sol.set_result "Something went wrong. Please retry."
end
```

The `set prompt = …` seed makes the flow runnable standalone via
`relix-cli flow-run`. When the bridge grows a `.sflow` template
renderer it will replace this line (or override the variable
before the try block) with the operator-supplied value.

### 5.2 `flows/conditional_demo.sflow`

Branch on a named step's status:

```sflow
step check: ai.chat "sflow-conditional-demo|ping"
if step.check.status == "completed"
  sol.set_result "AI is online"
else
  sol.set_result "AI is unavailable"
end
return
```

`return` with no value returns the current `sol.set_result`
value — the flow exits with `"AI is online"` when the AI peer
answers the ping, `"AI is unavailable"` otherwise.

### 5.3 `flows/loop_demo.sflow`

`loop N times` with `${loop.iter}`:

```sflow
set counter = "0"
loop 3 times
  sol.log "iteration ${loop.iter}"
end
sol.set_result "done"
return
```

No mesh calls — useful for exercising the executor + chronicle
event path in isolation. Each iteration writes one `sol.loop_iter`
event followed by one `sol.log` event; the flow returns `"done"`.

---

## 6. Limits

| Cap | Where | Value |
|---|---|---|
| Variables per execution | Sflow | 50 |
| Loop iterations per block | Sflow | 100 (configurable) |
| Block nesting depth | Sflow | 8 |
| Identifier length | Sflow | 32 chars |
| `sol.sleep` seconds | Sflow | 30 (hard clamp) |
| String literal escape sequences | Both | none (SIMP-016) |

Hitting any of these aborts the flow with a structured error
rather than misbehaving silently.
