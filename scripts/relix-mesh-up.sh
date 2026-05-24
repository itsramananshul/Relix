#!/usr/bin/env bash
#
# scripts/relix-mesh-up.sh
#
# POSIX-shell sibling of scripts/relix-mesh-up.ps1. Brings up the local
# Relix mesh and blocks until the operator hits Ctrl-C, at which point
# it kills exactly the PIDs it started (no `pkill -f relix-*`).
#
# Nodes started:
#   memory controller   — SQLite + FTS5 session store
#   ai controller       — provider-agnostic ai.chat / ai.embed
#   tool controller     — file system, web, terminal, browser tools
#   coordinator         — durable Task ledger
#   telegram controller — opt-in via RELIX_TELEGRAM=1
#   discord controller  — opt-in via RELIX_DISCORD=1
#   slack controller    — opt-in via RELIX_SLACK=1
#   plugin-host         — opt-in via RELIX_PLUGINS=1
#   relix-web-bridge    — HTTP + OpenAI shim + dashboard
#
# Tested on bash 5 (Linux) and zsh-as-/bin/sh on macOS.
# Requires: cargo build target/debug/{relix-cli,relix-controller,relix-web-bridge}
# already produced (run `cargo build --workspace` if not).

set -euo pipefail

# ---- CLI parsing ----

PROVIDER="mock"
BASE_URL=""
RUN="local"
BRIDGE_PORT=19791
MEM_PORT=19711
AI_PORT=19712
TOOL_PORT=19713
COORDINATOR_PORT=19714
TELEGRAM_PORT=19715
DISCORD_PORT=19716
SLACK_PORT=19717
PLUGIN_HOST_PORT=19718
TOOL_ALLOW_HTTP=0
NO_TOOL=0
NO_COORDINATOR=0
NO_TELEGRAM=0
NO_DISCORD=0
NO_SLACK=0
NO_PLUGINS=0

usage() {
    cat <<'EOF'
Usage: scripts/relix-mesh-up.sh [options]

Options:
  --provider <name>      AI provider: mock | openai | openrouter | xai |
                         anthropic | gemini | local   (default: mock)
  --base-url <url>       Override provider's default base URL
  --run <name>           Deployment label (prefixes dev-keys / dev-data
                         dirs).                         (default: local)
  --bridge-port <n>      Bridge HTTP port               (default: 19791)
  --mem-port <n>         Memory node libp2p port        (default: 19711)
  --ai-port <n>          AI node libp2p port            (default: 19712)
  --tool-port <n>        Tool node libp2p port          (default: 19713)
  --coordinator-port <n> Coordinator libp2p port        (default: 19714)
  --telegram-port <n>    Telegram libp2p port           (default: 19715)
  --discord-port <n>     Discord libp2p port            (default: 19716)
  --slack-port <n>       Slack libp2p port              (default: 19717)
  --plugin-host-port <n> Plugin host libp2p port        (default: 19718)
  --tool-allow-http      Allow http:// URLs in tool.web_fetch
                         (default: https:// only)
  --no-tool              Skip the tool controller
  --no-coordinator       Skip the coordinator controller
  --no-telegram          Skip telegram even if RELIX_TELEGRAM=1
  --no-discord           Skip discord even if RELIX_DISCORD=1
  --no-slack             Skip slack even if RELIX_SLACK=1
  --no-plugins           Skip plugin host even if RELIX_PLUGINS=1
  -h, --help             Print this message

Environment:
  RELIX_TELEGRAM=1   + RELIX_TELEGRAM_BOT_TOKEN       — boots telegram
  RELIX_DISCORD=1    + RELIX_DISCORD_BOT_TOKEN
                     + RELIX_DISCORD_CHANNEL_ID       — boots discord
  RELIX_SLACK=1      + RELIX_SLACK_BOT_TOKEN
                     + RELIX_SLACK_CHANNEL_ID         — boots slack
  RELIX_PLUGINS=1    + RELIX_PLUGIN_DIR (default ./plugins)
                                                      — boots plugin host
  RELIX_DATA_DIR     overrides the data root          (default dev-data)

Ctrl-C tears the mesh down.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --provider)          PROVIDER="$2"; shift 2 ;;
        --base-url)          BASE_URL="$2"; shift 2 ;;
        --run)               RUN="$2"; shift 2 ;;
        --bridge-port)       BRIDGE_PORT="$2"; shift 2 ;;
        --mem-port)          MEM_PORT="$2"; shift 2 ;;
        --ai-port)           AI_PORT="$2"; shift 2 ;;
        --tool-port)         TOOL_PORT="$2"; shift 2 ;;
        --coordinator-port)  COORDINATOR_PORT="$2"; shift 2 ;;
        --telegram-port)     TELEGRAM_PORT="$2"; shift 2 ;;
        --discord-port)      DISCORD_PORT="$2"; shift 2 ;;
        --slack-port)        SLACK_PORT="$2"; shift 2 ;;
        --plugin-host-port)  PLUGIN_HOST_PORT="$2"; shift 2 ;;
        --tool-allow-http)   TOOL_ALLOW_HTTP=1; shift ;;
        --no-tool)           NO_TOOL=1; shift ;;
        --no-coordinator)    NO_COORDINATOR=1; shift ;;
        --no-telegram)       NO_TELEGRAM=1; shift ;;
        --no-discord)        NO_DISCORD=1; shift ;;
        --no-slack)          NO_SLACK=1; shift ;;
        --no-plugins)        NO_PLUGINS=1; shift ;;
        -h|--help)           usage; exit 0 ;;
        *)                   echo "unknown arg: $1" >&2; usage; exit 1 ;;
    esac
