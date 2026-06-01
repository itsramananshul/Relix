# YAML Flow Reference

Relix flows can be written in either SOL or YAML. YAML is the
operator-facing alternative — it has no curly braces, no
semicolons, and no type system to fight. Under the hood the YAML
frontend lowers to SOL source text and runs through the same
compile pipeline, so YAML flows execute on the exact same VM,
event log, and dispatcher as SOL flows.

For the underlying SOL language reference (every keyword, every
operator, every built-in) see
[`sol-language-reference.md`](sol-language-reference.md). For the
operator's tutorial covering both languages, see
[`sol.md`](sol.md).

## Minimum viable flow

The smallest YAML flow that calls a peer and returns its
response:

```yaml
steps:
  - call:
      peer: ai
      method: ai.chat
      arg: "demo|hello|"
      assign: reply
  - result: "{{reply}}"
```

Three things to know:

1. The file is a single top-level `steps:` key holding a list
   of steps.
2. Each step is a one-key map; the key names the step type.
3. `{{name}}` interpolates a variable into a string literal.

That is the whole format. Everything below this paragraph is
detail.

## Steps

### `let` — declare a local variable

```yaml
- let:
    name: session
    type: str
    value: "demo-session"
```

Fields:

| Field | Required | Notes |
|---|---|---|
| `name` | yes | SOL identifier (letters / digits / underscore; must start with a letter or underscore). |
| `type` | yes | One of: `int`, `str`, `bool`, `float`, `list`, `map`. |
| `value` | yes | Initial value. Shape depends on the declared type — see below. |

#### Scalar values (`int` / `str` / `bool` / `float`)

For scalar types, `value` is a scalar (string, number, or
bool). The YAML form is emitted as a SOL literal of the
matching shape:

```yaml
- let: { name: greeting, type: str, value: "hello" }
- let: { name: count, type: int, value: 5 }
- let: { name: ok, type: bool, value: true }
```

A scalar value with a shape that doesn't match the declared
type is a clear schema error — passing a sequence for
`type: str` reports:

```
at line 4, column 14 (step 1): let.value is a YAML sequence
but let.type is `str` — use `type: list` for sequence values
```

#### Native list and map literals

For `type: list`, `value` can be a native YAML sequence:

```yaml
- let:
    name: items
    type: list
    value:
      - alpha
      - beta
      - gamma
```

Nested lists work too:

```yaml
- let:
    name: pairs
    type: list
    value:
      - - a
        - b
      - - c
        - d
```

For `type: map`, `value` can be a native YAML mapping:

```yaml
- let:
    name: config
    type: map
    value:
      model: gpt-4o
      temp: "0.2"
```

Nested maps work too:

```yaml
- let:
    name: tree
    type: map
    value:
      outer:
        inner_k: v
      other:
        another: "1"
```

The lowerer recursively translates the YAML structure into
SOL literal syntax — the example above emits
`tree = {"outer": {"inner_k": "v"}, "other": {"another": "1"}};`.
String map keys are required (YAML allows non-string keys
via implicit typing; SOL's map literal only accepts string
keys).

A legacy escape hatch is preserved for backwards
compatibility: a `string` value for `type: list` / `type: map`
is treated as a literal SOL list / map and emitted verbatim
(e.g. `value: '["a", "b", "c"]'`). Operators authoring new
flows should use the native YAML syntax instead.

Multiple `let` steps with the same name and same type are
allowed — they read as overwriting the variable.

### `call` — unary remote call

```yaml
- call:
    peer: memory
    method: memory.write_turn
    arg: "demo|user|hello"
```

Fields:

| Field | Required | Notes |
|---|---|---|
| `peer` | yes | Peer alias from `peers.toml`. |
| `method` | yes | Capability method name (`memory.search`, `ai.chat`, ...). |
| `arg` | yes | Argument bytes (UTF-8). Supports interpolation. |
| `assign` | no | When set, the response is bound to this variable. Without it the response is discarded. |

