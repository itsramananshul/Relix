# Replay Model

Be very careful reading this. The Coordinator + Task model that ship
today are **not** full resumable replay. The semantics they deliver are
**checkpointed re-run**. If you confuse these two things you will design
the wrong system on top of Relix.

## The two regimes side by side

| Property | Checkpointed re-run (alpha, today) | Resumable replay (Gate 2, not built) |
|---|---|---|
| Task records survive Coordinator restart | yes | yes |
| Per-flow event log survives | yes | yes |
| `task.event` checkpoints survive | yes | yes |
| A flow interrupted mid-execution resumes from where it stopped | **no** | yes |
| Side effects already performed (`memory.write_turn` for the user turn) are observed and skipped on retry | **no** | yes |
| Idempotent retry of a flow with externally-observable side effects | **no** | yes |
| SOL VM yields at `remote_call` and persists between yields | **no** | yes |
| Operator can `task retry <id>` and get the flow to run again from the start | yes | yes |

Read that table once more. Today's system gives you everything **except
mid-flow resume**. Tomorrow's system (Gate 2) adds it.

## What "checkpointed re-run" actually means

After a flow runs (or fails):

- The Task record persists: id, title, flow_template, owner, params,
  status, latest_result, latest_flow_id, latest_flow_log_path, error_*.
- The per-flow event log on disk persists at the path the Task points
  to. That log has every `RemoteCallIssued` + `RemoteCallCompleted /
  RemoteCallFailed` with hash chaining.
- Caller-written checkpoints persist as rows in `task_events`.

If something interrupts the flow (executor crash, peer disappearance,
operator Ctrl-C), and the operator wants to try again, the path is:

1. Inspect: `relix-cli task get <id>` shows what status the task was
   left in, what the last result/error was, and the chronicle of
   checkpoint events.
2. Inspect deeper if needed: open `latest_flow_log_path` with
   `relix-cli flow-inspect --flow <path>` to see the exact remote
   calls the previous attempt made.
3. **Decide.** The operator decides whether to retry or abandon, based
   on what those records say. The system does not auto-decide.
4. Retry: re-issue the original chat / `flow-run` / task creation.
   The flow runs **from the start**. `memory.write_turn` runs again.
   `ai.chat` runs again. Whatever happened in the previous attempt is
   observable through the records but not replayed *for* you.

The Coordinator's value is that the *records* survive — so the operator
making the retry decision has real information to work with, instead of
"the bridge crashed, no idea what happened, just hit send again."

## Why we can't do real replay yet

The SOL VM is synchronous (SIMP-001, SIMP-014). `remote_call` blocks
the VM thread; the runtime bridges to async libp2p via
`spawn_blocking + Handle::current().block_on(...)`. The VM has no
yield instruction, no continuation form, no in-flight state that can
be persisted between calls and rehydrated on a different process.

To get real replay we need durable yield: the VM pauses at every
`remote_call`, persists its full state (operand stack, local frames,
program counter, heap-string arena), and a different executor can pick
it up later, replay the recorded `RemoteCallCompleted` results from
the per-flow event log without re-issuing the calls, and resume from
the next instruction.

That is a multi-week language change to the SOL VM and a protocol
change to the dispatcher. It is explicitly Gate 2 scope; the alpha
SIMP entries (specs/alpha-simplifications.md SIMP-001 + SIMP-014)
document the deferral.

We chose to ship the durable-records half now because:

- The records are independently valuable. An operator who can `task
  list` and `task get <id>` to see what happened is materially better
  off than an operator who can't. Audit, debugging, retry-decision
  support, future analytics — all of these depend only on the records.
- The records are *forwards-compatible* with real replay. When the SOL
  VM gains yield, the Coordinator's `task_events` table is exactly
  where the "VM checkpoint" rows will land. We are not building
  throwaway scaffolding.
- The honesty cost of shipping records now is just this document — as
  long as we don't pretend the records mean resume, nobody will design
  on a wrong assumption.

## Idempotency: who owns it today

Because retry restarts the flow from scratch, the *responder handlers*
have to be safe under repeated calls. Today:

