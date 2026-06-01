# Multi-Agent Workflows

A workflow is a typed DAG of *agent steps*. Each step dispatches one capability call against a peer identified by alias, binds the response into a named output variable, and routes execution to downstream steps based on success / failure / always / parallel edges.

The workflow engine lives in `crates/relix-runtime/src/workflow/`. Workflow files live in `<data_dir>/workflows/*.workflow` (override with `RELIX_WORKFLOWS_DIR`). Three working examples ship in `examples/workflows/`.

## Quick start

```bash
relix workflow list                                       # show the catalog
relix workflow validate examples/workflows/chat-then-summarize.workflow
relix workflow run chat-then-summarize --input "Why is the sky blue?"
relix workflow status <execution-id>                      # look up a past run
```

## File format

A `.workflow` file is YAML with four top-level keys:

```yaml
name: my-workflow         # operator-visible name + filename stem
version: 1                # schema version — must be 1 today
description: One-line summary shown by `workflow list`.

agents:                   # map of step-name → AgentSpec
  step_one:
    peer: ai              # peer alias (matches [peers] in controller.toml)
    capability: ai.chat   # capability method
    input: "{{workflow.input}}"
    output: step_one      # variable name later steps reference

flow:                     # the execution graph
  start: step_one         # which step runs first
  edges: []               # ordered list of directed edges
  result: "{{step_one.output}}"  # template projected as the final result
```

### Agent step

Each agent step is one capability call:

| field        | type   | purpose                                                           |
|--------------|--------|-------------------------------------------------------------------|
| `peer`       | string | Peer alias resolved through the controller's `[peers]` config.    |
| `capability` | string | Capability method name (e.g. `ai.chat`, `memory.search`).         |
| `input`      | string | Template with `{{workflow.input}}` / `{{<step>.output}}` markers. |
| `output`     | string | Variable name later steps interpolate via `{{<output>.output}}`.  |

Output names must be unique across agents. Step names are used for graph edges; output names are referenced inside `{{...}}` markers.

### Flow graph

`flow.start` names the agent step the executor runs first. `flow.edges` is an ordered list; each edge has a `from`, a `to`, and a `condition`:

| condition  | fires when…                                                                  |
|------------|------------------------------------------------------------------------------|
| `success`  | the `from` step's dispatch returned OK.                                      |
| `failure`  | the `from` step's dispatch returned ERR (transport / deadline / responder).  |
| `always`   | regardless of outcome — used for cleanup or always-run join steps.           |
| `parallel` | fan-out fork — all `parallel` edges from the same source fire concurrently.  |

`flow.result` is an optional template projected as the workflow's final return value. When omitted, the workflow returns the last output bound during execution.

### Variable interpolation

Two kinds of variables are visible inside `input` templates and `flow.result`:

- `{{workflow.input}}` — the global input string the caller passed to `workflow.run`. Always defined.
- `{{<output>.output}}` — the response body of an upstream agent step. Defined once that step has run.

`{{name}}` markers are whitespace-tolerant inside the braces. Non-identifier or unterminated markers are preserved verbatim so operators see typos rather than silent eats. Markers referencing variables that are not visible at the step's point in the graph fail validation up front (not at execution).

## Execution semantics

### Sequential
A success edge from A to B causes B to run after A. B sees A's bindings.

### Conditional
Two edges from the same source — one `success`, one `failure` — give classic if-else routing.

### Parallel + join
`parallel` edges from a single source fan out concurrently. Each branch runs in its own copy of the execution state. When ALL siblings finish, the executor merges their bindings + traces back into the parent and THEN follows each sibling's outgoing edges with the merged state. A join step downstream of multiple siblings runs exactly once (the visited-set keeps it from re-executing) and sees every sibling's output.

### Failure handling
A step whose dispatch returned ERR routes along the matching `failure` (or `always`) edge if one exists. The error cause is bound to `{{<step>.output}}` so the failure handler can include it. A failed step with no matching failure or always edge stops the workflow and produces `WorkflowResult { status: failed }` with the agent name and the cause in the result string.

### Cooperative cancellation

