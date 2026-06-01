# Configuration

> Version 0.4.1 — complete reference for every TOML key, environment variable,
> and feature flag across the Relix mesh.

## The config file

For day-to-day use, the only file you touch is
**`~/.relix/config.toml`**. The setup wizard (`relix setup`)
writes it on first install; `relix boot` reads it on every run.
You can also edit it by hand — it's plain TOML and reloaded on
each boot.

Path:

| Platform        | Config location                              |
|-----------------|----------------------------------------------|
| macOS / Linux   | `~/.relix/config.toml`                       |
| Windows         | `%USERPROFILE%\.relix\config.toml`           |

Override with `$RELIX_HOME` if you want a different parent dir.
The file is written `chmod 600` on POSIX because it holds your
API keys.

Full schema with every field's type and default:

```toml
[provider]
name    = "mock"     # Default. Enum: mock | openai | openrouter | xai | anthropic | gemini | local
api_key = ""         # Required for all except mock and local

[channels]
telegram        = false
telegram_token  = ""    # Required when telegram = true
discord         = false
discord_token   = ""    # Required when discord = true
discord_channel = ""    # Required when discord = true
slack           = false
slack_token     = ""    # Required when slack = true
slack_channel   = ""    # Required when slack = true

[mesh]
data_dir    = "~/.relix/data"   # Default
bridge_port = 19791              # Default

[mesh.rate_limits]
ai_calls_per_min        = 60     # Default
dashboard_polls_per_min = 120    # Default
task_mutations_per_min  = 30     # Default
ws_max_concurrent       = 5      # Default

[coordinator]
# No keys needed unless enabling the subsystems below.

[coordinator.retention]
enabled               = false
max_task_age_days     = 30
max_events_per_task   = 500
compact_interval_h    = 24
max_passes_per_run    = 10

[confidence]
enabled                 = false
window_size             = 100
p95_latency_baseline_ms = 1500

[credentials]
enabled    = false
master_key = ""    # Generated at wizard time; never a hardcoded default

[approvals]
enabled = false
channel = "dashboard"   # Default: dashboard | telegram | slack | discord | email
```

`[channels]` and `[mesh]` sections (and every field within) have
defaults, so a minimal config is just `[provider]`. Add a section
later by editing the file or re-running `relix setup` — the
wizard pre-fills existing values so you only fill in what's new.

### Setup wizard (7 pages)

`relix setup` / `relix reconfigure` runs an interactive 7-page TUI:

1. **Pre-flight** — checks Docker, Ollama, Qdrant; if Qdrant not
   running, prompts to boot with or without memory.
2. **Welcome** — version banner.
3. **Provider** — arrow keys: `openrouter` (default), `openai`,
   `anthropic`, `xai`, `gemini`, `local`, `mock`.
4. **API Key** — skipped for `mock` / `local`; input hidden.
5. **Channels** — multi-select: Telegram, Discord, Slack.
6. **Confidence** — single toggle for `enabled`.
7. **Subsystems** — credential vault + approval delivery toggles.
   Vault generates a 32-byte random master key when enabled. Key
   is printed once — not recoverable.
8. **Confirm** — shows diff; Enter saves; left-arrow goes back.

Ctrl-C from any page exits with code 130. Non-TTY stdin skips raw
mode and saves defaults (for `curl | bash` / `irm | iex`).

## Per-node TOML

Every controller is a peer with its own TOML config. The boot
script generates these from the operator config + CLI flags into
`$DATA_BASE/` (default `~/.relix/data/<run>/`) on every run. The
files are plain TOML — operators running a production mesh can
edit them by hand and skip the boot script entirely.

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

### Common blocks (all nodes)

```toml
[controller]
name        = "<run>-<node>"   # string; used in data dir path and heartbeat
node_type   = "memory"         # memory|ai|tool|coordinator|telegram|discord|slack|email|plugin_host
listen_port = 19711            # libp2p TCP port
role        = "controller"     # "router" enables the router observability role
# router_peer_id = ""          # base58 libp2p peer-id of the designated router
# session_ttl_secs = 1800      # router only: TTL for completed/failed sessions

[identity]
key_path = "dev-keys/<run>-<node>.key"

[trust]
org_root_key_path = "dev-keys/<run>-org-root.pub"

[policy]
file               = "configs/policies/<run>.toml"
# dir              = "configs/policies/tenants/"   # per-tenant policy dir
# tenant_cache_ttl_secs = 60                       # seconds to cache per-tenant engine

[audit]
partition_by_tenant = false           # when true, mirrors every audit record to SQLite
# db_path           = "<data_dir>/audit-partition.db"

[peers]
# [peers.<alias>]
# port = 19712
```

`[policy] dir` points to a directory of per-tenant policy files
(`{dir}/{tenant_id}.policy.toml`). Missing tenant files fall
through to the global policy.

`[audit] partition_by_tenant = true` enables the queryable
SQLite audit mirror accessible via `node.audit.tenant_list` and
`node.audit.tenant_recent` capabilities.

### Memory node

```toml
[memory]
db_path         = "dev-data/<run>/memory.db"
max_n           = 100         # hard limit on N for recent_for_session / search_turns

# Optional. Wires the embedding dispatcher so memory.embed /
# memory.search / memory.embed_all can dial an AI peer.
[memory.embedding_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 30
model         = "text-embedding-3-small"   # passed to ai.embed
dimensions    = 1536                        # accepted but not enforced

# Optional. Qdrant-backed four-layer vector store.
[memory.qdrant]
url                = "http://localhost:6333"  # empty string = disabled
collection         = "relix_memory"
dim                = 1536
# api_key          = ""                       # sent as api-key header (not Bearer)
tenant_isolation   = false                    # when true, per-tenant collections
collection_prefix  = "relix"                  # prefix for derived collection names

# Optional. Background embedding pipeline.
[memory.embedder]
enabled           = false
batch_size        = 32
interval_secs     = 60
score_threshold   = 0.75   # cosine floor applied to Qdrant search results

# Optional. Spawns the background memory curator.
[memory.curator]
enabled                   = true
interval_secs             = 3600
min_chars_to_curate       = 100
promotion_enabled         = false    # enables LayerPromoter background task
promotion_interval_secs   = 300
promotion_batch_size      = 20
dialectic_model           = "openrouter/anthropic/claude-3-5-haiku"

[memory.curator.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 30

[memory.curator.coord_peer]   # optional; for curator chronicle events
addr          = "/ip4/127.0.0.1/tcp/19714"
alias         = "coordinator"
deadline_secs = 10

# Optional. PII handling at write time.
[memory.pii]
enabled  = false
strategy = "redact"    # redact | pseudonymize | allow
# overrides = {}       # per-PII-type strategy map
```

**Tenant isolation:** two independent planes.

- SQLite (`LayeredMemoryStore`): opt-in; tenant-aware methods
  fail-closed on empty `tenant_id` when enabled.
