# Multi-Specialist Planning (RELIX-7.24)

The planning subsystem (`crates/relix-runtime/src/planning/`) is a multi-stage, multi-agent pipeline that sits on top of the workflow engine. An operator writes a natural-language specification; the pipeline parses it, scores the registry, optionally orchestrates multiple specialists, resolves conflicts, adversarially reviews, optionally gates on human approval, and optionally verifies step outputs — then executes the resulting workflow through the standard executor. It does not replace or duplicate any workflow logic.

> **Scope**: `~10 000 lines`, zero prior documentation. This page covers everything operators need to configure, invoke, and operate the planning subsystem in Relix 0.4.1.

---

## Five-stage pipeline

`planning.create_plan` drives the pipeline. Stages run in order:

```
spec text
   │
   ▼
1. SpecParser        → PlanSpec (complexity score, spec_id, signature, changelog)
   │
   ▼
2. Orchestrator      → sub-goal decomposition + specialist assignment (if complex)
   │
   ▼
3. ConflictResolver  → duplicate outputs, interfering parallel writes, undefined refs
   │
   ▼
4. CriticLoop        → adversarial AI review; injects issues as constraints; regenerates
   │
   ▼
5. ApprovalGate      → optional human approval gate
   + VerificationHarness → optional step-level output verification
   │
   ▼
  workflow::execute
```

### Stage 1 — SpecParser

`SpecParser::parse` converts a free-text spec into a `PlanSpec`. It splits the input on `.`, `!`, `?`, `;`, and newlines into sentences and classifies each:

- **constraints** — sentences containing any of: `must not`, `should not`, `must`, `do not use`, `do not`, `avoid`, `without`, `no more than`, `under`, `less than`, `at most`, `never`.
- **success_criteria** — sentences containing any of: `return`, `produce`, `output`, `result should`, `ensure`, `summary`, `report`, `deliver`, `must include`, `should include`.
- **preferred_agents** — known agent names that appear verbatim in the spec (without a preceding negation prefix).
- **forbidden_agents** — known agent names preceded within ~50 characters by: `do not use`, `don't use`, `without`, `avoid`, `exclude`, `not allowed`, `forbidden`, `never use`; negation is cancelled if a clause-break (`and`, `or`, `then`, `but`, `also`, `plus`, `,`, `;`) appears between the prefix and the mention.
- **budget_hint** — first match of: `tokens`, `cheap`, `expensive`, `fast`, `slow`, `cost`, `budget`.

Pass `SpecParser::with_known_agents(names)` to enable agent-mention extraction.

#### Complexity scoring

Four triggers, each contributing `0.7`, summed and capped at `1.0`. Any single trigger clears the default threshold:

| trigger | condition |
|---------|-----------|
| many success criteria | `success_criteria.len() > 3` |
| many constraints | `constraints.len() > 5` |
| long goal | `goal word count > 150` |
| multiple output types | `>= 2` distinct output-type keywords in the spec (`report`, `code`, `summary`, `analysis`, `plan`, `design`, `implementation`, `documentation`) |

`is_complex = complexity_score >= 0.6` (`DEFAULT_COMPLEXITY_THRESHOLD`). `PLAN_SPEC_VERSION = 1`.

### Stage 2 — Orchestrator

The orchestrator activates only when ALL three conditions hold:

1. `[planning] enabled = true`
2. `max_agents > 1` (from the call args)
3. `spec.complexity_score >= complexity_threshold` (default `0.6`)

When active it asks an AI agent (configured via `orchestrator_agent` / `orchestrator_peer`) to decompose the goal into 2–4 sub-goals. Specialists are assigned from the registry by scored match. Sub-plans are built in parallel (up to `max_parallel_specialists`, default 4) and merged into a single workflow. When the mesh cell is empty or the AI is unreachable, the orchestrator falls back to `heuristic_decompose`, which splits the goal on clause boundaries without AI.

**Agent selection order** (both `PlanGenerator` and `Orchestrator`):
1. Preferred agents from the spec (in spec order), skipping forbidden.
2. Registry-scored agents (descending score), skipping forbidden and duplicates.
3. Capped to `min(max_agents, max_steps_from_spec)`.

**Registry scoring**: tag keyword match = +3 per keyword, method-name-segment match = +2, description or capability description match = +1. Minimum token length 3; stopwords stripped.

**Topology selection** (`PlanGenerator`):
- 1 agent → `Single`
- Parallel keywords in original spec → `Parallel` (fan-out + synthetic `merge` step on seed peer)
- Sequential keywords or default → `Sequential` (chain via `Success` edges)