`peer`, `method`, and `arg` all go through SOL string
interpolation, so `peer: capability:{{x}}` works the same way
a SOL literal would.

### `stream` — streaming remote call

Same shape as `call` but invokes the streaming dispatcher:

```yaml
- stream:
    peer: ai
    method: ai.chat.stream
    arg: "demo|hello|"
    assign: reply
```

From the YAML author's perspective `stream:` is equivalent to
`call:` — both produce a single result. The streaming benefit is
external: when the host wires a chunk observer (the web bridge
does for HTTP SSE), each chunk fires the observer as it arrives,
before the VM has finished collecting.

### `result` — set the flow result

```yaml
- result: "{{reply}}"
```

Lowers to `return value;`. A flow with no `result:` step returns
the empty string.

### `print` — write to stdout

```yaml
- print: "now running"
```

Lowers to `print(value);`. Useful for `relix-cli flow-run`
debugging; the bridge does not capture stdout.

### `if` — conditional branching

```yaml
- if:
    condition: status == "completed"
    then:
      - result: "done"
    else:
      - result: "pending"
```

Fields:

| Field | Required | Notes |
|---|---|---|
| `condition` | yes | A SOL boolean expression. See [`sol-language-reference.md`](sol-language-reference.md) §4.2 for the operators that produce `bool`. |
| `then` | yes | List of steps run when the condition is true. May be empty. |
| `else` | no | List of steps run when the condition is false. Defaults to no else branch. |

The condition is emitted verbatim into the lowered SOL between
`if` and `{`. The SOL analyzer enforces that it type-checks as
`bool`.

### `loop` — bounded iteration

Two shapes — counted and for-each. Exactly one must be set.

**Counted loop**:

```yaml
- loop:
    times: 5
    steps:
      - print: "tick"
```

`times` is the number of iterations. Lowers to a synthesised
counter + `while`; the counter name is gensym'd so two counted
loops in the same flow do not collide.

**For-each loop**:

```yaml
- loop:
    for_each: x
    in: items
    steps:
      - print: "{{x}}"
```

`for_each` names the loop variable; `in` names a `list` variable
that must have been declared earlier (typically via a `let:`
with `type: list`). The loop variable is scoped to the body —
referencing it after the loop is not supported.

### `try` — error handling

The `catch` field accepts either a **single catch clause**
(shorthand for one-handler flows) or a **sequence of clauses**
(the multi-catch form). The lowered SOL emits one
`} catch <kind> { ... }` block per clause, in source order.

#### Single-catch shorthand

```yaml
- try:
    steps:
      - call:
          peer: ai
          method: ai.chat
          arg: "x"
          assign: reply
    catch:
      kind: any
      steps:
        - let:
            name: reply
            type: str
            value: "fallback"
```

#### Multi-catch with kind dispatch

```yaml
- try:
    steps:
      - call:
          peer: ai
          method: ai.chat
          arg: "{{session}}|{{message}}|"
          assign: reply
    catch:
      - kind: timeout
        steps:
          - let:
              name: reply
              type: str
              value: "timed out, try again"
      - kind: policy_denied
        steps:
          - let:
              name: reply
              type: str
              value: "not allowed"
      - kind: any
        steps:
          - let:
              name: reply
              type: str
              value: "error"
```

**First matching clause wins.** The classified kind of the
failure is compared against each `catch.kind` in source
order. `any` matches every failure unconditionally — put it
last (as a catch-all fallback) or omit it (so unmatched
failures propagate to an outer `try` or halt the VM).

Fields:

| Field | Required | Notes |
|---|---|---|
| `steps` | yes | Body to wrap. |
| `catch` | yes | A single mapping (single-catch shorthand) OR a sequence of mappings (multi-catch). At least one clause is required. |

`catch.kind` values match SOL exactly:

