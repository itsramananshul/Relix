#!/usr/bin/env bash
# scripts/alpha-bringup-m6.sh
#
# End-to-end M6/S4 demo: a SOL flow runs `remote_call` against a real
# libp2p controller, with admission pipeline + audit on the responder
# and per-flow event log on the runner side.
#
# Usage:
#   ./scripts/alpha-bringup-m6.sh             # full happy path; cleans up after
#   ./scripts/alpha-bringup-m6.sh --keep      # leave dev-keys/, dev-data/, configs/policies/m6demo.toml
#
# Requires: cargo (1.95+). Run from the repo root.

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."

# Git-bash / MSYS: keep multiaddr slashes intact.
export MSYS_NO_PATHCONV=1

mkdir -p dev-keys dev-data
rm -f dev-keys/m6demo-*

ORG_KEY=dev-keys/m6demo-org-root.key
ORG_PUB=dev-keys/m6demo-org-root.pub
ALICE=dev-keys/m6demo-alice.aic
NODE_KEY=dev-keys/m6demo-node.key
DATA_DIR=dev-data/m6demo
PORT=19501

# 1) Org root + identities. Alice is admitted; Bob (guest) is policy-denied.
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m6demo
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name alice --groups chat-users --out "$ALICE"
BOB=dev-keys/m6demo-bob.aic
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB"

# 2) Per-run controller config + policy (same shape as the M5 script).
mkdir -p "$DATA_DIR"
CONFIG="$DATA_DIR/controller.toml"
cat > "$CONFIG" <<EOF
[controller]
name = "m6demo"
node_type = "demo"
listen_port = $PORT

[identity]
key_path = "$NODE_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "configs/policies/m6demo.toml"

[peers]
EOF

mkdir -p configs/policies
cat > configs/policies/m6demo.toml <<EOF
[admit]
groups = ["chat-users"]

[[rules]]
name = "chat_users_health"
method = "node.health"
allow_groups = ["chat-users"]
EOF

# 3) Start controller in background.
LOG="$DATA_DIR/controller.log"
rm -f "$DATA_DIR/audit.log"
echo "starting controller on tcp/$PORT ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info \
    cargo run -q -p relix-controller -- --config "$CONFIG" \
    > "$LOG" 2>&1 &
PID=$!

cleanup() {
    if [[ -n "${PID:-}" ]]; then
        kill "$PID" 2>/dev/null || true
        wait "$PID" 2>/dev/null || true
    fi
    if [[ "$KEEP" -ne 1 ]]; then
        rm -f "$ORG_KEY" "$ORG_PUB" "$ALICE" "$BOB" "$NODE_KEY"
        rm -f configs/policies/m6demo.toml
        rm -rf "$DATA_DIR" dev-data/flow-runner
    fi
}
trap cleanup EXIT

# Wait for controller readiness.
for i in $(seq 1 60); do
    if grep -q "transport listening" "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.5
if ! grep -q "transport listening" "$LOG" 2>/dev/null; then
    echo "controller failed to start; log:"
    tail -30 "$LOG"
    exit 1
fi

# 4) Write the alpha peers.toml the flow runner reads.
PEERS=$DATA_DIR/peers.toml
cat > "$PEERS" <<EOF
[peers.controller]
addr = "/ip4/127.0.0.1/tcp/$PORT"
EOF

# 5) Run the SOL flow through the real M6 path.
echo "=== flow-run flows/ping.sol ==="
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/ping.sol \
        --identity "$ALICE" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"

echo
echo "=== flow-run flows/ping.sol as bob (guest) — expect policy_denied ==="
set +e
RELIX_DATA_DIR=dev-data \
    cargo run -q -p relix-cli -- flow-run \
        --flow flows/ping.sol \
        --identity "$BOB" \
        --client-key "$ORG_KEY" \
        --peers "$PEERS"
RC=$?
set -e
if [[ "$RC" -ne 2 ]]; then
    echo "expected exit code 2 from policy_denied bob run, got $RC" >&2
    exit 1
fi
echo "(bob correctly denied at responder; runner halted with FlowFailed)"

echo
echo "=== responder audit log (both calls) ==="
cargo run -q -p relix-flow-inspect -- --audit "$DATA_DIR/audit.log"

echo
echo "=== runner flow log (most recent) ==="
LATEST_FLOW_LOG=$(ls -t dev-data/flow-runner/flows/*.log 2>/dev/null | head -1 || true)
if [[ -n "${LATEST_FLOW_LOG:-}" ]]; then
    cargo run -q -p relix-flow-inspect -- --flow "$LATEST_FLOW_LOG" --human
fi

echo
echo "M6 demo OK."