done

case "$PROVIDER" in
    mock|openai|openrouter|xai|anthropic|gemini|local) ;;
    *) echo "unknown provider: $PROVIDER" >&2; exit 1 ;;
esac

# ---- Locate repo root + binaries ----

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_BIN="$SCRIPT_DIR/../bin"

# Resolve a binary by trying a list of candidate paths in order.
# Order:
#
#   1. Same install prefix as the script — `$SCRIPT_DIR/../bin/`.
#      `install.sh` drops the binaries into `~/.local/bin/` and the
#      mesh scripts into `~/.local/scripts/`, so this is the right
#      relative hop on a clean binary install.
#   2. `target/debug/`   relative to the repo root — repo checkout
#      with `cargo build --workspace`.
#   3. `target/release/` relative to the repo root — repo checkout
#      with `cargo build --release --workspace`.
#
# The CLI ships as `relix` from the release archive (the `relix-cli`
# crate is renamed in `release.yml` so the installed command is just
# `relix`) but stays as `relix-cli` under `target/...`. The CLI
# candidate list covers both names.
resolve_bin() {
    local name="$1"; shift
    local cand
    for cand in "$@"; do
        if [[ -x "$cand" ]]; then
            echo "$cand"
            return 0
        fi
    done
    {
        echo "missing binary: $name"
        echo "Searched:"
        for cand in "$@"; do echo "  - $cand"; done
        echo
        echo "Install the release binaries from https://github.com/itsramananshul/Relix/releases"
        echo "or run \`cargo build --workspace\` in a repo checkout."
    } >&2
    exit 1
}

CLI="$(resolve_bin relix-cli \
    "$INSTALL_BIN/relix" \
    "$INSTALL_BIN/relix-cli" \
    "$REPO_ROOT/target/debug/relix-cli" \
    "$REPO_ROOT/target/release/relix-cli")"
CONTROLLER="$(resolve_bin relix-controller \
    "$INSTALL_BIN/relix-controller" \
    "$REPO_ROOT/target/debug/relix-controller" \
    "$REPO_ROOT/target/release/relix-controller")"
BRIDGE="$(resolve_bin relix-web-bridge \
    "$INSTALL_BIN/relix-web-bridge" \
    "$REPO_ROOT/target/debug/relix-web-bridge" \
    "$REPO_ROOT/target/release/relix-web-bridge")"

cd "$REPO_ROOT"

# ---- Channel + plugin opt-in resolution ----

TELEGRAM_ENABLED=0
if [[ "${RELIX_TELEGRAM:-}" == "1" && "$NO_TELEGRAM" -eq 0 ]]; then
    TELEGRAM_ENABLED=1
fi
DISCORD_ENABLED=0
if [[ "${RELIX_DISCORD:-}" == "1" && "$NO_DISCORD" -eq 0 ]]; then
    DISCORD_ENABLED=1
