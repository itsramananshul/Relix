# Relix Gap Report — Roadmap Claims vs Actual Code

**Audit date:** 2026-05-28
**Author:** ground-truth audit pass — read both roadmaps in full, scanned the codebase, ignored every status tag and commit hash, verified each claim against actual files / types / capabilities.

**Sources of truth:**
- `docs/RELIX_ROADMAP.md` (5253 lines, current)
- `docs/old RELIX_ROADMAP.md` (4099 lines, pre-rewrite snapshot)
- Codebase under `crates/`

**Method:** For every feature mentioned in either roadmap, I located the actual module / file / capability / endpoint / CLI command that backs it. Sub-agents searched in parallel, all findings cross-checked with direct grep.

**Scope of this report:** Every roadmap entry that is mislabeled, partially implemented, or missing entirely. Sections where claim and code match are noted briefly at the end and not enumerated.

---

## Severity legend

- **MISLABELED [DONE]:** Roadmap says `[DONE]` (with commit hash); code is materially incomplete or absent.
- **PARTIAL DONE:** Roadmap claims `[DONE]` or `[PARTIAL]`; some code exists but documented sub-features are missing.
- **MISSING:** Roadmap describes something that has no implementation at all.
- **CONSISTENT SKIP:** Roadmap says `[SKIPPED]` and the code genuinely has nothing — listed for completeness, not a gap in execution.

---

## GAP 1 — §7.17 Backend SDK: Python + TypeScript SDKs — CLOSED

**Closed in commits 29d25e9 (Python) + 3d1317d (TypeScript).**

- **Python SDK** (`29d25e9`): `sdks/python/` ships a production-quality package wrapping the Relix web bridge's HTTP surface. Public surface: `RelixClient` with both sync (`chat`) and async (`achat`) variants of every method; sub-APIs `client.memory` (search / ingest_document / dialectic / flush_context), `client.planning` (plan / agents / search_agents / validate), `client.skills` (search / stats / get), `client.observability` (health / alerts / alert_history). Streaming returns `Iterator[StreamChunk]` / `AsyncIterator[StreamChunk]` with a buffer-carry SSE parser that tolerates LF/CRLF separators and bytes split across `iter_text` boundaries. Bearer auth + `X-Relix-Tenant` propagation per request; typed exception hierarchy (`RelixError` / `Connection` / `Auth` / `Response` / `Timeout`). httpx + pydantic v2; Python 3.10+. 30 pytest tests passing via respx-mocked httpx; README documents every sub-API.
- **TypeScript SDK** (`3d1317d`): `sdks/typescript/` ships `@relix/sdk` using native Node 18+ `fetch` (no axios, no node-fetch). Mirror surface of the Python SDK. Streaming via `eventsource-parser` for correct framing across split-byte chunks. Strict TypeScript with `noImplicitAny` + every public type exported; no `any` in the source. 28 jest tests passing using a tiny in-tree FetchMock (the SDK accepts a `fetch` override on `RelixClientOptions` so tests inject without monkey-patching globals); README documents the full surface.

Both SDKs target the same wire contract the Rust SDK consumes, so a polyglot deployment sees consistent behaviour. The Rust `relix-sdk` crate continues to ship unchanged.

---

## GAP 2 — §7.17 / 5.7.3 "Embeddable Mode" (`relix-embedded` crate) — CLOSED

**Closed in commit 44f83d0.**

`crates/relix-embedded/` ships an in-process runtime for developers who want Relix capabilities embedded in their own Rust application. The crate exposes a `RelixEmbedded` struct (clone-able) constructed via a builder; the builder requires an AI provider (any `Arc<dyn ChatProvider>` impl — MockProvider, OpenAICompatibleProvider for Ollama / OpenAI / OpenRouter / xAI, Anthropic, Gemini, or a custom impl) and optionally takes a SQLite memory-db path (defaults to `:memory:`).

Three operations:
- `chat(ChatInput) -> ChatResponse` — renders a per-session conversation history from an in-process 20-turn ring, calls the configured provider, then persists both turns to the memory store as Layer-1 `Raw` records. SQLite write failures are logged but do not invalidate the reply that already came back.
- `memory_ingest_document(MemoryIngestInput) -> MemoryIngestResult` — paragraph chunker with 100-char overlap; writes Layer-2 `Semantic` records keyed by `subject_id`. Capped at 5,000 chunks per call.
- `memory_search(MemorySearchInput) -> Vec<MemoryHit>` — `text_search` (SQLite `LIKE`) with optional `subject_id` filter.

What's deliberately NOT included (and the reason): libp2p mesh networking, the web bridge HTTP server, the CLI, multi-node federation, and Qdrant vector search. Embedded mode is for single-process apps — the moment a host app needs cross-process orchestration, it runs the full mesh instead.

**Honest deviation from the roadmap text**: the roadmap suggested adding an `embedded` feature flag to `relix-runtime` that "gates out the libp2p transport, mesh client, and web bridge". This commit does NOT add such a feature flag. Threading a feature flag through `relix-runtime` would touch ~150 files cross-cutting; the resulting refactor was scoped out as multi-day cross-cutting work. Instead `relix-embedded` consumes the runtime's existing public surface (`LayeredMemoryStore`, `ChatProvider`, `MockProvider`, `OpenAICompatibleProvider`, etc.) and bypasses libp2p by simply never instantiating a `DispatchBridge` or `MeshClient`. Embedded callers compile the same `relix-runtime` binary as the full mesh — they just exercise less of it. Adding a true `--features embedded` opt-in is a future follow-up if the embedded use case justifies the cross-cutting refactor.