Execution can be stopped mid-run via a `CancellationFlag`. The flag is checked before each dispatch (after a `yield_now()` so event-drain tasks can process the preceding step's completion). When the flag fires, the executor records a `Cancelled` event and returns `ExecutionStatus::Cancelled` with the reason string. The flag is wired by the verification harness when a required-step check fails; callers can also pass one explicitly through `execute_with_cancellation`.

### Execution status

| status            | meaning                                                                             |
|-------------------|-------------------------------------------------------------------------------------|
| `success`         | Every executed step succeeded and the final result resolved.                        |
| `partially_failed`| Workflow completed (failure handler recovered) but trace contains an err step.      |
| `failed`          | A step failed with no failure handler; workflow stopped.                            |
| `cancelled`       | A `CancellationFlag` was set mid-execution (e.g. by the verification harness).      |

## Validation

`relix workflow validate <file>` runs the same checks the engine runs at load:

- Every edge endpoint must be a declared agent.
- Output bindings must be unique across agents.
- Variable references must resolve to upstream outputs or `workflow.input`.
- No cycles in success-path edges. Failure / always edges are treated as recovery loops and NOT counted as cycles.
- When peers are configured on the coordinator, every `agent.peer` must exist in `[peers]`.

Parse errors carry exact `(line, column)` positions; validation errors name the offending field, variable, or cycle path.

## WorkflowStore security

`WorkflowStore` applies three layers of defense before reading a file from disk:

1. **Name validation** — `validate_name` rejects names that are empty, contain `/`, `\`, `..`, or a NUL byte, or resolve to anything other than a single normal path component. This catches drive-prefix attacks and other platform-specific edge cases the substring checks above might miss.
2. **Canonicalization check** — after joining the validated name to the store directory, both paths are `canonicalize()`d; the resolved path must still start with the canonical store directory, blocking symlink escapes.
3. **Size cap** — the file is `stat`-ed before opening; files over `MAX_WORKFLOW_BYTES` (4 MiB) are rejected with a `TooLarge` error without reading any bytes into memory.

`StoreError::InvalidName` is returned for steps 1 and 2; `StoreError::TooLarge` for step 3. `workflow.reload` clears the in-memory cache so updated files on disk take effect without a coordinator restart.

## Tenant isolation

`WorkflowChronicle` attributes every finished execution to a `tenant_id`. `workflow.status` looks up the record with `get_for_tenant`, scoping the read to the verified caller tenant. The legacy `record()` / `get()` paths write and read the reserved `'default'` tenant.

## HTTP API

The bridge exposes five endpoints at `/v1/workflows`:

| method + path                                       | purpose                              |
|-----------------------------------------------------|--------------------------------------|
| `POST /v1/workflows/run`                            | Execute by name.                     |
| `GET  /v1/workflows`                                | List the catalog.                    |
| `GET  /v1/workflows/status/:execution_id`           | Fetch a past execution.              |
| `POST /v1/workflows/validate`                       | Type-check a source string.          |
| `POST /v1/workflows/reload`                         | Drop the workflow file cache.        |

### Run request body

```json
{ "name": "chat-then-summarize", "input": "Hi", "stream": false }
```

When `stream: true`, the endpoint returns `text/event-stream` via the `workflow.run.stream` capability carrying live per-step events:

| event name        | payload                                                   |
|-------------------|-----------------------------------------------------------|
| `started`         | `{ execution_id, workflow_name }`                         |
| `step_started`    | `{ agent, peer, capability, input }`                      |
| `step_completed`  | `{ agent, peer, capability, latency_ms, output }`         |
| `step_failed`     | `{ agent, peer, capability, latency_ms, error }`          |
| `finished`        | full execution record (same shape `workflow run` returns) |
| `cancelled`       | `{ agent, reason }` — emitted when a `CancellationFlag` fires mid-run |

`execution_id` in the `started` event is a 32-character hex string (16 random bytes). It is the key used by `workflow.status` to retrieve the persisted record.

## Capability surface

The coordinator registers six capabilities the bridge forwards to:

- `workflow.run`        — unary execution; returns the full execution record.
- `workflow.run.stream` — streaming variant emitting per-step JSON frames over SSE.
- `workflow.list`       — enumerate every workflow in the directory.
- `workflow.status`     — fetch a past execution by id.
- `workflow.validate`   — type-check a source string without touching the catalog.
- `workflow.reload`     — drop the workflow-file cache so freshly-edited `.workflow` files pick up without a coordinator restart.

## Persistence

Each finished execution is persisted to `<data_dir>/workflows.sqlite` (separate from the task chronicle so workflow lifecycle doesn't entangle with the task schema's migrations). `workflow.status` looks the record up by execution id and survives controller restarts.

## Example: sequential

```yaml
name: chat-then-summarize
version: 1
agents:
  responder:
    peer: ai
    capability: ai.chat
    input: "{{workflow.input}}"
    output: responder
  summarizer:
    peer: ai
    capability: ai.chat
    input: "Summarise: {{responder.output}}"
    output: summary
flow:
  start: responder
  edges:
    - { from: responder, to: summarizer, condition: success }
  result: "{{summary.output}}"
```

Full versions of the three core patterns (sequential, conditional, parallel) live in `examples/workflows/`.
