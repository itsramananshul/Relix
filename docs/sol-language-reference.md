# SOL Language Reference

This document describes the SOL syntax accepted by the compiler at
`crates/relix-runtime/src/sol/` and the runtime semantics of the VM at the
same path. It is a *reference*, not a tutorial.

Every example in this document is compiled and executed by the test at
`crates/relix-runtime/src/sol/language_reference_examples.rs`. If the test
fails, either the example or the doc is wrong; the test wins.

For the operator-facing tutorial covering both SOL and Sflow, see
[`sol.md`](sol.md). For the side-by-side parity table comparing SOL with
Sflow, see [`sol-sflow-parity.md`](sol-sflow-parity.md). For the M6
integration analysis (VM stack model, heap object layout, dispatcher
contract) see [`sol-runtime-analysis.md`](sol-runtime-analysis.md).

---

## 1. Lexical structure

### 1.1 Keywords

These identifiers are reserved by the lexer:

```
for in as function if else import while struct enum let return true false try catch rethrow
```

`delegate`, `send`, `goal`, `subject`, `body`, `from`, `to` are soft
keywords — recognised only in the positions they introduce sugar (`§11`,
`§12`); valid identifiers everywhere else.

### 1.2 Identifiers

An identifier is a non-empty sequence of `[A-Za-z0-9_]` characters whose
first character is alphabetic. Outside an identifier, the lexer treats `_`
as whitespace, so source like `let _x: int = 1;` tokenises as
`let x : int = 1 ;` — the leading underscore disappears. To use an
identifier starting with what looks like an underscore, start it with a
letter instead.

### 1.3 Literals

| Form | Token | Notes |
|---|---|---|
| `42` | Integer (`i128` internally) | Decimal only. No leading sign — `-1` is a unary expression. |
| `3.14` | Float (`f64`) | Requires both digits and a fractional part; a trailing `.` falls back to integer. |
| `"text"` | String | **No escape sequences** (SIMP-016). A literal `"` ends the string; you cannot include one. |
| `'x'` | Char | Single character between single quotes. |
| `true` / `false` | Bool | Keywords. |
| `[a, b, c]` | List literal (`§6.4`) | |
| `{ "k": v, ... }` | Map literal (`§6.5`) | Allowed only in expression positions where struct initialisers are also allowed. |

### 1.4 Comments

```sol
// line comment to end of line

/* block comment, may
   span multiple lines */
```

### 1.5 Whitespace

Spaces, tabs, newlines, and bare underscores between tokens are
discarded.

---

## 2. Types

The analyzer tracks the following types:

| Source spelling | `Type` variant | Storage | Notes |
|---|---|---|---|
| `int` | `Type::Integer` | one `u64` on the stack, interpreted as `i64` | 64-bit signed at the VM layer even though literals parse as `i128`. |
| `float` | `Type::Float` | one `u64` on the stack, interpreted as `f64::from_bits` | |
| `bool` | `Type::Bool` | one `u64` on the stack: exactly `0` or `1` | The VM's `LogAnd` / `LogOr` compare against literal `1`; non-canonical truthy values from bitwise ops do *not* work as booleans. |
| `char` | `Type::Char` | one `u64` on the stack, interpreted as a Unicode scalar | |
| `str` | `Type::String` | a `u64` heap reference to `HeapObject::String` | Strings live on the heap; the stack carries only the index. |
| `list` | `Type::List` | a `u64` heap reference to `HeapObject::List(Vec<u64>)` | Heterogeneous — elements are raw heap refs. Builtins (`list_get`, `list_join`) interpret elements as heap strings. |
| `map` | `Type::Map` | a `u64` heap reference to `HeapObject::Map(Vec<(String, u64)>)` | String-keyed. Insertion order is preserved. |
| `[N]T` | `Type::Array { size, inner }` | a `u64` heap reference to `HeapObject::Array` | The verbatim OpenPrem array type. **No source syntax constructs an `Array` today** — the `[...]` literal always produces a `list`. Typed arrays are kept as a `Type` variant for the port but are not constructable from SOL source. |
| `(T1, T2, ...)` | `Type::Tuple(...)` | n/a | Parsed in type-annotation positions only. **No tuple expression syntax exists** — there is no way to construct or destructure a tuple in current SOL. The variant is parsed for the verbatim port; using it in a `let` is a dead end. |
| `StructName` | `Type::Ident(String)` | a `u64` heap reference to `HeapObject::Struct` | See `§3.3`. |
| Function | `Type::Function { params, ret }` | n/a — synthesised by the analyzer for function symbols | Not a first-class value; you cannot assign a function to a `let`. |
| (none) | `Type::Void` | n/a | Implicit return type for `function name() { }`. |

