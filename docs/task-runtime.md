# Task Runtime

_Version: 0.4.1_

The Task is Relix's durable orchestration unit. One Task = one logical
piece of work — a chat turn, a tool flow, a future scheduled agent run
— with a stable id, a status field, and an event chronicle.

This document covers the schema, the wire format of the five `task.*`
capabilities, and the state-transition convention. For the *peer* that
owns the ledger see [`coordination.md`](coordination.md); for what
"checkpointed re-run" actually delivers see
[`replay-model.md`](replay-model.md).

## SQLite schema

The Coordinator owns one SQLite database at `[coordinator] db_path`.

```sql
CREATE TABLE tasks (
    task_id              TEXT PRIMARY KEY,         -- 32 hex chars
    title                TEXT NOT NULL,
    status               TEXT NOT NULL,            -- see "Status convention" below
    owner_subject_id     TEXT NOT NULL,            -- hex NodeId of the requesting identity
    flow_template        TEXT NOT NULL,            -- e.g. 'chat_template.sol'
    params_json          TEXT NOT NULL,            -- caller-supplied; Coordinator does not parse
    latest_result        TEXT,                     -- final reply on success
    latest_flow_id       TEXT,                     -- 32 hex; points into dev-data/flow-runner/flows
    latest_flow_log_path TEXT,                     -- absolute or RELIX_DATA_DIR-relative
    error_kind           INTEGER,                  -- relix_core::types::error_kinds when failed
    error_cause          TEXT,
    created_at           INTEGER NOT NULL,         -- unix seconds
    updated_at           INTEGER NOT NULL,
    -- C1: retry + recovery metadata.
    retry_count          INTEGER NOT NULL DEFAULT 0,
    retry_policy         TEXT    NOT NULL DEFAULT 'none', -- 'none'|'once'|'bounded'
    max_retries          INTEGER NOT NULL DEFAULT 0,
    max_runtime_secs     INTEGER,                  -- recovery-scan deadline; NULL = no ceiling
    last_failure_reason  TEXT,                     -- mirror of error_cause; survives 'retrying'
    last_failure_class   TEXT,                     -- see interruption-semantics.md
    started_at           INTEGER                   -- stamped on first transition to 'running'
);
CREATE INDEX tasks_updated ON tasks(updated_at DESC);
CREATE INDEX tasks_status  ON tasks(status);

CREATE TABLE task_events (
    event_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT    NOT NULL,
    ts         INTEGER NOT NULL,
    event_type TEXT    NOT NULL,                   -- caller-defined ('checkpoint', 'step', ...)
    payload    TEXT    NOT NULL,                   -- free-form
    FOREIGN KEY (task_id) REFERENCES tasks(task_id)
);
CREATE INDEX task_events_task ON task_events(task_id, event_id);
```

The base schema above; the live database also contains
`task_attempts` (C2a per-attempt timeline — see
[`attempt-lineage.md`](attempt-lineage.md)), `task_edges`
(cross-task lineage edges), and `task_todos` (per-task todo lists).
All columns beyond the base set are added via idempotent
`ALTER TABLE ADD COLUMN` migrations at startup, so older databases
upgrade in place. Migration version is tracked in `_relix_migrations`.

## Status convention

`status` is a free string at the database level. The convention used
by the bridge and recommended for other callers (C1 expansion):

| Status | Meaning |
|---|---|
| `pending` | Task created, no execution attempted yet. |
| `running` | Some executor took ownership and is running the flow now. |
| `retrying` | Previous attempt failed; another attempt is scheduled (operator-initiated today). |
| `interrupted` | Executor died or `max_runtime_secs` was exceeded. The recovery scan owns this transition. |
| `awaiting_input` | Flow paused on an external dependency. Recorded today; resume primitive is Gate 2. |
| `completed` | Final attempt succeeded. `latest_result` holds the reply. |
| `failed` | Final attempt failed and the task will not retry. `last_failure_class` + `last_failure_reason` filled. |
| `cancelled` | Operator explicitly cancelled an active task. |

