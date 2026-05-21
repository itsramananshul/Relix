# Relix — Full Context Briefing for Prompt Generation

> **How to use this document**: Paste this entire file into ChatGPT and say:
> "You are a prompt engineer for an AI coding agent that has full access to the Relix codebase.
> Based on this context, generate precise, detailed coding prompts I can queue to my agent.
> Each prompt must reference exact file paths, exact function names, exact crate names, and exact
> constraints from this document. Do not hallucinate paths. Do not suggest things already built."

---

## 1. What Relix Is

Relix is a **decentralized AI agent platform** built on top of OpenPrem's P2P infrastructure.

**Core differentiator**: No central gateway. Every node is a controller peer on the mesh.
There is no "hub" a client connects to. Every node discovers every other via libp2p + Kademlia DHT.

**Stack (inherited from OpenPrem INFRA)**:
- Transport: libp2p, TCP, Noise XK handshake, Yamux multiplexing
- RPC: CBOR-encoded, custom `/relix/rpc/1` protocol
- Orchestration: SOL — a domain-specific scripting language already built in OpenPrem
- Discovery: Kademlia DHT for peer discovery, NodeManifest for capability advertisement
- Identity: Ed25519 key pairs, CBOR-encoded IdentityBundle signed by org root key

**Inspiration sources analyzed (do not copy — design from analysis)**:
- **Hermes Agent** (by Anthropic): memory system, skill system, ReAct loop, context compression
- **OpenClaw** (134 extensions, browser automation, MCP client)
- **Open WebUI** (being forked as `relix-web/` under `RELIX_MODE=true`)

---

## 2. Architecture Invariants — NEVER VIOLATE THESE

These are hard constraints baked into every design decision:

1. **The responding node enforces.** Identity verification → policy check → handler → audit — this pipeline runs on the RESPONDER side, never centralized.
2. **AI provider keys live ONLY in the AI node's local config.** No other node, no web backend, no coordinator ever sees an LLM API key.
3. **The web backend in `RELIX_MODE` makes zero LLM provider calls.** It proxies to the bridge only.
4. **No routing decision outside SOL.** All multi-node orchestration must be expressed as SOL flows. Never hardcode a routing path in Rust.
5. **Adding a new channel (Telegram, Slack, Discord) requires zero changes to memory/AI/tool/web nodes.** Only a new binary + a new SOL flow.

---

## 3. Codebase Structure

