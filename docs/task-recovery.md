# Task Recovery — Operator Playbook

_Version: 0.4.1_

Concrete steps for diagnosing and acting on interrupted or failed
Tasks. Companion to [`interruption-semantics.md`](interruption-semantics.md)
(the contract) and [`retry-model.md`](retry-model.md) (what retries
mean today).

## Prerequisites

- A Coordinator peer is running and reachable. Its libp2p multiaddr
  is what you pass as `--peer`.
- You have an identity bundle (`.aic`) and a 32-byte client key whose
  subject is permitted by the policy that gates `task.*`.
- `relix-cli` is on `PATH` (or use `cargo run --bin relix-cli --`).

## Step 1: see what state things are in

List recent tasks, optionally filtered by status:

```bash
relix-cli task list --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --limit 100

# Just the ones the recovery scan acted on:
relix-cli task list --peer ... --identity ... --client-key ... \
    --status interrupted

# Just outright failures (bridge wrote 'failed'):
relix-cli task list --peer ... --identity ... --client-key ... \
    --status failed
```

The CLI prints one line per task: `<task_id_prefix>  <status>  <title>`.

## Step 2: inspect one task in detail

```bash
relix-cli task get --peer ... --identity ... --client-key ... \
    --task-id <full-32-hex> --pretty
```

`--pretty` reformats the response as a header block plus a timeline:

```
task_id=2b52a499bbce34a5a64746273e9af79b
title=chat: walk me through ...
status=interrupted
flow_template=chat_template.sol
retry_count=0
retry_policy=bounded
max_retries=2
max_runtime_secs=120
started_at=1779235935
last_failure_class=timeout
last_failure_reason=deadline_exceeded: started_at=1779235935 max_runtime_secs=120 now=1779236060
event_count=3

chronology:
        1779235935  task.created        chat_template.sol
  +   0s 1779235935  flow.started        chat_template.sol
  + 125s 1779236060  task.interrupted    started_at=1779235935 max_runtime_secs=120 ...
```

Drop `--pretty` to get the raw `key=value` lines (grep-friendly for
scripts).

## Step 3: decide what to do

Use `last_failure_class` to decide:

| Class | Typical action |
|---|---|
| `transient` | Re-run if idempotent. Same flow_template + params. |
| `timeout` | Re-run with a higher `max_runtime_secs`, or investigate why the flow stalled. |
| `unavailable` | Wait, then re-run. Check whether the responder peer is alive (`relix-cli ping`). |
| `policy_denied` | Do NOT re-run blindly. Fix the policy or identity first. |
| `invalid_args` | Do NOT re-run. Fix the caller. |
| `permanent` | Do NOT re-run. Investigate. |

The chronicle (`task.event` entries) often tells you what step
failed; the per-flow event log on disk (`latest_flow_log_path`) has
every `RemoteCallIssued` / `RemoteCallFailed` for forensic detail.

## Step 4: act

### Re-run from scratch (most common)

```bash
# Retry primitive: validate state + budget, emit task.retry_requested,
# flip to retrying. Refused by default for non-retryable failure
# classes (policy_denied / invalid_args / permanent); use --force to
# override.
relix-cli task retry --peer ... --identity ... --client-key ... \
    --task-id <hex>

# Re-run with the same flow_template + params. The flow's
# success/failure path drives task.update, which opens a NEW attempt
# row (#N+1) and closes it with the terminal outcome.
relix-cli flow-run --flow flows/chat_template.sol \
    --identity ... --client-key ... --peers peers.toml
```

After the new attempt finishes, inspect the full attempt history:

```bash
relix-cli task attempts --peer ... --identity ... --client-key ... \
    --task-id <hex>
#   #  status       started     duration     failure       flow_id
#   1  failed       1700000000  5s           transient     flowA...
#   2  completed    1700000020  3s           -             flowB...
```

### Give up on a task

```bash
relix-cli task update --peer ... --identity ... --client-key ... \
    --task-id <hex> --status cancelled \
    --error-cause 'operator gave up — see incident #N'
```

The `error_cause` text gets mirrored to `last_failure_reason` so the
chronicle reflects why.

### Force the recovery scan to run now

If you just set `max_runtime_secs` on a long-running task and don't
want to restart the Coordinator:

```bash
relix-cli task recover --peer ... --identity ... --client-key ...
# Prints one line per recovered task plus `recovered=N` at the end.
```

### Adjust the deadline retroactively

You cannot today — `max_runtime_secs` is an INSERT-time decision and
the CLI does not expose an UPDATE flag for it. Workarounds:

- Issue the recovery scan, mark the row `cancelled`, re-create the
  Task with the higher deadline, and re-run.
- Skip the recovery scan entirely (`[coordinator] recovery_scan =
  false`) and rely on operator-initiated cancellation.

A `task.update --max-runtime-secs` flag is a candidate follow-up.

## Step 5: pattern-match across many tasks

Both `task list` and `task get` are line-oriented and grep-friendly.
For "show me every interrupted task whose deadline was less than 60
seconds":

```bash
relix-cli task list --peer ... --identity ... --client-key ... \
    --status interrupted --limit 200 \
| awk '{print $1}' \
| while read prefix; do
    relix-cli task get --peer ... --identity ... --client-key ... \
        --task-id "$prefix..." \
    | grep -E 'task_id|max_runtime_secs'
  done
```

The default `task get` output (no `--pretty`) is designed for exactly
this kind of pipeline.

## Common pitfalls

- **`task get` on a prefix fails.** The Coordinator's `task.get`
  takes the full 32-hex id, not a prefix. The list output truncates
  for display but you need the full id from a previous `task get` or
  from the response of `task create`.
- **`task recover` on an empty mesh returns silently.** The default
  exit is 0 with `recovered=0\n`. Check stderr for `policy_denied` if
  you expected something to happen — your identity may not be in the
  group the policy admits.
- **The bridge writes `task_id` to its responses too.** Look in
  `ChatResponse.task_id` (native endpoints) or `relix.task_id`
  (OpenAI shim) if you want to recover a task without scrolling
  through `task list`.

## See also

- [`runtime-lifecycle.md`](runtime-lifecycle.md) — the status
  transition diagram.
- [`attempt-lineage.md`](attempt-lineage.md) — per-attempt rows,
  how they map back to flow logs, and the attempt-aware event
  vocabulary.
- [`interruption-semantics.md`](interruption-semantics.md) — what
  `interrupted` does and does not mean.
- [`retry-model.md`](retry-model.md) — what `task.retry` actually
  does and why nothing auto-retries today.
- [`task-runtime.md`](task-runtime.md) — schema and wire format.
- [`coordination.md`](coordination.md) — the peer and its trust model.
