# Agent employee permission model

_Version: 0.4.1_

Relix treats every agent as a first-class **employee record**: an
identity (the AIC bundle), a job (role / title / department /
team), a permission scope (categorical allow + deny lists, a risk
ceiling, a surface allowlist), and a lifecycle (active /
suspended / disabled). The runtime evaluates the agent's
permissions at every inbound call — before the policy engine
fires — and can pause an action until an operator approves it.

Design proposal: `docs/proposals/agent-employee-permissions.md`.
This file documents the shipped implementation.

## How it slots into the existing pipeline

```
1. decode envelope
2. validate identity bundle
3. capability lookup
4. AGENT GATE  ← new
5. PolicyEngine.evaluate  (unchanged)
6. dispatch handler
7. audit
```

The agent gate runs between identity verification and the
policy engine. It is **additive narrowing**: an Allow at the
gate still goes through PolicyEngine.evaluate afterward. The
policy file remains the floor — categorical permissions can
**never** widen what policy denies.

When a caller has **no agent profile** keyed by their
`subject_id`, the gate denies with `AGENT_NO_PROFILE` (fail-closed).
When the agent store is not wired at all, the gate denies with
`AGENT_STORE_NOT_CONFIGURED`. Neither is a no-op.

## The five-phase gate

The gate evaluates checks in this order. The first match wins:

1. **Status.** `suspended` → `agent_suspended` deny.
   `disabled` → `agent_disabled` deny. Anything other than
   `active` is denied.
2. **Surface.** When `surface_allowlist` is non-empty, the
   transport-layer-derived `caller_surface` (peer alias) must match one
   of the allowed surfaces. The envelope's `surface` field is ignored
   for admission — it is operator-asserted and untrusted.
   Empty allowlist = all surfaces.
3. **Risk ceiling.** Compares the called capability's
   `risk_level` against the agent's `risk_ceiling`. Order:
   `safe < low < medium < high < critical`. Above the
   ceiling → `agent_risk_ceiling_exceeded`.
4. **Deny lists.** The capability's `categories` or
   `sensitivity_tags` overlap with the agent's
   `deny_categories` or `deny_sensitivity_tags` → deny.
5. **Allow lists.** When non-empty, the capability must
   have at least one category in `allow_categories` and all
   of its sensitivity tags must be in
   `allow_sensitivity_tags`.
6. **Approval-required.** When the capability matches the
   agent's `approval_required_categories`, the gate either:
   - admits through a standing approval (Phase 5) if one is
     active for the matched category, or
   - returns `RequireApproval` and the bridge mints an
     approval row + chronicle event + Telegram notification.

The approval-token fast-path: when an inbound carries an
`approval_token`, the gate looks it up. If the token is
approved + unconsumed + the method matches + not expired,
the gate admits with `consumed_approval_id` set and the
bridge consumes the token (one-shot — second use fails).

## Creating an agent profile

### Dashboard

`#/agents` → fill in the create form → Create.

### CLI

```
relix-cli ops agent create \
  --name "Research Assistant" \
  --role research_assistant \
  --title "Junior research analyst" \
  --department research \
  --team research-ops \
  --created-by alice \
  --subject-id <64-char hex subject_id from `relix-cli identity mint`> \
  --risk-ceiling medium
```

### HTTP

```
POST /v1/agents
{
  "name": "Research Assistant",
  "role": "research_assistant",
  "title": "Junior research analyst",
  "department": "research",
  "team": "research-ops",
  "created_by": "alice",
  "subject_id": "<64-char hex>",
  "risk_ceiling": "medium"
}
```

### Wire capability

```
agent.create  arg: name|role|title|department|team|created_by|subject_id|risk_ceiling
```

## Updating an agent

`PATCH /v1/agents/:agent_id` with any subset of:

```json
{
  "status": "active" | "suspended" | "disabled",
  "role": "...",
  "title": "...",
  "department": "...",
  "team": "...",
  "risk_ceiling": "safe" | "low" | "medium" | "high" | "critical",
  "surface_allowlist": "telegram,openwebui,scheduler",
  "allow_categories": "browser,fetch,summarise",
  "deny_categories": "payments,production_deploy",
  "allow_sensitivity_tags": "external:network,browser:session",
  "deny_sensitivity_tags": "credentials:read,fs:write:host",
  "approval_required_categories": "payments,production_deploy,credentials:read,email:send,external_api:write,browser.form_submit",
  "approval_timeout_secs": 86400
}
```

Each provided field becomes one `agent.update` call against
the coordinator. The bridge applies them in the order
listed above.

List / array fields accept either JSON syntax (`["a","b"]`)
or comma-separated strings (`a,b,c`). The store normalises
to JSON.

## The approval flow

When a call matches the agent's
`approval_required_categories` AND no active standing
approval covers the category:

1. The gate returns `RequireApproval`. The bridge writes
   an `approval_requests` row with
   `status = pending`, `expires_at = now + approval_timeout_secs`
   (default 24 h), and a BLAKE3 hash of the method as the
   redaction handle.
2. The audit log records the denial with `kind =
   APPROVAL_REQUIRED`. The agent's calling code sees an
   error envelope whose `cause` contains the new
   `approval_id`.