The Coordinator enforces transitions via `is_allowed_transition(from,
to)`. A caller attempting an illegal transition (e.g. `completed →
running`) receives `INVALID_ARGS`. Unknown status values used in
`task.update` are only accepted if they follow a valid from-status.
CLI tooling assumes the canonical vocabulary; operators who write
non-standard values will see them displayed verbatim but some
dashboard features may not recognise them.

See [`runtime-lifecycle.md`](runtime-lifecycle.md) for the canonical
transition diagram, [`interruption-semantics.md`](interruption-semantics.md)
for the recovery-scan contract, and [`retry-model.md`](retry-model.md)
for what `retry_policy` / `max_retries` mean today (versus when bounded
auto-retry lands).

## Wire format (every capability, exact)

All args and returns are UTF-8 strings (alpha SIMP-016). Pipe-delim is
the per-method convention; empty fields skip a column.

### `task.create`

Request: `title|flow_template|params_json|owner_subject_id|retry_policy|max_retries|max_runtime_secs`

- `title` and `flow_template` are required.
- `params_json` is opaque — JSON encouraged.
- `owner_subject_id` defaults to the caller's verified `subject_id`.
- `retry_policy` (optional) is `none` (default), `once`, or `bounded`.
- `max_retries` (optional, int) applies under `bounded`; default `0`.
- `max_runtime_secs` (optional, int > 0) sets the recovery-scan
  deadline. Omit for no ceiling.

The C1 trailer (retry_policy + max_retries + max_runtime_secs) is
optional — older callers that omit it keep working unchanged. The
Coordinator does NOT auto-retry today; these knobs are metadata for
operators and for future bounded-retry logic (see
[`retry-model.md`](retry-model.md)).

Response: 32 hex chars (the new `task_id`).

```bash
relix-cli task create \
    --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --title 'doc walkthrough' \
    --flow-template chat_template.sol \
    --params-json '{"session":"demo"}'
# -> task_id: 2b52a499bbce34a5a64746273e9af79b
```

### `task.update`

Request: `task_id|status|result|flow_id|flow_log_path|error_kind|error_cause|failure_class|trace_id`

Any field may be empty — the Coordinator preserves the existing column
value for empty fields. Non-empty `error_kind` must parse as an
integer. Non-empty `failure_class` must be one of `transient` /
`permanent` / `policy_denied` / `invalid_args` / `timeout` /
`unavailable`. Non-empty `trace_id` must be 32 hex chars; otherwise
rejected with `INVALID_ARGS`.

Side effects:

- `status -> running` with no open attempt opens a new attempt row,
  stamps its `started_at`, persists `trace_id` (if supplied), and
  emits `task.attempt_started`. Also stamps the task-level
  `started_at` on the first-ever `running` transition (one-shot via
  COALESCE; preserves the "first started" timestamp).
- `status -> running` while an attempt is already open is a no-op at
  the attempt level — `trace_id` is NOT clobbered.
- `status -> completed | failed | cancelled` closes the open attempt
  with the supplied outcome columns (flow_id, flow_log_path,
  error_*, failure_class) and emits `task.attempt_finished`. No-ops
  cleanly when no attempt is open (preserves the pre-C2a pending →
  completed shortcut).
- Setting `error_cause` also mirrors it to `last_failure_reason` so
  the cause survives a later `retrying` transition that clears
  `error_cause` itself.

Response: `ok\n` on success; `INVALID_ARGS` with cause
`task.update: not found: <id>` when the task id is unknown.

### `task.attempts`

Request: `task_id`

Response: one tab-delimited line per attempt, in chronological order:
`<attempt_num>\t<status>\t<started_at>\t<finished_at|->\t<failure_class|->\t<flow_id|->\n`

Empty body when the task has no attempts yet (created but never
transitioned to `running`). The Coordinator's `task.update` opens /
closes attempts implicitly; `task.attempts` is the read-only view.

### `task.recover`

Request: empty.

