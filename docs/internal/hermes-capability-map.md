# Hermes Capability Map for Relix

Inventory + parity status of every Hermes Agent capability vs. what
Relix already implements vs. what's queued.

**Source:** automated scan of
`reference/hermes-agent-main/` (~150K LOC, 76 tools, 35 bundled
skills, 9 optional skill packs, 11+ platform integrations) by an
Explore sub-agent on 2026-05-21.

The map is the authoritative reference for the autonomous
Hermes-parity execution wave. Implementation status is tracked
per row; commits adding new parity should update the relevant
row's status field.

## How to read this doc

- **Hermes name** — exact name as it appears in Hermes sources.
- **What it does** — one-sentence summary.
- **Leverage** — high/med/low — ops value to a Relix operator.
- **Effort** — trivial/small/medium/large/very-large.
- **Relix status** — `shipped` / `partial` / `pending` / `deferred:DXXX`.
- **Relix counterpart** — capability or module name in Relix
  (or `—` when not yet built).
- **Notes** — implementation hints, architectural fit, gotchas.

---

## TOOLS

### Filesystem + files

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| file_tools | read/write/list/mkdir/rm/append | high | medium | shipped | tool.read_file, tool.write_file, tool.list_dir + fs::FsJail | jailed path | 
| file_operations | batch ops, encoding, validation | high | small | partial | tool.search_files + tool.patch | no batch wrapper yet |
| file_state | session-scoped file state (diffs, undo) | medium | medium | pending | — | requires session-store |
| patch_parser | parse + validate unified diffs | medium | small | shipped | tool.patch (diffy) | matches Hermes shape |
| credential_files | read/write credential vaults | medium | medium | partial | bridge-secrets.toml | bridge owns, not a tool |
| path_security | traversal guards + sandbox | high | trivial | shipped | fs::FsJail + tool.path_security | identical model |
| binary_extensions | binary sniff | low | trivial | pending | — | small, can ship when needed |

### Web + network

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| web_tools | HTTP GET/POST + headers + cookies | high | medium | partial | tool.web_fetch + tool.web_get | GET only today |
| url_safety | URLhaus / phishing checks | medium | medium | pending | — | requires external service |
| web_extract | DOM parse + CSS selectors | medium | medium | shipped | tool.web_extract | hand-rolled parser |
| x_search_tool | X search + trending | low | medium | deferred:DXXX | — | requires API key — operator decision |
| osv_check | Open Source Vulnerabilities | medium | medium | pending | — | net-bound; small wrapper |
| vision_tools | image OCR + landmark | medium | medium | pending | — | needs provider |
| transcription_tools | Whisper audio→text | medium | medium | pending | — | needs provider |
| feishu_doc_tool, feishu_drive_tool | Feishu doc ops | low | medium | pending | — | OAuth |

### Browser + interaction

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| browser_tool | Playwright automation | high | large | pending (CW4) | — | own controller crate likely |
| browser_camofox | anonymous browser mode | medium | medium | pending | — | extends browser_tool |
| browser_cdp_tool | direct CDP | medium | large | pending | — | requires CDP client |
| browser_dialog_tool | modal/alert/confirm | low | trivial | pending | — | trivial once browser shipped |
| browser_supervisor | session lifecycle, pool | high | medium | pending | — | core for browser_tool |
| computer_use_tool | mouse/kb/screenshot | medium | very-large | deferred:D-007 | — | requires VNC/HCB backend; decision needed |
| clarify_tool | interactive user approval | medium | small | pending | — | bridge has no chat-back channel yet |
| approval | async approval queue | medium | small | pending | — | needs operator dashboard surface |

### Terminal + code execution

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| terminal_tool | shell exec w/ env isolation | high | large | shipped (CW1) | tool.terminal.run | sandboxed, allowlisted |
| code_execution_tool | Python REPL | medium | large | pending | — | requires py runtime |
| terminal_summary | compress long terminal output | high | trivial | shipped | H2 summarizer covers `[terminal]` shape | hand-extend if needed |
| interrupt | SIGINT running agent | high | small | partial | task.pause / task.freeze (M70/M71) | cooperative not hard |
| process_registry | spawned PID tracking | medium | small | pending | — | useful for shell exec |

### Model + inference

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| mixture_of_agents_tool | ensemble inference | low | large | pending | — | exotic; defer |
| image_generation_tool | DALL-E/Flux/Midjourney | medium | medium | pending | — | provider adapter |
| video_generation_tool | Runway/Pika | low | large | pending | — | specialised service |
| neutts_synth / tts_tool | voice synthesis | low | medium | pending | — | audio I/O |
| voice_mode | voice I/O manager | low | medium | pending | — | audio hardware |