Sequential keywords: `then`, `after`, `next`, `followed by`, `step by step`, `pipeline`.  
Parallel keywords: `compare`, `contrast`, `in parallel`, `concurrently`, `multiple angles`, `multiple sources`, `each of`, `independent`, `simultaneously`, `and also`.

Planner-generated workflows are named `planning__<slug>` (single-agent) or `planning_orch__<slug>` (orchestrator), where `<slug>` is up to 48 characters of the goal with non-alphanumeric characters replaced by `_`.

### Stage 3 — ConflictResolver

Detects and fixes three classes of conflict in the generated workflow before the critic sees it:

| kind | detection | strategy |
|------|-----------|----------|
| `DuplicateOutput` | two agents bind the same output variable | keeps alphabetically-first producer; renames others to `{base}_{n}` |
| `InterferingParallelCall` | two parallel siblings call the same `(peer, capability)` where the capability is write-like | keeps `members[0]` parallel; re-sequences others after it via a `Success` edge |
| `UndefinedReference` | agent input or `flow.result` references a variable not produced by any reachable upstream step | strips the unknown `{{name.output}}` marker; agent still runs |

Write-like capability keywords: `set`, `put`, `write`, `create`, `delete`, `update`, `post`, `send`, `publish`, `mutate`.

If validation still fails after all three fixes, `report.escalated` is set and `planning.create_plan` returns `INVALID_ARGS`. The full `ConflictResolutionReport` is included in the response when any conflicts were detected.

### Stage 4 — CriticLoop

The critic sends the current workflow + spec to an AI agent (configured via `critic_agent` / `critic_peer`) and asks it to return a structured verdict: `approved: bool`, `issues: [string]`, `suggestions: [string]`.

- If approved, the loop exits and the plan proceeds.
- If not approved, `inject_feedback` appends `issues` as constraints to the spec (signing the updated spec), and the plan is regenerated. This repeats up to `max_critic_rounds` times (default 3).
- After `max_critic_rounds` without approval the loop returns the best plan with a warning in the response.
- If the AI is unreachable or returns an unparseable response, the loop exits with a warning (`"plan not adversarially reviewed"`). The `critic_approved` field in the response is `true` when skipped by `dry_run = true`; distinguish a genuine skip from a real review pass by checking `critic.rounds == 0`.

### Stage 5 — Approval gate and verification

#### Human approval gate

When `require_approval = true` and an `ApprovalStore` is wired:

- `planning.create_plan` persists the plan as `pending`, fans out notifications to every `[[planning.approval_targets]]` channel, and returns immediately with `approval.status = "pending"`. No execution happens.
- An operator calls `planning.approve_plan` or `planning.reject_plan`.
- On approve, the stored `workflow_yaml` is re-parsed and executed via the mesh dispatcher.

If `require_approval = true` but no `ApprovalStore` is configured, `planning.create_plan` returns `RESPONDER_INTERNAL`.

Per-call `require_approval` in the `planning.create_plan` args overrides the global config.

`planning.approve_plan` verifies the `PlanSpec` signature (blake3) before executing. A tampered spec returns `INVALID_ARGS`.

The approval expiry sweep runs every 60 seconds; plans older than `approval_timeout_secs` (default 3600) are auto-expired.

#### Verification harness

When `verify_steps = true`, each step's output is evaluated against the `success_criteria` from the spec using one of five strategies. The picker stops at the first match:

| strategy | trigger |
|----------|---------|
| `LengthCheck` | criterion contains a length phrase (`under`, `at most`, `no more than`, `less than`, `fewer than`, `up to`) AND contains `word` or `token` |
| `KeywordAbsence` | criterion contains `must not include`, `must not contain`, `should not include`, `should not contain`, `without`, `no mention of`, `do not include`, or `do not mention` |
| `KeywordPresence` | criterion contains `must include`, `must contain`, `should include`, `should contain`, `include the`, `contain the`, or `must mention` |
| `PatternMatch` | criterion starts with `/` and ends with `/` (explicit regex marker) |
| `AiJudge` | all other criteria |

Security invariants for `PatternMatch`:
- Regex compile failures **fail closed** (verification step fails; execution is not allowed to proceed on a bad pattern).
- Regex timeouts (ReDoS guard, default `regex_timeout_ms = 100` ms) also **fail closed**.
- An `AiJudge` that is unreachable or returns an unparseable response **assumes pass** (non-blocking).