fi
SLACK_ENABLED=0
if [[ "${RELIX_SLACK:-}" == "1" && "$NO_SLACK" -eq 0 ]]; then
    SLACK_ENABLED=1
fi
PLUGINS_ENABLED=0
if [[ "${RELIX_PLUGINS:-}" == "1" && "$NO_PLUGINS" -eq 0 ]]; then
    PLUGINS_ENABLED=1
fi

# ---- Paths ----

DATA_ROOT="${RELIX_DATA_DIR:-dev-data}"
DATA_BASE="$DATA_ROOT/$RUN"
mkdir -p "$DATA_BASE" dev-keys "configs/policies"

ORG_KEY="dev-keys/$RUN-org-root.key"
ORG_PUB="dev-keys/$RUN-org-root.pub"
MEM_KEY="dev-keys/$RUN-memory.key"
AI_KEY="dev-keys/$RUN-ai.key"
TOOL_KEY="dev-keys/$RUN-tool.key"
COORDINATOR_KEY="dev-keys/$RUN-coordinator.key"
TELEGRAM_KEY="dev-keys/$RUN-telegram.key"
DISCORD_KEY="dev-keys/$RUN-discord.key"
SLACK_KEY="dev-keys/$RUN-slack.key"
PLUGIN_HOST_KEY="dev-keys/$RUN-plugin-host.key"
BRIDGE_KEY="dev-keys/$RUN-bridge.key"

BRIDGE_AIC="dev-keys/$RUN-bridge.aic"
MEMORY_AIC="dev-keys/$RUN-memory.bundle"
TELEGRAM_BUNDLE="dev-keys/$RUN-telegram.bundle"
DISCORD_BUNDLE="dev-keys/$RUN-discord.bundle"
SLACK_BUNDLE="dev-keys/$RUN-slack.bundle"
PLUGIN_HOST_BUNDLE="dev-keys/$RUN-plugin-host.bundle"

POLICY="configs/policies/$RUN.toml"
PEERS="$DATA_BASE/peers.toml"
MEM_CONFIG="$DATA_BASE/memory.toml"
AI_CONFIG="$DATA_BASE/ai.toml"
TOOL_CONFIG="$DATA_BASE/tool.toml"
COORDINATOR_CONFIG="$DATA_BASE/coordinator.toml"
TELEGRAM_CONFIG="$DATA_BASE/telegram.toml"
DISCORD_CONFIG="$DATA_BASE/discord.toml"
SLACK_CONFIG="$DATA_BASE/slack.toml"
PLUGIN_HOST_CONFIG="$DATA_BASE/plugin-host.toml"
BRIDGE_CONFIG="$DATA_BASE/bridge.toml"

MEM_LOG="$DATA_BASE/memory.log";       MEM_ERR="$DATA_BASE/memory.err.log"
AI_LOG="$DATA_BASE/ai.log";            AI_ERR="$DATA_BASE/ai.err.log"
TOOL_LOG="$DATA_BASE/tool.log";        TOOL_ERR="$DATA_BASE/tool.err.log"
COORDINATOR_LOG="$DATA_BASE/coordinator.log"
COORDINATOR_ERR="$DATA_BASE/coordinator.err.log"
TELEGRAM_LOG="$DATA_BASE/telegram.log"; TELEGRAM_ERR="$DATA_BASE/telegram.err.log"
DISCORD_LOG="$DATA_BASE/discord.log";   DISCORD_ERR="$DATA_BASE/discord.err.log"
SLACK_LOG="$DATA_BASE/slack.log";       SLACK_ERR="$DATA_BASE/slack.err.log"
PLUGIN_HOST_LOG="$DATA_BASE/plugin-host.log"
PLUGIN_HOST_ERR="$DATA_BASE/plugin-host.err.log"
BRIDGE_LOG="$DATA_BASE/bridge.log";     BRIDGE_ERR="$DATA_BASE/bridge.err.log"

# ---- 1. Identity bundles + org root ----

if [[ ! -f "$ORG_KEY" || ! -f "$ORG_PUB" ]]; then
    echo "minting org root ..."
    "$CLI" identity init-org --root-key "$ORG_KEY" --org "$RUN"
