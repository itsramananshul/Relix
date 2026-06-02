# Relix Product Spine Roadmap

Relix has powerful subsystems, but the product needs a single operating model.
The target spine is:

```text
tenant -> goal -> agent -> task -> run -> event -> approval/budget
```

The immediate goal is not to copy Paperclip's stack. The goal is to force every
Relix subsystem to hang from durable work objects instead of exposing a pile of
raw capabilities.

## Phase 1: Canonical Control-Plane Contract

Status: started.

Add a single typed overview endpoint that names every spine layer, links the
routes that back it, and exposes the honest gap for that layer.

Current endpoint:

```text
GET /v1/control-plane/spine
```

This gives the dashboard and CLI one source of truth for which product surfaces
exist, which are partial, and which are missing.

## Phase 2: Task-Bound Execution

Status: started.

Every high-risk execution path must carry task/run context:

- `ai.chat`
- tool calls
- plugin calls
- MCP calls
- memory writes
- credential access
- terminal/filesystem operations

Anonymous capability execution should become an explicit ad-hoc run, not a
silent side path.

Current progress:

- bridge chat / tool-chat / OpenAI shim flows create a coordinator task when
  the recorder is wired
- flow-issued unary and streaming `remote_call` envelopes now carry that
  `task_id`, so responder-side approval gates can bind risky calls back to the
  durable task
- `/v1/mcp/invoke` accepts optional `task_id`/`run_id`, stamps `task_id` into
  the mesh dispatch envelope, and records durable activity plus best-effort task
  events for bound calls
- `/v1/tools/screen` accepts optional `task_id`/`run_id`, stamps `task_id` into
  the mesh dispatch envelope, records durable activity, and adds scope metadata
  to object responses
- standalone CLI flow runs remain unbound unless the caller explicitly grows a
  task binding path

Remaining launch work:

- make task creation fail-closed for production modes: implemented through
  `[coordinator] required = true`, which refuses startup when the coordinator
  alias is unavailable and refuses chat dispatch when `task.create` fails
- bind plugin/direct bridge utility calls to tasks or explicit ad-hoc runs
- attach run ids to the same execution context; bridge chat/OpenAI/WS paths
  now accept a workspace lease id and stamp the resolved workspace path into
  dispatch envelopes
- expose the binding clearly in dashboard and SDK responses

## Phase 3: Scoped Approvals

Status: started.

Single-call approvals are not enough for autonomous work. Relix now supports
standing approvals scoped by task, session, capability/method prefix, workspace
path, category, expiry time, and call count. The remaining launch-critical work
is making that scope obvious in the dashboard/CLI and adding hard budget limits.

Approval scopes:

- one call
- one task: implemented via `scope_kind = "task"` and `task_id`
- one session: implemented in the store/API and wired through bridge flow dispatch via `session_id`
- one agent plus capability family: implemented via `scope_kind = "method_prefix"`
- one workspace path: implemented in the store/API; bridge chat/OpenAI/WS
  paths resolve active workspace leases and stamp the resolved `workspace_path`
  into dispatch envelopes
- until a time limit: implemented through `expires_at`
- until a call-count limit: implemented through `max_calls` and atomic
  `calls_used` consumption in the admission gate
- until a budget limit: still missing

Approval decisions write durable activity events; standing approvals are
revocable.

## Phase 4: Execution Workspaces

Status: started.

Relix needs first-class workspace leases:

- local path or sandbox id: implemented as a persisted lease field
- git branch/worktree: implemented as lease metadata
- provision command: implemented and executed on lease creation with
  `RELIX_WORKSPACE_*` environment binding
- teardown command: implemented and executed before lease release; failures
  mark the lease `cleanup_failed`
- owner agent: implemented
- active run: implemented as optional `run_id`, but not automatically bound yet
- chat/OpenAI/WS execution binding: implemented through `workspace_lease_id`
  request metadata resolved against active tenant-owned leases
- cleanup status: implemented
- failure reason: implemented

Without this, agent work cannot be reliably resumed, audited, cancelled, or
rolled back.

Current endpoints:

```text
GET  /v1/workspaces
POST /v1/workspaces
GET  /v1/workspaces/{lease_id}
POST /v1/workspaces/{lease_id}/release
```

## Phase 5: Durable Activity Ledger

Status: started.

Unify scattered rings/logs/provenance into one durable activity ledger:

- actor: implemented for workspace and intervention events
- tenant: implemented for workspace events; intervention events currently default
- task: implemented when a workspace or intervention target carries a task id
- run: implemented for workspace events
- method/action: implemented as `action`; method remains optional
- decision: implemented
- cost: implemented for idempotent `/v1/metrics/cost` aggregate
  observations; per-run/per-call spend producers are still missing
- approval id: implemented for REST/dashboard and channel approval decisions
- policy result: implemented for recent policy-denial rows with idempotent
  activity ids
- timestamp: implemented

The operator question "what happened?" should not require scraping five
different surfaces.

Current endpoint:

```text
GET /v1/activity/recent
```

Current durable source:

```text
<data_dir>/bridge-activity.jsonl
```

Current producers:

- workspace lease create/release
- operator intervention audit rows
- approval decisions from the dashboard/API and channel callbacks
- policy denials discovered through `/v1/policy/denials`
- cost aggregate observations from `/v1/metrics/cost`
- MCP invocations from `/v1/mcp/invoke`
- screen captures from `/v1/tools/screen`

## Phase 6: Dashboard Decomposition

Status: started.

The embedded dashboard should stop growing as one giant HTML file. Split the UI
into product surfaces aligned with the spine:

- Overview
- Work
- Agents
- Runs
- Approvals
- Budgets
- Memory
- Activity
- Settings

The dashboard should consume `/v1/control-plane/spine` so navigation reflects
the real product contract instead of hard-coded endpoint guesses.

Current endpoint:

```text
GET /v1/control-plane/dashboard
```

The current dashboard now consumes the dashboard manifest and annotates sidebar
surfaces with spine ids/status. The remaining work is the real split: move each
surface into maintainable modules without losing the strict single-page CSP and
auth bootstrap guarantees.

## Phase 7: Tenant as a Hard Invariant

Tenant context must be mandatory for tenant-owned data. No handler should
silently fall back to `None` for memory, agents, tasks, approvals, credentials,
budget, or audit data in multi-tenant mode.

Current progress:

- standing approval list now resolves through the verified invocation tenant
  instead of returning same-agent rows across every tenant

## Phase 8: Setup That Does Not Waste The User's Life

The first-run path should be:

```text
relix setup
relix mesh up
open dashboard
create first task
watch first run
```

Every required config value should be either generated, validated, or explained
with the exact fix.
