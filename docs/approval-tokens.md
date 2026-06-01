# Approval Tokens

Version: 0.4.1

Approval tokens are the cryptographic artefacts that allow an agent to carry
proof of a recorded operator decision through the admission gate without
re-running the full approval flow. This document is the authoritative
reference for operators and contributors.

## What an approval token is

An approval token is an **Ed25519-signed structured JSON object**,
base64url-nopad encoded. On the wire it looks like a single opaque string of
roughly 300–400 characters. It is **not** a random hex string and is **not**
HMAC. The prior HMAC-SHA256 scheme (version `0x01`) is rejected at parse time
with `TokenFormatDeprecated`. Any agent still holding a v0x01 token must be
restarted to receive a fresh v0x02 token.

## Wire format

```
base64url_nopad( JSON({
  "version":                 2,          // u8, MUST be 0x02
  "approval_id":             string,     // approval row this token was issued for
  "method":                  string,     // exact capability method name
  "subject_id":              string,     // hex NodeId of the authorised caller
  "session_id":              string,
  "issued_at_ms":            i64,        // unix milliseconds
  "expires_at_ms":           i64,        // unix milliseconds
  "nonce":                   string,     // 64 hex chars = 32 random bytes (OsRng)
  "signing_key_fingerprint": string,     // 32 hex chars = BLAKE3 prefix of verifying key
  "signature":               string      // base64url-nopad Ed25519, ~86 chars
}) )
```

The ten-field JSON payload is serialized, then base64url-nopad encoded to
produce the wire token. The typical wire token is 300–400 characters.

### Canonical signing bytes

The signature covers the canonical pipe-delimited byte string:

```
{version:02x}|{approval_id}|{method}|{subject_id}|{session_id}|{issued_at_ms}|{expires_at_ms}|{nonce}|{signing_key_fingerprint}
```

Any of `approval_id`, `method`, `subject_id`, or `session_id` containing a
`|` character is rejected at issue time with `ForbiddenDelimiter`. Relix IDs
(UUIDs, hex NodeIds, lower-snake-case identifiers) never contain `|` by
convention.

Verification uses `ed25519_dalek::VerifyingKey::verify_strict`, which rejects
weak, malleable, or non-canonical signatures (small-order R/S values, etc.).

## Algorithm summary

| Property | Value |
|---|---|
| Signing algorithm | Ed25519 (`ed25519_dalek`) |
| Wire version byte | `0x02` |
| Deprecated version | `0x01` — HMAC-SHA256; refused at parse |
| Key fingerprint | First 32 hex chars of BLAKE3 hash of the verifying key |
| Nonce | 32 bytes of `OsRng`, hex-encoded to 64 chars on wire |
| Signature | base64url-nopad Ed25519, ~86 chars |
| Signature input | canonical pipe-delimited bytes (above) |
| Wire encoding | base64url-nopad of UTF-8 JSON |
| Wire length | ~300–400 chars |

## Environment variable

```
RELIX_APPROVAL_SIGNING_KEY=<64-hex string>
```

The value is the 32-byte Ed25519 signing-key seed, hex-encoded. This variable
is **required** on every controller instance that issues approval tokens. If
the variable is absent or malformed:

- The `ApprovalSigner` cannot be constructed (`TokenError::MissingSigningKey`).
- The `ApprovalKeySet` is empty.
- Every token-bearing inbound call fails admission with
  `approval_token_missing_key`.

There is no fallback and no soft failure. Deployment checklists must include
this variable for coordinator nodes that have the approval subsystem enabled.

## TTL and expiry

TTL is specified in seconds and is **clamped** to `[30, 86400]` (30 seconds
to 24 hours) at the coordinator before the token is minted. Values outside
this range are silently clamped — the caller receives a token with the clamped
TTL. The default TTL is 86400 (24 h).

Expiry check: `now_ms >= expires_at_ms` → rejected. The boundary is
inclusive; a token is invalid at the exact millisecond it expires.

## Controller issues; operator approves

The controller (coordinator node) mints and signs the token **after** the
operator has recorded an approval decision via `approval.record_decision`. The
flow is:

1. Agent calls a capability method; gate emits `RequireApproval`.
2. Coordinator persists an `approval_delivery` row and dispatches the request
   to the configured channel (dashboard, Slack, email, …).
3. Operator reviews and calls `approval.record_decision{approved}`.
4. Coordinator's `ApprovalSigner` (loaded from `RELIX_APPROVAL_SIGNING_KEY`)
   mints the token and returns it.
5. Agent presents the token on the next call to the same capability method.

The approver never holds the signing key. The approver authorises; the
controller mints.

## Admission gate verification path

When the admission gate receives a token-bearing call, `evaluate_token` runs
these checks in order. Failure at any step returns `GateDecision::Deny`:

1. **Parse** `ApprovalToken::parse(wire)` — rejects `version != 0x02` with
   `TokenFormatDeprecated` (deny rule `approval_token_format_deprecated`).
