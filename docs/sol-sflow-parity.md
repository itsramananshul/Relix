# SOL ↔ Sflow parity: list & map data structures

Status as of commit landing this doc. Tracks what shipped in
each language, where the two languages diverge by design, and
where genuine gaps remain.

## Summary

| Feature | SOL | Sflow |
|---|---|---|
| List literal `[a, b, c]` | ✅ `Ast::ExprList`, `Inst::PushList(n)` | ✅ `Expr::ListLit`, stored as `SflowValue::List(Vec<SflowValue>)` (F11) |
| Empty list `[]` | ✅ | ✅ |
| Map literal `{ "k": v, … }` | ✅ `Ast::ExprMap`, `Inst::PushMap(n)` | ✅ `Expr::MapLit`, stored as `SflowValue::Map(Vec<(String, SflowValue)>)` (F11) |
| Empty map `{}` | ✅ | ✅ |
| `list_len` / `_get` / `_push` / `_contains` / `_join` / `_split` | ✅ each = one dedicated `Inst::*` opcode | ✅ each = an arm in `eval_builtin` |
| `map_get` / `_set` / `_has` / `_keys` / `_len` / `_del` | ✅ each = one dedicated `Inst::*` opcode | ✅ each = an arm in `eval_builtin` |
| Immutable update semantics | ✅ all `*_set` / `*_push` / `*_del` return a fresh heap object | ✅ same — `eval_builtin` returns a fresh `SflowValue` |
| Out-of-bounds / missing-key returns empty string | ✅ | ✅ |
| `for x in lst { … }` iteration | ✅ via `Inst::ListLen` / `Inst::ListGet` | ✅ F9 — `for x in <list>` binds each element as `SflowValue::String` (loop var restored after `end`) |
| Nested lists / maps | ✅ F11 — `Inst::ListGetList` / `Inst::MapGetMap` typed accessors; `ListJoin` recurses via `heap_display` | ✅ F11 — `SflowValue::List(Vec<SflowValue>)` / `Map(Vec<(String, SflowValue)>)`; `list_get_list` / `map_get_map` typed accessors |
| Type tracking | ✅ `Type::List` / `Type::Map` in the analyzer; `let xs: list = …` checked | ❌ — Sflow has no `let` / type annotations |
| Heterogeneous elements | ✅ values are raw `u64` heap refs | ✅ F11 — Sflow stores typed `SflowValue` (String, List, Map); stringification happens at display/interpolation time, not at store time |

## Where the languages intentionally diverge

### Sflow stringifies at the boundary, not at store time

Since F11, Sflow stores values as a typed `SflowValue` enum —
`SflowValue::String(String)`, `SflowValue::List(Vec<SflowValue>)`,
and `SflowValue::Map(Vec<(String, SflowValue)>)`. Nested lists and
maps are represented faithfully inside the variable store.

Stringification happens only when a value crosses a boundary into
a step argument, `${…}` interpolation, or a condition: lists become
`a|b|c`, maps become `k1=v1;k2=v2`. This means a typed list or map
produced inside a Sflow flow can be passed to a capability that
expects a pipe-delimited payload without any extra `list_join(…, "|")`
step.

SOL has no analogue — there is no implicit stringification.
A SOL flow that wants to pass a list to `remote_call` calls
`list_join(xs, "|")` explicitly, the same way it would write
a separator string.

### Sflow built-ins return `"true"` / `"false"`, not a typed bool

`list_contains` / `map_has` return `bool` in SOL (`1` / `0` on
the VM stack) but `"true"` / `"false"` as `SflowValue::String`
in Sflow. That is because Sflow has no `bool` type — every
condition compares strings. The canonical Sflow idiom is
`if list_contains(var.xs, "x") == "true" …`.

### Both languages have `for-in` (F9)

`for x in <list>` works in both SOL and Sflow. SOL binds the
loop variable as `str` (the list element); Sflow binds the
typed `SflowValue` so a nested list inside a list-of-lists
exposes each inner list to the body as a real list. Both
languages honor the per-execution loop iteration cap and write
`sol.loop_iter` chronicle events on each iteration.

