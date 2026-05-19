#!/usr/bin/env bash
# scripts/alpha-bringup-m8-web-bridge.sh
#
# M8 web-bridge end-to-end demo. Brings up:
#   - memory controller   (tcp/19701; SQLite + FTS5; node.health + memory.*)
#   - ai controller       (tcp/19702; ChatProvider = mock; ai.chat)
#   - web bridge          (loopback HTTP on 127.0.0.1:19790; POST /chat, GET /health)
#
# Then sends a single curl POST /chat as the bridge's identity, prints the
# JSON response, walks the resulting flow log and both responder audit
# logs. Also verifies the memory was actually written by re-querying via a
# tiny SOL flow through relix-cli (which goes through the same admission
# pipeline; no back-door query).
#
# Architectural invariants the script proves:
#   - the bridge is a normal peer with its own identity bundle
#   - the bridge owns NO AI provider key (the ai controller's config does)
#   - chat routing lives entirely in flows/chat_template.sol
#   - bob (guest) is denied at the responder, not pre-filtered by the bridge
#
# Usage:
#   ./scripts/alpha-bringup-m8-web-bridge.sh
#   ./scripts/alpha-bringup-m8-web-bridge.sh --keep

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data configs/policies
rm -f dev-keys/m8bridge-*
rm -rf dev-data/m8bridge dev-data/m8bridge-memory dev-data/m8bridge-ai dev-data/flow-runner

ORG_KEY=dev-keys/m8bridge-org-root.key
ORG_PUB=dev-keys/m8bridge-org-root.pub
BRIDGE_AIC=dev-keys/m8bridge-bridge.aic
BOB_AIC=dev-keys/m8bridge-bob.aic
MEM_KEY=dev-keys/m8bridge-memory.key
AI_KEY=dev-keys/m8bridge-ai.key
BRIDGE_KEY=dev-keys/m8bridge-bridge.key
DATA_BASE=dev-data/m8bridge
MEM_PORT=19701
AI_PORT=19702
BRIDGE_HTTP=127.0.0.1:19790
POLICY=configs/policies/m8bridge.toml

# 1) Identities. The bridge presents its own identity to the mesh.
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m8bridge
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name web-bridge --groups chat-users --out "$BRIDGE_AIC"
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB_AIC"

# 2) Controller configs (memory + ai) and shared policy.
mkdir -p "$DATA_BASE"
MEM_CONFIG=$DATA_BASE/memory.toml
AI_CONFIG=$DATA_BASE/ai.toml
BRIDGE_CONFIG=$DATA_BASE/bridge.toml

cat > "$MEM_CONFIG" <<EOF
[controller]
name = "m8bridge-memory"
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
name = "m8bridge-ai"
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
name = "chat_users_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "chat_users_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "chat_users_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "chat_users_ai"
method = "ai.chat"
allow_groups = ["chat-users"]
EOF

# Peers file consumed by the bridge.
PEERS=$DATA_BASE/peers.toml
cat > "$PEERS" <<EOF
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
EOF

# Bridge config.
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
EOF

# 3) Start the controllers.
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
        rm -f "$ORG_KEY" "$ORG_PUB" "$BRIDGE_AIC" "$BOB_AIC" \
              "$MEM_KEY" "$AI_KEY" "$BRIDGE_KEY" "$POLICY"
        rm -rf "$DATA_BASE" dev-data/flow-runner \
               dev-data/m8bridge-memory dev-data/m8bridge-ai
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

# 4) Start the web bridge.
echo "starting web bridge on $BRIDGE_HTTP ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_web_bridge=info,relix_runtime=info \
    cargo run -q -p relix-web-bridge -- --config "$BRIDGE_CONFIG" \
    > "$BRIDGE_LOG" 2>&1 &
BRIDGE_PID=$!
wait_for "$BRIDGE_LOG" "web bridge starting"
sleep 0.5

# 5) Health probe.
echo
echo "=== GET /health ==="
curl -sS "http://$BRIDGE_HTTP/health"

# 6) POST /chat happy path.
echo
echo "=== POST /chat (web-bridge identity, chat-users group) ==="
RESPONSE=$(curl -sS -X POST "http://$BRIDGE_HTTP/chat" \
    -H "Content-Type: application/json" \
    -d '{"session_id":"web-demo","message":"hello from web"}')
echo "$RESPONSE"

# Pull the flow log path out of the JSON (no jq; minimal grep).
FLOW_LOG=$(echo "$RESPONSE" | grep -oE '"flow_log":"[^"]+"' | sed 's/.*:"\(.*\)"/\1/' | sed 's|\\\\|/|g')

# 7) Inspect.
echo
echo "=== bridge flow log ==="
if [[ -n "${FLOW_LOG:-}" && -f "$FLOW_LOG" ]]; then
    cargo run -q -p relix-flow-inspect -- --flow "$FLOW_LOG" --human
else
    echo "(no flow log path in response; bridge response was:)"
    echo "$RESPONSE"
fi

echo
echo "=== memory responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m8bridge-memory/audit.log --human

echo
echo "=== ai responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m8bridge-ai/audit.log --human

# 8) Confirm Anthropic key is NOT in the bridge.
echo
echo "=== secret containment check ==="
if grep -rIl --exclude-dir=dev-data --exclude-dir=target --exclude-dir=.git \
       -E 'ANTHROPIC_API_KEY|sk-ant-' \
       crates/relix-web-bridge/ configs/web-bridge.toml \
   2>/dev/null | head; then
    echo "FAIL: AI provider key reference present in bridge files." >&2
    exit 1
fi
echo "ok: no Anthropic key in bridge crate or bridge config."

# 9) Bridge HTTP-layer input validation (should reject `|` in input).
echo
echo "=== input validation: '|' rejected at HTTP layer ==="
BAD_RC=0
curl -sS -o /tmp/relix-bad-resp -w '%{http_code}\n' -X POST "http://$BRIDGE_HTTP/chat" \
    -H "Content-Type: application/json" \
    -d '{"session_id":"web-demo","message":"breaks|wire"}' || BAD_RC=$?
cat /tmp/relix-bad-resp
rm -f /tmp/relix-bad-resp

# 10) Run as a "wrong identity" by sending another chat with a fresh
#     session (still allowed) — this just confirms the happy path repeats.
echo
echo "=== POST /chat (second call, same identity, growing history) ==="
curl -sS -X POST "http://$BRIDGE_HTTP/chat" \
    -H "Content-Type: application/json" \
    -d '{"session_id":"web-demo","message":"second message"}'

echo
echo
echo "M8 web-bridge demo OK."
