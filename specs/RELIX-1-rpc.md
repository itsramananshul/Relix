# RELIX-1 — Relix RPC Protocol

**Version:** 0.4.1 | **Status:** Frozen target. Alpha implements a subset; see `specs/alpha-simplifications.md`.

## 1.1 Responsibilities

`/relix/rpc/1` is the unary, typed, identity-bearing, policy-evaluable request/response primitive between two Relix controllers. Every cross-node interaction that is not a stream travels here. Carries: method invocation, verified caller identity, pinned capability version, deterministic timeout, audit correlation key, replay protection.

## 1.2 Invariants

1. Every RPC is end-to-end correlatable by a single `request_id`.
2. Every RPC carries a verified identity bundle or is rejected at admission.
3. Every RPC pins exactly one capability major version.
4. Deadlines are absolute, not relative.
5. Every RPC produces exactly one audit record on the responder regardless of outcome.
6. Replay of a non-idempotent RPC is rejected.

## 1.3 Transport

Over libp2p protocol `/relix/rpc/1`. Stack: TCP + Noise XK + Yamux. Request and response are each one deterministic-CBOR document delivered as a libp2p `request_response` exchange. Max request 1 MiB, max response 4 MiB. Larger uses RELIX-2.

## 1.4 Request Envelope (fields)

`pv` (u8 protocol version), `rid` (16-byte request id), `tid`/`sid`/`pid` (trace context), `m` (method tstr), `mv` (u32 capability major), `args` (CBOR typed per capability), `ib` (signed identity bundle), `at` (optional attenuated token), `dl` (absolute deadline tag(1)), `n` (16-byte nonce), `sig` (caller signature if capability requires), `idem` (optional idempotency key).

## 1.5 Response Envelope

`pv`, `rid` (echoed), `rn` (responder node id), `res` (tagged union: `ok(value)` / `err(error_envelope)` / `approval_required(descriptor)` / `throttled(backoff_hint)`), `pa` (policy attachment point evaluated), `aid` (audit record id), `pt` (processed timestamp), `sig` (responder signature if required).

## 1.6 Error Kinds (stable enum, values ≥ 1024 reserved)

```
1  transport            2  timeout               3  peer_unreachable
4  unknown_method       5  invalid_args          6  policy_denied
7  identity_invalid     8  credential_expired    9  capability_deprecated
10 capability_removed  11  responder_internal   12  responder_overloaded
13 replay_rejected     14  version_mismatch     15  approval_timeout
16 approval_denied     17  cancelled            18  manifest_stale
19 approval_required   20  approval_token_invalid
21 security_denied     22  resource_exhausted
23 manifest_invalid    24  manifest_unknown_signer
```

| Code | Name | When used |
|------|------|-----------|
| 19 | `approval_required` | Admission requires an operator approval token (capability on always-require list or agent-gate verdict). |
| 20 | `approval_token_invalid` | An approval token was supplied but failed atomic-consume verification. |
| 21 | `security_denied` | Session-token gate failed; tenant isolation deny; approval token present but gate not wired. |
| 22 | `resource_exhausted` | Budget enforcer returned `Reject`. |
| 23 | `manifest_invalid` | Received manifest failed Ed25519 verification, CBOR decode, or fingerprint check. |
| 24 | `manifest_unknown_signer` | Received manifest signer pubkey not in TOFU pin store and first-seen TOFU pin was rejected. |

## 1.7 Timeouts

Absolute deadlines. Responder rejects immediately if `local_clock_unix_secs > deadline` — there is no grace period beyond the deadline itself. Clock-skew tolerance (`max_clock_skew_ms`, default 5 s) applies to the *freshness* check on `issued_at_ms` (step 3b of the admission pipeline), not to deadline enforcement. Mid-handler expiry returns `timeout`. Operators MUST run NTP.

## 1.8 Retries

Caller-side, governed by capability `idempotency`:
- `idempotent`: retry freely; reuse `idem`/`rid` for dedup.
- `at_most_once`: MUST NOT retry on `responder_internal`.
- `at_least_once_safe`: retry freely; responder caches result keyed by `(caller, idem)` for ≥ 5 min.

Intermediate attempts go to ops log, not event log.

## 1.9 Replay Protection

Responder maintains a sliding-window cache keyed on `"{subject_id}|{rid_hex}|{issued_at_ms}"` covering `max_deadline_skew + max_request_lifetime` (default 5 min window, ~1 M entry hard cap). Two different callers with the same `rid` bytes do NOT collide because `subject_id` is part of the key. The `issued_at_ms` anchors uniqueness to the exact envelope timestamp, so a verbatim replayed envelope — identical `rid` and `issued_at_ms` — is rejected. Duplicate ⇒ `replay_rejected`.