The type checker treats `list` and `map` as scalar nominal types — there
is no element-type parameterisation. `let xs: list = ["a"];` and
`let ys: list = [1, 2];` are both well-typed.

---

## 3. Top-level declarations

A SOL file is a sequence of top-level declarations:

```
declaration ::= function_decl | var_decl | struct_decl | enum_decl | import_stmt
```

The codegen appends a final `Call(start, 0)` at the bottom of the
emitted bytecode, so a SOL program enters at `function start() -> ...`.
A file without a `start` function compiles but executes no user code.

### 3.1 Functions

```sol
function name(p1: T1, p2: T2) -> ReturnType {
    // body
}
```

- Parameter names are alphabetic identifiers; each parameter declares its
  own type.
- The `-> ReturnType` clause is optional; omitting it makes the return
  type `Type::Void`.
- Top-level function names are registered in pass 1, then bodies are
  type-checked in pass 2. Forward references between top-level functions
  are legal:

  ```sol
  function start() -> int {
      return helper();        // helper is defined below — fine
  }

  function helper() -> int {
      return 7;
  }
  ```

- The body is a block (`§5`). `return expr;` exits the function with
  `expr`'s value; bare `return;` exits with no value (use only in `-> Void`
  functions or as a no-value early exit).
- Recursion is allowed (the function is a top-level symbol before its
  body is analyzed). There is no tail-call optimisation; deep recursion
  consumes host stack.

### 3.2 Variables

```sol
let name: type;             // declare without initialiser
let name: type = expr;      // declare with initialiser
```

- The annotation is **mandatory**. There is no type inference at `let`.
- The analyzer checks `type_eq(annotation, expr_type)`.
- Redefining a name in the same scope is a hard error (the analyzer
  panics, the compile pipeline surfaces it as `Err("sol parse: …")`).

### 3.3 Structs

```sol
struct Point {
    x: int,
    y: int,
}

function start() -> int {
    let p: Point = Point { x: 3, y: 4 };
    return p.x;
}
```

- Fields are alphabetical-ordered by the codegen (`struct_layouts.sort_by`),
  so field indices are stable regardless of source order.
- Member access: `p.field`.
- Field assignment: `p.field = expr` is permitted at parse time but the
  analyzer's `ExprAssign` path goes through `ExprMemAcc`-aware codegen
  (see `bytecode.rs::compile`).

### 3.4 Enums

```sol
enum Status {
    Pending,
    Running = 10,
    Done,
}
```

- Each variant gets an `isize` value: implicit `0`, then `+1` for each
  next variant, unless an explicit `= N` override resets the counter.
  In the example above: `Pending=0`, `Running=10`, `Done=11`.
- A variant is referenced as `Status::Running`.
- At the VM layer, variants compile to a fixed integer hash
  (`var.chars().next() % 10`) — the runtime encoding is degenerate and
  enum *equality* is not implemented. Enums today are essentially a
  parse-time-only feature; do not branch on a variant value.

### 3.5 Imports

```sol
import some.module.path;
import some.module.path as alias;
```

- Imports are parsed and recorded in the symbol table when an alias is
  given.
- No module resolution or loading is performed. The bridge does not ship
  an importable standard library. `import` is, in practice, a no-op
  preserved from the verbatim port.

---

## 4. Expressions

The parser uses precedence-climbing. From lowest to highest precedence:

| Precedence (low→high) | Operators | Associativity |
|---|---|---|
| 1 | `=` (assignment) | right |
| 2 | `\|\|` | left |
| 3 | `&&` | left |
| 4 | `\|` (bitwise OR) | left |
| 5 | `^` (bitwise XOR) | left |
| 6 | `&` (bitwise AND) | left |
| 7 | `==` `!=` | left |
| 8 | `<` `<=` `>` `>=` | left |
| 9 | `<<` `>>` | left |
| 10 | `+` `-` | left |
| 11 | `*` `/` | left |
| 12 | unary `!` `-` `~` | (prefix) |
| 13 | postfix `.` `[]` | (left) |

### 4.1 Arithmetic

`+ - * /` are typed: the analyzer requires *both* operands to be the
same numeric type (`int` or `float`). String `+` is concatenation
(`ConcatStr`). String `- * /` is a type error.

```sol
function start() -> int { return 2 + 3 * 4; }
// → 14 (multiplication binds tighter than addition)
```

```sol
function start() -> str { return "a" + "b" + "c"; }
// → "abc"
```

