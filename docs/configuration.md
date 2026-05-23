# Configuration

Complete reference for every config file, env var, TOML key,
policy rule, port, and bridge route in a Relix mesh. The
authoritative source is `scripts/relix-mesh-up.ps1` (and its
POSIX sibling `scripts/relix-mesh-up.sh`); this document mirrors
what those scripts emit so operators editing the files by hand
have an index.

For provider-specific detail see
[`provider-configuration.md`](provider-configuration.md);
this doc folds the per-provider tail into the AI-node section
below.

## Configuration files

Every controller is a peer with its own TOML config. The boot
script generates these from CLI flags + env vars under
`$DATA_BASE/` (default `dev-data/<run>/`) on every run. The
files are plain TOML — operators running a production mesh edit
them by hand and skip the boot script entirely.

Per-run layout:

```
dev-keys/
  <run>-org-root.key                 # org root signing key
  <run>-org-root.pub                 # org root public key (verifier)
  <run>-bridge.aic                   # bridge identity bundle
  <run>-bridge.key                   # bridge per-call signing key
  <run>-memory.bundle                # memory outbound identity bundle
  <run>-memory.key                   # memory per-call signing key
  <run>-ai.key / -tool.key / -coordinator.key
  <run>-telegram.bundle / .key       # if RELIX_TELEGRAM=1
  <run>-discord.bundle  / .key
  <run>-slack.bundle    / .key
  <run>-plugin-host.bundle / .key

dev-data/<run>/
  memory.toml         memory.db           memory.log
  ai.toml             ai.log
  tool.toml           fs-jail/            tool.log
  coordinator.toml    tasks.db            coordinator.log
  telegram.toml       telegram_sessions.db
  discord.toml
  slack.toml
  plugin-host.toml    plugin-registry.db
  bridge.toml         bridge.log
  peers.toml                              # bridge → peer alias map

configs/policies/<run>.toml                # shared admission policy
```

## Identity and trust

Every peer signs its outbound calls and verifies every inbound
call against the org root.

- **`<run>-org-root.key`** — the org's root signing key. Mints
  identity bundles for every peer. Lives on the operator's
  machine; never deployed.
- **`<run>-org-root.pub`** — the verifier half, distributed to
  every controller via `[trust] org_root_key_path` so each peer
  can validate signatures on inbound calls.
- **`*.aic` / `*.bundle`** — identity bundles. `.aic` was the
  alpha extension; `.bundle` is the current name. Functionally
  identical TOML. Contains the subject's name, groups, public
  key, and the org-root signature over those fields.
- **`*.key`** — per-peer ed25519 signing key the controller
  uses on every outbound call. Generated on first boot;
  persisted to disk.

`relix-cli identity init-org --root-key <path> --org <name>`
mints the root pair. `relix-cli identity mint --root-key
<root> --name <peer> --groups <comma-list> --out <bundle>`
mints a peer bundle. The boot script runs both idempotently —
existing files are reused.

## Per-node-type TOML

Every controller config starts with the same four blocks
(`[controller] [identity] [trust] [policy]`) and adds a
node-type-specific section. The blocks below are exactly what
the mesh-up scripts write.

### Memory node

```toml
[controller]
name = "<run>-memory"
node_type = "memory"
listen_port = 19711

[identity]
key_path = "dev-keys/<run>-memory.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[memory]
db_path = "dev-data/<run>/memory.db"

# Optional. Wires the embedding dispatcher so memory.embed /
# memory.search / memory.embed_all can dial an AI peer.
[memory.embedding_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 30
model = "mock-embed"            # or "text-embedding-3-small"
dimensions = 8                  # 1536 for OpenAI

# Optional. Spawns the background memory curator.
[memory.curator]
enabled = true
interval_secs = 3600
min_chars_to_curate = 100

[memory.curator.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 30

[peers]
```

### AI node

