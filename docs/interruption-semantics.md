# Interruption Semantics

_Version: 0.4.1_

What it means when the Coordinator marks a Task `interrupted`, what
the recovery scan does, and — equally important — what it deliberately
does NOT do.

## The recovery scan, exactly

`TaskStore::recover_interrupted(now_secs)`:

1. SELECT all rows where `status = 'running'` AND `max_runtime_secs
   IS NOT NULL` AND `COALESCE(current_attempt.started_at,
   task.started_at) + max_runtime_secs < now_secs`. The scan keys off
   the **current attempt's** `started_at` so a retry whose new attempt
   is within the deadline is not falsely flagged because attempt #1 ran
   out.
2. For each candidate, in a single transaction, UPDATE
   `status = 'interrupted'`, `last_failure_reason = 'deadline_exceeded
   started_at=N max_runtime_secs=N now=N'`, `last_failure_class =
   'timeout'`, and `updated_at = now`. The UPDATE re-checks
   `status = 'running'` so an in-flight `task.update` from a long-lived
   executor cannot be silently overwritten.
3. Close the open `task_attempts` row as `interrupted` with
   `failure_class = 'timeout'`.
4. INSERT `task.interrupted`, `task.attempt_finished`, and
   `task.terminal_summary` events into `task_events` describing the
   transition.
5. COMMIT.
6. Return the list of recovered task ids.

It runs in two places:

- **Coordinator startup**, when `[coordinator] recovery_scan = true`
  (the default). Logged at `warn` level when it changes anything,
  `info` otherwise.
- **`task.recover` capability**, on demand. Same code path, same
  guarantees, same response. Useful when an operator just set
  `max_runtime_secs` on a long-running task and does not want to
  restart the node.

## Boundaries (deliberate)

- **Both fields required.** A `running` row with `started_at` set but
  no `max_runtime_secs` is indistinguishable from a flow making slow
  progress. The scan leaves it alone. Operators who want such rows
  swept must set a ceiling explicitly (`task.create
  --max-runtime-secs N`).
- **Race-guarded.** The UPDATE re-asserts `status = 'running'` in its
  WHERE clause. If the executor wrote `completed` between our SELECT
  and our UPDATE, the row is skipped.
- **Single transaction.** A partial scan crash cannot leave the
  chronicle inconsistent with the row's status field — either both
  the status flip and the `task.interrupted` event commit, or neither
  does.
- **Idempotent.** A second scan after the first one finds nothing
  (the rows are no longer `running`). Tests assert this.
- **Does not touch terminal rows.** `completed`, `failed`, and
  `cancelled` are never re-classified by the scan even if their
  `started_at + max_runtime_secs` is in the past.

## What `interrupted` MEANS to other code

- `task.list --status interrupted` (C1c CLI) shows operators which
  tasks the scan acted on.
- `last_failure_class = 'timeout'` is the wire-level signal. A future
  bounded-retry policy can pattern-match this without parsing the
  reason string.
- The original `latest_flow_id` and `latest_flow_log_path` (if any)
  are NOT cleared — the previous attempt's flow log is still pointed
  at, so operator forensics work.

## What `interrupted` does NOT mean

- It does NOT mean the executor is dead. The Coordinator has no way
  to verify that. It means "we said this task had a deadline and the
  deadline has passed; we are no longer willing to claim it's
  progressing." The executor process may very well still be running
  the flow — it just can't tell the Coordinator anymore (or hasn't
  yet).
- It does NOT trigger a re-run. The operator decides:
  - `task.update --status pending` to mark the row eligible for
    another `relix-cli flow-run` (or the next bridge call that picks
    it up).
  - `task.update --status cancelled` to terminate it.
  - Leave it for forensic investigation.
- It does NOT propagate to the executor. If the executor was alive
  and writing the eventual `completed` after the scan moved the row
  to `interrupted`, the `completed` write succeeds (no race guard on
  the executor's side) and ends up "on top". That's fine — the
  chronicle still records the `task.interrupted` event so the order
  is reconstructible.

## When NOT to enable the recovery scan

`[coordinator] recovery_scan = false` is a real configuration. Use it
when:

- You want stale `running` rows preserved for forensic investigation
  (you're chasing why an executor died and want every clue).
- You're running a workload where `max_runtime_secs` is unreliable
  (e.g. you set it to 3600 but flows routinely take 4000 seconds).

In both cases the on-demand `task.recover` capability still exists
and an operator can fire it manually when ready.

## See also

- [`runtime-lifecycle.md`](runtime-lifecycle.md) — the full
  transition diagram.
- [`task-recovery.md`](task-recovery.md) — operator playbook for
  acting on the scan's output.
- [`task-runtime.md`](task-runtime.md) — schema and wire format.