**Root**: `D:\DATA\WORK\OpenPrem\Apps\Relix\` (workspace)

### Rust Crates (`crates/`)

| Crate | Purpose |
|-------|---------|
| `relix-core` | Codec, types, IdentityBundle, policy engine, EventLog, CapabilityDescriptor |
| `relix-controller` | Binary — boots, generates identity, registers capabilities, listens on libp2p |
| `relix-runtime` | SOL VM execution, FlowRunner, RemoteCall opcode dispatcher |
| `relix-web-bridge` | HTTP/SSE server peer; translates HTTP → RPC → SOL → HTTP; zero LLM calls |
| `relix-cli` | CLI: `identity`, `task`, `capability`, `flow-run` subcommands |
| `relix-flow-inspect` | Operator binary: reads audit log + flow log, replay-verify |
| `relix-telegram` | Telegram channel scaffold (MockBotApi complete; live HTTPS client needs bot token) |

### Nodes (controller instances configured for specific roles)

| Role | Capabilities |
|------|-------------|
| **memory node** | `memory.write_turn`, `memory.recent_for_session`, `memory.search` (SQLite + FTS5) |
| **ai node** | `ai.chat` streaming; providers: mock/openai/openrouter/xai/local/anthropic |
| **tool node** | `tool.web_fetch`, `tool.web_extract`, `tool.pdf`, `tool.read_file`, `tool.write_file`, `tool.search_files`, `tool.patch` |
| **coordinator node** | Task ledger: `task.create/update/event/get/list/count/list_cursor/recover/retry/export/compact_events` |
| **web-bridge** | HTTP/SSE/OpenAI-shim gateway peer |

### SOL Flows (`flows/`)

| Flow | Purpose |
|------|---------|
| `ping.sol` | Single-peer health check |
| `chained_health.sol` | Multi-peer chain health |
| `chat.sol` | Full conversational chat with memory |
| `chat_template.sol` | Template variant |
| `chat_with_tool.sol` | Chat + tool node invocation |
| `memory_demo.sol` | Memory read/write demo |

### Specs (`specs/`)
All 8 protocol specs are frozen: `RELIX-1-rpc.md` through `RELIX-8-flow.md`.

### Docs (`docs/`)
22 reference docs. Key ones:
- `phase-1-status.md` — single source of truth for what's done
- `alpha-plan.md` — day-by-day alpha plan with acceptance criteria
- `event-contract.md`, `task-api.md`, `capability-discovery.md`
- `bridge-invariants.md` — the 7 hard MUST-NOTs for the web bridge
- `plugin-foundations.md`, `chronicle-retention.md`
- `docs/internal/nightly-summary-20260520.md` — last session summary

---

## 4. Current Build State

**Git**: 169 commits, 395 tests passing, `cargo clippy --workspace --all-targets -- -D warnings` clean.

### ✅ FULLY COMPLETE (do not re-build)

**Transport + Identity**:
- libp2p `/relix/rpc/1` over TCP + Noise XK + Yamux
- Ed25519 IdentityBundle, org root signing, admission pipeline
- Allowlist policy engine, default-deny per method
- Hash-chained audit log + per-flow event log
- NodeManifest discovery, `capability:<method>` routing
- Reconnect-on-drop (A.4), 60s manifest refresh
- Pooled `MeshClient` (M11)

**All 5 node types** (memory, ai, tool, coordinator, web-bridge) — fully functional

**Task system** (8-state machine):
- States: `pending / running / retrying / interrupted / awaiting_input / completed / failed / cancelled`
- `task_attempts` table with lineage, trace_id propagation
- Recovery scan (startup + on-demand), operator retry with `--force` guard
- `FailureClass` taxonomy, bridge auto-classification

**Scale-grade event system (S1-S6)**:
- Cursor pagination (`task.list_cursor`)
- Typed event envelopes with schema_version, attempt_id, trace_id
- Experimental SSE (`/v1/tasks/:id/events/stream`)
- Chronicle retention design + dry-run candidate counter (no destructive deletion yet)
- 10k-task + 10k-event scale smoke tests

**Bridge HTTP surface (`/v1`)**:
- OpenAI shim: `GET /v1/models`, `POST /v1/chat/completions` (streaming + non)
- Native: `POST /chat`, `POST /chat/stream`, `POST /chat_with_tool`
- Tasks: full CRUD + lineage + export + SSE + compact_events
- `GET /dashboard` with live SSE chronology

**CLI**:
- `identity init-org`, `mint`, `ping`, `flow-run`
- `task create/update/event/get/list/count/attempts/recover/retry/watch/compact/export`
- `capability ls/get/validate`

**CapabilityDescriptor** with description, categories, environment_requirements on every capability.

**Bridge invariant canary tests** in `crates/relix-web-bridge/tests/invariants.rs`.

### ❌ NOT YET BUILT (next targets)

1. **Cedar policy engine** — Day 4 of alpha plan. Wire `cedar-policy` Rust crate into `relix-core` admission pipeline. Node-local policy bundles signed by org root. Audit records include policy decisions. Demo: unauthorized identity rejected before handler fires.

2. **Relix Web** — Day 5. Fork Open WebUI into `relix-web/` at repo root. Add `RELIX_MODE=true` env flag. Strip all provider plumbing under that flag. Add `relix_provider.py` that POSTs to the local web bridge. Verify: no `openai.` calls, no `anthropic.` calls from backend in `RELIX_MODE`.

3. **Skill system** — Not started. See Section 6 for the full Hermes design to adapt from.

4. **Agent-level memory (USER.md / MEMORY.md equivalent)** — Memory node has SQLite + FTS5 for session turns. Does NOT yet have persistent cross-session user profile or skill-indexed memory. See Section 6.

5. **Agent loop / context compression** — No ReAct loop or context compression in Relix yet. The AI node does single-turn `ai.chat`. See Section 6 for Hermes design.

6. **Live Telegram channel** — `crates/relix-telegram` scaffold is complete (MockBotApi, SessionStorage, SqliteSessionStore). Blocked on bot token from @BotFather. Once token is available: add `reqwest`-backed `BotApi` impl.

7. **Replay-verify** (`relix-flow-inspect --replay-verify`) — Inspect binary exists but replay-verify is not yet implemented.

8. **Chronicle destructive deletion** — Design in `chronicle-retention.md`. Gated on operator-export (which is now done). Step 3+ not yet built.

---

## 5. Alpha Acceptance Criteria (all must pass before alpha ships)

1. Four `relix-controller` processes discover each other via libp2p
2. Every cross-node RPC identity-verified on responder before handler
3. Every cross-node RPC produces audit record correlatable by `request_id`
4. `chat.sol` end-to-end from Relix Web with memory persistence + streamed tokens
5. `chat_with_tool.sol` end-to-end with real `web.fetch`
6. Anthropic key present ONLY in AI node config
7. Web backend zero LLM calls in `RELIX_MODE`
8. No routing outside SOL
9. Replay-verify reports integrity OK on recorded flow log
10. Killing any single node does not crash the mesh
11. Docs present and accurate
12. CI passes: `cargo test --workspace` + integration demo job
13. No marketplace code
14. No secrets in repo

---

## 6. Hermes Agent Deep-Dive Analysis (reference for building Relix equivalents)

This is a complete analysis of how Hermes Agent builds its memory, skill, and agent loop systems.
Use this to design Relix's equivalents — adapt the design, do not copy code.

### 6A. Memory System

**Storage**: Two Markdown files at `~/.hermes/memories/MEMORY.md` and `USER.md`.
Delimiter: `\n§\n` between entries.

**Limits**: 2,200 chars (MEMORY.md), 1,375 chars (USER.md). Char-based, NOT token-based.
Design reason: predictable, model-independent budget.

**Atomic write pattern** (use this in Relix memory node too):
```
tempfile.mkstemp() in same directory → write → fsync → os.replace()
```
Same filesystem = atomic rename. Cross-device never happens because temp is co-located.

**Security scan** on every memory write: 8 invisible unicode codepoints blocked (U+200B–U+2069) + 11 prompt injection regex patterns.

**File locking**: `fcntl.flock(LOCK_EX)` on Unix, `msvcrt.locking` on Windows.

**Frozen snapshot pattern** for prefix cache:
- `MemoryStore` has two states: live state + `_system_prompt_snapshot`
- Snapshot is built once per session start, frozen, never updated mid-session
- Injected into system prompt at the "volatile" tier (per-turn, but from frozen snapshot)
- Date-only timestamp (no minute precision) — keeps the prefix cache KV stable

**SQLite session DB** (`~/.hermes/state.db`, SCHEMA_VERSION=11):
- `sessions` table (30+ columns), `messages` table (20+ columns)
- Dual FTS5 index: `messages_fts` (unicode61 tokenizer) + `messages_fts_trigram` (trigram tokenizer)
- FTS indexed content: `COALESCE(content,'') || ' ' || COALESCE(tool_name,'') || ' ' || COALESCE(tool_calls,'')`
- WAL mode + `BEGIN IMMEDIATE` + 15 retries with 20-150ms random jitter
- Three-way CJK routing: standard FTS5 → trigram FTS5 → LIKE fallback

**Relix equivalent**: Memory node already has SQLite + FTS5 for session turns. Still needs:
- Cross-session user profile storage (USER.md equivalent)
- Frozen snapshot injection mechanism in the AI node system prompt builder
- Invisible unicode + injection pattern security scan on memory writes

---

### 6B. Skill System

**File layout**: `~/.hermes/skills/<name>/SKILL.md` or `~/.hermes/skills/<category>/<name>/SKILL.md`

**SKILL.md format** (required frontmatter):
```yaml
---
name: my-skill
description: What this skill does (max 1024 chars)
version: "1.0.0"
platforms: [darwin, linux, windows]
prerequisites:
  env_vars: [API_KEY]