11 integration tests in `crates/relix-embedded/tests/embedded_smoke.rs` + 1 doctest passing.

---

## GAP 3 — §7.20 SKILL.md + AGENTS.md Compatibility: partial

**Roadmap claim (`[DONE — commit 0dcad1e]`):**
> 1. SKILL.md reader …
> 2. AGENTS.md reader …
> 3. SKILL.md writer …
> 4. Skill discovery endpoint — `GET /v1/skills` returns all skills available to the current agent …
> 5. CLAUDE.md and .cursorrules compatibility — Relix reads these files when present so agents working in codebases that already have Claude or Cursor configuration can pick up that context automatically.

**Actual code:**
- `crates/relix-runtime/src/nodes/ai/skills.rs` exists and has skill discovery + `discover_agents_md()` (walks up 5 dirs).
- **`GET /v1/skills` endpoint:** NOT registered anywhere in `crates/relix-web-bridge/src/main.rs`.
- **SKILL.md writer:** No code path emits SKILL.md.
- **CLAUDE.md / .cursorrules loaders:** No code reads these files.

**Severity:** PARTIAL DONE. Readers ship; writer + HTTP endpoint + Claude/Cursor support do not.

**Gap size:** Small-to-medium — about a day of writer work + an endpoint, plus design for which CLAUDE.md / .cursorrules to scan.

---

## GAP 4 — §7.21 Auto-Skill Generation — **CLOSED (0bac31e + e47dab2)**

**Roadmap claim (`[DONE — commit 10932cb]`):**
> When an agent successfully completes a non-trivial task, it automatically crystallizes what it learned into a reusable skill. … skill confidence scoring … skill versioning … skill refinement over time … skill sharing across agents.
>
> New capabilities: `memory.skill_search`, `memory.skill_store`, `memory.skill_update`. New bridge endpoints: `GET /v1/skills`, `GET /v1/skills/{id}`, `POST /v1/skills/import`, `DELETE /v1/skills/{id}`. `/skill list / show / edit / delete / export / import / stats` from CLI and channels.

**Closure (commits 0bac31e + e47dab2):**

`0bac31e — feat(ai): GAP 4 part 1+2 — SkillStore + auto-extraction pipeline`:
- `nodes/ai/skill_store.rs` — SQLite-backed `SkillStore` with `skills` + `skill_versions` tables (schema matches the spec verbatim), standard relix pragmas, versioned migrations. CRUD + search + version-aware update + FIFO example cap + stats + refinement-candidate query. 21 unit tests.
- `nodes/ai/skill_extractor.rs` — 5-stage pipeline: complexity scoring (response > 200 words +0.3, tool calls +0.2, structured output +0.2, duration > 3s +0.1, session > 3 turns +0.2; floor 0.6) → duplicate check (cosine >= 0.85 bumps usage, no new skill) → LLM synthesis (strict JSON, name <= 40 chars snake_case, description <= 120 chars, 2-6 steps, 2-5 tags) → validation → insert. Non-blocking spawn from the AI handler; failures never panic. 17 unit tests.
- `LocalProviderAiDispatcher` / `LocalProviderEmbedDispatcher` adapters route the synthesis + dedup calls through the local `ChatProvider` — no libp2p hop, no recursion through `ai.chat`.

`e47dab2 — feat(ai, bridge, cli): GAP 4 part 3+4+5+6 — refinement, caps, bridge, CLI`:
- `nodes/ai/skill_refinement.rs` — `record_usage(skill_id, UsageOutcome)` confidence updates (liked +0.05, success +0.01, failed -0.10; clamped to [0.05, 0.95]). Background refinement task (default 24h tick) pulls eligible candidates and asks the LLM to suggest improvements; only writes a new version row when the steps actually differ. 13 unit tests.
- `nodes/ai/skill_caps.rs` — six caps `memory.skill_search / get / store / update / deprecate / stats` registered on the AI controller's DispatchBridge. 12 unit tests.
- Bridge — `GET /v1/skills`, `GET /v1/skills/stats`, `GET /v1/skills/:id`, `POST /v1/skills`, `PATCH /v1/skills/:id`, `POST /v1/skills/:id/deprecate`. INVALID_ARGS → 400, SECURITY_DENIED → 403.
- CLI — `relix skills list` extended with `--query / --agent / --min-confidence / --limit` (switches to bridge mode). New subcommands `show`, `edit`, `delete`, `export`, `import`, `stats`.
- Controller wiring — `SkillsRuntime` bundle constructed via `build_skills_runtime`. `[skills] enabled + db_path` is the trigger; `auto_extract` and `refinement_enabled` are independent flags.

---

## GAP 5 — Part 6 Layered Memory: four spec capabilities missing — **CLOSED (3c9f3ec)**

**Roadmap claim (Part 6, `[DONE — commits 41ad328 through 406a995]`):**
> Add `memory.ingest_document`, `memory.ingest_image`, `memory.dialectic` capabilities. Add `memory.context_flush` capability. … Document Ingestion API … New bridge endpoint: `POST /v1/memory/ingest`. New CLI command: `relix memory ingest --subject user-123 --file ./notes.md`. … Multimodal Support — Text → `nomic-embed-text` via Ollama, Images → `nomic-embed-vision` via Ollama.

