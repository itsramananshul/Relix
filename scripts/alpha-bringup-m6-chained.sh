#!/usr/bin/env bash
# scripts/alpha-bringup-m6-chained.sh
#
# M6/S7 demo: two real controller processes (memory + ai) over libp2p,
# a single SOL flow that calls `node.health` on each in order.
#
# Acceptance:
#   - Alice (chat-users) succeeds; flow log has 6 events in order.
#   - Bob (guest) fails at the first remote_call; flow log records
#     RemoteCallFailed + FlowFailed and the runner exits 2.
#   - memory and ai audit logs each contain one record per Alice call,
#     and the bob deny path writes a single record at the memory responder.
#
# Usage:
#   ./scripts/alpha-bringup-m6-chained.sh        # run end-to-end and clean up
#   ./scripts/alpha-bringup-m6-chained.sh --keep # leave keys + data dirs

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."

# Git-bash / MSYS: keep multiaddr slashes intact for the libp2p exe.
export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data configs/policies
rm -f dev-keys/m6chained-*
# Wipe any audit / data from a prior partial run; old audit logs are signed
# by a previous controller's identity key and verify_chain on reopen would
# fail with an integrity error.
rm -rf dev-data/m6chained-memory dev-data/m6chained-ai dev-data/m6chained dev-data/flow-runner

ORG_KEY=dev-keys/m6chained-org-root.key
ORG_PUB=dev-keys/m6chained-org-root.pub
ALICE=dev-keys/m6chained-alice.aic
BOB=dev-keys/m6chained-bob.aic
MEM_KEY=dev-keys/m6chained-memory.key
AI_KEY=dev-keys/m6chained-ai.key
DATA_BASE=dev-data/m6chained
MEM_PORT=19501
AI_PORT=19502

# 1) Org root + alice (chat-users) + bob (guest).
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m6chained
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name alice --groups chat-users --out "$ALICE"
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB"

# 2) Per-controller configs + a shared policy file (same allow rule).
mkdir -p "$DATA_BASE/memory" "$DATA_BASE/ai"
MEM_CONFIG=$DATA_BASE/memory/controller.toml
AI_CONFIG=$DATA_BASE/ai/controller.toml
POLICY=configs/policies/m6chained.toml

write_controller_config() {
    local out=$1 name=$2 node_type=$3 port=$4 keyfile=$5
    cat > "$out" <<EOF
[controller]
name = "$name"
node_type = "$node_type"
listen_port = $port

[identity]
key_path = "$keyfile"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "$POLICY"

[peers]
EOF
}

write_controller_config "$MEM_CONFIG" "m6chained-memory" "memory" "$MEM_PORT" "$MEM_KEY"
write_controller_config "$AI_CONFIG"  "m6chained-ai"     "ai"     "$AI_PORT"  "$AI_KEY"

cat > "$POLICY" <<EOF
[admit]
groups = ["chat-users"]

[[rules]]
name = "chat_users_health"
method = "node.health"
allow_groups = ["chat-users"]
EOF

# 3) Start both controllers in the background.
MEM_LOG=$DATA_BASE/memory/controller.log
AI_LOG=$DATA_BASE/ai/controller.log
rm -f "$DATA_BASE/memory/audit.log" "$DATA_BASE/ai/audit.log"

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
        rm -f "$ORG_KEY" "$ORG_PUB" "$ALICE" "$BOB" "$MEM_KEY" "$AI_KEY"
        rm -f "$POLICY"
        rm -rf "$DATA_BASE" dev-data/flow-runner
        rm -rf dev-data/m6chained-memory dev-data/m6chained-ai
    fi
}
trap cleanup EXIT

wait_for_listening() {
    local log=$1
    for _ in $(seq 1 60); do
        grep -q "transport listening" "$log" 2>/dev/null && return 0
        sleep 0.2
    done
    echo "controller failed to start; log:"
    tail -30 "$log"
    return 1
}
wait_for_listening "$MEM_LOG"
wait_for_listening "$AI_LOG"
sleep 0.5

# 4) Run chained flow as alice — expect success.
echo
echo "=== flow-run flows/chained_health.sol as alice (chat-users) ==="
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/chained_health.sol \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers configs/peers-chained.toml

ALICE_FLOW_LOG=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1)

# 5) Run as bob — expect first-call policy denial.
echo
echo "=== flow-run flows/chained_health.sol as bob (guest) — expect denial ==="
set +e
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/chained_health.sol \
        --identity "$BOB" \
        --client-key "$ORG_KEY" \
        --peers configs/peers-chained.toml
RC=$?
set -e
if [[ "$RC" -ne 2 ]]; then
    echo "expected exit 2 from bob denied path, got $RC" >&2
    exit 1
fi
echo "(bob correctly denied at first remote_call; flow halted with FlowFailed)"

BOB_FLOW_LOG=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1)

# 6) Inspections.
echo
echo "=== alice flow log ==="
cargo run -q -p relix-flow-inspect -- --flow "$ALICE_FLOW_LOG" --human

echo
echo "=== bob flow log ==="
cargo run -q -p relix-flow-inspect -- --flow "$BOB_FLOW_LOG" --human

# The controller writes audit to $RELIX_DATA_DIR/<controller.name>/audit.log
# (see controller_runtime::data_dir_for). With RELIX_DATA_DIR=dev-data and
# the per-controller `name`s set above, the responder audit logs end up at:
echo
echo "=== memory responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m6chained-memory/audit.log

echo
echo "=== ai responder audit ==="
cargo run -q -p relix-flow-inspect -- --audit dev-data/m6chained-ai/audit.log

echo
echo "M6/S7 chained-orchestration demo OK."
