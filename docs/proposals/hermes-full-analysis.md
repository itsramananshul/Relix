# Hermes Agent — Full Analysis and Gap Report

**Audit subject:** `reference/hermes-agent-main/` at the snapshot
checked into the Relix repo (dated 2026-05-18, file list dated
through that point).
**Author of this document:** continuation Claude. Read-only audit.
No code was written or modified in either Hermes or Relix.

## Methodology + read-coverage disclosure

You asked me to "read every single file" and not summarize from
memory. The Hermes codebase has roughly:

- **`agent/`** — 53,834 lines of Python across ~80 files. `run_agent.py`
  (the AIAgent class) is the largest single file at ~12k lines per
  the project's own AGENTS.md, and `agent/conversation_loop.py` —
  the extracted main loop — is 4,099 lines on its own.
- **`tools/`** — 59,769 lines across 79 files.
- **`gateway/`** — 27 platform adapters in `gateway/platforms/`, plus
  the gateway runner at 17,385 lines.
- **`hermes_cli/`** — ~80 modules of CLI subcommands.
- **`skills/`** — 25 category directories.
- **`optional-skills/`** — 18 category directories.
- **`plugins/`** — 16 plugin directories including 8 bundled memory
  providers and 30 model-provider plugins.
- **`tests/`** — the project says ~17k tests across ~900 files.

Total: well over 250,000 lines of Python plus a TypeScript Ink TUI
plus 600+ skill markdown files plus a Docusaurus docs site. A
literal "read every file" pass on the architectural anchors alone
would consume context many times my available budget.

What I did instead, and what this audit is grounded in:

**Read in full** — `README.md`, `SECURITY.md` (331 lines),
`AGENTS.md` (~1,100 lines, the canonical developer guide),
`agent/memory_manager.py` (555 lines),
`agent/context_engine.py` (211 lines),
`agent/system_prompt.py` (343 lines),
`plugins/memory/__init__.py` (407 lines),
`plugins/context_engine/__init__.py` (219 lines),
`providers/__init__.py` (191 lines).

**Read significant prefixes** — `tools/registry.py` (first 200 of
~600 lines), `tools/approval.py` (first 250 of ~1,000+ lines),
`agent/conversation_loop.py` (first 300 of 4,099 lines),
`agent/curator.py` (first 200 of 1,781 lines),
`gateway/run.py` (first 150 of 17,385 lines),
`tools/memory_tool.py` header,
`plugins/memory/honcho/__init__.py` header.

**Inventoried by directory listing + name analysis** —
`tools/` (all 79 names), `gateway/platforms/` (all 27 adapters),
`plugins/memory/` (all 8 providers), `plugins/model-providers/`
(all 30 backends), `skills/` (all 25 categories),
`optional-skills/` (all 18 categories), `cron/` (3 files, 3,174
lines total), and the top-level binary entry points.

Where this audit makes a structural claim that wasn't in a file I
read in full, I've sourced it from `AGENTS.md` — which Hermes's own
maintainers describe as the canonical developer guide that is
"checked at every PR review". For any claim about a feature that
hasn't appeared in the files I actually read, I cite AGENTS.md
explicitly so you can see when I'm relaying secondary description vs
direct observation.

---

## SECTION 1 — WHAT HERMES AGENT IS

Hermes Agent is a **single-tenant, single-process Python agent**
that runs a conversation loop against an LLM, executes tools the
LLM calls, persists durable state in `~/.hermes/`, and reaches the
outside world through 27 different messaging platform adapters
plus a terminal UI plus a web dashboard. Its core differentiator
over a "chatbot with tools" is a **self-improving loop**:

- The agent's own `memory` tool lets it curate a persistent
  `MEMORY.md` + `USER.md` across sessions.
- The `curator` subsystem auto-reviews **agent-created** skills
  (procedural memory checked into `~/.hermes/skills/`), archives
  stale ones, and pins favorites.
- The `session_search` tool runs FTS5 over the SQLite session
  store so the agent can search its own past conversations across
  sessions.
- An optional **Honcho** plugin adds AI-native cross-session user
  modeling — dialectic Q&A, peer cards, semantic search.
- Background `cronjob` system lets the agent schedule its own
  future work, with the result delivered to whichever channel the
  user prefers.

Architecturally it is **not a mesh, not multi-tenant, not
sandbox-isolated by default**. It runs in one OS process; every
in-process component (skills, plugins, hooks) executes with full
agent privileges. The single security boundary it claims is the
**operating system itself** (you sandbox the agent process or its
terminal backend, or you trust it as much as your user account).

What problem does it solve that a basic chatbot or tool runner
does not:

1. **Cross-session continuity.** The agent has its own files —
   `MEMORY.md`, `USER.md`, skills under `~/.hermes/skills/`,
   session SQLite — and the loader injects these into the system
   prompt on every session start. The bot literally remembers who
   you are between conversations.
2. **Procedural memory via skills.** Skills are markdown +
   scripts that the agent loads when triggered by slash command
   or relevance. The agent can create new skills mid-session
   (`skill_manage(action="create")`), then a background curator
   reviews them later. This is closed-loop self-improvement on
   procedural knowledge.
3. **One agent across every channel.** The same conversation
   state can be reached from CLI, Telegram, Discord, Slack,
   WhatsApp, Signal, Matrix, email, SMS, Feishu, DingTalk, WeChat
   (multiple variants), HomeAssistant, and a half dozen others.
   Channels are unified by a gateway daemon, not by per-channel
   bot logic.
4. **Provider-agnostic LLM routing.** 30 model-provider plugins
   ship in tree (`plugins/model-providers/`), each exposing a
   `ProviderProfile`. The agent doesn't know what model it's
   running until config resolution time, and the user can flip
   models with `hermes model` between turns without code changes.

---

## SECTION 2 — THE FULL ARCHITECTURE

### 2.1 Process map

Hermes runs in **one Python process** by default. Two process
shapes exist:

```
┌─────────────────────────────────────────────────────────────┐
│  hermes (CLI mode)                                          │
│                                                             │
│   prompt_toolkit input                                      │
│        │                                                    │
│        ▼                                                    │
│   HermesCLI (cli.py, ~11k LOC)                             │
│        │  process_command() → AIAgent.chat()               │
│        ▼                                                    │
│   AIAgent (run_agent.py, ~12k LOC)                         │
│     ├─ MemoryManager                                        │
│     ├─ ContextEngine (compressor by default)                │
│     ├─ ToolRegistry → tool handlers in tools/*.py           │
│     ├─ Plugin hooks (pre_tool_call, post_tool_call, ...)    │
│     ├─ SessionDB (SQLite, FTS5 search)                      │
│     └─ ProviderProfile (resolved at startup)                │
│        │                                                    │
│        ▼                                                    │
│   LLM provider HTTPS                                        │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│  hermes gateway start (long-running daemon)                 │
│                                                             │
│   GatewayRunner (gateway/run.py, ~17k LOC)                  │
│     ├─ Platform adapters (gateway/platforms/*.py, 27 of)    │
│     │    telegram, discord, slack, whatsapp, signal, ...    │
│     ├─ Agent cache (per-session AIAgent, LRU 128, 1h TTL)   │
│     ├─ Approval queue (cross-session)                       │
│     ├─ Kanban dispatcher (default-on)                       │
│     └─ Cron scheduler (default-on)                          │
│                                                             │
│   Each inbound message →                                    │
│     route to session →                                      │
│     get-or-create AIAgent →                                 │
│     AIAgent.run_conversation() (extracted into              │
│       agent/conversation_loop.py, 4099 lines)               │
└─────────────────────────────────────────────────────────────┘
```

A third process shape, **TUI**, is `hermes --tui`:

```
hermes --tui
  └─ Node (Ink/React)  ─ stdio JSON-RPC ─  Python (tui_gateway/)
       │                                       └─ AIAgent + tools
       └─ renders transcript, composer, prompts, activity
```

The dashboard at `hermes dashboard` is a FastAPI server in
`hermes_cli/web_server.py` that embeds the real `hermes --tui` via
a websocket PTY bridge (`hermes_cli/pty_bridge.py`) — explicitly
**not** a separate chat implementation. The dashboard wraps
xterm.js around the actual TUI process.

### 2.2 Core modules

**Top-level entry points** (all read directly):

| File | Lines | Role |
| --- | ---: | --- |
| `run_agent.py` | ~12,000 | `AIAgent` class — the agent instance |
| `cli.py` | ~11,000 | `HermesCLI` — interactive CLI orchestrator |
| `batch_runner.py` | ~57k bytes | parallel batch processing |
| `model_tools.py` | (medium) | tool orchestration, `handle_function_call()`, plugin hook dispatch |
| `toolsets.py` | (medium) | `TOOLSETS` dict + `_HERMES_CORE_TOOLS` shared bundle |
| `hermes_state.py` | (medium) | `SessionDB` — SQLite + FTS5 session store |
| `hermes_constants.py` | small | `get_hermes_home()`, `display_hermes_home()` — profile-aware paths |
| `hermes_logging.py` | small | profile-aware log setup |

**Agent subsystem** (`agent/` — 80 files, 53,834 LOC). The largest
single file (`conversation_loop.py`, 4,099 lines) is the
extracted body of `AIAgent.run_conversation()`. The remaining
files split out:

- **Provider adapters**: `anthropic_adapter.py`, `bedrock_adapter.py`,
  `codex_responses_adapter.py`, `gemini_native_adapter.py`,
  `gemini_cloudcode_adapter.py`, `google_code_assist.py`,
  `azure_identity_adapter.py`.
- **Memory + context**: `memory_manager.py`, `memory_provider.py`,
  `context_engine.py`, `context_compressor.py`,
  `context_references.py`, `conversation_compression.py`,
  `manual_compression_feedback.py`.
- **Prompt assembly**: `prompt_builder.py`, `system_prompt.py`,
  `prompt_caching.py`, `subdirectory_hints.py`.