- Qdrant: `tenant_isolation = true` routes each tenant to its
  own collection (`{collection_prefix}_{sanitized_tenant_id}`).
  Empty tenant → `MissingTenant` error (fail-closed).

Collection name sanitization: non-alphanumeric-non-underscore
chars become `_`; empty defaults to `"default"`; truncated to
63 chars.

### AI node

```toml
[ai]
provider = "mock"   # mock | openai | openrouter | xai | anthropic | gemini | local
model    = ""       # empty = use provider's default_model

# Optional. Per-provider endpoint + key config.
[ai.providers.openai]
base_url      = "https://api.openai.com/v1"
api_key_env   = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"
timeout_secs  = 60

[ai.providers.openrouter]
base_url      = "https://openrouter.ai/api/v1"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"
timeout_secs  = 60

[ai.providers.xai]
base_url      = "https://api.x.ai/v1"
api_key_env   = "XAI_API_KEY"
timeout_secs  = 60

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "claude-3-5-sonnet-latest"
timeout_secs  = 60

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
default_model = "gemini-2.0-flash"
timeout_secs  = 60

[ai.providers.local]
base_url     = "http://localhost:11434/v1"
# api_key_env intentionally unset for local/Ollama servers
timeout_secs = 60

# Optional. Wires memory injection into ai.chat.
[ai.memory_peer]
addr              = "/ip4/127.0.0.1/tcp/19711"
alias             = "memory"
deadline_secs     = 5
max_history_turns = 10       # cap on automatic history fetch
rag_enabled       = false    # embed prompt and vector-search before provider call
rag_top_k         = 5        # max RAG hits to include
rag_min_score     = 0.70     # cosine floor for RAG hits

# Optional. Soul/persona file.
[ai.agent]
name       = ""    # triggers soul discovery at ~/.relix/souls/<name>.md
# soul_path = ""   # explicit path wins over name-based discovery

# Optional. Tier-based complexity routing.
[ai.routing]
enabled = false
# [ai.routing.tiers.simple]
#   provider = "openrouter"
#   model    = "openai/gpt-4o-mini"
# [ai.routing.tiers.medium]
#   provider = "openrouter"
#   model    = "openai/gpt-4o"
# [ai.routing.tiers.complex]
#   provider = "anthropic"
#   model    = "claude-3-5-sonnet-latest"

# Optional. LLM-driven belief tracking.
[ai.belief_state]
enabled                = false
# belief_model         = ""   # provider name override
belief_model_name      = ""   # empty = provider default cheap model
max_beliefs            = 10
min_confidence_to_retain = 0.55
inject_into_prompt     = true

# Optional. Judge model.
[ai.judge]
enabled             = false
# judge_model       = ""    # provider name override
judge_model_name    = ""    # empty = provider default
judge_threshold     = 0.6   # confidence ceiling for judge activation
max_judge_latency_ms = 6000  # exceeded → synthetic "proceed" verdict
recent_buffer_size  = 256   # ring buffer depth for judge.recent_verdicts

# Optional. Two-stage perception security for ai.perception_extract.
[ai.perception_security]
enabled          = false
extraction_model = ""    # empty = controller default model
max_output_chars = 8192
```

The `mock` provider needs no `[ai.providers.mock]` tail.

API keys are stored in `zeroize::Zeroizing<String>` and wiped
on provider drop. Keys are read at startup from the named env
var; a missing env var causes a startup crash. Local/Ollama
providers: leave `api_key_env` unset or empty — no auth header
is sent.

Anthropic system blocks are always sent with `cache_control:
{type: ephemeral}` for prompt-caching (~90% cost reduction on
repeated system prompts within ~5 min). Extended thinking is
activated by setting `thinking_budget_tokens` in a `ChatInput`.

### Tool node

```toml
[tool]
max_bytes               = 262144    # 256 KiB; hard cap on fetch/post response body
timeout_secs            = 15        # total per-request deadline
max_redirects           = 3         # 0 disables all redirects
allow_http              = false      # permit http:// URLs
user_agent              = "Relix-tool/<version>"
extract_max_input_bytes = 1048576   # 1 MiB; cap for tool.web_extract HTML input
blocked_hosts           = []         # exact hostname match; case-insensitive
url_allowlist           = []         # glob host allowlist; empty = no restriction
ssrf_protection         = true       # false logs WARNING and disables private-IP block

# NOTE: [tool] uses #[serde(deny_unknown_fields)]; unknown keys are hard parse errors.

# Optional. Enables tool.read_file / tool.write_file / tool.search_files /
# tool.patch and 8 other fs capabilities.
[tool.fs]
root               = "dev-data/<run>/fs-jail"
max_read_bytes     = 10485760    # 10 MiB
max_write_bytes    = 10485760    # 10 MiB
max_search_results = 200

# Optional. Enables tool.pdf.
[tool.pdf]
max_input_bytes  = 20971520    # 20 MiB
max_pages        = 200
max_output_chars = 200000

# Optional. Enables tool.terminal.* (10 capabilities).
[tool.terminal]
allowed_commands  = []         # bare program names only; required for run/spawn
allowed_shells    = []         # bare names for shell.open
max_timeout_secs  = 30
inherit_env       = false      # false = env_clear() + PATH only
# working_dir     = ""         # child cwd; default = controller cwd
allowed_dirs      = []         # cwd restriction; empty = unrestricted
env_allowlist     = []         # credential vars exempt from scrubber
pty               = false      # requires --features terminal-pty

# Optional. Enables tool.browser.* (10 capabilities).
[tool.browser]
backend                  = "none"   # none | headless_chrome | playwright | webdriver
max_sessions             = 16
call_timeout_secs        = 30
webdriver_url            = "http://127.0.0.1:9515"
# screenshot_on_failure_dir = ""    # dir for failure PNGs; read by tool.browser.capture_read

# Optional. Enables tool.mcp.* (3 capabilities).
[tool.mcp]
# [[tool.mcp.servers]]
# id            = "my-server"
# transport     = "stdio"        # stdio | http
# endpoint      = "npx"          # program name for stdio; URL for http
# command       = "npx"          # optional; wins over endpoint when set
# args          = ["-y", "@my/mcp-server"]
# declared_tools = []
# description   = ""

# Optional. Controls tool.parse_document pipeline (LlamaParse → Jina → Firecrawl → local).
[tool.parse_document]
enabled              = true
prefer_cloud         = true
llama_cloud_api_key_env = "LLAMA_CLOUD_API_KEY"
jina_api_key_env     = "JINA_API_KEY"
firecrawl_api_key_env = "FIRECRAWL_API_KEY"
cloud_timeout_secs   = 60

# Optional. Controls tool.web_read pipeline (Jina → Firecrawl → local).
[tool.web_read]
cloud_timeout_secs = 30     # note: different default from parse_document's 60

# Optional. Enables tool.screen (screen capture).
[tool.screen]
enabled      = false
timeout_secs = 15
# temp_dir   = ""    # default: std::env::temp_dir()
```