```toml
[controller]
name = "<run>-ai"
node_type = "ai"
listen_port = 19712

[identity]
key_path = "dev-keys/<run>-ai.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[ai]
provider = "mock"               # mock | openai | openrouter | xai |
                                # anthropic | gemini | local
model    = ""                   # caller default; empty = use provider's default_model

# Optional. Wires memory injection into ai.chat. When this
# block is set, the AI node dials the memory peer at startup
# and the ai.chat handler does TWO things automatically:
#
#   - memory.agent_read    → frozen agent + user memory block
#                            prepended to the system prompt
#   - memory.recent_for_session → recent conversation turns
#                            merged with the caller-supplied
#                            history field
#
# Either is silent-skip on failure; ai.chat never errors
# because memory is unavailable. See docs/memory.md.
[ai.memory_peer]
addr               = "/ip4/127.0.0.1/tcp/19711"
alias              = "memory"
deadline_secs      = 5
max_history_turns  = 10              # cap on automatic history fetch

[peers]
```

Provider tails — one is appended depending on `[ai] provider`:

```toml
[ai.providers.openai]
base_url      = "https://api.openai.com/v1"
api_key_env   = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"

[ai.providers.openrouter]
base_url      = "https://openrouter.ai/api/v1"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"

[ai.providers.xai]
base_url      = "https://api.x.ai/v1"
api_key_env   = "XAI_API_KEY"

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "claude-3-5-sonnet-latest"

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"

[ai.providers.local]
base_url      = "http://localhost:11434/v1"
# api_key_env intentionally unset for local servers
```

The `mock` provider needs no tail.

### Tool node

```toml
[controller]
name = "<run>-tool"
node_type = "tool"
listen_port = 19713

[identity]
key_path = "dev-keys/<run>-tool.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[tool]
max_bytes               = 262144
timeout_secs            = 15
max_redirects           = 3
allow_http              = false           # accept http:// fetches
user_agent              = "Relix-tool/0.1.0"
extract_max_input_bytes = 1048576

[tool.fs]
root                = "dev-data/<run>/fs-jail"
max_read_bytes      = 10485760
max_write_bytes     = 10485760
max_search_results  = 200

[tool.pdf]
max_input_bytes  = 20971520
max_pages        = 200
max_output_chars = 200000

[peers]
```

### Coordinator node

```toml
[controller]
name = "<run>-coordinator"
node_type = "coordinator"
listen_port = 19714

[identity]
key_path = "dev-keys/<run>-coordinator.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[coordinator]
db_path = "dev-data/<run>/tasks.db"
max_list = 200

# Optional. Opt in to the cron scheduler.
[coordinator.cron]
enabled = true
tick_secs = 30
max_concurrent = 3
max_job_secs = 300

[coordinator.cron.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 60

# Optional. Opt in to the delegation executor.
[coordinator.delegation]
enabled = true
max_depth = 3

[coordinator.delegation.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
alias = "ai"
deadline_secs = 60

[peers]
```

### Telegram node

```toml
[controller]
name = "<run>-telegram"
node_type = "telegram"
listen_port = 19715

[identity]
key_path = "dev-keys/<run>-telegram.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[telegram]
token_env                   = "RELIX_TELEGRAM_BOT_TOKEN"
allowed_users               = []           # numeric user_ids
operator_chat_id            = 0
messages_ring_capacity      = 200
flow_template               = "flows/chat_template.sol"
session_db_path             = "dev-data/<run>/telegram_sessions.db"
poll_interval_secs          = 1
approval_poll_interval_secs = 15

[telegram.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[telegram.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[telegram.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"

[peers]
```

### Discord node

```toml
[controller]
name = "<run>-discord"
node_type = "discord"
listen_port = 19716

[identity]
key_path = "dev-keys/<run>-discord.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[discord]
token_env              = "RELIX_DISCORD_BOT_TOKEN"
channel_id             = "0000000000"      # Discord snowflake (string)
allowed_users          = []                # snowflake strings
operator_user_id       = ""
messages_ring_capacity = 200
poll_interval_secs     = 2

[discord.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[discord.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[discord.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"

[peers]
```

### Slack node

```toml
[controller]
name = "<run>-slack"
node_type = "slack"
listen_port = 19717

[identity]
key_path = "dev-keys/<run>-slack.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[slack]
token_env              = "RELIX_SLACK_BOT_TOKEN"
channel_id             = "C000000000"      # Slack channel id (C/G/D prefix)
allowed_users          = []                # Slack user ids ("U01...")
operator_user_id       = ""
messages_ring_capacity = 200
poll_interval_secs     = 2

[slack.memory_peer]
addr = "/ip4/127.0.0.1/tcp/19711"

[slack.ai_peer]
addr = "/ip4/127.0.0.1/tcp/19712"
deadline_secs = 60

[slack.coord_peer]
addr = "/ip4/127.0.0.1/tcp/19714"

[peers]
```

