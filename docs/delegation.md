# Delegation

_Version: 0.4.1_

Delegation lets one agent spawn another as a subtask, wait for the
result, and continue. It runs on the **coordinator** node — there's
no new node type — and builds on the existing `task_edges` table
that already had the `delegated_to` edge type and `task.delegated_to`
chronicle event.

## What it does

Agent A wants agent B to handle a subtask:

1. **Spawn** — agent A calls `delegate.spawn` with a goal, optional
   context, and (optionally) a target subject_id for the child.
   The coordinator:
   - mints a child task with `origin_surface = "delegation"` and
     `flow_template = "delegation"`,
   - writes a `delegated_to` edge from A's task to the child (and
     a `task.delegated_to` chronicle event on A's task) via the
     existing `record_delegated`,
   - flips A's task to `awaiting_input`,
   - appends a `task.awaiting` chronicle event on A's task with
     payload `child_task_id=<id>`,
   - returns the new `child_task_id` so the caller can poll.
2. **Execute** — the delegation executor (a background tokio task
   on the coordinator) polls every 5 s for child tasks whose
   `origin_surface = 'delegation'` and `status = 'pending'`. Per
   child, it:
   - acquires a semaphore permit (default `max_concurrent = 5`),
   - flips the child to `running`,
   - dispatches `ai.chat(session_id = child.owner_subject_id,
     prompt = goal, history = "[delegation_context] <context>\n")`
     with a hard timeout (`max_job_secs`, default 300 s),
   - writes the reply to `latest_result` (capped at 800 chars),
   - appends `delegate.completed` (or `delegate.failed` on
     timeout / empty / unset dispatcher) to the child's chronicle,
   - flips the child to `completed` / `failed`,
   - appends `delegate.child_completed` to the **parent** task's
     chronicle with payload
     `child_task_id=<id>|status=<status>|preview=<200 chars>`,
   - flips the parent back to `running` (but only when it's
     currently `awaiting_input` — paused / frozen parents are
     left alone).
3. **Resume** — the calling agent loop polls `delegate.result`
   until it sees a terminal `status` and reads the preview from
   the response.

## Why this isn't a blocking call

The spec is honest: `delegate.spawn` is **not** a synchronous
blocking call. The handler returns the `child_task_id`
immediately and the caller decides how to wait. There are two
reasons:

- **Durability.** The parent task is durably paused in the
  coordinator (`status = awaiting_input` + a chronicle event).
  If the calling agent's process dies mid-wait, the child still
  runs to completion and the parent's state is recoverable.
- **No nested blocking RPC.** The mesh's request/response
  envelopes are bounded by per-call deadlines. A 30-minute
  child task would require either an enormous deadline (bad)
  or a streaming pseudo-call (out of scope). Polling is the
  pragmatic answer.

Agent loops written for Relix should expose a small "wait for
delegation" helper that polls `delegate.result` on a cadence
(say 1–5 s) and surfaces the result. SOL flows can do the same
via `remote_call(coordinator, "delegate.result", <id>)`.

## The depth cap

Default `max_depth = 3`. A delegation chain of length 3 (A → B →
C → D) is the deepest allowed; `delegate.spawn` rejects attempts
to go further with `INVALID_ARGS`.

Defence in depth:

- The caller's `depth` integer in the wire arg is checked first.
- The coordinator walks the `delegated_to` ancestor chain
  independently via `TaskStore::delegation_chain_depth` and
  rejects if the chain (plus the new child) would exceed
  `max_depth`. A caller under-reporting `depth` still gets
  caught.

The cap exists because a delegating agent can recursively call
delegate again — without a cap, a buggy or malicious agent
spawns an infinite tree. The chain walk is the trust boundary.

## Enabling the executor

Add a `[coordinator.delegation]` section:

```toml
[coordinator.delegation]
enabled = true
max_depth = 3
max_concurrent = 5
executor_poll_secs = 5
max_job_secs = 300

[coordinator.delegation.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 60
```

