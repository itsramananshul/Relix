# Capability Discovery + Planner Foundations

How a SOL flow (or, eventually, a planner) finds out which
capabilities are available on the mesh, what they do, and which
ones it's allowed to use. This is a **foundations document** —
the discovery mechanism exists today; the planner-side surface is
described here so future work has a contract to satisfy without
violating the architecture invariants.

This is Track 4 of the autonomous roadmap. It's reference + design,
**not** an implementation of a planner. Planners are explicitly
out of scope until the substrate they need is stable enough.

## What "capability" means here

A capability is the unit of work the mesh exposes. Each capability
has a `CapabilityDescriptor` (see
[`crates/relix-core/src/capability.rs`](../crates/relix-core/src/capability.rs))
with:

| Field | What it tells you |
|---|---|
| `method_name` | The method string the caller passes to `remote_call`. |
| `major_version` | Pinned by callers; bumps imply breaking changes. |
| `kind` | `unary` or `stream_out` (server-sent token stream). |
| `idempotency` | `idempotent` / `at_most_once` / `at_least_once_safe`. Drives whether retries are safe. |
| `cost_class` | `cheap` / `expensive` / `external_paid`. Drives rate-limiting and budgeting. |
| `sensitivity_tags` | Free-form tags policy-engine can match (e.g. `reads:internal`, `external:network`, `fs:write`, `parse:html`). |
| `requires_groups` | Minimum-claim groups required to call (structural pre-filter before policy). |
| `policy_attachment_point` | The id policy rules attach to (defaults to method_name). |
| `risk_level` | `unknown` / `safe` / `low` / `medium` / `high` / `critical`. `unknown` means unaudited; flagged as a deployment warning. |
| `description` | *(optional)* One-sentence human-readable description. Used by `relix-cli capability ls` and planners. |
| `categories` | *(optional)* Free-form category tags (e.g. `fetch`, `summarise`, `persist`). |
| `environment_requirements` | *(optional)* Runtime requirements (e.g. `network:outbound`, `api_key:openai`). |

Descriptors are not metadata-on-the-side; they are **the** authoritative
description of what the responder will do.

## How discovery works today

Two layers:

1. **Per-node `node.manifest` capability.** Each controller exposes
   `node.manifest`. As of RELIX-5 PART 2, the handler returns a fully
   signed `SignedManifest` envelope — Ed25519-signed CBOR of `NodeManifest`,
   including the signer's raw public key and a BLAKE3 fingerprint. The
   receiving bridge verifies the signature and pins the signer key via
   TOFU (Trust On First Use). The admission pipeline still gates who may
   call `node.manifest`; the response itself is now cryptographically
   authenticated.
2. **Bridge-side `ManifestCache`** (see
   `crates/relix-runtime/src/manifest/mod.rs`). At bridge startup, the
   bridge dials each configured peer alias and pulls its manifest.
   The cache lives on `AppState.manifest_cache` (an
   `Arc<ManifestCache>`) and is consulted by:
   - the bridge's `/v1/models` endpoint to derive provider labels;
   - the SOL flow runner's `capability:` peer-alias resolver
     (`remote_call("capability:ai.chat", ...)` routes to whichever
     peer registered `ai.chat`).
   - the bridge's M10 capability-routing path generally.

Discovery is **periodic**: the bridge calls `MeshClient::spawn_refresh_loop`
with a caller-configured period. The bridge binary wires 60 seconds (A.4),
but this is a configuration choice at the call site, not a hardcoded constant
in the manifest module. A failed refresh leaves the previous cache entry
intact (`ManifestCache::last_refreshed_at` is only updated on a successful
insert); the bridge does not flap.

**TOFU pin lifetime:** pins are in-memory only and are lost on bridge restart.
A restarting bridge re-pins from the first `SignedManifest` it receives for
each peer — the first-seen fingerprint becomes the trusted key. If a peer
rolls its signing key, the bridge must be restarted (or the pin store cleared)
to accept the new key.

## What discovery does NOT do today

- **No semantic descriptions.** The descriptor has tags and
  groups, but no free-form description. A planner that wants to
  "find a capability that summarises text" can't ask "which
  capabilities summarise text?" today. The current matching is
  exact-method-name or tag-based.
- **No suitability scoring.** No "this capability is 80% suitable
  for the request based on its cost class + sensitivity tags".
  Planner scaffolding (below) needs this.
- **No environment requirements.** A capability like `tool.pdf`
  needs the `lopdf` crate to be linked; the descriptor doesn't
  surface that. Operators figure it out by reading the controller's
  startup log.
- **No version negotiation beyond pin-and-fail.** Callers pin
  `major_version`; mismatches surface as
  `error_kinds::VERSION_MISMATCH`. There's no auto-fallback to an
  older compatible version. (Documented in
  [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md).)

## Planner foundations (design only, not implemented)