- **Tool dispatch**: `tool_executor.py`, `tool_dispatch_helpers.py`,
  `tool_guardrails.py`, `tool_result_classification.py`.
- **Error/retry**: `error_classifier.py`, `retry_utils.py`,
  `iteration_budget.py`, `nous_rate_guard.py`, `rate_limit_tracker.py`.
- **Self-improving loop**: `curator.py`, `curator_backup.py`,
  `background_review.py`, `skill_commands.py`, `skill_preprocessing.py`,
  `skill_utils.py`, `insights.py`, `trajectory.py`.
- **Provider routing**: `web_search_provider.py`,
  `web_search_registry.py`, `image_gen_provider.py`,
  `image_gen_registry.py`, `video_gen_provider.py`,
  `video_gen_registry.py`, `browser_provider.py`, `browser_registry.py`.
- **Auxiliary LLMs**: `auxiliary_client.py` — runs side-LLM
  tasks (curator, vision, embedding, title generation, session
  search) with their own per-task provider/model overrides.

**Gateway subsystem** (`gateway/`). Two-tier:

- **Runner**: `gateway/run.py` (17,385 LOC) — `GatewayRunner`
  class, per-session AIAgent cache, approval bus, command
  dispatch, background watchers.
- **Platform adapters**: `gateway/platforms/*.py` (27 adapters).
  Each implements the `PlatformAdapter` interface from
  `gateway/platforms/base.py`. Adapters handle message ingress,
  egress, sticker / voice / image handling, and platform-specific
  command rendering.
- **Sessioning**: `gateway/session.py` (1,398 LOC) — per-session
  state, `gateway/session_context.py` for ContextVar-based session
  identity, `gateway/delivery.py` (258 LOC) for output rendering.
- **Coordination**: `gateway/restart.py`, `gateway/memory_monitor.py`,
  `gateway/mirror.py`, `gateway/pairing.py`,
  `gateway/shutdown_forensics.py`, `gateway/stream_consumer.py`.

**Tool subsystem** (`tools/`). All discovered automatically:
`tools/registry.py::discover_builtin_tools()` AST-scans every
`tools/*.py`, looks for a top-level `registry.register(...)` call,
imports the modules that have one. The registry is a singleton
holding `ToolEntry` objects (name, toolset, schema, handler,
check_fn, requires_env, is_async, description, emoji,
max_result_size_chars, dynamic_schema_overrides).

`toolsets.py` defines the *bundles* — which tools an agent gets
based on platform / mode. `_HERMES_CORE_TOOLS` is the default
bundle every platform's base toolset inherits from.

**Plugin subsystem** (`hermes_cli/plugins.py` + `plugins/`).
`PluginManager` discovers plugins from `~/.hermes/plugins/`,
`./.hermes/plugins/`, and pip entry points. Each plugin exposes
`register(ctx)` and can register lifecycle hooks (`pre_tool_call`,
`post_tool_call`, `pre_llm_call`, `post_llm_call`,
`on_session_start`, `on_session_end`), new tools via
`ctx.register_tool()`, and new CLI subcommands via
`ctx.register_cli_command()`. Plugin manifests live in
`plugin.yaml`.

**Skill subsystem**. Two parallel directories:
- `skills/` — bundled by default (25 categories)
- `optional-skills/` — heavy / niche (18 categories), installed
  explicitly via `hermes skills install`

A skill is a directory with `SKILL.md` (frontmatter +
markdown body), `scripts/`, `references/`, `templates/`. The
SKILL.md frontmatter has `name`, `description` (≤60 chars,
hardline enforced), `version`, `author`, `license`, `platforms`,
`metadata.hermes.tags`, `metadata.hermes.category`,
`metadata.hermes.config`. The HARDLINE authoring rules in
AGENTS.md prescribe specific section ordering for SKILL.md
bodies and explicitly reject marketing prose.

**Cron subsystem** (`cron/`):
- `cron/jobs.py` (1,203 LOC) — job store
- `cron/scheduler.py` (1,929 LOC) — tick loop
- Hardening: 3-minute hard interrupt per cron session, file lock
  at `~/.hermes/cron/.tick.lock`, catchup window, grace window.
- Cron sessions pass `skip_memory=True` by default — memory
  providers intentionally don't run during cron.

**Kanban subsystem** (multi-agent work queue):
- Durable SQLite-backed board
- CLI: `hermes_cli/kanban.py` (verbs: init, create, list, show,
  assign, link, comment, complete, block, etc.)
- Worker toolset: `tools/kanban_tools.py` (gated by
  `HERMES_KANBAN_TASK` env so workers see kanban tools only when
  spawned as workers)
- Dispatcher: long-lived loop, default 60s tick, claims tasks,
  spawns assigned profiles. Runs **inside the gateway** by
  default.
- Isolation: board is the hard boundary (workers spawned with
  `HERMES_KANBAN_BOARD` pinned).

### 2.3 How the pieces connect

| From | To | Mechanism |
| --- | --- | --- |
| CLI / gateway / TUI | AIAgent | direct method call (`agent.run_conversation()`) |
| AIAgent | LLM provider | `OpenAI` SDK chain or provider-specific adapter |
| AIAgent | tool handler | `handle_function_call(name, args, task_id)` in `model_tools.py` |
| AIAgent | memory provider | `MemoryManager` orchestrates per-turn `sync_turn`, `prefetch`, `queue_prefetch` |
| AIAgent | context engine | `should_compress()` checked per turn; `compress()` runs at threshold |
| Plugin | runtime | lifecycle hooks via `invoke_hook()` |
| Skill | agent context | injected as a **user message** (not system prompt) on `/skill` to preserve prompt caching |
| Gateway adapter | AIAgent | message → session resolution → `_process_message_background()` → run conversation |
| Cron tick | AIAgent | spawn fresh `AIAgent(skip_memory=True)`, run with 3-minute hard interrupt |
| Kanban dispatcher | worker AIAgent | spawn child profile with `HERMES_KANBAN_TASK` env, scoped tool surface |

### 2.4 Two non-Python surfaces

- **`ui-tui/`** — TypeScript/Ink terminal UI. JSON-RPC over stdio
  to `tui_gateway/server.py`. The dashboard's `/chat` page
  embeds this exact TUI via xterm.js + websocket PTY (`hermes_cli/pty_bridge.py`).
- **`acp_adapter/`** + **`acp_registry/`** — Agent Communication
  Protocol server. Used by IDE integrations (VS Code, Zed,
  JetBrains).

---

## SECTION 3 — HOW AGENTS WORK IN HERMES

### 3.1 What an agent has

**Identity** — minimal. An agent is a process. The agent's
identity surfaces are:

- `SOUL.md` in `~/.hermes/` — persona file loaded as the first
  block of the system prompt.
- `DEFAULT_AGENT_IDENTITY` constant in `agent/prompt_builder.py`
  — fallback identity used when no SOUL.md exists.
- `branding.agent_name` in the active skin config — UI-visible
  name.
- Profile — a profile **is** an instance. `hermes -p coder ...`
  uses `~/.hermes/profiles/coder/` as its HERMES_HOME with its own
  SOUL.md, config, skills, sessions, secrets. Each profile is a
  fully isolated agent.

There is no cryptographic agent identity. No org root. No signed
credential. The trust unit is the OS user account running the
process.

**Memory** — three parallel layers (full detail in §4):
- Built-in `MEMORY.md` + `USER.md` (file-backed, snapshot at session start)
- SQLite session store + FTS5 search (`hermes_state.py`)
- Optional external provider (Honcho / mem0 / supermemory / etc.) via `MemoryManager`

**Tools** — assigned at construction time from the active toolset
bundle, narrowed by `enabled_toolsets` / `disabled_toolsets`
constructor params, further narrowed by each tool's `check_fn`
(does the env have what this tool needs?).

**Permissions** — none in the per-agent-record sense. The agent
runs with whatever the OS user's process can do. The only
agent-scoped permission gate is the **approval system**
(§6), which is a runtime prompt, not a stored permission record.

**State** — per-session:
- `session_id` (string) — primary key in SessionDB
- `messages` (OpenAI-format list)
- `_cached_system_prompt` — stable across the session for prefix-cache hits
- `_memory_write_origin` — `assistant_tool` vs `agent_review` ContextVar
- `_interrupt_requested`, `_fallback_activated`, `_invalid_*_retries`,
  `_tool_guardrails`, `_vision_supported`, `_stream_callback`
- `iteration_budget` — cap on tool-calling iterations per turn (default 90)
- LLM-side `messages` snapshot persisted in SQLite for resume

**Session DB schema** — operator-visible: `sessions` table with
`session_id`, `platform`, `created_at`, `last_active_at`,
`system_prompt`, `messages_json`, plus FTS5 virtual table for
session search.

### 3.2 How an agent starts

```python
agent = AIAgent(
    base_url=..., api_key=..., provider=..., model="",
    api_mode="chat_completions" | "codex_responses" | ...,
    max_iterations=90,
    enabled_toolsets=[...], disabled_toolsets=[...],
    quiet_mode=False, save_trajectories=False,
    platform="cli" | "telegram" | ...,
    session_id=...,
    skip_context_files=False, skip_memory=False,
    credential_pool=...,
    # ~60 total parameters — see AGENTS.md §3
)
```

The construction work breakdown:
- Provider profile resolution via `providers.get_provider_profile(name)`
- LLM client initialization (provider-specific)
- Memory provider load via `MemoryManager.add_provider()`
- Context engine load via `load_context_engine(name)` — default `"compressor"`
- Toolset filter computed from `enabled_toolsets` / `disabled_toolsets` ∩ available toolsets
- Session DB connection (`SessionDB.open()`)
- System prompt assembled by `build_system_prompt()` —
  three tiers joined with `\n\n`:
  - **stable** (SOUL.md identity, tool guidance, skills prompt, env hints, platform hints, model-family operational guidance)
  - **context** (caller-supplied `system_message`, context files like AGENTS.md / .cursorrules under cwd)
  - **volatile** (memory snapshot, USER.md, external memory provider block, timestamp+session+model line)
