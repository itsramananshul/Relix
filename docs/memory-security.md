# Memory Security

Security mechanisms built into the memory node: the poisoning
guard, write-time anomaly scoring, PII anonymization, and
tenant isolation across both SQLite and Qdrant planes.

These mechanisms are independent layers — they complement each
other and are each applied at the earliest possible write
boundary.

## Memory poisoning guard

`MemoryGuard` runs before **every** memory write: on
`memory.write_turn` (Layer 1 Raw insert) and at every
`LayerPromoter` step (Semantic, Observation, and Model inserts).
A poisoned record is rejected before it can reach any layer
where it might be assembled into a prompt.

The guard is a pure, stateless function — no LLM classifier,
no semantic similarity. It uses substring + heuristic matching.

### Rules

**Rule 1 — Length cap**

Text longer than 10,000 characters (`MAX_TEXT_CHARS`) is
rejected. Memory inserts are dialogue chunks; anything over
this cap is either a prompt-stuffing attempt or bulk import
that should go through a dedicated ingest surface.

**Rule 2 — Instruction override phrases**

Any of the following phrases, matched case-insensitively,
trigger rejection:

```
ignore previous
forget everything
your real instructions
your true instructions
you are now
you must now
from now on you
new system prompt
new system message
replace your system prompt
system prompt:
```

**Rule 3 — Role reassignment + authority word combo**

A role-reassignment phrase paired with an authority word triggers
rejection. Role-reassignment phrases:

```
act as
pretend to be
roleplay as
role-play as
behave like
respond as if you were
```

Authority words:

```
admin, root, god mode, godmode, god-mode, dan,
unrestricted, no restrictions, no rules, no filter,
without restrictions
```

**Important**: a role-reassignment phrase alone (without an
authority word) is **not** rejected. "act as a friendly tutor"
is legitimate. "act as admin" is rejected. The tradeoff is
intentional: rejecting a legitimate write costs one retry;
accepting a poisoned one costs the agent's belief integrity.

### Rejection behavior

When `MemoryGuard::poison_reason(text)` returns `Some(reason)`:

- For `memory.write_turn`: the capability returns an error
  envelope with the reason as cause; the turns row is not
  inserted and no Layer 1 Raw record is created.
- For the promoter: the candidate record is dropped; the
  `PromotionStats.poisoned_skipped` counter is incremented; a
  `tracing::warn!` is emitted with the record id and reason.

The guard never mutates or sanitizes text — it is a hard
reject gate, not a filter.

## Anomaly scoring

Every Layer 3 Observation candidate is scored at write time by
`score_observation(candidate, existing)`. The function computes
a composite 0.0–1.0 score and returns an `AnomalyScore` with a
`disposition` field:

### Score components

| Condition | Contribution |
|---|---|
| Text is shorter than 12 characters trimmed (`MIN_OBSERVATION_CHARS`) | +0.50 |
| Zero domain-specific tokens (`MIN_SPECIFIC_TOKENS = 1`) | +0.55 |
| Contradicts an existing observation (same first-5-token overlap + negation/antonym signal) | +0.50 |

Score is clamped at 1.0.

### Dispositions

| Score range | `AnomalyAction` | Effect |
|---|---|---|
| >= 0.85 (`REJECT_THRESHOLD`) | `Reject` | Never stored; logged as warn |
| >= 0.55 (`QUARANTINE_THRESHOLD`) | `Quarantine` | Stored in `memory_quarantine`; requires operator approval |
| < 0.55 | `Accept` | Inserted into `memory_records` as Layer 3 |

**External-trust content** from `memory.ingest_document` and
`memory.ingest_image` is tagged `source_trust: external`.
External observations are quarantined at the lower
`QUARANTINE_THRESHOLD = 0.55` regardless of origin — operators
must explicitly approve externally-sourced observations before
they enter the live record store.

### Quarantine operator workflow

```
memory.quarantine_list  { limit?, source? }   → JSON list of pending rows
memory.quarantine_approve { id }               → re-insert into memory_records
memory.quarantine_reject  { id }               → permanent deletion
```

Quarantine rows never auto-expire. See
[`four-layer-memory.md`](four-layer-memory.md) for the full
quarantine workflow.

## PII anonymization

The memory node integrates `crate::training::PiiDetector` and
`PiiAnonymizer`. Configuration:

```toml
[memory.pii]
enabled  = false          # false = all paths are pass-through
strategy = "redact"       # redact | pseudonymize | allow

# Per-entity-type strategy overrides (optional)
[memory.pii.overrides]
EMAIL = "pseudonymize"
PHONE = "redact"
```

When `enabled = false`, the anonymizer is a no-op on every path.
When `enabled = true`, PII scrubbing runs at:

1. **`memory.write_turn`** — `body` is scrubbed before the
   `turns` insert and before the Layer 1 Raw record is created.
2. **`LayerPromoter`** — text is scrubbed before Semantic,
   Observation, and Model record inserts.
3. **`EmbeddingPipeline`** — defensive pass on text before the
   embed call and before the Qdrant payload is written. This
   catches records that arrived via a path that bypassed
   rule 1 (defense-in-depth).
4. **`memory.bulk_anonymize`** — operator migration pass that
   scrubs all existing text in both the Hermes `turns` /
   `agent_memory` tables and the four-layer `memory_records`
   table.

