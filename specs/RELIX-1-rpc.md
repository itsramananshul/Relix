# RELIX-1 — Relix RPC Protocol

**Status:** Frozen target. Alpha implements a subset; see `specs/alpha-simplifications.md`.

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
1 transport          2 timeout              3 peer_unreachable
4 unknown_method     5 invalid_args         6 policy_denied
7 identity_invalid   8 credential_expired   9 capability_deprecated
10 capability_removed 11 responder_internal 12 responder_overloaded
13 replay_rejected   14 version_mismatch    15 approval_timeout
16 approval_denied   17 cancelled           18 manifest_stale
```

## 1.7 Timeouts

Absolute deadlines. Responder rejects if local clock > deadline + 30s skew. Mid-handler expiry returns `timeout`. Operators MUST run NTP.

## 1.8 Retries

Caller-side, governed by capability `idempotency`:
- `idempotent`: retry freely; reuse `idem`/`rid` for dedup.
- `at_most_once`: MUST NOT retry on `responder_internal`.
- `at_least_once_safe`: retry freely; responder caches result keyed by `(caller, idem)` for ≥ 5 min.

Intermediate attempts go to ops log, not event log.

## 1.9 Replay Protection

Responder maintains sliding-window cache of `(caller_peer_id, rid, n)` covering `max_deadline_skew + max_request_lifetime` (default 5 min). Duplicate ⇒ `replay_rejected`.

## 1.13 Admission Pipeline (strict order)

The responder MUST evaluate in this order, rejecting at the first failure:

1. Decode envelope.
2. Verify protocol version.
3. Verify deadline not exceeded.
4. Verify nonce not in replay cache (add to cache).
5. Verify and resolve identity bundle.
6. Verify signed envelope if capability requires.
7. Look up capability by `(method, major)`.
8. Validate `args` against capability args CDDL.
9. Apply policy engine. Allow → proceed. Deny → `policy_denied`. Approval-required → return descriptor.
10. Dispatch handler.
11. Write audit record (success or failure).

Steps 1–9 complete before handler logic touches state. **The ordering is non-negotiable.**

## 1.17 Versioning

`pv` increment is breaking. Within `pv`, additive fields use map keys ≥ 1024; unknown high keys MUST be ignored.

---

## Alpha Implementation Notes

Alpha implements:
- Steps 1, 3, 5, 9, 10, 11 of the admission pipeline. Steps 2, 4, 6, 7, 8 are present in stub form (return success) and tracked in SIMPs for full implementation at Gate 2.
- Subset of error kinds: `transport`, `timeout`, `unknown_method`, `invalid_args`, `policy_denied`, `identity_invalid`, `responder_internal`.
- Idempotency cache deferred (SIMP — capabilities are alpha-idempotent by handler design).
- Signed-envelope requirement deferred (no capabilities currently require it in alpha).

The wire envelope format above is the alpha target. The `relix-runtime` codec produces and consumes envelopes of this shape.