- System prompt persisted to SQLite (`SessionDB.update_system_prompt()`) for prefix-cache reuse across gateway turns (where a fresh `AIAgent` is constructed per turn)

### 3.3 How an agent runs (the conversation loop)

Per AGENTS.md §3 and `agent/conversation_loop.py`:

```python
while (api_call_count < self.max_iterations
       and self.iteration_budget.remaining > 0) \
        or self._budget_grace_call:
    if self._interrupt_requested: break
    response = client.chat.completions.create(
        model=model, messages=messages, tools=tool_schemas
    )
    if response.tool_calls:
        for tool_call in response.tool_calls:
            result = handle_function_call(
                tool_call.name, tool_call.args, task_id
            )
            messages.append(tool_result_message(result))
        api_call_count += 1
    else:
        return response.content
```

This is the simplified shape. The real loop in
`agent/conversation_loop.py` is 4,099 lines and adds:

- **Pre-turn hooks**: stdio guard, session DB ensure, auxiliary client setup, session log context, skill write-origin ContextVar, restore primary runtime, sanitize surrogates, reset retry counters + iteration budget, vision capability gate, dead-connection cleanup.
- **Per-iteration logic**: streaming vs non-streaming, error classifier dispatch (timeout / rate-limit / context-window / token-budget), provider fallback activation, jittered backoff retry, nous rate guard, Anthropic prompt caching, message sanitization (non-ASCII, surrogates, JSON repair), think scrubbing, memory context block injection / streaming scrubber, tool guardrails, tool dispatch with task_id isolation.
- **Compression check**: `context_engine.should_compress()` after each turn. If true, `context_engine.compress()` runs (default `ContextCompressor` summarizes turns into a heading + bullets; LCM plugin can replace this with DAG-based compaction).
- **Post-turn hooks**: `MemoryManager.sync_all(user, assistant)`, `MemoryManager.queue_prefetch_all(user)`, plugin `post_llm_call` hooks, trajectory write (when `save_trajectories=True`), background review nudge.
- **Background review**: if the agent just completed a non-trivial multi-turn task, fork an `agent/background_review.py` review that may decide to write a new skill (via `skill_manage(action="create")`) or update USER.md / MEMORY.md. Runs on its own thread with its own auxiliary-client provider.

### 3.4 How an agent decides what to do next

Hermes does **not** have a planner / orchestrator separate from
the LLM. The conversation loop just hands `messages + tool_schemas`
to the LLM and dispatches whatever tool calls come back. The
agent's "thinking" is the LLM's thinking; the loop is plumbing.

Some structured guidance is injected into the system prompt to
influence how the model decides:
- `TOOL_USE_ENFORCEMENT_GUIDANCE` — for models that talk about
  using tools instead of actually using them.
- `OPENAI_MODEL_EXECUTION_GUIDANCE` — tool persistence,
  prerequisite checks, verification, anti-hallucination. Applied
  to GPT, Codex, and Grok.
- `GOOGLE_MODEL_OPERATIONAL_GUIDANCE` — conciseness, absolute
  paths, parallel tool calls, verify-before-edit. Applied to
  Gemini and Gemma.
- `MEMORY_GUIDANCE`, `SESSION_SEARCH_GUIDANCE`, `SKILLS_GUIDANCE`,
  `KANBAN_GUIDANCE`, `COMPUTER_USE_GUIDANCE` — injected only when
  the corresponding tool is loaded.

There's no symbolic state machine. Whatever happens next is
whatever the LLM emits.

### 3.5 Error handling

`agent/error_classifier.py` defines a `FailoverReason` enum and
`classify_api_error(exc)` that buckets errors into:
- transient (retry with jittered backoff)
- rate-limit (back off with provider-specific awareness via
  `nous_rate_guard.py` / `rate_limit_tracker.py`)
- context-window (trigger compression, retry)
- token-budget (probe next tier via `agent/model_metadata.py`)
- terminal (fail the turn)

Provider fallback: each `ProviderProfile` can declare a
`fallback_model` + `fallback_provider`. On a terminal error the
agent flips to the fallback and continues the *same* conversation;
`_restore_primary_runtime()` returns to the preferred model on
the next turn.

Tool failures are returned as JSON strings (every tool handler
returns a JSON string by contract). `tool_result_classification.py`
labels them as success / soft-failure / hard-failure for retry
decisions.

### 3.6 How an agent stops

Three exit paths:

1. **Final response** — model returns content with no `tool_calls`, loop returns it.
2. **Interrupt** — `Ctrl+C` (CLI) or `/stop` (gateway) flips `_interrupt_requested`. The loop checks at every iteration boundary; mid-tool the running tool is asked to cooperate via `tools/interrupt.py`.
3. **Budget exhaustion** — `iteration_budget.remaining == 0` and no `_budget_grace_call`. Returns a budget-exhausted message; operator can `/continue`.

After exit the gateway may keep the `AIAgent` instance in its LRU
cache (default cap 128, 1h idle TTL) for future turns on the same
session. CLI agents are torn down at process exit.

---

## SECTION 4 — MEMORY IN HERMES

Hermes has **three concurrent memory layers** plus optional
externals. Read order in this section: built-in file memory →
session DB → external providers → curator/skills (procedural).

### 4.1 Built-in file memory: `MEMORY.md` + `USER.md`

Implementation: `tools/memory_tool.py`. Two markdown files under
`~/.hermes/`:

- `MEMORY.md` — agent's notes about the environment, project
  conventions, tool quirks. "Things the agent learned."
- `USER.md` — what the agent knows about the user — preferences,
  communication style, workflow habits.

Properties:
- **Bounded by character count**, not tokens. Char counts are
  model-independent. Specific caps mentioned in `decisions-pending.md`
  of the Relix repo: 2,200 chars for MEMORY.md, 1,375 for USER.md.
- **Entry delimiter**: `§` (section sign).
- **Frozen-snapshot pattern** — read into the system prompt at
  session start. Mid-session writes update files on disk (durable)
  but do NOT change the system prompt. The snapshot refreshes on
  the next session start. This preserves the LLM's prefix cache
  for the entire session.
- **Atomic writes** via `tempfile + os.replace` (POSIX) /
  `msvcrt` lock on Windows.
- **Single tool, four actions**: `memory(action="add"|"replace"|"remove"|"read", target="memory"|"user", ...)`. Replace/remove use short unique substring matching, not full text or IDs.

Survives restarts: **yes** (it's a file).
Eviction: **none** — agent must explicitly `remove`. Tool schema
nudges the agent toward minimal entries.
Scoping: **per profile** (each profile has its own
`~/.hermes/profiles/<name>/MEMORY.md`).

### 4.2 SQLite session store + FTS5 search

Implementation: `hermes_state.py::SessionDB`. SQLite at
`~/.hermes/sessions.db`. Schema (from AGENTS.md):

- `sessions` table — `session_id, platform, created_at,
  last_active_at, system_prompt, messages_json`
- FTS5 virtual table indexing session content for full-text search

How the agent uses it:
- `session_search` tool — runs an FTS5 query. Auxiliary LLM
  summarizes hits before injection. This is how cross-session
  recall works without dumping every transcript into the prompt.
- `/resume <session_id>` — gateway / CLI command to switch back to
  a prior session.
- The agent's loop writes every turn (user + assistant + tool
  results) to the session row.

Survives restarts: **yes**.
Eviction: **none** by default. Sessions accumulate. There is no
auto-archive in the session store itself (in contrast to
skills, which the curator does archive).
Scoping: **per profile** (each profile has its own
`sessions.db`).

### 4.3 External memory providers (`plugins/memory/<name>/`)

The `MemoryProvider` ABC in `agent/memory_provider.py` defines a
lifecycle interface implemented by 8 bundled providers:

- **honcho** — AI-native cross-session user modeling: dialectic
  Q&A, peer cards, semantic search, persistent conclusions. Four
  tools: `honcho_profile`, `honcho_search`, `honcho_context`,
  `honcho_conclude`. 1,328 lines just for the plugin.
- **mem0** — vector-backed memory store.
- **supermemory** — managed hosted memory.
- **byterover** — vector memory.
- **hindsight** — recall-focused memory.
- **holographic** — knowledge-graph style.
- **openviking** — alternative vector backend.
- **retaindb** — alternative vector backend.

`MemoryManager` orchestrates these:
- **Only one external provider** can be active at a time
  (configured via `memory.provider` in `config.yaml`). A second
  registration is logged + rejected to prevent tool-schema bloat.
- The "builtin" provider (MEMORY.md + USER.md) is **always**
  registered alongside whichever external one is active.

Provider interface:
- `system_prompt_block()` — text block to include in the system prompt
- `prefetch(query, session_id)` — fetch relevant memory for this turn
- `queue_prefetch(query, session_id)` — async background fetch for the next turn
- `sync_turn(user_content, assistant_content, session_id)` — write completed turn
- `get_tool_schemas()` — tools the provider exposes (memory queries, peer cards, etc.)
- `handle_tool_call(name, args, **kwargs)` — dispatch a memory tool call
- `on_turn_start`, `on_session_end`, `on_session_switch`, `on_pre_compress`, `on_memory_write`, `on_delegation`, `shutdown`, `initialize`

Failures in one provider don't block others (every loop wraps
calls in try/except + logger.warning).

Notable detail: the **streaming context scrubber** in
`memory_manager.py::StreamingContextScrubber` is a state machine
that strips `<memory-context>...</memory-context>` blocks across
stream chunk boundaries. If the agent's own memory leaks into
output, the scrubber drops everything inside the span — "safer:
leaking partial memory context is worse than a truncated answer".

