# Proposal — Agent Employee Permission Model

**Status:** draft for review · author: continuation Claude · 2026-05-21
**Track:** core identity / authorization (precedes Wave 3 replay-debug push)
**Scope:** design pass only — no code lands until this is signed off.

---

## 0. Framing

Today Relix's identity system treats every agent as a generic
"verified caller with some groups". Policy decides allow/deny per
method, an audit log records what happened. That gets us
correct-by-construction admission, but it does not give us the
mental model of "this agent is a specific employee with a job, a
boundary, and a supervisor". Operators thinking in terms of
"the research agent" or "the filing assistant" have to translate
into ad-hoc group names every time.

This proposal commits Relix to the **agent-employee** posture:

- Every agent is a first-class record with identity, role, scope,
  and lifecycle.
- Permissions are categorical (browser, payments, fs:write,
  external:egress) rather than method-by-method.
- Some actions require human approval before they happen, and
  the system is honest about the wait.
- Every decision — allow, deny, approval-required, approved,
  rejected — is chronicle-visible.
- The dashboard surfaces an **agent profile** page that shows
  permissions, recent actions, denied actions, pending approvals,
  and audit trail for that specific agent.

### Non-goals (explicit)

- **Not a replacement** for the existing PolicyEngine. Categorical
  permissions compile *down to* PolicyEngine rules; the engine
  stays the source of truth at the dispatch layer.
- **Not a multi-tenancy / multi-org effort**. Org boundaries
  remain as they are (single org-root key today).
- **Not a sandbox**. Permission denial is policy-level, not
  process-level. A misbehaving handler can still reach the host
  if the policy admits the call.
- **Not OAuth / OIDC.** Agent identities are Relix-internal,
  minted via `relix-cli identity mint`. Federation is a separate
  Wave.

---

## 1. What exists today (the substrate)

The agent-employee model is **additive over** these existing
pieces. Calling them out so the proposal grounds in real code,
not on a green field.

### 1.1 Identity bundle (`relix_core::identity::IdentityBundle`)

```rust
pub struct IdentityBundle {
    pub subject_id: NodeId,         // pubkey hash
    pub name: String,                // human name within the org
    pub org_id: NodeId,              // issuing org root
    pub groups: Vec<String>,         // policy match target
    pub role: String,                // "agent" / "human" / "admin" / "service"
    pub clearance: String,           // "public" / "internal" / "restricted" / "confidential"
    pub supervisors: Vec<String>,    // reserved (unused today)
}
```

`relix-cli identity mint --name alice --groups chat-users,operators`
already creates these. They're cryptographically signed by the
org root key.

### 1.2 Verified identity (`VerifiedIdentity`)

Construction-private: only `validate_identity_bundle` produces
one. Downstream code (policy, dispatch, audit) consumes this
rather than the raw bundle, so the type system enforces that
nothing skips verification.

### 1.3 Policy engine (`relix_core::policy::PolicyEngine`)

Two-stage admission:
1. `[admit] groups = [...]` — node-level admission filter.
2. `[[rules]]` per-method — first matching rule wins, default deny.

Each `Decision` is `Allow { matched_rule }` / `Deny { reason,
matched_rule }`. **`RequireApproval` is reserved in the source
comment as a Gate-2 item** but not implemented.

### 1.4 Capability descriptor metadata

Already present on every advertised capability:

- `sensitivity_tags: Vec<String>` — `["external:network",
  "browser:session", "fs:write", "binary:image", ...]`
- `categories: Vec<String>` — `["browser", "mutate", "fetch",
  "persist", "notify", ...]`
- `risk_level: RiskLevel` — `Safe / Low / Medium / High /
  Critical` (with `Unknown` as the unaudited-gap default)
- `environment_requirements: Vec<String>` — `["network:outbound",
  "filesystem:read", "api_key:openai", ...]`
- `requires_groups: Vec<String>` — structural pre-filter

These are the **building blocks** the agent permission model
references. We don't need to invent new descriptor fields; we
need to *consume* the existing ones from the agent side.

### 1.5 Audit trail

- `DispatchBridge` writes an audit log entry on every dispatch
  outcome (allowed / denied / unknown method / handler-err).
- W2-007d denial ring (256 entries per peer) is the live
  operator view of recent denies.
- W2-006 per-capability counters give long-horizon stats.

### 1.6 Task status — `awaiting_input` already exists

