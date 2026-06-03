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
- `/v1/browser/captures/:filename` accepts optional `task_id`/`run_id`, stamps
  `task_id` into the mesh dispatch envelope, records durable activity, and
  returns scope metadata as response headers for the PNG payload
- `/v1/email/send` and `/v1/email/send_template` accept optional
  `task_id`/`run_id`, stamp `task_id` into the mesh dispatch envelope, record
  durable activity without leaking message body/subject content, and add scope
  metadata to object responses
- `/v1/messages` send plus `/v1/messages/{id}/read` and
  `DELETE /v1/messages/{id}` accept optional `task_id`/`run_id`, stamp
  `task_id` into the mesh dispatch envelope, record durable activity without
  leaking message body/subject content, and return scope metadata in mutation
  responses
- `/v1/plugins/:id/reload` and `/v1/plugins/:id/disable` accept optional
  `task_id`/`run_id`, stamp `task_id` into the mesh dispatch envelope, record
  durable activity, and return scope metadata in mutation responses
- memory write proxies (`/v1/memory/ingest`, `/v1/memory/ingest_image`,
  `/v1/memory/context_flush`, quarantine decisions, record edits/freezes, and
  model refresh requests) plus `/v1/memory/export` accept optional
  `task_id`/`run_id`, strip that bridge metadata before forwarding, stamp
  `task_id` into the mesh dispatch envelope, record durable activity without
  copying document/image/export payloads, and add scope metadata to object
  responses
- manual memory curation (`POST /v1/memory/curate`) accepts optional
  `task_id`/`run_id`, stamps `task_id` into the mesh dispatch envelope, records
  durable activity, appends best-effort task events for bound calls, and returns
  scope metadata in the response
- standalone memory embedding writes (`/v1/memory/embed` and
  `/v1/memory/embed_all`) accept optional `task_id`/`run_id`, stamp `task_id`
  into the mesh dispatch envelope, record durable activity without copying raw
  text payloads, and return scope metadata
- knowledge transfer mutations (`/v1/knowledge/share`,
  `/v1/knowledge/broadcast`, `/v1/knowledge/revoke`, and
  `/v1/knowledge/recall`) accept optional `task_id`/`run_id`, stamp `task_id`
  into the mesh dispatch envelope, record durable activity without copying
  shared messages or recalled context, and return scope metadata for object
  responses
- skill-store mutations (`POST /v1/skills`, `PATCH /v1/skills/{id}`, and
  `POST /v1/skills/{id}/deprecate`) accept optional `task_id`/`run_id`, stamp
  `task_id` into the mesh dispatch envelope, record durable activity without
  copying skill bodies or deprecation reasons, and return scope metadata
- credential vault operations (`/v1/credentials` reads and mutations) accept
  optional `task_id`/`run_id`, stamp `task_id` into the mesh dispatch envelope,
  record durable activity without copying secret values or revoke reasons, and
  return scope metadata
- workflow execution and cache reload (`POST /v1/workflows/run` and
  `POST /v1/workflows/reload`) accept optional `task_id`/`run_id`, stamp
  `task_id` into unary and streaming mesh dispatch envelopes, record durable
  activity without copying workflow input, and return scope metadata through
  JSON fields or SSE response headers
- delegation mutations (`POST /v1/delegate/spawn` and
  `POST /v1/delegate/cancel/{child_id}`) stamp the parent/child task id into
  mesh dispatch envelopes, record durable activity without copying delegated
  goal/context/reason text, and return scope metadata for mutation responses
- cron scheduler mutations (`POST/PATCH/DELETE /v1/cron/jobs` and
  `POST /v1/cron/jobs/{id}/trigger`) accept optional `task_id`/`run_id`, stamp
  scope task ids into mesh dispatch envelopes, record durable activity without
  copying recurring prompts, and keep trigger-launched task ids separate from
  request scope ids
- standing approval mutations (`POST /v1/agents/{id}/standing-approvals` and
  `DELETE /v1/standing-approvals/{id}`) stamp available task scope into mesh
  dispatch envelopes, record durable activity without copying operator notes,
  and return scope metadata for create/revoke responses
- planning mutations (`POST /v1/planning/plan`, `POST /v1/planning/approve`,
  and `POST /v1/planning/reject`) accept optional `task_id`/`run_id`, stamp
  `task_id` into mesh dispatch envelopes, record durable activity without
  copying plan specs or operator notes, and return scope metadata for object
  responses
- execution rollback (`POST /v1/execution/rollback`) accepts optional
  `task_id`/`run_id`, stamps `task_id` into the mesh dispatch envelope, records
  durable activity, appends best-effort task events for bound calls, and returns
  scope metadata for object responses
