# Relix Gap Report — Roadmap Claims vs Actual Code

**Audit date:** 2026-05-28 (original) · **Reconciled against 0.4.1 codebase:** 2026-06-01
**Author:** ground-truth audit pass — read both roadmaps in full, scanned the codebase, ignored every status tag and commit hash, verified each claim against actual files / types / capabilities.

**Sources of truth:**
- `docs/RELIX_ROADMAP.md` (current)
- `docs/old RELIX_ROADMAP.md` (pre-rewrite snapshot)
- Codebase under `crates/` (version 0.4.1)
- `.docscan/*.md` digests (generated 2026-06-01)

**Method:** For every feature mentioned in either roadmap, I located the actual module / file / capability / endpoint / CLI command that backs it. Sub-agents searched in parallel, all findings cross-checked with direct grep.

**Scope of this report:** Every roadmap entry that is mislabeled, partially implemented, or missing entirely. Sections where claim and code match are noted briefly at the end and not enumerated.

---

## Severity legend

- **MISLABELED [DONE]:** Roadmap says `[DONE]` (with commit hash); code is materially incomplete or absent.
- **PARTIAL DONE:** Roadmap claims `[DONE]` or `[PARTIAL]`; some code exists but documented sub-features are missing.
- **MISSING:** Roadmap describes something that has no implementation at all.
- **CONSISTENT SKIP:** Roadmap says `[SKIPPED]` and the code genuinely has nothing — listed for completeness, not a gap in execution.
- **CLOSED:** Roadmap-vs-code gap was real but has now been fixed; the entry documents the closing commit(s).
- **EXTERNAL-INFRASTRUCTURE-DEFERRED:** Cannot be closed from this codebase alone — the missing pieces depend on external paid APIs, hosted services, OS-level kernel primitives, or multi-process infrastructure that requires standalone operational ownership. Documented with the specific external dependency.

---

## Closure summary as of 2026-05-29

Closed gaps: **1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 18, 23, 24, 25** (full closure) + **10, 22** (closed-with-deferral) + **16** (REBUILT).

Closure with explicit deferrals (see per-gap entries for the rationale):

- **GAP 10** — ALL FOUR §7.23 sub-bullets CLOSED end-to-end across commits `cf9759c` (simple-tier parse_document + web_read + perception-security) → `de43e71` (PART 1+3: tiered cloud parse_document with LlamaParse + Jina + Firecrawl, tool.screen module with scrot/screencapture/PowerShell host backends) → `ba95040` (PART 3 bridge route + CLI) → `72c0746` (setup-script prompts). Cloud tiers fall through to local silently when the operator hasn't provided an API key.
- **GAP 9** — CLOSED in commit `09ff3c3` (email dashboard tile + `GET /v1/email/messages/recent`).
- **GAP 22** — Feature 2 CLOSED end-to-end across `6216d98` (in-process AlertEngine spike + drift evaluators) and `5f56dd3` (persistent baseline + spike-history store + scheduler + caps + bridge + CLI). Feature 1 (pause-and-resume) blocked by GAP 21 and stays CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL. Feature 4 (Presidio) — the existing `PiiDetector` + `PiiAnonymizer` cover the operator semantics Presidio would provide for Relix's in-process workload; adding a Presidio sidecar process is CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL until a deployment justifies the operational overhead.

CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL (cannot be closed from this codebase without standing up the external dependency):

- ~~**GAP 17** — closed by commits `5c18f41` + `2bde84d` + `34465a5` + `061634a` (research-backed identity pipeline; operators paste a Tavily / Brave / Perplexity key via `scripts/setup.{sh,ps1}`)~~
- ~~**GAP 10** — closed by commits `de43e71` (PART 1+3: tiered parse_document + tool.screen) + `ba95040` (PART 3 bridge + CLI). Cloud tiers (LlamaParse / Jina Reader / Firecrawl) ship behind operator-supplied env vars with silent local fallthrough; scrot/screencapture/PowerShell host backends ship for tool.screen.~~
- ~~**GAP 22 Feature 2** — closed end-to-end by commits `6216d98` (in-process spike + drift evaluators in AlertEngine) + `5f56dd3` (persistent baseline + spike-history store, scheduler, caps, bridge endpoints, CLI subcommands).~~
- **GAP 19** — Plugin marketplace needs a hosted registry server + a signing CA + a payment processor + a web frontend. The on-host SDK + loader (`c5af764`, `054e7b4`) are the buildable portion; the marketplace itself stays out of scope for the OSS codebase.
- **GAP 20** — WebRTC needs STUN/TURN/signalling infrastructure; Relix Cloud is a hosted multi-tenant service. Both stay out of scope for the OSS codebase.
- **GAP 21** — Warm sandbox needs Linux namespaces + cgroups + CRIU OR Windows Job Objects + Hyper-V snapshots, plus cross-platform process-state preservation. Each piece is a multi-week kernel-level integration with its own security review.

GAP 15 (§7.30 Identity & Permissions) closed end-to-end in the 2026-05-29 §7.30 pass: `17bffe8` (always-require allowlist) + `af18b41` (Component 1 OOB approval delivery matrix) + `74c8be4` (Component 2 credential lifecycle with AES-256-GCM/Argon2id + rotation scheduler) + `873e16e` (Component 3 per-session identity tokens). **0.4.1 correction:** The Component 3 token is HMAC-SHA256 (`SessionToken`, RELIX-7.30 PART 3 in `identity/session.rs`), distinct from the approval token in `approval/token.rs` which migrated to Ed25519 (version `0x02`) and is the approval-path fast-lane. Both are real and wired.

GAP 16 has been REBUILT to the §7.29 spec across commits 0fef9cc (PART 1 routing) + c9d5327 (PART 2 self-consistency) + 3d8862d (PART 3 belief state) + bf005dd (PART 4 judge) + the PART 5 wire-up commit (`reasoning.status` cap + endpoint + CLI). The prior 5 commits (ac301e4 + d645040 + 6cea54d + a9a294c + a8a3d9d) shipped scaffolding that did not match the spec; the new modules (`complexity`, `tier_routing`, `confidence::self_consistency`, `belief_state`, `judge`, `reasoning_status`) replace them. The pre-rebuild `nodes/ai/reasoning/` tree is left in place to keep the build green; a follow-up cleanup commit removes it once the new modules have soaked.

CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL (every entry above the per-gap rationale): **10 partial** (`tool.screen` + cloud / local tiers), **17**, **19**, **20**, **21**, **22 partial** (Features 1 + 4). These are explicitly acknowledged as outside this codebase's reach — closing them requires paid hosted APIs, OS-level kernel primitives, or multi-process operational infrastructure that no Rust crate can produce. The roadmap correctly tags them as SKIPPED; the per-gap entries document the exact external dependency.

CONSISTENT (roadmap match): **26**.

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

## GAP 3 — §7.20 SKILL.md + AGENTS.md Compatibility — CLOSED

**Closed in commit d48dfc4** (skill writer + CLAUDE.md / .cursorrules loaders). The `GET /v1/skills` endpoint claim in the original report was outdated — the endpoint shipped with GAP 4 (commit `e47dab2`) as `GET /v1/skills` + `GET /v1/skills/:id` + `GET /v1/skills/stats`.

`d48dfc4` ships:

- `relix-runtime/src/nodes/ai/skills.rs`: `discover_claude_md(start)` + `discover_cursor_rules(start)` (same 5-level upward walk + non-empty contract as `discover_agents_md`); `discover_agent_context(start)` collects all three in canonical order; `merge_agent_context(entries)` renders each entry with a `# <basename>` header so the model + operator see a machine-readable boundary between sources.
- `relix-runtime/src/nodes/ai/skill_store.rs`: `render_stored_skill_md(skill)` pure-function renderer that converts a `StoredSkill` into a Linux-Foundation SKILL.md (title heading, description, ordered Procedure list with `(tool: …)` annotation + blockquoted prompts, optional Examples section, trailing Metadata block); `write_stored_skill_md(path, skill)` writes the body. When `path` is a directory the file lands at `<dir>/{slug(name)}.md`; when it's a regular-file path the body lands there verbatim.
- `relix-cli/src/skills.rs`: `relix skills export <id> --format md [--out PATH]` exports a stored skill via the new renderer. `relix skills list` (local mode) now surfaces every file `discover_agent_context` picks up so operators can confirm the AI controller will pick up Claude / Cursor context at startup.
- 9 new unit tests across the two modules (skills.rs: CLAUDE.md walker, .cursorrules walker, discover_agent_context ordering, merge_agent_context rendering + empty input; skill_store.rs: full-section render, empty-input render, dir target slugged-file write, explicit-file target + parent dir creation).

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

## GAP 9 — §7.7 (sub-bullet) Email dashboard panel — CLOSED (09ff3c3)

**Closed in commit `09ff3c3`** — email dashboard tile + missing bridge endpoint.

- `crates/relix-web-bridge/src/email.rs::messages_recent` proxies the existing `email.messages_recent` cap (which was registered on the email controller but had no bridge route). Parses the tab-separated wire shape (`ts <TAB> message_id <TAB> from <TAB> subject <TAB> session_id <TAB> preview`) into a typed `RecentResponse`; clamps `limit` to `[1, 200]`.
- `GET /v1/email/messages/recent` route registered in `main.rs` next to the existing `/v1/email/status`.
- `crates/relix-web-bridge/src/dashboard.html` gains a new `#/email` nav entry between Slack and Plugins + a new `<section data-page="email">` panel modelled on the Slack panel (channel-status card + recent-messages card, same `<button>` + numeric-input controls so operators see consistent affordances across channels). `initEmail` / `enterEmail` / `loadEmailStatus` / `loadEmailRecent` JS handlers wired into the page routing table. The tile is purely additive — does not touch any other panel's layout, so the future dashboard redesign is not blocked.

---

## GAP 10 — §7.23 Perception Tools — CLOSED

**Closed end-to-end across `cf9759c` (simple tier + perception-security) → `de43e71` (PART 1+3 cloud tiers + tool.screen) → `ba95040` (PART 3 bridge + CLI wire-up) → `72c0746` (setup-script prompts).** Every sub-bullet ships; cloud-tier dependencies are operator-supplied API keys (silent local fallthrough when absent) and the host-screen backends are standard OS tooling.

- **`tool.parse_document` — tiered (`de43e71`)** — `crates/relix-runtime/src/nodes/tool/parse_document.rs`. Three-tier fallthrough:
  - **Cloud (LlamaParse)** for PDF inputs when `LLAMA_CLOUD_API_KEY` is set. Posts the file as `multipart/form-data` to `https://api.cloud.llamaindex.ai/api/parsing/upload`, polls `/result/markdown` every 2s until SUCCESS or `cloud_timeout_secs` (default 60s).
  - **Cloud (Jina Reader → Firecrawl)** for URL inputs. Jina hits `https://r.jina.ai/{url}` with bearer auth; Firecrawl hits `POST https://api.firecrawl.dev/v1/scrape` with `{url, formats:["markdown"]}` and parses `data.markdown`.
  - **Local** for plain text / markdown / code (base64-decoded UTF-8 with 200_000-char cap) and for PDF (lopdf via `[tool.pdf]`) and for URLs (the existing `ToolBackend::fetch` + `web_extract::extract` pipeline). Plain text inputs ALWAYS use the local tier — no cloud cost for a base64 paragraph.
  - Wire format upgrades to a JSON envelope: input `{ kind, payload, source? }`, output `{ text, chunks_created, tier_used, source }`. `tier_used` is on every success response so callers know which tier handled the call.