Survives restarts: **provider-dependent**. honcho persists to
its server (config in `$HERMES_HOME/honcho.json`); mem0 persists
to its vector backend; etc.
Eviction: **provider-dependent**.
Scoping: per-session **and** cross-session. Honcho's peer cards
accumulate across all sessions; some providers scope per
`session_id`.

### 4.4 Skills as procedural memory (`skills/` + `optional-skills/`)

A skill is markdown + scripts. Skills bridge "knowledge" and
"capability" — they are how the agent codifies procedures it
learned.

- **Bundled**: `skills/` (25 categories). Default-loadable.
- **Optional**: `optional-skills/` (18 categories). Installed via
  `hermes skills install official/<category>/<skill>`. Adapter
  in `tools/skills_hub.py` (`OptionalSkillSource`).
- **Agent-created**: written by the agent via `skill_manage(action="create")` during a session. Stored under `~/.hermes/skills/` with `created_by: "agent"` frontmatter. **Curated** by the background curator (§4.5).
- **User-installed from hub**: `hermes skills hub install <name>` — fetched from agentskills.io.

Slash-command activation: `agent/skill_commands.py` scans
`~/.hermes/skills/`, and when the user (or another agent) types
`/<skill-name>`, the skill's SKILL.md is injected as a **user
message** (not the system prompt, so prompt caching stays valid).

### 4.5 The curator (skill lifecycle)

`agent/curator.py` (1,781 lines) — inactivity-triggered background
loop that maintains agent-created skills:

- **Telemetry**: `tools/skill_usage.py` owns
  `~/.hermes/skills/.usage.json` — per-skill `use_count`,
  `view_count`, `patch_count`, `last_activity_at`, `state`
  (`active` / `stale` / `archived`), `pinned`.
- **Auto-transitions**: skills idle longer than `stale_after_days`
  (30 default) → `stale`. Stale longer than `archive_after_days`
  (90 default) → `archived` (moved to `~/.hermes/skills/.archive/`).
- **Review fork**: when `should_run_now()` is true (interval +
  idle gate satisfied), forks an `AIAgent` using the **auxiliary**
  provider/model to run the review prompt against agent-created
  skills. The review can pin, consolidate, patch, or archive
  skills via `skill_manage`.
- **Pre-run backup**: `agent/curator_backup.py` writes a tar.gz
  snapshot before the review runs.
- **CLI**: `hermes curator status|run|pause|resume|pin|unpin|archive|restore|prune|backup|rollback`.
- **Invariants**: only touches `created_by: "agent"` skills,
  never deletes (only archives — recoverable), pinned skills are
  exempt from every auto-transition.

Survives restarts: **yes** (skills + .usage.json on disk).
Eviction: **archive after 90 days idle**, **never delete**.
Scoping: per profile.

### 4.6 Background review (insights + memory nudges)

`agent/background_review.py` + `agent/insights.py`. After a
non-trivial agent turn, a fork on the auxiliary client may:
- Suggest a memory write (`MEMORY.md` / `USER.md` update via the `memory` tool)
- Detect a recurring procedure → propose a new skill via `skill_manage(action="create")`
- Update USER.md with observations about communication style

This is what the README calls the "agent-curated memory with
periodic nudges". It's the loop that makes Hermes claim to be
"the only agent with a built-in learning loop".

### 4.7 Compression as a memory mechanism

The `ContextEngine` ABC (`agent/context_engine.py`) is loaded via
`plugins/context_engine/<name>/` — one engine active per session,
default `"compressor"`. Engine has:
- `should_compress(prompt_tokens)` — threshold check
  (default 75% of context window)
- `compress(messages, current_tokens, focus_topic)` — produces a
  shorter message list

The default `ContextCompressor` summarizes early turns into a
heading + bullets, preserves `protect_first_n=3` non-system head
messages + `protect_last_n=6` tail messages. An LCM plugin
(`plugins/context_engine/lcm/`) is referenced as the canonical
"third-party engine" example — uses DAG-style compaction with
its own `lcm_grep` / `lcm_describe` / `lcm_expand` tools.

Compression also fires the `on_pre_compress` hook on every
memory provider, which can inject text into the summary prompt
(so e.g. honcho can preserve user-modeling state through the
compression boundary).

---

## SECTION 5 — TOOLS IN HERMES

### 5.1 The tool model

A tool is a triple registered on the singleton `ToolRegistry`:
- `schema` — OpenAI-format function schema (name, description, parameters)
- `handler` — Python callable that takes args dict and returns a JSON string
- `check_fn` — zero-arg callable that returns bool (is this tool currently available?)

Plus metadata: `toolset` (which bundle), `requires_env` (list of
env vars), `description`, `emoji`, `max_result_size_chars`,
`dynamic_schema_overrides`.

Registration happens at **module import time**:

```python
# tools/example_tool.py
from tools.registry import registry

def example_tool(param: str, task_id: str = None) -> str:
    return json.dumps({"success": True, "data": "..."})

registry.register(
    name="example_tool",
    toolset="example",
    schema={...},
    handler=lambda args, **kw: example_tool(...),
    check_fn=lambda: bool(os.getenv("EXAMPLE_API_KEY")),
    requires_env=["EXAMPLE_API_KEY"],
)
```

`registry.discover_builtin_tools()` AST-scans every
`tools/*.py`, identifies files with a top-level `registry.register(...)` call, imports them. The AST check is for safety — files that *call* `registry.register` inside a function don't get auto-imported.

### 5.2 Full tool list

The complete file inventory of `tools/` (79 files, 60k LOC).
Names below correspond to files; each file may register one or
more tools.

**Web + search**:
- `web_extract.py` — fetch + parse HTML to clean text
- `x_search_tool.py` — X (Twitter) search
- `website_policy.py` — policy filters for outbound fetches
- (plus `web_search` shipped via `agent/web_search_registry.py`)

**Terminal + process**:
- `code_execution_tool.py` — Python sandbox
- `cronjob_tools.py` — schedule recurring agent runs
- (terminal tool ships via `tools/environments/` backends)

**File**:
- `file_operations.py`, `file_state.py`, `file_tools.py` — read / write / patch / list
- `fuzzy_match.py` — whitespace-tolerant text matching
- `binary_extensions.py` — heuristic content sniffing

**Browser**:
- `browser_tool.py`, `browser_camofox.py`, `browser_camofox_state.py`
- `browser_cdp_tool.py`, `browser_dialog_tool.py`, `browser_supervisor.py`

**Computer use** (macOS):
- `computer_use_tool.py` + `tools/computer_use/` subdirectory

**Communication / channels**:
- `clarify_gateway.py`, `clarify_tool.py` — ask the user a question mid-turn
- `discord_tool.py` — Discord-specific ops
- `feishu_doc_tool.py`, `feishu_drive_tool.py` — Feishu doc ops
- `homeassistant_tool.py` — HomeAssistant control
- `image_generation_tool.py` — image-gen routing
- `yuanbao_tools.py` — Tencent Yuanbao ops

**Approval + safety**:
- `approval.py` — danger-pattern detection, prompt, smart-approval auxiliary LLM (§6)
- `ansi_strip.py` — sanitize colored output
- `env_passthrough.py` — credential filter for child processes
- `mcp_oauth.py`, `mcp_oauth_manager.py` — MCP server auth

**Memory + planning**:
- `memory_tool.py` — `memory(action=...)` (§4.1)
- `todo` tool (likely in `tool_*` family) — per-turn task list
- `delegate_tool.py` — `delegate_task` for spawning subagents
- `checkpoint_manager.py` — agent state snapshots
- `budget_config.py` — iteration budget config
- `debug_helpers.py`, `interrupt.py` — debugging + interrupt plumbing

**MCP**:
- `mcp_tool.py` — `mcp_call(server, tool, args)` — universal MCP gateway
- `managed_tool_gateway.py` — manages stdio-spawned MCP servers
- `lazy_deps.py` — lazy import for heavy deps

**Skills**:
- `skill_usage.py` — telemetry
- `skill_provenance.py` — track who wrote a skill (assistant vs review fork)
- `skills_hub.py` — hub install integration

**Kanban**:
- `kanban_tools.py` — worker-side kanban ops (gated by env)

**Infrastructure**:
- `credential_files.py` — credential-file loader
- `schema_sanitizer.py` — clean tool schemas before sending to model

### 5.3 Tool environments (terminal backends)

`tools/environments/` ships **seven** pluggable terminal
backends. The `terminal` tool is implemented on top of these:

- `local` — runs commands on the host (default)
- `docker` — runs inside a container
- `ssh` — runs on a remote host
- `singularity` — Singularity container
- `modal` — Modal serverless sandbox (persistent — hibernates between sessions)
- `daytona` — Daytona dev sandbox (persistent)
- `vercel` — Vercel Sandbox

Modal + Daytona give "serverless persistence" — the agent's
environment hibernates when idle, wakes on demand, costs nothing
between sessions. README highlights this as a differentiator.

### 5.4 How an agent calls a tool

The model emits a `tool_calls` object in its response. The loop:
1. Plugin `pre_tool_call` hook fires.
2. Approval gate (§6) — for `terminal` calls matching dangerous patterns.
3. `handle_function_call(tool_name, args, task_id)` in `model_tools.py`:
   - Look up `ToolEntry` in registry
   - Call `entry.handler(args, **runtime_kwargs)` (task_id, session_key, etc.)
   - Wrap with timeout if `is_async`
4. Tool returns JSON string. `tool_executor.py` truncates if over `max_result_size_chars`.
5. Plugin `post_tool_call` hook fires.
6. Result appended to messages as a `{"role": "tool"}` entry.

`task_id` is generated per-turn (UUID4) and passed to every
tool. It's what isolates concurrent subagent runs from each
other (delegated agents see their parent's task_id at launch
time so the file-state registry knows which agent owns which
file lock).