fi
if [[ ! -f "$BRIDGE_AIC" ]]; then
    echo "minting bridge identity ..."
    "$CLI" identity mint --root-key "$ORG_KEY" --name web-bridge \
        --groups chat-users --out "$BRIDGE_AIC"
fi
if [[ ! -f "$MEMORY_AIC" ]]; then
    echo "minting memory identity ..."
    "$CLI" identity mint --root-key "$ORG_KEY" --name memory \
        --groups chat-users --out "$MEMORY_AIC"
fi

# ---- 2. Memory config ----

cat > "$MEM_CONFIG" <<EOF
[controller]
name = "$RUN-memory"
node_type = "memory"
listen_port = $MEM_PORT

[identity]
key_path = "$MEM_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[memory]
db_path = "$DATA_BASE/memory.db"

[memory.embedding_peer]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
alias = "ai"
deadline_secs = 30
model = "mock-embed"
dimensions = 8

[peers]
EOF

# ---- 3. AI config + provider tail ----

cat > "$AI_CONFIG" <<EOF
[controller]
name = "$RUN-ai"
node_type = "ai"
listen_port = $AI_PORT

[identity]
key_path = "$AI_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[ai]
provider = "$PROVIDER"
model    = ""

# Outbound memory wiring. With this block set, the AI node
# dials the memory peer at startup and ai.chat fetches recent
# conversation turns automatically — flows no longer have to
# call memory.recent_for_session manually. See docs/memory.md.
#
# Optional RAG retrieval (off by default) — when enabled the
# AI node embeds the user prompt locally and queries the
# vector memory for semantically related chunks across all
# past sessions, injecting them as a "Relevant context"
# block in the system prompt. To enable: set rag_enabled to
# true and tune rag_top_k / rag_min_score below. See
# docs/memory.md "RAG (Retrieval-Augmented Generation)".
[ai.memory_peer]
addr               = "/ip4/127.0.0.1/tcp/$MEM_PORT"
alias              = "memory"
deadline_secs      = 5
max_history_turns  = 10
rag_enabled        = false           # set true to enable RAG
rag_top_k          = 5
rag_min_score      = 0.70

[peers]
EOF

case "$PROVIDER" in
    openai)
        url="${BASE_URL:-https://api.openai.com/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.openai]
base_url      = "$url"
api_key_env   = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"
EOF
        ;;
    openrouter)
        url="${BASE_URL:-https://openrouter.ai/api/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.openrouter]
base_url      = "$url"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"
EOF
        ;;
    xai)
        url="${BASE_URL:-https://api.x.ai/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.xai]
base_url      = "$url"
api_key_env   = "XAI_API_KEY"
EOF
        ;;
    local)
        url="${BASE_URL:-http://localhost:11434/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.local]
base_url      = "$url"
EOF
        ;;
    anthropic)
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "claude-3-5-sonnet-latest"
EOF
        ;;
    gemini)
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
EOF
        ;;
    mock)
        ;; # no tail
esac

# ---- 4. Tool config ----

if [[ "$NO_TOOL" -eq 0 ]]; then
    allow_http_value="false"
    [[ "$TOOL_ALLOW_HTTP" -eq 1 ]] && allow_http_value="true"
    cat > "$TOOL_CONFIG" <<EOF
[controller]
name = "$RUN-tool"
node_type = "tool"
listen_port = $TOOL_PORT

[identity]
key_path = "$TOOL_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[tool]
max_bytes              = 524288
timeout_secs           = 12
max_redirects          = 5
allow_http             = $allow_http_value
user_agent             = "Relix/0.1 (alpha)"
extract_max_input_bytes = 1048576

[tool.fs]
root                = "$DATA_BASE/tool-jail"
max_read_bytes      = 1048576
max_write_bytes     = 524288
max_search_results  = 256

[tool.pdf]
max_input_bytes  = 5242880
max_pages        = 30
max_output_chars = 65536

[peers]
EOF
    mkdir -p "$DATA_BASE/tool-jail"
fi

# ---- 5. Coordinator config ----

if [[ "$NO_COORDINATOR" -eq 0 ]]; then
    cat > "$COORDINATOR_CONFIG" <<EOF
