# Task Runtime

The Task is Relix's durable orchestration unit. One Task = one logical
piece of work — a chat turn, a tool flow, a future scheduled agent run
— with a stable id, a status field, and an event chronicle.

This document covers the schema, the wire format of the five `task.*`
capabilities, and the state-transition convention. For the *peer* that
owns the ledger see [`coordinator.md`](coordinator.md); for what
"checkpointed re-run" actually delivers see
[`replay-model.md`](replay-model.md).

## SQLite schema

The Coordinator owns one SQLite database at `[coordinator] db_path`.

```sql
CREATE TABLE tasks (
    task_id              TEXT PRIMARY KEY,         -- 32 hex chars
    title                TEXT NOT NULL,
    status               TEXT NOT NULL,            -- 'pending' / 'running' / ...
    owner_subject_id     TEXT NOT NULL,            -- hex NodeId of the requesting identity
    flow_template        TEXT NOT NULL,            -- e.g. 'chat_template.sol'
    params_json          TEXT NOT NULL,            -- caller-supplied; Coordinator does not parse
    latest_result        TEXT,                     -- final reply on success
    latest_flow_id       TEXT,                     -- 32 hex; points into dev-data/flow-runner/flows
    latest_flow_log_path TEXT,                     -- absolute or RELIX_DATA_DIR-relative
    error_kind           INTEGER,                  -- relix_core::types::error_kinds when failed
    error_cause          TEXT,
    created_at           INTEGER NOT NULL,         -- unix seconds
    updated_at           INTEGER NOT NULL
);
CREATE INDEX tasks_updated ON tasks(updated_at DESC);

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

Two tables, one foreign key, no triggers. The deliberate minimalism is
why this can land in a single commit and stay honest about scope.

## Status convention

`status` is a free string at the database level. The convention used
by the bridge and recommended for other callers:

| Status | Meaning |
|---|---|
| `pending` | Task created, no execution attempted yet. |
| `running` | Some executor took ownership and is running the flow now. |
| `completed` | Final attempt succeeded. `latest_result` holds the reply. |
| `failed` | Final attempt failed. `error_kind` + `error_cause` filled. |
| `abandoned` | Operator gave up — executor died mid-`running` and the operator does not want to retry. |

The Coordinator does **not** enforce transitions. A caller can write
`status = "blueberry"` and the Coordinator will store it. CLI tooling
assumes the convention; nothing breaks if you don't, but tooling will
display whatever you write. State-machine enforcement is a Gate 2 item.

## Wire format (every capability, exact)

All args and returns are UTF-8 strings (alpha SIMP-016). Pipe-delim is
the per-method convention; empty fields skip a column.

### `task.create`

Request: `title|flow_template|params_json|owner_subject_id`

- `title` and `flow_template` are required.
- `params_json` is opaque — JSON encouraged.
- `owner_subject_id` defaults to the caller's verified `subject_id`.

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

Request: `task_id|status|result|flow_id|flow_log_path|error_kind|error_cause`

Any of `status` / `result` / `flow_id` / `flow_log_path` / `error_kind` /
`error_cause` may be empty — the Coordinator preserves the existing
column value for empty fields. Non-empty `error_kind` must parse as an
integer.

Response: `ok\n` on success; `INVALID_ARGS` with cause
`task.update: not found: <id>` when the task id is unknown.

### `task.event`

Request: `task_id|event_type|payload`

Appends one event. `payload` may itself contain `|`. The Coordinator
verifies the task exists (rejects with `NotFound` otherwise) so events
don't accumulate as orphans.

Response: the new `event_id` as a decimal integer.

Use for: checkpoint markers (`event_type=checkpoint, payload=step=3`),
attempt boundaries (`event_type=attempt_start`), or any other
chronological observation the caller wants to remember alongside the
Task.

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

Request: `` (empty = default 50) or `<limit>` (integer).

Response: one task per line, tab-delimited:
```
<task_id>\t<status>\t<title>
```

Sorted by `updated_at DESC` so the most recently touched task is first.
The Coordinator clamps `limit` to `[coordinator] max_list` (default
200).

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
3. Bridge appends `flow_selected` (with the template path). For the
   tool flow it also appends `tool_target` (URL) and `tool_invoked`
   (`tool.web_fetch`).
4. Bridge runs the SOL flow through the existing FlowRunner. No
   per-`remote_call` events are written today — the bridge can't see
   inside the VM's RemoteCall opcodes from where it's standing. Per-
   call detail is fully available in `dev-data/flow-runner/flows/<flow_id>.log`
   which `task.latest_flow_log_path` points at.
5. On success: bridge appends `flow_completed` (with a truncated
   reply excerpt, ≤200 chars) and calls `task.update(status=completed,
   result=excerpt, flow_id=..., flow_log_path=...)`.
6. On failure: bridge appends `flow_failed` (with the cause) and calls
   `task.update(status=failed, error_kind=..., error_cause=...)`.
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

- No multi-attempt tracking — only `latest_*` columns. Previous
  attempts live in the flow logs the bridge wrote, not in the
  Coordinator's database.
- No automatic "abandoned" sweep on Coordinator restart.
- No state-machine enforcement — `status` is a free string.
- Single Coordinator instance only. Leadership election + multi-leader
  reconciliation is Gate 2.
- `params_json` is opaque to the Coordinator — no validation, no
  schema.

## See also

- [`coordinator.md`](coordinator.md) — the peer.
- [`replay-model.md`](replay-model.md) — what "checkpointed re-run"
  actually delivers.
- [`architecture.md`](architecture.md) — where the Coordinator sits in
  the request flow.
