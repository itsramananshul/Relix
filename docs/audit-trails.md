# Audit Trails — Per-Node + Per-Flow Reconstruction

> Version 0.4.1

Relix produces six independent on-disk audit surfaces. This doc
explains what each captures, how they correlate, and how to use
`relix-flow-inspect` to walk them when reconstructing what
happened on a particular request.

The surfaces are independent on purpose: a responder's audit
log is the **responder's own attested record** of what it did; a
caller's flow log is the **caller's own attested record** of what
it asked for; the Coordinator's chronicle is the **durable
metadata layer** the operator queries by `task_id`; the tenant
audit partition provides a **queryable SQLite mirror** sliced by
tenant; the evidence store holds **structured per-action records**
with PII redaction and state diffs; and the PII chronicle logs
every PII detection event. Together they let you reconstruct a
request across the trust boundary between caller and responder
without either party having to trust the other's logs.

## The six surfaces

### 1. Per-node audit log (`~/.relix/<node-name>/audit.log`)

Every controller writes one append-only signed log of admission
decisions. Each record covers a single inbound RPC and includes:

- `request_id` (hex) — RPC envelope's unique id.
- `trace_id` (hex) — caller-supplied or runtime-minted trace.
- `caller_subject_id` — the verified IdentityBundle subject.
- `method` — the capability called (e.g. `ai.chat`).
- `decision` — `admitted` / `policy_denied` / `identity_invalid`.
- `latency_ms` — wall-clock spent inside the handler.
- a hash chain that lets you verify the log hasn't been tampered
  with after the fact.

**Wire framing:** each record is preceded by a 4-byte big-endian
length followed by the CBOR-encoded `AuditRecord` bytes.

**`ResponseEnvelope.aid`** in every response is a server-minted
UUIDv4, independent of `request_id`. It identifies the audit row
on the responder's side. The caller's `request_id` is still
recorded in the `AuditDraft` for cross-correlation, but it is not
the row's primary key.

This is the **responder's** view: "I was asked to do X by Y, here's
what the admission pipeline decided." It's signed by the
controller's own key.

Key-rotation recovery: if the audit chain fails verification
because a record was written by a different key, the old file is
quarantined as `{name}.quarantined-{ms}` and a fresh chain starts.
If the faulting record claims the current key, `open` hard-fails —
this is a tamper signal, not a key-rotation case.

Production data dir: `~/.relix/<controller.name>/`. When
`$RELIX_DATA_DIR` is set it overrides `~/.relix`.

### 2. Per-flow event log (`~/.relix/flow-runner/flows/<flow_id>.log`)

The caller-side counterpart. One file per `FlowRunner::run`
invocation. Records include:

- `FlowStarted` (with the flow_template path, trace_id).
- One `RemoteCallIssued` per `remote_call` opcode the VM executed.
- One `RemoteCallCompleted` (with `latency_ms`) or
  `RemoteCallFailed` per outcome.
- `FlowCompleted` or `FlowFailed`.

Same 4-byte big-endian length + CBOR framing as the audit log.
Hash-chained + signed by the controller that ran the flow (the
bridge for `/chat` requests; whoever ran `relix-cli flow-run` for
manual invocations).

Base path: `$RELIX_DATA_DIR/flow-runner/flows/<flow_id>.log`,
falling back to `~/.relix/flow-runner/flows/<flow_id>.log`.

### 3. Coordinator's `task_events` chronicle

The operator-facing summary. See
[`event-vocabulary.md`](event-vocabulary.md) for the full
event-name contract. The chronicle is NOT signed (it's queried by
the operator, not used for inter-peer trust); it's an index into
the other surfaces.

### 4. Tenant audit partition mirror (`~/.relix/<node>/audit-partition.db`)

Enabled via `[audit] partition_by_tenant = true`. When enabled,
the dispatch bridge mirrors every signed audit record to a
queryable SQLite store **before** finalising the canonical CBOR
chain. Mirror write failures are logged as WARN but never block
the canonical write — the CBOR chain is the source of truth.

Two capabilities expose the partition:

| Capability | Args | Response |
|---|---|---|
| `node.audit.tenant_list` | (none) | One tenant id per line + `count=N`; returns `count=0` when partition is disabled |
| `node.audit.tenant_recent` | `<tenant_id>\|<limit>` (pipe-delimited; limit default 100, clamp `[1, 1000]`) | JSON `{"tenant_id":"...","count":N,"rows":[...]}` newest-first |

**Tenant id sanitisation:** ASCII alphanumeric + `_` only; every
other character becomes `_`; `None`/empty becomes `"default"`.
When `partition_by_tenant = true`, missing or empty tenant id is
rejected with an error — the partition does not silently file under
`"default"` in that mode.

Configure in `[audit]`:

```toml
[audit]
partition_by_tenant = true
db_path             = "~/.relix/my-node/audit-partition.db"   # default
```

Schema migration id: `"audit_partition.v1"` (tracked in
`_relix_migrations`).

### 5. Evidence store (`~/.relix/<node>/evidence.db`)

The `EvidenceStore` (`evidence_records` table) records every tool
dispatch that flows through the execution gateway. One row per
action, with:

- `arguments_redacted` — args passed through `PiiAnonymizer` before
  storage (PII removed).
- `policy_decision` — `"allowed"` / `"blocked"` / `"dry_run"`.
- `reversibility` — `"auto_compensated"` / `"human_rollback"` /
  `"blocked"`.
- `state_before` / `state_after` / `diff` — before/after snapshots
  and unified diff when a `StateProbe` is wired (NULL in alpha
  production).
- `tenant_id` — verified tenant; default `"default"`.

Accessible via the `execution.evidence` capability (args:
`{action_id?, actor_id?, limit?}`; default `limit=20`, clamp
`[1, 200]`).

Configure in `[execution.gateway]`:

```toml
[execution.gateway]
evidence_db_path = "~/.relix/my-node/evidence.db"   # default: same as db_path
```

### 6. PII audit chronicle (`~/.relix/<node>/pii_events.sqlite`)

When `[mesh_pii] enabled = true`, every PII detection event is
written to the `pii_events` table regardless of the action taken
(`block`, `redact`, or `log_only`). Each row captures:
`request_id`, `agent`, `method`, `direction` (`"inbound"` /
`"outbound"`), `action_taken`, `span_count`, `types`
(comma-joined distinct PII type names).

Accessible via:
- `pii.scan_stats` (args: `{hours?}`; default 24 h, clamp `[1, 2160]`)
- `pii.recent_events` (args: `{limit?, method?}`; default 50, clamp `[1, 1000]`)

Configure in `[mesh_pii]`:

```toml
[mesh_pii]
enabled         = true
action          = "redact"          # "block" | "redact" | "log_only"
chronicle_path  = "~/.relix/my-node/pii_events.sqlite"
```

## How they correlate

A single `/chat` HTTP request produces records across multiple
surfaces:

```
1. Operator: POST /chat
   ↓
2. Bridge mints trace_id T, task_id K.
   ↓
3. Bridge writes coordinator events:
   task.created → flow.started → task.attempt_started(trace=T)
   ↓
4. Bridge runs FlowRunner → opens ~/.relix/flow-runner/flows/F.log
   ↓ FlowStarted(trace=T)
   ↓
5. SOL VM emits remote_call("memory", "memory.write_turn", ...)
   ↓ Bridge sends RPC (request_id=R1, trace_id=T)
   ↓ Memory peer: admission pipeline runs
   ↓ Memory peer writes audit record (rid=R1, trace=T, method=memory.write_turn)
   ↓   aid = server-minted UUIDv4 (NOT equal to R1)
   ↓ If partition_by_tenant=true: mirror row written to audit-partition.db
   ↓ Bridge writes flow log: RemoteCallCompleted(rid=R1)
   ↓
6. ...repeat for memory.read, ai.chat, etc.
   ↓
7. SOL VM completes.
   ↓ Bridge writes flow log: FlowCompleted
   ↓ Bridge writes coordinator events: task.attempt_finished → task.completed
   ↓ Bridge calls task.update(status=completed, flow_id=F, ...)
```

After the fact you can:

- Start from the HTTP response's `task_id` (or `trace_id`).
- Read the Coordinator chronicle to see the high-level shape +
  `flow_log_path` pointer.
- Open the flow log to see the per-remote_call detail.
- Open each responder's audit log filtered by `trace_id` or
  `request_id` to see what the responder thought about each call.
- Query the tenant partition if you need SQL over admission records.
- Query the evidence store if you need per-action records with
  reversibility classification.

## Reading the logs with `relix-flow-inspect`

The inspector binary is in `crates/relix-flow-inspect/`. Build
once:

```bash
cargo build --release -p relix-flow-inspect
```

### Flow log: summary

```bash
relix-flow-inspect --flow ~/.relix/flow-runner/flows/<flow_id>.log
# -> records: 12
#    seq=0 kind=FlowStarted          payload_len=87
#    seq=1 kind=RemoteCallIssued     payload_len=64
#    seq=2 kind=RemoteCallCompleted  payload_len=120
#    ...
```

### Flow log: human-readable trace

```bash
relix-flow-inspect --flow ~/.relix/flow-runner/flows/<flow_id>.log --human
```

### Flow log: integrity verification