### Memory + context

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| memory_tool | MEMORY.md / USER.md / SESSION.md | high | medium | pending | — | per D-001 needs decision OR ship via task.memory |
| session_search_tool | full-text search history | medium | medium | partial | task.recent_events + dashboard filter | no FTS yet |
| checkpoint_manager | conversation snapshots | high | medium | pending | — | per user PHASE H6 |
| tool_result_storage | cache large outputs | medium | small | pending | — | cheap chronicle extension |

### Planning + orchestration

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| todo_tool | task list decomposition | high | trivial | shipped | task.todo_set / task.todo_list / task.todo_update (PH-WAVE2D) | per-task ordered list w/ open/done status |
| delegate_task | subagent spawning | high | large | partial | task.spawned_child + task.delegated_to events (M72) | edge events exist; no executor consumer yet |
| cronjob_tools | schedule/cancel background jobs | medium | medium | pending | — | needs scheduler |
| skill_manager_tool | load/unload skills | medium | large | pending | — | full skill ecosystem dep |

### Platforms + messaging

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| discord_tool | Discord DMs / channels | medium | medium | pending | — | mirror Telegram model |
| send_message_tool | cross-platform dispatch | medium | medium | partial | telegram channel | broaden to a router |
| homeassistant_tool | HA control | low | medium | pending | — | network ping wrapper |
| kanban_tools | board state | low | medium | pending | — | external |
| microsoft_graph_client | Teams / OneDrive | low | medium | pending | — | OAuth |
| yuanbao_tools | Alibaba Yuanbao | low | medium | pending | — | regional |

### Skills + extensions

| Hermes name | What it does | Leverage | Effort | Relix status | Relix counterpart | Notes |
|---|---|---|---|---|---|---|
| skills_hub | browse/install from cloud hub | medium | large | pending | — | per D-002 trust-tier decision |
| skills_tool | manage installed skills | medium | medium | pending | — | needs registry |
| skills_sync | sync skills between devices | low | large | pending | — | depends on hub |
| mcp_tool | MCP server discovery | medium | large | pending (CW5) | — | per D-002 |
| mcp_oauth_manager | OAuth for MCP | medium | medium | pending | — | follow MCP |

### Utility + meta

(All trivial/small — ship as-needed alongside the consumers.)
ansi_strip, debug_helpers, tool_output_limits, schema_sanitizer
(PH5 covers schema-level arg repair), managed_tool_gateway,
skills_guard, budget_config.

### Specialized

| Hermes name | What it does | Leverage | Effort | Relix status | Notes |
|---|---|---|---|---|---|
| tirith_security | code security scanning | medium | large | pending | external service |
| website_policy | robots.txt + ToS parsing | low | small | pending | small wrapper |

---

## SKILLS (bundled + optional)

Hermes ships **35 bundled skills + 9 optional skill packs**.
Relix does **not** yet have a skill-pack abstraction. The right
fit appears to be: a small `skill` directory format
(`skills/<name>/manifest.toml` + `*.sol` flow + `*.md` doc) that
the coordinator can load on demand. The full skill-ecosystem
build is several weeks of work; the table below is a backlog.

**Highest-leverage Hermes skills to port first:**

1. **GitHub** — gh CLI + PR workflows. Operator value: huge.
2. **Software Development** — git/lint/test/refactor. Reuses
   our existing fs/terminal capabilities.
3. **Diagramming** — Mermaid render via tool.web_fetch already
   half-shipped; small additive.
4. **DevOps** — container mgmt + deploy. Operator value: huge.
5. **Note Taking** — Obsidian / Markdown sync. Operator value:
   medium.

Defer: Apple/macOS, Gaming, Smart Home, Yuanbao, Inference.sh,
the regional/specialised ones. All can wait until after the
skill-ecosystem foundation lands.

---

## ORCHESTRATION HELPERS

The single most leverage-rich subset Hermes ships:

| Hermes module | What it does | Leverage | Effort | Relix status | Counterpart / next |
|---|---|---|---|---|---|
| conversation_loop.py | turn driver (~3900 LOC): model→tool→retry→compress→fallback | high | very-large | partial | dispatch + AI node together; full Hermes loop = PHASE H9 |
| error_classifier.py | FailoverReason enum + classify_api_error | high | medium | shipped | H1 failover.rs |
| retry_utils.py | jittered exponential backoff | high | trivial | shipped | relix_core::retry::Backoff (PH-WAVE2A) |
| iteration_budget.py | per-agent turn cap | high | trivial | pending | task.max_retries close; no per-turn yet |
| context_compressor.py | auto-summarize when nearing token limit | high | very-large | pending | per D-001; needs aux LLM client. Note: H2 chronicle summarizer + H14 terminal_summary auto-emit are upstream-of-LLM analogues that already cover the "summarize the chronicle" half of this. |
| context_engine.py | pluggable context strategies | medium | small | pending | plugin loader needed |
| memory_manager.py | MEMORY.md/USER.md orchestration | high | medium | pending | per D-001 |
| memory_provider.py (ABC) | external memory backends (Honcho, Hindsight, Mem0) | medium | large | pending | extends memory_manager |
| delegate_tool.py | subagent thread pool with restricted toolset | high | very-large | partial | M72 edges; no executor yet |