**SSRF protection** (default on): blocks loopback, RFC 1918,
link-local, CGNAT, documentation ranges, IPv4-mapped IPv6, and
other private ranges. `ssrf_protection = false` logs a WARNING
at startup and disables private-IP blocking for ALL outbound
HTTP (tool capabilities AND cloud tiers). `url_allowlist` uses
glob matching with `*` matching any chars including `.`; cloud
tiers are exempt. `blocked_hosts` is exact-match only (no
subdomain matching by design).

**Terminal security:** `allowed_commands` enforces bare program
names (no path separators); `allowed_dirs` restricts the child
cwd. With `inherit_env = false` (default), the child starts
with an empty environment then gets `PATH` (+ `PATHEXT`,
`SYSTEMROOT` on Windows). With `inherit_env = true`, sensitive
env vars (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`,
`*_KEY`, `DATABASE_URL`, `RELIX_BRIDGE_TOKEN`) are scrubbed
unless listed in `env_allowlist`.

**Output guard:** every tool reply is inspected for prompt-injection
patterns and hard-truncated at 50,000 chars.

**Feature flags for browser/terminal compilation:**
`browser-headless-chrome`, `browser-playwright`,
`browser-webdriver`, `terminal-pty`.

### Coordinator node

```toml
[coordinator]
db_path       = "dev-data/<run>/tasks.db"
max_list      = 200     # ceiling for task.list + event queries
recovery_scan = true    # on startup, flip overdue running tasks to interrupted

[coordinator.retention]
enabled             = false
max_task_age_days   = 30
max_events_per_task = 500
compact_interval_h  = 24
max_passes_per_run  = 10

[coordinator.ai_peer]           # optional; for drift embedding
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 10

# Optional. Opt in to the cron scheduler.
[coordinator.cron]
enabled        = true
tick_secs      = 30
max_concurrent = 3
max_job_secs   = 300

[coordinator.cron.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60

# Optional. Opt in to the delegation executor.
[coordinator.delegation]
enabled             = true
max_depth           = 3
max_concurrent      = 5
executor_poll_secs  = 5
max_job_secs        = 300

[coordinator.delegation.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60
```

`recovery_scan = false` is useful for forensic investigation —
it prevents stale tasks from being auto-transitioned on restart.

`retention.compact_interval_h` controls the background retention
loop interval. Retention only deletes events whose parent task
is in terminal status. Before deletion, a `task.snapshot` event
is emitted per qualifying task.

### Channel nodes

#### Telegram

```toml
[telegram]
token_env                   = "RELIX_TELEGRAM_BOT_TOKEN"
allowed_users               = []      # numeric i64 user_ids; empty = allow all
operator_chat_id            = 0       # 0 = approval notifier disabled
messages_ring_capacity      = 200
flow_template               = ""      # reserved; not validated or wired
session_db_path             = ""      # optional; absent = in-memory
poll_interval_secs          = 1
approval_poll_interval_secs = 15
mode                        = "long_poll"  # long_poll | webhook
# webhook_url               = ""      # required when mode = "webhook"

[telegram.memory_peer]
addr          = "/ip4/127.0.0.1/tcp/19711"
alias         = "memory"
deadline_secs = 10

[telegram.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60

[telegram.coord_peer]
addr          = "/ip4/127.0.0.1/tcp/19714"
alias         = "coordinator"
deadline_secs = 10

# Optional. Routes voice messages to tool.audio.transcribe.
[telegram.audio_peer]
addr          = "/ip4/127.0.0.1/tcp/19713"
alias         = "tool"
deadline_secs = 90
```

#### Discord

```toml
[discord]
token_env              = "RELIX_DISCORD_BOT_TOKEN"
channel_id             = "0000000000"     # snowflake string
allowed_users          = []               # snowflake strings; empty = allow all
operator_user_id       = ""               # reserved
messages_ring_capacity = 200
poll_interval_secs     = 2
# state_db_path        = ""              # optional SQLite for persistent cursor

[discord.memory_peer]
addr          = "/ip4/127.0.0.1/tcp/19711"
alias         = "memory"
deadline_secs = 10

[discord.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60

[discord.coord_peer]
addr          = "/ip4/127.0.0.1/tcp/19714"
alias         = "coordinator"
deadline_secs = 10
```

`state_db_path` enables the `DiscordWatermarkStore` — a SQLite
table that persists the polling cursor across restarts so
messages are not re-processed.

#### Slack

```toml
[slack]
token_env              = "RELIX_SLACK_BOT_TOKEN"
channel_id             = "C000000000"     # C/G/D prefix
allowed_users          = []               # Slack user ids; empty = allow all
operator_user_id       = ""               # reserved
messages_ring_capacity = 200
poll_interval_secs     = 2
# state_db_path        = ""              # optional SQLite for historical filter

[slack.memory_peer]
addr          = "/ip4/127.0.0.1/tcp/19711"
alias         = "memory"
deadline_secs = 10

[slack.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60

[slack.coord_peer]
addr          = "/ip4/127.0.0.1/tcp/19714"
alias         = "coordinator"
deadline_secs = 10
```

`state_db_path` enables `SlackBotStartStore` — records the
first-ever bot start timestamp and filters out messages sent
before it, so replayed history is not re-processed on restart.

#### Email

```toml
[email]
enabled                    = true
smtp_host                  = "smtp.example.com"    # required
smtp_port                  = 587
smtp_username              = ""
smtp_password_env          = ""     # env var name; empty = no password auth
smtp_oauth2_token_env      = ""     # XOAUTH2; wins over smtp_password_env
smtp_from                  = "bot@example.com"     # required
smtp_tls                   = "starttls"    # starttls | tls | implicit | smtps | none | plain
smtp_max_retries           = 3
smtp_pool_max              = 8
dkim_private_key_path      = ""    # PEM (PKCS#1 or PKCS#8); all 3 DKIM fields must be set
dkim_selector              = ""
dkim_domain                = ""
imap_host                  = "imap.example.com"    # required
imap_port                  = 993
imap_username              = ""
imap_password_env          = ""
imap_oauth2_token_env      = ""    # mutually exclusive with imap_password_env
imap_folder                = "INBOX"
imap_processed_folder      = ""    # move-to after dispatch; empty = mark \Seen only
imap_poll_interval_secs    = 60
imap_max_message_bytes     = 10485760   # 10 MiB; oversized messages bounced
oauth2_client_id_env       = ""    # all four OAuth2 fields must be set or all absent
oauth2_client_secret_env   = ""
oauth2_refresh_token_env   = ""
oauth2_token_endpoint      = ""
messages_ring_capacity     = 200
allowed_senders            = []    # case-insensitive addr-spec; empty = allow all
operator_address           = ""    # reserved for approval notifications

[email.memory_peer]
addr          = "/ip4/127.0.0.1/tcp/19711"
alias         = "memory"
deadline_secs = 10

[email.ai_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 60

[email.coord_peer]
addr          = "/ip4/127.0.0.1/tcp/19714"
alias         = "coordinator"
deadline_secs = 10
```

DKIM signing (RSA-SHA256, relaxed/relaxed) is enabled only when
all three of `dkim_private_key_path`, `dkim_selector`, and
`dkim_domain` are set; any missing field disables signing
silently. IMAP uses IDLE push where available; falls back to
polling every `imap_poll_interval_secs`. Maximum outbound
message size: 26 MiB (`MAX_MESSAGE_BYTES`).

Email templates can be customized via
`RELIX_EMAIL_TEMPLATES_DIR` (TOML files `<name>.toml` with
`subject`, `body`, optional `html`). Built-in templates:
`welcome`, `reset_password`, `task_completed`, `task_failed`.

#### Reports (scheduled channel summaries)

```toml
[reports]
enabled  = false
schedule = "0 9 * * *"    # 5-field cron OR duration shorthand (e.g. "30m")
channels = []              # "telegram" | "discord" | "slack"
```

`cost_cents` and `memory_items_added` in the summary report are
always 0 in the current alpha (billing not yet wired). Missed
ticks are NOT replayed.

### Plugin host node

```toml
[plugin_host]
plugin_dir       = "./plugins"
max_plugins      = 20
# registry_db_path = ""      # default: ./plugin-registry.db under RELIX_DATA_DIR
max_memory_mb    = 512     # RLIMIT_AS cap on plugin subprocess (Unix); 0 = do not apply
max_cpu_secs     = 30      # RLIMIT_CPU cap (Unix); 0 = do not apply
```

`max_open_fds = 100` is hardcoded in `SandboxLimits::default()`
and **not configurable via TOML**. On non-Unix platforms, any
non-zero sandbox cap causes `LoadError::SandboxUnenforceable`;
set all three caps to `0` to run plugins on Windows.

Each plugin declares itself in a `plugin.toml` manifest:

```toml
[plugin]
name        = "my-plugin"   # [a-z0-9-], 3..=64 chars
version     = "1.0.0"
description = "Does something"
# author          = ""
# homepage        = ""
# license         = ""
# publisher_key   = ""   # 64-hex Ed25519 pubkey; triggers .sig verification

[[plugin.capabilities.provides]]
method      = "my.capability"   # dotted-identifier; ≥2 segments
description = "Does something useful"
risk_level  = "low"             # low | medium | high

[plugin.runtime]
kind               = "subprocess"         # only accepted value
binary             = "./bin/my-plugin"    # absolute or relative to manifest dir
# args             = []
# protocol         = "relix-plugin-v1"   # only accepted value
invoke_timeout_secs = 30                  # 1..=300
# binary_sha256    = ""                   # 64-hex SHA-256; validated at spawn
```

Bare command names (no path separator) are refused in
`runtime.binary`. If `publisher_key` is set, a companion
`plugin.toml.sig` file (128-hex Ed25519 signature over the
manifest bytes) is required.

Plugin transport is TLS loopback (pinned self-signed cert per
plugin per launch). The loader passes the cert+key to the
subprocess via `RELIX_PLUGIN_TLS_CERT_DER_B64`,
`RELIX_PLUGIN_TLS_KEY_DER_B64`, and a per-launch bearer token
via `RELIX_PLUGIN_BEARER`.

### Web bridge

```toml
[bridge]
listen_addr    = "127.0.0.1:19791"
# token_path   = "~/.relix/bridge-token"         # bridge bearer token file
# secrets_path = "<data_dir>/bridge-secrets.toml" # provider keys + Telegram token file
# memory_db_path = ""    # optional SQLite layered memory store for /v1/memory inspector

[identity]
bundle_path     = "dev-keys/<run>-bridge.aic"
client_key_path = "dev-keys/<run>-bridge.key"

[transport]
peers_path    = "dev-data/<run>/peers.toml"
deadline_secs = 30

[flow]
template_path          = "flows/chat_template.sol"
# tool_template_path   = ""   # SOL tool-fetch template; absent → /chat_with_tool is 404
# streaming_template_path = ""  # SOL/YAML streaming template for true end-to-end streaming

[sse]
chunk_bytes    = 32    # UTF-8-safe SSE chunk size in bytes
chunk_delay_ms = 25    # inter-chunk delay; 0 = immediate flush

[openai_compat]
default_model = "relix"
# [[openai_compat.models]]
# id          = "relix-openrouter"
# description = "Relix mesh — OpenRouter"

# Optional. Enables coordinator task persistence from the bridge.
[coordinator]
alias = "coordinator"

# Auth configuration.
[auth]
multi_tenant_mode          = false      # true: missing tenant binding → HTTP 401
trusted_internal_origins   = ["127.0.0.1", "::1"]
tenant_bindings            = {}         # 8-char bearer prefix → tenant_id
# setup_token              = ""         # guards GET /v1/auth/token; falls back to RELIX_SETUP_TOKEN

# Log stream redaction.
[logging]
redact_stream = true   # mask bearer tokens, API keys, JWTs in GET /v1/logs/stream

# Per-principal rate limiting.
[mesh.rate_limits]
ai_calls_per_min        = 60    # /chat, /chat/stream, /v1/chat/completions, /ws/chat
dashboard_polls_per_min = 120   # GET on tasks/topology/capabilities/streams/routing
task_mutations_per_min  = 30    # POST/PUT/PATCH/DELETE on /v1/tasks/*
ws_max_concurrent       = 5     # max concurrent WebSocket sockets per principal

# Optional OTel export.
[observability.otel]
enabled      = false
# endpoint   = ""    # OTLP/HTTP /v1/traces URL; absent = buffer-only
# service_name = ""
# events     = []    # event types to export e.g. ["model_call", "tool_call"]
```

The bridge emits a WARN at startup when `listen_addr` is not
loopback (production-posture guard). CSRF protection is
Origin-header same-host matching (not CORS). The `?token=`
query parameter is rejected with 400. Provider API keys are
never held by the bridge — they live exclusively in the AI
node's environment.

Setup token bootstrap: `GET /v1/auth/token` requires
`Authorization: Bearer <setup_token>` or
`X-Relix-Setup-Token: <setup_token>`. Returns 403 when not
configured.

Rate limits use a continuous-refill token bucket. `ai_calls_per_min = 0`
refuses all AI calls (misconfiguration guard).

## Metrics, observability, and alerts

These sections live on any node that has metrics enabled (typically
the coordinator):

```toml
[metrics]
enabled                     = false   # must be true to open store + spawn loops
# db_path                   = "<data_dir>/metrics.db"
retention_days              = 30
retention_sweep_interval_secs = 3600
alert_interval_secs         = 60

[metrics.thresholds]
error_rate_pct                  = 10.0
p95_latency_ms                  = 5000
cost_per_hour_micros            = 1_000_000    # $1.00
zero_success_window_mins        = 10
min_invocations_for_rate_alert  = 20
eval_window_mins                = 10
provider_cost_spike_factor      = 3.0
provider_cost_baseline_hours    = 24
provider_cost_recent_hours      = 1
provider_cost_min_baseline_micros = 10_000     # $0.01 noise floor
ask_human_drift_factor          = 3.0
ask_human_baseline_hours        = 24
ask_human_recent_hours          = 1
ask_human_min_attempts          = 10
ask_human_min_recent_rate       = 0.05         # 5%

# Optional per-model price overrides (micro-USD per 1k tokens).
# Built-in defaults: gpt-4o (2500/10000), gpt-4o-mini (150/600),
#   claude-opus-4 (15000/75000), claude-sonnet-4 (3000/15000),
#   claude-haiku-4 (250/1250), gemini-2.5-pro (1250/5000),
#   gemini-2.5-flash (75/300), mock (0/0).
# [metrics.prices]
# "my-model" = { prompt_per_1k_micros = 500, completion_per_1k_micros = 1500 }

[metrics.alerts]
# chronicle_path = "<data_dir>/alerts.sqlite"
# [[metrics.alerts.targets]]
# channel     = "telegram"   # telegram | discord | slack | email
# peer        = "telegram"
# chat_id     = "123456789"

[metrics.cost_alerts]
enabled                   = false      # baseline spike detector; off by default
baseline_window_mins      = 60
tick_interval_secs        = 300        # minimum enforced to 60s
spike_multiplier          = 2.0
drift_threshold           = 0.3        # 30pp absolute
retention_days            = 7
# db_path (derived from metrics.db_path as cost_baselines.db)
absolute_hourly_cap_usd   = 50.0
absolute_daily_cap_usd    = 500.0
absolute_per_request_cap_usd = 5.0

[budget]
throttle_backoff_ms  = 2000
cache_refresh_secs   = 60
exempt_methods       = []

# [[budget.agents]]
# agent              = "my-agent"
# daily_limit_usd    = 10.0
# hourly_limit_usd   = 1.0
# action_on_exceed   = "throttle"   # throttle | reject | alert_only

# [budget.deployment]
# daily_limit_usd    = 100.0
# hourly_limit_usd   = 10.0
# action_on_exceed   = "throttle"

[observability.otel]
enabled       = false
# endpoint    = ""    # OTLP/HTTP /v1/traces URL
service_name  = "relix-runtime"
# events      = []    # per-event-type opt-in

[observability.two_sink]          # two-sink session tracing
enabled              = false
# metadata_db_path   = ""         # required when enabled; Sink A (safe for export)
# content_db_path    = ""         # Sink B (local only; short retention)
# provenance_db_path = ""
content_retention_days = 7
```

**Alert dedup:** `Fired` emitted only on healthy→above threshold
crossing; `Recovered` only on above→healthy. Same condition
across ticks does NOT re-fire.

**Budget enforcer:** NaN/Infinite limits fail-closed with
`Reject`. Cache invalidated immediately on every `cost > 0`
metric (not just on the `cache_refresh_secs` interval).
`alert_only` action fires a `BudgetExceeded` alert and returns
`Allow`.

**Absolute spend caps:** `absolute_per_request_cap_usd` blocks
dispatch before the call if the estimated cost exceeds the cap.
Hourly and daily rolling windows are enforced post-dispatch.

**Two-sink tracing:** Sink A (`metadata_events`) holds
metadata-only rows with long retention; Sink B
(`content_events`) holds prompt/response/tool content with
short retention. OTel export reads only Sink A — Sink B content
never leaves the node.

## Security gating

### Approval delivery

```toml
[approval]
always_require_methods = []         # methods requiring a token regardless of policy
# delivery_db_path     = "<data_dir>/approval_delivery.db"
approval_token_ttl_secs = 300       # clamped to [30, 86400]

[approval.delivery]
default_channel = "dashboard"   # dashboard | telegram | slack | discord | email

# [[approval.delivery.rules]]
# agent_pattern          = "*"
# action_pattern         = "email:send"
# channel                = "telegram"
# escalation_timeout_secs = 0    # 0 = no escalation
# escalation_channel     = ""

# [approval.delivery.channels.telegram]
# enabled  = true
# chat_id  = "123456789"
# peer     = "telegram"

# [approval.delivery.channels.slack]
# enabled         = true
# webhook_url     = ""
# channel_id      = "C000000000"
# signing_secret  = ""
# peer            = "slack"

# [approval.delivery.channels.email]
# enabled  = true
# to       = "ops@example.com"
# from     = "bot@example.com"
# reply_to = ""
# peer     = "email"

# [approval.delivery.channels.dashboard]
# enabled = true
```

Approval tokens are Ed25519-signed (version `0x02`). The signing
key is provided via the `RELIX_APPROVAL_SIGNING_KEY` environment
variable (64 hex chars = 32 bytes). When unset, every
token-bearing admission call is denied. Version `0x01` (HMAC-SHA256)
tokens are refused at parse time.

### Credentials vault

```toml
[credentials]
enabled                    = false
# db_path                  = ""
master_key_env             = "RELIX_CREDENTIAL_KEY"  # default env var for v1 key
rotation_check_interval_secs = 60
argon2_memory_cost         = 65536   # KiB = 64 MB
argon2_time_cost           = 3
argon2_parallelism         = 4

# Key versioning (for key rotation without downtime).
# [credentials.key_versions]
# v1 = "RELIX_CREDENTIAL_KEY"
# v2 = "RELIX_CREDENTIAL_KEY_V2"
```

Encryption: AES-256-GCM with Argon2id KDF. Per-vault 32-byte
salt stored in `vault_metadata`. Format version 2 (Argon2id);
version 1 (SHA-256, no salt) is refused at open and only
reachable via `relix credentials migrate-kdf`. Key versioning:
the active version is the highest-ranked `vN` entry; new
credentials are encrypted under the active version; existing
rows are re-encrypted with `relix credentials rotate-vault-key`.

### Mesh-level PII gate

```toml
[mesh_pii]
enabled         = false
action          = "redact"   # block | redact | log_only
scan_args       = true       # scan inbound request args
scan_responses  = false      # scan outbound response bodies (expensive for SSE)
exempt_methods  = []
# chronicle_path = "<data_dir>/pii_chronicle.db"
```

When `enabled = false` (default), zero scanning overhead — the
dispatch path is byte-for-byte identical to pre-7.28.

### Per-agent execution policies

```toml
# [[execution.agents]]
# agent                  = "my-agent"
# allowed_capabilities   = []   # empty = unrestricted (subject to deny + rate)
# denied_capabilities    = []   # checked first; wins over allow list
# max_calls_per_minute   = 60
# max_cost_cents_per_hour = 500  # NOTE: carried but NOT enforced today; audit only

[execution.gateway]
dry_run       = false    # returns DryRunPreview instead of invoking handler
# db_path     = ""       # transaction store SQLite; absent = in-memory only
blocked_tools = []
# evidence_db_path = ""
```

`max_cost_cents_per_hour` is intentionally not enforced — it is
a forward-compatibility placeholder for the upcoming cost-tracker
integration. Unknown agents (no policy registered) always
`Allow`.

### JIT secret injection

Any `RELIX_<NAME>` environment variable is loaded into the
`SecretStore` at startup. Tool arg templates can use
`{{secret:<name>}}` placeholders (names are lowercase). Values
are never returned via any HTTP endpoint — only names are
visible at `GET /v1/secrets/available`. Secrets are not
hot-reloaded; rotation requires a process restart.

## Training, knowledge, and confidence

```toml
[training]
enabled                   = true
# db_path                 = "<data_dir>/training.sqlite"
retention_days            = 90
retention_sweep_interval_secs = 86400
scorer_enabled            = true
scorer_interval_secs      = 30
scorer_batch_size         = 50
# export_dir              = "<data_dir>/training_exports"
min_quality_score         = 0.7

[training.pii]
enabled  = false
strategy = "redact"   # redact | pseudonymize | allow
# overrides = {}      # per-type: EMAIL = "allow", PHONE = "pseudonymize", etc.

[[knowledge.groups]]
name               = "team-a"
members            = ["agent-1", "agent-2"]
auto_share_layers  = ["observation"]   # observation | model
# min_quality_score = 0.6
# [[knowledge.groups.member_nodes]]
# agent = "agent-3"
# node  = "memory-node-2"

[knowledge]
auto_share_interval_secs    = 60
max_observations_per_agent  = 10000   # null disables eviction cap
auto_share_per_tick_budget  = 200
auto_share_per_agent_limit  = 50

[knowledge.quality_scorer]
enabled              = true
interval_secs        = 60
batch_size           = 50
observation_baseline = 0.75

[knowledge_trust]
allow_unbound_sources = false    # false = fail-closed for unregistered source nodes
# [[knowledge_trust.source_nodes]]
# node   = "remote-memory"
# pubkey = "<64-hex Ed25519 pubkey>"

[confidence]
enabled                 = false
window_size             = 100
p95_latency_baseline_ms = 1500
error_rate_discount     = 0.5

[confidence.weights]
response_length   = 0.20
response_coherence = 0.25
provider_signal   = 0.30
error_rate_history = 0.15
latency_signal    = 0.10

# [[confidence.policies]]
# capability         = "ai.chat"
# low_threshold      = 0.5
# critical_threshold = 0.3
# low_action         = "pass"      # pass | retry | escalate | safe_default | alert | abort
# critical_action    = "pass"

[confidence.self_consistency]
enabled              = false
sample_count         = 3
min_score_to_enable  = 0.70
capability_patterns  = []    # empty = match all capabilities
max_trigger_rate_pct = 50    # rolling 1000-sample trigger-rate cap
disable_duration_secs = 300  # SC disabled for this long after guard trip
sc_hourly_budget_usd = 10.0
per_request_budget_usd = 1.0
```

**Training retention is hard-delete** — rows are permanently
removed, not soft-deleted. `TRAINING_CHANNEL_CAP = 10,000`:
overflow evicts the oldest record. `dropped_count()` is the
only signal.

**Knowledge source-node binding (default fail-closed):** with
`allow_unbound_sources = false`, shares from nodes not in
`knowledge_trust.source_nodes` are rejected even if the Ed25519
signature is valid. Peers that connect via the mesh are
auto-registered at handshake and auto-unregistered at disconnect
without manual `[knowledge_trust]` config.

**Self-consistency (SC):** when `confidence.self_consistency.enabled = true`
and baseline confidence drops below `min_score_to_enable`,
`sample_count` parallel `ai.chat` calls are fanned out, their
core answers embedded, pairwise cosine computed, and the
highest-coherence sample is substituted. Cost guards
(`sc_hourly_budget_usd`, `per_request_budget_usd`,
`max_trigger_rate_pct`) disable SC for `disable_duration_secs`
when exceeded.

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
`node.policy.simulate`, `node.policy.recent_denials`,
`node.policy.tenant_list`, `node.policy.tenant_get`,
`node.audit.tenant_list`, `node.audit.tenant_recent`.

**Memory**

`memory.write_turn`, `memory.recent_for_session`,
`memory.search_turns`, `memory.search`, `memory.embed`,
`memory.embed_all`, `memory.agent_read`, `memory.agent_write`,
`memory.agent_curate`, `memory.curator_status`,
`memory.pii_scan`, `memory.anonymize_preview`,
`memory.bulk_anonymize`.
When `[memory.qdrant]` is configured:
`memory.records_search`, `memory.dialectic`,
`memory.ingest_document`, `memory.ingest_image`,
`memory.context_flush`, `memory.quarantine_list`,
`memory.quarantine_approve`, `memory.quarantine_reject`,
`memory.edit_record`, `memory.freeze_record`,
`memory.unfreeze_record`, `memory.bulk_export`,
`memory.request_model_refresh`.

**AI**

`ai.chat`, `ai.chat.stream`, `ai.embed`,
`ai.perception_extract`, `routing.explain`,
`belief.get`, `belief.reset`,
`judge.recent_verdicts`, `judge.stats`,
`reasoning.status`.

**Tool**

`tool.web_fetch`, `tool.web_get`, `tool.web_search`,
`tool.web_extract`, `tool.web.post`, `tool.web.robots_check`,
`tool.web.blocklist_summary`, `tool.read_file`,
`tool.write_file`, `tool.search_files`, `tool.patch`,
`tool.patch_preview`, `tool.append_file`, `tool.list_dir`,
`tool.fs.tree`, `tool.fs.stat`, `tool.fs.audit_recent`,
`tool.fuzzy_replace`, `tool.binary_sniff`, `tool.pdf`,
`tool.parse_document`, `tool.web_read`, `tool.screen`,
`tool.text.chunk`, `tool.ask_human`,
`tool.terminal.run`, `tool.terminal.spawn`,
`tool.terminal.sessions`, `tool.terminal.tail`,
`tool.terminal.cancel`, `tool.terminal.audit_recent`,
`tool.terminal.shell.open`, `tool.terminal.shell.input`,
`tool.terminal.shell.control`, `tool.terminal.shell.close`,
`tool.browser.open_session`, `tool.browser.close_session`,
`tool.browser.navigate`, `tool.browser.get_text`,
`tool.browser.screenshot`, `tool.browser.list_sessions`,
`tool.browser.click`, `tool.browser.type_text`,
`tool.browser.wait_for_selector`, `tool.browser.capture_read`,
`tool.mcp.list_servers`, `tool.mcp.list_tools`,
`tool.mcp.invoke`, `memory.session_search`.

**Coordinator — task ledger**

`task.create`, `task.update`, `task.event`, `task.get`,
`task.list`, `task.list_cursor`, `task.count`, `task.events`,
`task.recover`, `task.attempts`, `task.retry`, `task.replay`,
`task.export`, `task.compact_events`, `task.edges`,
`task.recent_edges`, `task.note`, `task.mark_investigation`,
`task.pause`, `task.resume`, `task.freeze`, `task.unfreeze`,
`task.lineage`, `task.subtree_metrics`, `task.stuck`,
`task.recent_events`, `task.interruption_check`,
`task.observe_interruption`, `task.record_spawned`,
`task.record_delegated`, `task.record_awaited`,
`task.transition_check`, `task.todo_set`, `task.todo_list`,
`task.todo_update`, `task.session_export`, `task.session_search`.

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
`coord.approval.poll`,
`agent.standing_approval.create`,
`agent.standing_approval.list`,
`agent.standing_approval.revoke`.

**Coordinator — messaging**

`msg.send`, `msg.inbox`, `msg.read`, `msg.thread`, `msg.delete`.

**Metrics + observability**

`metrics.agent_summary`, `metrics.method_breakdown`,
`metrics.timeseries`, `metrics.alerts_active`,
`metrics.cost_report`, `metrics.agents`,
`metrics.cost_baselines`, `metrics.ask_human_baselines`,
`metrics.cost_spike_history`,
`observability.active_alerts`, `observability.alert_history`,
`observability.health_summary`.

**Training**

`training.list_interactions`, `training.get_interaction`,
`training.export`, `training.score_interaction`,
`training.stats`, `training.delete_interaction`,
`training.pii_scan`, `training.anonymize_preview`.

**Knowledge**

`knowledge.share`, `knowledge.list_shared`,
`knowledge.group_broadcast`, `knowledge.groups`,
`knowledge.revoke`, `knowledge.recall`,
`knowledge.accept_shared`, `knowledge.autoshare_stats`.

**Confidence**

`confidence.policy_list`, `confidence.score_history`,
`confidence.reset_history`, `confidence.self_consistency_stats`.

**Execution**

`execution.rollback`, `execution.transaction_get`,
`execution.evidence`.

**PII**

`pii.scan_stats`, `pii.recent_events`.

**Approval delivery**

`approval.delivery_status`, `approval.deliver`,
`approval.record_decision`, `approval.failed_deliveries`,
`approval.list_pending`.

**Credentials**

`credentials.store`, `credentials.get`, `credentials.rotate`,
`credentials.revoke`, `credentials.list`, `credentials.audit`.

**Channels**

`telegram.status`, `telegram.messages_recent`,
`telegram.send`, `telegram.approval_send`, `telegram.health`,
`telegram.webhook_update`,
`discord.status`, `discord.messages_recent`,
`discord.send`, `discord.approval_send`, `discord.health`,
`slack.status`, `slack.messages_recent`,
`slack.send`, `slack.approval_send`, `slack.health`,
`email.status`, `email.messages_recent`,
`email.send`, `email.send_template`, `email.approval_send`.

**Plugin host**

`plugin.list`, `plugin.status`, `plugin.reload`,
`plugin.disable`, plus each registered plugin capability.
Each management cap and plugin cap is also registered under
the peer-prefixed alias (`plugin_host.plugin.list`,
`plugin_host.hello.greet`, …) for `.sflow` wire-method
compatibility.

**Router (role = "router" only)**

`router.heartbeat`, `router.network_summary`,
`router.session_list`, `router.log`.

## Environment variables

Read by boot scripts and controllers.

| Variable | Read by | Purpose |
|---|---|---|
| `RELIX_HOME` | CLI, boot scripts | Override config/data home directory |
| `RELIX_DATA_DIR` | boot scripts, flow runner | Root directory for runtime data |
| `RELIX_SUPPRESS_NO_CONFIG_HINT` | CLI boot | Suppress "no config.toml found" note |
| `RELIX_INSTALL_DIR` | installer | Override install dir |
| `RELIX_VERSION` | installer | Pin a specific release version |
| `RELIX_TELEGRAM` | boot scripts | `=1` enables the Telegram controller |
| `RELIX_TELEGRAM_BOT_TOKEN` | Telegram controller | BotFather token |
| `RELIX_TELEGRAM_OPERATOR_CHAT_ID` | boot scripts | Operator numeric chat id |
| `RELIX_TELEGRAM_ALLOWED_USERS` | boot scripts | Comma-separated numeric user_ids |
| `RELIX_DISCORD` | boot scripts | `=1` enables the Discord controller |
| `RELIX_DISCORD_BOT_TOKEN` | Discord controller | Discord bot token |
| `RELIX_DISCORD_CHANNEL_ID` | boot scripts | Discord channel snowflake |
| `RELIX_DISCORD_OPERATOR_USER_ID` | boot scripts | Operator snowflake |
| `RELIX_DISCORD_ALLOWED_USERS` | boot scripts | Comma-separated snowflake strings |
| `RELIX_SLACK` | boot scripts | `=1` enables the Slack controller |
| `RELIX_SLACK_BOT_TOKEN` | Slack controller | `xoxb-...` bot token |
| `RELIX_SLACK_CHANNEL_ID` | boot scripts | Slack channel id (`C/G/D...`) |
| `RELIX_SLACK_OPERATOR_USER_ID` | boot scripts | Operator Slack user id |
| `RELIX_SLACK_ALLOWED_USERS` | boot scripts | Comma-separated Slack user ids |
| `RELIX_PLUGINS` | boot scripts | `=1` enables the plugin host |
| `RELIX_PLUGIN_DIR` | boot scripts | Plugin host scan directory |
| `RELIX_APPROVAL_SIGNING_KEY` | every controller | 64-hex Ed25519 seed for approval tokens; controllers that issue tokens **must** set this; missing → every token-bearing admission call denied |
| `RELIX_CREDENTIAL_KEY` | credentials module | Default master secret for credential vault (v1 key) |
| `RELIX_BRIDGE_MAILGUN_SIGNING_KEY` | bridge email reply | Mailgun HMAC-SHA256 key for inbound webhook verification |
| `RELIX_SETUP_TOKEN` | web bridge | Fallback setup token when `[auth] setup_token` is unset |
| `RELIX_EMAIL_TEMPLATES_DIR` | email node | Custom email template directory |
| `RELIX_CREDENTIAL_VAULT` | boot scripts | `=1` when vault enabled (set from config) |
| `RELIX_APPROVALS` | boot scripts | `=1` when approvals enabled (set from config) |
| `RELIX_APPROVAL_CHANNEL` | boot scripts | Approval delivery channel (set from config) |
| `OPENAI_API_KEY` | AI node | Set when `[ai] provider = "openai"` |
| `OPENROUTER_API_KEY` | AI node | Set when provider is `openrouter` |
| `XAI_API_KEY` | AI node | Set when provider is `xai` |
| `ANTHROPIC_API_KEY` | AI node | Set when provider is `anthropic` |
| `GEMINI_API_KEY` | AI node | Set when provider is `gemini` |
| `LLAMA_CLOUD_API_KEY` | tool node | LlamaParse cloud tier (name configurable) |
| `JINA_API_KEY` | tool node | Jina cloud tier (name configurable) |
| `FIRECRAWL_API_KEY` | tool node | Firecrawl cloud tier (name configurable) |
| `RELIX_<NAME>` | tool node (JIT secrets) | Any `RELIX_`-prefixed var is injected via `{{secret:<name>}}` in tool args; lowercase after stripping prefix |
| `RUST_LOG` | every controller, bridge | tracing-subscriber log directive |

Provider keys never live anywhere except the AI node's
environment — not in the bridge, not in any channel node, not
in any client.

## Ports

Default TCP ports. Each controller's libp2p port carries
mesh-internal traffic; the bridge port is the only HTTP
listener.

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
GET  /ws/chat                               — ws::ws_chat (WebSocket)
```

### OpenAI shim

```
GET  /v1/models                             — openai::models
POST /v1/chat/completions                   — openai::chat_completions
GET  /v1/info                               — openai::info
GET  /v1/schema                             — schema::schema
```

### Auth + bootstrap

```
GET  /v1/auth/token                         — auth::bootstrap_token (PUBLIC)
GET  /dashboard                             — dashboard::page (PUBLIC)
```

### Validators

```
POST /v1/sol/validate                       — sol_validate::validate
POST /v1/yaml/validate                      — yaml_validate::validate
```

### Secrets

```
GET  /v1/secrets/available                  — secrets_available::available (names only)
```

### Tasks

```
GET    /v1/tasks
GET    /v1/tasks/count
GET    /v1/tasks/cursor
GET    /v1/tasks/:id
GET    /v1/tasks/:id/attempts
GET    /v1/tasks/:id/edges
GET    /v1/tasks/:id/lineage_graph
GET    /v1/tasks/edges/recent
GET    /v1/tasks/events/recent
GET    /v1/tasks/events/stream              — SSE
GET    /v1/tasks/stuck
GET    /v1/tasks/:id/todos
PUT    /v1/tasks/:id/todos
PATCH  /v1/tasks/:id/todos/:todo_id
GET    /v1/tasks/:id/summary
GET    /v1/tasks/:id/events
GET    /v1/tasks/:id/events/stream          — SSE
GET    /v1/tasks/:id/lineage
GET    /v1/tasks/:id/export
GET    /v1/tasks/compact_events
POST   /v1/tasks/recover
POST   /v1/tasks/:id/retry
POST   /v1/tasks/:id/replay
POST   /v1/tasks/:id/cancel
POST   /v1/tasks/:id/note
POST   /v1/tasks/:id/investigation
POST   /v1/tasks/:id/pause
POST   /v1/tasks/:id/resume
POST   /v1/tasks/:id/freeze
POST   /v1/tasks/:id/unfreeze
```

### Capabilities + topology

```
GET  /v1/capabilities
GET  /v1/capabilities/:method
GET  /v1/topology
GET  /v1/topology/events
GET  /v1/streams
GET  /v1/routing
GET  /v1/health
GET  /v1/dispatch/stats
GET  /v1/policy/simulate
GET  /v1/policy/denials
```

### MCP

```
GET  /v1/mcp/servers
GET  /v1/mcp/tools
POST /v1/mcp/invoke
GET  /v1/mcp/audit
```

### Tool audits + diagnostics

```
GET  /v1/fs/audit
GET  /v1/terminal/audit
GET  /v1/tool/blocklist
GET  /v1/browser/sessions
GET  /v1/browser/captures/:filename
```

### Memory

```
GET  /v1/memory/agent
POST /v1/memory/curate
GET  /v1/memory/curator/status
POST /v1/memory/embed
POST /v1/memory/search
POST /v1/memory/embed_all
```

### Channels

```
GET  /v1/telegram/status
GET  /v1/telegram/messages/recent
GET  /v1/discord/status
GET  /v1/discord/messages/recent
GET  /v1/slack/status
GET  /v1/slack/messages/recent
```

### Plugins

```
GET  /v1/plugins
GET  /v1/plugins/:plugin_id
POST /v1/plugins/:plugin_id/reload
POST /v1/plugins/:plugin_id/disable
```

### Cron

```
GET    /v1/cron/jobs
POST   /v1/cron/jobs
GET    /v1/cron/jobs/:job_id
PATCH  /v1/cron/jobs/:job_id
DELETE /v1/cron/jobs/:job_id
POST   /v1/cron/jobs/:job_id/trigger
```

### Delegation

```
POST /v1/delegate/spawn
GET  /v1/delegate/result/:child_task_id
POST /v1/delegate/cancel/:child_task_id
GET  /v1/delegate/list/:parent_task_id
```

### Agents + approvals

```
GET    /v1/agents
POST   /v1/agents
GET    /v1/agents/:agent_id
PATCH  /v1/agents/:agent_id
DELETE /v1/agents/:agent_id
GET    /v1/approvals
POST   /v1/approvals/:approval_id/decide
GET    /v1/agents/:agent_id/standing-approvals
POST   /v1/agents/:agent_id/standing-approvals
DELETE /v1/standing-approvals/:standing_id
```

### Messaging

```
POST   /v1/messages
GET    /v1/messages/inbox/:subject_id
POST   /v1/messages/:message_id/read
GET    /v1/messages/thread/:thread_id
DELETE /v1/messages/:message_id
```

### SOL

```
POST /v1/sol/validate
```

### Config + providers

```
GET    /v1/config
GET    /v1/config/providers
GET    /v1/config/providers/:name
PUT    /v1/config/providers/default
GET    /v1/config/telegram
PUT    /v1/config/telegram
POST   /v1/config/telegram/test
```

### Identity + session tokens

```
POST /v1/identity/tokens
GET  /v1/identity/tokens
POST /v1/identity/tokens/verify
POST /v1/identity/tokens/revoke
POST /v1/identity/research
```

### Intervention audit + dashboard

```
GET  /v1/intervention/recent
GET  /dashboard
```

See the named handler modules under
`crates/relix-web-bridge/src/` for request/response schemas.
The bridge stays translation-only — every route maps to one or
more capability calls into the mesh.