**Integer overflow** is wrapping at the host's `i64` boundary
(`(a + b) as u64` is what the VM does). No trap.

**Division by zero** on integers is a recoverable fault. `IntDiv` uses
`checked_div`; a zero divisor (or `i64::MIN / -1` overflow) records a
structured fault, halts the VM with `VM_ERROR_SENTINEL`, and sets
`last_error`. It does **not** panic the host process. Wrap the call in
`try { … } catch any { … }` to recover.

### 4.2 Comparison

`== != < <= > >=` require both operands to share a type and return
`bool`. Strings support `==` and `!=` only (via `EqStr`); `<` and friends
on strings will produce an `Inst::EqStr` in the wrong slot and yield an
undefined comparison — do not compare strings with `<` `<=` `>` `>=`.

```sol
function start() -> bool { return 5 > 3; }
// → true (1 on the stack)
```

### 4.3 Logical

`&&` `||` require both operands to be exactly `Type::Bool`. They are
**not short-circuiting** at the bytecode layer: both sides are
evaluated, then `LogAnd` / `LogOr` reduces. The VM's `LogAnd` /
`LogOr` compare against the literal `1`, so passing a non-canonical
truthy value (e.g. the result of a bitwise operation) yields `0`.
Use `!` to normalise if needed.

```sol
function start() -> bool { return true && false; }
// → false
```

```sol
function start() -> bool { return true || false; }
// → true
```

`!` flips a bool (and works on int/float — anything zero → 1, anything
non-zero → 0):

```sol
function start() -> bool { return !false; }
// → true
```

### 4.4 Bitwise

`& | ^ << >>` and the unary `~` require `int` operands and return
`int`. They operate on the full 64-bit value.

```sol
function start() -> int { return 12 & 10; }
// → 8 (0b1100 & 0b1010 = 0b1000)
```

### 4.5 Unary

| Operator | Operand types | Result |
|---|---|---|
| `-x` | `int`, `float` | negation |
| `!x` | `int`, `float`, `bool` | logical NOT — non-zero → 0, zero → 1 |
| `~x` | `int` | bitwise NOT |

### 4.6 Assignment

```sol
function start() -> int {
    let x: int = 1;
    x = 5;
    return x;
}
// → 5
```

Assignment is an expression — the RHS value is left on the stack after
the store (via `Dup` + `StoreLocal`). At the top of an expression
statement the trailing value is `Pop`'d, so the side effect is what
matters.

### 4.7 String interpolation

A string literal containing `{{name}}` is rewritten at parse time into
a `+`-concatenation of `ExprString` and `ExprVar(name)` chunks. The
generated code is byte-identical to writing the concat by hand.

Rules:

- `name` must match `[A-Za-z0-9_]+`. Whitespace inside the braces is
  trimmed: `{{  name  }}` is `{{name}}`.
- An unterminated `{{` (no closing `}}`) is preserved verbatim so an
  operator's typo is visible.
- An empty `{{}}` marker is preserved verbatim.
- A non-identifier inside the braces (e.g. `{{1+2}}`) is preserved
  verbatim. SOL does **not** support arbitrary-expression interpolation.

```sol
function start() -> str {
    let n: str = "world";
    return "hello {{n}}";
}
// → "hello world"
```

```sol
function start() -> str {
    return "literal {{}}";
}
// → "literal {{}}" (empty marker preserved)
```

### 4.8 List literal

```sol
let empty: list = [];
let xs: list = ["a", "b", "c"];
let mixed: list = [1, "two", true];   // heterogeneous; values are raw heap refs
let nested: list = [["a", "b"], ["c"]];
```

Compiles to `n` element-pushes followed by `Inst::PushList(n)`.

### 4.9 Map literal

```sol
let bare: map = {};
let m: map = { "model": "gpt-4o", "temp": "0.2" };
let nested: map = { "outer": { "inner_k": "v" } };
```

- Keys MUST be string literals (the parser rejects anything else).
- Values are arbitrary expressions.
- A map literal in the body of an `if` / `while` / `for` condition is
  parsed as an empty block instead of a map — the parser disables the
  map-literal alternative inside conditions (`can_struct = false`).
  Wrap a map literal in `()` to force expression parsing if you need
  one as a condition value:

  ```sol
  // OK: in let RHS.
  let m: map = {};

  // NOT OK: SOL parses the `{}` as the if body.
  // if some_bool { let m: map = {}; }
  ```

### 4.10 Member access and indexing

```sol
let v: int = p.x;          // struct field
let e: int = arr[i];       // array index (typed arrays only; not constructable in source)
```