```bash
relix-flow-inspect --flow ~/.relix/flow-runner/flows/<flow_id>.log \
    --replay-verify --signer-key dev-keys/local-bridge.key
# -> INTEGRITY OK
#    records: 12
#    next_seq: 12
```

### Audit log: filter by trace_id

```bash
relix-flow-inspect --audit ~/.relix/local-ai/audit.log \
    --trace <trace_id_hex> --human
```

### Audit log: filter by request_id

```bash
relix-flow-inspect --audit ~/.relix/local-memory/audit.log \
    --rid <request_id_hex>
```

## Operator reconstruction recipes

### "What happened on task X?"

```bash
# 1. Get the high-level summary + chronology.
relix-cli task get --peer ... --task-id <X> --pretty

# 2. Pull the latest flow log path from the output.
flow_log=$(relix-cli task get --peer ... --task-id <X> | grep '^latest_flow_log_path=' | cut -d= -f2)

# 3. Get the human-readable execution trace.
relix-flow-inspect --flow $flow_log --human
```

### "Why did this remote_call fail?"

```bash
# 1. Find the request_id of the failed call in the flow log.
relix-flow-inspect --flow $flow_log --human | grep -i 'remotecallfailed' -A 4

# 2. Read the responder's audit record for that exact call.
relix-flow-inspect --audit ~/.relix/local-<responder>/audit.log \
    --rid <request_id> --human
```

### "Did anything else on this trace fail?"

```bash
for node in memory ai tool coordinator; do
    echo "=== $node ==="
    relix-flow-inspect --audit ~/.relix/local-${node}/audit.log \
        --trace <trace_id>
done
```

### "Walk every attempt of a retried task"

```bash
relix-cli task attempts --peer ... --task-id <X>

relix-cli task attempts --peer ... --task-id <X> \
    | awk '{print $6}' \
    | while read flow_id; do
        if [ "$flow_id" != "-" ]; then
          relix-flow-inspect --flow ~/.relix/flow-runner/flows/${flow_id}.log --human
          echo "---"
        fi
      done
```

### "Show me all audit records for tenant acme"

Requires `[audit] partition_by_tenant = true` on the node.

```bash
# List all tenants seen by this node
relix-cli cap --peer my-node -- node.audit.tenant_list

# Get the 50 most recent records for tenant "acme"
relix-cli cap --peer my-node -- node.audit.tenant_recent "acme|50"
```

The response is JSON: `{"tenant_id":"acme","count":50,"rows":[...]}`.
Fields per row: `ts_secs`, `request_id`, `tenant_id`, `caller_name`,
`method`, `policy_decision`, `status`, `error_kind`, `latency_ms`.

### "What did an agent dispatch (with PII redacted)?"

Requires evidence store enabled (`[execution.gateway] evidence_db_path` set).

```bash
# Query the evidence capability on the node
relix-cli cap --peer my-node -- execution.evidence '{"actor_id":"agent-x","limit":20}'
```

Returns `{"records":[...],"count":N}`. The `arguments_redacted`
field has PII stripped; `state_before`/`state_after` are NULL in
alpha production (probe not wired).

## What the logs do NOT contain

- **No request body / response body in the audit log.** Audit
  records carry metadata (method, decision, latency). Args are
  not logged in the audit log; the caller's flow log carries
  `arg_bytes` sizes (not the actual bytes).
- **No cross-trust-root correlation.** Each org's audit logs are
  signed by that org's controllers. Operators across trust roots
  cannot verify each other's logs without exchanging the relevant
  public keys.
- **No automatic correlation between flow logs and audit logs
  across runs.** The `trace_id` is the join key; you perform the
  join via `--trace` filters or scripts.
- **No retention policy.** CBOR logs append forever; the SQLite
  stores (partition mirror, evidence, PII chronicle) also grow
  indefinitely unless swept. Operators rotate with their own
  tooling. SIMP-024 documents this.
- **`tenant_id` is not signed into `AuditRecord`.** It routes to
  the partition mirror (surface 4) for per-tenant slicing; missing
  or empty tenant becomes `"default"`. The canonical CBOR chain
  is tenant-agnostic; the partition is an additive queryable view.

## See also

- [`coordination.md`](coordination.md) — the Task ledger that points
  at the flow logs.
- [`event-vocabulary.md`](event-vocabulary.md) — the chronicle
  events the Coordinator records and the Sink A / alert event types.
- [`runtime-observability.md`](runtime-observability.md) — metrics,
  alerts, health dashboard, and two-sink session tracing.
- [`security.md`](security.md) — what the audit pipeline enforces
  on every call.
- [`runtime-lifecycle.md`](runtime-lifecycle.md) — the status
  transitions a task walks while emitting events.
- [`crates/relix-flow-inspect/src/main.rs`](../crates/relix-flow-inspect/src/main.rs)
  — the inspector's full flag set.
