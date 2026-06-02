# Task API — Bridge HTTP Surface

_Version: 0.4.1_

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

Offset-paginated list, **most-recently-updated first**. Use when you
need a simple page through the full ledger and don't care about strict
snapshot stability.

Query parameters (all optional):

- `limit=N` — page size; default 50, clamped server-side to max 200.
- `offset=N` — skip the first N rows; default 0. Use `offset=50&limit=50`
  for page 2.
- `status=<string>` — filter to one status value (e.g. `running`,
  `failed`).

Response:

```json
[
  {"task_id": "...", "status": "running", "title": "chat: hello"},
  ...
]
```

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

### `GET /v1/tasks/compact_events?max_age_secs=N`

Chronicle-retention **dry-run** candidate counter. Counts
`task_events` rows that *would* be deleted under a max-age
policy, broken down by parent task status. Honours the
retention design's R5 invariant — only events whose parent
task is in a terminal state (`completed` / `failed` /
`cancelled` / `interrupted`) are counted.

`max_age_secs` is required and must be a positive integer.
Events with `ts < now - max_age_secs` are candidates.

Response shape:

```json
{
  "mode": "dry-run",
  "destructive": false,
  "cutoff_ts": 1700000000,
  "candidate_events": 12345,
  "candidate_tasks": 567,
  "oldest_candidate_ts": 1699000000,
  "newest_candidate_ts": 1699999999,
  "by_task_status": {
    "completed": 12000,
    "failed": 300,
    "cancelled": 45
  }
}
```

`oldest_candidate_ts` / `newest_candidate_ts` are omitted
when the candidate set is empty. The `by_task_status`
object only contains terminal statuses with non-zero
candidate counts.

This is the operator's planning surface for the eventual
destructive Step 3 pass — see the implementation status
in [`chronicle-retention.md`](chronicle-retention.md). No
deletion happens here; the bridge hard-codes
`mode=dry-run` and the Coordinator rejects any other
mode with INVALID_ARGS.

CLI parity: `relix-cli task compact --max-age-secs N`.

## Operator actions

### `POST /v1/tasks/:id/retry?force=<bool>`

Operator-initiated retry (M18). Bridge guards
non-retryable failure classes (`policy_denied` /
`invalid_args` / `permanent`); pass `force=true` to
override. Returns a typed envelope distinguishing
accepted / exhausted / refused outcomes; see
`docs/retry-model.md` for the semantics. Idempotent —
re-submitting after acceptance returns the next attempt
number or `exhausted` once the budget is reached.

### `POST /v1/tasks/:id/cancel`

Mark a task cancelled (M19). Body:

```json
{ "reason": "operator-supplied text" }
```

Response:

```json
{
  "task_id":            "...",
  "prior_status":       "running",
  "new_status":         "cancelled",
  "flow_still_running": true
}
```

`flow_still_running: true` when prior status was `running`
or `retrying`. **Honest:** the runtime has no flow-side
cancellation today — a currently-executing flow continues
and may overwrite the cancelled status when it finishes.
The bridge appends a `task.cancelled` chronicle event
with the reason so the operator action is audit-visible
even when the runtime ignores it.

Rejects (409) terminal states: `completed` / `failed` /
`cancelled`. See `docs/failure-modes.md` for the
operator playbook around the flow-still-running case.

### `POST /v1/tasks/:id/mark-investigation`

Set or clear the investigation marker on a task. Body:

```json
{ "marked": true, "reason": "repeated timeouts on AI peer" }
```

`marked: false` clears the marker; `reason` is optional on clear.

Response:

```json
{ "task_id": "...", "investigation_marked_at": 1700000000 }
```

`investigation_marked_at` is `null` when the marker was cleared.

The coordinator also sets this marker automatically when anti-thrash
detection fires (`ANTI_THRASH_THRESHOLD = 3` consecutive failures with
the same `failure_class`). The investigation marker is visible in `GET
/v1/tasks/:id` under `investigation_marked_at` and
`investigation_reason`.

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

## Dashboard config

These endpoints back the read-only Configuration panel
(`config-providers` / routing / effective config in
`dashboard.html`) and the `relix setup` wizard. They are
local/dev only: no auth at the HTTP layer; production
deployments must put a reverse proxy with auth in front
before exposing the
bridge beyond loopback. See
[`dashboard-redesign.md`](dashboard-redesign.md) for the
full security model.

### `GET /v1/config`

Read-only redacted snapshot of the bridge's effective
config. Lists configured providers (names only, no
secrets) + Telegram configured flag + paths to the
bridge's own config files. Useful for "what did I
actually configure" troubleshooting.

### `GET /v1/config/providers`

Lists every provider in the allowlist (`mock`, `openai`,
`anthropic`, `openrouter`, `xai`, `google`) with redacted
status. Response: `{ "providers": [ProviderStatus, ...] }`.

`ProviderStatus`:

```json
{
  "name": "openai",
  "configured": true,
  "default_model": "gpt-4o",
  "key_preview": "…cdef",         // last 4 chars only; omitted when unset
  "key_set_at": 1700000000        // omitted when unset
}
```

### `GET /v1/config/providers/:name`

Per-provider redacted status. Returns 404 when the name
is not in the allowlist.

