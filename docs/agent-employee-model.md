# Agent employee model — operator handbook

_Version: 0.4.1_

Relix runs every inbound mesh call through an **agent gate**
before the policy engine fires. The gate looks up an
**agent profile** keyed by the caller's `subject_id` and
enforces a five-step admission contract per profile:

1. **Status** — `active` proceeds; `suspended` / `disabled` deny.
2. **Surface** — when the profile has a `surface_allowlist`,
   the transport-layer-derived `caller_surface` (peer alias, not the
   envelope's `surface` field — which is operator-asserted and untrusted)
   must match.
3. **Risk ceiling** — the called capability's `risk_level`
   must fit under the agent's `risk_ceiling`.
4. **Categorical permissions** — capability categories +
   sensitivity tags compared against the profile's deny /
   allow lists.
5. **Approval flow** — if the call hits an
   `approval_required` category and no active standing
   approval matches, the gate pauses the calling task and
   creates a pending approval row for the operator.

Callers without an agent profile are **denied** with
`AGENT_NO_PROFILE` (fail-closed). Callers whose agent store is not
wired are denied with `AGENT_STORE_NOT_CONFIGURED`. The policy engine
still runs after the gate Allows; categorical permissions only ever
**narrow** what policy already permits.

The shipped implementation lives across four files:

- `crates/relix-runtime/src/admission/agent_gate.rs` — gate
  primitives + evaluation logic.
- `crates/relix-runtime/src/nodes/coordinator/agent/store.rs` —
  SQLite store + CRUD for profiles, approval requests, and
  standing approvals.
- `crates/relix-runtime/src/nodes/coordinator/agent/handlers.rs` —
  `agent.*` and `coord.approval.*` capabilities the bridge
  proxies onto.
- `crates/relix-web-bridge/src/agent.rs` — HTTP routes
  documented below.

The deep-dive design + per-deny-reason vocabulary lives in
[`agent-permissions.md`](./agent-permissions.md). This file is
the operator handbook: how to wire one up.

---

## 1. Create an agent profile

`POST /v1/agents`:

```jsonc
{
  "name":          "research-assistant",
  "role":          "agent",
  "title":         "Junior research analyst",
  "department":    "research",
  "team":          "research-ops",
  "created_by":    "alice",
  "subject_id":    "<hex from `relix-cli identity mint`>",
  "risk_ceiling":  "medium"   // safe | low | medium | high | critical
}
```

Every field except `name` and `subject_id` is optional. The
`subject_id` MUST match the AIC bundle the agent presents on
the wire — that's the gate's lookup key.

Mint an AIC first (this is the standard relix-cli flow,
unchanged):

```bash
relix-cli identity mint \
    --name research-assistant \
    --groups chat-users \
    --root-key dev-keys/org-root.key \
    --out research-assistant.aic
relix-cli identity show research-assistant.aic
# subject_id: a3f0…
```

Then POST the agent record with that `subject_id`. The same
bundle is used by the agent at every dispatch; the gate
matches the bundle's subject_id back to the profile row.

---

## 2. Configure permissions

`PATCH /v1/agents/:agent_id`:

```jsonc
{
  "field": "allow_categories",
  "value": ["browser", "fetch", "summarise"]
}
```

`PATCH` updates one field per request. Supported fields:

| Field | Wire type | Notes |
| --- | --- | --- |
| `status` | `"active"` / `"suspended"` / `"disabled"` | Suspended → all non-safe deny. Disabled → all deny. |
| `role`, `title`, `department`, `team` | string | Display metadata. Free-form. |
| `risk_ceiling` | string | `safe` / `low` / `medium` / `high` / `critical` |
| `surface_allowlist` | comma-separated string | `"telegram,scheduler"` — empty = all surfaces. |
| `allow_categories` | comma-separated string | Capability categories the agent may call. Empty = unrestricted (subject to deny + ceiling + policy). |
| `deny_categories` | comma-separated string | Always denied. Wins over allow. |
| `allow_sensitivity_tags` | comma-separated string | Tags the call's `sensitivity_tags` must all match. |
| `deny_sensitivity_tags` | comma-separated string | Any overlap denies. |
| `approval_required_categories` | comma-separated string | Categories that need operator approval before each call. |
| `approval_timeout_secs` | integer string | How long a pending approval lives before auto-expire. Default 86400. |

Sensible defaults ship for `approval_required_categories`:
`payments,production_deploy,credentials:read,email:send,external_api:write,browser.form_submit`.
Operators can clear or extend it via `PATCH`.

The bridge serialises arrays as comma-separated strings on
the wire. Empty string clears the list.

`DELETE /v1/agents/:agent_id` is a **soft delete** — flips
`status` to `disabled` so the audit chain stays intact.

---

## 3. Evaluation order in full

The gate evaluates in the order below. The first match wins;
subsequent steps don't run.

```
1. status check
     suspended → AGENT_SUSPENDED deny
     disabled  → AGENT_DISABLED  deny
2. surface check
     surface_allowlist non-empty AND caller_surface (transport-derived
     peer alias; NOT envelope.surface, which is untrusted) not in list
       → AGENT_SURFACE_DENIED deny
3. deny lists
     any capability category in deny_categories
       → AGENT_CATEGORY_DENIED deny
     any capability sensitivity_tag in deny_sensitivity_tags
       → AGENT_SENSITIVITY_DENIED deny
4. risk ceiling
     capability.risk_level > agent.risk_ceiling
       → AGENT_RISK_CEILING_EXCEEDED deny
5. allow lists
     allow_categories non-empty AND no category match
       → AGENT_CATEGORY_NOT_ALLOWED deny
     allow_sensitivity_tags non-empty AND any tag missing
       → AGENT_SENSITIVITY_NOT_ALLOWED deny
6. approval check
     capability category in approval_required_categories
       → standing-approval fast path:
            active matching standing → Allow(matched_rule="standing_approval:<id>")
       → else: RequireApproval(...) — pauses the task
7. approval-token check
     envelope.approval_token set
       → token must be approved + unconsumed + matches
         agent+method on file
         pass: consume token, Allow
         fail: APPROVAL_TOKEN_INVALID deny
8. Allow → PolicyEngine.evaluate runs next (existing
   admission step).
```

Deny lists run before risk ceiling so an operator's explicit
"no" wins over a generous ceiling. Allow lists run after
ceiling so an unbounded allow list still respects the ceiling.

---

## 4. Approvals

When the gate returns `RequireApproval`, three things happen:

1. The bridge's per-controller `on_require_approval` hook
   creates a row in the `approval_requests` table with a
   pending status, the args-redacted hash, the approver
   groups, and (when the caller threaded one) the task_id.
2. The coordinator flips that task to `awaiting_input`.
3. The caller's HTTP response is a structured
   `APPROVAL_REQUIRED` error carrying the new `approval_id`.
   The task does not fail. It parks.

The operator decides via:

```bash
# Approve
curl -X POST localhost:9100/v1/approvals/<approval_id>/decide \
    -H 'authorization: Bearer <bridge-token>' \
    -H 'content-type: application/json' \
    -d '{"decision":"approved","reason":"verified manually"}'

# Reject
curl -X POST localhost:9100/v1/approvals/<approval_id>/decide \
    -H 'authorization: Bearer <bridge-token>' \
    -H 'content-type: application/json' \
    -d '{"decision":"rejected","reason":"out-of-scope"}'
```

The `approved` response carries a **one-shot approval token** — an
Ed25519-signed structured object (not a random hex string). See
[`approval-tokens.md`](approval-tokens.md) for the exact wire format.
The calling agent passes that token back on the retry via the
envelope's `approval_token` field; the gate verifies the signature,
checks expiry, confirms the method and subject match, then atomically
inserts a row in `approval_token_blocklist` (`INSERT OR IGNORE`) so
two concurrent replays cannot both succeed. A second use is denied with
`approval_token_consumed`.

A pending approval auto-expires after the agent's
`approval_timeout_secs` (default 24 h). The expiry loop (60 s)
flips status to `expired` and transitions linked tasks to `failed`
with `error_cause = "approval_timeout"`.

`GET /v1/approvals` lists pending approvals (newest first).
The dashboard page reads from this.

### Telegram bridge

When `[telegram] operator_chat_id` is set (non-zero), the
controller's approval-notifier loop posts every new pending
approval to the operator chat:

```
⏳ Approval required
Task: <task_id>
Agent: <agent name> (<agent_id short>)
Action: <capability>
Preview: <args_preview>
Reply /approve <approval_id> or /reject <approval_id> <reason>
```

Both commands are operator-only — the controller checks
`msg.chat_id == cfg.operator_chat_id` before invoking the
coord bridge. `/approve` mints the token and posts it back as
a reply; `/reject` fails the task with the supplied reason.

---

## 5. Standing approvals

Standing approvals let an operator pre-authorise an
agent + category pair for a bounded window. The gate's
approval check consults active standing approvals first; a
match admits the call without creating an approval request
and without notifying the operator. A standing approval can
also be bounded by call count (`max_calls`) and estimated spend
(`max_cost_micros`); both counters are consumed atomically by
the admission gate.

```bash
# Grant a 24-hour standing approval for payments
curl -X POST localhost:9100/v1/agents/<agent_id>/standing-approvals \
    -H 'authorization: Bearer <bridge-token>' \
    -H 'content-type: application/json' \
    -d '{"category":"payments","expires_at":1717200000,"max_calls":20,"max_cost_micros":200000}'
```

```bash
# List
curl localhost:9100/v1/agents/<agent_id>/standing-approvals

# Revoke
curl -X DELETE localhost:9100/v1/standing-approvals/<standing_id>
```

Standing approvals match by **category**, not method. An
operator who grants `match_category="browser"` admits every
capability whose `categories` include `browser` for that
agent. Expiry is operator-controlled; `expires_at` is unix
seconds. Cost budgeting is an admission-time estimate based on
the capability cost class, not a post-hoc provider invoice.
Revocation is a hard delete on the row; the next gate fire
falls through to the standard approval flow.

---

## 6. Dashboard

`#/agents` — agent list + detail + create form. The
right-hand detail panel shows status, full permission scope,
and edit fields that PATCH the bridge endpoints above. Real
data only — no placeholders.

`#/approvals` — pending approval queue. Oldest first; click
approve / reject inline. Operators see the same data the
bridge endpoint surfaces.

Both pages live in `crates/relix-web-bridge/src/dashboard.html`
under their `<section data-page="agents">` /
`<section data-page="approvals">` blocks. The sidebar entries
in the same file (under "Operate") route to them.

---

## 7. Reference

| Route | Method | Purpose |
| --- | --- | --- |
| `/v1/agents` | GET | List profiles |
| `/v1/agents` | POST | Create profile |
| `/v1/agents/:id` | GET | One profile |
| `/v1/agents/:id` | PATCH | Update one field |
| `/v1/agents/:id` | DELETE | Soft delete (→ disabled) |
| `/v1/approvals` | GET | Pending approvals |
| `/v1/approvals/:id/decide` | POST | Approve / reject |
| `/v1/agents/:id/standing-approvals` | GET | List standing |
| `/v1/agents/:id/standing-approvals` | POST | Grant standing |
| `/v1/standing-approvals/:id` | DELETE | Revoke standing |

Coordinator-side capabilities backing the routes:
`agent.create`, `agent.get`, `agent.list`, `agent.update`,
`agent.delete`, `agent.standing_approval.list / create /
revoke`, `coord.approval.pending`, `coord.approval.decide`.

Deep-dive design and per-deny-reason vocabulary:
[`agent-permissions.md`](./agent-permissions.md).
Proposal: [`proposals/agent-employee-permissions.md`](./proposals/agent-employee-permissions.md).
