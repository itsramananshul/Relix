# RELIX-6 — Capability Descriptor Format

**Version:** 0.4.1 | **Status:** Frozen target. Alpha implements minimal descriptor (inline schemas; no CDDL stdlib yet).

## 6.1 Responsibilities

A capability descriptor declares the contract of a method served by a node: name, version, kind, argument and return shape, error kinds, idempotency, cost class, sensitivity tags, policy attachment, signed-envelope requirement, deprecation state. SOL imports them at compile time; SolFlow renders them; policy attaches to them; responders validate wire-level traffic against them.

## 6.2 Invariants

1. A capability is uniquely identified within a node by `(method_name, major_version)`.
2. Args and return described by CDDL types (alpha: by hand-rolled Rust structs serialized via CBOR).
3. Within a major version, args/return is extension-only.
4. Sensitivity tags, cost class, policy attachment are stable for major version lifetime.
5. Descriptors are part of (or referenced from) the signed Node Manifest.

## 6.4 Capability Declaration Fields

- `method_name` (dotted-qualified: `memory.search`)
- `major_version` (u32; pinned by callers)
- `minor_version`, `patch_version`
- `kind` (`unary` / `stream_in` / `stream_out` / `bidi_stream`)
- `args_type` (CDDL type name)
- `return_type` (CDDL type name; `stream<T>` for streaming kinds)
- `error_kinds` (method-specific errors in addition to universals)
- `idempotency` (`idempotent` / `at_most_once` / `at_least_once_safe`)
- `cost_class` (`cheap` / `expensive` / `external_paid`)
- `sensitivity_tags` (e.g., `reads:pii`, `writes:production`)
- `policy_attachment_point` (stable identifier for policy lookup)
- `requires_signed_envelope` (bool)
- `requires_credential_claims` (minimum identity facts)
- `since`, `deprecated_in?`, `removed_in?`, `superseded_by?`
- `notes`

## 6.7 Version Negotiation

Callers pin `major_version`. Responder serves any (minor, patch) within. `removed_in` crossed ⇒ `version_mismatch`. `deprecated_in` crossed but not removed ⇒ served + audit `capability_deprecated` warning.

## 6.8 Compatibility Within Major

Permitted: add optional fields, add new capabilities, tighten sensitivity tags (more is allowed), add error kinds.

Forbidden (requires major bump): remove fields, tighten types, make optional required, change `kind`, relax `requires_signed_envelope`, reduce sensitivity tags.

## 6.13 SOL Import Resolution

```
import Finance.LedgerWrite@1
```

Compile-time: resolve against manifest cache, validate SOL usage against CDDL.
Load-time: verify some peer advertises `(method_name, major_version)`.
Runtime: wire-level args validated against current responder CDDL.

---

## Alpha Implementation Notes (v0.4.1)

Alpha ships the following `CapabilityDescriptor` struct in `relix-core::capability`:

```rust
pub struct CapabilityDescriptor {
    pub method_name: String,
    pub major_version: u32,
    pub kind: CapabilityKind,            // unary | stream_out
    pub idempotency: Idempotency,        // idempotent | at_most_once | at_least_once_safe
    pub cost_class: CostClass,           // cheap | expensive | external_paid
    pub sensitivity_tags: Vec<String>,
    pub policy_attachment_point: String,
    pub requires_groups: Vec<String>,    // alpha shortcut for requires_credential_claims

    // Optional advisory metadata (T4 P1); omitted from wire when None/empty:
    pub description: Option<String>,
    pub categories: Vec<String>,
    pub environment_requirements: Vec<String>,
    pub risk_level: RiskLevel,           // unknown | safe | low | medium | high | critical
}
```

Supporting enums (all serialised as snake_case strings):

| Type | Variants |
|------|---------|
| `CapabilityKind` | `unary`, `stream_out` |
| `Idempotency` | `idempotent`, `at_most_once`, `at_least_once_safe` |
| `CostClass` | `cheap`, `expensive`, `external_paid` |
| `RiskLevel` | `unknown` (default), `safe`, `low`, `medium`, `high`, `critical` |

`RiskLevel::Unknown` means unaudited; flagged as a deployment warning by `relix-cli capability validate`.

The optional fields (`description`, `categories`, `environment_requirements`) are omitted from the CBOR wire encoding when `None`/empty — backward-compatible with pre-T4-P1 manifests.

The following spec §6.4 fields are **not** present in alpha: `minor_version`, `patch_version`, `args_type`, `return_type`, `error_kinds`, `requires_signed_envelope`, `requires_credential_claims`, `since`, `deprecated_in`, `removed_in`, `superseded_by`, `notes`.

- Args and return schemas not formally declared; alpha capabilities accept and return hand-defined CBOR structs (Rust types deriving `Serialize`/`Deserialize`).
- Capabilities registered by node binaries at startup; advertised in the node manifest.
- Versioning: alpha capabilities are all `major=1`; first deprecation/removal cycle exercised at Gate 2.

Alpha capability set:
- `memory.search` (unary)
- `memory.write_turn` (unary)
- `memory.recent_for_session` (unary)
- `ai.chat` (stream_out)
- `tool.web_fetch` (unary)
- `node.health` (unary; default on every node)
- `node.manifest` (unary; default on every node)
