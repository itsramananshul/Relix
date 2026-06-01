# Attempt Lineage

_Version: 0.4.1_

What a "task attempt" is, where it's stored, what events it emits,
and how the recovery scan, the retry primitive, and operator tooling
interact with it. The per-attempt timeline (C2a) is fully implemented.

## TL;DR

Every time a Task transitions to `running`, the Coordinator opens a
new attempt row in `task_attempts`. When the attempt ends
(`completed` / `failed` / `cancelled` / `interrupted`), the row
gets a `finished_at` plus the outcome columns. The chronicle
records the boundary with `task.attempt_started` and
`task.attempt_finished` events.

Operators see per-attempt detail via:

- `relix-cli task attempts --task-id <hex>` — tabular dump.
- `relix-cli task get --pretty` — inlines an `attempts:` block and
  groups the chronology with `---- attempt #N ----` separators.
- The `task.attempts` capability — wire-level read for scripts.

## What a row contains

```sql
CREATE TABLE task_attempts (
    attempt_id    INTEGER PRIMARY KEY AUTOINCREMENT, -- monotonic global
    task_id       TEXT    NOT NULL,
    attempt_num   INTEGER NOT NULL,                  -- 1-based per task
    started_at    INTEGER NOT NULL,                  -- unix secs
    finished_at   INTEGER,                           -- NULL while running
    status        TEXT    NOT NULL,                  -- running|completed|failed|cancelled|interrupted
    flow_id       TEXT,                              -- per-attempt; differs across retries
    flow_log_path TEXT,                              -- pointer into dev-data/flow-runner/flows
    trace_id      TEXT,                              -- C2b.1 correlation id
    error_kind    INTEGER,
    error_cause   TEXT,
    failure_class TEXT,                              -- transient|timeout|...
    UNIQUE (task_id, attempt_num)
);
```

The `tasks` row carries cached pointers — `attempt_count`,
`current_attempt_id` — for fast "what's the current attempt" lookup
without joining. They are NOT the source of truth; the table is.

## When attempts open and close

Implicit, driven by `task.update` status transitions:

| Transition | Effect |
|---|---|
| `pending → running` | Opens attempt #1. Stamps `started_at` + `trace_id`. Emits `task.attempt_started`. |
| `running → running` | No-op at the attempt level (idempotent on the bridge side). `trace_id` is NOT clobbered. |
| `running → completed \| failed \| cancelled` | Closes the open attempt with the supplied outcome columns. Emits `task.attempt_finished`. |
| `failed → retrying` | No attempt activity. `error_kind` / `error_cause` cleared on the task row (but `last_failure_class` / `last_failure_reason` preserved). |
| `retrying → running` | Opens attempt #2 (or N+1). Same as the first running transition. |
| `running → interrupted` (via recovery scan) | Closes the open attempt as `interrupted` with `failure_class = 'timeout'`. Emits `task.attempt_finished`. |

Pre-C2a callers that go `pending → completed` directly (skipping the
`running` stamp) still work — the attempts table stays empty for
that task, and nothing breaks.

## Attempt-aware events

Standardized C2a event-type vocabulary on the chronicle:

| event_type | When | Payload format |
|---|---|---|
| `task.attempt_started` | Open attempt | `attempt_id=N attempt_num=M [trace_id=hex]` |
| `task.attempt_finished` | Close attempt | `attempt_id=N status=...` (with `failure_class=...` when set) |
| `task.retry_requested` | `task.retry` accepts | `attempt=N of_budget=M policy=... prior_class=...` |
| `task.retry_exhausted` | `task.retry` rejects (budget hit) | `retry_count=N budget=M policy=...` |
| `task.interrupted` | Recovery scan flipped task | `started_at=N max_runtime_secs=M now=K reason=deadline_exceeded` |

These names are stable. `attempt_started` / `attempt_finished` are
emitted by the Coordinator's `update` helpers; `retry_requested` /
`retry_exhausted` by `task.retry`; `interrupted` by the recovery
scan. The bridge's `task.created` and `flow.started` are emitted
before the attempt opens.

## How the recovery scan uses attempts

[`interruption-semantics.md`](interruption-semantics.md) covers the
scan's contract. The C2a expansion:

- The scan keys off the **current attempt's** `started_at`
  (`COALESCE(attempt.started_at, task.started_at)`). A retry whose
  new attempt is inside the deadline is NOT falsely flagged because
  attempt 1 ran out.
- On flip, the scan closes the open attempt row as `interrupted`
  with `failure_class = 'timeout'` and emits both `task.interrupted`
  AND `task.attempt_finished`. Attempt-level forensics survive.

## Per-attempt flow lineage

Each attempt carries its own `flow_id` + `flow_log_path` + `trace_id`.
Across retries, these differ — every `FlowRunner::run` creates a
fresh flow log. The Coordinator's chain is the only place that
records "which flow log belonged to which attempt":

```
Task   ──┬── attempt #1 ── flow_id=A ── trace_id=T1 ── dev-data/flow-runner/flows/A.log
         ├── attempt #2 ── flow_id=B ── trace_id=T2 ── ...
         └── attempt #3 ── flow_id=C ── trace_id=T3 ── ...
```

For operators: `task get --pretty` shows this mapping. For scripts:
`task.attempts` returns it in tab-delimited form.

## What this does NOT do

- **No re-launch.** Closing an attempt as failed/interrupted does
  not re-open it. The operator (or the bridge, on a fresh request)
  drives a new `running` transition that opens attempt N+1.
- **No leasing.** There is no "executor X owns attempt #N until
  timestamp T" record. The Coordinator only knows whether the
  current attempt has finished (`finished_at IS NOT NULL`). A
  dead-but-not-timed-out executor presents identically to a slow-
  but-alive one until `max_runtime_secs` kicks in.
- **No automatic flow execution.** `task.retry` updates metadata;
  it doesn't ask anyone to re-run the flow. That's the operator's
  job today (via `relix-cli flow-run` or by re-driving through the
  bridge). Bounded auto-retry is Gate 2 work.

## See also

- [`runtime-lifecycle.md`](runtime-lifecycle.md) — status transitions
  including the retry cycle.
- [`task-runtime.md`](task-runtime.md) — wire format and schema.
- [`task-recovery.md`](task-recovery.md) — operator playbook,
  including how to read `task get --pretty` and act on it.
- [`retry-model.md`](retry-model.md) — what `task.retry` does and
  doesn't do.
- [`interruption-semantics.md`](interruption-semantics.md) — what
  the recovery scan touches (and what it leaves alone).