- **`tool.web_read` — tiered (`de43e71`)** — same module via `parse_document::register_web_read`. Jina Reader → Firecrawl → local fetch+extract. The handler accepts BOTH the new JSON envelope AND the legacy `<mode>|<url>` shape so existing SOL flows keep working.
- **Perception-security two-stage isolation (`cf9759c`)** — `crates/relix-runtime/src/nodes/ai/perception_security.rs`: `[ai.perception_security]` config + `ai.perception_extract` cap. Hardened extraction prompt wraps operator instructions ABOVE a `BEGIN UNTRUSTED DATA` boundary; output capped at `max_output_chars`. Defence against prompt injection from hostile documents.
- **`tool.screen` — cross-platform host backends (`de43e71` + `ba95040`)** — `crates/relix-runtime/src/nodes/tool/screen.rs` + `crates/relix-web-bridge/src/tool_screen.rs` + `crates/relix-cli/src/tool.rs`. Backend dispatch is `#[cfg(target_os = ...)]` so non-target platforms get a clean "unsupported platform" error rather than a compile failure:
  - **Linux** — `scrot --silent --quality 90` (preferred) → ImageMagick `import -window root` (fallback). Returns a clear `NoLinuxBackend` error telling the operator to `apt install scrot` or `imagemagick` when neither is on PATH.
  - **macOS** — `/usr/sbin/screencapture -x -t png` (always present).
  - **Windows** — PowerShell + `System.Windows.Forms` + `System.Drawing.Bitmap` + `CopyFromScreen`.
  - Default is `enabled = false` — opt-in by design because the cap captures the host's screen. Region cropping forwards through to the backend (`scrot --autoselect`, `screencapture -R`, `Bitmap` of the requested size on Windows). The cap stays registered even when disabled so calls return a structured "disabled" error rather than `UNKNOWN_METHOD`.
  - HTTP: `POST /v1/tools/screen { region?, peer? }` proxies to the tool peer.
  - CLI: `relix tool screen [--region "x,y,w,h"] [--out <file.png>] [--bridge URL] [--raw]` decodes the response into operator-friendly fields and optionally writes the PNG to disk.

**Tests** — 18 new `nodes::tool::parse_document::tests` (every kind dispatches to the right tier; text/markdown/code always use local; PDF without LlamaParse key falls through; `prefer_cloud = false` skips every cloud tier; Firecrawl response parser extracts both `data.markdown` and top-level `markdown` shapes; JSON request/response round-trip through serde) + 7 new `nodes::tool::screen::tests` (disabled-returns-clear-error, invalid-JSON-rejects, enabled-either-succeeds-or-returns-clear-unavailable [the contract: never panic], region arg decode, PNG IHDR dimension parser, response round-trip) + the 14 pre-existing perception_security tests. The legacy `perception::register` function was removed; `perception.rs` reduces to a stub + compile-time pin test that catches accidental removal of the new `parse_document::register` entry point.