### `PUT /v1/config/providers/:name`

Set the provider's API key + optional default model.
Idempotent. Body:

```json
{
  "api_key": "sk-...",
  "default_model": "gpt-4o"
}
```

Response:

```json
{
  "status": { /* ProviderStatus, redacted */ },
  "restart_required": true
}
```

`restart_required` is always `true` today — provider keys
are read at AI controller startup, not at every chat. The
dashboard surfaces this as a "restart AI controller to
apply" notice.

Status codes: 200 on success; 400 (empty api_key); 422
(unknown provider); 500 (disk persist failed).

### `DELETE /v1/config/providers/:name`

Remove the provider entry. Idempotent: deleting an absent
entry returns the redacted "not configured" status.

### `GET /v1/config/telegram`

Redacted Telegram bot status.

```json
{
  "configured": true,
  "token_preview": "…mnop",
  "mode": "polling",
  "token_set_at": 1700000000
}
```

### `PUT /v1/config/telegram`

Set the bot token + delivery mode. Body:

```json
{
  "bot_token": "1234567:...",
  "mode": "polling"
}
```

`mode` accepts `polling` or `webhook` in the schema, but
`webhook` is rejected with 422 today (live HTTPS client
pending).

## Mesh topology

### `GET /v1/topology`

One row per peer in the bridge's `ManifestCache`, with freshness
aggregates. Translation-only — pure projection of an already-
existing cache; the bridge does NOT probe peers here. The
`last_refreshed_at` field reflects only successful
`node.manifest` round-trips from the 60s background refresh
loop. See [`failure-modes.md`](failure-modes.md) for what to do
when a peer is reported `stale` or `expired`.

Response shape:

```json
{
  "peers": [
    {
      "alias": "memory",
      "node_id": "...",
      "node_type": "memory",
      "node_name": "local-memory",
      "manifest_version": 1,
      "capability_count": 3,
      "methods": ["memory.recent_for_session", "memory.search", "memory.write_turn"],
      "last_refreshed_at":      1700000000,
      "last_refreshed_secs_ago": 42,
      "freshness": "fresh"
    }
  ],
  "generated_at": 1700000042
}
```

Sort order: peers are sorted alphabetically by alias, then by
`node_id` (deterministic). Peers without an alias sort after
aliased peers.

Freshness buckets (aligned with the 60s refresh period):

| Bucket | Range | Meaning |
|---|---|---|
| `fresh` | <120s | Within one refresh tick + clock-skew grace. |
| `stale` | <600s | 1–10 missed ticks; peer probably reachable. |
| `expired` | ≥600s | Operator action recommended; routing still uses cached caps. |

CLI parity: `relix-cli topology show [--bridge URL] [--json]
[--warn-after-secs N]`.

Dashboard surface: the Overview panel's "System Health" card
rolls peer count and node-type breakdown up from `/v1/topology`
(`crates/relix-web-bridge/src/dashboard.html`).

### `GET /v1/topology/events?since=<ts>&limit=N`

Server-side ring of recent topology transitions (M23).
The bridge runs a background diff every 5s; transitions
are pushed newest-first to a 500-entry ring. Resets on
bridge restart.

Response:

```json
{
  "events": [
    {
      "ts":             1700003600,
      "kind":           "freshness_changed",
      "alias":          "ai",
      "node_id":        "...",
      "node_type":      "ai",
      "from_freshness": "fresh",
      "to_freshness":   "stale",
      "detail":         "ai fresh → stale"
    }
  ],
  "seq":          42,
  "generated_at": 1700003700
}
```

`kind` values: `joined` / `freshness_changed` / `dropped`.

Dashboard surface: there is no dedicated topology page in the
current console; the Overview panel's "System Health" card shows
current peer state from `/v1/topology`, and this transition ring
is available over HTTP at `/v1/topology/events`.

### `GET /v1/routing`

Capability-to-peer routing snapshot (M33). For each
method in the bridge's manifest cache, returns the
peer the bridge would route to right now under the
first-match-in-cache policy.

```json
{
  "entries": [
    {
      "method":              "ai.chat",
      "alias":               "ai",
      "node_id":             "...",
      "node_type":           "ai",
      "freshness":           "fresh",
      "last_refreshed_at":   1700000000,
      "multiple_candidates": false
    }
  ],
  "generated_at": 1700003700,
  "policy":       "first peer in manifest cache that advertises the method (no scoring, no priority)"
}
```

The `policy` string is the honest description of the
runtime's choice rule — bridges to the operator the
"why this peer" answer without inventing rationale.
`multiple_candidates: true` marks methods where
first-match-in-cache made a non-trivial choice.

Dashboard surface: Execution path panel in task detail.

### `GET /v1/streams`

List currently-open SSE consumers of
`/v1/tasks/:id/events/stream` (M25).

```json
{
  "active": [
    { "id": 7, "task_id": "abc...",
      "opened_at": 1700003600, "age_secs": 142 }
  ],
  "opened_total": 47,
  "generated_at": 1700003742
}
```

Bridge-process-local; resets on restart. Useful for
"which task is being watched right now" operator
visibility — surfaced in the live-streams KPI delta on
the dashboard's overview page.

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
