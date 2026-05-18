#!/usr/bin/env bash
# scripts/alpha-bringup-m5.sh
#
# End-to-end M5 demo: two real processes (controller + CLI) talking over
# libp2p with signed identity, policy, and audit.
#
# Usage:
#   ./scripts/alpha-bringup-m5.sh             # full happy path + deny case
#   ./scripts/alpha-bringup-m5.sh --keep      # leave dev-keys/ and dev-data/ in place
#
# Requires: cargo (1.95+) in $PATH. Run from the repo root.

set -euo pipefail

KEEP=0
if [[ "${1:-}" == "--keep" ]]; then
    KEEP=1
fi

cd "$(dirname "$0")/.."

# Git-bash / MSYS: keep multiaddr slashes intact when forwarded to a Windows exe.
export MSYS_NO_PATHCONV=1

# Local sandbox dirs (gitignored).
mkdir -p dev-keys dev-data
rm -f dev-keys/m5demo-*.key dev-keys/m5demo-*.aic dev-keys/m5demo-*.pub

ORG_KEY=dev-keys/m5demo-org-root.key
ORG_PUB=dev-keys/m5demo-org-root.pub
ALICE=dev-keys/m5demo-alice.aic
BOB=dev-keys/m5demo-bob.aic
NODE_KEY=dev-keys/m5demo-node.key
DATA_DIR=dev-data/m5demo
PORT=19501

# 1) Org root + identities. init-org writes both the secret (.key) and the
# companion .pub file alongside it; the trust-root config references the .pub.
cargo run -q -p relix-cli -- identity init-org --root-key "$ORG_KEY" --org m5demo

cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name alice --groups chat-users --out "$ALICE"
cargo run -q -p relix-cli -- identity mint \
    --root-key "$ORG_KEY" --name bob --groups guest --out "$BOB"

# 2) Write an ad-hoc controller config pointing at the demo dirs + port.
# Use a repo-local path: /tmp doesn't translate to a Windows path that the
# cargo-launched binary can read.
mkdir -p dev-data/m5demo
CONFIG=dev-data/m5demo/controller.toml
cat > "$CONFIG" <<EOF
[controller]
name = "m5demo"
node_type = "demo"
listen_port = $PORT

[identity]
key_path = "$NODE_KEY"

[trust]
org_root_key_path = "$ORG_PUB"

[policy]
file = "configs/policies/m5demo.toml"

[peers]
EOF

mkdir -p configs/policies
cat > configs/policies/m5demo.toml <<EOF
[admit]
groups = ["chat-users", "guest"]

[[rules]]
name = "chat_users_health"
method = "node.health"
allow_groups = ["chat-users"]
EOF

# 3) Start the controller in the background.
LOG=dev-data/m5demo/controller.log
# Don't rm -rf $DATA_DIR — we just wrote the config there. Just clear any
# pre-existing audit log from a prior run.
rm -f "$DATA_DIR/audit.log"
echo "starting controller on tcp/$PORT ..."
RELIX_DATA_DIR=dev-data RUST_LOG=relix_runtime=info,relix_cli=info \
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
        rm -f configs/policies/m5demo.toml
        rm -rf "$DATA_DIR"
    fi
}
trap cleanup EXIT

# Wait for controller to be ready (look for the listening-on log line).
for i in $(seq 1 30); do
    if grep -q "transport listening" "$LOG" 2>/dev/null; then
        break
    fi
    sleep 0.2
done
sleep 0.5

# 4) Allowed identity → expect OK.
echo "=== ping as alice (chat-users) ==="
cargo run -q -p relix-cli -- ping \
    --peer "/ip4/127.0.0.1/tcp/$PORT" \
    --identity "$ALICE" \
    --client-key "$ORG_KEY"

echo
# 5) Denied identity → expect non-zero exit, ERR line printed.
echo "=== ping as bob (guest) — expect policy_denied ==="
set +e
cargo run -q -p relix-cli -- ping \
    --peer "/ip4/127.0.0.1/tcp/$PORT" \
    --identity "$BOB" \
    --client-key "$ORG_KEY"
RC=$?
set -e
if [[ "$RC" -ne 2 ]]; then
    echo "expected exit code 2 from policy_denied, got $RC" >&2
    exit 1
fi
echo "(bob correctly denied)"

echo
# 6) Inspect audit log on the responder.
echo "=== responder audit log ==="
cargo run -q -p relix-flow-inspect -- --audit "$DATA_DIR/audit.log"

echo
echo "M5 demo OK."
