# Agents as Employees — Identity, Policy, Enforcement

## H.0 Core Principle

> An agent's knowledge of a node's existence confers no ability to use that node. Every cross-node call is gated by the responding node's policy evaluation of the calling identity, the requested action, and the supplied arguments — evaluated before the action runs, and recorded after.

In central-gateway designs (OpenClaw, Hermes, Open WebUI), the gateway holds every credential; any agent the gateway trusts inherits the gateway's full authority. Relix removes the gateway, placing enforcement at the network edge of each node. The Finance node cryptographically refuses calls from non-Finance peers; no superuser process can subvert this because no central process exists.

## H.1 Identity Model

**Root primitive:** libp2p Ed25519 `PeerId`. Every agent is a peer; the keypair is the cryptographic identity. The private key never leaves the agent's daemon.

**Layered above the PeerId:** a signed `AgentIdentityCredential` (AIC) — a CBOR-encoded, Ed25519-signed bundle issued by an organization root (or delegated issuer). Contains:

- `peer_id`, `agent_name`, `org_id`
- `groups` (initial set; extensible via separate GMCs)
- `role` (`analyst`, `operator`, `read_only_observer`, ...)
- `clearance_level` (`public` / `internal` / `restricted` / `confidential`)
- `supervising_principals` (human or agent identities authorized to approve actions on behalf of this agent)
- `issued_at`, `expires_at` (bounded lifetime; mandatory)
- `delegation_chain` (signing keys back to the org root)
- `signature`

Org-root keys live offline / in HSM in production. Issuer Authority keys sign AICs day-to-day.

Every RPC: the responding node verifies signature → walks the delegation chain to a trusted root → checks expiry → checks revocation. Only then does it consider the request.

Human users (Relix Web logins) have identity credentials of the same shape with `role=human`. On-behalf-of operations carry both an agent identity and a delegating-user identity.

## H.2 Groups / Departments

Groups are named sets of agent identities. Group membership is a separate signed credential (GMC), issued by a group admin and presented alongside the AIC.

- Groups can nest: `Finance.Treasury ⊂ Finance`.
- An agent may hold many GMCs simultaneously.
- GMCs have independent expiry — short-lived for on-call rotations, longer for stable roles.
- Group revocation = revoke the GMC; the underlying AIC remains valid for other groups.

GMCs separate from AICs because group churn is faster than identity churn.

## H.3 Node-Level Permissions

Each node carries a node policy file (signed bundle) declaring who may speak to it at all. Evaluated immediately after libp2p connection and identity verification, before any method dispatch.

```
node: finance-ledger
trust_roots: [org-root-public-key]
admit:
  - group: Finance
  - group: Audit
  - identity: human:cfo@org
deny:
  - clearance_level: public
default: deny
```

Coarse-layer rejection prevents probing of capability surface, work consumption, and rate-budget exhaustion.

## H.4 Action-Level Permissions

Per-method, per-arguments, per-identity policy. Examples:

```
method: ledger.write_journal_entry
allow_when: caller.group includes Finance.Treasury AND args.amount <= 10000
require_approval_when: args.amount > 10000 AND args.amount <= 100000
  approvers: group:Finance.Director, count: 1
deny_when: args.amount > 100000
```

**Policy is declarative, not Turing-complete.** SOL is the wrong tool here (general-purpose, dynamic). Use Cedar (decidable, formally analyzable). Policy lives on the responding node, never centralized.

## H.5 Approval Flows for Sensitive Actions

`require_approval` is a third outcome alongside `allow` / `deny`. On match:

1. Responding node suspends the request.
2. Emits `approval.requested` event to the network.
3. Approval node(s) deliver to qualified humans.
4. Approver signs `approval.granted{nonce, decision}` envelope.
5. Responding node verifies the approver satisfies the policy criteria, executes, returns result.

Approval flows are uncircumventable: no code path on the responding node lets a `require_approval` decision proceed without a valid signed approval.

## H.6 Enforcement Pipeline (Strict Order)

On every RPC, the responding node MUST evaluate, in order, and reject at the first failure:

1. Transport auth (Noise XK).
2. Credential verification (AIC + GMCs → delegation chain → trust root → expiry → revocation).
3. Node-level admission (§H.3).
4. Capability lookup.
5. Action-level policy (§H.4) → `allow` / `deny` / `require_approval`.
6. If allow: handler runs. Otherwise: signed denial / approval orchestration.
7. Audit emit.
8. Response.

Steps 1–5 complete before any handler logic touches state.

## H.7 Audit Logs

Every step from §H.6 produces an audit record on the responding node (never the caller — a caller cannot be trusted to audit itself).

**Record shape:** timestamp, caller_peer_id, caller_groups[], delegating_user (if any), method, args_hash, policy_decision, result_status, latency, prev_record_hash (hash chain), signature.

Two-tier audit storage: local on the responding node (replay/forensics) and audit nodes (cross-cutting queries). If audit write to local fails, the action MUST NOT proceed.

A separate `audit.query` capability serves compliance queries.

## H.8 Access Requests / Grants / Denials / Revocation

**Access requests:** an agent receiving `deny — missing_group:X` may submit a signed access request to an admin node. Admins review via Relix Web; approval issues a new GMC.

**Grants:** issuance of new credentials, always time-bounded.

**Revocation:** three layers — short-lived credentials (primary defense), gossiped CRLs (secondary), emergency push-revoke signed by Org Root (tertiary).

**IA-key compromise:** Org Root revokes IA; all credentials chained through it become invalid; replacement IA spun up; agents re-issued.

## H.9 Why P2P Enables This

Central-gateway: gateway holds all keys; agent the gateway trusts inherits gateway's full authority; no enforcement boundary inside the gateway. Bug, prompt injection, or misconfigured plugin gets the union of all access.

Relix P2P: Finance node holds Finance keys only; no shared process holds multiple node-types' keys. A compromised AI node leaks its own LLM API key and nothing else. A compromised Finance-Agent-01 leaks one agent identity bounded by Finance's policy and audited line-by-line. Blast radius is structurally bounded by the architecture.

Equally important: **policy authoring is local to the team that owns the data.** Finance writes Finance's policy. No central security team forced to understand every domain.

This framing is the strongest production differentiator Relix has against every existing agent platform. It is non-negotiable.

## Alpha Status

The alpha implements a simplified form of this model. See `specs/alpha-simplifications.md` for the deltas (single-key trust model, allowlist policy DSL instead of Cedar, no approval flows yet, no CRL gossip). The architectural shape — identity-bundle wire format, enforcement pipeline ordering, audit-on-responder — is unchanged.