metadata:
  hermes:
    tags: [tag1, tag2]
    related_skills: [other-skill]
---
Body text with instructions (non-empty after strip)
```

**Validation on create**:
1. Name regex: `^[a-z0-9][a-z0-9._-]*` (no uppercase, no spaces, no leading hyphen/dot), max 64 chars
2. Category validation: same regex, blocks `/` and `\` to prevent traversal
3. Frontmatter YAML parse + required field check
4. Content size: max 100,000 chars (~36K tokens)
5. Collision check via `rglob("SKILL.md")` across all skill roots
6. `mkdir(parents=True, exist_ok=True)` at skill dir path
7. Atomic write: tempfile in same dir → `os.replace()`
8. Optional security scan (guard)
9. Invalidate system prompt cache so next turn rebuilds with updated skill list

**CRUD actions**: `create`, `edit` (full replace), `patch` (fuzzy find-and-replace), `delete`, `write_file`, `remove_file`

**Allowed support file subdirectories**: `references/`, `templates/`, `scripts/`, `assets/`

**Deletion guard**: reads `pinned` flag from `.usage.json` before `shutil.rmtree()`

**Progressive disclosure**:
- `skills_list()`: reads only first 4,000 chars per SKILL.md (metadata scan, no token explosion)
- `skill_view()`: full content + env var injection + 4 lookup strategies

**Security scanner** (60+ patterns, 10 categories):
- Exfiltration: `~/.ssh`, `~/.aws`, env dumping, DNS exfil
- Injection: role hijack, DAN mode, educational pretext
- Destructive: `rm -rf /`, `shutil.rmtree`
- Persistence: crontab, `.bashrc`, `authorized_keys`
- Mining: xmrig, stratum+tcp
- Supply chain: `curl|bash`, unpinned pip/npm
- Credentials: hardcoded `sk-`, `sk-ant-`, `AKIA`, private key headers

**Trust matrix**:
```
builtin:       (allow, allow, allow)
trusted:       (allow, allow, block)
community:     (allow, block, block)
agent-created: (allow, allow, ask)
```
`community` blocked on "caution"; `agent-created` only asks on "dangerous".

**ContextVar for provenance**: Python `contextvars.ContextVar` scopes write origin per-coroutine.
Background review forks each get their own context — parallel agent runs don't contaminate each other.
**Rust equivalent**: `tokio::task_local!()` macro.

**Bundled skill sync** (4-case content-addressed algorithm with MD5 hashes):
1. Not in manifest, dest exists, same hash → record hash, skip
2. Not in manifest, dest missing → copy, record hash
3. In manifest, dest exists, user modified → skip (preserve user changes)
4. In manifest, dest exists, bundled updated → backup + overwrite + update hash
5. In manifest, dest deleted → respect deletion, don't re-copy

**Usage telemetry** (`.usage.json` sidecar, not inside skill dirs):
- Bundled and hub-installed skills are EXCLUDED from telemetry
- Only agent-created skills accumulate use_count, view_count, patch_count
- States: `active`, `stale`, `archived`
- Archive: `os.rename()` to `.archive/<name>/` (atomic on same FS)
- Restore: original category NOT reconstructed (known limitation — lands flat)

---

### 6C. Agent Loop (ReAct)

**Three-tier system prompt**:
1. `stable` — built once at agent init (model instructions, tool schemas)
2. `context` — session-stable (session ID, user profile from USER.md)
3. `volatile` — per-turn (memory snapshot injected here, ephemeral reminders)

**Context compression** (fires at 50% context window):

Three-pass tool result pruning (NO LLM needed, cheap):
1. MD5 dedup — keep newest copy of identical tool results, replace older with note
2. 1-line summaries for old tool results: `[terminal] ran 'npm test' → exit 0, 47 lines`
3. JSON-aware truncation of oversized tool call args (200-char head kept)

LLM summarization prompt (12 labeled sections):
- Active Task (copy user's EXACT request verbatim — this is the most important field)
- Completed Actions (numbered, with tool names + outcomes)
- Active State (working dir, branch, modified files, test status)
- In Progress / Blocked (with EXACT error messages) / Key Decisions
- Pending User Asks / Relevant Files / Remaining Work

**Critical bug fix to implement from day 1**:
The last user message MUST always be in the protected tail — even if it exceeds the token budget.
Without this, the active task can be summarized away and the agent stalls.

**Anti-thrashing**: track ineffective compression count.
If 2 consecutive compressions save <10% → pause compression until `/new` or topic-scoped compress.

**Compression threshold uses ONLY prompt tokens**, NOT total tokens.
Reasoning models (QwQ, R1, DeepSeek) inflate completion tokens with thinking tokens that don't consume context window space. Using total tokens triggers premature compression.

**Tool dispatch loop**:
1. Fuzzy-repair typos in tool names BEFORE returning error to model
2. JSON arg validation — empty string → `{}`, truncated (doesn't end `}`) → error
3. Dedup tool calls, cap delegate-task calls
4. Dispatch
5. Guardrail halt check
6. Context compression check (AFTER tool execution, not before)

**Tool result pruning summaries** (build these for Relix tools):
- `tool.read_file`: `[read_file] read config.rs from line 1 (3,400 chars)`
- `tool.web_fetch`: `[web_fetch] fetched https://example.com → 200 OK, 8,200 chars`
- `tool.patch`: `[patch] patched src/lib.rs — 2 replacements`
- `tool.search_files`: `[search_files] found 12 matches for 'TODO' in *.rs`

