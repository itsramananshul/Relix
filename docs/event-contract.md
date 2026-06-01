# Event Contract

> Version 0.4.1

The S2 typed-event-envelope wire contract for the **Coordinator's
`task_events` chronicle**. This document covers that one store.
Additional event stores exist and have their own schemas; see the
table below.

| Store | SQLite file | Accessed via |
|---|---|---|
| Task chronicle | `<coordinator-data>/tasks.db` (`task_events` table) | `/v1/tasks/:id/events` HTTP endpoint, `task.events` capability |
| Alert chronicle | `~/.relix/<node>/alerts.sqlite` (`alert_events` table) | `observability.alert_history` capability |
| Observability Sink A | `~/.relix/<ai-node>/metadata.db` (`metadata_events` table) | `SessionDebugger` + OTel export |
| Observability Sink B | `~/.relix/<ai-node>/content.db` (`content_events` table) | `SessionDebugger` only; never exported via OTel |
| PII audit chronicle | `~/.relix/<node>/pii_events.sqlite` (`pii_events` table) | `pii.recent_events` capability |
| Audit partition mirror | `~/.relix/<node>/audit-partition.db` (`audit_partition` table) | `node.audit.tenant_list`, `node.audit.tenant_recent` capabilities |

All on-disk CBOR event records (flow logs, per-node audit logs) use
4-byte big-endian length framing: each record is preceded by a
4-byte big-endian length field followed by the CBOR-encoded record
bytes. This framing applies to `relix-core::eventlog` and
`relix-core::audit` files; it does NOT apply to the SQLite-backed
stores listed above.

Read this when:

- adding a new event emitter inside the Coordinator or bridge,
- building a dashboard / consumer of `/v1/tasks/:id/events`,
- writing a chronicle parser that wants typed access to event
  payloads.

## Two envelope versions

Every row in `task_events` has a `schema_version` (default `0`):

- **`schema_version = 0` — legacy string envelope.** Only
  `payload` (free text) is populated. `attempt_id`, `trace_id`,
  `payload_json` are NULL. Operator-defined events written via
  the `task.event` capability are always v0 by design; the
  Coordinator doesn't fabricate structure it doesn't have.
- **`schema_version = 1` — structured envelope.** Runtime-
  emitted events (attempt boundaries, recovery scan,
  retry decisions) carry typed columns: `attempt_id` and
  `trace_id` when known, plus a JSON `payload_json` document
  whose shape depends on the `event_type`. The legacy `payload`
  string is also populated for back-compat — consumers that
  haven't been upgraded still see something sensible.

A consumer that ignores `schema_version` and reads only
`event_type` + `payload` continues to work unchanged.

## Typed payload schemas (v1)

The Coordinator emits these payloads. Each is rendered as JSON in
`payload_json` and the wire-level `{"...", "payload_json": {...}}`
slot of the event object.

### `task.attempt_started`

Emitted by `TaskStore::update_with_trace` when a new attempt row
opens (status → running with no open attempt).

```json
{
  "attempt_id":   <i64>,        // global attempt id (PK in task_attempts)
  "attempt_num":  <i64>,        // 1-based per-task
  "trace_id":     "<32 hex>"    // present only when caller supplied one
}
```

`attempt_id` is duplicated into the envelope's top-level
`attempt_id` column for fast index scans without parsing JSON.

### `task.attempt_finished`

Emitted by `TaskStore::update_with_trace` when an attempt closes
(status → completed/failed/cancelled) and by the recovery scan
when it forces an attempt closed.

```json
{
  "attempt_id":     <i64>,
  "status":         "completed" | "failed" | "cancelled" | "interrupted",
  "failure_class":  "transient" | "permanent" | "policy_denied"
                  | "invalid_args" | "timeout" | "unavailable"
                  // omitted when not a failure
}
```

### `task.interrupted`

Emitted by `recover_interrupted` when the scan flips a `running`
task whose `started_at + max_runtime_secs < now`.

```json
{
  "started_at":       <i64>,    // current attempt's started_at
  "max_runtime_secs": <i64>,
  "now":              <i64>,
  "reason":           "deadline_exceeded"
}
```

If a `current_attempt_id` was set, this event's envelope
`attempt_id` column also holds it.

### `task.retry_requested`

Emitted by `TaskStore::request_retry` on `RetryDecision::Accepted`.

```json
{
  "attempt":      <i64>,    // new retry_count (1-based)
  "of_budget":    <i64>,    // configured retry budget
  "policy":       "once" | "bounded",
  "prior_class":  "transient" | ... | "-"  // last_failure_class on the row, or "-"
}
```