The Coordinator has an `awaiting_input` task status with
transitions to/from `running` / `cancelled` / `frozen`. This is
the natural state for "task is paused waiting on a human
approval". **No new task status needed** for the approval
flow's caller-side blocking.

---

## 2. Agent record

Today the IdentityBundle is the closest thing to an "agent
record". It captures identity + groups but not the job. The
agent-employee model treats the AIC bundle as the **credential**
and introduces a separate **agent profile** record that lives
alongside it.

### 2.1 The record

Proposed shape (TOML + signed JSON envelope, same posture as
identity bundles):

```toml
agent_id      = "agt_research_assistant_01"   # stable id, ${role}_${seq}
name          = "Research Assistant"           # display name
title         = "Junior research analyst"      # human-readable role
department    = "research"                     # free-form group
team          = "research-ops"                 # finer-grained group
created_by    = "alice@org"                    # subject_id or human name
created_at    = 1716000000
status        = "active"                       # "active" | "suspended" | "disabled"

# Binding to the credential layer:
subject_id    = "..."                          # the IdentityBundle's subject_id
bundle_id     = "..."                          # the cryptographic AIC bundle id

# Permission scope — compiles down to PolicyEngine input.
[permissions]
allow_categories       = ["browser", "fetch", "summarise"]
deny_categories        = ["payments", "production_deploy"]
allow_sensitivity_tags = ["external:network", "browser:session"]
deny_sensitivity_tags  = ["fs:write:host", "credentials:read"]
max_risk_level         = "medium"   # Safe / Low / Medium / High / Critical
allowed_surfaces       = ["telegram", "openwebui", "scheduler"]

# Per-action approval requirements.
[[approvals]]
match_category  = "browser"
match_method    = "tool.browser.click"
when            = "always"                     # "always" | "first_per_session" | "off"
approver_groups = ["ops", "admin"]
```

The record is signed by an org-admin key (same trust root as
AICs). Agents cannot edit their own record. **Auditors can
hash-diff records over time** because they're signed envelopes.

### 2.2 Status lifecycle

| Status | Behavior at dispatch | Notes |
| --- | --- | --- |
| `active` | Permissions apply normally. | Default after creation. |
| `suspended` | All non-`Safe` capabilities denied with `agent_suspended`. Read-only capabilities still work. | Quick-pause without revoking the AIC. |
| `disabled` | All capabilities denied with `agent_disabled`, including reads. | Equivalent to a soft revocation; the AIC bundle remains valid (so audit log signatures remain checkable), but the runtime refuses to act on its calls. |

Status changes write a `agent.status_changed` chronicle event so
revocations leave a paper trail.

### 2.3 Storage

Two honest options:

**Option A.** Agent records live on the coordinator (file or
SQLite), alongside the existing identity-mint output. Bridge +
nodes look up records via a new capability `agent.lookup(subject_id)`.

**Option B.** Each node ships a `[agents]` section in its config,
mirroring the policy file. Operators edit + restart to update.

Option B is the alpha-honest path (no new persistence, no new
node-type) and matches how policy works today. Option A is
where this lands when we want runtime agent management without
restarts. **Default for V1: Option B.** Migrate to Option A in a
later phase if operators ask.

---

## 3. Permissions model

Permissions are **categorical**, not method-by-method. The agent
profile lists what *kinds* of things it can do; the runtime
compiles those into per-method allows.

### 3.1 Five dimensions

| Dimension | What it controls | Source field on capability |
| --- | --- | --- |
| **Category** | What kind of work (`browser`, `fetch`, `payments`) | `categories: Vec<String>` |
| **Sensitivity tag** | What kind of resource the call touches | `sensitivity_tags: Vec<String>` |
| **Risk level ceiling** | Max risk class allowed | `risk_level: RiskLevel` |
| **Surface** | Where the call originated from | new request envelope field |
| **Data clearance** | Min clearance level needed | `clearance` on identity (today) |

### 3.2 Evaluation order

When agent X attempts to invoke capability Y:

1. **Status check.** If `status != "active"`, deny.
2. **Surface check.** If the request envelope's surface field
   isn't in `allowed_surfaces`, deny.
3. **Risk ceiling.** If `Y.risk_level > X.max_risk_level`, deny.
4. **Deny lists.** If Y's category is in `deny_categories` *or*
   any of Y's sensitivity_tags overlaps `deny_sensitivity_tags`,
   deny.
5. **Allow lists.** Y must satisfy *both*:
   - At least one of Y's categories is in `allow_categories`
     (or `allow_categories` is empty = "all").
   - All of Y's sensitivity_tags are in `allow_sensitivity_tags`
     (or `allow_sensitivity_tags` is empty = "all").