**Empty response recovery order** (7 paths before giving up):
1. Partial stream recovery (content streamed before disconnect)
2. Prior housekeeping content (memory/todo wrote something)
3. Post-tool nudge (append synthetic empty + user nudge, retry)
4. Thinking-only prefill retry (max 2)
5. Empty retry (max 3)
6. Fallback provider
7. `"(empty)"` final

**Budget exhaustion handling**: when iteration budget hits zero, strip tools from API kwargs and make ONE more call asking model to summarize what it accomplished and what remains. Return that as the final response.

---

## 7. What to Build Next (in order)

### Priority 1: Cedar Policy Engine (Day 4 of alpha plan)

**Crate**: `cedar-policy` (Rust crate, `cedar-policy = "4"` or latest)
**Where to wire it**: `crates/relix-core/src/policy.rs` — the `PolicyEngine` trait + current allowlist impl
**What to add**:
- `CedarPolicyEngine` struct implementing `PolicyEngine` trait
- Load policy bundle from `<node-data-dir>/policy.cedar` signed by org root key
- Wire into the 4-step admission pipeline: identity → **cedar policy** → handler → audit
- Audit records must include: which policy rule matched, the Cedar decision, the principal entity
- Entity types: `Relix::Principal` (peer_id), `Relix::Resource` (capability method), `Relix::Action` (call)
- Demo: create a policy file that denies `Relix::Action::"call"` for peers not in `Relix::Group::"tool-users"` group; show rejection in audit log