- budget reset (`POST /v1/budget/reset`) accepts optional `task_id`/`run_id`,
  stamps `task_id` into the mesh dispatch envelope, records durable governance
  activity, appends best-effort task events for bound calls, and returns scope
  metadata for object responses
- confidence reset (`POST /v1/confidence/reset`) accepts optional
  `task_id`/`run_id`, stamps `task_id` into the mesh dispatch envelope, records
  durable governance activity, appends best-effort task events for bound calls,
  and returns scope metadata for object responses
- training export/score/delete operations (`POST /v1/training/export`,
  `POST /v1/training/score/{id}`, and
  `DELETE /v1/training/interactions/{id}`) accept optional `task_id`/`run_id`,
  stamp `task_id` into the mesh dispatch envelope, record durable activity
  without copying export paths or training examples, append best-effort task
  events for bound calls, and return scope metadata for object responses
- config mutations/tests (`PUT /v1/config/providers/default`,
  `PUT /v1/config/telegram`, and `POST /v1/config/telegram/test`) accept
  optional `task_id`/`run_id`, record durable activity without copying raw
  tokens, webhook URLs, or upstream failure text, append best-effort task
  events for bound calls, and return scope metadata for mutation/test responses
- identity mutations (`POST /v1/identity/tokens`,
  `POST /v1/identity/tokens/revoke`, and `POST /v1/identity/research`) accept
  optional `task_id`/`run_id`, stamp `task_id` into mesh dispatch envelopes,
  record durable activity without logging issued token values or research
  subject/context text, and return scope metadata for object responses
- belief reset (`POST /v1/belief/{session_id}` with `{"action":"reset"}`)
  accepts optional `task_id`/`run_id`, stamps `task_id` into the mesh dispatch
  envelope, records durable activity, appends best-effort task events for bound
  calls, and returns scope metadata for object responses
- Rust/Python/TypeScript SDK chat responses surface task and workspace binding
  fields; Python/TypeScript callers can also pass workspace lease ids into chat
  and streaming chat so SDK users can bind work to the same execution context
- standalone CLI flow runs remain unbound unless the caller explicitly grows a
  task binding path

Remaining launch work:

- make task creation fail-closed for production modes: implemented through
  `[coordinator] required = true`, which refuses startup when the coordinator
  alias is unavailable and refuses chat dispatch when `task.create` fails
- bind remaining direct bridge utility calls to tasks or explicit ad-hoc runs
- attach run ids to the same execution context; bridge chat/OpenAI/WS paths
  now accept a workspace lease id and stamp the resolved workspace path into
  dispatch envelopes

## Phase 3: Scoped Approvals

Status: started.

Single-call approvals are not enough for autonomous work. Relix now supports
standing approvals scoped by task, session, capability/method prefix, workspace
path, category, expiry time, call count, and estimated spend. The remaining
launch-critical work is making that scope obvious in the dashboard.

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
- until a budget limit: implemented through `max_cost_micros` and atomic
  `cost_used_micros` consumption in the standing-approval admission gate

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
- active run: implemented as optional `run_id`; chat/OpenAI/WS flows now
  automatically bind the created coordinator task onto the resolved
  workspace lease (`workspace.bind_run` activity), clearing any stale
  lease `run_id` when no fresh run id is available so a lease cannot
  show a new task paired with an old run
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
  observations. Per-run/per-call spend producers remain coordinator-side
  work: the bridge chat/OpenAI/WS execution paths do not observe token
  usage or provider cost (the OpenAI shim deliberately omits usage rather
  than report zeros), so honest per-run spend rows must be emitted by the
  coordinator/provider that actually meters the call, not synthesized at
  the bridge
- approval id: implemented for REST/dashboard and channel approval decisions;
  standing approval grants/revocations are recorded as approval-control tool
  activity
- policy result: implemented for recent policy-denial rows with idempotent
  activity ids
- planning, execution rollback, memory, knowledge-transfer, skill-store,
  training export/score/delete, config mutation/test, credential, workflow,
  delegation, and cron operations:
  implemented for the GAP 5 bridge memory-write proxies, standalone embedding
  writes, knowledge share/broadcast/revoke/recall calls, skill-store mutations,
  training export/score/delete calls, config mutation/test calls, credential
  vault reads/mutations, workflow run/reload calls, delegation spawn/cancel
  calls, scheduler mutations, planning create/approve/reject calls, and
  execution rollback calls without logging raw
  document/image/text/knowledge-message/skill/training examples/export
  paths/config-secret/webhook URL/upstream failure text/secret/workflow-input/
  delegation/cron-prompt/spec/note payloads