| Kind | Triggers on |
|---|---|
| `any` | every failure regardless of classification |
| `timeout` | `TIMEOUT`, `APPROVAL_TIMEOUT` |
| `mesh_error` | `TRANSPORT`, `PEER_UNREACHABLE`, dispatcher-local failures |
| `policy_denied` | `POLICY_DENIED`, `APPROVAL_DENIED`, `APPROVAL_REQUIRED` |
| `responder_error` | application errors from the responder |

Failures that route to a `try` handler: `call` / `stream`
failures, `list_get_list` / `map_get_map` runtime errors. Other
VM integrity faults (stack underflow, bad heap reference) halt the
VM with `VM_ERROR_SENTINEL` but do not panic the host process.

## Variable scoping

YAML hides SOL's lexical scoping: every variable introduced by a
`let` step or a `call.assign` / `stream.assign` field is
**hoisted** to the outermost function scope on a pre-pass, with
the canonical zero value for its declared type
(`""`, `0`, `false`, `0.0`, `[]`, `{}`).

This means a variable assigned inside a `try` / `catch` / `if` /
`loop` body is visible to later steps outside the block:

```yaml
- try:
    steps:
      - call: { peer: ai, method: ai.chat, arg: "x", assign: reply }
    catch:
      kind: any
      steps:
        - let: { name: reply, type: str, value: "fallback" }
- result: "{{reply}}"          # reads reply, regardless of which branch ran
```

The downside: hoisting means `let` is never strictly a fresh
declaration in YAML. Two `let` steps with the same name reuse
the same hoisted variable.

Conflicting types for the same name (a `let x: int` and a
`let x: str`) surface as a schema error before any code runs.

## String interpolation

`{{name}}` inside any string value resolves to the variable
`name`. Whitespace inside the braces is trimmed. An empty
marker (`{{}}`) or unterminated marker (`{{ no closer`) is
preserved verbatim so a typo is visible.

Markers reference *flow variables*, not raw environment values.
For templates rendered by the bridge (e.g.
`flows/chat_template.yml`), the operator's `{{SESSION}}` and
`{{MESSAGE}}` are substituted by the bridge *before* the flow
runs — those are render-time markers, not SOL interpolations.

## Limitations

The format is intentionally a thin layer over SOL. Some
limitations are deliberate:

- **No arbitrary expressions in `value` or `arg`**. Both are
  string templates; interpolation is the only expression-like
  feature. For arithmetic or builtin calls, lower to SOL or
  perform the work on the responder.
- **No `break` or `continue`**. SOL does not have them.
- **No `else if`**. Nest a fresh `if` inside the `else`
  branch.
- **No first-class functions**. Top-level helper functions
  cannot be declared in YAML; if you need them, use SOL.
- **No iteration cap**. Unlike Sflow, SOL has no built-in
  bound on counted or while loops; a runaway `loop` with a
  large `times` runs until the host kills it.

These are not bugs; they are the trade-off for keeping the
runtime a pure SOL VM.

## File and nesting limits

| Limit | Value | When checked |
|---|---|---|
| Max YAML file size | 10 MiB | Before YAML parsing (stat + re-check after read) |
| Max nesting depth | 20 | Before lowering, via bounded traversal |

Files exceeding 10 MiB produce `YamlFlowError::FileTooLarge`.
Flows exceeding depth 20 produce `YamlFlowError::NestingTooDeep`.
Both checks happen before any SOL compilation.

## Security: injection allowlists

Every user-supplied value that would be interpolated verbatim into
the emitted SOL source is validated against a strict regex allowlist
before emission. Values that fail validation produce
`YamlFlowError::InvalidCondition` or `YamlFlowError::InvalidScalar`
rather than a compile error at the SOL layer — operators see the
violation at the YAML step that caused it.

