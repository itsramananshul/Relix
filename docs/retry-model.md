# Retry Model

What `retry_policy`, `max_retries`, and `retry_count` mean **today**
versus what they will mean once bounded auto-retry lands. Read this
before relying on any retry behaviour in production.

## TL;DR

The runtime does NOT auto-retry today. The C1 retry columns are
metadata:

- `retry_policy` (`none` / `once` / `bounded`) — operator hint.
- `max_retries` — operator hint, only meaningful under `bounded`.
- `retry_count` — bumped explicitly via `bump_retry_count`. Nothing in
  the alpha calls it, but the seam exists for future bounded-retry
  logic and for operator scripts.
- `last_failure_class` — the wire-level signal a future policy will
  key off (see [`interruption-semantics.md`](interruption-semantics.md)).

If you want a Task retried today, an operator (or a script) does it
manually:

```bash
# Mark a failed task as a candidate for re-run.
relix-cli task update --peer ... --task-id $tid --status retrying

# Re-run the flow yourself; pass the same flow_template + params_json.
relix-cli flow-run --flow flows/... --identity ... --client-key ... --peers ...

# Roll the row to its final state.
relix-cli task update --peer ... --task-id $tid --status completed --result '...'
```

The bridge today does no retries on `/chat` — if the SOL flow fails,
the bridge appends a `task.failed` event, writes `status = failed`,
and surfaces the error envelope to the HTTP caller. The caller decides whether to
retry the request.

## What each value means

### `retry_policy = 'none'`

The default. No retry intent recorded. Operator scripts that look for
retry candidates should skip these.

### `retry_policy = 'once'`

Operator marker: "if this fails with a `transient` or `timeout`
class, one re-run is permitted." Nothing in the alpha acts on this —
it is a hint for the operator playbook (or for the bounded-retry
logic when it lands).

### `retry_policy = 'bounded'`

Operator marker: "up to `max_retries` re-runs permitted for
`transient` or `timeout` class failures." Same caveat: nothing in the
alpha acts on this yet.

### `max_retries`

Only meaningful under `bounded`. Stored as `INTEGER NOT NULL DEFAULT
0`; the Coordinator does not validate it (a value of `1` is
effectively `once`). Operators use it to express intent; future
policy will enforce it.

### `retry_count`

Bumped via `TaskStore::bump_retry_count(task_id) -> i64` returning
the new count. The bridge does not bump it today. The expected wiring
when bounded auto-retry lands:

1. Bridge sees a `transient` / `timeout` failure on attempt N.
2. Bridge reads `retry_policy` and `max_retries` from the task.
3. If policy permits, bridge appends a `retry.started` event, calls
   `bump_retry_count`, and re-runs the flow with the same template
   + params.
4. On success: terminal `completed`. On final failure: `status =
   failed`, `last_failure_class` set to the most recent class.

`retry_count` is never decremented and is never automatically reset
— an operator who re-uses a Task for a fresh attempt should write
`task.update --retry-count 0` (note: no such flag today; would need
to be added at the same time as the bounded-retry logic).

### `last_failure_class`

The class lives on the row across `retrying` transitions. When the
bridge calls `task.update --status retrying`, it deliberately does
not clear `last_failure_class` or `last_failure_reason` — they are
the "why we're retrying" record. Only a fresh `task.update --status
running --error-cause ''` clears the cause (and even then,
`last_failure_reason` keeps its previous value because the Coordinator
only mirrors **non-empty** `error_cause` into the failure-reason
column).

## Why no auto-retry today

Two reasons, both load-bearing:

1. **The SOL VM is synchronous.** A retry today is "re-run the flow
   from the start with the same params" — which is fine for
   idempotent flows but corrupting for any flow that wrote state
   (memory, FS, external API) before failing. The bridge has no way
   to know which side a given flow is on without a per-capability
   idempotency contract, and the alpha hasn't shipped that contract
   broadly enough.
2. **Per-attempt event-log isolation.** Today every `FlowRunner::run`
   creates a fresh `flow_id` and writes its own event log. A
   bridge-side retry would clobber `latest_flow_id` /
   `latest_flow_log_path` on the second attempt unless the schema
   started carrying a list of historical attempts. That's a real
   schema change, and gating bounded-retry behind it is the right
   call.

When bounded auto-retry lands it will require: (a) a `task_attempts`
table or equivalent, (b) capability-level idempotency declarations
the bridge can read at retry-decision time, and (c) a documented
backoff curve. This is post-C1 work; C1 just establishes the
vocabulary.

## What operators can rely on today

- The metadata fields survive Coordinator restarts (durable SQLite).
- `last_failure_class` is correct: the bridge writes it on every
  `failed` transition via `FailureClass::from_kind` (see
  [`task-runtime.md`](task-runtime.md)). Operators can pattern-match
  on it.
- The recovery scan writes `last_failure_class = 'timeout'` when it
  flips a row to `interrupted`. So "find me everything to consider
  retrying" today is:

  ```bash
  relix-cli task list --status interrupted   # timeout class
  relix-cli task list --status failed        # everything else
  ```

  Then inspect `last_failure_class` on each to decide whether to
  re-run.

## See also

- [`runtime-lifecycle.md`](runtime-lifecycle.md) — where `retrying`
  fits in the status convention.
- [`interruption-semantics.md`](interruption-semantics.md) — how
  `timeout` failures get classified.
- [`task-recovery.md`](task-recovery.md) — operator playbook.
- [`task-runtime.md`](task-runtime.md) — the wire format for
  `task.update --failure-class`.