There is no list / map indexing via `xs[i]` — use `list_get(xs, i)` and
`map_get(m, k)`. The `[]` postfix lowers to `Inst::GetElem`, which only
works on `HeapObject::Array`.

---

## 5. Blocks and scope

A block is `{ statement* }`. Each block introduces a fresh scope. Locals
declared inside a block are dropped at the closing `}`. Locals are
addressed by stack offsets relative to the current frame pointer.

A block is itself a statement, so `{ {} }` parses (an inner empty block
inside an outer block).

The analyzer tracks scopes via the type-table arena (`tt_arena`);
codegen mirrors the layout to assign stack slots.

---

## 6. Statements

### 6.1 if / else

```sol
if cond { body } else { body }
```

- `cond` MUST be `Type::Bool`. `if 1 { … }` is a compile error.
- The else clause is optional.
- `else if` is not a distinct grammar production — the parser reads
  the `else` then calls `block()`, which descends into another `if`
  statement when the next token is `if`. The chain works as expected:

  ```sol
  function start() -> int {
      let x: int = 2;
      if x == 1 { return 10; }
      else if x == 2 { return 20; }
      else { return 30; }
  }
  // → 20
  ```

### 6.2 while

```sol
while cond { body }
```

- `cond` MUST be `Type::Bool`.
- Re-evaluated each iteration. Standard `Jump(loop_start)` /
  `JumpFalse(end)` codegen.
- No `break` or `continue` keyword exists.
- No iteration cap — a runaway `while true { }` runs until the host
  kills the process. Sflow's 100/loop cap does **not** apply to SOL.

```sol
function start() -> int {
    let i: int = 0;
    let sum: int = 0;
    while i < 5 {
        sum = sum + i;
        i = i + 1;
    }
    return sum;
}
// → 10 (0+1+2+3+4)
```

### 6.3 for

```sol
for x in iterable { body }
```

- `iterable` must have type `Type::List` or `Type::Array`. Anything else
  is a compile error.
- For a `list`, the loop variable `x` is bound as `Type::String` (the
  practical case operators use). For an `Array`, `x` carries the array's
  inner type.
- Iteration order: index `0` to `length - 1`.
- The loop body sees a fresh scope; `x` is reset each iteration.

```sol
function start() -> str {
    let xs: list = ["a", "b", "c"];
    let acc: str = "";
    for x in xs {
        acc = acc + x;
    }
    return acc;
}
// → "abc"
```

### 6.4 return

```sol
return expr;
return;
```

`return expr;` pops `expr`'s value off the stack and returns it from the
enclosing function. `return;` returns Void (no value) — typical inside
a `Void` function or as an early exit.

### 6.5 Expression statements

Any expression followed by `;` is a statement. The trailing value is
popped:

```sol
function start() -> int {
    print(42);          // expression statement; print returns Void
    return 1;
}
```

---

## 7. Built-in functions

The compiler recognises these names specially (codegen emits a dedicated
opcode instead of `Inst::Call`). They are not declared in source.

### 7.1 print

```
print(value) -> Void
```

Writes `value` to stdout with a trailing newline. Dispatches on `value`'s
type:

| Argument type | Opcode | Format |
|---|---|---|
| `int`, `bool` | `PrintInt` | `i64` decimal (bool → `0` / `1`) |
| `float` | `PrintFloat` | Rust `{}` format for `f64` |
| `char` | `PrintChar` | Unicode scalar as a single character |
| `str` | `PrintString` | string body |
| anything else | `PrintInt` (fallback) | raw `u64` of stack slot |

Side-effect only. `print` always pushes `0` after the print so the stack
balance is preserved; an expression statement immediately pops it.

### 7.2 remote_call

```
remote_call(peer: str, method: str, arg: str) -> str
```

Dispatches a unary capability call. The VM pops `arg`, `method`, `peer`
(in that order) and invokes the host-attached
`Arc<dyn RemoteCallDispatcher>`. Returns the response body as a fresh
`HeapObject::String`.

On failure:

- The VM sets `last_error` to the `RemoteCallError`.
- If wrapped in a `try { ... }`, control jumps to the catch dispatch
  block (`§9`).
- If not wrapped, the VM halts with `VM_ERROR_SENTINEL` and `run()`
  returns `u64::MAX`. The host (`flow_runner`) reads `last_error()` for
  the cause.

If no dispatcher is attached to the VM, every `remote_call` halts the
VM with `last_error.cause == "no RemoteCallDispatcher attached to VM"`
and `kind == 0`.

### 7.3 remote_call_stream

```
remote_call_stream(peer: str, method: str, arg: str) -> str
```