### Plugin host node

```toml
[controller]
name = "<run>-plugin-host"
node_type = "plugin_host"
listen_port = 19718

[identity]
key_path = "dev-keys/<run>-plugin-host.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file = "configs/policies/<run>.toml"

[plugin_host]
plugin_dir       = "./plugins"
max_plugins      = 20
registry_db_path = "dev-data/<run>/plugin-registry.db"

[peers]
```

### Web bridge

```toml
[bridge]
listen_addr = "127.0.0.1:19791"

[identity]
bundle_path     = "dev-keys/<run>-bridge.aic"
client_key_path = "dev-keys/<run>-bridge.key"

[transport]
peers_path    = "dev-data/<run>/peers.toml"
deadline_secs = 60

[flow]
template_path      = "flows/chat_template.sol"
tool_template_path = "flows/chat_with_tool.sol"     # omitted when -NoTool

[sse]
chunk_bytes    = 24
chunk_delay_ms = 15

[openai_compat]
default_model = "relix-<provider>"

[[openai_compat.models]]
id          = "relix-<provider>"
description = "Relix mesh route - AI node currently set to <provider>"

# Appended only when the coordinator peer is enabled.
[coordinator]
alias = "coordinator"
```

The bridge resolves peer addresses through the separate
`peers.toml` map (one `[peers.<alias>]` block per controller —
`memory`, `ai`, `tool`, `coordinator`, optionally `telegram`,
`discord`, `slack`, `plugin_host`).

## Policy file

The policy file is loaded by every controller (`[policy] file
= ...`) and consulted on every inbound capability call. It is
**default-deny**: a method without a matching `[[rules]]` block
is rejected with `policy_denied` regardless of which groups the
caller has.

```toml
[admit]
groups = ["chat-users"]              # admit identities holding any listed group

[[rules]]
name = "node_health"
method = "node.health"
allow_groups = ["chat-users"]
```

Every method needs one rule. The boot script writes the
canonical mesh-wide policy at `configs/policies/<run>.toml`
covering every capability the alpha ships. The grouped list:

**Built-in (every controller)**

`node.health`, `node.manifest`, `node.dispatch.stats`,
`node.policy.simulate`, `node.policy.recent_denials`.

**Memory**

`memory.write_turn`, `memory.recent_for_session`,
`memory.search_turns`, `memory.search`, `memory.embed`,
`memory.embed_all`, `memory.agent_read`, `memory.agent_write`,
`memory.agent_curate`, `memory.curator_status`.

**AI**

`ai.chat`, `ai.embed`.

**Tool**

`tool.web_fetch`, `tool.web_extract`, `tool.read_file`,
`tool.write_file`, `tool.search_files`, `tool.patch`, `tool.pdf`.

**Coordinator — task ledger**

`task.create`, `task.update`, `task.event`, `task.get`,
`task.list`.

**Coordinator — cron**

`cron.create`, `cron.list`, `cron.get`, `cron.update`,
`cron.delete`, `cron.trigger`.

**Coordinator — delegation**

`delegate.spawn`, `delegate.result`, `delegate.cancel`,
`delegate.list`.

**Coordinator — agents + approvals**

`agent.create`, `agent.get`, `agent.list`, `agent.update`,
`agent.delete`, `agent.effective_capabilities`,
`coord.approval.pending`, `coord.approval.decide`,
`agent.standing_approval.create`,
`agent.standing_approval.list`,
`agent.standing_approval.revoke`.

**Coordinator — messaging**

`msg.send`, `msg.inbox`, `msg.read`, `msg.thread`, `msg.delete`.

**Channels**

`telegram.status`, `telegram.messages_recent`,
`discord.status`, `discord.messages_recent`,
`slack.status`, `slack.messages_recent`.

**Plugin host**

`plugin.list`, `plugin.status`, `plugin.reload`,
`plugin.disable`, plus each registered plugin capability
(e.g. `hello.greet`, `web_lookup.fetch`).

### The `plugin_host.<method>` alias pattern

