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
| `value` | yes | Initial value as a string. For `str` it is emitted as a quoted SOL string literal (so `{{name}}` interpolation works). For other scalar types the string is emitted verbatim — write `5` for an int, `true` for a bool, `["a", "b"]` for a list. |

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

Fields:

| Field | Required | Notes |
|---|---|---|
| `steps` | yes | Body to wrap. |
| `catch` | yes | Catch clause (`kind` + `steps`). At least one catch is required. |

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
SOL errors (stack underflow, etc.) are bugs and panic the host.

The YAML format currently supports a single `catch` per `try`.
Flows that need multiple kinds either dispatch inside the catch
on `{{error_kind}}` (via a string comparison) or drop to SOL.

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
- **Single catch per try**. Multi-catch flows drop to SOL.
- **List / map literals are SOL syntax**. A `let` with
  `type: list` takes its `value` as `'["a", "b", "c"]'`
  (SOL list syntax embedded in the YAML string). Native YAML
  sequences as `value` are not yet supported.
- **No iteration cap**. Unlike Sflow, SOL has no built-in
  bound on counted or while loops; a runaway `loop` with a
  large `times` runs until the host kills it.

These are not bugs; they are the trade-off for keeping the
runtime a pure SOL VM.

## Errors

Three categories, each surfaced with an actionable message:

| Error | Trigger | Locator |
|---|---|---|
| `YamlFlowError::Parse` | YAML itself is malformed (unbalanced bracket, bad indentation, missing colon). | Line and column from `serde_yaml`. |
| `YamlFlowError::Semantic` | YAML parses but violates the schema — unknown step name, missing required field, conflicting variable types, `catch.kind` outside the recognised set, `let.type` outside the supported scalar set, etc. | 1-based step path (`step 2 → catch.step 1`). |
| `YamlFlowError::Lower` | The YAML frontend emitted SOL the compiler rejected. This is a frontend bug; the error includes the SOL error AND the lowered source so the issue can be reproduced from the failure. | n/a — this is for developers, not operators. |

`relix-cli flow-run my-flow.yml` and the bridge's
`POST /v1/sol/validate` both surface these errors with the
locator on the first line of the message.

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
