# Chronicle Retention + Compaction (Design)

The Coordinator's `task_events` table grows unbounded by design.
This document is the **design contract** for how retention,
compaction, and operator export should work once they're
implemented. It's intentionally docs-first per the S5 directive
— no destructive deletion has been built. Once a strategy is
greenlit, the implementation lands as small, additive,
operator-controlled primitives.

## Why this matters

A live mesh writes events constantly:

- Every `/chat` hits `task.created` + `flow.started` +
  `task.attempt_started` + `task.attempt_finished` +
  `task.completed` — five events per request, minimum.
- Tool flows add `capability.invoked`.
- Retried tasks add `task.retry_requested`,
  `task.attempt_started`, `task.attempt_finished` per cycle.
- Operator scripts and channels (e.g. future Telegram) add
  their own `ops.*` events.

A mesh handling 10K chat requests / day generates ~50K events
/ day at minimum. After 30 days that's 1.5M rows in `task_events`
+ comparable in attempt rows. SQLite handles this fine in raw
terms, but operator queries (`task get` with the full chronicle,
`task list` join scans) and on-disk size grow with it.

Retention closes that loop without removing forensic
capability.

## Hard architectural constraints

Any retention/compaction implementation MUST satisfy:

### R1 — Operator-controlled, never automatic

The Coordinator does not delete rows by default. Retention is a
**configured** opt-in (`[coordinator]` knobs) or an **explicit**
operator capability call. There is no hidden background reaper.

### R2 — Audit-preserving

The per-peer `audit.log` files + per-flow event logs on disk
remain untouched by retention. Those are signed + hash-chained
attested records, distinct from the Coordinator's metadata.
Retention only affects the Coordinator's `task_events` /
`task_attempts` / `tasks` tables — the auditable forensic
trail survives.

### R3 — Idempotent + reversible-where-feasible

A retention pass that deletes nothing should leave the schema
identical. A retention pass that removes events should also
write a `task.compacted` event (or similar) recording what was
removed, so a future operator scan can still see *something
happened here*.

### R4 — Bounded per-pass

A single retention pass operates on a bounded subset (by row
count or by time window). Long-running deletion-of-everything
queries can lock SQLite for unacceptable durations on a live
ledger. Each pass commits its own transaction.

### R5 — No coupling with active tasks

Retention never touches a row whose `status` is `running`,
`pending`, `retrying`, or `awaiting_input`. Only terminal states
(`completed` / `failed` / `cancelled` / `interrupted`) are
candidates.

### R6 — Operator export before delete

The implementation provides a working path for an operator to
**export** a chronicle slice (per-task or per-time-window)
before deletion runs. If retention deletes data an operator
needed but didn't export, that's an operator bug, not a runtime
bug.

## Three approaches (sketched, not chosen)

### Approach A — Time-based event pruning

Configuration:

```toml
[coordinator.retention]
event_max_age_days = 30   # opt-in; 0/missing = retain forever
```

On startup (after the recovery scan) and on operator call to
`task.compact_events` (new capability), the Coordinator deletes
`task_events` rows where `ts < now - max_age_days` AND the
parent task is in a terminal state.

Pros: simple, well-understood. Pros: aligns with audit log
retention practices most operators already have.

Cons: a long-running task's early-attempt chronology gets pruned
while later attempts are intact. The chronicle on disk no longer
reconstructs the full history; operators relying on it need to
have exported in time.

### Approach B — Per-task event count cap

Configuration:

```toml
[coordinator.retention]
events_per_task_cap = 1000   # opt-in
```

When a task's chronicle exceeds the cap, the Coordinator deletes
the oldest events down to `cap`. Same terminal-state guard as
Approach A.

Pros: per-task bounded — predictable storage growth even with
hot tasks. Pros: doesn't penalise slow workloads (a flow that
runs for a year still has its full chronicle).

Cons: still loses early-chronology data on hot tasks. A flapping
retry loop could lose its own `task.created` event before
operators notice.

### Approach C — Compact + snapshot

Per task, when its chronicle exceeds a threshold, the Coordinator
emits a `task.snapshot` event whose `payload_json` summarises
the events being compacted, then deletes the originals. Subsequent
operator queries see one summary event in place of the burst.

```json
// task.snapshot payload_json
{
  "compacted_event_count": 850,
  "compacted_event_id_range": [12, 862],
  "compacted_ts_range":       [1700000000, 1700005000],
  "summary": {
    "attempt_count":   42,
    "failure_classes": {"transient": 38, "timeout": 4},
    "final_status":    "failed"
  }
}
```

Pros: preserves operator-facing semantics (you can still see
"this task had 42 attempts of which most failed transient"
without the per-attempt chronicle). Pros: compatible with B as
an enrichment.

Cons: more code; the summary is the hard part because it requires
event-type-aware logic, which the Coordinator deliberately
doesn't have today (we treat events as opaque from the runtime's
perspective).