`.sflow` callers reach a plugin capability by writing
`plugin_host.<plugin>.<method>` — the dotted target is
preserved verbatim by the parser. The plugin host registers
each management cap and each plugin cap under *both* names
(the bare name `plugin.list`, `hello.greet`, … and the
peer-prefixed alias `plugin_host.plugin.list`,
`plugin_host.hello.greet`, …). The policy file admits both
spellings for the same reason: `.sflow` and direct dispatch
need to share one allow-list.

This is why the policy block has paired rules
(`plugin_list` + `plugin_host_plugin_list`,
`hello_greet` + `plugin_host_hello_greet`,
`web_lookup_fetch` + `plugin_host_web_lookup_fetch`, etc.).

## Environment variables

Read by the boot scripts and the controllers themselves.

| Variable                                  | Read by                              | Purpose |
|-------------------------------------------|--------------------------------------|---------|
| `RELIX_DATA_DIR`                          | boot scripts, bridge, flow-runner    | Root directory for runtime data; default `dev-data`. |
| `RELIX_TELEGRAM`                          | boot scripts                         | `=1` enables the telegram controller. |
| `RELIX_TELEGRAM_BOT_TOKEN`                | telegram controller (via `token_env`)| BotFather token. |
| `RELIX_TELEGRAM_OPERATOR_CHAT_ID`         | boot scripts                         | Numeric chat id that gets approval / operator pings. |
| `RELIX_TELEGRAM_ALLOWED_USERS`            | boot scripts                         | Comma-separated numeric user_ids allowed to chat. |
| `RELIX_DISCORD`                           | boot scripts                         | `=1` enables the discord controller. |
| `RELIX_DISCORD_BOT_TOKEN`                 | discord controller (via `token_env`) | Discord bot token. |
| `RELIX_DISCORD_CHANNEL_ID`                | boot scripts                         | Discord channel snowflake. |
| `RELIX_DISCORD_OPERATOR_USER_ID`          | boot scripts                         | Operator snowflake. |
| `RELIX_DISCORD_ALLOWED_USERS`             | boot scripts                         | Comma-separated snowflake strings. |
| `RELIX_SLACK`                             | boot scripts                         | `=1` enables the slack controller. |
| `RELIX_SLACK_BOT_TOKEN`                   | slack controller (via `token_env`)   | `xoxb-...` bot token. |
| `RELIX_SLACK_CHANNEL_ID`                  | boot scripts                         | Slack channel id (`C.../G.../D...`). |
| `RELIX_SLACK_OPERATOR_USER_ID`            | boot scripts                         | Operator Slack user id. |
| `RELIX_SLACK_ALLOWED_USERS`               | boot scripts                         | Comma-separated Slack user ids. |
| `RELIX_PLUGINS`                           | boot scripts                         | `=1` enables the plugin host. |
| `RELIX_PLUGIN_DIR`                        | boot scripts                         | Directory the plugin host scans for `plugin.toml`; default `./plugins`. |
| `OPENAI_API_KEY`                          | AI node, `[ai.providers.openai]`     | Set when `[ai] provider = "openai"`. |
| `OPENROUTER_API_KEY`                      | AI node, `[ai.providers.openrouter]` | Set when provider is `openrouter`. |
| `XAI_API_KEY`                             | AI node, `[ai.providers.xai]`        | Set when provider is `xai`. |
| `ANTHROPIC_API_KEY`                       | AI node, `[ai.providers.anthropic]`  | Set when provider is `anthropic`. |
| `GEMINI_API_KEY`                          | AI node, `[ai.providers.gemini]`     | Set when provider is `gemini`. |
| `RUST_LOG`                                | every controller                     | tracing-subscriber log directive (e.g. `relix_runtime=info`). |

Provider keys never live anywhere except the AI node's
environment — not in the bridge, not in any channel node, not
in any client. See
[`provider-configuration.md`](provider-configuration.md) for
the credential-ownership contract.

## Ports

Default TCP ports the boot script binds. Each controller's
libp2p port carries mesh-internal traffic; the bridge port is
the only HTTP listener.

