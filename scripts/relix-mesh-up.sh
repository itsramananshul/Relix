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

# Locate the `flows/` directory the bridge + telegram controller read
# their SOL/sflow templates from. Same probe order as the binaries:
#
#   1. `$SCRIPT_DIR/../flows/` — install layout
#      (`~/.local/scripts/` next to `~/.local/flows/`).
#   2. `$REPO_ROOT/flows/`     — repo checkout.
resolve_dir() {
    local name="$1"; shift
    local cand
    for cand in "$@"; do
        if [[ -d "$cand" ]]; then
            local abs
            abs="$(cd "$cand" && pwd)"
            # The generated config paths are consumed by native binaries. On
            # Windows (Git Bash/MSYS) `pwd` yields an MSYS path like /d/... that
            # the native .exe cannot resolve (os error 3). Convert to a mixed
            # Windows path (D:/...) which is both Windows-resolvable and safe in
            # TOML. cygpath is absent on Linux/macOS, where pwd is already native.
            if command -v cygpath >/dev/null 2>&1; then
                cygpath -m "$abs"
            else
                printf '%s\n' "$abs"
            fi
            return 0
        fi
    done
    {
        echo "missing directory: $name"
        echo "Searched:"
        for cand in "$@"; do echo "  - $cand"; done
        echo
        echo "Install with install.sh (which bundles the flow templates) or run from a repo checkout."
    } >&2
    exit 1
}

FLOWS_DIR="$(resolve_dir flows \
    "$SCRIPT_DIR/../flows" \
    "$REPO_ROOT/flows")"

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
# Workflows directory the coordinator reads for `workflow.list` (defaults
# to <coordinator-db-dir>/workflows). Create it empty so the dashboard's
# Workflows panel lists zero workflows (200) instead of erroring that the
# directory does not exist.
mkdir -p "$DATA_BASE/workflows"

# Pidfile recording every process THIS run started. An out-of-band
# shutdown (scripts/relix-mesh-down.sh or `relix stop`) reads it and
# signals exactly these PIDs, never a name-based sweep, so an unrelated
# mesh on the same box survives. Written once the mesh is up; removed by
# cleanup() on exit. Lives under DATA_BASE so a per-run label isolates it.
PID_FILE="$DATA_BASE/mesh.pids"

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
# The AI node is itself a mesh client of the memory peer (`[ai.memory_peer]`)
# for automatic history injection. Its dispatcher loads the identity bundle
# next to its signing key (`<ai-key>.bundle`), so it must be minted too —
# without it the AI node logs "identity bundle missing; memory injection
# disabled" and silently drops auto-history fetch.
AI_AIC="dev-keys/$RUN-ai.bundle"
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

# Days before a bundle's not_after at which boot (and the renewal loop)
# re-mints it. Matches relix-core DEFAULT_RENEWAL_WINDOW_SECS (30 days).
RENEWAL_WINDOW_DAYS="${RELIX_IDENTITY_RENEWAL_WINDOW_DAYS:-30}"

# The org root is the trust anchor: mint once, never re-mint. Re-minting it
# would change org_id and invalidate every leaf bundle signed under it.
if [[ ! -f "$ORG_KEY" || ! -f "$ORG_PUB" ]]; then
    echo "minting org root ..."
    "$CLI" identity init-org --root-key "$ORG_KEY" --org "$RUN"
fi

# Leaf identities are SELF-HEALING. `identity ensure` (re)mints a bundle
# when it is missing, expired, signed by a stale/foreign org root, or
# within RENEWAL_WINDOW_DAYS of expiry — otherwise it is a cheap no-op.
# This is why a fresh install always boots (no pre-minted, already-expired
# bundle can wedge it) and why a long-running mesh never lapses. Locally
# minted bundles get the 365-day relix-core default lifetime.
ensure_identity() {  # name  out  [groups]
    local _name="$1" _out="$2" _groups="${3:-chat-users}"
    "$CLI" identity ensure --root-key "$ORG_KEY" --name "$_name" \
        --groups "$_groups" --renewal-window-days "$RENEWAL_WINDOW_DAYS" \
        --out "$_out"
}
ensure_identity web-bridge "$BRIDGE_AIC"
ensure_identity memory     "$MEMORY_AIC"
ensure_identity ai         "$AI_AIC"