Runs the recovery scan immediately. Equivalent to what the
Coordinator does at startup when `[coordinator] recovery_scan = true`:
promotes overdue `running` tasks to `interrupted` and appends a
`task.interrupted` event to each. See
[`interruption-semantics.md`](interruption-semantics.md).

Response: one task id per line for each recovered task, plus a
trailing `recovered=N\n` line.

### `task.event`

Request: `task_id|event_type|payload`

Appends one event. `payload` may itself contain `|`. The Coordinator
verifies the task exists (rejects with `NotFound` otherwise) so events
don't accumulate as orphans.

Response: the new `event_id` as a decimal integer.

Use for: checkpoint markers, attempt boundaries, or any other
chronological observation the caller wants to remember alongside the
Task.

#### Standard event vocabulary

`event_type` is a free string, but Relix-shipped callers (the bridge,
the recovery scan) use this vocabulary. Stick to it — drift becomes
expensive once dashboards start pattern-matching on these names.

| `event_type` | Emitted by | Payload convention |
|---|---|---|
| `task.create` | Coordinator, on task creation (chronicle event auto-emitted) | flow_template path |
| `flow.started` | bridge, before invoking `FlowRunner::run` | flow_template path |
| `capability.invoked` | bridge, before a capability call it knows about | `method=... target=...` (target optional) |
| `capability.completed` | (reserved; not emitted yet) | capability method |
| `capability.failed` | (reserved; not emitted yet) | capability method + cause |
| `task.interrupted` | Coordinator recovery scan | `started_at=N max_runtime_secs=N now=N reason=deadline_exceeded` |
| `task.retry_started` | (reserved; not emitted yet — see [`retry-model.md`](retry-model.md)) | attempt number |
| `task.completed` | bridge, on successful flow | truncated reply (≤200 chars) |
| `task.failed` | bridge, on failed flow | error cause string |

Reserved names land when the corresponding logic does. The bridge
does NOT emit per-`remote_call` `capability.invoked` /
`capability.completed` today — those would need RemoteCall callbacks
the bridge can't observe from outside the VM. Per-call detail still
lives in the per-flow event log on disk.

### `task.get`

Request: `task_id`

Response: a stable multi-line `key=value` block followed by `events=[...]`
as a JSON array. Format chosen for grep-friendliness in CLI output and
parseability if you want to feed it back through `jq` (just slice off
`events=` and parse). Example:

```
task_id=2b52a499bbce34a5a64746273e9af79b
title=doc walkthrough
status=running
owner_subject_id=814a75e836dbfd2d5bec972fb537df4ea5e50f69e2a68b3717b4b879ded3d46d
flow_template=chat_template.sol
params_json={"session":"demo"}
created_at=1779235935
updated_at=1779235935
event_count=1
events=[{"id":1,"ts":1779235935,"type":"checkpoint","payload":"memory.write_turn ok"}]
```

### `task.list`

Request: `` (empty = defaults) or `limit|offset|status`
(all optional; defaults: limit 50, offset 0, all statuses).

Response: one task per line, tab-delimited:
```
<task_id>\t<status>\t<title>
```

Sorted by `updated_at DESC` so the most recently touched task is first.
The Coordinator clamps `limit` to `[coordinator] max_list` (default
200). For stable pagination under concurrent writes, prefer
`task.list_cursor`.

## Bridge integration (B1, wired)

The bridge persists every chat request as a Task. The wiring is
configured by an optional `[coordinator] alias = "..."` section in
the bridge TOML; when absent, the bridge runs without persistence and
nothing breaks.

The canonical write path per request:

1. Bridge receives `POST /chat`, `/chat_with_tool`, or
   `POST /v1/chat/completions` (the OpenAI shim).
2. Bridge calls `task.create(title=truncate("chat: ..."),
   flow_template=<template path>, params_json=<JSON of req fields>,
   owner=<empty -> caller subject_id>)`. The Coordinator returns a
   task_id; the bridge stores it in request state.