| Port  | Node          | Override |
|-------|---------------|----------|
| 19711 | memory        | `-MemPort` / `--mem-port` |
| 19712 | ai            | `-AiPort` / `--ai-port` |
| 19713 | tool          | `-ToolPort` / `--tool-port` |
| 19714 | coordinator   | `-CoordinatorPort` / `--coordinator-port` |
| 19715 | telegram      | `-TelegramPort` / `--telegram-port` |
| 19716 | discord       | `-DiscordPort` / `--discord-port` |
| 19717 | slack         | `-SlackPort` / `--slack-port` |
| 19718 | plugin_host   | `-PluginHostPort` / `--plugin-host-port` |
| 19791 | web-bridge    | `-BridgePort` / `--bridge-port` / `relix boot --bridge-port` |

`relix boot --bridge-port` forwards to the mesh-up scripts via
env vars. All other ports flow through the `--*-port` flags on
the scripts.

## Bridge HTTP surface

Every route in `crates/relix-web-bridge/src/main.rs`, grouped
by handler module. Each route is a thin translator over one or
more mesh capabilities.

### Health + chat

```
GET  /health                                — chat::health
POST /chat                                  — chat::chat
POST /chat/stream                           — chat::chat_stream (SSE)
POST /chat_with_tool                        — chat::chat_with_tool
```

### OpenAI shim

```
GET  /v1/models                             — openai::models
POST /v1/chat/completions                   — openai::chat_completions
```

### Tasks

(`crates/relix-web-bridge/src/tasks.rs`.)

```
GET    /v1/tasks                              — list
GET    /v1/tasks/count                        — count
GET    /v1/tasks/cursor                       — list_cursor
GET    /v1/tasks/:id                          — get_one
GET    /v1/tasks/:id/attempts                 — attempts
GET    /v1/tasks/:id/edges                    — edges
GET    /v1/tasks/:id/lineage_graph            — lineage_graph (BFS, ?depth=)
GET    /v1/tasks/edges/recent                 — recent_edges
GET    /v1/tasks/events/recent                — recent_events
GET    /v1/tasks/events/stream                — events_stream_global (SSE)
GET    /v1/tasks/stuck                        — stuck (recovery diagnostic)
GET    /v1/tasks/:id/todos                    — todo_list
PUT    /v1/tasks/:id/todos                    — todo_put
PATCH  /v1/tasks/:id/todos/:todo_id           — todo_patch
GET    /v1/tasks/:id/summary                  — summary
GET    /v1/tasks/:id/events                   — events
GET    /v1/tasks/:id/events/stream            — events_stream (SSE)
GET    /v1/tasks/:id/lineage                  — lineage
GET    /v1/tasks/:id/export                   — export
GET    /v1/tasks/compact_events               — compact_events_dry_run
POST   /v1/tasks/recover                      — recover
POST   /v1/tasks/:id/retry                    — retry
POST   /v1/tasks/:id/replay                   — replay
POST   /v1/tasks/:id/cancel                   — cancel
POST   /v1/tasks/:id/note                     — note (operator annotation)
POST   /v1/tasks/:id/investigation            — investigation
POST   /v1/tasks/:id/pause                    — pause
POST   /v1/tasks/:id/resume                   — resume
POST   /v1/tasks/:id/freeze                   — freeze
POST   /v1/tasks/:id/unfreeze                 — unfreeze
```

### Capabilities + topology

(`capabilities.rs`, `topology.rs`.)

```
GET  /v1/capabilities                         — list
GET  /v1/capabilities/:method                 — get_one
GET  /v1/topology                             — get (peer/freshness view)
GET  /v1/topology/events                      — lifecycle_events
GET  /v1/streams                              — streams_list
GET  /v1/routing                              — routing_snapshot
GET  /v1/health                               — health (JSON; distinct from /health plaintext)
GET  /v1/dispatch/stats                       — dispatch_stats::stats
GET  /v1/policy/simulate                      — policy_simulate::simulate
GET  /v1/policy/denials                       — policy_denials::denials
```

### MCP

(`mcp.rs`.)

```
GET  /v1/mcp/servers                          — servers
GET  /v1/mcp/tools                            — tools
POST /v1/mcp/invoke                           — invoke
GET  /v1/mcp/audit                            — audit
```

### Tool audits + diagnostics

```
GET  /v1/fs/audit                             — fs_audit::audit
GET  /v1/terminal/audit                       — term_audit::audit
GET  /v1/tool/blocklist                       — blocklist::blocklist
GET  /v1/browser/sessions                     — browser_sessions::sessions
GET  /v1/browser/captures/:filename           — browser_captures::capture
```