**Do NOT**: replace the existing allowlist engine — add Cedar as an opt-in layer alongside it.

### Priority 2: Relix Web (Day 5 of alpha plan)

**Location**: `relix-web/` at repo root (NOT inside `crates/`)
**Source**: Fork Open WebUI (latest tag)
**What to change**:
- Add `RELIX_MODE` env variable check in `backend/config.py`
- Under `RELIX_MODE=true`: disable all direct provider connections (OpenAI, Anthropic, Ollama, etc.)
- Add `backend/apps/relix/provider.py` with a single function `chat_completion(messages, stream)` that POSTs to `http://localhost:8080/v1/chat/completions` (the web bridge OpenAI shim)
- Wire `relix_provider.py` into the chat completion path when `RELIX_MODE=true`
- Add `Dockerfile.relix` that sets `RELIX_MODE=true` and `BRIDGE_URL` env vars
- Verification test: `grep -r "openai\." backend/ | grep -v "relix"` should return nothing in RELIX_MODE paths

**Keep**: all Open WebUI UI, session management, user auth, markdown rendering
**Strip**: provider selection UI, API key entry fields (hide under RELIX_MODE)

### Priority 3: Skill System for Relix

**What it is**: A way for the agent to create, edit, and invoke "skills" — SKILL.md files that tell the agent how to accomplish a category of tasks.

**Relix-native design** (adapted from Hermes analysis):
- Skills stored in the memory node's SQLite DB (not files) — fits the P2P model
- Capability: `skill.create`, `skill.get`, `skill.list`, `skill.patch`, `skill.delete`
- SKILL.md format: same YAML frontmatter as Hermes (name, description, version, platforms)
- Security scan: port the 60+ threat patterns from Hermes to Rust (compile to `Regex` set on startup)
- Trust levels: `builtin` (shipped with Relix), `user-created`, `agent-created`, `hub-installed`
- Progressive disclosure: `skill.list` returns metadata only; `skill.get` returns full content
- Cache invalidation: every write to skills must signal the AI node to rebuild its system prompt context
- **ContextVar equivalent**: use `tokio::task_local!()` to scope write origin per-task so parallel agent runs don't contaminate each other's provenance tracking