3. Bridge appends `task.created` and then `flow.started` (both with
   the template path). For the tool flow it also appends
   `capability.invoked` with payload `method=tool.web_fetch
   target=<url>`.
4. Bridge runs the SOL flow through the existing FlowRunner. No
   per-`remote_call` events are written today — the bridge can't see
   inside the VM's RemoteCall opcodes from where it's standing. Per-
   call detail is fully available in `dev-data/flow-runner/flows/<flow_id>.log`
   which `task.latest_flow_log_path` points at.
5. On success: bridge appends `task.completed` (with a truncated
   reply excerpt, ≤200 chars) and calls `task.update(status=completed,
   result=excerpt, flow_id=..., flow_log_path=...)`.
6. On failure: bridge appends `task.failed` (with the cause) and
   calls `task.update(status=failed, error_kind=...,
   error_cause=..., failure_class=...)`.
7. Bridge returns the HTTP response with `task_id` added to the JSON
   (`ChatResponse.task_id`) or the `relix.task_id` provenance field
   (OpenAI shim). The field is omitted entirely when persistence was
   not wired or failed.

**All `task.*` calls are fail-soft.** Every method on the bridge's
`TaskRecorder` returns silently on Coordinator failure — a `WARN` is
logged and the chat continues. The user's request never blocks on
Coordinator availability. Live-verified: kill the Coordinator
process mid-session, send another `/chat` — the response comes back
normally with `task_id` absent and the bridge log shows the structured
WARN.

That fail-soft behavior is the local/dev default, not the production ceiling.
Production deployments can opt into fail-closed task creation:

```toml
[coordinator]
alias = "coordinator"
required = true
```

With `required = true`, the bridge refuses to start if the coordinator alias
is not discovered at boot. If `task.create` fails during a request, the bridge
returns `503 Service Unavailable` before dispatching the SOL flow. This prevents
anonymous high-risk execution with no durable task record.

Cost: 3-5 additional `/relix/rpc/1` round-trips per chat request,
each loopback + admission-pipeline + SQLite-insert latency (single
digit ms on a local mesh). Worth it for the durable lineage on
operator request triage.

## `relix-cli flow-run` and Tasks

The CLI's `flow-run` path does not currently create Tasks either. If
an operator wants durable records of CLI flow runs, they can wrap the
call:

```bash
$tid = relix-cli task create --peer ... --title 'manual run' \
    --flow-template my-flow.sol --params-json '...'
relix-cli task update --peer ... --task-id $tid --status running
relix-cli flow-run --flow flows/my-flow.sol --identity ... --client-key ... --peers ...
# inspect the printed flow_log, then:
relix-cli task update --peer ... --task-id $tid \
    --status completed --result '...' --flow-id <hex> --flow-log-path <path>
```

A `flow-run --task` flag that does this automatically is a candidate
follow-up.

## Limitations

See [`current-limitations.md`](current-limitations.md). Highlights:

- No bounded auto-retry — `retry_policy` + `max_retries` are metadata
  for operators and the explicit `task.retry` primitive; no background
  auto-retry loop exists today. See [`retry-model.md`](retry-model.md).
- The recovery scan promotes `running` past `max_runtime_secs` to
  `interrupted` but does NOT re-launch. See
  [`interruption-semantics.md`](interruption-semantics.md).
- Single Coordinator instance only. Leadership election + multi-leader
  reconciliation is Gate 2.
- `params_json` is opaque to the Coordinator — no validation, no
  schema.

## See also

- [`coordination.md`](coordination.md) — the peer.
- [`replay-model.md`](replay-model.md) — what "checkpointed re-run"
  actually delivers.
- [`runtime-lifecycle.md`](runtime-lifecycle.md) — canonical status
  transitions.
- [`task-recovery.md`](task-recovery.md) — operator playbook for
  recovering interrupted tasks.
- [`retry-model.md`](retry-model.md) — what retry knobs mean today.
- [`interruption-semantics.md`](interruption-semantics.md) — recovery
  scan contract.
- [`architecture.md`](architecture.md) — where the Coordinator sits in
  the request flow.