| Field | Allowed characters |
|---|---|
| `if.condition` | `A-Za-z0-9_.\s()!=<>&\|` — SOL boolean predicate chars only |
| `let.value` when `type: int` | `-?[0-9]+` |
| `let.value` when `type: float` | `-?[0-9]+(\.[0-9]+)?` |
| `let.value` when `type: bool` | `true` or `false` only |
| `let.value` when `type: list`/`map` (string form) | `[\[\]{}:,\s"A-Za-z0-9_.\-]+` |

Additionally, any string value containing a `"` character is rejected
with a Semantic error (SOL has no string escape sequences — SIMP-016).

## Errors

Four categories, each surfaced with an actionable message
and the exact source position of the offending node:

| Error | Trigger | Locator |
|---|---|---|
| `YamlFlowError::Parse` | YAML itself is malformed (unbalanced bracket, bad indentation, missing colon). | Line and column from `saphyr`. |
| `YamlFlowError::Semantic` | YAML parses but violates the schema — unknown step name, missing required field, conflicting variable types, `catch.kind` outside the recognised set, `let.type` outside the supported scalar set, value shape mismatch, etc. | Real line + column of the offending node, from the saphyr-annotated tree. **Nested errors report the nested node's line, not the outer step.** |
| `YamlFlowError::Lower` | The YAML frontend emitted SOL the compiler rejected. This is a frontend bug; the error includes the SOL error, the lowered source, AND the path of the last successfully-lowered step so the bug can be reproduced. | Step path of the last lowered step. |
| `YamlFlowError::Io` | File read failure (only via `compile_path`). Carries the file path the bridge / CLI tried to open. | n/a (file-level). |
| `YamlFlowError::InvalidCondition` | `if.condition` contains characters outside the allowlist. | Offending step node. |
| `YamlFlowError::InvalidScalar` | A scalar `value` (int/float/bool/collection) contains characters outside its allowlist, or a string contains `"`. | Offending `let` node. |
| `YamlFlowError::FileTooLarge` | File exceeds 10 MiB. | File-level. |
| `YamlFlowError::NestingTooDeep` | Flow nesting depth exceeds 20. | Offending node. |

A typical Semantic error message:

```
at line 14, column 9 (step 2 → catch.step 1): missing required field `value`
```

Line and column are 1-based and point at the offending YAML
node — for a nested step inside `try → catch → steps`, the
line is the line of that nested step's dash, not the outer
`try`'s line.

`relix-cli flow-run my-flow.yml` and the bridge's
`POST /v1/yaml/validate` both surface these errors with the
locator on the first line of the message.

## The `POST /v1/yaml/validate` endpoint

The bridge exposes a parse-only validator for YAML flows.
The dashboard editor calls it to surface inline errors
before a flow is deployed.

Request:

```http
POST /v1/yaml/validate
Content-Type: application/json

{ "source": "<yaml flow text>" }
```

Successful response (HTTP 200):

```json
{ "status": "ok" }
```

Error response (HTTP 400):

```json
{
  "status": "error",
  "message": "at line 14, column 9 (step 2 → catch.step 1): missing required field `value`",
  "line": 14,
  "column": 9
}
```

`message` is the full `YamlFlowError` Display rendering;
`line` / `column` come from the underlying error variant.
Both fields are omitted when zero, so a `Lower` or `Io`
error returns `status: error` with `message` only.

Curl example:

```sh
curl -s -X POST http://127.0.0.1:9100/v1/yaml/validate \
  -H 'Content-Type: application/json' \
  -d '{"source":"steps:\n  - let:\n      name: x\n      type: str\n"}' \
| jq
# {
#   "status": "error",
#   "message": "at line 2, column 5 (step 1): missing required field `value`",
#   "line": 2,
#   "column": 5
# }
```

The dashboard validator panel auto-routes between
`/v1/sol/validate` and `/v1/yaml/validate` based on the
language dropdown — and, as a convenience, when the editor
content starts with `steps:` it switches to the YAML route
even if the dropdown is left on the default.

## The `relix flow yaml` CLI scaffold

```
relix-cli flow yaml [--template <name>]
```

