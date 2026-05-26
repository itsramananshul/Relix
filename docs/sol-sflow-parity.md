# SOL ↔ Sflow parity: list & map data structures

Status as of commit landing this doc. Tracks what shipped in
each language, where the two languages diverge by design, and
where genuine gaps remain.

## Summary

| Feature | SOL | Sflow |
|---|---|---|
| List literal `[a, b, c]` | ✅ `Ast::ExprList`, `Inst::PushList(n)` | ✅ `Expr::ListLit`, stored as `SflowValue::List(Vec<String>)` |
| Empty list `[]` | ✅ | ✅ |
| Map literal `{ "k": v, … }` | ✅ `Ast::ExprMap`, `Inst::PushMap(n)` | ✅ `Expr::MapLit`, stored as `SflowValue::Map(Vec<(String, String)>)` |
| Empty map `{}` | ✅ | ✅ |
| `list_len` / `_get` / `_push` / `_contains` / `_join` / `_split` | ✅ each = one dedicated `Inst::*` opcode | ✅ each = an arm in `eval_builtin` |
| `map_get` / `_set` / `_has` / `_keys` / `_len` / `_del` | ✅ each = one dedicated `Inst::*` opcode | ✅ each = an arm in `eval_builtin` |
| Immutable update semantics | ✅ all `*_set` / `*_push` / `*_del` return a fresh heap object | ✅ same — `eval_builtin` returns a fresh `SflowValue` |
| Out-of-bounds / missing-key returns empty string | ✅ | ✅ |
| `for x in lst { … }` iteration | ✅ via new `Inst::ListLen` / `Inst::ListGet` | ❌ — Sflow has no `for-in` construct; use `loop N times` + `list_get` |
| Type tracking | ✅ `Type::List` / `Type::Map` in the analyzer; `let xs: list = …` checked | ❌ — Sflow has no `let` / type annotations |
| Heterogeneous elements | ✅ values are raw `u64` heap refs | ⚠ — Sflow stores all elements as `String`; non-string values stringify on insert |

## Where the languages intentionally diverge

### Sflow stringifies everything outside its rich slots

Sflow's design philosophy: every value is a `String` in
expression and step contexts. The list/map work added a typed
`SflowValue` enum that's stored in `vars`, but the moment a
value crosses into a step argument, `${…}` interpolation, or a
condition, it stringifies. Lists become `a|b|c`; maps become
`k1=v1;k2=v2`.

This means a list / map produced inside a Sflow flow can be
passed to a capability that expects a pipe-delimited payload
without any extra `list_join(…, "|")` step.

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

### Sflow has no `for-in`

SOL ships `for x in lst { … }` with codegen routing to
`Inst::ListLen` / `Inst::ListGet`. Sflow has no `for-in`
construct (the parser would need a new statement form) so list
iteration in Sflow is `loop N times` + `list_get(lst, "${loop.iter}")`.

Closing this gap requires adding a new `Stmt::ForIn` variant
to Sflow + executor support. Not done — the existing
`loop N times` pattern is good enough for the common case
(operator iterating a fixed-length list).

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

- **Sflow `for-in`** — needs a new statement form + executor
  support. The existing `loop N times` idiom covers the common
  case but lacks the symmetric ergonomics with SOL.
- **Nested lists / maps** — neither language supports a list
  of lists or a map of maps. The VM can carry the refs (heap
  values are `u64`) but the built-ins flatten on read. Future
  work: add `list_get_list(...)` / `map_get_map(...)` accessors
  that return the typed inner value.
- **Numeric typing for `list_len` / `map_len` / `list_get`
  index** — Sflow returns `"3"` as a string and the index
  parameter has to be a `"0"` string. SOL returns a real `int`.
  Future work: add a `to_int` / `to_str` pair of conversion
  built-ins in Sflow so flows can mix-and-match without
  string-parsing the count.

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
