# Changelog

All notable changes to Relix are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once a stable release is cut.

## [Unreleased]

## [0.1.0] - 2026-05-23

First public alpha. Everything below is real and ships.

### Mesh and dispatch

- Mesh of OS-process peers connected via libp2p (`/relix/rpc/1`
  over TCP + Noise XK + Yamux). CBOR envelopes carry caller's
  signed `IdentityBundle`, method, args, deadline.
- Six controller node types (`memory`, `ai`, `tool`, `coordinator`,
  `router`, `plugin_host`) plus the `relix-web-bridge` HTTP front.
  Each node is its own OS process with its own dispatch bridge.
- Admission pipeline on every responder: decode → identity verify
  → deadline check → `PolicyEngine` evaluate → handler dispatch
  → audit append. The audit log is signed and hash-chained
  (`relix-core/src/eventlog.rs`).
- Five built-in capabilities on every node: `node.health`,
  `node.manifest`, `node.dispatch.stats`, `node.policy.simulate`,
  `node.policy.recent_denials`.

### AI and memory

- `ai.chat` and `ai.embed` on the `ai` node, with provider routing
  for `mock`, `openai`, `openrouter`, `xai`, `anthropic`, `gemini`,
  and a `local` Ollama-compatible base URL. Provider keys live only
  in the AI node's local config.
- `memory.write_turn`, `memory.recent_for_session`,
  `memory.search_turns` (FTS5) on the `memory` node — SQLite-backed
  per-session conversation history.
- Vector memory: `memory.embed`, `memory.search` (cosine,
  top-K up to 20), `memory.embed_all`. Default 8-dim mock vectors;
  switch the AI node to OpenAI-compatible to get real
  `text-embedding-3-small`. See `docs/vector-memory.md`.
- Persistent agent memory: `memory.agent_read`, `memory.agent_write`,
  `memory.agent_curate`, `memory.curator_status`.

### Tools

- File system: `tool.read_file`, `tool.write_file`, `tool.append_file`,
  `tool.patch`, `tool.patch_preview`, `tool.fuzzy_replace`,
  `tool.search_files`, `tool.list_dir`, `tool.fs.tree`,
  `tool.fs.stat`, `tool.binary_sniff`, `tool.fs.audit_recent` —
  all scoped to operator-configured jail roots.
- Web: `tool.web_fetch`, `tool.web_get`, `tool.web_search`,
  `tool.web_extract`, `tool.web.post`, `tool.web.robots_check`,
  `tool.web.blocklist_summary` — SSRF-guarded, blocklist-aware.
- Terminal: `tool.terminal.run` and friends — allowlisted commands
  only, via `portable-pty`. Sessions are pausable, resumable, and
  fully audited.
- Browser automation: `tool.browser.*` — headless Chrome / WebDriver
  with per-session lifecycle.
- MCP integration: `tool.mcp.list_servers`, `tool.mcp.list_tools`,
  `tool.mcp.invoke` — registers external MCP servers as proxied
  capabilities.
- PDF and text: `tool.pdf`, `tool.text.chunk`.

### Coordinator

- Durable task ledger: `task.create`, `task.update`, `task.event`,
  `task.list`, `task.get`, `task.attempt`, `task.todo`,
  `task.metadata`, `task.link_parent`, `task.cancel`, `task.retry`,
  `task.recover`, `task.replay`, `task.lineage`, plus pause/resume/
  freeze/unfreeze and note/investigation.
- Multi-agent coordination: `delegate.spawn`, `delegate.result`,
  `delegate.cancel`, `delegate.list` with a configurable depth cap.
- Inter-task messaging: `msg.send`, `msg.inbox`, `msg.read`,
  `msg.thread`, `msg.delete` with TTL.
- Cron / scheduler: `cron.create`, `cron.list`, `cron.get`,
  `cron.update`, `cron.delete`, `cron.trigger` — supports cron
  expressions, duration intervals, and one-shot.

### Channels

- Telegram, Discord, and Slack channel controllers. Each polls the
  bot platform's API, forwards messages to AI through the same SOL
  flow used by the HTTP bridge, and persists conversation history
  in `memory`. Opt-in per channel via env vars.

### Plugins

- `plugin_host` node type with `relix-plugin-v1` HTTP/JSON protocol
  for subprocess plugins. SDK crate (`relix-plugin-sdk`) for Rust
  authors; the protocol is the contract, so plugins in any language
  that can speak HTTP are supported (Python example ships).
- Management capabilities: `plugin.list`, `plugin.status`,
  `plugin.reload`, `plugin.disable`. Each registered under both the
  bare name and a `plugin_host.<method>` alias so both SOL and
  `.sflow` can call them.

### Orchestration

- **SOL** — a small Rust-like imperative DSL with one mesh primitive,
  `remote_call(peer, method, args)`. Typed `str` values, `let`, `if`,
  `while`, `for`, function definitions, `print`, `return`.
- **`.sflow`** — a line-oriented step-based DSL with `if`/`elif`/
  `else`, `loop N times`, `while`, `until`, `try`/`catch`/`rethrow`,
  `set var = ...`, `${var}` interpolation, and `sol.log` /
  `sol.sleep` / `sol.assert` / `sol.set_result` built-ins. The
  parser preserves the user's dotted target verbatim as
  `wire_method`, so plugin and multi-segment capabilities admit
  correctly.

### HTTP bridge

- OpenAI-compatible `/v1/chat/completions` (including SSE
  streaming via `/chat/stream`) routed through the SOL chat flow.
- Operator dashboard at `/dashboard` showing topology, tasks, cron
  jobs, policy denials, audit ring, plugins, and per-channel status.
- Direct HTTP surfaces for every operator workflow listed above —
  see `docs/configuration.md` and the route list in
  `crates/relix-web-bridge/src/main.rs`.

### CLI

- `relix-cli` (installed as `relix`) with subcommands `identity`,
  `ping`, `task`, `capability`, `topology`, `ops`, `router`, `mcp`,
  `fs`, `web`, `browser`, `sol`, `doctor`, `terminal`, `flow-run`.
- New top-level wrappers: `relix boot`, `relix stop`, `relix status`
  — cross-platform mesh control over the underlying PowerShell /
  bash boot scripts.

### Tooling

- GitHub Actions CI (`fmt`, `clippy -D warnings`, `test --workspace`
  on Linux / macOS / Windows).
- Cross-platform install: `install.sh` (Mac / Linux) and
  `install.ps1` (Windows) that fetch pre-built release binaries.
- Mesh boot scripts: `scripts/relix-mesh-up.ps1` (Windows) and
  `scripts/relix-mesh-up.sh` (POSIX), with `relix-mesh-down.sh` for
  shutdown.

[Unreleased]: https://github.com/itsramananshul/Relix/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/itsramananshul/Relix/releases/tag/v0.1.0