# Bundles the periodic renewal loop re-checks while the mesh runs
# ("name|path" entries). Channel/plugin bundles append themselves below.
RENEWABLE_BUNDLES=("web-bridge|$BRIDGE_AIC" "memory|$MEMORY_AIC" "ai|$AI_AIC")

# Capture the web-bridge identity's verified subject id and hand it to
# the coordinator so it can provision the operator-console agent
# profile at startup. Without a profile the fail-closed agent gate
# denies the dashboard's Tasks/Workflows calls (agent_no_profile). The
# coordinator only seeds when this env var is set.
BRIDGE_SUBJECT="$("$CLI" identity inspect --bundle "$BRIDGE_AIC" --root-key "$ORG_KEY" 2>/dev/null \
    | awk '/^subject-id:/ {print $2; exit}')" || true
if [[ -n "$BRIDGE_SUBJECT" ]]; then
    export RELIX_OPERATOR_CONSOLE_SUBJECT="$BRIDGE_SUBJECT"
    echo "operator-console subject: $BRIDGE_SUBJECT"
else
    echo "warning: could not resolve web-bridge subject id; Tasks/Workflows may be agent-gated" >&2
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
        # RELIX_AI_MODEL (set from config by `relix boot`, or exported by
        # hand) picks the model; otherwise the provider default stands.
        model="${RELIX_AI_MODEL:-gpt-4o-mini}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.openai]
base_url      = "$url"
api_key_env   = "OPENAI_API_KEY"
default_model = "$model"
EOF
        ;;
    openrouter)
        url="${BASE_URL:-https://openrouter.ai/api/v1}"
        # Default to a $0 free model so chat works out of the box without
        # burning credits; RELIX_AI_MODEL overrides it. See RELA-45.
        model="${RELIX_AI_MODEL:-openai/gpt-oss-120b:free}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.openrouter]
base_url      = "$url"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "$model"
EOF
        ;;
    xai)
        url="${BASE_URL:-https://api.x.ai/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.xai]
base_url      = "$url"
api_key_env   = "XAI_API_KEY"
EOF
        [ -n "${RELIX_AI_MODEL:-}" ] && printf 'default_model = "%s"\n' "$RELIX_AI_MODEL" >> "$AI_CONFIG"
        ;;
    local)
        url="${BASE_URL:-http://localhost:11434/v1}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.local]
base_url      = "$url"
EOF
        [ -n "${RELIX_AI_MODEL:-}" ] && printf 'default_model = "%s"\n' "$RELIX_AI_MODEL" >> "$AI_CONFIG"
        ;;
    anthropic)
        model="${RELIX_AI_MODEL:-claude-3-5-sonnet-latest}"
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "$model"
EOF
        ;;
    gemini)
        cat >> "$AI_CONFIG" <<EOF

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
EOF
        [ -n "${RELIX_AI_MODEL:-}" ] && printf 'default_model = "%s"\n' "$RELIX_AI_MODEL" >> "$AI_CONFIG"
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

# Opt-in subsystem sections (enabled via `relix setup` → forwarded by
# `relix boot`, or set the RELIX_* env vars directly). Emitting these
# registers the credential-vault / approval-delivery caps on the
# coordinator so the dashboard's Credentials and Approval panels return
# real data instead of "unavailable".
#
# Credential vault: needs the master key in RELIX_CREDENTIAL_KEY (the
# vault's default master_key_env); the coordinator process inherits it
# from this script's environment. Without a key the vault stays off —
# never a hardcoded default.
cred_block=""
if [[ "${RELIX_CREDENTIAL_VAULT:-0}" == "1" && -n "${RELIX_CREDENTIAL_KEY:-}" ]]; then
    cred_block=$'\n[credentials]\nenabled = true\n'
fi
# Approval delivery: the default channel is the in-process dashboard
# (no external secret). Emitting [approval] + [approval.delivery]
# registers approval.list_pending / approval.failed_deliveries.
approval_block=""
if [[ "${RELIX_APPROVALS:-0}" == "1" ]]; then
    approval_block=$'\n[approval]\n\n[approval.delivery]\ndefault_channel = "'"${RELIX_APPROVAL_CHANNEL:-dashboard}"$'"\n'