When `required_steps` is non-empty, a verification failure on any listed step cancels the workflow immediately via `CancellationFlag.cancel_with_reason(...)` before the next BFS step starts and sets the workflow result to `Failed`. When `required_steps` is empty, all failures are advisory; `passed = true` only if no advisory failures occurred.

---

## PlanSpec tamper evidence

Every `PlanSpec` carries three tamper-evidence fields:

| field | purpose |
|-------|---------|
| `spec_id` | UUID v4 (hyphenated); stable across pipeline revisions — two parses of identical text produce different `spec_id`s |
| `signature` | blake3 hex of canonical JSON (all fields except `signature` itself; keys sorted, no whitespace) |
| `changelog` | append-only list of `{changed_at_ms, change_type, description}` entries |

`sign()` computes and stores the signature. `verify()` recomputes and compares; mismatch returns `SpecVerificationError::Mismatch`. `with_change()` appends to changelog and clears `signature`; `with_change_and_sign()` does both atomically. All pipeline mutations (critic feedback injection, conflict resolution recording) call `with_change_and_sign`.

`planning.approve_plan` calls `verify()` before executing; a mismatched signature is rejected with `INVALID_ARGS`.

---

## Configuration

All keys are under `[planning]` in `controller.toml`. `OrchestratorConfig`, `CriticConfig`, and `VerificationConfig` fields are flattened — write them at the top level of `[planning]`, not in sub-tables.

| key | default | effect |
|-----|---------|--------|
| `enabled` | `true` | Orchestrator master switch. `false` → single-agent `PlanGenerator` path always. |
| `orchestrator_agent` | `"coordinator"` | Agent name for AI decomposition calls. |
| `orchestrator_peer` | `"coordinator"` | Peer alias for AI decomposition calls. |
| `complexity_threshold` | `0.6` | Min `complexity_score` to activate orchestrator. |
| `max_parallel_specialists` | `4` | Hard cap on concurrent specialist sub-plan tasks. |
| `critic_enabled` | `true` | Critic loop master switch. |
| `critic_agent` | `"coordinator"` | Agent name for critic review calls. |
| `critic_peer` | `"coordinator"` | Peer alias for critic review calls. |
| `max_critic_rounds` | `3` | Max adversarial review-revise iterations before returning best plan with warning. |
| `require_approval` | `false` | Gate every non-dry-run `planning.create_plan` on human approval. |
| `approval_timeout_secs` | `3600` | Pending plans older than this are auto-expired by the 60-second sweep task. |
| `approval_db_path` | _(none)_ | SQLite path for the approval store. When absent, the controller derives a path under `[coordinator] db_path`. |
| `approval_targets` | `[]` | `[[planning.approval_targets]]` channel fan-out rows (see below). |
| `verify_steps` | `false` | Enable step-level verification harness. |
| `verifier_agent` | `"coordinator"` | Agent name for AI-judge verification calls. |
| `verifier_peer` | `"coordinator"` | Peer alias for AI-judge calls. |
| `required_steps` | `[]` | Step IDs whose verification failures cancel the workflow and override status to `Failed`. |
| `regex_timeout_ms` | `100` | Wall-clock timeout per regex `is_match` call (ReDoS guard). |

### Approval targets

```toml
[[planning.approval_targets]]
channel = "slack"         # email | telegram | discord | slack
peer    = "coordinator"
slack_channel = "#ops"    # channel-specific field (see table below)
```

| channel | capability called | required fields |
|---------|------------------|-----------------|
| `email` | `email.send` | `to`, `subject` |
| `telegram` | `telegram.send` | `chat_id` |
| `discord` | `discord.send` | `channel_id` |
| `slack` | `slack.send` | `channel` (mapped from `slack_channel`) |

Notification dispatch is best-effort; failures land in tracing logs only and do not block plan creation.

---

## HTTP API

| method + path | purpose |
|---------------|---------|
| `POST /v1/planning/plan` | Create a plan; body `{spec, max_agents?, dry_run?}` |
| `GET  /v1/planning/agents` | List all agents known to the registry |
| `POST /v1/planning/agents/search` | Find matching agents; body `{task}` |
| `POST /v1/planning/validate` | Validate a spec string; body `{spec}` |
| `GET  /v1/planning/status` | Orchestrator status (enabled, threshold, registry size) |

Approval management (`planning.approve_plan` etc.) is not exposed over HTTP today; operators call those capabilities directly through the coordinator.

---

## Capability surface