**Closure (commit 3c9f3ec, feat(memory): GAP 5 — dialectic / ingest_document / ingest_image / context_flush):**
- `memory.dialectic` registered with Qdrant-first / text-fallback retrieval, dispatched via the existing `ai.chat` peer, default model `openrouter/anthropic/claude-3-5-haiku` (overridable via `[memory.curator] dialectic_model`).
- `memory.ingest_document` registered, supports text / markdown / code / pdf (lopdf). blake3-stable chunk IDs make re-ingest idempotent. Graceful embedding-failure path surfaces `deferred_embeddings`.
- `memory.ingest_image` registered, vision-embed via the standard `EmbeddingDispatcher` `image/base64;…` wire format; PDFs route through the same lopdf pipeline.
- `memory.context_flush` registered with `flushed` column on the turns table, `keep_recent_n` default 5, idempotent re-runs.
- Bridge endpoints `POST /v1/memory/dialectic` / `/ingest` / `/ingest_image` / `/context_flush` wired in `crates/relix-web-bridge/src/memory_gap5.rs`.
- CLI subcommands `relix memory dialectic / ingest / ingest-image / flush` wired in `crates/relix-cli/src/ops.rs`.
- 21 unit tests across `dialectic.rs`, `ingest.rs`, `context_flush.rs`.

---

## GAP 6 — Memory Security poisoning defense — **CLOSED (80980e1)**

**Roadmap claim (`[DONE — commit 7e8ccc5]`):**
> 1. Source attribution on every memory record …
> 2. Write-time anomaly scoring — before writing any observation to Qdrant, score it for anomalousness …
> 3. Low-trust source quarantine — observations derived from ingested external content … are tagged `source_trust: external`. They go into a quarantine layer and require user confirmation before being promoted to the main observation store.
> 4. Periodic memory integrity audit — scheduled job that re-reads the observation and model layers …
> 5. Memory inspector UI …

**Closure (commit 80980e1, feat(memory): GAP 6 — anomaly scorer, quarantine flow, integrity auditor):**
- `nodes/memory/anomaly.rs` — write-time AnomalyScorer with three signals (short-message ≥0.5, low-specificity ≥0.55, contradiction ≥0.5). Reject ≥0.85, Quarantine ≥0.55, Accept otherwise. Pure function; 11 unit tests.
- `nodes/memory/quarantine.rs` — three JSON-wire caps `memory.quarantine_list / approve / reject`, bridge endpoints `/v1/memory/quarantine/list|approve|reject`, CLI subcommands `relix memory quarantine-list|approve|reject`.
- `nodes/memory/integrity.rs` — `MemoryIntegrityAuditor` spawned every 24h. Three checks per tick: contradiction sweep (symmetric), observations/models with empty source, sources with stale (>30d) observations and no Layer-4 model. WARN/INFO tracing lines; 5 unit tests.
- Promoter hook in `nodes/memory/promoter.rs`: every extracted observation is scored against existing valid observations on the same source; quarantine/reject paths update the new `quarantined` and `anomaly_rejected` StageOutcome counters and carry inherited `source_trust`.
- Schema: `source_trust` enum column, `memory_quarantine` table, `column_exists`-guarded migrations from the GAP-4 pass.

---

## GAP 7 — Memory Inspector editing surface — **CLOSED (e39a079)**

**Roadmap claim (`[DONE — commit 35e49c8]`):**
> Edit wrong observations directly. Delete individual observations — cascades to refresh the living model. Freeze an observation so the curator never overwrites it. Scope memories to contexts ("only use this in personal chats, not work chats"). Export full memory as JSON for portability. Request a full model refresh on demand.

**Closure (commit e39a079, feat(memory): GAP 7 — inspector edit / freeze / export / refresh-model):**
- `memory.edit_record {id, text}` — anonymizer-clean, clears embedding pointer so the background pipeline re-embeds on next tick.
- `memory.freeze_record` / `memory.unfreeze_record` — flip the `frozen` column added in GAP-4.
- `memory.bulk_export {source, layer?}` — full JSON export of every record for one source, optionally narrowed to one layer.
- `memory.request_model_refresh {source}` — ages the latest Layer-4 model past `MODEL_THROTTLE_SECS` so the next promoter tick regenerates without losing content.
- Bridge endpoints `/v1/memory/records/edit|freeze|unfreeze`, `/v1/memory/export`, `/v1/memory/refresh_model`.
- CLI `relix memory edit-record|freeze-record|unfreeze-record|export|refresh-model`.
- 8 unit tests; every test verifies SQLite state, not just the response body.