### Sflow tolerates string-encoded lists / maps where SOL doesn't

Sflow's built-ins accept a `SflowValue::String` where a list or
map is expected and parse the canonical encoding (`|` for lists,
`;` + `=` for maps). This lets operators interleave structured
data with steps that produce strings — the result of a
`remote_call` step is a `String`, but it can be passed to
`list_split(…)` and immediately treated as a list afterwards.

SOL is strict: `list_len(var.xs)` requires `xs` to be a
`Type::List` at compile time; the analyzer rejects a `str`
in a list slot.

## Remaining gaps

- **Numeric typing for `list_len` / `map_len` / `list_get`
  index** — Sflow returns `"3"` as a string and the index
  parameter has to be a `"0"` string. SOL returns a real `int`.
  Future work: add a `to_int` / `to_str` pair of conversion
  built-ins in Sflow so flows can mix-and-match without
  string-parsing the count.
- **`{}` map literal in SOL condition context** — SOL gates
  map literal parsing on `can_struct`, so `if cond {}` reads
  the brace as an if-body opener rather than an empty map.
  This is intentional disambiguation, not a missing feature,
  but operators should know.

These gaps are documented as known limitations rather than
silent divergences — operators authoring parity flows can read
this table and decide which language fits their use case.

## Test parity

| Test | SOL location | Sflow location |
|---|---|---|
| Empty list literal | `sol::list_map_tests::empty_list_literal_compiles_and_has_length_zero` | `sflow::executor::tests::empty_list_literal_in_set_stores_empty_list` |
| 3-element list | `sol::list_map_tests::three_element_list_has_length_three` | `sflow::executor::tests::three_element_list_literal_has_length_three` |
| `list_get` happy path | `sol::list_map_tests::list_get_returns_element_at_index` | `sflow::executor::tests::list_get_returns_element_at_index` |
| `list_get` out of bounds | `sol::list_map_tests::list_get_out_of_bounds_returns_empty_string_not_panic` | `sflow::executor::tests::list_get_out_of_bounds_returns_empty_string` |
| `list_push` immutability | `sol::list_map_tests::list_push_returns_new_list_original_unchanged` | `sflow::executor::tests::list_push_returns_new_list_original_unchanged` |
| `list_contains` true / false | `sol::list_map_tests::list_contains_*` | `sflow::executor::tests::list_contains_*` |
| `list_join` | `sol::list_map_tests::list_join_concatenates_with_separator` | `sflow::executor::tests::list_join_produces_correct_string` |
| `list_split` | `sol::list_map_tests::list_split_*` | `sflow::executor::tests::list_split_*` |
| Empty map | `sol::list_map_tests::empty_map_literal_compiles_and_has_length_zero` | `sflow::executor::tests::empty_map_literal_has_length_zero` |
| `map_get` / `_has` | `sol::list_map_tests::map_get_*` / `map_has_*` | `sflow::executor::tests::map_get_*` / `map_has_*` |
| `map_set` / `_del` immutability | `sol::list_map_tests::map_set_*` / `map_del_*` | `sflow::executor::tests::map_set_*` / `map_del_*` |
| Insertion order preserved | `sol::list_map_tests::map_keys_preserves_insertion_order` | `sflow::executor::tests::map_keys_returns_keys_list_in_insertion_order` |
| Chained functional update | `sol::list_map_tests::nested_map_set_calls_chain_correctly_for_functional_updates` | `sflow::executor::tests::nested_map_set_chains_correctly` |
| Interpolation inside literal | `sol::list_map_tests::map_literal_with_string_interpolation_value_compiles` | `sflow::executor::tests::map_literal_value_can_carry_interpolation` |
| Stringification format | (not relevant — SOL is strict) | `sflow::executor::tests::list_display_format_is_pipe_separated` / `map_display_format_is_semicolon_separated` |

Twelve direct parity tests; three Sflow-specific tests for the
stringification contract.