### `task.retry_exhausted`

Emitted by `TaskStore::request_retry` on `RetryDecision::Exhausted`.

```json
{
  "retry_count":  <i64>,    // current count (>= budget)
  "budget":       <i64>,
  "policy":       "once" | "bounded"
}
```

### `task.created` / `flow.started` / `task.completed` / `task.failed` / `capability.invoked`

Currently emitted by the **bridge** as v0 (string-only) events
since the bridge writes them via the operator-facing
`task.event` capability — not via the runtime helpers. The
typed envelope columns are `null`; the existing `payload` field
holds the human-readable string described in
[`event-vocabulary.md`](event-vocabulary.md).

Upgrading these to v1 is a follow-up. The bridge would need a new
internal path that bypasses `task.event` (which is operator-
addressable on purpose and shouldn't take structured payloads
from outside callers).

## Wire format on the bridge

`GET /v1/tasks/:id/events?...` returns a JSON array. Each event:

```json
{
  "event_id":       <i64>,
  "ts":             <i64>,
  "event_type":     "<string>",
  "payload":        "<string>",         // legacy string; always present
  "schema_version": <i64>,              // omitted when 0
  "attempt_id":     <i64>,              // omitted when null
  "trace_id":       "<string>",         // omitted when null
  "payload_json":   <object|array|value> // omitted when null; ANY valid JSON
}
```

`GET /v1/tasks/:id` returns the same per-event shape inside the
`events` array of `TaskDetail`.

The Coordinator's raw `task.events` body uses identical key
names (just newline-delimited rather than wrapped in an array).
Bridge parsing is via `serde_json::from_str` since `payload_json`
can nest arbitrarily.

## Adding a new event_type

If you're a runtime author landing a new emitter:

1. Pick a name that fits an existing namespace (`task.*`,
   `flow.*`, `capability.*`) or open a new one and document it
   in [`event-vocabulary.md`](event-vocabulary.md).
2. Define the typed payload schema in this file. Use real keys
   the emitter actually has — don't invent fields you can't fill.
3. Use the `insert_typed_event` helper inside the Coordinator's
   `crates/relix-runtime/src/nodes/coordinator/mod.rs`. It
   handles the structured INSERT + back-fills the legacy
   `payload` string from whatever human-readable form you pick.
4. Update the renderer (`render_event_json`) only if your new
   fields aren't already in the schema — most won't need to
   touch it (`payload_json` is an opaque blob).
5. Add a test asserting the typed columns + JSON shape on emit.

If you're an operator adding a custom event via `task.event`:

1. Stick to v0 (string `payload`). Use `key=value` pairs
   separated by ASCII space — the convention every renderer
   parses well.
2. Namespace with `ops.*` or similar (anything that doesn't
   collide with the runtime vocabulary).

## Versioning

Today there is one version (`1`) for runtime events and one
legacy form (`0`). The convention going forward:

- **Add an optional field to an existing v1 schema:** safe.
  Existing consumers ignore the unknown key.
- **Remove a field from an existing v1 schema:** unsafe; coin
  a v2 and emit both during a transition window.
- **Change a field's type:** unsafe; coin a v2.
- **Change a field's semantics:** unsafe; coin a new
  `event_type` rather than re-versioning. Dashboards filtering
  by name are then explicit about which semantic they're
  consuming.

The Coordinator does NOT enforce schema validation today — the
typed payload is opaque JSON. A future Gate 2 item is a CDDL/JSON-
Schema registry per `event_type`; out of scope for the alpha.

## What the contract does NOT do

- **No automatic v0 → v1 migration of historical rows.** Existing
  legacy events stay legacy. Consumers that need typed data on
  old events parse the `payload` string themselves.
- **No event compaction or deletion.** Retention design lives
  in [`chronicle-retention.md`](chronicle-retention.md) (when
  written); no destructive deletion has been implemented.
- **No cross-event causality field.** `attempt_id` ties events
  to an attempt; there's no separate `parent_event_id`. Add one
  when there's a concrete use case.

## See also

- [`event-vocabulary.md`](event-vocabulary.md) — the event names
  and naming conventions (operator-facing).
- [`task-runtime.md`](task-runtime.md) — schema + capability wire
  format.
- [`task-api.md`](task-api.md) — bridge HTTP surface for events
  + tasks (operator/dashboard-facing).
- [`runtime-observability.md`](runtime-observability.md) —
  mental model for using these surfaces.
- [`crates/relix-runtime/src/nodes/coordinator/mod.rs`](../crates/relix-runtime/src/nodes/coordinator/mod.rs)
  — authoritative emitter source.
