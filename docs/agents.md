# Agents

_Version: 0.4.1_

In Relix, an **agent** is a first-class employee record: an identity,
a job (role / department / team), a permission scope, and a
lifecycle (`active` / `suspended` / `disabled`). When an inbound
mesh call's subject matches an agent profile, the dispatch bridge
runs a five-phase **agent gate** between identity verification and
the policy engine — additive narrowing, never widening — and can
pause execution until an operator approves it.

When the caller has no agent profile, the gate **denies** with
`AGENT_NO_PROFILE` (fail-closed). When the agent store is not wired
at all, every call is denied with `AGENT_STORE_NOT_CONFIGURED`.

## Identity

Every peer holds an **identity bundle** (`.aic` or `.bundle`) signed
by the org root Ed25519 key. The bundle carries:

- `subject_id` — 32 bytes, the canonical agent identifier on the wire
- `name` — the human-friendly label given to `relix-cli identity mint`
- `groups` — coarse policy-engine group memberships (e.g. `chat-users`)
- `issued_at` / `expires_at`

Mint one with the CLI:

```sh
relix-cli identity mint \
    --root-key dev-keys/local-org-root.key \
    --name research-assistant \
    --groups chat-users \
    --out dev-keys/research-assistant.aic
```

The printed `subject-id` is the value you'll pin an agent profile to
when you create one.

## The five-phase gate

The agent gate runs in this order; the first match wins.

1. **Status.** `suspended` → `agent_suspended` deny.
   `disabled` → `agent_disabled` deny. Anything other than `active`
   is denied.
2. **Surface allowlist.** When non-empty, the transport-layer-derived
   `caller_surface` (peer alias, not the envelope's `surface` field —
   which is untrusted) must match one of the agent's allowed surfaces
   (e.g. `dashboard`, `telegram`, `flow`). Empty allowlist = all
   surfaces.
3. **Risk ceiling.** The called capability's `risk_level` is
   compared against the agent's `risk_ceiling`. Order:
   `safe < low < medium < high < critical`. Above the ceiling →
   `agent_risk_ceiling_exceeded`.
4. **Deny lists.** Reject if the capability's `categories` or
   `sensitivity_tags` overlap the agent's `deny_categories` or
   `deny_sensitivity_tags`.
5. **Allow lists.** When non-empty, the capability must have at
   least one category in `allow_categories` and all of its
   sensitivity tags must be in `allow_sensitivity_tags`.

After all five pass, the call falls through to the policy engine and
then to the handler. **The policy file is still the floor** — agent
permissions can never widen what policy denies.

A sixth pseudo-phase handles **approval-required** categories: when
the capability matches `approval_required_categories`, the gate
either admits through a standing approval if one is active, or
returns `RequireApproval`. The bridge then mints an approval row +
chronicle event + Telegram notification (when wired) and the call
waits.

The **approval-token fast path**: when an inbound envelope carries
an `approval_token`, the gate verifies its Ed25519 signature, checks
expiry and method match, then atomically records it in the
`approval_token_blocklist`. Single use only — a second use fails with
`approval_token_consumed`. See [`approval-tokens.md`](approval-tokens.md)
for the token format.

## Creating an agent profile

### Dashboard

`/dashboard` → `#/agents` → fill in the create form → **Create**.

### CLI

```sh
relix-cli ops agent create \
    --name "Research Assistant" \
    --role research_assistant \
    --title "Junior research analyst" \
    --department research \
    --team research-ops \
    --created-by alice \
    --subject-id <hex subject_id from `relix-cli identity mint`> \
    --risk-ceiling medium
```

### HTTP

```http
POST /v1/agents
Content-Type: application/json

{
  "name": "Research Assistant",
  "role": "research_assistant",
  "title": "Junior research analyst",
  "department": "research",
  "team": "research-ops",
  "created_by": "alice",
  "subject_id": "<hex>",
  "risk_ceiling": "medium"
}
```

Once created, the profile is keyed by `subject_id`. Subsequent
inbound calls from that subject pass through the agent gate.

## Lifecycle

| Status      | Effect                                                         |
|-------------|----------------------------------------------------------------|
| `active`    | Normal admission — gate evaluates checks 2–5.                  |
| `suspended` | All calls denied with `agent_suspended` until reactivated.     |
| `disabled`  | All calls denied; intended for offboarding. Profile is kept for audit history but the agent cannot be brought back without manual operator action. |

Flip with the dashboard, `relix-cli ops agent suspend|activate|disable`,
or `POST /v1/agents/:subject_id/{suspend,activate,disable}`.

## Subject derivation for channel users

A Telegram, Discord, or Slack user does not log in with an identity
bundle — they're identified by the channel platform's user ID. The
channel controller **derives** a stable `subject_id` from that ID
using a per-channel blake3 namespace (`telegram:`, `discord:`,
`slack:`). The derived subject id is what the agent gate sees, so an
operator can pin agent profiles to specific platform users.

Allow-list gating for channels is independent of agent profiles — an
unlisted Telegram user is dropped at the channel controller before
any subject derivation happens. See [`channels/index.md`](channels/index.md).

## Standing approvals

For repetitive `approval_required` actions an operator can grant a
**standing approval**. Standing approvals can expire by time,
call count (`max_calls`), or estimated spend (`max_cost_micros`):

```sh
# HTTP
POST /v1/agents/<agent_id>/standing-approvals
{
  "category": "payments",
  "expires_at": 1717200000,
  "scope_kind": "task",
  "task_id": "<task_id>",
  "max_calls": 20,
  "max_cost_micros": 200000
}
```

The gate then admits matching calls without minting per-call
approvals until expiry. List and revoke:

```sh
curl localhost:9100/v1/agents/<agent_id>/standing-approvals
curl -X DELETE localhost:9100/v1/standing-approvals/<standing_id>
```

## Inspecting agent activity

- `relix-cli ops agent list` / `agent get --subject-id <hex>`
- `GET /v1/agents` / `GET /v1/agents/:subject_id`
- `GET /v1/agents/:subject_id/effective_capabilities` —
  the gate's view of "what could this agent actually call right now",
  computed from the local capability index intersected with the
  agent's allow / deny / ceiling.
- `GET /v1/policy/denials` — the bridge's denial ring (256 entries,
  hash-chained) shows gate denials too, tagged with the deny phase.

## Deep detail

The shipped gate semantics, exact error kinds, and CLI surface live
in [`agent-permissions.md`](agent-permissions.md). The text /
embedding / persistent memory layers — which are subject-id scoped
the same way — live in [`memory.md`](memory.md).