| Handler | Safe under unintended replay? | Note |
|---|---|---|
| `memory.write_turn` | **No** — double-writes the turn | Handler does not de-dupe on caller-supplied request ids; alpha SIMP. |
| `memory.recent_for_session` | yes — read-only | |
| `memory.search` | yes — read-only | |
| `ai.chat` | provider-dependent — most LLM APIs are non-idempotent and charge | A retry will likely make a new (paid) call. |
| `tool.web_fetch` | **No** by design — `Idempotency::AtMostOnce` in the descriptor | Re-fetching an URL may return different content; some origins charge per request. |
| `task.*` | mixed: `task.create` is not idempotent, others are operations on a specific id | A naive retry of `task.create` makes two tasks. |

The Gate 2 work will:
1. Add typed `idem` keys to the wire envelope (RELIX-1 already reserves
   this field; alpha doesn't fill it).
2. Have responders de-dupe based on `idem`.
3. Let SOL replay observe a cached `RemoteCallCompleted` for an `idem`
   it has already seen, instead of re-issuing the call.

Until then, an operator triggering retry has to know what their flow's
side effects are.

## Failure modes today (and how to think about them)

### Flow runs to completion, then bridge crashes before responding to HTTP

- Task is `running` in the Coordinator.
- Flow event log has `FlowCompleted` with the result.
- Operator next-step: `task update --status completed --result <reply>`,
  using the result from the flow event log. The HTTP caller saw a 500
  and may have already retried — the duplicate request creates a new
  Task.

### Flow runs partway, executor dies, no retry

- Task is `running`.
- Flow event log has some `RemoteCallCompleted`s and possibly a
  `RemoteCallFailed` or nothing terminal.
- Operator next-step: `task update --status abandoned --error-cause
  'executor died'`. The previously-completed memory writes are real
  and observable on the memory peer.

### Flow runs partway, operator decides to retry

- Operator triggers the same flow again. It runs from the start.
  `memory.write_turn` runs again, double-writing the user turn. AI
  call happens again (potentially paid). Final `memory.write_turn`
  for the assistant turn happens once with the new reply.

### Coordinator dies mid-call

- SQLite is durable. The half-completed `task.update` either succeeded
  or didn't (transactional). After restart, the Coordinator is
  consistent.
- The caller saw a transport error and may have retried. If the retried
  call landed before the Coordinator restart, it failed with
  `DialFailure`. The bridge's MeshClient reconnect (commit `878359d`)
  will redial and retry once automatically.

### Caller dies after creating a Task but before running the flow

- Task is `pending` forever (no executor will pick it up; the alpha
  has no auto-scheduler).
- Operator next-step: `task update --status abandoned`, or trigger the
  flow manually with the params from `task get`.

## What "real replay" would look like (preview, not built)

For context, the Gate 2 design that this document's existence keeps
honest about deferring:

1. SOL VM gains `Inst::Yield` semantics. Every `remote_call` is a
   yield point; the VM serialises its full state to a Task checkpoint
   row before issuing the call.
2. `RemoteCallCompleted` rows in the flow event log are content-
   addressable by `idem`. On resume, a new executor reads the previous
   completed calls and rebuilds the VM heap from them without
   re-issuing the network calls.
3. Coordinator owns leasing: an executor that wants to run / continue a
   Task obtains a lease (heartbeated). If a leaseholder fails, the
   lease expires and another executor can pick it up. Multi-executor
   reconciliation lives here.
4. `task retry` becomes "resume from the next unobserved opcode" by
   default, with `task retry --from-scratch` as the opt-in for the
   today-style restart.

None of that is in the alpha. The current Coordinator + Task model
gives you the *substrate* on which that lands without rewriting the
records you've already accumulated.

## TL;DR

- Records persist. Mid-flow state does not.
- Retry restarts the flow. Side effects observed by the previous
  attempt are real and not undone.
- Operators make retry decisions. The system does not.
- Real resume is Gate 2.

If a document, commit message, demo, or pitch claims more than that
about the alpha's "replay", it's wrong.

## See also

- [`coordination.md`](coordination.md) — the peer.
- [`task-runtime.md`](task-runtime.md) — schema + wire.
- [`current-limitations.md`](current-limitations.md) — global honesty list.
- [`flows-and-sol.md`](flows-and-sol.md) — why SIMP-001 / SIMP-014 are
  what they are.
- [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md) —
  the formal SIMP entries.