### Memory

(`agent_memory.rs`, `memory_curator.rs`, `memory_embed.rs`.)

```
GET  /v1/memory/agent                         — agent_memory::agent_memory
POST /v1/memory/curate                        — memory_curator::curate
GET  /v1/memory/curator/status                — memory_curator::status
POST /v1/memory/embed                         — memory_embed::embed
POST /v1/memory/search                        — memory_embed::search
POST /v1/memory/embed_all                     — memory_embed::embed_all
```

### Channels

(`discord.rs`, `slack.rs`, `telegram.rs`.)

```
GET  /v1/telegram/status                      — telegram::status
GET  /v1/telegram/messages/recent             — telegram::messages_recent
GET  /v1/discord/status                       — discord::status
GET  /v1/discord/messages/recent              — discord::messages_recent
GET  /v1/slack/status                         — slack::status
GET  /v1/slack/messages/recent                — slack::messages_recent
```

### Plugins

(`plugins.rs`.)

```
GET  /v1/plugins                              — list
GET  /v1/plugins/:plugin_id                   — status
POST /v1/plugins/:plugin_id/reload            — reload
POST /v1/plugins/:plugin_id/disable           — disable
```

### Cron

(`cron.rs`.)

```
GET    /v1/cron/jobs                          — list
POST   /v1/cron/jobs                          — create
GET    /v1/cron/jobs/:job_id                  — get_one
PATCH  /v1/cron/jobs/:job_id                  — update
DELETE /v1/cron/jobs/:job_id                  — delete
POST   /v1/cron/jobs/:job_id/trigger          — trigger
```

### Delegation

(`delegate.rs`.)

```
POST /v1/delegate/spawn                       — spawn
GET  /v1/delegate/result/:child_task_id       — result
POST /v1/delegate/cancel/:child_task_id       — cancel
GET  /v1/delegate/list/:parent_task_id        — list
```

### Agents + approvals

(`agent.rs`.)

```
GET    /v1/agents                                          — list_agents
POST   /v1/agents                                          — create_agent
GET    /v1/agents/:agent_id                                — get_agent
PATCH  /v1/agents/:agent_id                                — update_agent
DELETE /v1/agents/:agent_id                                — delete_agent
GET    /v1/approvals                                       — pending_approvals
POST   /v1/approvals/:approval_id/decide                   — decide_approval
GET    /v1/agents/:agent_id/standing-approvals             — list_standing
POST   /v1/agents/:agent_id/standing-approvals             — create_standing
DELETE /v1/standing-approvals/:standing_id                 — revoke_standing
```

### Messaging

(`messaging.rs`.)

```
POST   /v1/messages                                        — send
GET    /v1/messages/inbox/:subject_id                      — inbox
POST   /v1/messages/:message_id/read                       — read
GET    /v1/messages/thread/:thread_id                      — thread
DELETE /v1/messages/:message_id                            — delete
```

### SOL

(`sol_validate.rs`.)

```
POST /v1/sol/validate                         — validate (parse-only)
```

### Config + providers

(`config_api.rs`.)

```
GET    /v1/config                                          — get_effective_config
GET    /v1/config/providers                                — list_providers
GET    /v1/config/providers/:name                          — get_provider
PUT    /v1/config/providers/:name                          — put_provider
DELETE /v1/config/providers/:name                          — delete_provider
POST   /v1/config/providers/:name/test                     — test_provider
PUT    /v1/config/providers/:name/enabled                  — set_provider_enabled
PUT    /v1/config/providers/:name/quarantine               — set_provider_quarantine
PUT    /v1/config/providers/default                        — put_default_provider
GET    /v1/providers/health                                — providers_health
POST   /v1/providers/route_test                            — route_test (HealthAwareRouter preview)
GET    /v1/config/telegram                                 — get_telegram
PUT    /v1/config/telegram                                 — put_telegram
POST   /v1/config/telegram/test                            — test_telegram
```

### Intervention audit + dashboard

```
GET  /v1/intervention/recent                  — intervention_audit::recent
GET  /dashboard                               — dashboard::page (static HTML)
```

See the named handler modules under
`crates/relix-web-bridge/src/` for request/response schemas.
The bridge stays translation-only — every route maps to one or
more capability calls into the mesh.