Strategy semantics:

| Strategy | Effect |
|---|---|
| `redact` | Replace detected span with a placeholder, e.g. `[EMAIL]` |
| `pseudonymize` | Replace with a deterministic pseudonym (same input → same pseudonym) |
| `allow` | Pass-through; entity is detected but not modified |

### PII capability surface

| Capability | Notes |
|---|---|
| `memory.pii_scan` | Always registered; scans arbitrary text; not gated on `enabled`. Returns detected spans and count. |
| `memory.anonymize_preview` | Always registered; applies the requested strategy to a text sample without persisting anything. Useful for testing before enabling production anonymizer. |
| `memory.bulk_anonymize` | Requires `[memory.pii] enabled = true`; returns `invalid_args` otherwise. Idempotent: re-running after all records are clean produces zero changes. |

Bridge HTTP surface:

| Method | Path | Capability |
|---|---|---|
| POST | `/v1/memory/pii/scan` | `memory.pii_scan` |
| POST | `/v1/memory/pii/preview` | `memory.anonymize_preview` |
| POST | `/v1/memory/pii/bulk_anonymize` | `memory.bulk_anonymize` |

## Tenant isolation

The memory node implements two independent isolation planes for
multi-tenant deployments. Both default to off.

### SQLite plane (LayeredMemoryStore)

Opt-in: `LayeredMemoryStore::open_with_tenant_isolation(path, true)`.

When enabled, all four-layer record operations use
tenant-scoped method variants:

| Tenant-blind (maintenance) | Tenant-aware (caller paths) |
|---|---|
| `text_search` | `text_search_for_tenant(tenant_id)` |
| `get` | `get_for_tenant(tenant_id)` |
| `list` | `list_for_tenant(tenant_id)` |

Tenant-aware methods filter on the `tenant_id` column. Calls
with `tenant_id = None` or `tenant_id = ""` when isolation is
enabled return `LayeredMemoryError::MissingTenant` immediately
(fail closed). They are never routed to a shared fallback.

The Hermes `agent_memory` table has **no** `tenant_id` column —
it uses `subject_id` as its sole primary key. Multi-tenant
deployments must scope subject IDs per tenant at the identity
layer.

The `memory_embeddings` table does have a `tenant_id` column
(added by GROUP 6 tenant isolation work):

- `insert()` writes `tenant_id = "default"`.
- `insert_for_tenant(tenant_id)` writes the caller's verified
  tenant.

### Qdrant plane

Opt-in: `[memory.qdrant] tenant_isolation = true`.

When enabled, each tenant gets its own Qdrant collection:
`{collection_prefix}_{sanitized_tenant_id}`.

`sanitize_tenant_id` rules:

1. Non-alphanumeric, non-underscore characters → `_`.
2. Leading/trailing `_` characters are trimmed.
3. Empty or all-separator input → `"default"`.
4. Truncate to 63 characters (Qdrant collection name limit).

Examples (with `collection_prefix = "relix"`):

| `tenant_id` | Collection name |
|---|---|
| `acme` | `relix_acme` |
| `acme-corp` | `relix_acme_corp` |
| `acme/tenant.1` | `relix_acme_tenant_1` |
| `a:b@c#d$e%` | `relix_a_b_c_d_e` |
| (empty) | `relix_default` |
| (all separators, e.g. `///`) | `relix_default` |

`QdrantClient::collection_for_tenant(tenant_id)` fails closed:
- `tenant_id = None` or `tenant_id = ""` when
  `tenant_isolation = true` → `QdrantError::MissingTenant`.
- Never routed to the shared `collection` fallback.

The embedding pipeline groups Qdrant upserts by tenant bucket
before each tick. Records with a missing `tenant_id` when
isolation is enabled are **skipped** with a `tracing::warn!` —
never written to a cross-tenant collection.

### Fail-closed guarantees

| Plane | Missing tenant behavior |
|---|---|
| SQLite (LayeredMemoryStore) | `LayeredMemoryError::MissingTenant`; no read or write proceeds |
| Qdrant | `QdrantError::MissingTenant`; embedding pipeline skips record + logs WARN |

Both planes are fail-closed: the system returns an error rather
than silently routing cross-tenant data.

## Summary — write path stack

Every write to the memory node traverses this security stack in
order:

```
1. MemoryGuard::poison_reason()
      → reject if > 10k chars or instruction-override / role-reassignment phrases
2. PiiAnonymizer::anonymize()  (if [memory.pii] enabled = true)
      → scrub detected PII per configured strategy
3. score_observation()  (Layer 3 candidates only)
      → Accept / Quarantine (>= 0.55) / Reject (>= 0.85)
4. Tenant scope check  (if isolation enabled)
      → fail closed on missing tenant_id
5. Insert into memory_records / turns / agent_memory
6. EmbeddingPipeline defensive PII pass  (defense-in-depth re-scrub before Qdrant write)
```

A record that clears all six gates is considered safe to store
and to surface via prompt injection.

## See also

- [`four-layer-memory.md`](four-layer-memory.md) — quarantine
  workflow, anomaly scoring context, four-layer promoter.
- [`vector-memory.md`](vector-memory.md) — Qdrant tenant
  configuration, embedding pipeline.
- [`memory.md`](memory.md) — full capability index.
- [`security.md`](security.md) — system-wide security model.
