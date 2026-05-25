# Changelog

All notable changes to Relix are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once a stable release is cut.

## [Unreleased]

## [0.1.5] - 2026-05-25

Boot-loop polish on top of the v0.1.4 install fixes. No
mesh-protocol or wire-format changes — same binaries, same flow
templates, same configs.

### Fixed

- **`relix boot` now blocks the terminal until the mesh stops**
  instead of returning the prompt as soon as the bridge becomes
  healthy. Previously the boot script's cleanup output raced the
  shell prompt — operators saw their prompt back before the
  controllers had finished tearing down on `relix stop` from
  another terminal. The boot command now waits on the script's
  exit and forwards Ctrl-C through to it.
- **PowerShell mesh script: replaced `TreatControlCAsInput` loop
  with a 500ms poll loop** that works correctly when the script is
  launched via `Command::spawn` from `relix boot`. The old loop
  silently no-op'd in non-interactive spawned contexts, leaving
  the script running forever after a clean `relix stop`.

## [0.1.1] - 2026-05-24

Zero-configuration install. After this release the
`curl | bash` / `irm | iex` one-liner ends with a running mesh
and an open dashboard — no env vars to export, no scripts to
clone, no flags to remember.

### Added

- **`relix setup`** — guided interactive wizard. Five pages
  (welcome → provider picker → hidden API-key input → channel
  multi-select with per-channel secret follow-ups → confirm and
  save). Runs automatically at the end of `install.sh` /
  `install.ps1`; can be re-run any time to change provider,
  rotate keys, or add a channel. crossterm-driven raw terminal
  input; Ctrl-C exits 130 with the terminal restored.
- **`~/.relix/config.toml`** — persistent operator config. Holds
  `[provider]` (name + api_key), `[channels]` (per-channel
  toggle + token + channel-id), and `[mesh]` (data_dir,
  bridge_port). Written `chmod 600` on POSIX via tmp-write +
  rename so an interrupted save can't half-write the file.
  Every field has a serde default so partial configs deserialise.
- **Config-driven `relix boot`** — reads
  `~/.relix/config.toml` on startup and translates it into the
  env vars the mesh-up script consumes. The right
  `OPENROUTER_API_KEY` / `OPENAI_API_KEY` / etc. is set
  automatically from `provider.api_key`; channel toggles +
  tokens are wired through. Explicit `--with-*` flags still
  stack on top.
- **`memory.recent_for_session` auto-injection** — `[ai.memory_peer]
  max_history_turns = N`. With this set, the AI node fetches
  recent turns itself and merges them with any caller-supplied
  history, so flow templates no longer need to chain
  `memory.recent_for_session` → `ai.chat` manually. Silent skip
  on memory peer failure.
- **RAG retrieval** — `[ai.memory_peer] rag_enabled = true` +
  `rag_top_k` + `rag_min_score`. When set, the AI node embeds
  the user prompt locally and queries `memory.search` across
  both agent and user vector stores, formatting the top-K hits
  as a "Relevant context from memory" block prepended to the
  system prompt. `memory.search` wire grew an optional
  `embedding=<base64-LE-f32>` 5th field so the precomputed
  vector skips the responder's own embed RPC. Silent skip on
  empty results, embedding failure, or peer unreachable.
- **`GET /ws/chat`** — WebSocket streaming endpoint. JSON
  request `{session_id, message, model?}` followed by a stream
  of `{type: "chunk", text: "..."}` frames terminated by
  `{type: "done", session_id, text}`. Bearer auth on the
  upgrade (`Authorization: Bearer <token>`; loopback alpha
  accepts any non-empty token). `ChatProvider` gained
  `generate_reply_stream`; the mock provider streams
  word-by-word with a 20ms gap, and the OpenAI-compatible
  provider parses real `delta.content` deltas from the upstream
  SSE response.
- **`relix boot` / `relix stop` / `relix status`** — top-level
  CLI subcommands implemented in `crates/relix-cli/src/mesh.rs`.
  Cross-platform shim around the mesh-up scripts; `stop` kills
  by name (`taskkill /F /IM` on Windows, `pkill -x`
  elsewhere); `status` polls `/health` + `/v1/topology` and
  prints a peer-by-peer table.
- **`relix setup` bundled with install** — install scripts now
  call `relix setup` as their last step. They also fetch the
  mesh-up + mesh-down scripts from the main branch and drop
  them in `~/.local/scripts/` so `relix boot` has them after a
  binary-only install. `scripts/relix-mesh-down.ps1` ships as
  the Windows counterpart to `relix-mesh-down.sh`.
- **All three binaries in each release archive** — every
  per-target archive now contains `relix` (= `relix-cli`),
  `relix-controller`, and `relix-web-bridge` so `relix boot`
  can spawn its siblings from the same directory.

### Changed

- **Default data dir** is now `~/.relix/data/<run>/` instead of
  the repo-relative `dev-data/<run>/`. Repo-checkout
  development still uses `dev-data/` automatically. Docs and
  README updated.
- **README + getting-started** rewritten around the wizard
  flow. Env-var exports for API keys are no longer the
  recommended path — config-file primary, env-var fallback.
- **CI workflow** runs on manual `workflow_dispatch` only;
  contributors run the same gates locally
  (`cargo fmt --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace`). Re-enable push
  triggers when CI gates are needed on every commit.

### Fixed

- `install.ps1` no longer crashes with "the property 'Count'
  cannot be found on this object" under PowerShell strict mode
  when the release zip contains a single `relix.exe`.
- `parse_literal_ip` in `tool.web_fetch`'s SSRF guard now
  strips brackets from IPv6 hosts (`url::Url::host_str()`
  returns IPv6 with brackets); previously `[::1]` and
  `[fe80::1]` fell through to DNS and were rejected as
  `DnsFailed` on Linux/macOS instead of `IpForbidden`.
- `.sflow` parser preserves the user's dotted target verbatim
  as `wire_method`, and plugin capabilities are double-
  registered (bare name + `plugin_host.<method>` alias) so the
  natural `step x: plugin_host.hello.greet "..."` form admits
  against the bridge handler.

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

[Unreleased]: https://github.com/itsramananshul/Relix/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/itsramananshul/Relix/releases/tag/v0.1.5
[0.1.1]: https://github.com/itsramananshul/Relix/releases/tag/v0.1.1
[0.1.0]: https://github.com/itsramananshul/Relix/releases/tag/v0.1.0