Identical type signature and wire-format contract as `remote_call`. The
difference is at the host layer: the dispatcher's
`remote_call_stream` method opens a `/relix/rpc/stream/1` substream and
invokes the VM's chunk observer (attached via
`VM::with_chunk_observer`) once per Chunk frame as it arrives.

From the SOL author's perspective the call is still synchronous — the
opcode pushes a single concatenated body string. The streaming benefit
is purely about *when* an external observer (the web bridge's SSE
response) sees each chunk.

If the dispatcher has no streaming implementation, the default trait
method falls back to a single `remote_call` and reports the entire body
as one chunk.

### 7.4 error_kind, error_cause, error_retry_hint

```
error_kind() -> str
error_cause() -> str
error_retry_hint() -> int
```

These three zero-argument builtins read the VM's current `last_error`.
They are intended for use inside a catch block (`§9`); outside a catch,
they return empty / zero rather than panicking.

`error_kind()` returns one of the catch-kind labels SOL recognises,
derived from `last_error.kind` via:

| `last_error.kind` | `error_kind()` |
|---|---|
| `TIMEOUT`, `APPROVAL_TIMEOUT` | `"timeout"` |
| `TRANSPORT`, `PEER_UNREACHABLE`, `0` | `"mesh_error"` |
| `POLICY_DENIED`, `APPROVAL_DENIED`, `APPROVAL_REQUIRED` | `"policy_denied"` |
| anything else | `"responder_error"` |

`error_cause()` returns `last_error.cause` verbatim.

`error_retry_hint()` currently always returns `0` — the field is not
carried on `RemoteCallError`. Treat the value as advisory and do not
depend on a non-zero return today.

### 7.5 List builtins

All list builtins are pure — the original list is never mutated. `*_push`
returns a new list with the new tail; the original survives unchanged
for any callers that aliased the old reference.

| Builtin | Signature | Notes |
|---|---|---|
| `list_len(lst)` | `(list) -> int` | Length, `0` for empty list. Tolerates an `Array` ref. |
| `list_get(lst, idx)` | `(list, int) -> str` | Returns the element at `idx` as a heap-string. Out-of-bounds (negative or `>= len`) returns `""`. |
| `list_get_list(lst, idx)` | `(list, int) -> list` | Same shape as `list_get` but the element MUST resolve to a `HeapObject::List`. Missing index or non-list element halts the VM with `VM_ERROR_SENTINEL`; catchable via `try` (kind = `mesh_error`). |
| `list_push(lst, val)` | `(list, any) -> list` | New list with `val` appended. Original unchanged. |
| `list_contains(lst, val)` | `(list, any) -> bool` | Compares each element's stringified form against `val`'s stringified form (`heap_display`). Returns `true` / `false`. |
| `list_join(lst, sep)` | `(list, str) -> str` | Joins each element's stringified form with `sep`. Nested lists stringify as `\|`-separated; nested maps as `;`-separated `k=v` pairs. |
| `list_split(s, sep)` | `(str, str) -> list` | Standard `str::split` semantics. Empty input → one empty element. Empty separator → whole string as one element. |

### 7.6 Map builtins

| Builtin | Signature | Notes |
|---|---|---|
| `map_get(m, k)` | `(map, str) -> str` | Returns the value at `k`. Missing key returns `""`. The analyzer types the return as `str` even though the VM returns the raw heap ref — if the value is actually a heap list/map, downstream `list_*` / `map_*` calls work if you bind it to the right SOL type and bypass `map_get`. The canonical way to get a non-string value out of a map is `map_get_map`, `map_keys`, or `list_get_list`. |
| `map_get_map(m, k)` | `(map, str) -> map` | Same shape as `map_get` but the value MUST be a `HeapObject::Map`. Missing key or non-map value halts the VM with `VM_ERROR_SENTINEL`; catchable via `try`. |
| `map_set(m, k, v)` | `(map, str, any) -> map` | New map with `(k, v)` set (overwrites existing key). Original unchanged. |
| `map_has(m, k)` | `(map, str) -> bool` | `true` if `k` is present. |
| `map_keys(m)` | `(map) -> list` | Returns the keys as a fresh list of strings, in insertion order. |
| `map_len(m)` | `(map) -> int` | Number of pairs. |
| `map_del(m, k)` | `(map, str) -> map` | New map without `k`. Absent key is a no-op (returns a copy). |

### 7.7 last_confidence — RELIX-7.19

```
last_confidence() -> float
```