### Priority 4: Agent-Level Context Compression

**Where**: `crates/relix-runtime/src/compressor.rs` (new file)

**What to build** (adapted from Hermes ContextCompressor):
- `ContextCompressor` struct with `threshold_percent: f64` (default 0.50), `protect_last_n: usize` (default 20)
- Three-pass pruning (no LLM): dedup identical tool results by SHA-256 → 1-line summaries → truncate args
- LLM summarization: 12-section structured prompt (see Section 6C above)
- CRITICAL: last user message always in protected tail (bug fix to build in from start)
- Anti-thrashing: track `ineffective_compression_count`; pause after 2 consecutive <10% saves
- Token counting: use ONLY prompt token count from API response, never total tokens

### Priority 5: Live Telegram Channel

**Blocker**: bot token from @BotFather
**Once available**:
- Add `reqwest`-backed `BotApi` impl in `crates/relix-telegram/src/client.rs`
- Wire into `relix-telegram` binary's `main.rs`
- Create `flows/telegram_chat.sol` — same pattern as `chat.sol` but sourced from Telegram message
- No changes to memory/AI/tool nodes required (invariant 4)

---

## 8. Key Design Decisions Already Made (don't re-debate)

| Decision | Rationale |
|----------|-----------|
| SOL as the ONLY routing surface | Auditable, replayable, no routing logic scattered in Rust |
| CBOR for all RPC payloads | Binary-efficient, schema-describable via CDDL, native to OpenPrem |
| SQLite for memory + coordinator | Embedded, no separate process, WAL mode for concurrent reads |
| FTS5 dual index (unicode61 + trigram) | unicode61 for English/most languages, trigram for CJK and partial matches |
| Cedar for policy | Deterministic, bounded execution time, auditable, Rust-native |
| Open WebUI fork not rewrite | 80% of the UI is identical; minimal diff under RELIX_MODE |
| Ed25519 for identity | Fast verification, compact signatures, already in OpenPrem |
| No central gateway | Every node self-enforces; gateway is a SPOF and a trust bottleneck |

---

## 9. Things That Are Explicitly Out of Scope

Do NOT suggest or build:
- Marketplace
- Central gateway
- LLM credentials in web backend or coordinator
- Routing decisions in Rust (only in SOL)
- HSM, IA hierarchy, federation
- SolFlow live mode
- Mobile peers
- Voice, image generation
- General MCP client
- Dynamic tool discovery
- Autonomous retry daemon / task-leasing / executor election
- Multi-bridge load balancing
- Cross-trust-root audit correlation

---

## 10. How to Write Good Prompts for This Agent

The agent has full read/write access to `D:\DATA\WORK\OpenPrem\Apps\Relix\` and a Linux shell.

A good prompt:
1. Names the exact crate and file to create or modify
2. Names the exact trait or struct to implement
3. References an existing file as a pattern (e.g., "follow the same pattern as `crates/relix-core/src/policy.rs`")
4. States the architecture invariants that must be preserved
5. Specifies the test to add or the CLI demo that should work when done
6. Does NOT ask the agent to "figure out" the architecture — it already knows it

Example of a GOOD prompt:
> "In `crates/relix-core/src/policy.rs`, add a `CedarPolicyEngine` struct that implements
> the existing `PolicyEngine` trait. Use the `cedar-policy` crate (add to `crates/relix-core/Cargo.toml`).
> Load policy from `<node-data-dir>/policy.cedar`. Wire it into the admission pipeline in
> `crates/relix-controller/src/admission.rs` after identity verification and before the handler.
> Add a test in `crates/relix-core/tests/cedar_policy.rs` that: (1) creates a Cedar policy denying
> all calls from peer `test-peer-1`, (2) runs the admission pipeline with that identity, (3) asserts
> the result is `PolicyDenied`. Preserve the existing allowlist engine — Cedar is additive."

Example of a BAD prompt:
> "Add Cedar policy to Relix" (too vague, agent will have to guess everything)
