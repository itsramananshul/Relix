# Event Vocabulary

The Coordinator's `task_events` table is the runtime's audit trail
for what happened on a Task. This document is the **stable
contract** for the event names runtime components emit and the
payload conventions operator tooling expects. Priority C of the
operator-platform-maturation roadmap.

Stick to this vocabulary when adding new event emitters. Drift is
expensive once dashboards and parsers start pattern-matching on
specific names.

## Why this matters

`event_type` is a free string at the database level (so operators
can attach their own custom events via `task.event`), but the
Coordinator itself + the bridge + the recovery scan emit a fixed
set the C2c `--pretty` renderer (and now `/v1/tasks/:id/events`)
project. Once a name is shipped, renaming it without a
compatibility shim breaks any dashboard polling on it.

The current set is small enough to audit; this doc keeps it that
way.

## The current vocabulary

All names use dot-separated namespacing (`subsystem.event`). The
subsystem prefix groups related events visually in a chronicle
and lets operators filter by family (`grep '^task\.'`).

### `task.*` — lifecycle events emitted by the Coordinator

| Event | Emitted by | Payload format | Wire-stable since |
|---|---|---|---|
| `task.created` | bridge (right after `task.create` succeeds) | `<flow_template>` | C1c.6 |
| `task.attempt_started` | Coordinator inside `update_with_trace` when a new attempt opens | `attempt_id=<N> attempt_num=<M> [trace_id=<hex>]` | C2a.2 |
| `task.attempt_finished` | Coordinator inside `update_with_trace` when an attempt closes; also recovery scan when it forces-closes a stale attempt | `attempt_id=<N> status=<terminal_status> [failure_class=<class>]` | C2a.2 |
| `task.completed` | bridge on successful flow run | excerpt of reply text (≤ 200 chars) | C1b.3 |
| `task.failed` | bridge on flow failure | cause string from the responder error envelope | C1b.3 |
| `task.interrupted` | recovery scan on stale `running` rows | `started_at=<N> max_runtime_secs=<N> now=<N> reason=deadline_exceeded` | C1b |
| `task.retry_requested` | Coordinator's `request_retry` on Accepted | `attempt=<N> of_budget=<M> policy=<policy> prior_class=<class\|->` | C2c.1 |
| `task.retry_exhausted` | Coordinator's `request_retry` on Exhausted | `retry_count=<N> budget=<M> policy=<policy>` | C2c.1 |

### `flow.*` — runtime-orchestration events emitted by the bridge

| Event | Emitted by | Payload format | Wire-stable since |
|---|---|---|---|
| `flow.started` | bridge before invoking `FlowRunner::run` | flow_template path | C1b.3 |

(`flow.completed` and `flow.failed` are NOT separate events today
— the bridge collapses them into `task.completed` / `task.failed`
since for the alpha "flow done" == "task done.")

### `capability.*` — pre-execution intent emitted by the bridge

| Event | Emitted by | Payload format | Wire-stable since |
|---|---|---|---|
| `capability.invoked` | bridge before a capability call it knows about (today: tool.web_fetch path only) | `method=<method_name> target=<arg>` (target optional) | C1b.3 |

### Reserved names (not emitted today)

These names are reserved for future runtime work. Any new emitter
SHOULD use one of them rather than coining a similar variant.

| Reserved name | Intent |
|---|---|
| `capability.completed` | per-`remote_call` success at the bridge level (requires VM-side hooks the alpha doesn't have) |
| `capability.failed` | per-`remote_call` failure at the bridge level |
| `task.cancelled` | explicit operator cancellation (today implicit via `task.update --status cancelled`) |
| `task.awaiting_input` | resume-pause notification (Gate 2 — requires durable VM yield) |

The `--pretty` renderer in `relix-cli task get` and the
`/v1/tasks/:id/events` endpoint will gain handling for these as
the corresponding runtime work lands; until then they're a
documented vocabulary slot, NOT silently emitted.

### Operator-defined custom events

Operators are welcome to call `task.event` with any event_type
string they want. The convention is:

- Use a subsystem prefix that does NOT collide with the runtime
  vocabulary above. `ops.*`, `script.*`, `audit.*` are all fine.
- Keep payloads grep-friendly: `key=value` pairs separated by
  spaces.
- Avoid newlines and tabs in payload (the Coordinator escapes
  them, but readable rendering across tools depends on flat
  single-line payloads).

## Payload format contract

Payloads are free strings at the database level. The runtime-
emitted events follow these conventions; operator-defined events
SHOULD follow them too for consistent rendering:

1. **`key=value` pairs separated by ASCII space.** No commas.
   This lets the CLI's `extract_kv_int` (and any equivalent in
   future tooling) parse without a JSON dependency.
2. **No newlines or tabs in payload values.** The Coordinator's
   `task.event` handler accepts them, but rendering breaks down
   the chain. If you need a multi-line value, log to a file and
   put the file path in the payload.
3. **Numeric values without quotes.** `attempt_id=42`, not
   `attempt_id="42"`.
4. **String values without quotes when they don't contain
   spaces.** `policy=bounded`, not `policy="bounded"`. Quote
   only when the value would otherwise be ambiguous.
5. **Use `-` for "no value" rather than omitting the key entirely.**
   Operator scripts that grep for `failure_class=` get a usable
   answer either way.

## Versioning

The current vocabulary is v0. There is no explicit version field
on events today.

When a payload format changes incompatibly (e.g. renaming a key,
changing a value's semantics), the convention is:

1. Coin a new event_type rather than mutating the old one.
2. Document the old name's deprecation in this file.
3. Keep both emitters running for at least one release cycle to
   give dashboards time to migrate.

When a payload format gains a new optional key (extending, not
breaking), it's safe to ship in place — existing parsers ignore
unknown keys per convention #1.

## Renderer contracts

Tooling that renders the chronicle is bound by these rules:

- `relix-cli task get --pretty`:
  - Groups events by attempt boundary using
    `task.attempt_started` / `task.attempt_finished` markers.
  - Surfaces failure-class hints (see [`retry-model.md`](retry-model.md))
    in the header callout block.
  - Falls back to raw rendering if the events array doesn't parse.

- `GET /v1/tasks/:id` and `GET /v1/tasks/:id/events`:
  - Emit each event as
    `{"event_id":N,"ts":N,"event_type":"...","payload":"..."}`.
  - Field names are stable; new optional fields may be added in
    future projections.

- `GET /v1/tasks/:id/events?since=N`:
  - Returns only events with `event_id > since`, oldest-first.
  - Empty array (not 404) when nothing is newer than `since`.

## See also

- [`task-runtime.md`](task-runtime.md) — schema + wire format of
  `task.event` itself.
- [`runtime-lifecycle.md`](runtime-lifecycle.md) — which events
  fire on each status transition.
- [`attempt-lineage.md`](attempt-lineage.md) — the
  `task.attempt_*` events in their per-attempt context.
- [`retry-model.md`](retry-model.md) — the
  `task.retry_*` events.
- [`interruption-semantics.md`](interruption-semantics.md) — the
  `task.interrupted` event.
- [`crates/relix-runtime/src/nodes/coordinator/mod.rs`](../crates/relix-runtime/src/nodes/coordinator/mod.rs)
  — the authoritative emitters.