Always registered:

| capability | description |
|------------|-------------|
| `planning.list_agents` | Return all agents + capabilities known to the registry. |
| `planning.find_agents` | Scored match for a task description; args `{task: string}`. |
| `planning.validate_spec` | Parse a spec and return the `PlanSpec` (no generation). Args `{spec: string}`. |
| `planning.create_plan` | Full five-stage pipeline. See args and response below. |
| `planning.orchestrator_status` | Read-only view: enabled flag, complexity threshold, registry agent count. |

Registered only when an `ApprovalStore` is wired (`approval_store = Some(...)`):

| capability | description |
|------------|-------------|
| `planning.approve_plan` | Execute a pending plan; verifies spec signature first. Args `{plan_id, note?}`. |
| `planning.reject_plan` | Reject a pending plan. Args `{plan_id, note?}`. |
| `planning.list_approvals` | List approval records; optional `{status: pending\|approved\|rejected\|expired}`. |
| `planning.get_approval` | Fetch one record. Args `{plan_id}`. |
| `planning.verification_log` | Fetch verification entries for a plan. Args `{plan_id}`. |
| `planning.export_spec` | Export `PlanSpec` as JSON or markdown. Args `{plan_id, format: "json"\|"markdown"}`. |

### `planning.create_plan` args

```json
{
  "spec":             "<natural-language specification>",
  "max_agents":       3,
  "dry_run":          false,
  "require_approval": false
}
```

`max_agents` is clamped to 1–16; default 3. `require_approval` overrides the global config for this call.

### `planning.create_plan` response

```json
{
  "plan_spec":                 { /* PlanSpec — see tamper-evidence section */ },
  "topology":                  "single | sequential | parallel",
  "workflow_name":             "planning__<slug>",
  "workflow_yaml":             "<generated YAML>",
  "agents_selected":           [ /* AgentInfo[] */ ],
  "orchestrator_activated":    false,
  "specialist_count":          0,
  "critic_rounds":             2,
  "critic_approved":           true,
  "orchestrator":              { /* OrchestratorSummary */ },
  "critic":                    { /* CriticSummary */ },
  "conflict_resolution_report": { /* present only when conflicts > 0 */ },
  "execution":                 { /* present on non-dry-run, non-approval execution */ },
  "approval":                  { /* present when gated on approval */ },
  "verification":              { /* present when verify_steps enabled + executed */ }
}
```

---

## SQLite schemas

**`plan_approvals`** — approval records (migration versions 1–3):

```sql
CREATE TABLE plan_approvals (
    id                TEXT PRIMARY KEY,  -- spec_id (uuid v4)
    spec_json         TEXT NOT NULL,     -- full PlanSpec JSON (including signature)
    workflow_yaml     TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending',  -- pending|approved|rejected|expired
    created_at_ms     INTEGER NOT NULL,
    decided_at_ms     INTEGER,
    decision_note     TEXT,
    orchestrator_meta TEXT,              -- JSON; NULL when orchestrator skipped
    critic_meta       TEXT,             -- JSON; NULL when critic skipped
    tenant_id         TEXT NOT NULL DEFAULT 'default'
);
CREATE INDEX plan_approvals_status_idx ON plan_approvals(status);
```

**`plan_verifications`** — step-level verification entries (migration v2):

```sql
CREATE TABLE plan_verifications (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    plan_id         TEXT NOT NULL,
    step_id         TEXT NOT NULL,
    criterion       TEXT NOT NULL,
    strategy_used   TEXT NOT NULL,  -- length_check|keyword_presence|keyword_absence|pattern_match|ai_judge
    passed          INTEGER NOT NULL,
    reason          TEXT NOT NULL,
    verified_at_ms  INTEGER NOT NULL,
    tenant_id       TEXT NOT NULL DEFAULT 'default'
);
CREATE INDEX plan_verifications_plan_id_idx ON plan_verifications(plan_id);
```

Both tables gain `tenant_id` via migration v3; legacy rows default to `'default'`. `insert_pending_for_tenant` / `get_for_tenant` / `insert_verification_for_tenant` / `count_verifications_for_tenant` scope reads and writes to the verified caller tenant.

---

## File paths

| path | purpose |
|------|---------|
| `<data_dir>/workflows/<name>.workflow` | Generated workflow files |
| `<data_dir>/workflows.sqlite` | Workflow chronicle (`default_chronicle_path`) |
| `[planning] approval_db_path` or derived under `[coordinator] db_path` | Approval + verification store |
