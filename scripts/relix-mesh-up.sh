#!/usr/bin/env bash
# scripts/relix-mesh-up.sh
#
# Operator boot driver. Brings up a 3-node local mesh:
#
#   memory controller (tcp/$MEM_PORT)   — SQLite + FTS5
#   ai controller     (tcp/$AI_PORT)    — ChatProvider (mock by default)
#   relix-web-bridge  (loopback HTTP)   — /chat /chat/stream /v1/*
#
# Then it BLOCKS until you Ctrl-C. Unlike the alpha-bringup-* scripts (which
# self-demo and exit), this one is meant to stay alive so you can talk to it
# from curl, Open WebUI, or any OpenAI-compatible client.
#
# All config and data go under dev-data/<RUN>/ so multiple runs can coexist
# and so a single `rm -rf dev-data/<RUN>` cleans up.
#
# Usage:
#   ./scripts/relix-mesh-up.sh
#   ./scripts/relix-mesh-up.sh --provider openrouter   # requires $OPENROUTER_API_KEY
#   ./scripts/relix-mesh-up.sh --provider openai       # requires $OPENAI_API_KEY
#   ./scripts/relix-mesh-up.sh --provider anthropic    # requires $ANTHROPIC_API_KEY
#   ./scripts/relix-mesh-up.sh --provider local --base-url http://localhost:11434/v1
#   ./scripts/relix-mesh-up.sh --run myrun --bridge-port 19800
#
# Telegram channel: this 3-node bringup script does not spawn
# the telegram controller. Use scripts/relix-mesh-up.ps1 with
# `$env:RELIX_TELEGRAM = "1"` (Windows) for the full mesh
# including telegram, or stand up a telegram controller
# manually with `relix-controller --config configs/telegram.toml`
# after setting RELIX_TELEGRAM_BOT_TOKEN. See docs/telegram.md
# for the full setup walk-through.

set -euo pipefail

PROVIDER=mock
BASE_URL=""
RUN=local
BRIDGE_PORT=19791
MEM_PORT=19711
AI_PORT=19712

while [[ $# -gt 0 ]]; do
    case "$1" in
        --provider)    PROVIDER=$2; shift 2 ;;
        --base-url)    BASE_URL=$2; shift 2 ;;
        --run)         RUN=$2; shift 2 ;;
        --bridge-port) BRIDGE_PORT=$2; shift 2 ;;
        --mem-port)    MEM_PORT=$2; shift 2 ;;
        --ai-port)     AI_PORT=$2; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0"
            exit 0 ;;
        *)
            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

case "$PROVIDER" in
    mock|openai|openrouter|xai|local|anthropic|gemini) : ;;
    *) echo "unknown provider: $PROVIDER" >&2; exit 2 ;;
esac

DATA_BASE=dev-data/$RUN
ORG_KEY=dev-keys/$RUN-org-root.key
ORG_PUB=dev-keys/$RUN-org-root.pub
BRIDGE_AIC=dev-keys/$RUN-bridge.aic
MEM_KEY=dev-keys/$RUN-memory.key
AI_KEY=dev-keys/$RUN-ai.key
BRIDGE_KEY=dev-keys/$RUN-bridge.key
POLICY=configs/policies/$RUN.toml
BRIDGE_HTTP=127.0.0.1:$BRIDGE_PORT

mkdir -p dev-keys "$DATA_BASE" configs/policies

# 1) Identities — idempotent: only mint if missing so restarts are cheap.
if [[ ! -f "$ORG_KEY" || ! -f "$ORG_PUB" ]]; then
    cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org "$RUN"
fi
if [[ ! -f "$BRIDGE_AIC" ]]; then
    cargo run -q -p relix-cli -- identity mint \
        --root-key "$ORG_KEY" --name web-bridge --groups chat-users --out "$BRIDGE_AIC"
fi

MEM_CONFIG=$DATA_BASE/memory.toml
AI_CONFIG=$DATA_BASE/ai.toml
BRIDGE_CONFIG=$DATA_BASE/bridge.toml
PEERS=$DATA_BASE/peers.toml

# 2) Memory + AI controller configs.
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

[peers]
EOF

# AI config — provider-specific sections.
{
    cat <<EOF
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

[peers]
EOF
    case "$PROVIDER" in
      openai)
        echo
        echo "[ai.providers.openai]"
        echo "base_url      = \"${BASE_URL:-https://api.openai.com/v1}\""
        echo "api_key_env   = \"OPENAI_API_KEY\""
        echo "default_model = \"gpt-4o-mini\"" ;;
      openrouter)
        echo
        echo "[ai.providers.openrouter]"
        echo "base_url      = \"${BASE_URL:-https://openrouter.ai/api/v1}\""
        echo "api_key_env   = \"OPENROUTER_API_KEY\""
        echo "default_model = \"openai/gpt-4o-mini\"" ;;
      xai)
        echo
        echo "[ai.providers.xai]"
        echo "base_url      = \"${BASE_URL:-https://api.x.ai/v1}\""
        echo "api_key_env   = \"XAI_API_KEY\"" ;;
      local)
        echo
        echo "[ai.providers.local]"
        echo "base_url      = \"${BASE_URL:-http://localhost:11434/v1}\"" ;;
      anthropic)
        echo
        echo "[ai.providers.anthropic]"
        echo "api_key_env   = \"ANTHROPIC_API_KEY\""
        echo "default_model = \"claude-3-5-sonnet-latest\"" ;;
      gemini)
        echo
        echo "[ai.providers.gemini]"
        echo "api_key_env   = \"GEMINI_API_KEY\"" ;;
      mock) ;;
    esac
} > "$AI_CONFIG"