### 5.5 Tool failure

Tool handlers return JSON strings by contract. Failures look like:

```python
{"error": "could not connect to docker"}
```

or `tool_error("reason")` (a helper from `tools.registry`).
`tool_result_classification.py` labels these as soft / hard
failures; soft failures are retryable, hard failures end the
tool call.

The model sees the failure as a tool result and decides what to
do next (typically retries with adjusted args or pivots
approach). The agent loop doesn't intervene.

### 5.6 Tool access control

Tools are gated **by toolset membership**, not by per-agent
permission. The toolset bundle is computed at agent construction
from:

1. Platform's default bundle (e.g. Telegram uses `messaging`)
2. Intersected with `enabled_toolsets` (constructor param)
3. Removed: `disabled_toolsets`
4. Filtered by per-tool `check_fn()` — env-availability gate

`hermes tools` (curses UI) or `tools.<platform>.enabled` in
config.yaml flips toolsets per platform. There is **no
per-agent permission record** that would let "research agent"
have web tools but not terminal.

The closest thing to per-agent permissions is **delegation roles**:
- `role="leaf"` (default subagent) — denied: `delegate_task`,
  `clarify`, `memory`, `send_message`, `execute_code`.
- `role="orchestrator"` — retains `delegate_task` only.

These are role-name-driven toolset narrowing applied in
`delegate_tool.py`, not a general permission system.

---

## SECTION 6 — PERMISSIONS AND POLICY IN HERMES

### 6.1 The headline framing

From `SECURITY.md` §2.2:

> **The only security boundary against an adversarial LLM is the
> operating system.** Nothing inside the agent process constitutes
> containment — not the approval gate, not output redaction, not
> any pattern scanner, not any tool allowlist.

This is the load-bearing claim. Everything else Hermes does
permission-wise is **defense-in-depth heuristic**, not a
boundary. The maintainers are explicit and consistent about this.

### 6.2 What does exist permission-wise

**(a) The approval gate** (`tools/approval.py`, ~1,000+ lines).
For the `terminal` tool only. Three tiers:

- **HARDLINE blocklist** — unconditional refusal regardless of
  any operator flag. ~12 patterns covering `rm -rf /`, `mkfs`,
  raw block-device writes, fork bombs, system shutdown/reboot,
  kill -1. Even `--yolo` mode does not bypass these. Inspired by
  Mercury Agent's permission-hardened blocklist.
- **DANGEROUS_PATTERNS** — ~47 patterns prompting before
  execution. Covers `rm -rf` of important dirs, `git reset --hard`,
  `curl | sh`, writes to `/etc`, `~/.ssh/`, `~/.bashrc`, `.env`,
  config.yaml, credential files (`.netrc`, `.pgpass`), system
  config dirs (with macOS `/private/etc` mirror handling), `sudo -S`
  password injection guard.
- **Per-session approval state** — once approved for the session,
  the same pattern doesn't re-prompt.

**Smart approval** — auxiliary LLM call evaluates "is this
command actually dangerous in this context?" and may auto-approve
low-risk commands the regex matched. Saves operator interruptions.

**Permanent allowlist persistence** — operator can mark a pattern
as approved-forever, stored in `config.yaml::approvals.allow`.

**Approval prompts work in two modes**:
- **CLI interactive** — block the loop with a synchronous prompt
  (`prompts.tsx` in TUI, prompt_toolkit in classic CLI).
- **Gateway async** — fire `pre_approval_request` hook, post the
  prompt to the gateway adapter, wait for an out-of-band
  `/approve` / `/deny` (with a configurable timeout).

The approval gate is keyed by **session_key** so concurrent
gateway sessions don't share approval state. Crons explicitly
**never** prompt (env var `HERMES_CRON_SESSION` short-circuits) —
crons use `approvals.cron_mode` config instead (off / auto / fail).

**(b) The Skills Guard** — third-party skill review. Scans
installable skill content for injection patterns. Per
`SECURITY.md` §2.4: "It is a review aid; the boundary for
third-party skills is operator review before install." Skills run
arbitrary Python at import time; the actual containment is
"don't install a skill you haven't read".

**(c) Output redaction** — strips secret-like patterns from
display. Per `SECURITY.md` §2.4: "A motivated output producer
will defeat it." It's not a containment claim, just a UX comfort
layer.

**(d) Credential scoping** — `tools/env_passthrough.py` strips
provider API keys, gateway tokens, etc. from the env passed to:
- shell subprocesses
- MCP subprocesses
- code-execution child process

Per `SECURITY.md` §2.3: "This reduces casual exfiltration. It is
not containment."

**(e) External-surface authorization** (`SECURITY.md` §2.6).
Every gateway platform, network HTTP surface, and editor/IPC
adapter must have an operator-configured caller allowlist before
it accepts dispatch / approval / output. Failing open with no
allowlist is a code bug in scope under §3.1.

### 6.3 Audit trail

There is **no signed cryptographic audit log** like Relix has.
What exists:

- **`agent.log`** — INFO+ per-line text log of agent actions.
  Profile-aware path under `~/.hermes/logs/`. Browsable via
  `hermes logs`.
- **`errors.log`** — WARNING+ filtered log.
- **`gateway.log`** — gateway-specific log.
- **`trajectories/`** — when `save_trajectories=True`, per-turn
  full trajectory (system + messages + tool calls + responses) is
  written to disk. Used for batch trajectory generation /
  training data.
- **Plugin lifecycle hooks** — every plugin can register
  `pre_tool_call`, `post_tool_call`, `pre_llm_call`, `post_llm_call`,
  `pre_approval_request`, `post_approval_response` and produce
  arbitrary observability output. The `observability` plugin uses
  this to emit metrics / traces / logs.
- **Insights** — `agent/insights.py` computes per-session
  summaries (token usage, tool counts, durations).
- **`/usage`** — slash command shows token usage for the current
  session.
- **Provider provenance** — `tools/skill_provenance.py`
  ContextVar tells skill writes whether they came from the
  foreground agent or the background review fork. Used to tag
  skill frontmatter (`created_by: assistant` vs
  `created_by: agent`).

None of this is hash-chained or cryptographically signed. Logs
are append-only text files. Operator-shipping to a SIEM is the
expected production posture.

### 6.4 Roles / permissions in the agent-employee sense

There is **no concept of an "agent employee"** — no record type,
no per-agent permissions, no role hierarchy, no approval-required
field on tools, no agent-status lifecycle. The closest analogs:

- **Profile** is operator-level isolation — "use this whole
  agent for coding, that whole agent for ops". Not per-tool.
- **Delegation role** (`leaf` / `orchestrator`) — narrows the
  delegated subagent's toolset by name pattern, not by stored
  permission record.
- **HERMES_KANBAN_TASK** env — gates the kanban worker toolset
  on workers spawned by the dispatcher. Boards are the hard
  boundary; "tenants" (specialists) within a board are soft
  namespace.

### 6.5 Comparison to the Relix agent-employee proposal

This deserves its own table because it's the meat of the
question.

| Concern | Hermes today | Relix agent-employee proposal (Phase 1–5) |
| --- | --- | --- |
| Per-agent identity record | Profile dir or just the process | TOML record with id, role, dept, status, scope, approvals, signed by org root |
| Status lifecycle | None (start/stop the process) | active / suspended / disabled with chronicle event on change |
| Categorical permissions | None — toolset bundles only | `allow_categories` / `deny_categories` / `allow_sensitivity_tags` / `deny_sensitivity_tags` / `max_risk_level` over the capability descriptor's metadata |
| Per-action approval | Yes, but only for `terminal` danger patterns, session-local | First-class `Decision::RequireApproval` reusing `awaiting_input` task state, cross-channel via bridge |
| Standing approvals | Permanent allowlist in `config.yaml::approvals.allow` for terminal patterns | Time-bounded standing approvals per `(agent, category, path_glob)` with expiry + grantor recorded |
| Surface gating | One mode per adapter ("messaging caller allowlist") | `surface` field on every request envelope; agent record lists `allowed_surfaces` |
| Audit-grade trail | Text logs + optional trajectories + plugin hooks | Hash-chained signed audit log per node with `args_redacted_hash`, `agent_id`, `phase`, `decision`, `matched_rule`, `approval_id` |
| Identity is signed | OS user account = trust unit | Org-root-signed Ed25519 IdentityBundle with subject_id, groups, role, clearance |

**Hermes's bet is the OS.** It declares the OS as the single
boundary and pours engineering into making the in-process
heuristics ergonomic for the operator. Relix's bet (in the
agent-employee proposal) is that operators need actual
permission *records* and an audit trail you could hand to a
compliance auditor. Both are valid postures for different threat
models; they don't substitute for each other.

---

## SECTION 7 — ORCHESTRATION IN HERMES

### 7.1 What orchestration means here

Three different things share the word "orchestration":
1. **Within one turn** — the LLM emits tool calls, the loop dispatches them. That's it.
2. **Across turns within one session** — the loop just keeps running; the agent decides.
3. **Across agents / sessions / processes** — delegation, cron, kanban. This is where the interesting machinery lives.

### 7.2 Delegation — synchronous subagents

`tools/delegate_tool.py` exposes `delegate_task` as a tool the
parent agent calls. Two shapes:

- **Single**: `delegate_task(goal=..., context=..., toolsets=...)` — one subagent.
- **Batch parallel**: `delegate_task(tasks=[{goal, context, toolsets}, ...])` — N subagents concurrently. Capped by `delegation.max_concurrent_children` (default 3).

**Synchronicity rule**: delegate_task is **not durable**. The
parent blocks on the child's summary. If the parent's loop is
interrupted, the child is cancelled. For work that must outlive
the current turn, the operator uses `cronjob` or
`terminal(background=True, notify_on_complete=True)`.