[controller]
name = "$RUN-coordinator"
node_type = "coordinator"
listen_port = $COORDINATOR_PORT

[identity]
key_path = "$COORDINATOR_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[coordinator]
db_path = "$DATA_BASE/coordinator.db"
max_list = 200

[peers]
EOF
fi

# ---- 6. Telegram config ----

if [[ "$TELEGRAM_ENABLED" -eq 1 ]]; then
    allowed_users_toml="[]"
    if [[ -n "${RELIX_TELEGRAM_ALLOWED_USERS:-}" ]]; then
        allowed_users_toml="[$(echo "$RELIX_TELEGRAM_ALLOWED_USERS" | tr -d ' ')]"
    fi
    op_chat="${RELIX_TELEGRAM_OPERATOR_CHAT_ID:-0}"
    cat > "$TELEGRAM_CONFIG" <<EOF
[controller]
name = "$RUN-telegram"
node_type = "telegram"
listen_port = $TELEGRAM_PORT

[identity]
key_path = "$TELEGRAM_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[telegram]
token_env                    = "RELIX_TELEGRAM_BOT_TOKEN"
allowed_users                = $allowed_users_toml
operator_chat_id             = $op_chat
messages_ring_capacity       = 256
flow_template                = "flows/chat_template.sol"
session_db_path              = "$DATA_BASE/telegram-sessions.db"
poll_interval_secs           = 2
approval_poll_interval_secs  = 5