fi

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
$cred_block$approval_block
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
flow_template                = "$FLOWS_DIR/chat_template.sol"
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

# Operator-console read surfaces. The dashboard's Dispatch-stats and
# Multi-tenant panels call these node.* operator methods; without an
# allow rule the engine default-denies them (kind=6 deny:default_deny).
[[rules]]
name = "node_dispatch_stats"
method = "node.dispatch.stats"
allow_groups = ["chat-users"]

[[rules]]
name = "node_policy_tenant_list"
method = "node.policy.tenant_list"
allow_groups = ["chat-users"]

[[rules]]
name = "node_policy_tenant_get"
method = "node.policy.tenant_get"
allow_groups = ["chat-users"]

[[rules]]
name = "node_audit_tenant_list"
method = "node.audit.tenant_list"
allow_groups = ["chat-users"]

[[rules]]
name = "node_audit_tenant_recent"
method = "node.audit.tenant_recent"
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
name = "agent_create"
method = "agent.create"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_get"
method = "agent.get"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_list"
method = "agent.list"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_update"
method = "agent.update"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_delete"
method = "agent.delete"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_effective_capabilities"
method = "agent.effective_capabilities"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_assign_check"
method = "agent.assign_check"
allow_groups = ["chat-users"]

[[rules]]
name = "coord_approval_pending"
method = "coord.approval.pending"
allow_groups = ["chat-users"]

[[rules]]
name = "coord_approval_decide"
method = "coord.approval.decide"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_create"
method = "agent.standing_approval.create"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_list"
method = "agent.standing_approval.list"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_revoke"
method = "agent.standing_approval.revoke"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_send"
method = "msg.send"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_inbox"
method = "msg.inbox"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_read"
method = "msg.read"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_thread"
method = "msg.thread"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_delete"
method = "msg.delete"
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

# Operator-console read surfaces (dashboard panels). Allowing a method
# that is not registered on any running node is harmless — the responder
# still returns unknown_method, which the bridge renders as an empty
# panel. These rules ensure that when the subsystem IS enabled the
# operator console is not default-denied.
[[rules]]
name = "workflow_list"
method = "workflow.list"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_run"
method = "workflow.run"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_status"
method = "workflow.status"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_validate"
method = "workflow.validate"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_reload"
method = "workflow.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_report"
method = "metrics.cost_report"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_alerts_active"
method = "metrics.alerts_active"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_agents"
method = "metrics.agents"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_agent_summary"
method = "metrics.agent_summary"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_method_breakdown"
method = "metrics.method_breakdown"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_timeseries"
method = "metrics.timeseries"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_baselines"
method = "metrics.cost_baselines"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_spike_history"
method = "metrics.cost_spike_history"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_health_summary"
method = "observability.health_summary"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_active_alerts"
method = "observability.active_alerts"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_alert_history"
method = "observability.alert_history"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_search"
method = "memory.skill_search"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_stats"
method = "memory.skill_stats"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_get"
method = "memory.skill_get"
allow_groups = ["chat-users"]

[[rules]]
name = "reasoning_status"
method = "reasoning.status"
allow_groups = ["chat-users"]

[[rules]]
name = "judge_recent_verdicts"
method = "judge.recent_verdicts"
allow_groups = ["chat-users"]

[[rules]]
name = "judge_stats"
method = "judge.stats"
allow_groups = ["chat-users"]

[[rules]]
name = "budget_status"
method = "budget.status"
allow_groups = ["chat-users"]

[[rules]]
name = "planning_get_approval"
method = "planning.get_approval"
allow_groups = ["chat-users"]

[[rules]]
name = "planning_find_agents"
method = "planning.find_agents"
allow_groups = ["chat-users"]

[[rules]]
name = "credentials_list"
method = "credentials.list"
allow_groups = ["chat-users"]

[[rules]]
name = "credentials_audit"
method = "credentials.audit"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_list_pending"
method = "approval.list_pending"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_failed_deliveries"
method = "approval.failed_deliveries"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_delivery_status"
method = "approval.delivery_status"
allow_groups = ["chat-users"]