6. **Approval check.** See §5.
7. **Underlying PolicyEngine.** Today's per-method rules still
   run last. Categorical allow doesn't override a per-method
   `Deny` rule.

The categorical layer is **additive narrowing**. PolicyEngine
remains the floor: if policy denies, agent permissions can't
override.

### 3.3 Why categorical over per-method

Three concrete reasons:
- **New capabilities inherit policy.** When a new
  `tool.browser.scroll_into_view` ships with `categories =
  ["browser"]`, every agent with `allow_categories = ["browser"]`
  gets it automatically — no per-agent edits.
- **Honest operator framing.** "The research agent may browse"
  is easier to reason about than 14 individual `allow_groups`
  entries.
- **Method-level rules remain available** in the underlying
  policy engine for exceptions ("this specific agent may NOT
  call `tool.browser.execute_js` even though it's in
  `browser`").

### 3.4 Surface

A `surface` field is added to the request envelope (set by the
bridge or whoever marshals the call). Values: `dashboard-cli`,
`telegram`, `openwebui-chat`, `scheduler`, `api-direct`, `cli`,
`internal-flow`. Agents can be locked to one or more surfaces —
e.g. a notification agent may only act from `scheduler`, never
from `openwebui-chat`.

**Honesty note**: surface is *operator-asserted*, not
cryptographically proven. A compromised bridge could fake the
surface. We don't pretend otherwise. Surface gates raise the
cost of misuse from "spoof a bundle" to "spoof a bundle *and*
forge the surface header" — defense in depth, not a hard wall.

---

## 4. Canonical roles

Roles are **bundles of permissions**, not a separate concept.
Each canonical role is shipped as a `roles/<name>.toml` template
operators copy + edit per agent.

| Role | `allow_categories` | `max_risk_level` | Default approvals |
| --- | --- | --- | --- |
| **Research Agent** | `fetch, parse, summarise, browser` | `medium` | `browser.click` on `.*payment.*` selectors |
| **Browser Agent** | `browser, fetch` | `medium` | `browser.form_submit` always |
| **Filing Assistant** | `fs, fetch, summarise, persist` | `medium` | `fs.write` outside `~/inbox/` |
| **Admin Agent** | `*` | `high` | `payments`, `production_deploy`, `credentials:read` |
| **Read-only Analyst** | `read, fetch, summarise` | `safe` | none (everything's read-only) |
| **High-trust Operator** | `*` | `critical` | none (used for break-glass) |

The roles ship as **starter templates**, not enums. Operators
fork + edit. The canonical names are advisory.

---

## 5. Approval flows

This is the architecturally novel piece. Three patterns, each
with concrete tradeoffs.

### 5.1 Pattern A — synchronous block (rejected)

Handler returns `Decision::RequireApproval`; the original caller
blocks on a `(deadline)` future. Operator approves → handler
proceeds. **Rejected** because: blocks Coordinator threads,
operator UX is brittle (the request times out the moment the
operator goes to lunch), no replay trail of "what would have
happened".

### 5.2 Pattern B — pause-and-resume (recommended)

The flow when an agent attempts an approval-required action:

1. Caller's task moves to status `awaiting_input` (already
   exists in the Coordinator).
2. Coordinator emits a `task.approval_requested` chronicle event
   with payload: `{agent_id, method, args_redacted_hash, reason,
   approver_groups, requested_at}`.
3. A new capability `agent.approval_pending` (per-tool-node ring,
   capacity 256) makes pending approvals visible to operators.
4. Operator sees the pending approval on the dashboard's new
   **Approvals** page, clicks Approve / Reject with an optional
   note.
5. Dashboard calls a new `POST /v1/approvals/:id/decide` endpoint;
   the responder writes a `task.approval_decided` event and the
   Coordinator un-pauses the task.
6. On approve, the original capability call retries through
   normal dispatch with an `approval_token` attached to the
   request envelope; the policy engine sees the token and admits
   the call.
7. On reject, the task moves to `failed` with
   `failure_class = "approval_rejected"`.

This pattern reuses existing infrastructure:
- `awaiting_input` task status — already there
- Chronicle events — already there
- DispatchBridge audit — already there
- Bounded rings — already there (fs audit, term audit, denial
  ring patterns)

What's actually new:
- `task.approval_requested` / `task.approval_decided` chronicle
  events
- An `approval_token` field on the request envelope
- A pending-approvals ring on the tool node (`tool.approval.pending`)
- Bridge proxy `/v1/approvals` (list) + `/v1/approvals/:id/decide`
- Dashboard Approvals page

**Open question.** Where does the approval ring live? Tool node
is wrong — approvals span multiple node types. **Coordinator** is
probably right, since the Coordinator is already the task-state
authority. The pending-approvals ring becomes a coordinator
capability: `coord.approval.pending`.

### 5.3 Pattern C — standing approval (complementary)

Some approvals are **categorical and time-bounded**, not
per-action: "Filing Assistant may write to `~/inbox/` until
2026-06-01". These are operator-configured ahead of time and
just check at dispatch (no pause).

Concretely:
```toml
[[standing_approvals]]
agent_id        = "agt_filing_assistant_01"
match_category  = "fs"
match_path_glob = "~/inbox/**"
expires_at      = 1717200000
granted_by      = "alice@org"
note            = "Monthly receipts processing window"
```

Standing approvals **reduce noise**: not every fs.write needs to
ping an operator. Pattern B handles the exceptions.

### 5.4 Default approval-required list

Out-of-the-box, these capability categories require approval:

| Category / tag | Default policy |
| --- | --- |
| `payments` | Always |
| `production_deploy` | Always |
| `credentials:read` | Always |
| `email:send` / `notify:human` | Always (configurable) |
| `external_api:write` | First-per-session (configurable) |
| `fs:write:host` | First-per-session for non-allowlisted paths |
| `browser.form_submit` | First-per-session |

Operators can override per-agent in the agent record's
`[[approvals]]` block.

---

## 6. Runtime enforcement

Where this slots into the existing dispatch admission pipeline:

```
Step 1: decode envelope                    (existing)
Step 2: validate identity bundle           (existing)
Step 3: deadline check                     (existing)
Step 4: lookup agent record by subject_id  (NEW)
Step 5: agent status check                 (NEW)
Step 6: surface check                      (NEW)
Step 7: risk-ceiling check                 (NEW)
Step 8: deny-list check                    (NEW)
Step 9: allow-list check                   (NEW)
Step 10: approval check                    (NEW — may pause)
Step 11: standing-approval check           (NEW — may admit)
Step 12: PolicyEngine.evaluate             (existing, unchanged)
Step 13: dispatch handler                  (existing)
Step 14: audit                             (existing, enriched)
```

Steps 4–11 collectively form a new module:
`relix_runtime::admission::agent_gate`. Inserted between the
identity-validate step and the existing PolicyEngine call. **The
PolicyEngine is left intact** — categorical checks are layered
over it, not in place of it.

### 6.1 Cost

Steps 4–11 are pure in-memory lookups against an `Arc<HashMap>`
of agent records, plus a string-set intersection for the
category/sensitivity checks. Sub-microsecond. Approval check is
free in the common case (no approval required); the pause +
event-write only happens when one is.

### 6.2 What does NOT change

- The wire envelope's existing fields (deadline, trace_id,
  request_id, identity bundle). The `surface` and
  `approval_token` fields are *additive*.