[telegram.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[telegram.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
deadline_secs = 30

[telegram.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$COORDINATOR_PORT"

[peers]
EOF
fi

# ---- 7. Discord config ----

if [[ "$DISCORD_ENABLED" -eq 1 ]]; then
    allowed_users_toml="[]"
    if [[ -n "${RELIX_DISCORD_ALLOWED_USERS:-}" ]]; then
        quoted=$(echo "$RELIX_DISCORD_ALLOWED_USERS" | tr ',' '\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;/^$/d;s/.*/"&"/' | paste -sd, -)
        allowed_users_toml="[$quoted]"
    fi
    op_user="${RELIX_DISCORD_OPERATOR_USER_ID:-}"
    channel_id="${RELIX_DISCORD_CHANNEL_ID:-0000000000}"
    cat > "$DISCORD_CONFIG" <<EOF
[controller]
name = "$RUN-discord"
node_type = "discord"
listen_port = $DISCORD_PORT

[identity]
key_path = "$DISCORD_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[discord]
token_env              = "RELIX_DISCORD_BOT_TOKEN"
channel_id             = "$channel_id"
allowed_users          = $allowed_users_toml
operator_user_id       = "$op_user"
messages_ring_capacity = 256
poll_interval_secs     = 3

[discord.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[discord.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
deadline_secs = 30

[discord.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$COORDINATOR_PORT"

[peers]
EOF
fi

# ---- 8. Slack config ----

if [[ "$SLACK_ENABLED" -eq 1 ]]; then
    allowed_users_toml="[]"
    if [[ -n "${RELIX_SLACK_ALLOWED_USERS:-}" ]]; then
        quoted=$(echo "$RELIX_SLACK_ALLOWED_USERS" | tr ',' '\n' | sed 's/^[[:space:]]*//;s/[[:space:]]*$//;/^$/d;s/.*/"&"/' | paste -sd, -)
        allowed_users_toml="[$quoted]"
    fi
    op_user="${RELIX_SLACK_OPERATOR_USER_ID:-}"
    channel_id="${RELIX_SLACK_CHANNEL_ID:-C000000000}"
    cat > "$SLACK_CONFIG" <<EOF
[controller]
name = "$RUN-slack"
node_type = "slack"
listen_port = $SLACK_PORT

[identity]
key_path = "$SLACK_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[slack]
token_env              = "RELIX_SLACK_BOT_TOKEN"
channel_id             = "$channel_id"
allowed_users          = $allowed_users_toml
operator_user_id       = "$op_user"
messages_ring_capacity = 256
poll_interval_secs     = 3

[slack.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[slack.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
deadline_secs = 30

[slack.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$COORDINATOR_PORT"

[peers]
EOF
fi

# ---- 9. Plugin host config ----

if [[ "$PLUGINS_ENABLED" -eq 1 ]]; then
    plugin_dir="${RELIX_PLUGIN_DIR:-./plugins}"
    cat > "$PLUGIN_HOST_CONFIG" <<EOF
[controller]
name = "$RUN-plugin-host"
node_type = "plugin_host"
listen_port = $PLUGIN_HOST_PORT

[identity]
key_path = "$PLUGIN_HOST_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[plugin_host]
plugin_dir       = "$plugin_dir"
max_plugins      = 20
registry_db_path = "$DATA_BASE/plugin-registry.db"

[peers]
EOF
fi

# ---- 10. Policy ----

cat > "$POLICY" <<'EOF'
[admit]
groups = ["chat-users"]

[[rules]]
name = "node_health"
method = "node.health"
allow_groups = ["chat-users"]

[[rules]]
name = "node_manifest"
method = "node.manifest"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_read"
method = "memory.agent_read"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_write"
method = "memory.agent_write"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_curate"
method = "memory.agent_curate"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_curator_status"
method = "memory.curator_status"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_search_turns"
method = "memory.search_turns"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_embed"
method = "memory.embed"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_embed_all"
method = "memory.embed_all"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_chat"
method = "ai.chat"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_embed"
method = "ai.embed"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_web_fetch"
method = "tool.web_fetch"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_web_extract"
method = "tool.web_extract"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_read_file"
method = "tool.read_file"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_write_file"
method = "tool.write_file"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_search_files"
method = "tool.search_files"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_patch"
method = "tool.patch"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_pdf"
method = "tool.pdf"
allow_groups = ["chat-users"]

[[rules]]
name = "task_create"
method = "task.create"
allow_groups = ["chat-users"]

[[rules]]
name = "task_update"
method = "task.update"
allow_groups = ["chat-users"]

[[rules]]
name = "task_event"
method = "task.event"
allow_groups = ["chat-users"]

[[rules]]
name = "task_get"
method = "task.get"
allow_groups = ["chat-users"]

[[rules]]
name = "task_list"
method = "task.list"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_create"
method = "cron.create"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_list"
method = "cron.list"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_get"
method = "cron.get"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_update"
method = "cron.update"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_delete"
method = "cron.delete"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_trigger"
method = "cron.trigger"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_spawn"
method = "delegate.spawn"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_result"
method = "delegate.result"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_cancel"
method = "delegate.cancel"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_list"
method = "delegate.list"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_send"
method = "msg.send"
allow_groups = ["chat-users"]

[[rules]]
name = "telegram_status"
method = "telegram.status"
allow_groups = ["chat-users"]

[[rules]]
name = "telegram_messages_recent"
method = "telegram.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "discord_status"
method = "discord.status"
allow_groups = ["chat-users"]

[[rules]]
name = "discord_messages_recent"
method = "discord.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "slack_status"
method = "slack.status"
allow_groups = ["chat-users"]

[[rules]]
name = "slack_messages_recent"
method = "slack.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_list"
method = "plugin.list"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_status"
method = "plugin.status"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_reload"
method = "plugin.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_disable"
method = "plugin.disable"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_list"
method = "plugin_host.plugin.list"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_status"
method = "plugin_host.plugin.status"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_reload"
method = "plugin_host.plugin.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_disable"
method = "plugin_host.plugin.disable"
allow_groups = ["chat-users"]

[[rules]]
name = "hello_greet"
method = "hello.greet"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_hello_greet"
method = "plugin_host.hello.greet"
allow_groups = ["chat-users"]

[[rules]]
name = "web_lookup_fetch"
method = "web_lookup.fetch"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_web_lookup_fetch"
method = "plugin_host.web_lookup.fetch"
allow_groups = ["chat-users"]
EOF

# ---- 11. peers.toml ----

{
    echo "[peers.memory]"
    echo "addr = \"/ip4/127.0.0.1/tcp/$MEM_PORT\""
    echo
    echo "[peers.ai]"
    echo "addr = \"/ip4/127.0.0.1/tcp/$AI_PORT\""
    if [[ "$NO_TOOL" -eq 0 ]]; then
        echo
        echo "[peers.tool]"
        echo "addr = \"/ip4/127.0.0.1/tcp/$TOOL_PORT\""
    fi
    if [[ "$NO_COORDINATOR" -eq 0 ]]; then
        echo
        echo "[peers.coordinator]"
        echo "addr = \"/ip4/127.0.0.1/tcp/$COORDINATOR_PORT\""
    fi
    if [[ "$PLUGINS_ENABLED" -eq 1 ]]; then
        echo
        echo "[peers.plugin_host]"
        echo "addr = \"/ip4/127.0.0.1/tcp/$PLUGIN_HOST_PORT\""
    fi
} > "$PEERS"

# ---- 12. Bridge config ----

flow_lines="template_path     = \"flows/chat.sol\""
if [[ "$NO_TOOL" -eq 0 ]]; then
    flow_lines+=$'\n'"tool_template_path = \"flows/chat_with_tool.sol\""
fi

coord_block=""
if [[ "$NO_COORDINATOR" -eq 0 ]]; then
    coord_block=$'\n\n[coordinator]\nalias = "coordinator"'
fi

cat > "$BRIDGE_CONFIG" <<EOF
[bridge]
listen_addr = "127.0.0.1:$BRIDGE_PORT"

[identity]
bundle_path     = "$BRIDGE_AIC"
client_key_path = "$BRIDGE_KEY"

[transport]
peers_path    = "$PEERS"
deadline_secs = 30

[flow]
$flow_lines

[sse]
chunk_bytes   = 96
chunk_delay_ms = 30

[openai_compat]
default_model = "relix-$PROVIDER"

[[openai_compat.models]]
id          = "relix-$PROVIDER"
description = "Relix mesh route — AI node currently set to $PROVIDER"$coord_block
EOF

# ---- 13. Process management ----

PIDS=()

cleanup() {
    set +e
    echo
    echo "stopping mesh ..."
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null && echo "  stopped pid=$pid"
        fi
    done
    sleep 0.3
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null
        fi
    done
    echo "mesh down."
}
trap cleanup EXIT INT TERM

wait_for_log() {
    local label="$1"
    local logpath="$2"
    local needle="$3"
    local timeout="${4:-30}"
    local elapsed=0
    while [[ "$elapsed" -lt "$timeout" ]]; do
        if [[ -f "$logpath" ]] && grep -q -- "$needle" "$logpath" 2>/dev/null; then
            echo "  $label ready"
            return 0
        fi
        sleep 0.5
        elapsed=$((elapsed + 1))
    done
    echo "  $label did not report ready within ${timeout}s. tail:"
    [[ -f "$logpath" ]] && tail -n 30 "$logpath" | sed 's/^/    /'
    return 1
}

start_node() {
    local label="$1"
    local exe="$2"
    local cfg="$3"
    local log="$4"
    local err="$5"
    local rust_log="${6:-relix_runtime=info}"
    echo "starting $label controller ..."
    : > "$log"
    : > "$err"
    RUST_LOG="$rust_log" "$exe" --config "$cfg" >>"$log" 2>>"$err" &
    PIDS+=($!)
}

# ---- 14. Start controllers ----

start_node "memory" "$CONTROLLER" "$MEM_CONFIG" "$MEM_LOG" "$MEM_ERR"
start_node "ai"     "$CONTROLLER" "$AI_CONFIG"  "$AI_LOG"  "$AI_ERR"

if [[ "$NO_TOOL" -eq 0 ]]; then
    start_node "tool" "$CONTROLLER" "$TOOL_CONFIG" "$TOOL_LOG" "$TOOL_ERR"
fi
if [[ "$NO_COORDINATOR" -eq 0 ]]; then
    start_node "coordinator" "$CONTROLLER" "$COORDINATOR_CONFIG" \
        "$COORDINATOR_LOG" "$COORDINATOR_ERR"
fi

if [[ "$TELEGRAM_ENABLED" -eq 1 ]]; then
    if [[ ! -f "$TELEGRAM_BUNDLE" ]]; then
        "$CLI" identity mint --root-key "$ORG_KEY" --name telegram \
            --groups chat-users --out "$TELEGRAM_BUNDLE"
    fi
    start_node "telegram" "$CONTROLLER" "$TELEGRAM_CONFIG" \
        "$TELEGRAM_LOG" "$TELEGRAM_ERR" "relix_runtime=info,relix_telegram=info"
fi
if [[ "$DISCORD_ENABLED" -eq 1 ]]; then
    if [[ ! -f "$DISCORD_BUNDLE" ]]; then
        "$CLI" identity mint --root-key "$ORG_KEY" --name discord \
            --groups chat-users --out "$DISCORD_BUNDLE"
    fi
    start_node "discord" "$CONTROLLER" "$DISCORD_CONFIG" \
        "$DISCORD_LOG" "$DISCORD_ERR" "relix_runtime=info,relix_discord=info"
fi
if [[ "$SLACK_ENABLED" -eq 1 ]]; then
    if [[ ! -f "$SLACK_BUNDLE" ]]; then
        "$CLI" identity mint --root-key "$ORG_KEY" --name slack \
            --groups chat-users --out "$SLACK_BUNDLE"
    fi
    start_node "slack" "$CONTROLLER" "$SLACK_CONFIG" \
        "$SLACK_LOG" "$SLACK_ERR" "relix_runtime=info,relix_slack=info"
fi
if [[ "$PLUGINS_ENABLED" -eq 1 ]]; then
    if [[ ! -f "$PLUGIN_HOST_BUNDLE" ]]; then
        "$CLI" identity mint --root-key "$ORG_KEY" --name plugin-host \
            --groups chat-users --out "$PLUGIN_HOST_BUNDLE"
    fi
    start_node "plugin-host" "$CONTROLLER" "$PLUGIN_HOST_CONFIG" \
        "$PLUGIN_HOST_LOG" "$PLUGIN_HOST_ERR"
fi

# ---- 15. Wait for controllers ----

wait_for_log "memory"      "$MEM_LOG" "transport listening"
wait_for_log "ai"          "$AI_LOG"  "transport listening"
[[ "$NO_TOOL"        -eq 0 ]] && wait_for_log "tool"        "$TOOL_LOG"        "transport listening"
[[ "$NO_COORDINATOR" -eq 0 ]] && wait_for_log "coordinator" "$COORDINATOR_LOG" "transport listening"
[[ "$TELEGRAM_ENABLED" -eq 1 ]] && wait_for_log "telegram" "$TELEGRAM_LOG" "transport listening"
[[ "$DISCORD_ENABLED"  -eq 1 ]] && wait_for_log "discord"  "$DISCORD_LOG"  "transport listening"
[[ "$SLACK_ENABLED"    -eq 1 ]] && wait_for_log "slack"    "$SLACK_LOG"    "transport listening"
[[ "$PLUGINS_ENABLED"  -eq 1 ]] && wait_for_log "plugin-host" "$PLUGIN_HOST_LOG" "transport listening"

# ---- 16. Start the bridge ----

echo "starting web bridge ..."
: > "$BRIDGE_LOG"; : > "$BRIDGE_ERR"
RUST_LOG="relix_web_bridge=info,relix_runtime=info" \
    "$BRIDGE" --config "$BRIDGE_CONFIG" >>"$BRIDGE_LOG" 2>>"$BRIDGE_ERR" &
PIDS+=($!)

# Bridge health = HTTP 200 on /health
elapsed=0
until curl -fsS "http://127.0.0.1:$BRIDGE_PORT/health" >/dev/null 2>&1; do
    sleep 0.5
    elapsed=$((elapsed + 1))
    if [[ "$elapsed" -ge 60 ]]; then
        echo "  bridge did not become healthy within 30s. tail:"
        tail -n 30 "$BRIDGE_LOG" | sed 's/^/    /'
        exit 1
    fi
done
echo "  bridge ready"
echo
echo "BRIDGE_UP"
echo
echo "Bridge:    http://127.0.0.1:$BRIDGE_PORT/dashboard"
echo "Health:    http://127.0.0.1:$BRIDGE_PORT/health"
echo "Provider:  $PROVIDER"
echo
echo "Logs:     $DATA_BASE/*.log"
echo "PIDs:     ${PIDS[*]}"
echo
echo "Ctrl-C to stop."

# ---- 17. Block until interrupted or a child dies ----

while true; do
    for pid in "${PIDS[@]}"; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "child pid=$pid exited; shutting down."
            exit 1
        fi
    done
    sleep 1
done
