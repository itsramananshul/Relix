# Coordinator

The Coordinator is a Relix peer that owns the **durable Task ledger**. It
runs as a regular controller (`node_type = "coordinator"`), is dialled
like any other peer, and serves five capabilities through the standard
admission pipeline.

It is **not** a central gateway, an orchestrator, or a flow executor.
Flow execution still happens on the caller's side (bridge or
`relix-cli flow-run`). The Coordinator's job is to remember that the
work happened, what state it's in, and where the per-flow event log
lives on disk.

For the **semantics** of "checkpointed re-run" (and why this is not
yet full resumable replay), see [`replay-model.md`](replay-model.md).
For the **schema** and capability wire formats, see
[`task-runtime.md`](task-runtime.md).

## Why a separate peer

Three reasons the bridge or any single executor doesn't own task state
itself:

1. **Outlives any one executor.** The bridge restarts every time you
   `cargo build` and re-run the script. Task records that lived only
   in bridge memory would die with it. SQLite on a different process
   survives.
2. **Single source of truth.** Multiple executors (bridge, CLI, future
   channel nodes like Telegram) can all create / update / read the
   same Task ledger. There's one place to look.
3. **Same admission pipeline.** Every `task.*` call goes through
   identity → policy → handler → audit on the Coordinator, just like
   every other capability. The Coordinator's audit log is the
   responder-side record of who touched which task and when.

## Configuration

The bringup script generates the Coordinator config at
`dev-data/<run>/coordinator.toml`:

```toml
[controller]
name = "<run>-coordinator"
node_type = "coordinator"
listen_port = 19714

[identity]
key_path = "dev-keys/<run>-coordinator.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[coordinator]
db_path = "dev-data/<run>/tasks.db"
max_list = 200

[peers]
```

Defaults (`max_list = 200`, schema in
`crates/relix-runtime/src/nodes/coordinator/mod.rs::init_schema`) are
fine for the local mesh. The Coordinator also serves `node.health` and
`node.manifest` like every other controller.

Pass `-NoCoordinator` to the bringup script to skip the Coordinator
entirely — the bridge stays operational; the only loss is that
nothing remembers a chat across bridge restarts.

## Capabilities

All wire formats are pipe-delimited UTF-8 strings (alpha SIMP-016).
Empty fields are valid — skip a field by leaving its slot empty.

| Method | Arg | Returns |
|---|---|---|
| `task.create` | `title\|flow_template\|params_json\|owner_subject_id` (owner defaults to caller's `subject_id`) | `<task_id>` (32 hex chars) |
| `task.update` | `task_id\|status\|result\|flow_id\|flow_log_path\|error_kind\|error_cause` | `ok\n` |
| `task.event`  | `task_id\|event_type\|payload` | `<event_id>` (integer) |
| `task.get`    | `task_id` | multi-line `key=value` summary + `events:` JSON |
| `task.list`   | `` or `<limit>` (default 50, max from config) | one `task_id\tstatus\ttitle\n` per line |

`task.update` preserves fields you omit — the empty slot means "don't
change this column". `status` is opaque to the Coordinator; the bridge
uses the canonical `pending` / `running` / `completed` / `failed` /
`abandoned` strings but the Coordinator itself does not enforce a state
machine. That keeps the surface minimal; tighter state-machine
enforcement is a Gate 2 item.

## CLI

```bash
# Set this once per shell so commands stay short.
$peer = "/ip4/127.0.0.1/tcp/19714"
$id   = "dev-keys/local-bridge.aic"
$key  = "dev-keys/local-bridge.key"

relix-cli task list   --peer $peer --identity $id --client-key $key
relix-cli task create --peer $peer --identity $id --client-key $key `
    --title 'demo'                  `
    --flow-template chat_template.sol `
    --params-json '{"session":"demo"}'
# -> task_id: 2b52a499bbce34a5a64746273e9af79b

relix-cli task get    --peer $peer --identity $id --client-key $key --task-id 2b52a499...
relix-cli task event  --peer $peer --identity $id --client-key $key --task-id 2b52a499... `
    --event-type checkpoint --payload 'memory.write_turn ok'
relix-cli task update --peer $peer --identity $id --client-key $key --task-id 2b52a499... `
    --status completed --result 'mock: ok' --flow-id <hex> --flow-log-path 'dev-data/...'
```

Every command goes through the real admission pipeline. If the policy
file doesn't admit your identity's groups for `task.*`, you'll see
`policy_denied` — by design.

## Persistence behaviour

The Coordinator keeps everything in `dev-data/<run>/tasks.db`.

Live-verified (mesh boot → create task → kill coordinator process →
restart coordinator → list / get task): the task and its event chronicle
survive intact, identical timestamps and payload. The CLI calls during
the outage time out cleanly; the next call after restart returns the
preserved record.

## What restarts do NOT recover

The Coordinator is a **task ledger**, not a resume engine:

- A flow that was mid-execution when the bridge died is **not** resumed
  automatically. The bridge does not know how to pick up a half-finished
  SOL flow because the alpha SOL VM does not yield (SIMP-001).
- A task left in status `running` after a Coordinator restart stays in
  `running` until an operator updates it. There is no automatic
  "abandoned" sweep. Sweep + retry is an operator gesture, not a
  background daemon. See [`replay-model.md`](replay-model.md) for the
  honest framing of what this gives you and what it doesn't.

## Trust + audit

The Coordinator inherits Relix's per-peer admission posture verbatim:

- Every `task.*` call is signed by the caller's `IdentityBundle` and
  verified against the org root.
- Every call writes a record to the Coordinator's hash-chained audit
  log at `dev-data/<run>-coordinator/audit.log`. Operator inspection:
  ```bash
  cargo run -p relix-flow-inspect -- --audit dev-data/local-coordinator/audit.log
  ```
- The Coordinator does **not** verify that the `owner_subject_id`
  passed to `task.create` matches the caller's `subject_id`. Operators
  who care can wire a policy rule (or a dispatch-layer check in a
  future milestone) that pins `task.create` to the bridge's group only.

## Open / NOT in scope

See [`current-limitations.md`](current-limitations.md). Specifically
the Coordinator does **not** yet:

- Track multiple attempts per task as first-class records (the
  `latest_flow_id` / `latest_flow_log_path` are *latest*-only; previous
  attempts live only in the flow logs they point to).
- Enforce a state-machine on `status`.
- Auto-detect crashed executors.
- Reconcile across multiple Coordinator instances (single-instance only
  in the alpha; leadership election is Gate 2).

## See also

- [`task-runtime.md`](task-runtime.md) — schema, wire details, how the
  bridge would integrate.
- [`replay-model.md`](replay-model.md) — exactly what "checkpointed
  re-run" promises and does not promise.
- [`architecture.md`](architecture.md) — where the Coordinator sits in
  the request flow.