- timestamp: implemented
- budget reset: implemented as task-aware governance activity
- confidence reset: implemented as task-aware governance activity

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
- budget reset calls from `/v1/budget/reset`
- confidence reset calls from `/v1/confidence/reset`
- training export/score/delete calls from `/v1/training`
- provider default, Telegram save, and Telegram test calls from `/v1/config`
- identity token issue/revoke and research-backed identity calls from
  `/v1/identity`
- belief reset calls from `/v1/belief/{session_id}`
- policy denials discovered through `/v1/policy/denials`
- cost aggregate observations from `/v1/metrics/cost`
- MCP invocations from `/v1/mcp/invoke`
- screen captures from `/v1/tools/screen`
- browser capture reads from `/v1/browser/captures/:filename`
- plugin management mutations from `/v1/plugins/:id/reload` and
  `/v1/plugins/:id/disable`
- memory writes and exports from GAP 5 bridge proxies
- manual memory curation from `/v1/memory/curate`
- memory embedding writes from `/v1/memory/embed` and `/v1/memory/embed_all`
- knowledge share/broadcast/revoke/recall calls from `/v1/knowledge`
- skill-store mutations from `/v1/skills`
- credential vault reads and mutations from `/v1/credentials`
- workflow execution and reload calls from `/v1/workflows`
- delegation spawn/cancel calls from `/v1/delegate`
- cron scheduler mutations from `/v1/cron/jobs`
- standing approval create/revoke calls from `/v1/agents`
- planning create/approve/reject calls from `/v1/planning`
- execution rollback calls from `/v1/execution/rollback`
- outbound email sends from `/v1/email/send` and `/v1/email/send_template`
- agent-message send/read/delete calls from `/v1/messages`

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
surfaces with spine ids/status, and renders a visible per-section spine-status
badge (ready / partial / missing) driven by the manifest — so navigation
reflects the real control-plane contract at a glance instead of only a hover
tooltip. Internally each product surface is already a self-contained render
module in the `Loaders` registry, dispatched by `navigate -> loadSection`, with
the `SECTIONS` array as the nav registry; the badge rendering uses the safe
`el()` DOM builder so the strict single-page CSP and no-`innerHTML` guarantees
hold. The remaining work is the cosmetic file-level split (extracting each
`Loaders.*` module to its own source unit) without losing the single inline
artifact — a maintainability refactor, not a contract change.

## Phase 7: Tenant as a Hard Invariant

Tenant context must be mandatory for tenant-owned data. No handler should
silently fall back to `None` for memory, agents, tasks, approvals, credentials,
budget, or audit data in multi-tenant mode.

The bridge resolves every request's tenant from the authenticated
principal in `tenant_middleware`, which already fails closed in
multi-tenant mode: a credential with no `[auth.tenant_bindings]` entry
(or no credential at all) is rejected with HTTP 401 before any handler
runs. Inside a handler `current_tenant()` is therefore always the
caller's verified tenant in multi-tenant mode, so the activity-attribution
`unwrap_or(DEFAULT_TENANT)` fallbacks only ever apply in single-tenant
mode. The remaining invariant work is the class of handler that reads or
writes tenant-owned data using a caller-supplied tenant value instead of
the verified one.

Current progress:

- standing approval list now resolves through the verified invocation tenant
  instead of returning same-agent rows across every tenant
- the durable activity ledger read (`GET /v1/activity/recent`) now forces
  its tenant filter to the verified per-request tenant and discards any
  caller-supplied `tenant_id`, closing the cross-tenant audit read where
  `?tenant_id=<victim>` (or an omitted filter) exposed another tenant's
  ledger
- identity-token issue (`POST /v1/identity/tokens`) reconciles the optional
  body `tenant_id` override against the verified tenant: in multi-tenant
  mode the verified tenant is forced and a disagreeing claim is rejected
  with HTTP 403, so a caller cannot mint a session token bound to another
  tenant; single-tenant mode still allows the override for seeding

Known out-of-scope for the silent-fallback invariant: the administrative
enumeration surfaces (`GET /v1/audit/tenants/:tenant_id` and
`GET /v1/policy/tenants/:tenant_id`) are intentionally cross-tenant
operator-console proxies. They need an admin/trusted-origin access gate
rather than per-tenant scoping; that is a separate auth-model change.

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

Current progress:

- `relix setup` runs dependency preflight before entering raw terminal mode
- non-interactive setup writes defaults instead of hanging on key reads
- the final setup screen now prints a concrete first-run checklist: `relix boot`,
  the configured dashboard URL, bridge token file path, a first `/v1/chat` curl
  smoke test, health/stop/reconfigure commands, and explicit warnings for
  missing dependencies, provider keys, or credential-vault master keys