| Field | Default | Notes |
|---|---|---|
| `enabled` | `true` | When `false` the executor loop is not spawned but the four `delegate.*` capabilities stay registered — operators can create children that just sit pending. |
| `max_depth` | `3` | Chain-walk + claimed-depth cap. |
| `max_concurrent` | `5` | Per-tick semaphore cap. |
| `executor_poll_secs` | `5` | Tick interval. |
| `max_job_secs` | `300` | Per-child hard timeout via `tokio::time::timeout`. |
| `ai_peer` | none | When absent the executor still picks up children but the AI step fails with cause `"ai dispatcher unset"` and the child flips to `failed`. |

When `[coordinator.delegation]` is **missing** the executor loop is
not spawned at all (the capabilities remain registered). This is
the honest contract: agents can still call `delegate.spawn`, the
edge + chronicle land, the child task is created — it just stays
`pending` until the executor is enabled.

## Using delegation

### From a SOL flow

```
let child_id: str = remote_call(
  "coordinator",
  "delegate.spawn",
  parent_task_id + "|" + goal + "|" + context + "||0"
);
// poll until terminal
loop {
  let result: str = remote_call("coordinator", "delegate.result", child_id);
  if result.starts_with("completed|") or result.starts_with("failed|") {
    break;
  }
  sleep(2);
}
```

### From the bridge / CLI

```
relix-cli ops delegate spawn \
  --parent-task-id <id> \
  --goal "summarise the last 24h of logs" \
  --context "filter to errors only" \
  --target-subject-id <agent-b>
```

Then watch the child task via `relix-cli task watch <child>`
(or `GET /v1/tasks/<child>`) or:

```
relix-cli ops delegate result --child-task-id <child>
```

### Cancelling

`delegate.cancel <child_task_id>|<reason>` (or `POST
/v1/delegate/cancel/<id>`) flips a non-terminal child to
`cancelled` and appends a `delegate.cancelled` chronicle event
with the reason. Refused (`INVALID_ARGS`) when the child is
already terminal.

## Capability surface

| Method | Arg | Return |
|---|---|---|
| `delegate.spawn`  | `parent_task_id\|goal\|context\|target_subject_id\|depth` | `<child_task_id>\n` |
| `delegate.result` | `<child_task_id>` | `status\|preview\|completed_at\n` (`-1` when not terminal) |
| `delegate.cancel` | `<child_task_id>\|<reason>` | `ok\n` |
| `delegate.list`   | `<parent_task_id>` | `<child>\t<goal>\t<status>\t<created_at>\n` per row + `count=N\n` |

The bridge proxies all four as JSON at `/v1/delegate/*`:

| Method | Path | Body / Query |
|---|---|---|
| POST | `/v1/delegate/spawn` | `{parent_task_id, goal, context?, target_subject_id?, depth?}` |
| GET | `/v1/delegate/result/:child_id` | — |
| POST | `/v1/delegate/cancel/:child_id` | `{reason?}` |
| GET | `/v1/delegate/list/:parent_id` | — |

## What happens to results

Two places to look:

- **`latest_result` column** on the child task (first 800 chars of
  the AI reply). Visible via `task.get` /
  `GET /v1/tasks/<child_id>`.
- **Chronicle events** on both tasks:
  - On the child: `delegate.completed` (with `chars=N|preview=...`)
    or `delegate.failed` (with `cause=...`).
  - On the parent: `delegate.child_completed`
    (`child_task_id=…|status=…|preview=…`) so the parent's
    timeline shows when each child landed.

## Hardening

| Concern | What the executor does |
|---|---|
| Runaway AI calls | `tokio::time::timeout(max_job_secs)` (default 300 s); exceeded children flip to `failed` with cause `"ai dispatch exceeded max_job_secs"`. |
| Parallel pileup | `max_concurrent` semaphore (default 5). Excess pending children wait for the next tick. |
| Pause / freeze / cancellation races | Cancelled tasks never run (the executor only picks up `pending` rows). Paused / frozen parents stay in their non-running state; the executor's parent-resume step only flips back when the parent is `awaiting_input`. |
| Infinite delegation chains | `max_depth` enforced twice — claimed depth + ancestor chain walk. |
| Coordinator restart | Pending children resume on the next tick. Parents stuck in `awaiting_input` after a crash mid-flight are re-resumed once the executor catches up with the child's outcome. |

The executor never crashes the coordinator on a child failure.
Every failure path is `tracing::warn!`; the loop keeps ticking.