Prints a minimal working YAML flow template to stdout.
Pipe into a file and edit. Four templates ship:

### `chat` (default)

The simplest case — a single `remote_call` returning the
reply. Used when `--template` is omitted.

```
$ relix-cli flow yaml > my.yml
$ head -20 my.yml
# Minimal Relix YAML flow — chat scaffold.
#
# Pipe into a file: relix-cli flow yaml > my.yml
# Run:               relix-cli flow-run --flow my.yml ...
# Full reference:    docs/yaml-flow-reference.md

steps:
  - let:
      name: session
      type: str
      value: "demo-session"
  - let:
      name: message
      type: str
      value: "hello"

  - call:
      peer: ai
      method: ai.chat
      arg: "{{session}}|{{message}}|"
      assign: reply

  - result: "{{reply}}"
```

### `--template stream`

Same shape as `chat` but uses the `stream:` step so the
host's chunk observer (e.g. the bridge's SSE response)
gets per-chunk callbacks while the VM is still running.

### `--template try`

Wraps the call in a multi-catch with `timeout` /
`policy_denied` / `any` clauses so the error-handling
pattern is right there to edit:

```yaml
  - try:
      steps:
        - call:
            peer: ai
            method: ai.chat
            arg: "{{session}}|{{message}}|"
            assign: reply
      catch:
        - kind: timeout
          steps:
            - let: { name: reply, type: str, value: "timed out, try again" }
        - kind: policy_denied
          steps:
            - let: { name: reply, type: str, value: "not allowed" }
        - kind: any
          steps:
            - let: { name: reply, type: str, value: "error" }
```

### `--template loop`

Counted loop calling a peer N times:

```yaml
  - loop:
      times: 3
      steps:
        - call:
            peer: ai
            method: ai.chat
            arg: "demo|tick|"
            assign: reply
```

All four templates compile through `compile_source` without
error, so a developer copying any of them sees a working
flow on the first run.

## Worked example — chat with retry

```yaml
steps:
  - let:
      name: session
      type: str
      value: "demo"
  - let:
      name: prompt
      type: str
      value: "hello"

  - try:
      steps:
        - stream:
            peer: ai
            method: ai.chat.stream
            arg: "{{session}}|{{prompt}}|"
            assign: reply
      catch:
        kind: timeout
        steps:
          - let:
              name: reply
              type: str
              value: "Taking too long. Please retry."
      # second catch would go here in SOL; for YAML, use `any`
      # plus dispatch on error_kind inside the body.

  - result: "{{reply}}"
```

When the AI peer answers, `reply` is bound to the response. When
it times out, the catch's `let` sets a fallback. Either way, the
final `result` step returns whatever `reply` ended up as.

## How to choose between YAML and SOL

Use **YAML** when:

- The flow is operator-authored and you want a forgiving
  syntax.
- The orchestration is linear with a small amount of error
  handling.
- You want the bridge to render a chat template against
  `{{SESSION}}` / `{{MESSAGE}}` markers without an operator
  needing to learn SOL syntax.

Use **SOL** when:

- You need multiple `catch` clauses on one `try`.
- You need a helper function or recursion.
- You want explicit lexical scoping (no hoisting).
- The flow exercises advanced SOL features (struct field
  access, custom expression-position operators, etc.).

Both languages share the runtime, the event log, the
dispatcher, the manifest cache, and the bridge wiring. Pick
whichever fits the use case — there is no "wrong" answer.

## See also

- [`sol-language-reference.md`](sol-language-reference.md) — SOL syntax and semantics.
- [`sol.md`](sol.md) — operator-facing tutorial covering both languages.
- [`sol-sflow-parity.md`](sol-sflow-parity.md) — comparison of SOL and Sflow.
- `crates/relix-runtime/src/yaml_flow/` — the YAML frontend implementation.
- `flows/chat_template.yml` and `flows/chat_template_streaming.yml` — shipped operator-authored chat templates.