Zero-argument builtin that returns a `float` in `[0.0, 1.0]` carrying
the confidence score of the most recently completed `remote_call` in
this execution context. Returns `1.0` (neutral) before any `remote_call`
has been made.

The score is produced by the host's
[`ConfidenceScorer`](../crates/relix-runtime/src/confidence/scorer.rs)
after the dispatch bridge returns each capability response. Five
weighted sub-scores combine:

- `response_length` — empty body scores `0.0` immediately; short bodies
  scale up; the optimal band is roughly 10–500 tokens.
- `response_coherence` — bumps for a sentence-final punctuation
  ending, penalty when the trigram-uniqueness ratio dips below `0.5`.
- `provider_signal` — derived from `finish_reason` (`stop` → `1.0`,
  `length` → `0.55`, `content_filter` → `0.30`) and per-token
  `logprob` when the provider emits one.
- `error_rate_history` — `1.0 − rolling error rate` for the
  `(caller_agent, called_method)` pair.
- `latency_signal` — `1.0` when the response was faster than the
  configured `p95_latency_baseline_ms`; linearly tapers to `0.0` at
  `4× baseline`.

When the rolling error rate is at or above `0.5`, the final score is
multiplied by the configured `error_rate_discount` (default `0.5`) —
brittle providers get their scores halved.

Use it from a flow to react to low confidence directly:

```sol
let answer: str = remote_call("ai", "ai.chat", prompt);
if (last_confidence() < 0.4) {
    log("low confidence; escalating to premium model");
    answer = remote_call("ai-premium", "ai.chat", prompt);
}
return answer;
```

Reading `last_confidence()` is wait-free (a single atomic load) so
flows can sprinkle it without performance concern. The opcode is
[`Inst::LoadLastConfidence`](../crates/relix-runtime/src/sol/bytecode.rs);
the source-of-truth is either the VM-local `last_confidence` field
(set via `VM::set_last_confidence`) or a shared
[`LastConfidenceCell`](../crates/relix-runtime/src/confidence/cell.rs)
when one has been attached. The dispatcher integration in
`crates/relix-runtime/src/dispatch/mod.rs` updates the cell after
every scored dispatch.

---

## 8. Try / catch / rethrow

```sol
try {
    body
} catch <kind> {
    handler
} [catch <kind> {
    handler
}]*
```

- At least one `catch` clause is required.
- `<kind>` is a bare identifier; the parser does not validate it against
  a known set. The recognised classified kinds are `any`, `timeout`,
  `mesh_error`, `policy_denied`, `responder_error`. Any other kind name
  will never match a real failure but is otherwise a no-op.
- `any` matches every failure unconditionally, regardless of `last_error`.
- Multiple `catch` clauses are evaluated in source order; the first
  matching one runs. If none match (and there is no `any`), the VM
  emits a `Rethrow` after the last clause's check.

### 8.1 Errors that route to a try handler

These VM events route to the nearest enclosing `try` handler:

- `remote_call` / `remote_call_stream` failure (dispatcher returned `Err`).
- `list_get_list` with out-of-bounds index or non-list element.
- `map_get_map` with missing key or non-map value.
- `Rethrow` opcode (either an explicit `rethrow;` or a synthesised
  `Rethrow` after no catch matched).

Other VM integrity faults (stack underflow, bad heap reference,
out-of-bounds index, invalid `PushConst` payload, allocation-ceiling
breach) are caught internally by the VM's `pop()` / `raise_malformed`
path. They halt the VM with `VM_ERROR_SENTINEL` and a `last_error`
describing the fault. The worker does **not** panic on malformed
bytecode; only logic bugs inside the Rust VM implementation itself
(unreachable arms) can produce a panic.

### 8.2 rethrow

```sol
try {
    remote_call("memory", "memory.search", "x");
} catch responder_error {
    rethrow;             // propagate to outer try (or halt if none)
} catch any {
    error_kind();        // local handler for everything else
}
```

`rethrow;` inside a catch body re-raises the *currently captured*
`last_error` to the next outer try handler. If there is no outer
handler, the VM halts with `VM_ERROR_SENTINEL`.

### 8.3 Nesting

`try` blocks nest arbitrarily. The VM maintains a stack of active
handlers; each `TryEnter` pushes; `TryExit` pops on the success path;
the dispatch path also pops as it walks outward.

### 8.4 Stack discipline on dispatch

When a failure dispatches to a catch:

- The VM restores the frame pointer that was active at the matching
  `TryEnter`.
- The VM truncates the operand stack to the length it had at that
  `TryEnter`.

This means any partial expression evaluation inside the `try` body is
discarded — the catch handler starts from a clean stack state in the
same logical frame as the `try`.

