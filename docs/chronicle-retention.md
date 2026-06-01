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
- Operator scripts and channels add their own `ops.*` events.

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

Pros: simple, well-understood. Aligns with audit log
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
hot tasks. Doesn't penalise slow workloads.

Cons: still loses early-chronology data on hot tasks. A flapping
retry loop could lose its own `task.created` event before
operators notice.

### Approach C — Compact + snapshot

Per task, when its chronicle exceeds a threshold, the Coordinator
emits a `task.snapshot` event whose `payload_json` summarises
the events being compacted, then deletes the originals.

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

Pros: preserves operator-facing semantics. Compatible with B as
an enrichment.

Cons: more code; the summary requires event-type-aware logic,
which the Coordinator deliberately doesn't have today.

## Operator export contract

Before any retention runs, the operator MUST be able to:

1. **Export one task's full chronicle** as a single file (JSON
   array of events + the task header + the attempt rows). This
   is approximately `task.get + task.attempts + task.events` in
   one call — `/v1/tasks/:id/lineage` is most of the way there,
   but a dedicated `task.export` capability is the
   write-aligned (one round-trip, single canonical artifact)
   form.

2. **Export many tasks by filter**: bulk export tasks updated
   between two timestamps, optionally narrowed by status.

3. **Verify export integrity** before deletion. The export
   should include a row count + content hash so the
   operator can confirm post-deletion that the export is
   complete.

The export capability lands BEFORE any retention capability —
deletion that doesn't have a working "save it first" path is
dangerous to ship.

## Implementation status

- **Step 1 (export-only)** — shipped. `task.export`
  Coordinator capability + `/v1/tasks/:id/export` bridge
  endpoint (Content-Disposition: attachment so browsers save
  directly). Returns the single-JSON archival artifact
  described in this doc's "Operator export contract"
  section. See
  [`task-runtime.md`](task-runtime.md) for the wire shape.
- **Step 2 (dry-run candidate counter)** — shipped.
  `task.compact_events` Coordinator capability accepts
  `max_age_secs|mode` (mode required to be `dry-run` today;
  any other value returns INVALID_ARGS with a clear
  "not implemented" cause). Counts events that *would* be
  deleted under the policy — broken down by parent task
  status — without deleting anything. Honours R5. Surfaced as
  `GET /v1/tasks/compact_events?max_age_secs=N` on the
  bridge and `relix-cli task compact --max-age-secs N` on
  the CLI.
- **Step 3 (bounded delete)** — pending.
- **Step 4 (snapshot synthesis)** — pending.
- **Step 5 (operator triage tooling)** — partial.
  `relix-cli task export` shipped. `relix-cli task compact`
  shipped for the dry-run side.

## Memory node retention (ConsolidationArchiver)

The memory node runs its **own** independent retention
mechanism for Layer 3 (Observation) records in the four-layer
store. This is distinct from the Coordinator chronicle retention
above and does not share configuration or code paths.

### What it does

`ConsolidationArchiver` runs as a background tokio task,
cycling every **6 hours** (configurable at compile time via
`DEFAULT_ARCHIVE_INTERVAL_SECS = 21600`). On each cycle it:

1. Selects all Layer 3 Observation records where
   `valid_to IS NULL`, `frozen = 0`, no `"archived"` tag, and
   `observed_at < now - 30 days` (`STALE_OBS_AGE_SECS = 2_592_000`).
2. For each candidate, checks that a Layer 4 Model record
   exists for the same `source` with
   `model.observed_at > obs.observed_at` — the observation must
   be superseded by a model before it can be archived.
3. On archive: adds the `"archived"` tag and stamps
   `valid_to = now`. **Does NOT delete rows.**
4. **Layer 1 cascade**: once ALL observation records for a
   given `source` are archived, stamps `consolidated = true` on
   the Layer 1 Raw records with matching `source`.

Frozen records (`frozen = 1`) are excluded from the archive
scan entirely.

### What it does NOT do

- Delete rows. Archive is soft — `valid_to` stamp + tag only.
- Touch Layer 2 Semantic records.
- Replace or modify the Layer 4 Model record.
- Interact with the Coordinator chronicle in any way.

### MemoryIntegrityAuditor

A companion task runs every **24 hours**
(`DEFAULT_AUDIT_INTERVAL_SECS = 86400`). It is purely
read-only — no mutations. On each cycle it:

1. **Contradiction sweep** — pairwise anomaly check over all
   currently valid (`valid_to IS NULL`) Layer 3 records grouped
   by `source`. Any pair where `score_observation` returns a
   contradiction signal is emitted as `tracing::warn!`.
2. **Missing source attribution** — Layer 3 and Layer 4 records
   with an empty `source` field are flagged.
3. **Stale unmodeled** — Layer 3 observations older than 30 days
   with no corresponding Layer 4 Model record are flagged.

All findings are `tracing::warn!` events. The operator can
surface these by watching logs or routing them to an
observability sink. The auditor produces an `IntegrityReport`
struct with `contradictions`, `missing_source`,
`stale_unmodeled`, `unsourced_models`, `sources_audited`,
`started_at`, and `finished_at` fields — but this report is
currently used only for testing; it is not yet surfaced via a
capability or HTTP endpoint.

### Relationship to chronicle retention

| Concern | Owner | Mechanism |
|---|---|---|
| Coordinator `task_events` growth | Coordinator (R1–R6 above) | Planned time-based / count-cap pruning with dry-run gate |
| Layer 3 observation lifecycle | Memory node `ConsolidationArchiver` | Tag + `valid_to` stamp, 30d/6h cycle |
| Layer 3 integrity | Memory node `MemoryIntegrityAuditor` | Read-only audit, 24h cycle, log-only output |

They are independent systems. Operators running the memory node
do not need the Coordinator chronicle retention to be enabled,
and vice versa.

## Suggested implementation order (Coordinator)

1. **Export-only first.** Shipped. `task.export` capability
   + `/v1/tasks/:id/export` bridge endpoint.
2. **Dry-run candidate counter.** Shipped.
   `task.compact_events` with `mode=dry-run`.
3. **Bounded delete.** Implement actual deletion with a
   bounded `LIMIT` per pass + transaction-per-pass. Default
   `disabled`; opt-in. Adds a `delete` mode plus operator
   confirmation gate.
4. **Snapshot synthesis.** Add Approach C's `task.snapshot`
   emit. Layer on top of step 3.
5. **Operator triage tooling.** `relix-cli task compact`
   destructive-side tool lands with Step 3.

Each step is independently shippable. Stop at step 3 if Approach
A meets operator needs.

## What this design does NOT cover

- **`tasks` row deletion** — out of scope for the first pass.
- **`task_attempts` deletion** — out of scope.
- **`audit.log` retention** — standard log-rotation tooling.
- **Per-flow event log retention** — separate problem; documented
  under SIMP-024 in
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
- [`four-layer-memory.md`](four-layer-memory.md) — full four-layer
  record store design including bi-temporal validity and the
  promoter pipeline that feeds the archiver.