A planner sits **above** SOL, not inside it. SOL is the
orchestration authority — a planner produces SOL flows (or
modifies an in-flight one's parameters) but does not replace SOL's
`remote_call` opcode with a non-deterministic alternative.

The minimum surface a future planner needs:

### P1 — Suitability-tagged manifest

Extend `CapabilityDescriptor` with **optional** fields:

```rust
pub struct CapabilityDescriptor {
    // ... existing fields ...

    /// Short human-readable description (one sentence).
    /// Planner uses this for prompt context; humans use it for
    /// `relix-cli capability ls`.
    pub description: Option<String>,

    /// Free-form categories: `fetch`, `parse`, `summarise`,
    /// `persist`, `notify`, etc. A planner narrows by category
    /// first, then by sensitivity tags.
    pub categories: Vec<String>,

    /// What the responder needs at runtime to provide this
    /// capability (e.g. `["network:outbound", "api_key:openai"]`).
    /// Operators see this; planners use it to avoid suggesting
    /// flows the mesh can't actually run.
    pub environment_requirements: Vec<String>,
}
```

These fields are **purely advisory**. Policy still gates every
call. A planner that suggests a flow involving `tool.web_fetch`
on a mesh whose tool node has no outbound network access will be
told `policy_denied` at execution time — no special handling
needed.

### P2 — Discoverable manifest aggregation

A bridge-side endpoint:

```
GET /v1/capabilities
GET /v1/capabilities?category=fetch&sensitivity=external:network
GET /v1/capabilities/<method_name>
```

Returns the discovered manifests in JSON. Same translation-only
contract as `/v1/tasks` — it's a projection of the
`ManifestCache`. No planning logic in the bridge.

### P3 — Operator visibility

`relix-cli capability ls / get / where-is`:

```bash
relix-cli capability ls           # all known capabilities, one per line
relix-cli capability ls --tag fs  # filter by sensitivity tag
relix-cli capability get tool.pdf # show one descriptor in detail
relix-cli capability where-is tool.web_fetch
#   → peer 'tool' (12D3KooW...)
#     manifest_age=12s
```

This is sibling to the existing `relix-cli task` surface — pure
projection of mesh state, no orchestration.

### P4 — Planning lives OUTSIDE the runtime

The actual "given this task, produce a SOL flow that satisfies it"
work is **not a Relix runtime concern**. It's a SOL-author
concern. Concretely:

- A planner could be a CLI tool that consumes `/v1/capabilities`
  and emits a `.sol` file.
- A planner could be a flow itself (calling `ai.chat` with the
  capability list as context and emitting a SOL program), which
  the operator then registers as a template.
- A planner could be a separate peer that exposes a
  `planner.plan_flow` capability returning a SOL string.

In every case, the planner is a **consumer** of the discovery
surface, not a runtime extension. SOL flow execution is still
deterministic and runs the same admission pipeline.

## Hard non-goals (architectural invariants)

These are **not allowed**, full stop:

1. **No autonomous recursive planner inside SOL.** A flow may call
   `ai.chat` to generate text; it MUST NOT use that text to spawn
   another flow as a side-effect of execution. Sub-flow spawning
   is a SOL VM feature reserved for Gate 2 (durable yield model),
   and even then it will be an explicit opcode, not a side
   channel.
2. **No hidden orchestration prompts.** Capability descriptors are
   public mesh state. There is no "system prompt" that customises
   how the bridge or planner behaves per-user. The substrate
   stays content-agnostic.
3. **No swarm routing.** The planner picks one execution path
   per task. There is no "race three planners and pick the
   winner" loop hidden in the runtime.
4. **No self-modifying flows.** A flow's SOL bytecode is hashed
   into its `flow_id`. The runtime does not reassemble bytecode
   mid-execution.
5. **SOL remains the orchestration authority.** Planners produce
   SOL; SOL executes; that boundary is load-bearing.

Violating any of these breaks the runtime guarantees that the
admission pipeline, the audit log, and the per-flow event log
depend on.

## Suggested next concrete steps (when this work is greenlit)

1. **P1 (small, safe):** add the three optional fields to
   `CapabilityDescriptor` with serde defaults so older serialised
   manifests still decode.
2. **P2 (small, safe):** add `/v1/capabilities` endpoints to the
   bridge as a thin projection of `ManifestCache`. Identical
   pattern to `/v1/tasks`.
3. **P3 (small, safe):** add `relix-cli capability` subcommand —
   no new runtime code, just consumes `node.manifest`.
4. **(deferred)** Anything resembling a planner remains a separate
   tool, NOT a runtime feature, until at least Gate 2.

Each of P1, P2, P3 is a 1-commit unit of work that preserves
every invariant above. None require touching SOL or the
admission pipeline.

## See also

- [`docs/architecture.md`](architecture.md) — the peer + admission
  model.
- [`docs/flows-and-sol.md`](flows-and-sol.md) — SOL semantics; what
  a planner would have to produce.
- [`docs/coordination.md`](coordination.md) — the Task layer a
  planner consults to know what's been tried.
- [`crates/relix-core/src/capability.rs`](../crates/relix-core/src/capability.rs)
  — the descriptor type.
- [`docs/current-limitations.md`](current-limitations.md) — what
  discovery does NOT do today.
