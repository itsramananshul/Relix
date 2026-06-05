#!/usr/bin/env bash
# scripts/relix-dashboard-admin-reset.sh
#
# LOCAL operator recovery for a forgotten dashboard admin password.
#
# Wraps `relix-web-bridge reset-admin`, which rewrites the dashboard admin
# credential (dashboard-admin.json) with a fresh Argon2id-hashed password —
# the SAME storage the first-run setup uses. This is a local filesystem
# operation on THIS machine; there is no remote / unauthenticated reset.
#
# It does NOT touch dev-data, Briefs, runs, or other state — only the single
# admin credential file. Restart the bridge afterward for the new password to
# take effect (a restart also drops existing in-memory sessions).
#
# Usage:
#   # Generate a strong password, keep the existing username (or "admin"):
#   ./scripts/relix-dashboard-admin-reset.sh
#
#   # Set a specific username + password:
#   ./scripts/relix-dashboard-admin-reset.sh --username ops --password 'my-strong-pass'
#
#   # Point at a specific admin file or bridge config (advanced):
#   ./scripts/relix-dashboard-admin-reset.sh --admin-file "$HOME/.relix/dashboard-admin.json"
#   ./scripts/relix-dashboard-admin-reset.sh --config dev-data/local/bridge.toml
set -euo pipefail

cd "$(dirname "$0")/.."

BRIDGE="target/debug/relix-web-bridge"
if [[ ! -x "$BRIDGE" ]]; then
    echo "relix-web-bridge not found at $BRIDGE" >&2
    echo "Build it first:  cargo build -p relix-web-bridge" >&2
    exit 1
fi

# Forward all args verbatim; the binary applies safe defaults for anything
# omitted (admin file -> ~/.relix; username -> existing or 'admin';
# password -> generated + printed).
echo "Resetting local dashboard admin credential..."
exec "$BRIDGE" reset-admin "$@"