---

## 9. Delegate sugar

```sol
let child: str = delegate goal <goal_expr> from <parent_expr> to <target_expr>;
```

Lowered to a synthetic `remote_call`:

```sol
remote_call("coord", "delegate.spawn", <parent>|<goal>||<target>|0)
```

(`<context>` and `<depth>` default to empty / `0`; power users who need
them call `remote_call` directly.)

Returns the child task id as a `str`.

`delegate` is a soft keyword — recognised only when immediately
followed by the `goal` sub-keyword. `let delegate: int = 1;` is legal
and binds a variable named `delegate`.

---

## 10. Send sugar

```sol
let mid: str = send subject <subj_expr> body <body_expr> from <from_expr> to <to_expr>;
```

Lowered to a synthetic `remote_call`:

```sol
remote_call("coord", "msg.send", <from>|<to>|<subj>|<body>|||0|sol_flow)
```

The two empty positions (`||`) are `thread_id` and `reply_to`; the `0`
is `ttl_secs`; `sol_flow` is the hardcoded `origin_surface` so the
message store records where the message came from.

Returns the message id as a `str`.

`send` is a soft keyword — recognised only when immediately followed
by the `subject` sub-keyword.

---

## 11. Execution model

### 11.1 VM shape

- Stack of `u64` operands.
- Heap of `HeapObject` variants: `String`, `Struct`, `Array`, `List`,
  `Map`.
- Call stack of frames carrying `(return_ptr, old_fp)`.
- Bytecode is a flat `Vec<Inst>` plus an instruction pointer.

Booleans live as `0` / `1` on the operand stack. Strings, lists, maps,
and structs live on the heap; the stack carries indices into the heap.

### 11.2 Entry point

After compiling all top-level declarations, the codegen emits a final
`Call(start_addr, 0)` where `start_addr` is the address of the `start`
function. The VM begins execution at instruction `0` and naturally
runs into that synthesised call. A file with no `start` function
compiles to a program that ends without invoking any user code.

### 11.3 Halting conditions

| Condition | Result of `run()` | `last_error()` |
|---|---|---|
| `start` returns | top of stack at end of program (heap ref for str, raw int for int/bool, raw bits for float) | `None` |
| `remote_call` fails, no outer try | `u64::MAX` (`VM_ERROR_SENTINEL`) | `Some(...)` with `kind` and `cause` |
| `list_get_list` / `map_get_map` runtime error, no outer try | `u64::MAX` | `Some(...)` with `kind = 0` and `cause` describing the builtin |
| `rethrow` with no outer try | `u64::MAX` | the original `last_error` is preserved |
| `remote_call` invoked with no dispatcher attached | `u64::MAX` | `kind = 0`, cause names the gap |
| VM integrity fault (stack underflow, bad heap ref, division by zero, allocation ceiling, invalid opcode payload) | `u64::MAX` (`VM_ERROR_SENTINEL`) | `Some(...)` with `kind = 0`, cause describes the fault |

### 11.4 What is bounded and what is not

**Bounded:**

- **Instruction count.** Every SOL execution has a fuel budget.
  The default is `DEFAULT_MAX_STEPS = 100_000` instructions.
  The hard ceiling is `MAX_STEPS_CEILING = 10_000_000`.
  A per-flow `#steps N` directive (see `§11.5`) or the caller-supplied
  `default_max_steps` argument may raise the budget up to the ceiling.
  When the budget hits zero the VM halts with `SolError::FuelExhausted`
  — runaway loops exhaust fuel before the host is killed.
- **Per-allocation element count.** `ALLOC_CEILING = 1 << 24`
  (16,777,216 elements) limits any single `NewArray`, `PushList`,
  `PushMap`, or `StoreLocal` growth request. Attempts above the ceiling
  halt with a structured VM fault.

**Not bounded:**

- Recursion depth (deep recursion consumes host stack).
- Total heap entries across all allocations (bounded only by host RAM).
- Operand stack depth within the allocation ceiling.

The Sflow executor's 100-iteration cap does not apply to SOL.

### 11.5 Per-flow fuel budget (`#steps N`)

A SOL source file may open with a `#steps N` directive that overrides
the caller's default budget:

```sol
#steps 500_000   # tuned for a large search flow
function start() -> str { ... }
```

Rules:

- Must appear at the top of the file before any non-comment,
  non-blank line.
- `N` must be a positive decimal integer. Underscores are allowed as
  thousands separators: `#steps 1_500_000`.