# Operator dashboard (/dashboard): product-spine Brief board / Mandate /
# roster capabilities the web bridge calls on the coordinator. Per-agent
# Key gates still apply inside each capability; this only lifts the mesh
# default-deny so the operator console reaches the spine.
[[rules]]
name = "spine_guild_counts"
method = "guild.counts"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_guild_get"
method = "guild.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_guild_spend"
method = "guild.spend"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_board_summary"
method = "brief.board_summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_board"
method = "brief.board"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unassigned"
method = "brief.unassigned"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unblocked"
method = "brief.unblocked"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_blocked_list"
method = "brief.blocked_list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_stale_list"
method = "brief.stale_list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_overdue"
method = "brief.overdue"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_detail"
method = "brief.detail"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_cost_rollup"
method = "brief.cost_rollup"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_search"
method = "brief.search"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_by_label"
method = "brief.by_label"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_desk"
method = "brief.desk"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_claim_holder"
method = "brief.claim_holder"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_wakeups"
method = "brief.wakeups"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_create"
method = "brief.create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_move"
method = "brief.move"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set"
method = "brief.set"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set_due"
method = "brief.set_due"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_pin"
method = "brief.pin"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_comment"
method = "brief.comment"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_open"
method = "brief.interaction_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_confirm_open"
method = "brief.plan_confirm_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interactions"
method = "brief.interactions"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_respond"
method = "brief.interaction_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_cancel"
method = "brief.interaction_cancel"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_create"
method = "brief.interaction_create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_suggest_open"
method = "brief.suggest_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_suggest_respond"
method = "brief.suggest_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_package_open"
method = "brief.plan_package_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_confirm_respond"
method = "brief.plan_confirm_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_snag"
method = "brief.snag"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unsnag"
method = "brief.unsnag"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_subbrief"
method = "brief.subbrief"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_add"
method = "brief.dossier_add"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_author"
method = "brief.dossier_author"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_latest"
method = "brief.dossier_latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_lock"
method = "brief.dossier_lock"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_unlock"
method = "brief.dossier_unlock"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_locks"
method = "brief.dossier_locks"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set_snags"
method = "brief.set_snags"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_clearance_request"
method = "brief.clearance_request"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_bridge_back_authorize"
method = "bridge_back.authorize"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_roster_summary"
method = "agent.roster_summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_recent_events"
method = "task.recent_events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_stuck"
method = "task.stuck"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_recent_edges"
method = "task.recent_edges"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_events"
method = "task.events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_search"
method = "mandate.search"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_tree"
method = "mandate.tree"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_briefs"
method = "mandate.briefs"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_list"
method = "mandate.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_create"
method = "mandate.create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestrate"
method = "mandate.orchestrate"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestration_latest"
method = "mandate.orchestration.latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestration_list"
method = "mandate.orchestration.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_propose"
method = "prime.propose"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_approve"
method = "prime.approve"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_start"
method = "prime.start"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_proposals"
method = "prime.proposals"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_proposal"
method = "prime.proposal"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_status"
method = "prime.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_next_step"
method = "prime.next_step"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_advance"
method = "prime.advance"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_plan"
method = "mandate.team_plan"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_plan_latest"
method = "mandate.team_plan.latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_readiness"
method = "mandate.team_readiness"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_status"
method = "mandate.strategy.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_propose"
method = "mandate.strategy.propose"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_approve"
method = "mandate.strategy.approve"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_reject"
method = "mandate.strategy.reject"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_rig_list"
method = "rig.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_rig_describe"
method = "rig.describe"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_wakeup"
method = "brief.wakeup"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_run"
method = "brief.run"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_runs"
method = "brief.runs"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_status"
method = "company.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_actions"
method = "company.actions"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_bootstrap_founder"
method = "company.bootstrap_founder"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_starter_crew"
method = "company.starter_crew"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_operatives"
method = "agent.operatives"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_keys"
method = "agent.keys"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_allowance_committed"
method = "agent.allowance_committed"
allow_groups = ["chat-users"]
[[rules]]
name = "agent_approve_hire"
method = "agent.approve_hire"
allow_groups = ["chat-users"]
[[rules]]
name = "agent_reject_hire"
method = "agent.reject_hire"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_workspace_config"
method = "run.workspace_config"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_get"
method = "run.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_events"
method = "run.events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_cancel"
method = "run.cancel"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifacts"
method = "run.artifacts"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifact_preview"
method = "run.artifact_preview"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifact_diff"
method = "run.artifact_diff"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_review"
method = "run.review"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_diff"
method = "run.diff"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_apply"
method = "run.apply"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_discard"
method = "run.discard"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_events_recent"
method = "run.events.recent"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_get"
method = "rig.runtime_state.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_list"
method = "rig.runtime_state.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_reset"
method = "rig.runtime_state.reset"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_summary"
method = "maintenance.summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_prune"
method = "maintenance.prune"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_audit"
method = "maintenance.audit"
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