# 3) Shared policy.
cat > "$POLICY" <<EOF
[admit]
groups = ["chat-users"]

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
name = "ai_chat"
method = "ai.chat"
allow_groups = ["chat-users"]
EOF

# 4) Peer alias map consumed by the bridge.
cat > "$PEERS" <<EOF
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
EOF

# 5) Bridge config — note the OpenAI shim is enabled by default with one
#    cosmetic model id matching the active provider so Open WebUI shows it.
cat > "$BRIDGE_CONFIG" <<EOF
[bridge]
listen_addr = "$BRIDGE_HTTP"

[identity]
bundle_path     = "$BRIDGE_AIC"
client_key_path = "$BRIDGE_KEY"

[transport]
peers_path    = "$PEERS"
deadline_secs = 60

[flow]
template_path = "flows/chat_template.sol"

[sse]
chunk_bytes    = 24
chunk_delay_ms = 15

[openai_compat]
default_model = "relix-$PROVIDER"

[[openai_compat.models]]
id          = "relix-$PROVIDER"
description = "Relix mesh route — AI node currently set to $PROVIDER"
EOF

MEM_LOG=$DATA_BASE/memory.log
AI_LOG=$DATA_BASE/ai.log
BRIDGE_LOG=$DATA_BASE/bridge.log

cleanup() {
    echo
    echo "stopping mesh ..."
    # Try graceful kill on the direct children first.
    for pid in "${BRIDGE_PID:-}" "${AI_PID:-}" "${MEM_PID:-}"; do
        [[ -n "${pid:-}" ]] && kill "$pid" 2>/dev/null || true
    done
    for pid in "${BRIDGE_PID:-}" "${AI_PID:-}" "${MEM_PID:-}"; do
        [[ -n "${pid:-}" ]] && wait "$pid" 2>/dev/null || true
    done
    # Windows fallback: `cargo run` spawns the actual binary as a grandchild
    # and bash signal forwarding through cargo doesn't always work on
    # git-bash. Reap any leftover Relix binaries by name. Harmless if none.
    if command -v taskkill >/dev/null 2>&1; then
        taskkill //F //IM relix-controller.exe //IM relix-web-bridge.exe >/dev/null 2>&1 || true
    fi
    echo "mesh down."
}
trap cleanup EXIT INT TERM

wait_for() {
    local log=$1 needle=$2 desc=$3
    for _ in $(seq 1 100); do
        grep -q "$needle" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    echo "FAIL: $desc never appeared in $log" >&2
    tail -30 "$log" >&2
    return 1
}

echo "== Relix mesh up =="
echo "  run:           $RUN"
echo "  provider:      $PROVIDER"
echo "  memory port:   tcp/$MEM_PORT"
echo "  ai port:       tcp/$AI_PORT"
echo "  bridge HTTP:   http://$BRIDGE_HTTP"
echo "  data dir:      $DATA_BASE"
echo
echo "starting memory controller ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$MEM_CONFIG" \
    > "$MEM_LOG" 2>&1 &
MEM_PID=$!

echo "starting ai controller ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$AI_CONFIG" \
    > "$AI_LOG" 2>&1 &
AI_PID=$!

wait_for "$MEM_LOG" "transport listening" "memory controller"
wait_for "$AI_LOG"  "transport listening" "ai controller"
sleep 0.4

echo "starting web bridge ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_web_bridge=info,relix_runtime=info \
    cargo run -q -p relix-web-bridge -- --config "$BRIDGE_CONFIG" \
    > "$BRIDGE_LOG" 2>&1 &
BRIDGE_PID=$!

wait_for "$BRIDGE_LOG" "web bridge starting" "web bridge"
sleep 0.4

cat <<EOF

mesh is UP. talk to it on:

  curl http://$BRIDGE_HTTP/health
  curl http://$BRIDGE_HTTP/v1/models
  curl -X POST http://$BRIDGE_HTTP/v1/chat/completions \\
      -H 'content-type: application/json' \\
      -d '{"model":"relix-$PROVIDER","messages":[{"role":"user","content":"hello"}]}'

Open WebUI:
  Settings → Connections → OpenAI API
  API Base URL: http://host.docker.internal:$BRIDGE_PORT/v1  (Docker)
                http://127.0.0.1:$BRIDGE_PORT/v1            (native)
  API Key:      any non-empty string
  Model:        relix-$PROVIDER

Logs:
  tail -F $MEM_LOG     # memory node
  tail -F $AI_LOG      # ai node
  tail -F $BRIDGE_LOG  # web bridge

Ctrl-C to stop the mesh.

EOF

# Park here until the operator hits Ctrl-C. The trap above handles teardown.
wait "$BRIDGE_PID"