2. **Signature** `verify_signature(keyset)` — looks up
   `signing_key_fingerprint` in the `ApprovalKeySet`; runs `verify_strict`.
   Unknown fingerprint → `UnknownSigningKey`; keyset empty →
   `MissingSigningKey` (deny rule `approval_token_missing_key`).
3. **Method binding** `check_method(request_method)` — byte-for-byte match;
   no method aliasing.
4. **Subject binding** `check_subject(caller_subject_id)` — constant-time
   comparison via `subtle::ConstantTimeEq`.
5. **Expiry** `check_not_expired(now_ms)` — `now_ms >= expires_at_ms` →
   rejected.
6. **Approval row** `store.get_approval(approval_id)` — row must exist and
   have `status = approved`.
7. **Replay blocklist** `store.try_consume_token_atomic(blocklist_key,
   approval_id, now_ms)` — atomic `INSERT OR IGNORE` on a unique BLAKE3 key;
   if the key was already present the token is `AlreadyConsumed` (deny rule
   `approval_token_consumed`).

On success: `GateDecision::Allow { matched_rule: "approval_token",
consumed_approval_id: Some(approval_id) }`.

### Deny rule strings

| Condition | `matched_rule` |
|---|---|
| Token version `0x01` (HMAC) at parse | `approval_token_format_deprecated` |
| Key fingerprint not in keyset | `approval_token_unknown_key` |
| Empty keyset (missing env var) | `approval_token_missing_key` |
| Already consumed (replay) | `approval_token_consumed` |
| Any other token validation failure | `approval_token_invalid` (legacy catchall) |

> Note: `APPROVAL_TOKEN_INVALID` (`approval_token_invalid`) is the legacy
> catchall deny reason constant. Replay specifically produces
> `approval_token_consumed`. These are distinct strings; monitoring rules
> should track both.

## Blocklist and replay prevention

The blocklist key is:

```
BLAKE3(nonce_bytes | b"|" | approval_id_bytes)
```

It is stored in the coordinator's agent-store SQLite, not in the approval
delivery store. Two tokens for the same approval row but with different nonces
produce different blocklist keys — re-minting a token (e.g. after a network
failure) gives a fresh key that does not collide with the previous one.

Consumption is atomic: `store.try_consume_token_atomic` performs a single
`INSERT OR IGNORE` against a UNIQUE column. If two concurrent callers present
the same token, exactly one wins and the other gets `AlreadyConsumed`. There
is no window between "check blocklist" and "insert to blocklist".

Store errors during consumption are **fail-closed**: `TokenError::Store` →
`GateDecision::Deny`.

## Key set and key rotation

`ApprovalKeySet` holds one or more `(fingerprint, VerifyingKey)` pairs. During
a signing-key rotation, load both the old and new verifying keys into the key
set so tokens minted before and after the rotation are both valid until they
expire. Once all pre-rotation tokens have expired or been consumed, remove the
old key from the set.

```toml
# Conceptual — key set is configured in code, not TOML today.
# Load the signer from RELIX_APPROVAL_SIGNING_KEY,
# then call ApprovalKeySet::from_signer(&signer).
```

An empty `ApprovalKeySet` (`is_empty() == true`) causes `MissingSigningKey`
for every token verify call.

## `approval.record_decision` and authorized approvers

Each `approval_delivery` row carries an `authorized_approvers` field —
a JSON array of subject_id strings. When non-empty, only those subjects (or
callers with `operator` / `admin` role) may call `approval.record_decision`
for that row.

## Delivery channels and escalation

The approval request is routed via `ApprovalDeliveryMatrix` using glob rules
`[agent × action] → channel`. Available channels: `telegram`, `slack`,
`discord`, `email`, `dashboard` (always available unless explicitly disabled).

If a matched rule has `escalation_timeout_secs > 0` and an
`escalation_channel`, a cancellable timer is armed on dispatch. When the
operator calls `approval.record_decision` the timer is cancelled. If the timer
fires and the row is still `pending`, the escalation channel receives the
request.

See [`configuration.md`](configuration.md) for the `[approval.delivery]` TOML
block reference.

## Fail-closed summary

| Condition | Outcome |
|---|---|
| `RELIX_APPROVAL_SIGNING_KEY` absent or invalid | Every token-bearing call fails `approval_token_missing_key` |
| `ApprovalKeySet` empty | Same as above |
| Token fingerprint not in keyset | `approval_token_unknown_key` deny |
| Token already consumed | `approval_token_consumed` deny |
| Blocklist store error | Fail-closed deny (`approval_token_invalid`) |
| Token version `0x01` | `approval_token_format_deprecated` deny |

## See also

- [`credentials.md`](credentials.md) — encrypted credential vault
- [`agents.md`](agents.md) — agent gate, five-phase admission, standing approvals
- [`security.md`](security.md) — full per-call admission pipeline
- [`configuration.md`](configuration.md) — `[approval.delivery]` TOML block