The cache is inserted **after** identity verification completes (step 5 of the admission pipeline). Unauthenticated envelopes are rejected before they can pin a nonce slot.

## 1.13 Admission Pipeline (strict order)

The responder MUST evaluate in this order, rejecting at the first failure:

1. Decode envelope.
2. Verify protocol version.
3. Verify deadline not exceeded (`now_unix_secs > req.deadline` → immediate `timeout`).
3b. Freshness + one-sided clock-skew check on `issued_at_ms` (`age_ms < -max_clock_skew_ms` → `replay_rejected("future_envelope")`; `age_ms > freshness_window_ms` → `replay_rejected("stale_envelope")`).
4. Verify and resolve identity bundle (→ `identity_invalid`). **Identity verification precedes the replay-cache insert** (step 4b) to prevent unauthenticated nonce pinning.
4b. Verify nonce not in replay cache, keyed `"{subject_id}|{rid_hex}|{issued_at_ms}"` (add to cache). → `replay_rejected`.
5. Verify signed envelope if capability requires.
6. Verify session token if `verify_on_dispatch` enabled (→ `security_denied`).
7. Look up capability by `(method, major)` (→ `unknown_method`).
8. Validate `args` against capability args CDDL.
9. Apply policy engine. Allow → proceed. Deny → `policy_denied`. Approval-required → `approval_required`.
9b. Apply access broker (rate-limit / concurrency check).
9c. Apply PII gate if wired.
9d. Apply budget enforcer if wired (→ `resource_exhausted`).
10. Dispatch handler.
11. Write audit record (success or failure). Audit id (`aid`) is a server-minted UUIDv4 — it is **not** the echoed `req.rid`.

Steps 1–9d complete before handler logic touches state. **The ordering is non-negotiable.**

## 1.17 Versioning

`pv` increment is breaking. Within `pv`, additive fields use map keys ≥ 1024; unknown high keys MUST be ignored.

---

## Alpha Implementation Notes (v0.4.1)

Alpha implements the following admission pipeline steps fully:

| Step | Status | Notes |
|------|--------|-------|
| 1 — Decode envelope | ✓ Implemented | `invalid_args` on failure |
| 2 — Protocol version | Stub (pass-through) | SIMP; Gate 2 |
| 3 — Deadline check | ✓ Implemented | `now > deadline` → immediate `timeout`; no 30 s grace |
| 3b — Freshness/skew | ✓ Implemented | `max_clock_skew_ms` default 5 s; `freshness_window_ms` default 300 000 ms |
| 4 — Identity verify | ✓ Implemented | Runs **before** replay-cache insert |
| 4b — Replay cache | ✓ Implemented | Key: `"{subject_id}\|{rid_hex}\|{issued_at_ms}"`; window 5 min; ~1 M entry cap |
| 5 — Signed envelope | Stub | No alpha capability requires it |
| 6 — Session token | ✓ Implemented | When `verify_on_dispatch = true`; fail-closed if service not wired |
| 7 — Capability lookup | ✓ Implemented | `unknown_method` on miss |
| 8 — Args CDDL | Stub | Hand-typed Rust structs; SIMP |
| 9 — Policy | ✓ Implemented | `policy_denied`; `approval_required` |
| 9b — Access broker | ✓ Implemented | `policy_denied` on rate-limit |
| 9c — PII gate | ✓ Implemented (when wired) | `policy_denied` on block |
| 9d — Budget enforcer | ✓ Implemented (when wired) | `resource_exhausted` on reject |
| 10 — Dispatch | ✓ Implemented | |
| 11 — Audit | ✓ Implemented | `aid` = server-minted UUIDv4 (not echoed `rid`) |

Implemented error kinds in alpha: `transport`, `timeout`, `unknown_method`, `invalid_args`, `policy_denied`, `identity_invalid`, `responder_internal`, `replay_rejected`, `approval_required`, `approval_token_invalid`, `security_denied`, `resource_exhausted`, `manifest_invalid`, `manifest_unknown_signer`.

- Idempotency cache deferred (SIMP — capabilities are alpha-idempotent by handler design).
- Signed-envelope requirement deferred (no capabilities currently require it in alpha).

The wire envelope format above is the alpha target. The `relix-runtime` codec produces and consumes envelopes of this shape.
