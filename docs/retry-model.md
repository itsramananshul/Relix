# Retry Model

_Version: 0.4.1_

What `retry_policy`, `max_retries`, and `retry_count` mean **today**
versus what they will mean once bounded auto-retry lands. Read this
before relying on any retry behaviour in production.

## TL;DR

The runtime does NOT auto-retry today. The C1/C2 retry columns are
metadata + an explicit operator primitive:

- `retry_policy` (`none` / `once` / `bounded`) — operator declaration
  at `task.create` time. The Coordinator only retries when this
  permits.
- `max_retries` — budget under `bounded`. Ignored under `once` (which
  has implicit budget = 1) and `none` (which refuses retry).
- `retry_count` — incremented by `task.retry` on accepted retries.
  Never decremented automatically.
- `last_failure_class` — pattern-match input for operator triage and
  for the client-side `--force` guard (see
  [`interruption-semantics.md`](interruption-semantics.md)).

To retry a failed task today, an operator uses the C2c retry
primitive:

```bash
# Validate state + budget on the Coordinator, emit
# task.retry_requested, flip status to retrying.
relix-cli task retry --peer ... --task-id $tid

# Re-run the flow. The bridge does NOT do this automatically — the
# operator runs flow-run with the same template + params.
relix-cli flow-run --flow flows/... --identity ... --client-key ... --peers ...
```

`task retry` refuses by default when `last_failure_class` is one of
`policy_denied` / `invalid_args` / `permanent` — re-running a
mutation flow whose last failure indicates the request was correctly
refused (policy_denied) or malformed (invalid_args) almost always
just masks the underlying issue. Pass `--force` to override after
inspecting the flow and chronicle.

The bridge today does no retries on `/chat` — if the SOL flow fails,
the bridge appends a `task.failed` event, writes `status = failed`,
and surfaces the error envelope to the HTTP caller. The caller
decides whether to retry the request.

## What each value means

### `retry_policy = 'none'`

The default. `task.retry` refuses with "retry_policy=none on this
task". Operator scripts that look for retry candidates should skip
these.

### `retry_policy = 'once'`

Implicit budget = 1. The first `task.retry` is accepted; subsequent
ones return `RetryDecision::Exhausted` (which the CLI prints and
the chronicle records as `task.retry_exhausted`).

### `retry_policy = 'bounded'`

Up to `max_retries` retries permitted. The Coordinator's
`task.retry` capability enforces this budget; the bridge does NOT
auto-retry — the operator must request each retry explicitly.

### `max_retries`

Budget under `bounded`. Stored as `INTEGER NOT NULL DEFAULT 0`.
Under `none` or `once` it is ignored.

### `retry_count`

Incremented by `task.retry` on accepted retries. Never decremented.
The Coordinator's `request_retry` clears `error_kind` and
`error_cause` on transition to `retrying` but preserves
`last_failure_class` and `last_failure_reason` for triage.

For programmatic incrementing (e.g. an operator script that
implements its own retry loop), the lower-level
`TaskStore::bump_retry_count` is available and does not validate
state or budget. Prefer `task.retry` unless you need to bypass the
policy check.

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

The `task_attempts` table is fully implemented (see
[`attempt-lineage.md`](attempt-lineage.md)). What bounded auto-retry
still requires is: (a) capability-level idempotency declarations the
bridge can read at retry-decision time, and (b) a documented backoff
curve. Both are post-C1 work.

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