## Operator export contract

Before any retention runs, the operator MUST be able to:

1. **Export one task's full chronicle** as a single file (JSON
   array of events + the task header + the attempt rows). This
   is approximately `task.get + task.attempts + task.events` in
   one call — `/v1/tasks/:id/lineage` is most of the way there,
   but a dedicated `task.export` capability would be the
   write-aligned (one round-trip, single canonical artifact)
   form.

2. **Export many tasks by filter**: bulk export tasks updated
   between two timestamps, optionally narrowed by status. The
   output is a series of per-task exports concatenated, with a
   header line per task.

3. **Verify export integrity** before deletion. The Coordinator
   doesn't sign events (it's metadata, not audit), but the
   export should include a row count + content hash so the
   operator can confirm post-deletion that the export is
   complete.

The export capability lands BEFORE any retention capability —
deletion that doesn't have a working "save it first" path is
dangerous to ship.

## Implementation status

- **Step 1 (export-only)** — ✅ shipped. `task.export`
  Coordinator capability + `/v1/tasks/:id/export` bridge
  endpoint (Content-Disposition: attachment so browsers save
  directly). Returns the single-JSON archival artifact
  described in this doc's "Operator export contract"
  section. See
  [`task-runtime.md`](task-runtime.md) for the wire shape.
- **Step 2 (dry-run candidate counter)** — ✅ shipped.
  `task.compact_events` Coordinator capability accepts
  `max_age_secs|mode` (mode required to be `dry-run` today;
  any other value returns INVALID_ARGS with a clear
  "not implemented" cause). Counts events that *would* be
  deleted under the policy — broken down by parent task
  status — without deleting anything. Honours R5 (only
  events whose parent task is in a terminal state are
  counted). Surfaced as
  `GET /v1/tasks/compact_events?max_age_secs=N` on the
  bridge and `relix-cli task compact --max-age-secs N` on
  the CLI. **Configurable max age** in `[coordinator]` TOML
  was deferred — the dry-run pass takes the policy from the
  caller per-invocation, which is what operators need today;
  pinning a default into config is only useful once an
  automatic compaction loop exists (it doesn't).
- **Step 3 (bounded delete)** — pending. Will land as a
  separate capability (`task.delete_compacted_events` or
  similar) with a `--confirm` token and per-pass `LIMIT`.
- **Step 4 (snapshot synthesis)** — pending.
- **Step 5 (operator triage tooling)** — partial.
  `relix-cli task export` ✅ shipped (CLI parity with the
  dashboard's Export button; `--out -` streams to stdout,
  `--out FILE` writes to disk). `relix-cli task compact`
  ✅ shipped for the dry-run side. The delete-side tool
  lands with Step 3.

## Suggested implementation order

This is the **recommended** sequence; greenlight required for
each remaining step.

1. **Export-only first.** ✅ Shipped. `task.export` capability
   + `/v1/tasks/:id/export` bridge endpoint.
2. **Dry-run candidate counter.** ✅ Shipped.
   `task.compact_events` capability with `mode=dry-run`
   (only mode accepted today; the destructive `delete` mode
   returns INVALID_ARGS until Step 3 lands). Counts
   candidates broken down by parent task status. Operators
   validate the policy here before any kill switch exists.
3. **Bounded delete.** Implement the actual deletion with a
   bounded `LIMIT` per pass + transaction-per-pass. Default
   `disabled`; opt-in. Adds a `delete` mode (or a separate
   capability) plus operator confirmation gate.
4. **Snapshot synthesis.** Add Approach C's `task.snapshot`
   emit. Layer on top of step 3.
5. **Operator triage tooling.** `relix-cli task export
   --task-id ID [--out FILE|-]` ✅ shipped (writes the
   archival artifact to stdout for piping into jq / gzip,
   or to a file). `relix-cli task compact --max-age-secs N`
   ✅ shipped for the dry-run side. The destructive-side
   tool lands with Step 3.

Each step is independently shippable. Stop at step 3 if Approach
A meets operator needs.

## What this design does NOT cover

- **`tasks` row deletion** — out of scope for the first pass.
  A terminal task with a compacted chronicle is still useful
  metadata.
- **`task_attempts` deletion** — out of scope. Attempts are
  small and the cardinality is bounded per task.
- **`audit.log` retention** — already an operator concern
  outside Relix. Standard log-rotation tooling applies.
- **Per-flow event log retention** — separate problem;
  documented under SIMP-024 in
  [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md).

## See also

- [`event-contract.md`](event-contract.md) — typed envelope
  shapes a `task.snapshot` payload would borrow from.
- [`task-runtime.md`](task-runtime.md) — schema the retention
  pass mutates.
- [`current-limitations.md`](current-limitations.md) — current
  state of "no retention today."
- [`audit-trails.md`](audit-trails.md) — why audit logs are
  out of scope for chronicle retention.