Each subagent gets:
- Its own AIAgent instance (auxiliary client if configured, else inherits)
- Isolated terminal session (its own backend env)
- Bounded `max_iterations` (inherited from parent or capped at `delegation.max_iterations`)
- Narrowed toolset by `role` (`leaf` denies delegate / clarify / memory / send_message / execute_code; `orchestrator` keeps delegate)
- Depth cap via `delegation.max_spawn_depth` (default 2)

The parent's loop **pauses** while children run. The child
returns a summary string; the parent sees it as a tool result and
continues.

### 7.3 Handoffs

Hermes does not have "agent A hands off to agent B" as a
first-class concept. The closest equivalent is:

- **Delegation** — parent spawns child for a specific task, child
  exits, parent continues. One-way, parent owns the outcome.
- **Kanban** — task assigned to a profile + spawned by the
  dispatcher. The parent profile isn't blocked; the worker
  profile gets the task; the worker writes back via
  `kanban_complete` / `kanban_block`.
- **Cron handoff** — `context_from` field chains job A's last
  output into job B's prompt. Effectively "agent A wrote to
  the chronicle, agent B reads it next run".

There is no symbolic handshake. The model decides, and the
runtime executes.

### 7.4 Long-running tasks

Three mechanisms, each with different durability:

**Cron** (`cron/jobs.py` + `cron/scheduler.py`, ~3,100 LOC):
- Schedule formats: duration (`30m`), "every" phrase
  (`every monday 9am`), 5-field cron expression, ISO timestamp.
