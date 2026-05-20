# Task API — Bridge HTTP Surface

The bridge exposes the Coordinator's task ledger as a JSON HTTP
API. Every endpoint is **read-only or operator-action only**;
the bridge stays translation-only and adds no orchestration
logic. Bodies are JSON; the Coordinator's underlying wire
contract is in [`task-runtime.md`](task-runtime.md).

This doc is the single reference for dashboard and operator-
tooling authors. Stability: every endpoint listed here is
expected to remain stable through Gate 1; additive changes
(new optional fields, new endpoints) won't break consumers.

## Listing tasks

### `GET /v1/tasks?limit=N&offset=N&status=...`

Offset-paginated list, oldest-updated first... wait, **most-
recently-updated first**. Use when you need a simple page through
the full ledger and don't care about strict snapshot stability.

Response:

```json
[
  {"task_id": "...", "status": "running", "title": "chat: hello"},
  ...
]
```

Cap: `limit` is clamped server-side (default 50, max 200).

**Use cursor pagination (`/v1/tasks/cursor`) for any live ledger
with concurrent writes** — offset pagination can repeat or skip
rows when ordering ties shift between page requests.

### `GET /v1/tasks/cursor?limit=N&status=...&cursor=<opaque>`

Cursor-paginated list. Stable under concurrent inserts and
updates. The cursor is opaque to the caller — pass back what we
returned.

Response:

```json
{
  "items": [{"task_id": "...", "status": "...", "title": "..."}, ...],
  "next_cursor": "1700000000:abc..."   // omitted on the last page
}
```

First page: omit `?cursor`. Subsequent pages: pass back
`next_cursor` from the previous response. End-of-stream: empty
`items` AND missing `next_cursor`.

### `GET /v1/tasks/count?status=...`

Total count, optionally filtered. Use for "showing N of M"
pagination footers without walking every page.

```json
{"count": 17}
```

## Inspecting one task

### `GET /v1/tasks/:id`

Full task body with chronicle.

Response:

```json
{
  "task_id": "<32 hex>",
  "header": {
    "status": "completed",
    "title": "...",
    "started_at": "1700000000",
    "updated_at": "1700000012",
    "attempt_count": "2",
    "...": "..."           // all key=value lines from task.get
  },
  "events": [
    {
      "event_id": 1,
      "ts": 1700000000,
      "event_type": "task.created",
      "payload": "chat_template.sol",
      "schema_version": 0      // omitted when 0
    },
    {
      "event_id": 2,
      "ts": 1700000000,
      "event_type": "task.attempt_started",
      "payload": "attempt_id=1 attempt_num=1 trace_id=abc",
      "schema_version": 1,
      "attempt_id": 1,
      "trace_id": "abc",
      "payload_json": {"attempt_id": 1, "attempt_num": 1, "trace_id": "abc"}
    }
  ]
}
```

Header is a `string → string` map by design — additive
Coordinator fields surface here without bridge code changes.
Event entries follow the typed envelope contract in
[`event-contract.md`](event-contract.md).

Errors: `400` malformed task_id, `404` unknown task, `502`
Coordinator-side errors, `503` no Coordinator wired.

### `GET /v1/tasks/:id/summary`

One-line operator synopsis as JSON. Same shape the CLI's
`task get --pretty` first line prints.

```json
{
  "task_id":              "...",
  "status":               "failed",
  "attempt_count":        2,           // optional
  "duration_secs":        12,          // only for terminal states
  "started_at":           1700000000,  // optional
  "last_failure_class":   "transient", // optional
  "last_failure_reason":  "...",       // optional
  "retries":              "1/3",       // "<count>/<max>" when retry_policy != none
  "retry_policy":         "bounded"    // optional
}
```

### `GET /v1/tasks/:id/attempts`

Per-attempt rows oldest-first.

```json
[
  {
    "attempt_num":    1,
    "status":         "failed",
    "started_at":     1700000000,
    "finished_at":    1700000005,    // omitted while running
    "failure_class":  "transient",   // omitted when none
    "flow_id":        "..."          // omitted when none
  },
  ...
]
```

### `GET /v1/tasks/:id/events?since=N&limit=M&type=...&order=asc|desc`

Incremental chronicle fetch.

Query parameters (all optional):

- `since=N` — return only events with `event_id > N`. Defaults
  to 0 (everything). Polling dashboards remember the largest id
  they've seen.
- `limit=M` — page size, clamped by the Coordinator.
- `type=<event_type>` — exact-match filter. Useful for
  attempt-only or retry-only subscriptions.
- `order=asc|desc` — `asc` (default) is the long-poll pattern;
  `desc` for "last N events" tail queries.

Response: JSON array of events using the same typed envelope as
`/v1/tasks/:id`'s `events` slot.

### `GET /v1/tasks/:id/events/stream` (experimental)

SSE wrapper around the polling form above. Opens a long-lived
HTTP response; the bridge polls `task.events` server-side and
emits one SSE message per new event. Operator dashboards that
want push-style updates use this; everyone else uses the polling
form.

Message format:

```
event: event
data: {"event_id":N,"ts":N,"event_type":"...","payload":"...",...}

event: gone
data: task.events: not found: <task_id>

event: error
data: <cause string>
```

`event: gone` terminates the stream (the task no longer exists).
`event: error` is a transient signal; the stream stays alive
and retries after the poll interval.

Status:

- **Experimental** — kept only if it stays clean. The bridge
  owns no per-stream task state beyond the cursor on the
  client's open socket. If SSE turns invasive at scale it will
  be retired in favour of the cursor + typed events polling
  surface, which covers every alpha use case.
- No reconnect-with-Last-Event-ID today (clients tracking
  cursor state externally just pass `?since=N` on a new
  request).

### `GET /v1/tasks/:id/lineage`

Single-round-trip combo for dashboard initial render. Packs
detail + summary + attempts in one response so a dashboard
doesn't need three serial fetches.

```json
{
  "task":     { ... TaskDetail ... },
  "summary":  { ... TaskSummary ... },
  "attempts": [ ... TaskAttempt ... ]
}
```

If `attempts` fails to fetch (older Coordinator, policy denial),
the lineage is returned with `attempts: []` and the other
components populated. Fail-soft on degradation.

### `GET /v1/tasks/:id/export`

Archival snapshot for download. Returns one JSON document
containing the task header, every attempt row, and every
chronicle event. The response carries
`Content-Disposition: attachment; filename="task-<id>.json"`
so browsers save directly to disk.

Response shape:

```json
{
  "schema_version": 1,
  "exported_at":    1700000000,
  "task_id":        "...",
  "task": {
    "title": "...", "status": "...", "owner_subject_id": "...",
    "flow_template": "...", "params_json": "...",
    "events": [
      {"id": 1, "ts": 100, "type": "task.created", "payload": "..."},
      ...
    ],
    ...
  },
  "attempts": [
    {"attempt_id": 1, "attempt_num": 1, "started_at": 100, "status": "completed", ...},
    ...
  ]
}
```

Use this as the **save-before-delete** artifact before any
chronicle compaction. See
[`chronicle-retention.md`](chronicle-retention.md) for the
retention design contract.

## Operator actions

### `POST /v1/tasks/recover`

Run the recovery scan now. Promotes overdue `running` tasks to
`interrupted` and emits `task.interrupted` events. Idempotent.

```json
{"recovered": ["abc...", "def..."], "count": 2}
```

No body required.

## Operator dashboard (browser)

### `GET /dashboard`

Single-page HTML dashboard. Static (one HTML file, inline CSS +
vanilla JS). Consumes the JSON endpoints above and renders:

- A status-filtered task list with click-to-inspect rows.
- Per-task summary + per-attempt table.
- Chronicle events grouped + colour-coded by `event_type`
  family (`task.*` / `attempt` / `retry` / `interrupted` /
  `failed`).
- Optional 5-second auto-refresh.

Security headers: `X-Frame-Options: DENY`,
`Content-Security-Policy: default-src 'none'; ... connect-src 'self'`.
No external resources are loaded; CSP enforces it.

The bridge introduces no per-session state to support this — it
is a presentation surface only, per
[`bridge-invariants.md`](bridge-invariants.md). When the page
needs new features the right move is usually to add a new
`/v1/tasks*` endpoint and consume it from JS, not to introduce
server-side dashboard state.

## Capability discovery

### `GET /v1/capabilities?category=...&tag=...`

JSON projection of the bridge's `ManifestCache`. Returns every
capability the bridge knows about, optionally filtered by
descriptor category or sensitivity tag.

### `GET /v1/capabilities/:method`

Scoped to one method. Returns 404 when no peer advertises it.

See [`capability-discovery.md`](capability-discovery.md) for the
planner-foundations contract these endpoints satisfy.

## Versioning + stability

- All endpoints listed above are stable through Gate 1.
- **Additive changes:** new optional response fields, new
  endpoints. No advance notice; consumers ignore unknown fields.
- **Breaking changes:** a new path (e.g. `/v2/tasks`). Old path
  stays operational for at least one release cycle.
- Field naming follows snake_case to match the Coordinator's
  wire convention; never camelCase.

## Status codes

| Code | When |
|---|---|
| `200` | Success. |
| `400` | Malformed task_id (not 32 hex chars), bad query parameter. |
| `404` | Task not found on the Coordinator. |
| `502` | Coordinator call failed (transport, policy denial other than not-found). The cause string is in the `error` field. |
| `503` | Bridge has no Coordinator configured (`[coordinator] alias` missing from bridge TOML). |

Operator-tooling note: a `503` is recoverable by the operator
configuring the bridge; a `502` is a runtime problem to debug.
Dashboards should treat the two distinctly.

## Auth

There is **no HTTP auth** at this layer. The bridge's own
identity is what gates the underlying capability calls on the
Coordinator's admission pipeline. Put a reverse proxy in front
before exposing beyond loopback — the bridge is a peer first and
a public surface second.

## See also

- [`task-runtime.md`](task-runtime.md) — Coordinator-side wire
  contract.
- [`event-contract.md`](event-contract.md) — typed event
  envelope schemas.
- [`event-vocabulary.md`](event-vocabulary.md) — event-type
  naming conventions.
- [`runtime-lifecycle.md`](runtime-lifecycle.md) — what each
  status means.
- [`runtime-observability.md`](runtime-observability.md) —
  mental model + dashboard primitives.
- [`crates/relix-web-bridge/src/tasks.rs`](../crates/relix-web-bridge/src/tasks.rs)
  + [`crates/relix-web-bridge/src/capabilities.rs`](../crates/relix-web-bridge/src/capabilities.rs)
  — handler source.