# Setup token guarding GET /v1/auth/token (the dashboard's
# bootstrap exchange). Honour an operator-supplied
# RELIX_SETUP_TOKEN; otherwise mint a strong random one. Never a
# hardcoded default — without a real token the dashboard cannot
# bootstrap. The value is written into the bridge config below and
# printed at the end so the operator can paste it into the
# dashboard's Authentication screen.
gen_setup_token() {
    if command -v openssl >/dev/null 2>&1; then
        openssl rand -hex 32
    elif [[ -r /dev/urandom ]]; then
        head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
    else
        # Last resort (no openssl, no /dev/urandom): still random,
        # just lower-entropy. Better than a fixed default.
        echo "${RANDOM}${RANDOM}${RANDOM}${RANDOM}$(date +%s%N)" \
            | sha256sum | cut -d' ' -f1
    fi
}
SETUP_TOKEN="${RELIX_SETUP_TOKEN:-$(gen_setup_token)}"

flow_lines="template_path     = \"$FLOWS_DIR/chat_template.sol\""
if [[ "$NO_TOOL" -eq 0 ]]; then
    flow_lines+=$'\n'"tool_template_path = \"$FLOWS_DIR/chat_with_tool.sol\""
fi

coord_block=""
if [[ "$NO_COORDINATOR" -eq 0 ]]; then
    coord_block=$'\n\n[coordinator]\nalias = "coordinator"'
fi

cat > "$BRIDGE_CONFIG" <<EOF
[bridge]
listen_addr = "127.0.0.1:$BRIDGE_PORT"

[auth]
setup_token = "$SETUP_TOKEN"

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
    rm -f "$PID_FILE" 2>/dev/null || true
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
    ensure_identity telegram "$TELEGRAM_BUNDLE"
    RENEWABLE_BUNDLES+=("telegram|$TELEGRAM_BUNDLE")
    start_node "telegram" "$CONTROLLER" "$TELEGRAM_CONFIG" \
        "$TELEGRAM_LOG" "$TELEGRAM_ERR" "relix_runtime=info,relix_telegram=info"
fi
if [[ "$DISCORD_ENABLED" -eq 1 ]]; then
    ensure_identity discord "$DISCORD_BUNDLE"
    RENEWABLE_BUNDLES+=("discord|$DISCORD_BUNDLE")
    start_node "discord" "$CONTROLLER" "$DISCORD_CONFIG" \
        "$DISCORD_LOG" "$DISCORD_ERR" "relix_runtime=info,relix_discord=info"
fi
if [[ "$SLACK_ENABLED" -eq 1 ]]; then
    ensure_identity slack "$SLACK_BUNDLE"
    RENEWABLE_BUNDLES+=("slack|$SLACK_BUNDLE")
    start_node "slack" "$CONTROLLER" "$SLACK_CONFIG" \
        "$SLACK_LOG" "$SLACK_ERR" "relix_runtime=info,relix_slack=info"
fi
if [[ "$PLUGINS_ENABLED" -eq 1 ]]; then
    ensure_identity plugin-host "$PLUGIN_HOST_BUNDLE"
    RENEWABLE_BUNDLES+=("plugin-host|$PLUGIN_HOST_BUNDLE")
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
BRIDGE_PID=$!
PIDS+=("$BRIDGE_PID")

