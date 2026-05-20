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

## Recommended bridge integration (not yet wired)

The bridge today runs flows in-process and **does not** create Tasks.
That's deliberate for this commit — the Coordinator + capabilities +
CLI ship and prove durable persistence end-to-end without changing
hot-path semantics. The bridge integration is a follow-up.

When it lands, the canonical write path will be:

1. Bridge receives `POST /chat`.
2. Bridge calls `task.create(title=truncate(message), flow_template,
   params_json, owner=bridge_subject_id)` on the Coordinator. Stores
   the returned `task_id` in request state.
3. Bridge calls `task.update(task_id, status="running", flow_id=...)`
   when the FlowRunner emits the new `flow_id`.
4. Bridge optionally calls `task.event(task_id, event_type="step",
   payload="<peer.method>")` on each `remote_call` for observability.
   (Cost / value to be measured before turning on by default.)
5. Bridge calls `task.update(task_id, status="completed",
   result=reply, flow_log_path=...)` on success, or
   `task.update(task_id, status="failed", error_kind=..., error_cause=...)`
   on failure.
6. Bridge returns the existing HTTP response with `task_id` added to
   the `relix` provenance block.

Every one of those calls goes through the standard libp2p RPC + the
Coordinator's admission pipeline + the Coordinator's audit log.
Bridge-side latency: ~5 extra RPCs at low single-digit ms each on
loopback, plus the existing flow execution time. Worth measuring; not
yet committed to.

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