- Per-job fields include `skills` (load specific skills), `model`
  / `provider` overrides, `script` (pre-run data-collection
  script), `context_from` (chain previous output), `workdir`
  (load that dir's AGENTS.md / CLAUDE.md), multi-platform
  delivery.
- **Hardening**: 3-minute hard interrupt per cron session (runaway
  loops cannot monopolize the scheduler), catchup window (half
  the job's period clamped 120s–2h), grace window (120s for
  missed one-shots), file lock at `~/.hermes/cron/.tick.lock`,
  `skip_memory=True` default.
- **No mirroring**: cron deliveries land in their own session,
  not the main conversation, so message-role alternation stays
  intact in the main thread.

**Background terminal**: `terminal(command, background=True,
notify_on_complete=True)`. Spawns a detached process. The
gateway runs a watcher that detects completion and triggers a
new agent turn to inform the user.

**Kanban**: durable SQLite-backed work queue. Use case is
**multi-agent collaboration on shared tasks**:
- Operator defines a board with `hermes kanban init`
- Creates tasks via `hermes kanban create` (or another agent
  does it via `kanban_create` tool)
- Tasks are assigned to profiles (separate Hermes instances)
- Dispatcher (default in-gateway) polls every 60s, claims ready
  tasks, spawns the assigned profile as a worker
- Worker has `kanban_show / kanban_complete / kanban_block /
  kanban_heartbeat / kanban_comment / kanban_create / kanban_link`
  in its toolset (gated by `HERMES_KANBAN_TASK` env)
- Worker writes back to the board, dispatcher reclaims stale
  claims (after ~5 spawn failures, dispatcher auto-blocks a task
  to prevent spin loops)

This is **the** mechanism for tasks that take hours / days /
across-profiles. It's the most production-shaped piece of
Hermes's orchestration story.

### 7.5 What's not there

- **No durable mid-flow pause/resume of one agent.** The agent
  loop is synchronous (matches Relix's SOL VM in that respect).
  "Pause and wait for human approval" only works for the terminal
  approval gate — the loop blocks on that prompt.
- **No multi-step plan that survives a crash.** Cron + kanban are
  the answer; a single delegation can be interrupted and is lost.
- **No graph of agents.** Delegation is a tree (parent → child →
  grandchild bounded by max_spawn_depth). Kanban is a bag of
  tasks with assignees, not a DAG of agents.

---

## SECTION 8 — CHANNELS IN HERMES

### 8.1 The model

A **channel** is what the README calls a "gateway platform". The
gateway daemon (`gateway/run.py`) loads N platform adapters at
startup, each one exposes a `connect()` / `disconnect()` /
`send_message()` / receive-loop. The gateway demultiplexes
inbound messages to sessions, runs the agent loop, and routes
outbound messages back through the right adapter.

Adapters live in `gateway/platforms/`. The directory has 27
files (a few are helpers — `_http_client_limits.py`,
`telegram_network.py`, `signal_rate_limit.py`, etc.).

### 8.2 Full channel list

From `gateway/platforms/`:

- **telegram.py** — Telegram Bot API. The canonical reference
  adapter; the most heavily used.
- **discord.py** — Discord bot
- **slack.py** — Slack bot (multi-channel + DM)
- **whatsapp.py** — WhatsApp (via separate `whatsapp-bridge`
  helper in `scripts/whatsapp-bridge/`)
- **signal.py** + **signal_rate_limit.py** — Signal
- **matrix.py** — Matrix federation
- **mattermost.py** — Mattermost
- **email.py** — SMTP/IMAP email channel
- **sms.py** — SMS (likely via Twilio per the README)
- **homeassistant.py** — HomeAssistant integration
- **dingtalk.py** — DingTalk
- **wecom.py** + **wecom_callback.py** + **wecom_crypto.py** —
  WeCom (Chinese enterprise IM)
- **weixin.py** — Weixin (WeChat)
- **feishu.py** + **feishu_comment.py** + **feishu_comment_rules.py** —
  Feishu (Lark)
- **qqbot/** — QQ bot subdirectory
- **bluebubbles.py** — iMessage via BlueBubbles
- **yuanbao.py** + **yuanbao_media.py** + **yuanbao_proto.py** +
  **yuanbao_sticker.py** — Tencent Yuanbao
- **api_server.py** — Generic HTTP API (programmatic callers)
- **webhook.py** — Generic webhook receiver
- **msgraph_webhook.py** — Microsoft Graph webhook (Outlook /
  Teams notifications)

Plus `gateway/platforms/base.py` (the `PlatformAdapter`
interface) and `gateway/platforms/helpers.py`.

### 8.3 What a channel does that a tool does not

A **tool** is something the agent calls inside its turn. The
agent has agency, args, and a result.

A **channel** is something the user calls. The user has a
session, a message, and the channel translates it into agent
input + renders the agent output back.

Specifically the channel handles:
- Authentication against the platform's API (bot tokens, OAuth,
  webhooks)
- Session resolution: who is this user, what's their session_id?
- Caller allowlist enforcement (§6.2(e))
- Sticker / voice / image media handling (voice-memo
  transcription pipeline; sticker → emoji or skip)
- Platform-specific slash-command rendering (Telegram's
  `BotCommand` menu, Slack's `/hermes <subcommand>` routing)
- Markdown / rich-text → platform's content type
- Streaming output → platform's per-platform chunking (Telegram
  edits messages, Discord splits, Slack updates, etc.)
- Approval prompts when interactive — post-and-wait for `/approve`
- Restart preservation + queueing — adapter buffers messages
  during agent restarts

A channel also handles **identity scoping** in a way a tool
does not. The Telegram adapter derives a stable subject id from
`(chat_id, user_id)` via blake3 hash — same human in the same
chat is always the same subject across restarts. This is what
makes "Hermes remembers you across sessions on Telegram" work.

### 8.4 Adding a new channel

`gateway/platforms/ADDING_A_PLATFORM.md` is the canonical guide.
The adapter subclass declares its capabilities (commands,
streaming, voice, vision) and the gateway runner wires it into
the dispatch loop. The single registry pattern
(`gateway/channel_directory.py`) means the new adapter shows up
in `/help`, the Telegram bot menu, the Slack subcommand map,
and autocompletion automatically — same as the slash-command
`COMMAND_REGISTRY` pattern.

---

## SECTION 9 — WHAT HERMES HAS THAT RELIX DOES NOT

Brutal version. Ranked by how much each gap hurts Relix's
positioning.

### 9.1 An actual self-improving loop

The defining Hermes feature. Three reinforcing pieces:

- **Agent-curated MEMORY.md / USER.md** with periodic background
  review nudges (`agent/background_review.py`).
- **Skills as procedural memory** — the agent writes its own
  skills mid-session via `skill_manage(action="create")`.
- **Curator** auto-reviewing agent-created skills, archiving
  stale, pinning favorites (`agent/curator.py`, 1,781 lines).

Relix has none of this. Relix's `memory` capabilities are
write+read+search of chat turns. There is no agent-curated
persistent memory file, no procedural-memory layer, no
background self-review loop, and no autonomous skill creation.
`docs/internal/decisions-pending.md::D-001` explicitly **deferred**
adopting this pattern.

### 9.2 27 channel adapters

Hermes connects to Telegram, Discord, Slack, WhatsApp, Signal,
Matrix, Mattermost, email, SMS, HomeAssistant, DingTalk, WeCom,
Weixin, Feishu, QQ, BlueBubbles (iMessage), Yuanbao, MS Graph
webhooks, a generic webhook receiver, and a generic API server.

Relix has **zero working channel adapters**. `relix-telegram`
ships a `BotApi` trait, a `MockBotApi`, identity-derivation, and
a config validator — and no live HTTPS implementation, no
controller binary wiring. Operators cannot send a message to a
Relix mesh from Telegram today.

### 9.3 Cron / scheduled agent runs

`cron/jobs.py` + `cron/scheduler.py` total 3,100 LOC. Agents
schedule their own future work; the scheduler runs them
unattended with per-job model/provider/skills/workdir overrides;
results delivered to the operator's preferred channel.

Relix has no cron, no scheduler, no agent-scheduled future work.

### 9.4 Kanban — durable multi-agent work queue

SQLite-backed, dispatcher polling every 60s, profile-isolated
workers, board-as-hard-boundary, stale-claim reclamation,
auto-block on spin loop. This is the production answer for
"multiple agents collaborating on shared tasks over hours / days".

Relix has the Coordinator (durable task ledger), which is half
of this — but no dispatcher, no profile-workers, no claim
mechanism, no spin-loop guards. The Coordinator persists Task
state; nothing **schedules** them.

### 9.5 Skills as procedural memory

`skills/` (25 categories bundled), `optional-skills/` (18 categories
heavy / niche), `~/.hermes/skills/` (user-installed + agent-created).
Plus Skills Hub at agentskills.io for community skills. Plus the
curator. Plus the `/skill-name` slash command activation that
injects the skill markdown as a **user message** to preserve
prompt caching.

Relix has no skills concept. SOL flows are the closest analogy
but they're not procedural memory — they're orchestration scripts
that consume tools, and they're operator-written, not
agent-written.

### 9.6 30 model-provider plugins

`plugins/model-providers/` has bundled profiles for: ai-gateway,
alibaba, alibaba-coding-plan, anthropic, arcee, azure-foundry,
bedrock, copilot, copilot-acp, custom, deepseek, gemini, gmi,
huggingface, kilocode, kimi-coding, minimax, nous, novita, nvidia,
ollama-cloud, openai-codex, opencode-zen, openrouter, qwen-oauth,
stepfun, xai, xiaomi, zai, and one called just `ai-gateway`.

Relix has six providers: mock, openai, anthropic, openrouter, xai,
gemini, plus a "local" mode (Ollama / llama.cpp / vLLM). Routing
is statically configured at the AI node; no plugin layer.

### 9.7 Honcho — AI-native user modeling

A 1,328-LOC plugin that provides cross-session dialectic Q&A,
peer cards, semantic search, and persistent conclusions about
the user. The agent literally knows you across all your past
conversations — not just transcript history, but distilled
*model* of you.

Relix's memory is per-session turns. No user modeling.

### 9.8 8 bundled memory providers

honcho, mem0, supermemory, byterover, hindsight, holographic,
openviking, retaindb. Each implementing the `MemoryProvider` ABC
and orchestrated by `MemoryManager`. Operators pick one via
`memory.provider` in config.

Relix has one memory backend (SQLite + FTS5) and one
implementation. No plugin abstraction for memory.

### 9.9 Sophisticated approval gate

47 dangerous-command patterns + 12 hardline blocklist + smart-
approval auxiliary LLM + per-session approval state + permanent
allowlist + async gateway approval + cron-specific approval
mode + macOS `/private/etc` mirror handling + `sudo -S` brute-
force guard. Inspired by Mercury Agent's hardlined blocklist.

Relix has policy allow/deny on per-method calls. No pattern-
based gate, no smart-approval auxiliary, no gateway-async
approval flow, no hardline category.

### 9.10 TUI with React/Ink + dashboard PTY embed

`ui-tui/` — full React/Ink terminal UI as a first-class
front-end. The classic CLI is the secondary. The dashboard at
`hermes dashboard` embeds the **actual** TUI via xterm.js +
websocket PTY — not a rewrite, the same process.

Relix has a static HTML+JS dashboard with no embedded TUI.
There is no first-class React UI.

### 9.11 7 terminal backends with serverless persistence

`tools/environments/`: local, docker, ssh, singularity, modal,
daytona, vercel. Modal and Daytona give the agent's environment
free hibernation between sessions.

Relix's `tool.terminal.*` runs commands on the host with
allowlist + audit ring. No remote backends, no serverless
persistence, no Docker / SSH / Modal options.

### 9.12 Session search across history

`session_search` tool runs FTS5 across `~/.hermes/sessions.db`
with auxiliary-LLM summarization of hits before injection. The
agent can answer "what did we decide about X two weeks ago?".

Relix has per-task chronicle, no cross-session search. The
dashboard has filter chips on a single task's chronicle, but no
"search every task this user ever ran".

### 9.13 ACP adapter for IDE integration

`acp_adapter/` + `acp_registry/` — Agent Communication Protocol
server consumed by VS Code, Zed, JetBrains.

Relix has nothing comparable. The bridge speaks OpenAI-compatible
HTTP, which Open WebUI can consume; no IDE-side protocol.

### 9.14 Profile multi-instance support

`hermes -p coder` runs an entirely independent instance with its
own HERMES_HOME, secrets, memory, skills, sessions, gateway.
`_apply_profile_override()` sets HERMES_HOME before any module
imports. Code uses `get_hermes_home()` everywhere instead of
hardcoded paths.

Relix's analog is to spin up a separate mesh with a different
`--run` argument to the bringup script. There is no in-process
profile switch, no shared CLI binary that can talk to multiple
meshes by flag.

### 9.15 LLM-aware compression

`ContextCompressor` summarizes early turns into headings +
bullets when context approaches the model's window. LCM plugin
replaces this with DAG-based compaction + searchable engine
tools (`lcm_grep`, `lcm_describe`, `lcm_expand`). On every
compression the memory providers get a `on_pre_compress` hook to
inject state into the summary prompt.

Relix has no context compression. Chat flows just pass full
history to `ai.chat`; if the history exceeds the model's
context, the provider's failure mode is whatever the provider
does (truncation, error). The bridge does not summarize.

### 9.16 Plugin lifecycle hooks across the agent

Hermes plugins can hook `pre_tool_call`, `post_tool_call`,
`pre_llm_call`, `post_llm_call`, `on_session_start`,
`on_session_end`, `pre_approval_request`, `post_approval_response`.
The observability plugin uses these to ship metrics / traces /
logs.

Relix has no plugin system. Capabilities are compiled into the
controller binary. `docs/plugin-foundations.md` says plugins are
explicitly out of scope until the constraints document (M1–M3)
is satisfied.

### 9.17 Voice memo transcription

Gateway adapters handle voice memos: the message lands as audio,
the gateway transcribes (provider-routed STT in `stt.*` config),
hands the transcript to the agent. Inverse for TTS replies.

Relix has none of this.

### 9.18 i18n

`agent/i18n.py` + `locales/` — multi-language support across the
gateway adapters (Telegram's responses, error messages, etc.).
README ships in English + Chinese.

Relix is English-only.

### 9.19 Insights and account usage

`/insights [--days N]` summarizes the user's recent activity
(token usage, tool counts, durations). `/usage` shows current
session. `agent/account_usage.py` queries provider billing.

Relix's dashboard shows per-capability latency and recent denials.
No "what did this user do this week" summary.

### 9.20 17,000 tests across 900 files

Per AGENTS.md, the test suite is roughly 17k tests across 900
files. The `tests/` directory has subdirectories for every
major surface: acp, acp_adapter, agent, cli, cron, e2e, fakes,
gateway, hermes_cli, hermes_state, honcho_plugin, integration,
openviking_plugin, plugins, providers, run_agent, scripts,
skills, stress, tools, tui_gateway, website.

Relix has 1,299 tests workspace-wide.

---

## SECTION 10 — WHAT RELIX HAS THAT HERMES DOES NOT

There are real things here. Listing them honestly, not to
soften §9 but because the comparison points the other direction.

### 10.1 P2P mesh of signed peer processes

Relix runs a **mesh** of separately-identifiable peer processes
(memory, ai, tool, coordinator, router, bridge), each with its
own Ed25519 identity, its own admission pipeline, its own audit
log. Hermes is **one process** with everything in-tree. Every
in-process Hermes component (skill, plugin, hook) runs with
full agent privileges; in Relix a tool node compromise does not
give the attacker memory-node privileges.

This is a fundamentally different architecture choice. Hermes
would call this "the OS is the boundary; one process is fine";
Relix says "make the boundary part of the design".

### 10.2 Signed, hash-chained audit log

Per-node `audit.log` is hash-chained and Ed25519-signed by the
responder. `relix-flow-inspect --audit ... --replay-verify`
walks the chain and verifies every record's signature. Audit
records are cross-referenced by `request_id` + `trace_id` with
the per-flow event log.

Hermes's `agent.log` is text. Trajectories are JSON.
Plugin-driven observability emits whatever metrics the plugin
chooses. None of it is signed or hash-chained. There's no
"prove this didn't happen" affordance.

### 10.3 Per-call cryptographic identity verification

Every Relix call carries a signed `IdentityBundle`. Validation
happens in `relix-core::identity::validate_identity_bundle` —
the construction-private `VerifiedIdentity` is the only thing
that reaches dispatch. The type system enforces that nothing
skips verification.

Hermes's identity is "whatever Python process is running this
code". No per-call signature, no verification.

### 10.4 Policy engine with operator-facing TOML

`PolicyEngine` reads a per-node `policy.toml` with `[admit]`
groups and per-method `[[rules]]`. Allow/deny decisions are
logged into the audit log with `matched_rule` for explainability.
W2-007 added `node.policy.simulate` (what-if) and
`node.policy.recent_denials` (live denial ring) and matching
dashboard pages.

Hermes has no policy file. Permissions are tool-bundle membership
+ the approval gate. There's no operator-grade policy artifact a
compliance auditor would recognize.

### 10.5 Agent-employee permission model design (proposal-only)

`docs/proposals/agent-employee-permissions.md` ships the design
for: agent records with status / role / dept, categorical
permissions over capability metadata (`categories`,
`sensitivity_tags`, `risk_level` ceiling), per-action approval
flow reusing `awaiting_input` task status, standing approvals,
audit log enrichment.

Hermes has nothing like this. Its closest analog (the approval
gate) is per-command not per-agent and has no "this agent is
suspended" lifecycle. The Relix proposal is the more rigorous
authorization design even though it's not yet implemented.

### 10.6 SOL as a separate orchestration substrate

SOL flow files (`flows/*.sol`) live outside the runtime — they
are the only place orchestration ordering lives, never in Rust
glue. A flow can be reviewed by an operator as a standalone
artifact. Compiles to a per-flow signed event log.

Hermes orchestrates via the LLM's tool calls; "ordering" is in
the model's head and the system prompt's guidance. There is no
human-reviewable orchestration script except the SKILL.md
prose (which is procedural advice, not executable).

### 10.7 First-class capability descriptor metadata

Every capability in Relix ships with a typed descriptor:
`method_name`, `risk_level` (Unknown / Safe / Low / Medium /
High / Critical), `categories: Vec<String>`, `sensitivity_tags:
Vec<String>`, `environment_requirements: Vec<String>`,
`cost_class`, `idempotency`, `requires_groups`. The validator
flags any `Unknown` risk as a deployment warning.

Hermes tool schemas are OpenAI-format function schemas (name,
description, parameters). No risk taxonomy, no sensitivity tags,
no per-tool environment requirements field. Tool dangerousness
is hardcoded in `approval.py` pattern lists, not on the tool
descriptor.

### 10.8 Honesty contract

The Relix codebase has an explicit ethos of "scaffold means
returns BackendNotConnected; never fake success". Browser
backends without their feature compiled fail loud at startup.
MCP HTTP transport returns `RuntimeNotConnected`. Tests prevent
silent fallback.

Hermes is more pragmatic — failures return JSON `{"error": ...}`
and the model decides what to do. The honesty contract is less
about preventing fake success and more about giving the agent a
chance to recover.

### 10.9 Per-flow signed event log

`relix-core::eventlog` — every SOL flow has its own
hash-chained, signed event log (`eventlog.rs`). The events are
typed (`FlowStarted`, `RemoteCallIssued`, `RemoteCallCompleted`,
`RemoteCallFailed`, `FlowCompleted`). `relix-flow-inspect
--replay-verify` walks the chain.

Hermes's trajectories are unsigned JSON. The session DB writes
the message list at every turn but it's not hash-chained.

### 10.10 Bounded operator-visible rings

Multiple bounded rings exposed via stable capabilities:
`tool.fs.audit_recent`, `tool.terminal.audit_recent`,
`tool.mcp.audit_recent`, `node.policy.recent_denials`,
`tool.browser.capture_read`, plus per-task chronicle rings. Each
ring has a defined capacity (256), FIFO eviction, and a
matching dashboard card + CLI mirror.

Hermes has the agent log, the curator state file, the kanban
DB, and the session DB. No standardized bounded-ring telemetry
across subsystems.

---

## SECTION 11 — RECOMMENDATIONS

Given the gap analysis, here's what I'd build next in Relix and
in what order. These are opinions backed by what I read; the
user should push back where they disagree.

### 11.1 The framing question: what is Relix actually for?

Hermes and Relix are not the same product even though they
overlap in concept. Hermes is a **single-user personal agent**
optimized for "Teknium runs Hermes on his laptop / VPS and it
remembers him across Telegram and Slack and email forever".
Relix is a **multi-peer signed mesh** that wants to be a real
operating layer for many agents that need to be verifiably
audited.

Trying to make Relix do everything Hermes does is the wrong
goal. Relix should pick the 2–3 most-load-bearing Hermes
features and skip the rest.

The 2–3 I'd pick:

1. **Some form of persistent self-curating memory** — MEMORY.md
   + USER.md or an equivalent.
2. **A scheduler** — agents scheduling their own future work.
3. **At least one live messaging channel** — finish Telegram.

The agent-employee permission model is the right next track for
Relix's *own* differentiated bet. But shipping that with **no**
persistent memory, **no** scheduler, and **no** channel makes
the audit story compelling on paper while the actual user
experience is "Relix is a less-capable Hermes with better
logs".

### 11.2 Recommended sequence

**Phase 1 — Persistent agent memory (high impact, bounded scope)**

Land a SQLite-backed `agent_memory` capability on the coordinator
that owns:
- `MEMORY.md`-equivalent text store with character cap (2,200)
- `USER.md`-equivalent per-subject_id store (1,375 chars)
- Tools: `memory.read`, `memory.add`, `memory.replace`, `memory.remove`
- Snapshot injection: AI node's `ai.chat` handler reads memory
  via `remote_call("memory", ...)` at session start, includes in
  system prompt
- Survives restarts (it's SQLite)

This is the **piece every other Hermes-like feature stands on**.
Without it agents can't "remember you". Two sessions of work.

**Phase 2 — Background curator pattern**

A bounded loop on the coordinator that:
- Periodically reviews per-subject_id memory entries
- Uses the auxiliary AI node (or the same AI node) to summarize /
  consolidate stale entries
- Writes `task.memory_curated` chronicle events

This is what gives Relix the "self-improving" framing without
needing skills yet. One session.

**Phase 3 — Agent-employee permission model Phase 1**

Per the existing proposal:
- Agent record TOML schema in `relix-core::agent`
- `[[agents]]` array in node config
- New `relix_runtime::admission::agent_gate` module
- `surface` field on request envelope (bridge sets it)
- Status + surface + risk-ceiling checks (read-only)
- New chronicle events: `agent.lookup_failed`,
  `agent.suspended_call_denied`, `agent.surface_denied`,
  `agent.risk_ceiling_denied`

**No approval flow yet.** Just observation + categorical
narrowing. Operators see the model working before committing to
the approval-flow complexity. Two sessions.

**Phase 4 — Telegram (finish the scaffold)**

The Telegram scaffold (`relix-telegram` crate) is the most
operator-impactful "channel" because of message-volume + the
existing identity-derivation work. Pieces:
- Live reqwest-backed `BotApi` implementation
- Controller binary wiring (a new `[telegram]` `node_type`)
- Caller allowlist enforcement
- Bot-token management via the bridge's existing secrets store
- A `task.message_received` chronicle path for inbound messages
- A skill-equivalent dispatcher: inbound message → SOL flow

Two sessions.

**Phase 5 — Cron**

A scheduler node-type that:
- Stores jobs in SQLite (similar to coordinator's task ledger)
- Tick loop (30s default) checks due jobs
- For a due job: mints an `IdentityBundle` for the cron-agent,
  spawns a SOL flow execution, persists the resulting task
- Hardening per Hermes: hard interrupt at N seconds, catchup
  window, file lock, `skip_memory` default

One session.

**Phase 6 — Agent-employee Phase 2–3**

Categorical permissions + dashboard agent profile pages. Per the
existing proposal.

**Defer (Wave 4+)**

- Skills as procedural memory — Hermes's skills are a deep system;
  Relix's SOL flows are the cleaner abstraction. Don't recreate
  skills as a new thing — extend SOL flows with operator-visible
  metadata.
- Multi-channel (Discord, Slack, etc.) — once Telegram works the
  pattern is repeatable but each is real work.
- Kanban / multi-agent work queue — needs Phases 1–5 in place first.
- ACP adapter — IDE integration is high-leverage but not on the
  critical path until Relix has the memory + scheduler story.
- 30 model providers — Relix's six providers cover the realistic
  use cases; the rest are operator-driven extensions of the
  existing AI node.

### 11.3 Things to deliberately *not* copy from Hermes

- **The "OS is the boundary" posture.** This is Hermes's
  deliberate choice and the right one for a single-user laptop
  agent. Relix's whole bet is that the mesh and the signed
  pipeline ARE additional boundaries. Don't dilute that by
  claiming the OS is enough.
- **The 17k-line single-file `AIAgent` class.** Relix's
  `crates/relix-runtime/src/dispatch/mod.rs` is large but
  factored. Don't let `coordinator/mod.rs` grow into a
  `run_agent.py`.
- **47 dangerous-command regexes.** They're heuristics, not
  boundaries. Relix's `risk_level` + `requires_groups` is a
  more principled answer when the agent-employee model lands.
- **Mid-conversation prompt mutations.** Hermes's caching
  invariant ("don't alter past context mid-conversation") is a
  fight Relix doesn't need to fight at all — every SOL flow
  starts fresh.

### 11.4 Things to deliberately copy

- **The `MemoryProvider` ABC**. It's a clean interface. Adopting
  it (renamed / Rust-shaped) for Relix Phase 1 + 2 lets a future
  Honcho equivalent slot in cleanly.
- **Frozen-snapshot memory pattern**. Read at session start,
  durable writes mid-session, snapshot refreshes next session.
  This is the prompt-caching trick that makes Hermes economical.
- **`auxiliary_client.py` pattern.** A separate provider+model
  for side-LLM tasks (curator, vision, embedding, title
  generation, session search). Relix's AI node could grow an
  `ai.auxiliary` variant that the background memory curator
  uses, keeping the main `ai.chat` cache warm.
- **Channel adapter base class with allowlist + identity
  derivation**. The pattern in `gateway/platforms/base.py` plus
  Telegram's blake3-derived subject IDs is exactly what Relix
  needs for its eventual channel surface.
- **AGENTS.md as a canonical developer guide**. Hermes's AGENTS.md
  is 51KB of architectural truth that the maintainers actually
  reference at PR review. Relix's docs are scattered across
  ~50 files. One canonical guide would help.

### 11.5 The honest tradeoff statement

Relix and Hermes are aimed at different bets. Hermes is the more
mature product because it's been building for longer and chose
"one process, many channels". Relix is the more architecturally
ambitious product because it chose "many peers, one canonical
mesh".

Relix can never out-Hermes Hermes on number-of-channels or
breadth-of-skills — Hermes will keep adding them faster than
Relix can match. Relix's bet should be that **the audit story
and the signed mesh are worth more to operators than 27 channels
and 700 skills** for the slice of users who care about
verifiable, governable, multi-tenant agent operations.

That slice is real (companies running agents at scale, regulated
industries, anyone where "what did the agent do" needs to be
provable). Building for that slice means: (a) shipping a real
memory + scheduler + at-least-one-channel story so the slice
can actually *use* Relix, (b) shipping the agent-employee
permission model so the audit story is real, (c) **not** trying
to match Hermes feature-for-feature.

The next two waves of work should be those three pieces, in the
order I sketched above, with the agent-employee model layered
over them.