# Wait for OUR bridge to confirm it bound the port. The bridge logs
# "web bridge starting" ONLY after a successful bind, into OUR log file
# ($BRIDGE_LOG). A stale bridge already holding the port logs elsewhere
# and would answer /health itself — so waiting on /health alone can be
# fooled into a false "ready" by the stale instance. Waiting for the
# needle in our own log (plus watching that the process we started is
# still alive) is not fooled: on a port collision our bridge exits with
# its actionable error and never emits the needle.
elapsed=0
until grep -q "web bridge starting" "$BRIDGE_LOG" 2>/dev/null; do
    if ! kill -0 "$BRIDGE_PID" 2>/dev/null; then
        echo "  web bridge exited during startup (port likely shadowed by a stale instance). tail:"
        tail -n 30 "$BRIDGE_ERR" "$BRIDGE_LOG" | sed 's/^/    /'
        exit 1
    fi
    sleep 0.5
    elapsed=$((elapsed + 1))
    if [[ "$elapsed" -ge 60 ]]; then
        echo "  web bridge did not start within 30s. tail:"
        tail -n 30 "$BRIDGE_ERR" "$BRIDGE_LOG" | sed 's/^/    /'
        exit 1
    fi
done
# Our bridge bound the port; confirm it serves.
elapsed=0
until curl -fsS "http://127.0.0.1:$BRIDGE_PORT/health" >/dev/null 2>&1; do
    if ! kill -0 "$BRIDGE_PID" 2>/dev/null; then
        echo "  web bridge exited during startup. tail:"
        tail -n 30 "$BRIDGE_ERR" "$BRIDGE_LOG" | sed 's/^/    /'
        exit 1
    fi
    sleep 0.5
    elapsed=$((elapsed + 1))
    if [[ "$elapsed" -ge 60 ]]; then
        echo "  bridge did not become healthy within 30s. tail:"
        tail -n 30 "$BRIDGE_LOG" | sed 's/^/    /'
        exit 1
    fi
done
echo "  bridge ready"

# Record every PID we started so an out-of-band shutdown can terminate
# exactly this mesh and nothing else. cleanup() removes it on exit.
printf '%s\n' "${PIDS[@]}" > "$PID_FILE"

echo
echo "BRIDGE_UP"
echo
echo "Dashboard:   http://127.0.0.1:$BRIDGE_PORT/dashboard"
echo "Health:      http://127.0.0.1:$BRIDGE_PORT/health"
echo "Provider:    $PROVIDER"
if [ -f "$HOME/.relix/dashboard-admin.json" ]; then
    echo "  Log in with your dashboard admin username + password."
    echo "  Forgot it? ./scripts/relix-dashboard-admin-reset.sh  (local recovery; restart the bridge after)."
else
    echo "  First run: open the dashboard and CREATE the admin account (username + password)."
    echo "  Prefer the CLI? ./scripts/relix-dashboard-admin-reset.sh  pre-creates it locally."
fi
echo "  Verify the product loop:  ./target/debug/relix-cli dashboard doctor"
echo
echo "Advanced (curl/scripts only — NOT the dashboard login):"
echo "  Setup token: $SETUP_TOKEN"
echo "  ^ presented as 'Authorization: Bearer <setup_token>' to GET /v1/auth/token to fetch the"
echo "    bridge bearer for raw HTTP. The browser dashboard does NOT use this — it uses the"
echo "    admin username/password above."
echo
echo "Logs:     $DATA_BASE/*.log"
echo "PIDs:     ${PIDS[*]}"
echo
echo "Ctrl-C to stop."

# ---- 17. Block until interrupted or a child dies ----

# Re-check identity bundles this often while running (seconds). 12h default.
RENEW_INTERVAL_SECS="${RELIX_IDENTITY_RENEW_INTERVAL_SECS:-43200}"
_last_renew=$SECONDS
while true; do
    for pid in "${PIDS[@]}"; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "child pid=$pid exited; shutting down."
            exit 1
        fi
    done
    # Periodic identity renewal: re-mint any bundle within its renewal
    # window so a mesh running for months never lapses. Cheap no-op when
    # healthy. The refreshed bundle on disk is adopted by a node on its
    # next restart (identity is loaded at boot; no hot-reload yet) — but
    # combined with the 365-day lifetime and boot-time self-heal this keeps
    # a long-running mesh valid with no operator action.
    if (( SECONDS - _last_renew >= RENEW_INTERVAL_SECS )); then
        for _entry in "${RENEWABLE_BUNDLES[@]}"; do
            ensure_identity "${_entry%%|*}" "${_entry#*|}" >/dev/null 2>&1 || true
        done
        _last_renew=$SECONDS
    fi
    sleep 1
done