- The PolicyEngine TOML format. Operators with no agent records
  configured see today's exact behavior.
- The audit log shape (we enrich, not replace).

---

## 7. Dashboard surface

A new top-level page: `#/agents`.

### 7.1 Agent list

Table: `agent_id · name · role · status · last_active · pending_approvals`.
Status pill (green/amber/red), `pending_approvals` count clickable
to filtered approvals page.

### 7.2 Agent profile page (`#/agents/:agent_id`)

Five sections:

1. **Identity** — display name, role/title, department/team,
   created_by, created_at, status, subject_id, bundle_id (last
   8 chars + copy-full).
2. **Permission scope** — categories (allow/deny lists with the
   actual capabilities each resolves to, computed from the
   manifest), sensitivity tags, max risk level, allowed surfaces.
3. **Recent actions** — last 50 calls from the dispatch audit
   ring, filterable to this `subject_id`. Each row: timestamp,
   method, outcome (`allow` / `deny` / `approved` / `rejected`),
   matched_rule.
4. **Pending approvals** — what this agent is currently waiting
   on; click to approve/reject inline.
5. **Audit trail** — read-only chronicle of agent-level events:
   status changes, role changes, standing-approval grants, etc.

### 7.3 New "Approvals" page (`#/approvals`)

Operator-centric view of every pending approval across all
agents. Default sort: oldest first (don't keep agents waiting).
Each row: requested_at, agent name, method, args-redacted summary,
reason, approve/reject buttons.

### 7.4 What we deliberately don't build (V1)

- Drag-to-edit permission UI. Permissions edit through the agent
  TOML; the dashboard only shows + decides approvals.
- Real-time WebSocket push of new approval requests. Polling at
  3s is enough; SSE later if operators ask.
- Bulk approve. Single-action approve is intentional friction.

---

## 8. Audit log shape

Each permission decision writes a row with these fields:

```jsonc
{
  "ts": 1716000000,
  "trace_id": "...",
  "request_id": "...",
  "agent_id": "agt_research_assistant_01",
  "subject_id": "...",
  "method": "tool.web_fetch",
  "decision": "allow",                  // allow | deny | approval_required | approved | rejected
  "phase": "agent_gate",                // identity | agent_gate | policy | handler
  "matched_rule": "browser_category",    // which rule / clause matched
  "reason": "category=browser in allow_categories",
  "approval_id": null,                  // present on approval_required / approved / rejected
  "approver": null,                     // subject_id of the approver
  "args_redacted_hash": "...",          // BLAKE3 of args; lets ops correlate without storing args
  "duration_ms": 0                      // for handler-completed rows only
}
```

`args_redacted_hash` lets operators correlate a denied call with
later replays of the same call without ever persisting raw args
(which may contain secrets). The hash is salted with `request_id`
so it's not a precomputable rainbow lookup.

**Honest scope note.** The existing audit log is per-node + signed
chronologically. Agent-gate decisions write to the same log,
enriched with the new fields above. We do not introduce a second
audit store.

---

## 9. Build order

Five phases, each independently shippable. Each phase ends with
operator-visible value and the next phase still works if we
pause.

### Phase 1 — Agent record + surface gating (small)

1. Define the agent record TOML schema in
   `relix-core::agent` (new module).
2. Add a `[[agents]]` array to the node config file (Option B
   from §2.3).
3. Plumb the agent record lookup into a new
   `relix_runtime::admission::agent_gate` module.
4. Add the `surface` field to the request envelope (additive
   wire change; bridge sets it).
5. Implement status check + surface check + risk-ceiling check
   (the read-only steps 4–7 from §6).
6. New chronicle events: `agent.lookup_failed`,
   `agent.suspended_call_denied`, `agent.surface_denied`,
   `agent.risk_ceiling_denied`.

**No approval flow yet.** Just observation + categorical
narrowing. Operators see the model working before we commit to
the approval-flow complexity.

### Phase 2 — Categorical permissions (small)

7. Implement deny-list + allow-list checks (steps 8–9).
8. `relix-cli capability list-for-agent <agent_id>` — given an
   agent record + manifest, compute the effective allowed
   method set.
9. Audit log enrichment: every decision now carries `agent_id`
   + `phase`.

### Phase 3 — Dashboard agent surface (small)

10. New `#/agents` list page (read-only, sourced from a new
    `/v1/agents` bridge proxy).
11. New `#/agents/:agent_id` profile (sections 1–3 from §7.2 —
    identity, scope, recent actions). Sections 4–5 land in
    Phase 4.

### Phase 4 — Approval flow (medium)

12. New `Decision::RequireApproval` variant on PolicyEngine
    (currently reserved).
13. `task.approval_requested` + `task.approval_decided`
    chronicle events.
14. `coord.approval.pending` capability (ring, capacity 256).
15. Bridge: `/v1/approvals` (list) + `/v1/approvals/:id/decide`
    (POST).
16. Dashboard: `#/approvals` page + agent profile sections 4–5.
17. The `approval_token` envelope field + admission step 10.

### Phase 5 — Standing approvals (small)

18. `[[standing_approvals]]` config block + admission step 11.
19. `relix-cli ops standing-approvals` (list / grant / revoke).
20. Dashboard surface for active standing approvals (chip on
    each agent profile).

### Explicit deferrals (Wave 3+)

- Agent storage on the Coordinator (Option A from §2.3).
- Federated / OAuth identity.
- Drag-to-edit permission UI.
- Multi-org boundaries.
- Per-resource (not just per-capability) permissions ("can read
  files under `/inbox` but not `/secrets`"). The
  `match_path_glob` field on standing approvals hints at this,
  but full per-resource ABAC is a separate effort.

---

## 10. Open questions

These need real conversation before code lands.

1. **Agent record storage — Option A vs B.** Recommend B for
   V1; the migration to A is bounded but requires a new node-
   type or coordinator-side persistence. Are you comfortable
   with the operator-edit-and-restart loop initially?

2. **`approval_token` lifetime.** Once an operator approves a
   request, how long does the resulting token live? Options:
   one-shot (consumed on first dispatch — safest), bounded
   (60s — friendlier to slow handlers), session-bound (until
   the task moves out of `awaiting_input`). I lean **one-shot
   with a 5-minute grace** for retry semantics.

3. **Categorical override of policy deny.** Today policy is the
   floor. Should the agent record ever be able to *grant* a
   capability that policy denies? My instinct is no — policy
   floor is a feature, not a bug — but operators may want
   "this admin agent can call anything" without editing the
   policy file for every method.

4. **Approval expiry.** What happens to a pending approval that
   no one decides on for a week? Auto-reject? Auto-allow if the
   agent has standing approval for the category? My default:
   auto-reject after `pending_max_secs = 86400` (24h), with the
   timeout reason `approval_timeout`.

5. **Audit-log secrecy.** The `args_redacted_hash` proposal
   stores no args, which is good. But the audit log entry's
   `matched_rule` + `reason` fields may leak operational
   intent (e.g. "denied because category=payments"). That's
   probably fine for an internal audit log; flagging in case
   it's not.

6. **Bundle vs. profile drift.** The AIC bundle has groups
   embedded in it (the credential). The agent record has
   categorical permissions (the profile). They can drift —
   an agent's profile can grow new permissions without
   re-issuing the AIC. Is that desirable (operator flexibility)
   or dangerous (the bundle no longer faithfully describes the
   subject)? Recommend: the AIC remains the floor (caller must
   still hold matching groups), the agent record narrows.
   Drift in the *narrowing* direction is fine; drift in the
   *broadening* direction is impossible.

7. **Surface field provenance.** As §3.4 admits, surface is
   operator-asserted. Should we sign it (the bridge / origin
   node signs the surface tag with its node key, audit can
   verify)? Adds wire complexity. Defer to Phase 5+.

8. **Approval UX on the dashboard.** Today the dashboard is
   loopback-only and assumes the operator is the developer.
   Real ops use needs an audit-grade approval UI with rate
   limiting, reasoning fields, and possibly multi-party
   approve-2-of-3. We probably ship single-approver with a
   reason field in Phase 4, multi-party in Phase 5+.

---

## 11. Honest deferrals & risks

- **Phase 4 (approvals) is genuinely the hard one.** Phases 1–3
  are mostly read-side plumbing of existing data. Phase 4
  introduces *task pausing on synchronous-feeling capability
  calls*, which is novel for the runtime. The implementation is
  bounded but the testing surface is wide.

- **Backward compat.** Existing deployments with no agent
  records configured must behave **identically** to today.
  Every new check has an "empty-list-means-permit" branch. We
  ship the model, then operators opt into agent records when
  they're ready.

- **Operator burden.** A real agent-employee model requires
  operators to maintain the records. We mitigate via canonical
  role templates (§4) but can't eliminate the work. The
  alternative is the current "everyone is in `chat-users`" mess,
  which doesn't scale past the demo.

- **Discoverability.** Agents themselves should not see the
  full list of capabilities they *don't* have access to —
  surfaces only the allowed set. That's both a manifest-cache
  filter and a UX choice (the agent's view in OpenWebUI, say,
  shouldn't list disallowed methods). Touch lightly in Phase 3.

- **The "honesty contract" remains.** When a call is denied,
  it returns a typed error envelope describing exactly why.
  When a call is approval-required, the caller (a SOL flow,
  the chat shim, etc.) sees a clear `awaiting_approval`
  status, not a silent stall or a fake success.

---

## 12. Decision points to greenlight before coding

In rough priority order:

1. **Approve the §0 framing** — agent-employee is the model;
   policy engine remains the floor.
2. **Approve §2.3 storage Option B** for V1 (TOML in node
   config, operator edit + restart).
3. **Approve §3 evaluation order** — categorical gates between
   identity validation and PolicyEngine.
4. **Approve §5.2 Pattern B** as the approval flow (reuse
   `awaiting_input` task status; approval ring on the
   coordinator).
5. **Approve §9 build order** — five phases, Phase 4 is the
   only one with novel runtime mechanism.
6. **Settle the open questions in §10** in order; #1, #3,
   and #4 block Phase 1.

Once these six points are signed off, Phase 1 is a 1–2
session implementation. The full track lands in
approximately 5 sessions if all five phases are scoped
green.
