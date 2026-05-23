# Audit Trails — Per-Node + Per-Flow Reconstruction

Relix produces three independent on-disk audit surfaces. This doc
explains what each captures, how they correlate, and how to use
`relix-flow-inspect` to walk them when reconstructing what
happened on a particular request.

The three surfaces are independent on purpose: a responder's audit
log is the **responder's own attested record** of what it did; a
caller's flow log is the **caller's own attested record** of what
it asked for; and the Coordinator's chronicle is the **durable
metadata layer** the operator queries by `task_id`. Together they
let you reconstruct a request across the trust boundary between
caller and responder without either party having to trust the
other's logs.

## The three surfaces

### 1. Per-node audit log (`dev-data/<run>-<node>/audit.log`)

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

This is the **responder's** view: "I was asked to do X by Y, here's
what the admission pipeline decided." It's signed by the
controller's own key.

### 2. Per-flow event log (`dev-data/flow-runner/flows/<flow_id>.log`)

The caller-side counterpart. One file per `FlowRunner::run`
invocation. Records include:

- `FlowStarted` (with the flow_template path, trace_id).
- One `RemoteCallIssued` per `remote_call` opcode the VM executed.
- One `RemoteCallCompleted` (with `latency_ms`) or
  `RemoteCallFailed` per outcome.
- `FlowCompleted` or `FlowFailed`.

Hash-chained + signed by the controller that ran the flow (the
bridge for `/chat` requests; whoever ran `relix-cli flow-run` for
manual invocations).

### 3. Coordinator's `task_events` chronicle

The operator-facing summary. See
[`event-vocabulary.md`](event-vocabulary.md) for the full
event-name contract. The chronicle is NOT signed (it's queried by
the operator, not used for inter-peer trust); it's an index into
the other two surfaces.

## How they correlate

A single `/chat` HTTP request produces all three:

```
1. Operator: POST /chat
   ↓
2. Bridge mints trace_id T, task_id K.
   ↓
3. Bridge writes coordinator events:
   task.created → flow.started → task.attempt_started(trace=T)
   ↓
4. Bridge runs FlowRunner → opens dev-data/flow-runner/flows/F.log
   ↓ FlowStarted(trace=T)
   ↓
5. SOL VM emits remote_call("memory", "memory.write_turn", ...)
   ↓ Bridge sends RPC (request_id=R1, trace_id=T)
   ↓ Memory peer: admission pipeline runs
   ↓ Memory peer writes audit record (rid=R1, trace=T, method=memory.write_turn, decision=admitted)
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

## Reading the logs with `relix-flow-inspect`

The inspector binary is in `crates/relix-flow-inspect/`. Build
once:

```bash
cargo build --release -p relix-flow-inspect
```

### Flow log: summary

```bash
relix-flow-inspect --flow dev-data/flow-runner/flows/<flow_id>.log
# -> records: 12
#    seq=0 kind=FlowStarted          payload_len=87
#    seq=1 kind=RemoteCallIssued     payload_len=64
#    seq=2 kind=RemoteCallCompleted  payload_len=120
#    ...
```

### Flow log: human-readable trace

```bash
relix-flow-inspect --flow dev-data/flow-runner/flows/<flow_id>.log --human
# Indented multi-line output with payload key=value lines surfaced
# and latency_ms extracted for each RemoteCall*.
```

### Flow log: integrity verification

```bash
relix-flow-inspect --flow dev-data/flow-runner/flows/<flow_id>.log \
    --replay-verify --signer-key dev-keys/local-bridge.key
# -> INTEGRITY OK
#    records: 12
#    next_seq: 12
```

This walks the hash chain and verifies every record's signature
against the supplied key. Tamper detection.

### Audit log: filter by trace_id

```bash
relix-flow-inspect --audit dev-data/local-ai/audit.log \
    --trace <trace_id_hex> --human
```

Shows every admission decision on the AI node that touched the
given trace_id. To follow a single request across peers, run the
same with `--audit dev-data/local-memory/audit.log` and
`--audit dev-data/local-tool/audit.log`.

### Audit log: filter by request_id

```bash
relix-flow-inspect --audit dev-data/local-memory/audit.log \
    --rid <request_id_hex>
```

Useful when a flow log shows a specific `RemoteCallFailed` and
you want the responder's view of that exact call.

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
relix-flow-inspect --audit dev-data/local-<responder>/audit.log \
    --rid <request_id> --human
```

The audit record tells you whether the responder admitted the
call, what method was attempted, and how long the handler ran
before the error.

### "Did anything else on this trace fail?"

```bash
# Walk every responder's audit log filtered by trace_id.
for node in memory ai tool coordinator; do
    echo "=== $node ==="
    relix-flow-inspect --audit dev-data/local-${node}/audit.log \
        --trace <trace_id>
done
```

A `policy_denied` on a peer the flow tried to call surfaces here
even if the flow log only shows the resulting `RemoteCallFailed`.

### "Walk every attempt of a retried task"

```bash
# Per-attempt rows (each has its own flow_id + flow_log_path).
relix-cli task attempts --peer ... --task-id <X>

# For each attempt, inspect its flow log.
relix-cli task attempts --peer ... --task-id <X> \
    | awk '{print $6}' \
    | while read flow_id; do
        if [ "$flow_id" != "-" ]; then
          relix-flow-inspect --flow dev-data/flow-runner/flows/${flow_id}.log --human
          echo "---"
        fi
      done
```

This is the per-attempt forensic loop — useful when a task
retried successfully and you want to see why attempts 1..N-1
failed.

## What the logs do NOT contain

Honest list of out-of-scope items:

- **No request body / response body.** Audit records carry
  metadata (method, decision, latency). The arg bytes themselves
  are not logged; if you need them, they're in the flow log's
  `RemoteCallIssued` payload (caller-side) but not the audit log
  (responder-side, intentional — would create a privacy
  surface).
- **No cross-trust-root correlation.** Each org's audit logs are
  signed by that org's controllers. Operators across trust roots
  cannot verify each other's logs without exchanging the relevant
  public keys.
- **No automatic correlation between flow logs and audit logs
  across runs.** The `trace_id` is the join key; you have to
  perform the join (via `--trace` filters or scripts).
- **No retention policy.** Logs append forever; operators rotate
  with their own tooling. SIMP-024 documents this.

## See also

- [`coordination.md`](coordination.md) — the Task ledger that points
  at the flow logs.
- [`event-vocabulary.md`](event-vocabulary.md) — the chronicle
  events the Coordinator records.
- [`security.md`](security.md) — what the audit pipeline enforces
  on every call.
- [`runtime-lifecycle.md`](runtime-lifecycle.md) — the status
  transitions a task walks while emitting events.
- [`crates/relix-flow-inspect/src/main.rs`](../crates/relix-flow-inspect/src/main.rs)
  — the inspector's full flag set.
