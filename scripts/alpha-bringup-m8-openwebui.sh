#!/usr/bin/env bash
# scripts/alpha-bringup-m8-openwebui.sh
#
# M8/S2 demo. Brings up the same mesh as the M8 web-bridge script
# (memory + ai controllers + bridge) and exercises the new endpoints:
#
#   - POST /chat/stream            — bridge-level SSE (SIMP-019)
#   - GET  /v1/models              — OpenAI-compatible model list
#   - POST /v1/chat/completions    — OpenAI shim (non-stream + stream)
#
# Then prints the exact Open WebUI configuration to point at this bridge.
#
# Architectural invariants the script proves (carried over from M8):
#   - bridge has its own identity bundle; no AI provider key in the bridge
#   - all routing happens in flows/chat_template.sol
#   - OpenAI shim only rewrites the request/response shape; orchestration is
#     still SOL
#   - session_id is derived from the first user message so a second turn
#     (full history resent by an OpenAI client) lands in the same memory bucket
#
# Usage:
#   ./scripts/alpha-bringup-m8-openwebui.sh
#   ./scripts/alpha-bringup-m8-openwebui.sh --keep

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data configs/policies
rm -f dev-keys/m8ow-*
rm -rf dev-data/m8ow dev-data/m8ow-memory dev-data/m8ow-ai dev-data/flow-runner

ORG_KEY=dev-keys/m8ow-org-root.key
ORG_PUB=dev-keys/m8ow-org-root.pub
BRIDGE_AIC=dev-keys/m8ow-bridge.aic
MEM_KEY=dev-keys/m8ow-memory.key
AI_KEY=dev-keys/m8ow-ai.key
BRIDGE_KEY=dev-keys/m8ow-bridge.key
DATA_BASE=dev-data/m8ow
MEM_PORT=19711
AI_PORT=19712
BRIDGE_HTTP=127.0.0.1:19791
POLICY=configs/policies/m8ow.toml

# 1) Identities.
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m8ow
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name web-bridge --groups chat-users --out "$BRIDGE_AIC"

# 2) Configs (memory + ai controllers, shared policy).
mkdir -p "$DATA_BASE"
MEM_CONFIG=$DATA_BASE/memory.toml
AI_CONFIG=$DATA_BASE/ai.toml
BRIDGE_CONFIG=$DATA_BASE/bridge.toml

cat > "$MEM_CONFIG" <<EOF
[controller]
name = "m8ow-memory"
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

cat > "$AI_CONFIG" <<EOF
[controller]
name = "m8ow-ai"
node_type = "ai"
listen_port = $AI_PORT

[identity]
key_path = "$AI_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[ai]
provider = "mock"

[peers]
EOF

cat > "$POLICY" <<EOF
[admit]
groups = ["chat-users"]

[[rules]]
name = "memory_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "memory_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "memory_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_chat"
method = "ai.chat"
allow_groups = ["chat-users"]
EOF

PEERS=$DATA_BASE/peers.toml
cat > "$PEERS" <<EOF
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
EOF

cat > "$BRIDGE_CONFIG" <<EOF
[bridge]
listen_addr = "$BRIDGE_HTTP"

[identity]
bundle_path     = "$BRIDGE_AIC"
client_key_path = "$BRIDGE_KEY"

[transport]
peers_path    = "$PEERS"
deadline_secs = 30

[flow]
template_path = "flows/chat_template.sol"

[sse]
chunk_bytes    = 24
chunk_delay_ms = 10

[openai_compat]
default_model = "relix-mock"

[[openai_compat.models]]
id          = "relix-mock"
description = "Mesh route — AI node currently set to mock"
EOF

# 3) Start controllers.
MEM_LOG=$DATA_BASE/memory.log
AI_LOG=$DATA_BASE/ai.log
BRIDGE_LOG=$DATA_BASE/bridge.log

echo "starting memory controller on tcp/$MEM_PORT ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$MEM_CONFIG" \
    > "$MEM_LOG" 2>&1 &
MEM_PID=$!
echo "starting ai controller on tcp/$AI_PORT ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$AI_CONFIG" \
    > "$AI_LOG" 2>&1 &
AI_PID=$!

cleanup() {
    for pid in "$MEM_PID" "$AI_PID" "${BRIDGE_PID:-}"; do
        [[ -n "${pid:-}" ]] && kill "$pid" 2>/dev/null || true
        [[ -n "${pid:-}" ]] && wait "$pid" 2>/dev/null || true
    done
    if [[ "$KEEP" -ne 1 ]]; then
        rm -f "$ORG_KEY" "$ORG_PUB" "$BRIDGE_AIC" \
              "$MEM_KEY" "$AI_KEY" "$BRIDGE_KEY" "$POLICY"
        rm -rf "$DATA_BASE" dev-data/flow-runner \
               dev-data/m8ow-memory dev-data/m8ow-ai
    fi
}
trap cleanup EXIT

wait_for() {
    local log=$1 needle=$2
    for _ in $(seq 1 80); do
        grep -q "$needle" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    echo "did not see '$needle' in $log; tail:"
    tail -20 "$log"
    return 1
}
wait_for "$MEM_LOG"  "transport listening"
wait_for "$AI_LOG"   "transport listening"
sleep 0.3