---

## PROVIDER + MODEL ROUTING

Hermes ships 30+ providers + 7 major adapters. Relix has 4
adapters (mock, openai-compat, anthropic, gemini-placeholder).
Highest-leverage parity:

| Capability | Hermes file | Leverage | Effort | Relix status | Counterpart / next |
|---|---|---|---|---|---|
| Rate-limit ladder (429 + body detection) | nous_rate_guard.py + account_usage.py + classify_api_error | high | medium | partial | H1 classifier covers detection; no ladder/cooldown yet |
| Prompt caching (cache_control) | anthropic_adapter.py prompt caching | high | small | shipped | system block sent as structured array with cache_control ephemeral (PH-WAVE2E) |
| Extended thinking (o1/o3) | lmstudio_reasoning.py | high | small | shipped | ChatInput.thinking_budget_tokens → Anthropic `thinking:{type:enabled,budget_tokens:N}` (PH-WAVE2F). Reasoning trace surface deferred. |
| Vision (multi-image, resize, cost est) | vision_tools.py | medium | medium | pending | needs ChatInput.images |
| Streaming SSE chunks | chat_completions transports | high | medium | partial | OpenAI shim streams; not all adapters |
| Structured output (JSON schema) | per-adapter | medium | medium | pending | adapter knob |
| Tool-use mode (native vs string parse) | per-adapter | high | medium | pending | future tool-use wave |
| Account usage polling | account_usage.py | high | small | pending | per-provider /usage endpoints |
| Credential rotation | OAuth managers | medium | medium | pending | future credential pool |

---

## PLATFORMS / GATEWAY

Hermes integrates 11+ platforms; Relix has 1 (Telegram). Top
3 most-asked-for:

1. **Discord** — chat bot mode, slash commands, threads.
2. **Slack** — bot mode + slash commands.
3. **Email** — IMAP poll + SMTP send, simplest of the three.

Architecture fit: each goes alongside `crates/relix-telegram/`
as a sibling crate with its own controller. The bridge needs no
changes (channels talk via the mesh).

---

## TOP-10 PRIORITISED NEXT MILESTONES

Per the Explore sub-agent's analysis, the ten capabilities Relix
would gain the most operator value from next:

1. **Error Classifier + Smart Failover** — shipped (H1) ✓
2. **Context Compression (aux LLM)** — pending, per D-001
3. **Memory Persistence (MEMORY.md/USER.md)** — pending, per D-001
4. **Prompt Caching (Anthropic)** — shipped (PH-WAVE2E) ✓
5. **Rate Limit Detection + Credential Rotation** — partial (H1
   classifier covers detection; ladder/cooldown pending)
6. **Delegation / Subagent Spawning** — partial (M72), needs
   executor
7. **Extended Thinking (o1/o3)** — shipped (PH-WAVE2F) ✓
8. **Browser Automation (Playwright)** — pending (CW4)
9. **Multi-Platform Channel Routing** — pending (Discord/Slack/Email)
10. **Skill Ecosystem + MCP** — pending (CW5), per D-002

Of those, items 1, 4, 5, 7 are the "small/medium effort, huge
ops value" cluster the autonomous wave should chase first.

---

## STATUS LEGEND

- `shipped` — feature is live in Relix.
- `partial` — partial parity (note explains gap).
- `pending` — not yet built; spec is ready.
- `deferred:D-XXX` — blocked on operator decision logged in
  `docs/internal/decisions-pending.md`.

## Decision touchpoints

This map references several open decisions that gate work:
- **D-001** — memory persistence architecture (file vs DB; per-task vs per-session)
- **D-002** — MCP trust tiers (operator-curated only vs full trust matrix)
- **D-003** — chronicle compaction threshold + automation
- **D-007** (new) — computer_use_tool requires VNC/HCB backend; should Relix
  ship its own backend or proxy through an external service?

When implementing a `deferred:` row, first check whether the
linked D-NNN has been answered; if yes, replace the deferred
marker with `pending` or `shipped`.