- Trailing line comments are allowed: `#steps 500_000  # note`.
- A duplicate `#steps` line is `SolError::BadStepsDirective`.
- `#steps 0` is `SolError::BadStepsDirective`.
- The value is clamped to `MAX_STEPS_CEILING` — no flow can exceed the
  ceiling even with an explicit directive.

Fuel resolution order in `compile_source_with_directives`:

1. `#steps N` directive in source (wins; clamped to `MAX_STEPS_CEILING`).
2. `default_max_steps` argument from the caller (if non-zero; clamped).
3. `DEFAULT_MAX_STEPS` (when the caller passes 0).

`compile_source` (the back-compat form) silently strips the directive
and does not apply it — fuel is not tracked when using that function.

YAML flows compiled through the YAML frontend always use
`DEFAULT_MAX_STEPS`; the `#steps` directive is not supported in
`.yml`/`.yaml` files.

---

## 12. Honest scope statements

### 12.1 Dead-code paths in the verbatim port

- `Type::Tuple(...)` is parsed in type positions but no expression
  syntax constructs a tuple value.
- `Type::Array { size, inner }` is parsed in type positions but the
  `[a, b, c]` literal always produces a `list` (the `ExprArrayInit`
  code path is unreachable from current source).
- `enum` variants compile to `var.chars().next() % 10` — variant
  equality is not implemented. Treat `enum` as parse-syntax only;
  do not branch on variant values.

### 12.2 Strings are escape-free (SIMP-016)

The lexer does not interpret `\n`, `\t`, `\"`, or any other escape
sequence. A string literal stops at the next unescaped `"`. To embed a
newline or a quote, use `+` concatenation with a host-rendered template
substitution.

### 12.3 No `break` / `continue` / `do`

SOL has no `break`, `continue`, or `do { } while`. Loops only exit when
the condition is false or the enclosing function returns.

### 12.4 No first-class functions

`Type::Function` exists in the analyzer but you cannot bind a function
to a `let`, pass one as an argument, or return one. Functions are
declarable only at the top level.

### 12.5 Synchronous dispatcher (SIMP-014)

`remote_call` blocks the VM thread until the dispatcher returns.
Production hosts run the SOL VM inside `tokio::task::spawn_blocking` so
the async runtime stays healthy.

### 12.6 No replay-driven VM

Per-flow event logs capture dispatcher results, but the VM is not yet
replay-driven (SIMP-008). A replay-mode VM is roadmapped but not in
alpha.

---

## Appendix A: Operator → opcode mapping (informative)

| Operator | int operands | float operands | str operands |
|---|---|---|---|
| `+` | `IntAdd` | `FloatAdd` | `ConcatStr` |
| `-` | `IntSub` | `FloatSub` | (compile error) |
| `*` | `IntMul` | `FloatMul` | (compile error) |
| `/` | `IntDiv` | `FloatDiv` | (compile error) |
| `==` | `IntEq` | `FloatEq` | `EqStr` |
| `!=` | `IntNeq` | `FloatNeq` | `EqStr` + `LogNot` |
| `<` | `IntLt` | `FloatLt` | (semantically undefined) |
| `<=` | `IntLte` | `FloatLte` | (semantically undefined) |
| `>` | `IntGt` | `FloatGt` | (semantically undefined) |
| `>=` | `IntGte` | `FloatGte` | (semantically undefined) |
| `&&` | `LogAnd` | (compile error) | (compile error) |
| `\|\|` | `LogOr` | (compile error) | (compile error) |
| `&` `\|` `^` `<<` `>>` | `BitAnd` / `BitOr` / `BitXor` / `BitLShift` / `BitRShift` | (compile error) | (compile error) |

---

## Appendix B: Reading the parser source

If something here disagrees with the compiler, the compiler wins. The
relevant files:

- `crates/relix-runtime/src/sol/lexer.rs` — token kinds, keyword
  identifier table.
- `crates/relix-runtime/src/sol/parser.rs` — grammar, sugar lowerings,
  string interpolation expansion.
- `crates/relix-runtime/src/sol/analyzer.rs` — types, scopes,
  type-checking, built-in arity/signature checks.
- `crates/relix-runtime/src/sol/bytecode.rs` — codegen,
  operator → opcode mapping, `for` lowering.
- `crates/relix-runtime/src/sol/vm.rs` — runtime semantics of every
  opcode, `VM_ERROR_SENTINEL`, error classification, heap-object
  display.
- `crates/relix-runtime/src/sol/dispatcher.rs` — the
  `RemoteCallDispatcher` trait, `RemoteCallError`.
- `crates/relix-runtime/src/sol/language_reference_examples.rs` —
  the executable test suite that backs every example in this document.