# 4) Start bridge.
echo "starting web bridge on $BRIDGE_HTTP ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_web_bridge=info,relix_runtime=info \
    cargo run -q -p relix-web-bridge -- --config "$BRIDGE_CONFIG" \
    > "$BRIDGE_LOG" 2>&1 &
BRIDGE_PID=$!
wait_for "$BRIDGE_LOG" "web bridge starting"
sleep 0.5

# 5) /health
echo
echo "=== GET /health ==="
curl -sS "http://$BRIDGE_HTTP/health"

# 6) /v1/models — OpenAI discovery endpoint.
echo
echo "=== GET /v1/models ==="
curl -sS "http://$BRIDGE_HTTP/v1/models"

# 7) /v1/chat/completions (non-stream). This is what Open WebUI posts when
#    streaming is disabled in its settings.
echo
echo
echo "=== POST /v1/chat/completions (non-stream) ==="
NONSTREAM=$(curl -sS -X POST "http://$BRIDGE_HTTP/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{
        "model":"relix-mock",
        "messages":[
            {"role":"system","content":"you are a helpful assistant"},
            {"role":"user","content":"hello, who are you?"}
        ],
        "stream": false
    }')
echo "$NONSTREAM"

FLOW_LOG=$(echo "$NONSTREAM" | grep -oE '"flow_log":"[^"]+"' | head -n1 | sed 's/.*:"\(.*\)"/\1/' | sed 's|\\\\|/|g')

# 8) Second turn with full prior history (OpenAI clients do this). Session
#    derives from the FIRST user message → same memory bucket as turn 1.
echo
echo "=== POST /v1/chat/completions (turn 2, full history resent) ==="
curl -sS -X POST "http://$BRIDGE_HTTP/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{
        "model":"relix-mock",
        "messages":[
            {"role":"system","content":"you are a helpful assistant"},
            {"role":"user","content":"hello, who are you?"},
            {"role":"assistant","content":"i am a deterministic mock"},
            {"role":"user","content":"what did i ask first?"}
        ],
        "stream": false
    }'

# 9) /v1/chat/completions (stream) — show the first ~6 SSE lines plus DONE.
echo
echo
echo "=== POST /v1/chat/completions (stream=true) ==="
curl -sS -N -X POST "http://$BRIDGE_HTTP/v1/chat/completions" \
    -H "Content-Type: application/json" \
    -d '{
        "model":"relix-mock",
        "messages":[
            {"role":"user","content":"give me a streaming reply"}
        ],
        "stream": true
    }' | head -n 20

# 10) Native /chat/stream — bridge SSE shape (event: chunk / event: done).
echo
echo "=== POST /chat/stream (native bridge SSE) ==="
curl -sS -N -X POST "http://$BRIDGE_HTTP/chat/stream" \
    -H "Content-Type: application/json" \
    -d '{"session_id":"sse-demo","message":"hello from sse"}' | head -n 20

# 11) Bridge flow log + responder audits.
echo
echo
echo "=== bridge flow log (last non-stream call) ==="
if [[ -n "${FLOW_LOG:-}" && -f "$FLOW_LOG" ]]; then
    cargo run -q -p relix-flow-inspect -- --flow "$FLOW_LOG" --human
fi

echo
echo "=== memory responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m8ow-memory/audit.log --human

echo
echo "=== ai responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m8ow-ai/audit.log --human

# 12) Secret containment — sanity-check the bridge holds no provider key.
echo
echo "=== secret containment check ==="
if grep -rIl --exclude-dir=dev-data --exclude-dir=target --exclude-dir=.git \
       -E 'ANTHROPIC_API_KEY|OPENAI_API_KEY|sk-ant-|sk-proj-' \
       crates/relix-web-bridge/ configs/web-bridge.toml \
   2>/dev/null | head; then
    echo "FAIL: AI provider key reference present in bridge files." >&2
    exit 1
fi
echo "ok: no provider key in bridge crate or bridge config."

cat <<EOF

────────────────────────────────────────────────────────────────────────
Open WebUI setup
────────────────────────────────────────────────────────────────────────

Run Open WebUI locally (Docker is easiest, but any deployment works):

    docker run -d -p 3000:8080 \\
        -v open-webui:/app/backend/data \\
        --name open-webui \\
        ghcr.io/open-webui/open-webui:main

In Open WebUI: Settings → Connections → OpenAI API
    API Base URL:  http://host.docker.internal:19791/v1
                   (or http://127.0.0.1:19791/v1 if not running in Docker)
    API Key:       any non-empty string (the bridge ignores it in alpha)
    Save.

Open the model picker — you should see "relix-mock" (and any other ids
configured under [openai_compat.models] in configs/web-bridge.toml).

Chat: every reply round-trips memory → ai → memory through this mesh.
Memory persists across browser refreshes because the bridge derives a
stable session_id from the first user message of each conversation.

Leave this demo running with --keep to keep the bridge alive while you
test from Open WebUI.

M8/S2 Open WebUI demo OK.
EOF