**Out of scope (deferred):**
- **Scope to context** (the spec's "only use this in personal chats, not work chats" pattern): not landed. Requires a `scope` column + per-call scope filter; left to a follow-up that also picks the scope vocabulary (free-form tags vs enum).
- **Hard delete cascade** (vs the existing `invalidate` flip): the inspector still uses invalidate. A hard delete adds blast-radius questions (cascaded Qdrant point deletes, knowledge-share fan-out) that warrant their own commit.

---

## GAP 8 — Memory Consolidation Strategy — **CLOSED (0e6fd5e)**

**Roadmap claim (`[DONE — commit fe98f9d (layer promotion curator v2)]`):**
> Raw turns that are fully captured in observations can be marked `consolidated = true` in SQLite. … Observations that are fully captured in the current living model can be archived — moved to a lower-priority Qdrant segment with lower retrieval weight. … Consolidation only runs on terminal observations — ones that haven't been updated in >30 days and have `confidence > 0.85`. … A `task.snapshot`-style consolidation event is written when a batch is archived.

**Closure (commit 0e6fd5e, feat(memory): GAP 8 — ConsolidationArchiver background task):**
- `nodes/memory/archiver.rs` — `ConsolidationArchiver` spawned every 6h.
- Layer-3 archive criteria: valid + observed_at > 30d + not frozen + not already archived + covered by a Layer-4 model with a newer observed_at on the same source (schema-level proxy for "confidence ≥ 0.85").
- Layer-1 cascade: raw rows whose source's observations are all archived get stamped `consolidated = true`.
- Side effects per archived record: `archived` tag (idempotency), `valid_to = now` (hide from default views).
- Structured tracing INFO line `event = memory.archiver.run` carries the per-tick counts — that's the chronicle channel for the alpha.
- 8 unit tests covering empty store, fresh observation, observation without model, the archive happy path, frozen-skip, raw consolidation, partial archive (no consolidate), and idempotent re-runs.

**Out of scope (deferred):**
- **Separate low-priority Qdrant segment**: the alpha's single-collection Qdrant deployment makes this infeasible without a breaking schema change. The `archived` tag is the filter operators apply at search; the `memory_records_archive_scan` index keeps the filter cheap.
- **`task.snapshot`-style consolidation event**: emitted as a structured tracing line, not a chronicle record. A dedicated chronicle channel would require a new memory-event surface in the coordinator and is left for the next coordinator pass.

**Gap size:** Medium — schema column, archive Qdrant segment, scheduler, event writes.

---

## GAP 9 — §7.7 (sub-bullet) Email dashboard panel

**Roadmap text under §7.7 explicitly says:**
> **Not shipped this session (documented gaps): Dashboard panel for the email channel** — like every other channel, the email node ships `email.status` / `email.messages_recent` as read-only capabilities the bridge proxies for the dashboard, but the actual dashboard tile rendering them sits in the same multi-week dashboard-redesign work …

**Status:** Consistent — explicitly called out as not shipped. Listed here so the reader sees the email feature isn't fully complete despite the section-header `[DONE]`.

**Severity:** PARTIAL DONE (documented inside the section).

---

## GAP 10 — §7.23 Perception Tools: 2-of-6 shipped, marked PARTIAL (consistent)

**Roadmap claim (`[PARTIAL — Browser shipped 26e3ec9; Audio shipped 19484c7; four remaining sub-tools SKIPPED]`):**

**Actual code:**
- `tool.browser` (Playwright backend): PRESENT (`nodes/tool/browser/`).
- Audio transcription via Whisper-via-Ollama: PRESENT (`nodes/tool/audio.rs`).
- `tool.parse_document` (LlamaParse / Docling / PyMuPDF tiering): **NOT present**. `nodes/tool/pdf.rs` exists but is a lopdf-based plain extractor, not the tiered cloud / local document-parser surface the spec describes.
- `tool.web_read` (Crawl4AI / Jina / Firecrawl): **NOT present**. `web_extract.rs` exists for HTML parsing but is not the cleaner JS-rendered Markdown surface.
- `tool.screen` (window capture / accessibility tree / click): **NOT present**.
- Perception Security two-stage isolation pipeline (extraction model separate from planning model): **NOT present**.

**Severity:** CONSISTENT — the roadmap correctly marks this PARTIAL.

**Gap size:** Large — four substantial tool integrations each with their own external dependencies (Stagehand / LlamaParse / Crawl4AI / Anthropic computer-use).

---

## GAP 11 — §7.26 Component 5 Transactional Action Gateway — **CLOSED (235a32b)**

**Roadmap claim (`[DONE — commit 663c737]`):**
> The gateway operates in three tiers:
> Tier A — Auto-compensated actions … Tier B — Human-rollback-plan actions … Tier C — Flat-out blocked actions.
> Idempotency keys across all tiers. Dry-run preview across Tiers B and C.

**Closure (commit 235a32b, feat(execution): GAP 11 — three-tier transactional action gateway):**
- `nodes/execution/gateway_tier.rs` — `GatewayTier::{AutoCompensated, HumanRollbackPlan, Blocked}` enum + `GatewayDispatchOptions` builder (transaction_id, idempotency_key, tier, dry_run, actor) + `DryRunPreview` + `RollbackResult` shapes.
- `nodes/execution/transaction_store.rs` — SQLite-backed `gateway_actions` table with a unique partial index on `(tool, idempotency_key)` so duplicate keys fail loudly. CRUD + `find_by_idempotency_key` + `mark_rolled_back`. Stable `g.<16hex>` and `tx.<16hex>` id formats.
- `nodes/execution/rollback.rs` — `execute_rollback(...)` walks the transaction newest-first, runs Tier A compensating calls, surfaces Tier B plans, errors on persisted Tier C rows. `execution.rollback` + `execution.transaction_get` caps registered on the coordinator DispatchBridge.
- `nodes/tool/dispatcher.rs` — new `dispatch_with_options(...)` consults Tier C lists (config + per-call), dedupes on idempotency keys, short-circuits to a `DryRunPreview` when `dry_run = true`, persists every successful + failed dispatch to the store. Legacy `dispatch(...)` unchanged.
- Bridge endpoints `POST /v1/execution/rollback`, `GET /v1/execution/transactions/:id`. CLI subcommands `relix execution rollback / transaction / evidence`.
- `[execution.gateway]` config block: `dry_run`, `db_path`, `blocked_tools`, `evidence_db_path`.
- 25 new unit tests across the three modules; 8 new dispatcher tests.

---

## GAP 12 — §7.26 Component 3 Evidence Capture — **CLOSED (5aacced)**

**Roadmap claim (Component 3, embedded under `[DONE]`):**
> Every action the executor runs produces a structured evidence record. Not a text log — a machine-readable artifact that captures the full before/after state.

**Closure (commit 5aacced, feat(execution): GAP 12 — structured evidence records):**
- `nodes/execution/evidence.rs` — SQLite-backed `evidence_records` with the spec's full column list (evidence_id, action_id, actor_id, tenant_id, tool, arguments_redacted, policy_decision, reversibility, tier, started/completed/duration, cost_usd, state_before, state_after, diff, error, recorded_at_ms). Three indexes: action_id, (actor_id, recorded_at_ms DESC), (tool, recorded_at_ms DESC).
- `EvidenceStore` implements the `EvidenceCaptureSink` trait the GAP-11 dispatcher declares. Every `dispatch_with_options` call produces one evidence row.
- `StateProbe` trait — tools that can snapshot pre/post state register a probe. When wired, the row carries `state_before` + `state_after` + a pure-Rust `unified_diff(a, b)` string.
- PII anonymisation: every `arguments_redacted` field runs through the configured `PiiAnonymizer` before storage.
- `execution.evidence` capability registered on the bridge. Bridge endpoint `GET /v1/execution/evidence` with `?action_id=` / `?actor_id=` / `?limit=` query params.
- 10 unit tests covering capture, redaction, action / actor filters, state probe + diff, dry-run + blocked policy decisions, diff edge cases, evidence-id shape, failed dispatch error capture.

**Out of scope (deferred):** screenshot capture for browser actions and test-outcome attachment for runners — the spec mentions both, but they need browser-specific + runner-specific instrumentation that lives in separate crates; the StateProbe interface gives those future commits a clean hook without re-touching the gateway.

---

## GAP 13 — §7.31 Provenance Registry — **CLOSED (c94f75a)**

**Roadmap claim (`[DONE — commits e16309e through 2f0ba25]`, Feature 4):**
> Every trace links back to exactly what was running when it ran. … `ProvenanceRegistry` stores: Every version of every system prompt, policy file, and tool manifest … Traces link to hashes. Queries join through the registry.

**Closure (commit c94f75a, feat(ai, observability, bridge, cli): GAP 13 + 14 — provenance writes from AI handler, two-sink observability for mesh-internal calls, prompt + manifest auto-versioning, relix provenance CLI):**
- `nodes/ai/provenance_hooks.rs` — `record_chat_provenance(...)` writes a ProvenanceSnapshot after every `handle_chat` AND `handle_chat_stream` completion. The payload mirrors the W8 bridge layout exactly so the diff endpoint sees identical field names from either entry point.
- `record_prompt_file_load(obs, path, content)` and `record_tool_manifest_register(obs, name, json)` — auto-versioning helpers. Trace ids derive from the content hash so unchanged content is idempotent; a changed file mints a new trace id.
- `record_soul_provenance(...)` is invoked at controller boot so the registry knows what's active before the first chat call.
- New `ProvenanceRegistry::list_recent(limit)` + bridge `GET /v1/provenance/recent?limit=200`.
- CLI `relix provenance show / diff / history / audit` (4 subcommands). `history` filters `prompt_file_load` snapshots by path + ISO date range; `audit` lists every snapshot in a time range.

**Out of scope (deferred):** the spec mentions a `policy file` auto-versioning leg alongside the prompt file + tool manifest legs; the existing policy plumbing already carries a `policy_version` string per call, so the deliberate decision here was to leave it as-is rather than build a second hashing pipeline. The `relix provenance diff` surface already shows `policy_version` changes.

---

## GAP 14 — §7.32 Wiring W8: observability metadata in AI handler — **CLOSED (c94f75a)**

**Roadmap claim (`Wiring Gaps … W8 — Provenance Not Recorded On Every Chat Call [DONE — commit 917a70e]`):**
> `ProvenanceRegistry` is on `AppState` but `record_chat_observability` in `openai.rs` does NOT write a provenance snapshot. Fix: after every `/v1/chat/completions` call, record a `ProvenanceSnapshot` with model_id, system_prompt_hash from the request body.

**Closure (commit c94f75a, same commit as GAP 13):**
- `record_chat_metadata(obs, session, trace, agent, event_type, model, duration_ms, tokens, success)` writes one Sink-A `MetadataEvent` after every `handle_chat` and `handle_chat_stream` completion. `event_type` is `"ai.chat.complete"` for the unary path and `"ai.chat.stream.complete"` for the streaming path.
- `[observability.two_sink]` config block: `enabled`, `metadata_db_path`, `content_db_path?`, `provenance_db_path?`, `content_retention_days`. When paths are unset, derived from the metadata path.
- `build_ai_observability(cfg)` opens the three sinks and returns an `Arc<ObservabilityContext>` plumbed into `nodes::ai::register`.
- Sink B is intentionally `None` on the mesh-internal path — `ai.chat` content lands in Sink B from the bridge's W8 path when the bridge boundary is involved; mesh-internal calls do not duplicate.
- 2 integration tests in `nodes/ai/mod.rs`: `handle_chat_records_provenance_and_metadata_when_observability_wired` and `handle_chat_skips_observability_when_no_context` (regression guard that the absent-context path is a true no-op).

---

## GAP 15 — §7.30 Identity & Permissions: explicitly SKIPPED but a partial implementation exists

**Roadmap claim (`[SKIPPED — three "build now" components … each requires its own cross-cutting cryptographic + policy review and are not single-session deliverables; deferred]`):**
> Component 1 — Out-of-Band Approval. Component 2 — Credential Lifecycle Management. Component 3 — Lightweight Session Identity.

**Actual code:**
- The roadmap's own text under SKIPPED says: "Component 1 partially covered today by the agent-gate + approval flow + telegram /approve wiring (commits e152c62 / dda09f6)".
- Agent gate + approval store DOES exist (`crates/relix-runtime/src/admission/agent_gate.rs`, agent.rs handlers, telegram approval wiring).
- **`relix credentials list / rotate / revoke / audit` CLI**: NOT present. No credentials.rs in CLI.
- **Session identity tokens**: NOT present (no `sessions.rs`, no scoped per-session JWT issuance, no auto-revoke at session end).
- **Out-of-band approval `[approval]` config block + always-require list**: the planning approval flow ships, but a generic "always require approval for these methods regardless of confidence/judge" allowlist is not separately wired.

**Severity:** CONSISTENT — the SKIPPED tag is honest. The note is that the partial coverage applies only to planning approvals, not the full identity-and-permissions surface described.

**Gap size:** Large — all three components are described as multi-day cross-cutting builds.

---

## GAP 16 — §7.29 Reasoning Engine: SKIPPED (consistent)

**Roadmap claim (`[SKIPPED — four sub-components (smart model routing, real confidence measurement via log-prob sampling, belief state tracking, judge model) … deferred]`):**

**Actual code:**
- `HealthAwareRouter` exists in `nodes/ai/router.rs` — but this is the M69 health-router that picks between providers based on cached status, NOT the spec's complexity-tier router (simple / medium / complex).
- **Smart model routing (tier classification)**: NOT present. `[reasoning.router.tiers]` config block has no parser.
- **Real confidence (self-consistency + retrieval quality + judge scan)**: NOT present. The §7.19 confidence scorer is signal-based (length, coherence, finish_reason, logprob, error history, latency) but is NOT the three-signal aggregation the §7.29 spec describes.
- **Belief state tracking**: NOT present. No `belief.rs`, no belief-state SQLite table.
- **Judge model (5-question verdict)**: NOT present. No `judge.rs`, no second-model verification call before significant actions.
- **`relix models` command** with provider model-ID fetching: NOT present.

**Severity:** CONSISTENT — SKIPPED tag is honest.

**Gap size:** Very large — four distinct multi-day features + a model-resolution CLI.

---

## GAP 17 — §7.18 Research-Backed Identity System: SKIPPED (consistent)

**Roadmap claim (`[SKIPPED — requires external web-search API access … multi-week build plus external account credentials; deferred]`):**

**Actual code:**
- `memory.identity_create` / `identity_switch` / `identity_list` capabilities: NOT registered.
- `/identity` slash commands: NOT in channels.
- `~/.relix/identities/{id}.toml` storage: NOT used.
- `[identity]` config block: NOT parsed.

**Severity:** CONSISTENT — SKIPPED.

**Gap size:** Very large — multi-week research + synthesis pipeline.

---

## GAP 18 — Bi-Temporal Validity on Facts: SKIPPED (consistent)

**Roadmap claim (`[SKIPPED — requires schema migration on the layered memory store … deferred]`):**

**Actual code:**
- `valid_from` / `valid_to` columns on memory records: **partially present** — `valid_to` exists for invalidation. `valid_from` is not a separate field (records use `created_at`).
- `superseded_by` field: NOT present.
- Time-travel queries (`valid_from <= target_time AND (valid_to IS NULL OR valid_to > target_time)`): NOT implemented.
- Contradiction-detection write path that supersedes-rather-than-overwrites: NOT present.

**Severity:** CONSISTENT — SKIPPED.

**Gap size:** Medium — schema migration + query rewrites.

---

## GAP 19 — §7.6 Plugin Marketplace: SKIPPED (consistent)

**Roadmap claim:** Local plugin SDK + loader shipped (`c5af764`, `054e7b4`). Marketplace itself needs external infrastructure.

**Actual code:**
- `crates/relix-plugin-sdk/` PRESENT.
- Local plugin loader PRESENT.
- Hosted registry + signing-authority CA + payment processor: NOT present (correctly skipped).

**Severity:** CONSISTENT — SKIPPED.

---

## GAP 20 — §7.13 WebRTC + §7.14 Relix Cloud: SKIPPED (consistent)

Both explicitly skipped in the roadmap. No matching code. Severity: CONSISTENT.

---

## GAP 21 — §7.26 Component 7 Warm Sandbox: SKIPPED (consistent)

**Roadmap claim:** SKIPPED — requires Linux namespaces + cgroups + Windows Job Objects + Docker container pool + snapshot/restore primitives.

**Actual code:**
- `crates/relix-runtime/src/terminal_sandbox.rs` exists for tool.terminal command sandboxing (resource limits, output capture) — that's Wave 3 §3.2, not §7.26 Component 7.
- **Warm pool of pre-initialized native processes** (Linux/Mac): NOT present.
- **Persistent Docker container with fresh workspace per session** (Windows): NOT present.
- **Snapshot/restore mechanism**: NOT present — and this is the dependency that blocks §7.28 Feature 1's pause-and-resume.

**Severity:** CONSISTENT — SKIPPED.

---

## GAP 22 — §7.28 Documented NOT DONE sub-bullets (consistent)

The §7.28 ship explicitly enumerated three NOT-DONE sub-bullets in the roadmap:

1. **Feature 1 pause-and-resume state preservation** — depends on §7.26 Component 7 warm-sandbox snapshot primitives. Not built.
2. **Feature 2 provider-cost-spike + ask-human-rate drift alerts** — need rolling-baseline metric storage; the metrics store doesn't bucket per-provider cost time-series or per-method invocation-rate baselines yet.
3. **Feature 4 Presidio integration** — operator semantics match between the in-process `PiiDetector` and a hypothetical Presidio sidecar; full Presidio integration deferred.

**Status:** CONSISTENT — these are explicitly called out in the §7.28 section itself.

---

## GAP 23 — §7.17 Multi-tenant identity namespacing — CLOSED

**Closed in commits 7feed75 (23A), 1f4368d (23B), 447744a (23C).**

- **23A Per-tenant Qdrant collections** (7feed75): `[memory.qdrant] tenant_isolation = true` + `collection_prefix = "relix"` route every record into `{prefix}_{sanitized_tenant_id}`. The X-Relix-Tenant header flows through `RequestEnvelope.tenant_id` → `InvocationCtx.tenant_id` → embedder buckets by tenant → `QdrantClient.upsert_in / search_in / ensure_collection_in`. New collections auto-create on first write (memoised). `MemoryRecord.tenant_id` column added via additive ALTER TABLE migration. Bridge `memory_gap5` handlers extract `Extension<TenantId>` and forward via `build_request_with_tenant`. 8 unit tests + the full sweep passing.
- **23B Per-tenant policy resolution** (1f4368d): new `relix-core::policy::TenantPolicyResolver` resolves overrides from `{policy.dir}/{tenant_id}.policy.toml` with TTL-cached engines (positive + negative entries). Tenant ids are sanitised before file lookup so `../../etc/policy.toml` cannot escape `dir`. `DispatchBridge` admission consults the resolver when wired; falls back to the global engine otherwise. New caps `node.policy.tenant_list` + `node.policy.tenant_get`; bridge HTTP `GET /v1/policy/tenants` + `GET /v1/policy/tenants/:tenant_id`. 5 unit tests + 2 bridge parse tests.
- **23C Per-tenant audit partitioning** (447744a): `AuditDraft` gained additive `tenant_id` field; the canonical signed CBOR `AuditRecord` + hash chain are deliberately NOT touched (changing the signed struct would break every existing chain). New `relix-runtime::audit_partition::AuditPartitionStore` mirrors every finalised audit into SQLite keyed by sanitised tenant id. Bridge admits + writes the mirror BEFORE finalising the canonical log; mirror failures degrade to `warn!` and the signed chain still finalises. New caps `node.audit.tenant_list` + `node.audit.tenant_recent`; bridge HTTP `GET /v1/audit/tenants` + `GET /v1/audit/tenants/:tenant_id?limit=N`. 5 unit tests + 2 bridge parse tests.

**Honest follow-ups (deferred):**
- The canonical `AuditRecord` still does not carry `tenant_id` in its signed body — operators who need cryptographic per-tenant tamper-evidence have to verify the partition mirror's row against the canonical chain separately. Adding `tenant_id` to the signed struct is a chain-rotation event and was out of scope.
- `tenant_id` is plumbed onto memory caps via `memory_gap5`; other bridge handlers default to `None` tenant. The bridge dispatch path itself reads `req.tenant_id` correctly, so nothing is *broken* on those handlers — they just don't propagate the header. Cross-cutting plumb of every bridge handler is a follow-up.

---

## GAP 24 — `relix sessions` CLI — CLOSED

**Closed in commit 3b708f6.**

`crates/relix-cli/src/sessions.rs` ships three subcommands wired into `main.rs` as `Cmd::Sessions`:

- `relix sessions list [--agent A] [--status running|completed|stalled] [--limit N] [--json]` — forwards `--status` to `GET /v1/sessions`, filters `--agent` client-side, prints a table.
- `relix sessions show <session_id> [--full --elevated] [--json]` — pulls `GET /v1/sessions/{id}`; with `--full` also fetches each event's content from `/v1/sessions/{id}/content/{event_id}` (requires `X-Relix-Elevated`). Per-event content fetches that fail degrade to a `content_error` field rather than aborting the whole timeline.
- `relix sessions search --query Q [--agent A] [--limit N]` — substring-matches `session_id` + `agent_id` case-insensitively. The bridge has no server-side `/v1/sessions/search` today; richer server-side search is a follow-up. 4 new unit tests cover query matching, missing-field tolerance, urlencode round-trip, and the default limit guard.

---

## GAP 25 — `relix provenance` CLI — CLOSED

**Closed in commit c94f75a** (predates this multi-tenant pass — verified during the GAP 23/24/25 sweep).

`crates/relix-cli/src/provenance.rs` ships `Show`, `Diff`, `History`, and `Audit` subcommands proxying the bridge's `/v1/provenance/*` endpoints, registered on `main.rs` as `Cmd::Provenance`.

---

## GAP 26 — Subject-line/sender-based agent routing rules (§7.7 sub-bullet)

**Roadmap claim (`[DONE — commit 29d48ea]`):**
> Channel-agnostic `ChannelRouter` with sender_match / subject_match / content_match / channel_type / catch_all rules, first-match-wins evaluation, peer validation at startup, `routing.resolve` and `routing.list` coordinator capabilities …

**Actual code:**
- `crates/relix-runtime/src/nodes/coordinator/routing.rs` PRESENT — ChannelRouter implemented.
- `routing.resolve` / `routing.list` coordinator capabilities registered.

**Severity:** CONSISTENT — this one matches the claim.

---

## Honest sections where claim and code match

The following entries were verified PRESENT with no material gap beyond what the roadmap itself documents:

- **Wave 1 (1.1 / 1.2 / 1.3)** — auth, process::exit removal, Windows ACL hardening.
- **Wave 2 (2.1 / 2.2)** — SQLite pragmas, single-mutex refactor.
- **Wave 3 (3.1 / 3.2)** — TOCTOU fix, terminal sandbox.
- **Wave 4 (4.1)** — XSS + CSP.
- **Wave 5 (5.1–5.6)** — Docker context, cargo deny, Gemini provider, rate limiting, chronicle retention, OpenAI compat honesty.
- **Dependency auto-install (cd9ea63)** — install --check / --fix.
- **§7.1 Real provider-native streaming** — eight commits backed.
- **§7.2 Telegram/Discord/Slack rich messages** — three channel crates + rich-message handlers.
- **§7.3 SOUL.md personas** — soul.rs in ai node.
- **§7.4 relix update self-upgrade** — full download + atomic replace (W6 closed).
- **§7.5 Multi-agent workflow foundation** — engine, validator, executor, three-mode dispatch, streaming, cancellation.
- **§7.7 Email channel** — smtp.rs / imap.rs / dkim.rs / templates / bridge / CLI.
- **§7.8 Scheduled reports** — coordinator reports module.
- **§7.9 Voice via Whisper** — `nodes/tool/audio.rs`.
- **§7.10 MCP tool expansion** — mcp.rs + mcp_stdio.rs + tool.fs / tool.terminal / tool.web_fetch / tool.web_extract / tool.pdf / tool.browser.
- **§7.11 Agent performance dashboard** — full metrics module + bridge + CLI + dashboard panel.
- **§7.12 Conversation export** — `task.session_export` + bridge endpoint.
- **§7.15 Training data pipeline + PII** — recorder, store, scorer, exporter, PiiDetector, PiiAnonymizer, bridge endpoints, CLI.
- **§7.16 Agent-to-agent knowledge transfer** — all five primary capabilities + four GAP follow-ups (recall, accept_shared, signed payloads, autoshare_stats).
- **§7.19 Per-step confidence scoring + fallback** — scorer + fallback engine + cell + SOL builtin + bridge + CLI + alert-pipeline wiring.
- **§7.24 Spec-driven multi-agent planning** — registry, parser, generator, orchestrator, critic, conflict, approval, verification, bridge, CLI, `relix build`, cancellation, SSE stream, export.
- **§7.26 Components 1, 2, 4, 6** — policy/executor separation, reversibility flag (not full tiering — see GAP 11), JIT secrets, AgentAccessBroker.
- **§7.27 Tool Dispatcher** — dispatcher + semantic retrieval + signed manifests + JSON-schema contracts + output guard + ask_human (wired W3).
- **§7.28 Cost-control + alerting dashboard + mesh PII gate** — shipped this session (BudgetEnforcer, observability caps + bridge + `relix observe`, MeshPiiGate + bridge + `relix pii`).
- **§7.31 Components 1, 2, 3** — OTel exporter (real OTLP POST), two-sink architecture, session debugger query layer.
- **§7.32 Guardrails** — input guardrails, drift detection (wired via mesh embed dispatcher), mode system, multi-agent handoff guards, red-team eval harness + `relix eval guardrails`.
- **Wiring gaps W1–W7** — all closed in code as documented.
- **YAML workflow format** — `yaml_flow` module + two flow templates.
- **SOL & Sflow language extensions** — interpolation, try/catch, list/map literals, for-in, accessors.
- **SOL language reference** — `docs/sol-language-reference.md` + tested examples.

---

## Top 10 gaps by impact

If a future session has limited budget, these are the highest-impact items where the roadmap currently overstates what's built:

1. ~~**GAP 1** — closed by commits 29d25e9 (Python SDK) + 3d1317d (TypeScript SDK)~~
2. ~~**GAP 4** — closed by commits 0bac31e + e47dab2~~
3. ~~**GAP 5** — closed by commit 3c9f3ec~~
4. ~~**GAP 6** — closed by commit 80980e1~~
5. ~~**GAP 11** — closed by commit 235a32b~~
6. ~~**GAP 12** — closed by commit 5aacced~~
7. ~~**GAP 7** — closed by commit e39a079~~
8. ~~**GAP 23** — closed by commits 7feed75 (23A Qdrant) + 1f4368d (23B policy) + 447744a (23C audit partition)~~
9. ~~**GAP 14** — closed by commit c94f75a~~
10. ~~**GAP 13** — closed by commit c94f75a~~

GAP 8 — closed by commit 0e6fd5e (alongside GAPs 5/6/7 in the same session).

---

## Methodology notes

- All claims read from both roadmaps in full (no skipping based on status tags).
- Code verification via four parallel exploration agents covering §7.24/26/27, §7.31/32 + W1-W8, Part 6 + §7.15/16/19, and §7.5/7/17/20/21/28 + YAML + §7.2/10.
- Cross-checked with direct grep for the specific capability names, file paths, and endpoint strings each section claims.
- Commit hashes in the roadmap were ignored as evidence; only file presence and type definitions were counted.

This report deliberately omits sections where the roadmap status matches reality. The full feature-by-feature audit lives in this document's body — additions or corrections should land here, not in `RELIX_ROADMAP.md`'s status tags, until both documents agree.