3. If the coordinator's Telegram peer has
   `[telegram] operator_chat_id` configured, an
   approval-required notification fires to that chat.
4. The operator approves or rejects via the dashboard
   (`#/approvals`), CLI (`relix-cli ops agent approval-decide`),
   the `/approve <approval_id>` Telegram command, or
   `POST /v1/approvals/:id/decide`.
5. Approval mints a **one-shot approval token** — an Ed25519-signed
   structured object (see [`approval-tokens.md`](approval-tokens.md)
   for the wire format; it is not a random hex string).
   The agent retries the same call with `approval_token` on the
   envelope.
6. The gate admits the retried call and atomically records the token
   in `approval_token_blocklist` (`INSERT OR IGNORE`). Replay fails
   with `approval_token_consumed`.

### Auto-expire

A 60 s background loop on the coordinator scans
`approval_requests` for pending rows whose `expires_at <=
now`. Expired approvals flip to `expired` + the waiting
task (when present) moves to `failed` with
`failure_class = approval_timeout`.

### Default approval-required categories

```
payments
production_deploy
credentials:read
email:send
external_api:write
browser.form_submit
```

Operators override via `agent.update <agent_id>|approval_required_categories|...`.

## Standing approvals

Standing approvals are time-bounded categorical
pre-approvals: "the filing agent may write to `~/inbox/`
until 2026-06-01." Granted ahead of time, they bypass the
per-call approval request — the gate admits the call
straight through.

### CLI

```
relix-cli ops agent standing-approval-grant \
  --agent-id agt_filing_assistant_xxx \
  --category fs \
  --expires-in 30d \
  --note "Monthly receipts processing"
```

`--expires-in` accepts duration strings: `30m` / `2h` /
`1d` / `7d` / `4w`.

### HTTP

```
POST /v1/agents/:agent_id/standing-approvals
{
  "category": "fs",
  "expires_at": <unix_seconds>,
  "note": "Monthly receipts processing",
  "path_glob": "/inbox/**"          // optional
}
```

`path_glob` is reserved for the per-resource ABAC layer
(not yet wired into the gate — the field is stored but
ignored on admission).

## Security notes

- **Policy is the floor.** Categorical Allow at the gate
  is checked against PolicyEngine.evaluate afterward.
  Agent profiles can only NARROW. There is no path for an
  agent profile to widen what policy denies. The
  `policy_floor_holds_after_gate_allow` test in
  `admission/agent_gate.rs` documents the contract.
- **AIC bundle vs profile drift.** The AIC bundle carries
  groups (the credential's structural claim); the agent
  profile carries categorical permissions. They can drift
  — operators can grow / shrink an agent's profile without
  re-issuing the AIC. The more restrictive of the two
  wins on every call.
- **One-shot tokens.** The approval token is consumed on first
  admission via an atomic `INSERT OR IGNORE` into
  `approval_token_blocklist` using a BLAKE3-keyed blocklist key.
  A second use fails with `approval_token_consumed`. The token is
  an Ed25519-signed structured object — see
  [`approval-tokens.md`](approval-tokens.md).
- **Surface is transport-layer-derived.** The gate reads
  `caller_surface` from the transport layer (peer alias), not from the
  request envelope's `surface` field (which is operator-asserted and
  ignored for admission). A forged `envelope.surface` cannot bypass a
  surface allowlist. The derivation is based on the TCP port-to-alias
  map populated at `PeerConnected` time.
- **Args redaction.** The audit log stores a BLAKE3 hash
  of the method (used as the redaction handle) — the raw
  args never land in the approval table. The hash lets
  operators correlate a denied call with a later replay
  without leaking secrets.

## Configuration reference

| Field on agent profile | Default | Meaning |
|---|---|---|
| `status` | `active` | `active` / `suspended` / `disabled`. |
| `risk_ceiling` | `medium` | Max risk level the gate admits. |
| `surface_allowlist` | `[]` (all surfaces) | When non-empty, only matching surfaces admit. |
| `allow_categories` | `[]` (all) | When non-empty, the call must overlap. |
| `deny_categories` | `[]` | Any overlap with the cap's categories denies. |
| `allow_sensitivity_tags` | `[]` (all) | When non-empty, all of the cap's tags must be in. |
| `deny_sensitivity_tags` | `[]` | Any overlap denies. |
| `approval_required_categories` | spec default list | Categories that need operator approval first. |
| `approval_timeout_secs` | `86400` (24 h) | Per-agent override for how long an approval row lives. |

## Capability surface

| Method | Notes |
|---|---|
| `agent.create / get / list / update / delete` | CRUD on `agent_profiles`. |
| `agent.effective_capabilities` | Intersect an agent's permissions with a peer's manifest. |
| `coord.approval.pending` | List pending approvals newest-first. |
| `coord.approval.decide` | Approve / reject; approve mints a one-shot token. |
| `agent.standing_approval.create / list / revoke` | Time-bounded categorical pre-approvals. |

Bridge JSON proxies for all of the above at `/v1/agents`,
`/v1/approvals`, `/v1/agents/:id/standing-approvals`, and
`/v1/standing-approvals/:id`.
