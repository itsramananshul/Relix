#!/usr/bin/env bash
# scripts/alpha-bringup-m7-memory.sh
#
# M7 memory-node demo: a SOL flow writes two turns to a real SQLite + FTS5
# memory store on the memory peer, then reads recent history back. All three
# remote_calls go through the M5 admission pipeline (identity + policy +
# audit).
#
# Acceptance:
#   - Alice succeeds; flow log has 8 events:
#       FlowStarted
#       RemoteCallIssued/Completed (write_turn user)
#       RemoteCallIssued/Completed (write_turn assistant)
#       RemoteCallIssued/Completed (recent_for_session)
#       FlowCompleted
#   - memory.recent_for_session returns both turns in oldest-first order.
#   - memory responder audit records all three Alice calls.
#   - Bob (guest) is denied at write_turn (4-event flow).
#
# Usage:
#   ./scripts/alpha-bringup-m7-memory.sh        # full demo + cleanup
#   ./scripts/alpha-bringup-m7-memory.sh --keep # leave keys / data / db

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."

export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data configs/policies
rm -f dev-keys/m7memory-*
# Pre-clean per-run state — stale audit/log files signed by a previous key
# would fail integrity check at controller startup.
rm -rf dev-data/m7memory dev-data/m7memory-memory dev-data/flow-runner

ORG_KEY=dev-keys/m7memory-org-root.key
ORG_PUB=dev-keys/m7memory-org-root.pub
ALICE=dev-keys/m7memory-alice.aic
BOB=dev-keys/m7memory-bob.aic
MEM_KEY=dev-keys/m7memory-memory.key
DATA_BASE=dev-data/m7memory
MEM_PORT=19501
MEM_DB=$DATA_BASE/memory.db

# 1) Org + alice (chat-users) + bob (guest).
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m7memory
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name alice --groups chat-users --out "$ALICE"
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB"

# 2) Memory controller config + policy.
mkdir -p "$DATA_BASE"
MEM_CONFIG=$DATA_BASE/controller.toml
POLICY=configs/policies/m7memory.toml

cat > "$MEM_CONFIG" <<EOF
[controller]
name = "m7memory-memory"
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
max_n = 50

[peers]
EOF

cat > "$POLICY" <<EOF
[admit]
groups = ["chat-users"]

[[rules]]
name = "chat_users_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "chat_users_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "chat_users_search"
method = "memory.search"
allow_groups = ["chat-users"]
EOF

# 3) Start the memory controller.
MEM_LOG=$DATA_BASE/controller.log
echo "starting memory controller on tcp/$MEM_PORT ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$MEM_CONFIG" \
    > "$MEM_LOG" 2>&1 &
PID=$!

cleanup() {
    [[ -n "${PID:-}" ]] && kill "$PID" 2>/dev/null || true
    [[ -n "${PID:-}" ]] && wait "$PID" 2>/dev/null || true
    if [[ "$KEEP" -ne 1 ]]; then
        rm -f "$ORG_KEY" "$ORG_PUB" "$ALICE" "$BOB" "$MEM_KEY" "$POLICY"
        rm -rf "$DATA_BASE" dev-data/flow-runner dev-data/m7memory-memory
    fi
}
trap cleanup EXIT

for _ in $(seq 1 60); do
    grep -q "transport listening" "$MEM_LOG" 2>/dev/null && break
    sleep 0.2
done
sleep 0.3
grep -q "transport listening" "$MEM_LOG" || { echo "controller didn't start:"; tail -20 "$MEM_LOG"; exit 1; }

# 4) Peer alias map for the SOL flow.
PEERS=$DATA_BASE/peers.toml
cat > "$PEERS" <<EOF
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MEM_PORT"
EOF

# 5) Run memory_demo.sol as alice — expect 8-event flow log + history readback.
echo
echo "=== flow-run flows/memory_demo.sol as alice (chat-users) ==="
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/memory_demo.sol \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"

ALICE_FLOW=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1)

# 6) Run as bob (guest) — expect denial at first write_turn.
echo
echo "=== flow-run flows/memory_demo.sol as bob (guest) — expect denial ==="
set +e
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/memory_demo.sol \
        --identity "$BOB" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"
RC=$?
set -e
if [[ "$RC" -ne 2 ]]; then
    echo "expected exit 2 from bob denied path, got $RC" >&2
    exit 1
fi
echo "(bob correctly denied at first memory.write_turn; flow halted with FlowFailed)"
BOB_FLOW=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1)

# 7) Inspect.
echo
echo "=== alice flow log ==="
cargo run -q -p relix-flow-inspect -- --flow "$ALICE_FLOW" --human
echo
echo "=== bob flow log ==="
cargo run -q -p relix-flow-inspect -- --flow "$BOB_FLOW" --human
echo
echo "=== memory responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m7memory-memory/audit.log

echo
echo "M7 memory-node demo OK."
