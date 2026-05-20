# Runtime Lifecycle

The canonical Task lifecycle as understood by the bridge, the CLI,
and the Coordinator's recovery scan. The Coordinator itself enforces
no transitions (`status` is a free string at the database level — see
[`task-runtime.md`](task-runtime.md)); this document is the convention
every Relix-shipped caller follows.

## States

| Status | Who writes it | Terminal? |
|---|---|---|
| `pending` | `task.create` | no |
| `running` | bridge / executor before `FlowRunner::run` | no |
| `retrying` | operator (alpha); future bounded-retry policy (post-C1) | no |
| `interrupted` | Coordinator's recovery scan; operator-triggered `task.recover` | no |
| `awaiting_input` | flow with a future resume primitive (Gate 2; recorded today, not honoured) | no |
| `completed` | bridge on successful `FlowRunner::run` | **yes** |
| `failed` | bridge when `FlowRunner` errors and the failure class is not retryable | **yes** |
| `cancelled` | operator via `task.update --status cancelled` | **yes** |

"Terminal" means no further automatic transitions. An operator can
still write any value over a terminal row (the Coordinator stores
whatever you send); tooling assumes it does not happen.

## Canonical transitions

```
                       task.create
                            │
                            ▼
                       ┌─────────┐
              ┌──────► │ pending │
              │        └────┬────┘
              │             │ bridge stamps started_at
              │             ▼
              │        ┌─────────┐
   operator ──┤        │ running │ ────────────────────┐
   re-runs    │        └────┬────┘                     │
              │             │                          │
              │      ┌──────┼──────────┐               │
              │      │      │          │               │
              │      ▼      ▼          ▼               ▼
              │ completed  failed    awaiting_input  interrupted
              │  (yes)    (yes)        (no — Gate 2)   ▲
              │                                        │
              └────────────────────────────────────────┘
                       (recovery scan, or operator)
```

- `pending → running` is the only path the bridge takes on a fresh
  task; `started_at` is stamped via `COALESCE` on this transition
  (one-shot per row).
- `running → completed | failed | awaiting_input | interrupted` are
  the four ways a `running` row can leave that state. The first three
  come from the executor; `interrupted` is the only one the
  Coordinator owns.
- `interrupted → pending` is the only valid "retry from scratch" today
  — an operator does this via `task.update --status pending` (or by
  shipping a new task; doctrine is loose).
- `completed | failed | cancelled` are terminal in tooling. No
  automatic transition out.

## What "running" actually means

A row is `running` because an executor (the bridge today, anything
else tomorrow) **claimed it and started a FlowRunner**. The
Coordinator has no insight into whether the executor is alive — the
flow runs in-process on the caller, not on the Coordinator. That
asymmetry is why the recovery scan exists:

- Executor still alive + still running: `started_at + max_runtime_secs`
  hasn't elapsed → row stays `running`.
- Executor died mid-flow + caller does not restart: row stays
  `running` forever **unless** `max_runtime_secs` was set, in which
  case the next recovery scan promotes it to `interrupted`.
- Operator wants to give up immediately: `task.update --status
  cancelled`.

The contract: **`running` means "claimed", not "actively
progressing"**. Operators reading dashboards should mentally treat
old `running` rows as "probably dead, no deadline configured" until
they verify.

## What's not a transition

- The Coordinator does not move tasks itself except via the recovery
  scan. There is no leadership election, no executor-takeover, no
  fan-out scheduler.
- The recovery scan does NOT re-launch. It only re-labels the row so
  dashboards stay honest.
- `retry_count` is incremented by `bump_retry_count` (a separate
  method) — not implicitly by `task.update`. The bridge does not call
  it today; it is the seam future bounded-retry logic will use.

## See also

- [`task-runtime.md`](task-runtime.md) — schema + wire format.
- [`attempt-lineage.md`](attempt-lineage.md) — per-attempt rows,
  when they open and close, attempt-aware event vocabulary.
- [`interruption-semantics.md`](interruption-semantics.md) — recovery
  scan contract and the `interrupted` transition.
- [`retry-model.md`](retry-model.md) — what `task.retry` does today
  and how `retry_policy` + `max_retries` gate it.
- [`task-recovery.md`](task-recovery.md) — operator playbook.
