#!/usr/bin/env bash
# scripts/alpha-bringup-m7-chat.sh
#
# M7 first chat orchestration: two real controller processes (memory + ai)
# and a SOL flow that fetches recent history, calls the AI peer, and persists
# both turns to memory. Stub AI for M7; M8 swaps in the real provider behind
# the same `ai.chat` capability.
#
# Acceptance:
#   Alice (chat-users) flow log = 10 events in exact order:
#       FlowStarted
#       Issued/Completed  (memory.recent_for_session)
#       Issued/Completed  (ai.chat)
#       Issued/Completed  (memory.write_turn user)
#       Issued/Completed  (memory.write_turn assistant)
#       FlowCompleted
#   Memory audit has 3 alice records; ai audit has 1.
#   Bob (guest) denied at first call; 4-event flow log; exit 2.
#   Re-running as alice shows the previous turns in recent_for_session output.
#
# Usage:
#   ./scripts/alpha-bringup-m7-chat.sh
#   ./scripts/alpha-bringup-m7-chat.sh --keep

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."
export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data configs/policies
rm -f dev-keys/m7chat-*
rm -rf dev-data/m7chat dev-data/m7chat-memory dev-data/m7chat-ai dev-data/flow-runner

ORG_KEY=dev-keys/m7chat-org-root.key
ORG_PUB=dev-keys/m7chat-org-root.pub
ALICE=dev-keys/m7chat-alice.aic
BOB=dev-keys/m7chat-bob.aic
MEM_KEY=dev-keys/m7chat-memory.key
AI_KEY=dev-keys/m7chat-ai.key
DATA_BASE=dev-data/m7chat
MEM_PORT=19601
AI_PORT=19602
MEM_DB=$DATA_BASE/memory.db
POLICY=configs/policies/m7chat.toml

# 1) Identities.
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m7chat
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name alice --groups chat-users --out "$ALICE"
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB"

# 2) Controller configs.
mkdir -p "$DATA_BASE"
MEM_CONFIG=$DATA_BASE/memory.toml
AI_CONFIG=$DATA_BASE/ai.toml

cat > "$MEM_CONFIG" <<EOF
[controller]
name = "m7chat-memory"
node_type = "memory"
listen_port = $MEM_PORT

[identity]
key_path = "$MEM_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[memory]
db_path = "$MEM_DB"

[peers]
EOF

cat > "$AI_CONFIG" <<EOF
[controller]
name = "m7chat-ai"
node_type = "ai"
listen_port = $AI_PORT

[identity]
key_path = "$AI_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[ai]
# Use the deterministic mock provider for the default demo.
# To run against real Anthropic, set provider = "anthropic" here and
# either export ANTHROPIC_API_KEY in the shell or set
# [ai.anthropic] api_key_path = "dev-keys/anthropic.key".
provider = "mock"

[peers]
EOF

# 3) Shared policy: chat-users admitted to memory.* and ai.chat.
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

# 4) Start both controllers.
MEM_LOG=$DATA_BASE/memory.log
AI_LOG=$DATA_BASE/ai.log
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
    for pid in "$MEM_PID" "$AI_PID"; do
        [[ -n "${pid:-}" ]] && kill "$pid" 2>/dev/null || true
        [[ -n "${pid:-}" ]] && wait "$pid" 2>/dev/null || true
    done
    if [[ "$KEEP" -ne 1 ]]; then
        rm -f "$ORG_KEY" "$ORG_PUB" "$ALICE" "$BOB" "$MEM_KEY" "$AI_KEY" "$POLICY"
        rm -rf "$DATA_BASE" dev-data/flow-runner
        rm -rf dev-data/m7chat-memory dev-data/m7chat-ai
    fi
}
trap cleanup EXIT

wait_for() {
    local log=$1
    for _ in $(seq 1 60); do
        grep -q "transport listening" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    echo "controller didn't start; log:"
    tail -20 "$log"
    return 1
}
wait_for "$MEM_LOG"
wait_for "$AI_LOG"
sleep 0.3

# 5) Peer alias map.
PEERS=$DATA_BASE/peers.toml
cat > "$PEERS" <<EOF
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AI_PORT"
EOF

# 6) Run chat.sol as alice.
echo
echo "=== flow-run flows/chat.sol as alice (chat-users) ==="
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/chat.sol \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"
ALICE_FLOW=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1)

# 7) Run again as alice — recent history should now contain the prior turns.
echo
echo "=== flow-run flows/chat.sol as alice (second run; recent should grow) ==="
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/chat.sol \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"

# 8) Run as bob — expect denial at first call.
echo
echo "=== flow-run flows/chat.sol as bob (guest) — expect denial ==="
set +e
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/chat.sol \
        --identity "$BOB" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"
RC=$?
set -e
if [[ "$RC" -ne 2 ]]; then
    echo "expected exit 2 from bob denied path, got $RC" >&2
    exit 1
fi
echo "(bob correctly denied at first call; flow halted)"

# 9) Inspect.
echo
echo "=== alice flow log (first run) ==="
cargo run -q -p relix-flow-inspect -- --flow "$ALICE_FLOW" --human

echo
echo "=== memory responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m7chat-memory/audit.log

echo
echo "=== ai responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m7chat-ai/audit.log

# Memory-persistence verification: ask the memory peer directly for the
# session's recent history. After two successful alice runs we expect at
# least four turns (user+assistant × 2). Uses a tiny throwaway flow so the
# verification itself goes through the same admission pipeline + audit, not
# a back-door query.
echo
echo "=== memory persistence verification (recent_for_session via SOL) ==="
VERIFY_FLOW=$DATA_BASE/verify_recent.sol
cat > "$VERIFY_FLOW" <<'EOF'
function start() -> str {
    let h: str = remote_call("memory", "memory.recent_for_session", "chat-session");
    print(h);
    return h;
}
EOF
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow "$VERIFY_FLOW" \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"

echo
echo "M7 chat orchestration OK."