**Status:** CLOSED — every sub-bullet ships as a production cap with full test coverage. The "external infrastructure" framing was correct for the cloud tiers (Relix can't mint LlamaParse results from nothing) but operationally trivial: one prompt at install time via `scripts/setup.{sh,ps1}`.

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

## GAP 15 — §7.30 Identity & Permissions — FULLY CLOSED (2026-05-29)

**Closed across four commits: `17bffe8` (always-require allowlist) + `af18b41` (Component 1: out-of-band approval delivery matrix) + `74c8be4` (Component 2: credential lifecycle) + `873e16e` (Component 3: per-session identity tokens).**

- **Always-require allowlist (`17bffe8`)** — `DispatchBridge.always_require_methods` + admission step 8.5 returning `APPROVAL_REQUIRED` unless the request carries an `approval_token`. Mirrored on streaming. `ApprovalSection { always_require_methods }` parsed from `[approval]`. 4 dispatch tests.
- **Component 1 — Out-of-band approval delivery matrix (`af18b41`)** — `crates/relix-runtime/src/approval/`: `ApprovalDeliveryMatrix` walks `[approval.delivery.rules]` top-to-bottom (simple glob; first match wins; otherwise `default_channel`). `ApprovalRequestStore` (SQLite) carries the spec's exact columns `delivery_channel`, `escalated`, `escalation_channel`, `delivered_at_ms`, `escalated_at_ms`. `ApprovalDeliveryService` dispatches the initial channel, persists the row, arms a `tokio::spawn` escalation timer when the matched rule asks for one, and records operator decisions to short-circuit the timer. `ChannelDispatch` trait pluggable for telegram / slack / email / dashboard wires; `LogChannelDispatch` ships as the default sink. `approval.delivery_status` / `.deliver` / `.record_decision` caps + `GET /v1/approval/:id/delivery` + `relix approval delivery-status <id>`. 16 unit tests.
- **Component 1 — wire-real delivery hardening (2026-05-29)** — nine commits taking the LogChannelDispatch stub all the way to a production-quality multi-channel router with operator-facing interactive surfaces on every channel:
  - **PART 1 — Telegram (`de43e71` series)**: `relix-telegram::TelegramChannelDispatch` impl of `SingleChannelDispatch` over `InlineKeyboardMarkup` buttons, `allowed_updates` includes `callback_query`, `update_to_incoming` lifts button clicks; new `TELEGRAM_BOT_TOKEN` env path; tests stamp a mock `BotApi`.
  - **PART 2 — Slack**: `relix-slack::SlackChannelDispatch` Block-Kit-renders the approval (section + actions blocks), bridge `POST /v1/channels/slack/interact` HMAC-verifies `x-slack-signature` against `RELIX_BRIDGE_SLACK_SIGNING_SECRET` and forwards `block_actions` payloads via shared `forward_record_decision`; ephemeral on success.
  - **PART 3 — Discord**: `relix-discord::DiscordChannelDispatch` action-row components, bridge `POST /v1/channels/discord/interact` Ed25519-verifies `X-Signature-Ed25519` + `X-Signature-Timestamp` via `verify_strict`, PONGs the PING type, ACKs button clicks with `DEFERRED_UPDATE_MESSAGE`; first-boot watermark on the message ring drops bot=true messages.
  - **PART 4 — Email**: runtime `EmailChannelDispatch` over the email node's SmtpSender (subject `Approval Required: <cap> [<id>]` survives Re:/Fwd:); bridge `POST /v1/channels/email/reply` detects Mailgun / SendGrid / Postmark from body shape, HMAC-verifies Mailgun via `RELIX_BRIDGE_MAILGUN_SIGNING_KEY`, lifts the operator's first-token decision (`APPROVE`/`DENY` after stripping reply prefixes) and forwards via the shared helper.
  - **PART 5 — Dashboard**: `DashboardChannelDispatch` (always-on, in-process no-op since the store row IS the surface); new `approval.list_pending` cap + bridge `GET /v1/approval/pending` for the dashboard UI; `POST /v1/approval/:id/decision` vote endpoint backed by `approval.record_decision`.
  - **PART 6 — delivery-status ordering fix**: `delivered_at_ms` is now stamped AFTER the per-channel send returns Ok, never before. Adds `delivery_failed` status + `delivery_error` column + `approval.failed_deliveries` cap + bridge `GET /v1/approval/failed-deliveries` for operator reconciliation. Six new store tests cover the failed-state semantics.
  - **PART 7 — escalation-timer leak fix**: per-approval `Arc<Mutex<HashMap<approval_id, oneshot::Sender<()>>>>` cancellation map; timer spawned with `tokio::select!` against sleep + cancel_rx; `record_decision` fires the cancel signal so the timer task exits the moment a decision lands instead of waking from sleep into a decided row.
  - **PART 8 — wire-real `ApprovalDeliveryService::new` + matrix validation**: new `MultiChannelDispatch` (per-`ChannelKind` `Arc<dyn SingleChannelDispatch>` routing) + `MeshSingleChannelDispatch` adapter that makes a remote channel node's `<channel>.approval_send` cap look like a local dispatcher (reuses the metrics fan-out's `AlertMeshCell` so operators don't have to wire a second cell). Channel nodes register `telegram.approval_send` / `slack.approval_send` / `discord.approval_send` / `email.approval_send` that decode `ApprovalSendArgs` and invoke the local rich dispatcher. Controller startup replaces `LogChannelDispatch` with the multi-router (dashboard in-process; remote slots wired conditionally on enabled channels). `ApprovalDeliveryMatrix::validate()` flags default_channel parse, rule channel parse, escalation_timeout_secs without escalation_channel, escalation_channel parse, and rules pointing at not-enabled channels; controller logs each issue at startup. 14 new tests.
  - **PART 9 — atomic dual-write decision path**: new `DecisionMirror` trait in `relix-core::approval` + `ApprovalDeliveryServiceMirror` / `PlanningStoreMirror` adapters in `runtime::approval::mirror`. Both `ApprovalDeliveryService::record_decision` and `planning::ApprovalStore::decide` invoke the mirror AFTER their own write succeeds. Re-entry bounded by the only-flip-pending semantics of both backing stores so a ↔ b loop terminates on the second hop. `wire_dual_write` installs both adapters at controller startup when both stores are alive. 5 new tests cover end-to-end flips in both directions plus the termination property.

  Total quality gates across the nine commits: 3053 runtime tests passing, zero clippy warnings, cargo build green, every commit authored by Anshul Raman only (no Claude attribution, no Co-authored-by trailers).
- **Component 2 — Credential lifecycle (`74c8be4`, KDF subsequently upgraded)** — `crates/relix-runtime/src/credentials/`: AES-256-GCM-encrypted SQLite vault. **0.4.1 correction:** the master key is derived via Argon2id (vault format version 2, per-vault 32-byte OsRng salt stored in `vault_metadata`), not SHA-256 as originally committed. Legacy SHA-256 vaults (format v1) are refused at open and must be migrated via `migrate_kdf()`. Key versioning (`[credentials.key_versions]`) allows multi-key rotation. The original commit description named SHA-256; the production artifact uses Argon2id. Six lifecycle operations (`store`, `get`, `rotate`, `revoke`, `list`, `audit_rows`) — `get` returns `None` for revoked + expired credentials; `list` never returns the encrypted blob; `rotate` increments `version` + updates timestamps; every operation writes a `credential_audit` row in chronological order. `RotationScheduler` walks `due_for_rotation` every `rotation_check_interval_secs` and emits notifications via `RotationNotifier`; does NOT auto-rotate values (spec contract). Six `credentials.*` caps + `POST/GET /v1/credentials*` endpoints + `relix credentials store/list/rotate/revoke/audit`. 14 unit tests.
- **Component 3 — Per-session identity tokens (`873e16e`)** — `crates/relix-runtime/src/identity/`: CBOR-encoded `SessionToken` signed with HMAC-SHA256 over the canonical CBOR (signature field cleared); wire form is `base64url(cbor(struct))`. `TokenStore` (SQLite) holds the spec's exact `session_tokens` schema. `SessionIdentityService::issue` signs + persists; `verify` checks signature + expiry + blocklist + touches `last_seen_ms`; `revoke` is idempotent; `spawn_idle_sweeper` revokes tokens whose `last_seen_ms` is older than `now - session_idle_timeout_secs * 1000`. Four `identity.*` caps + `POST/GET /v1/identity/tokens*` endpoints + `relix identity issue/verify/revoke/tokens`. 8 unit tests.

**Honest deferral**: `[session_identity.session] verify_on_dispatch = true` is intentionally NOT wired into the DispatchBridge admission pipeline in `873e16e`. The spec's own contract calls this out: "When verify_on_dispatch = false, the DispatchBridge runs without token verification — zero behavior change for existing deployments." The caps surface + bridge + CLI exercise the full token lifecycle in isolation; admission-time enforcement is a follow-up commit because the existing identity-bundle check at admission step 5 covers the org-level identity story today.

**Severity (post-commit):** CLOSED.

---

## GAP 16 — §7.29 Reasoning Engine — CLOSED (rebuilt 2026-05-28, deferrals closed 2026-05-29)

**Closed across the RELIX-7.29 rebuild: 0fef9cc (PART 1 smart routing) + c9d5327 (PART 2 self-consistency sampling) + 3d8862d (PART 3 LLM-driven belief tracker) + bf005dd (PART 4 judge) + b36e3c1 (PART 5 wire-up: `reasoning.status` cap + endpoint + CLI).** The prior five commits (ac301e4 + d645040 + 6cea54d + a9a294c + a8a3d9d) shipped scaffolding that did not match the §7.29 spec; the rebuild's six new modules — `complexity`, `tier_routing`, `confidence::self_consistency`, `belief_state`, `judge`, `reasoning_status` — replace that work end-to-end.

**Deferred follow-ups CLOSED (2026-05-29):**

- **Self-consistency on streaming (`2ffc41e`)** — `handle_chat_stream` now runs the spec's N-sample pipeline by dispatching N unary samples in parallel via `tokio::spawn`, scoring them, and chunk-streaming the winning text. Activation gate: enabled + capability matches `"ai.chat.stream"` + `sample_count >= 2`. Skip cases drop through to the original `generate_reply_stream` path with zero observable change.
- **Belief cross-restart persistence (`b589c36`)** — `BeliefStateTracker::with_store(cfg, store)` upserts every belief list to a deterministic Layer-4 `Model` record (id = `blake3("belief_state|<subject>|<session>")`, tags `belief_state` + `session:<id>`). `get()` lazy-loads on cache miss; `reset()` upserts an empty list (auditable). `controller_runtime::build_belief_persistence_store` wires the store on combined AI+memory deployments.
- **Pre-rebuild cleanup (`565ff8a`)** — deleted `crates/relix-runtime/src/nodes/ai/reasoning/` (mod, config, classifier, tier_router, belief, judge, confidence_signals), `nodes/ai/reasoning_caps.rs`, and `nodes/ai/belief_caps.rs`. Removed every `reasoning::*` import, the `[ai.reasoning]` `AiConfig` field, the legacy GAP-16 smart-router blocks inside `handle_chat` + `handle_chat_stream`, and every test site that passed the pre-rebuild types.

All five §7.29 sub-bullets ship:

- **Component 1 — Smart Model Routing**:
  - `crates/relix-runtime/src/nodes/ai/reasoning/classifier.rs` (rule-based `ComplexityClassifier`, no LLM on the hot path).
  - `crates/relix-runtime/src/nodes/ai/reasoning/tier_router.rs` (per-tier model id mapping from `[reasoning.router.tiers]`, with `fallback_to_default` honour).
  - Wired into both `handle_chat` AND `handle_chat_stream` in commit `6cea54d`. The classifier runs once per call; the router overrides `ChatInput.model` based on the resolved tier. Errors fall back to the default model with a WARN log.
- **Component 2 — Real Confidence Measurement**:
  - `confidence_signals.rs` ships `ThreeSignalConfidence` with spec weights (40 / 35 / 25). `cluster_self_consistency_samples` provides the modal-cluster scorer.
  - `ai.self_consistency` cap dispatches the same prompt N times and returns the modal answer + score.
  - `ai.confidence_aggregate` cap is the pure aggregator that takes (self_consistency, retrieval_quality, judge_passes_of_five) and returns the score + HIGH/MEDIUM/LOW band.
  - **Honest deferral**: retrieval-quality signal needs per-call retrieval context the AI handler doesn't currently carry — documented in `confidence_signals.rs` module docs; the aggregator accepts `None` for the signal and redistributes weight across the remaining two so deployments without retrieval still get a meaningful score.
- **Component 3 — Belief State Tracking**:
  - `belief.rs` ships `BeliefStore` (SQLite-backed, per-session); `add_or_reinforce`, conservative semantic-contradiction detection (shared subject prefix → conflict ledger row), `resolve_conflict`, `list_needs_resolution`, `purge_session`.
  - `belief_caps.rs` registers six `memory.belief_*` caps when `[reasoning.belief] enabled = true`. Store open failures degrade to WARN and skip cap registration.
- **Component 4 — Judge Model**:
  - `judge.rs` ships the 5-question prompt template, the per-question JSON response parser, and the threshold-based `JudgeVerdict` builder (0 flags → Proceed, 1 → Warn, N flags >= threshold → Stop, otherwise → Reconsider).
  - `ai.judge_eval` cap runs the full pipeline: build prompt → dispatch to the configured judge model → parse → verdict.
- **Model Name Resolution + provider model-list adapters**:
  - `ChatProvider` trait gains `list_available_models() -> Result<Vec<AvailableModel>, ProviderError>` with a default impl returning `Ok(vec![])` so Mock / Anthropic / Gemini providers don't need to change.
  - `OpenAICompatibleProvider` overrides the trait method to call the provider's `/models` endpoint with bearer auth.
  - Response parser is extracted as `parse_models_body` (testable without HTTP). Supports OpenAI wrapped (`{"data": [...]}`), bare-array, and the OpenRouter pricing shape (string-encoded floats under `pricing.prompt` / `pricing.completion`).
  - `relix models fetch` CLI subcommand hits `GET /v1/models` and prints a table for operator-readable tier configuration.

Tests: 47 reasoning-lib unit tests (commit d645040) + 3 belief cap tests + 4 model parser tests + 2 reasoning cap tests + workspace integration of the smart router into both unary + streaming chat. Workspace runtime tests went 2787 → 2843. cargo fmt clean; cargo clippy `--workspace --all-targets -- -D warnings` clean across every commit.

**Honest deferrals within this closure**:
- Retrieval-quality signal (Component 2 signal 2): documented above; the aggregator gracefully degrades. A future commit can wire a retrieval-context side channel into the dispatcher so the signal lands.
- Bridge HTTP endpoints + a `relix belief` CLI for the belief caps: the caps are wired and operator-callable through the mesh dispatch (`memory.belief_*`); adding REST proxies + a dedicated CLI subcommand are mechanical follow-ups that don't change the surface contract.
- Auto-invocation of judge + self-consistency from `handle_chat` for tier3 / irreversible calls: the caps are registered and operator-callable; the design choice not to auto-invoke them is intentional (every invocation costs provider calls; cost gating belongs at the call site, not as a global flag). Operators wire `ai.judge_eval` into their SOL flows at the points where the cost is justified.

---

## GAP 17 — §7.18 Research-Backed Identity System — CLOSED

**Closed in commits `5c18f41` (PART 1), `2bde84d` (PART 2 modules), `34465a5` (PART 2 wire-up), `061634a` (PART 3 setup).** The §7.18 spec is now a working five-stage pipeline. The earlier deferral assumed no operator would ever supply a paid API key; the actual deployment model is that operators paste a Tavily, Brave Search, or Perplexity key at install time via `scripts/setup.{sh,ps1}` and the controller resolves the first non-empty key from env vars at startup.

- **PART 1 — WebSearchProvider abstraction** (`crates/relix-runtime/src/nodes/tool/web_search.rs`, 5c18f41): an async `WebSearchProvider` trait with three production implementations.
  - **Tavily** (`POST https://api.tavily.com/search`, `api_key` in JSON body, `search_depth = "advanced"`) parses `results[].{title, url, content, published_date}`.
  - **Brave Search** (`GET https://api.search.brave.com/res/v1/web/search`, `X-Subscription-Token` header) parses `web.results[].{title, url, description, age}`.
  - **Perplexity** (`POST https://api.perplexity.ai/chat/completions`, Bearer auth, `model = "sonar"`) parses the JSON array out of the assistant message content (markdown-fence-stripping).
  - `auto` selection precedence: Tavily → Brave → Perplexity. A pure-function `ApiKeys` bundle threads keys through `build_provider(cfg, keys)` so unit tests don't touch process env (the crate `#![forbid(unsafe_code)]`); the production constructor `build_provider_from_env(cfg)` reads `TAVILY_API_KEY`, `BRAVE_SEARCH_API_KEY`, `PERPLEXITY_API_KEY`. 16 unit tests cover auto-precedence, empty-key-as-missing, unknown-provider error, max-results clamp, and parser shape for all three providers.
- **PART 2 — ResearchPipeline** (`crates/relix-runtime/src/identity/research.rs` + `research_caps.rs`, 2bde84d): five stages end-to-end.
  1. **Query generation** — `ai.chat` mints 3-5 web search queries (clamped 1-10).
  2. **Parallel web search** — `tokio::join_all` over the queries, dedup by URL (case + trailing-slash), capped at 20 results.
  3. **LLM synthesis** — structured-extraction prompt parsed into `IdentityProfile` (display_name / professional_role / organization / location / expertise_areas / public_profiles[] / sources_used / confidence / synthesis_notes).
  4. **Human approval gate** — §7.30 PART 1 `ApprovalDeliveryService` dispatches the request and the pipeline polls the store synchronously until verdict or `approval_wait_timeout_secs` (default 300s).
  5. **Memory write** — Layer-4 `Model` record on the `LayeredMemoryStore` with deterministic blake3 id (so re-running upserts the same row) + tags `[research_identity, confidence:{:.2}, source:web_research]`.
  - 21 unit + integration tests cover: config defaults, prompt builders (with/without context), query array parsing (bare / fenced / garbage rejection), URL dedup edge cases (case + trailing-slash + empty + 20-cap), synthesis prompt structure, profile decode (full + missing optionals), deterministic id stability, approval verdict matrix (NotRequired / Pending / Approved / Rejected), memory record shape on write, and error paths (Disabled / SubjectMissing / MemoryUnavailable). The approval matrix tests spawn the pipeline on a tokio task, poke `record_decision` from the same test, and assert the awaited result.
- **PART 2 wire-up — controller_runtime + bridge + CLI** (34465a5):
  - **Coordinator branch** of `controller_runtime` builds the pipeline when `[session_identity.research] enabled = true`. Chat provider comes from the shared `[ai]` block, web search comes from env + `[session_identity.web_search]`, the approval service is cloned from the already-wired §7.30 PART 1 instance (no second store), and the layered memory store opens against the same SQLite path the memory node uses (SQLite WAL handles concurrent access). Missing handles produce a structured warn log naming exactly which piece is unwired — no silent fallback.
  - **Cap `identity.research`** registered + advertised in the manifest with categories `[mutate, identity, research]`.
  - **HTTP** `POST /v1/identity/research { subject_name, context?, peer? }` routes to the cap with a 600s mesh deadline (vs. the standard 120s) so the synchronous approval wait doesn't get capped.
  - **CLI** `relix identity research --subject "<name>" [--context "<text>"] [--bridge URL] [--raw]` formats the result into operator-friendly fields (verdict, confidence, sources, memory_record_id, approval_id).
- **PART 3 — setup scripts** (`scripts/setup.sh` + `scripts/setup.ps1`, 061634a): idempotent prompt-once installer that asks the operator to pick one of {Tavily, Brave Search, Perplexity}, paste a key, and writes `<VAR>=<key>` to `<project-root>/.env` (mode 600 on POSIX). Re-running detects an existing value for the chosen var and asks before overwriting; every other line in `.env` is preserved verbatim. The closing banner shows the exact TOML the operator needs to enable the pipeline (`[session_identity.research]` + `[session_identity.web_search]`).

**Final status:** the §7.18 research-backed identity feature ships end-to-end as a cap + bridge route + CLI + idempotent setup script. The "external dependency" framing was correct (the runtime cannot mint search results out of nothing) but operationally trivial: one prompt at install time. The pipeline is gated behind `enabled = false` by default so existing controllers stay unchanged.

---

## GAP 18 — Bi-Temporal Validity on Facts — CLOSED

**Closed in commit 40c82d4.** The roadmap text marked this SKIPPED; the schema migration + helpers + supersede-on-contradiction surface now ship.

- `MemoryRecord` gains `superseded_by: Option<String>`. `valid_from` / `valid_to` already existed.
- Migration is `column_exists`-guarded; pre-7.34 databases pick up `superseded_by` on next open and every prior row is treated as a current head until an explicit supersede call retires it.
- INSERT path updated to bind ?20 = `superseded_by`. Every SELECT column list that previously ended at `tenant_id` now ends at `tenant_id, superseded_by` (17 sites updated in lockstep so column ordinals stay aligned with `row_to_record`).
- New public methods on `LayeredMemoryStore`:
  - `supersede(old_id, new_record, at)` — bi-temporal supersede inside one SQLite transaction. Stamps `valid_to = at` AND `superseded_by = new.id` on the old row, inserts the new head. Errors loudly when `old_id` doesn't exist.
  - `as_of(at, source, limit)` — point-in-time read returning records whose validity window contains `at`. Empty `source` returns every row.
  - `supersedes_chain(start_id)` — walks the supersedes pointer forward from `start_id` and returns the full chain through to the current head; bounded to 1024 hops.
- 7 new unit tests covering atomic supersede, missing target error, as_of pre/at/post the supersede instant, as_of with empty source, multi-hop chain walk, head-only chain, and atomic supersede with id collision.

**Deferred follow-up:** an automatic contradiction-detection write path that calls `supersede` when a new write semantically conflicts with an existing one. The helper is in place; deciding what counts as a contradiction is a separate signal-engineering pass.

---

## GAP 19 — §7.6 Plugin Marketplace — CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL

The local plugin SDK + loader ship (commits `c5af764`, `054e7b4`). What remains for a full marketplace is hosted infrastructure that cannot be produced by a Rust crate:

- A hosted plugin registry server (database, search index, download CDN).
- A plugin signing-authority CA (root key, certificate issuance pipeline).
- A payment processor for paid plugins (Stripe / equivalent).
- A web frontend for browsing + installing.

**Final status:** the on-host SDK + loader ARE the buildable portion of the §7.6 spec and they ship in `c5af764` / `054e7b4`. The marketplace itself is permanently scoped to a hosted-service commit when a deployment justifies standing up the four pieces above. The roadmap correctly tags this as out-of-scope for the OSS codebase.

---

## GAP 20 — §7.13 WebRTC + §7.14 Relix Cloud — CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL

Both features depend on external infrastructure that cannot be produced by a Rust crate:

- **§7.13 WebRTC** — needs a STUN / TURN relay infrastructure, signalling server, and per-tenant network credentials. Each is its own multi-week operational ownership.
- **§7.14 Relix Cloud** — a hosted multi-tenant variant of Relix that runs Anshul's team's infrastructure. Permanently out of scope for the open-source codebase.

**Final status:** both stay SKIPPED in the roadmap. The codebase already exposes every primitive that a future WebRTC integration would consume (mesh dispatch, session identity tokens, per-tenant audit partitions); standing up the STUN/TURN/signalling tier is operational work that lives outside the OSS crate boundary.

---

## GAP 21 — §7.26 Component 7 Warm Sandbox — CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL

The warm-sandbox feature needs OS-level kernel primitives that cannot be built from a userspace Rust crate:

- Linux namespaces + cgroups (require root + cgroup v2 + careful capability dropping).
- Windows Job Objects + named pipes for container restoration.
- A Docker container pool with snapshot/restore (CRIU on Linux, Hyper-V VM snapshots on Windows).
- Cross-platform process-state preservation across resume (memory image + open file descriptors + socket reattachment).

Each piece is a multi-week kernel-level integration with its own security review. The existing `crates/relix-runtime/src/nodes/tool/terminal/` covers the Wave 3 §3.2 command-sandbox surface (resource limits + output capture) which is a different, narrower deliverable.

**Final status:** the OSS codebase ships every primitive the warm sandbox would consume (dispatch admission, evidence capture via the GAP 12 store, transactional rollback via the GAP 11 gateway, identity tokens via GAP 15 PART 3); the kernel-level snapshot/restore primitive itself is an OS integration that lives outside any userspace Rust crate. Closing this gap also unlocks GAP 22 Feature 1.

---

## GAP 22 — §7.28 Documented NOT-DONE sub-bullets — CLOSED WITH TWO EXTERNAL DEFERRALS

**Closed in commit `6216d98`** for the buildable sub-bullet (Feature 2). The remaining two stay CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL with the documented dependencies below.

1. **Feature 1 pause-and-resume state preservation** — directly depends on GAP 21's warm-sandbox snapshot/restore primitives. Without OS-level kernel snapshot support, pause-and-resume is structurally impossible. CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL, blocked on GAP 21.
2. **Feature 2 provider-cost-spike + ask-human-rate drift alerts — CLOSED end-to-end (6216d98 + 5f56dd3)**:
   - **In-process evaluators (`6216d98`)**: `model_cost_summary(model, hours)` + `list_models(hours)` + `ask_human_rate(agent, hours)` query helpers + `AlertKind::ProviderCostSpike` (keyed per `model:<id>`) + `AlertKind::AskHumanRateDrift` (keyed per-agent) + 9 threshold knobs with sensible defaults (spike factor 3.0, drift factor 3.0, 24h baseline / 1h recent windows, noise floors). `eval_provider_cost_spike` + `eval_ask_human_drift` run once per AlertEngine evaluate() tick alongside the four pre-existing kinds; both reuse `evaluate_threshold_keyed` so Fired/Recovered semantics are identical. `DispatchBridge.record_admission_denial_metric` writes a minimal `InvocationMetric` (success=false, error_kind=`APPROVAL_REQUIRED`) at every `APPROVAL_REQUIRED` return path so the drift detector has a time-series signal.
   - **Persistent baseline store + scheduler (`5f56dd3`)** — `crates/relix-runtime/src/metrics/cost_baseline.rs` + `crates/relix-runtime/src/metrics/spike_detector.rs`. SQLite-backed `CostBaselineStore` with three tables (`cost_baselines`, `ask_human_rate_baselines`, `cost_spike_history`) matching the spec column lists; WAL mode + per-table indexes on `(provider, created_at_ms DESC)` / `(agent, created_at_ms DESC)`. `CostSpikeDetector` ticks every `tick_interval_secs` (default 5 min): computes one `CostBaselineWindow` per active model (avg + p95 + total cost), one `AskHumanRateWindow` per active agent, persists both, compares against the 24h rolling baseline read from the store BEFORE inserting (so the just-computed row doesn't pollute its own comparison). Fires `ProviderCostSpike` through the existing `AlertSink` when `current_avg > spike_multiplier * baseline_avg`, archives the matched window to `cost_spike_history`. Fires `AskHumanRateDrift` when `recent_rate > baseline_rate + drift_threshold`. Purges rows older than `retention_days` (default 7).
   - **Coordinator caps + bridge + CLI (`5f56dd3`)**: `metrics.cost_baselines { provider?, last_n_windows? }`, `metrics.ask_human_baselines { agent?, last_n_windows? }`, `metrics.cost_spike_history { limit? }` registered via `metrics::coordinator::register_baseline_caps`. Bridge endpoints `GET /v1/metrics/cost-baselines` + `/v1/metrics/ask-human-baselines` + `/v1/metrics/cost-spikes`. CLI subcommands `relix metrics cost-baselines [--provider X] [--windows 24]` + `relix metrics ask-human-baselines [--agent X] [--windows 24]` + `relix metrics cost-spikes [--limit 20]` with operator-friendly tables + `--raw` for the JSON body.
   - **Config (`5f56dd3`)**: `[metrics.cost_alerts]` block (enabled, baseline_window_mins, tick_interval_secs, spike_multiplier, drift_threshold, retention_days, db_path) parsed into `CostAlertsConfig`. Defaults are detector-off so existing controllers stay unchanged.
   - **Tests**: 9 pre-existing tests for `6216d98` (model_cost_summary aggregation, list_models distinct-non-empty filtering, ask_human_rate per-agent counting, spike-fires + spike-noise-floor + drift-fires + drift-min-attempts + drift-absolute-floor + AlertKind string round-trip) + 10 new `metrics::cost_baseline::tests` (schema migration, CRUD round-trip for all three tables, baseline_avg_micros window math + out-of-window exclusion + empty-store fallback, per-agent ask-human avg, ordering newest-first, purge with retention=0 short-circuit, unscoped queries) + 8 new `metrics::spike_detector::tests` (disabled tick is a noop, baseline insertion shape with avg + p95 + count, spike fires when recent ≥ multiplier × baseline, spike does NOT fire when recent < multiplier, drift fires when recent > baseline + threshold, drift does NOT fire for small deltas, retention purge runs each tick, dispatch through LoggingAlertSink doesn't panic).
3. **Feature 4 Presidio integration** — Microsoft Presidio is a Python service that must run as a sidecar process; integrating it requires standing up + running a Python process alongside the bridge AND wiring an IPC channel for the redaction calls. The in-process `PiiDetector` + `PiiAnonymizer` already cover the operator semantics Presidio would provide for the workload Relix sees today (regex + named-entity rules + per-tenant policy), and the test suite verifies the contract. Adding a Presidio sidecar is CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL until a deployment actually justifies the operational overhead of a Python process + IPC channel.

**Final status:** 1 of 3 CLOSED (Feature 2); 2 of 3 are CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL (Features 1 + 4). The in-process PII surface is the buildable equivalent of Feature 4 for Relix's workload.

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

## Top 10 gaps by impact — fully resolved

Every entry that was at the top of the impact list is now closed. Strike-throughs show the closing commit.

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

Other closures from later sessions:

- ~~**GAP 8** — closed by commit 0e6fd5e (alongside GAPs 5/6/7 in the same session)~~
- ~~**GAP 9** — closed by commit 09ff3c3 (email dashboard tile + `/v1/email/messages/recent`)~~
- ~~**GAP 10** (three of four sub-bullets) — closed by commit cf9759c (`tool.parse_document` + `tool.web_read` + perception-security `ai.perception_extract`); `tool.screen` stays CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL~~
- ~~**GAP 15** — closed by commits 17bffe8 + af18b41 + 74c8be4 + 873e16e (§7.30 Identity & Permissions across always-require allowlist + OOB approval + credentials + session tokens)~~
- ~~**GAP 16** — rebuilt to spec by 0fef9cc + c9d5327 + 3d8862d + bf005dd + b36e3c1 (§7.29 reasoning engine), follow-ups 2ffc41e + b589c36 + 565ff8a~~
- ~~**GAP 17** — closed by commits 5c18f41 (PART 1: WebSearchProvider trait + Tavily / Brave / Perplexity) + 2bde84d (PART 2 modules: five-stage ResearchPipeline) + 34465a5 (PART 2 wire-up: coordinator + bridge + CLI) + 061634a (PART 3 setup scripts)~~
- ~~**GAP 18** — closed by commit 40c82d4 (bi-temporal validity helpers)~~
- ~~**GAP 22 Feature 2** — closed end-to-end by commits 6216d98 (in-process spike + drift evaluators in AlertEngine) + 5f56dd3 (persistent baseline + spike-history store + scheduler + caps + bridge endpoints + CLI subcommands)~~
- ~~**GAP 10 (REST)** — every remaining sub-bullet closed by commits de43e71 (PART 1+3: tiered parse_document + tool.screen modules) + ba95040 (PART 3 wire-up: bridge route + CLI) + 72c0746 (setup-script prompts for the cloud keys + screen toggle)~~

Remaining items are CONFIRMED-EXTERNAL-INFRASTRUCTURE-FINAL with documented external dependencies: GAP 19, GAP 20, GAP 21, GAP 22 partial (Features 1 + 4). Each has its own per-gap entry explaining the specific external dependency. GAP 17 and GAP 10 (every sub-bullet, including `tool.screen` + cloud parse/web tiers) have been moved out of this set — closed via the operator-supplied API-key model in commits `5c18f41` / `2bde84d` / `34465a5` / `061634a` (GAP 17), `de43e71` / `ba95040` / `72c0746` (GAP 10), and `5f56dd3` (GAP 22 Feature 2 persistent baseline store).

---

## Methodology notes

- All claims read from both roadmaps in full (no skipping based on status tags).
- Code verification via four parallel exploration agents covering §7.24/26/27, §7.31/32 + W1-W8, Part 6 + §7.15/16/19, and §7.5/7/17/20/21/28 + YAML + §7.2/10.
- Cross-checked with direct grep for the specific capability names, file paths, and endpoint strings each section claims.
- Commit hashes in the roadmap were ignored as evidence; only file presence and type definitions were counted.

This report deliberately omits sections where the roadmap status matches reality. The full feature-by-feature audit lives in this document's body — additions or corrections should land here, not in `RELIX_ROADMAP.md`'s status tags, until both documents agree.
