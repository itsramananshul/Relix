#!/usr/bin/env bash
# scripts/relix-local-backup.sh
#
# Create a LOCAL tar.gz backup of important Relix state (SQLite DBs + configs).
# It never uploads anywhere — the archive stays on this machine.
#
# By default it EXCLUDES regenerable / large / sensitive material:
#   build output (target, target-audit, node_modules, dist), .git, run
#   workspaces (workspaces, runs), logs (*.log), and secrets (bridge-token,
#   dashboard-admin.json, *.key, *.aic, dev-keys, .env / .env.*).
#
# For a CONSISTENT database backup, stop the mesh first
# (./scripts/relix-mesh-down.sh) so the SQLite files aren't mid-write.
#
# Usage:
#   ./scripts/relix-local-backup.sh                    # backs up dev-data
#   ./scripts/relix-local-backup.sh --source dev-data --out-dir backups
#   ./scripts/relix-local-backup.sh --include-workspaces
#   ./scripts/relix-local-backup.sh --include-secrets  # careful!
set -euo pipefail
cd "$(dirname "$0")/.."

SOURCE="dev-data"
OUTDIR="backups"
INCLUDE_WS=0
INCLUDE_SECRETS=0
LIST_CONTENTS=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --source) SOURCE="$2"; shift 2 ;;
        --out-dir) OUTDIR="$2"; shift 2 ;;
        --include-workspaces) INCLUDE_WS=1; shift ;;
        --include-secrets) INCLUDE_SECRETS=1; shift ;;
        --list-contents) LIST_CONTENTS=1; shift ;;
        -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ ! -e "$SOURCE" ]]; then
    echo "Source '$SOURCE' does not exist. Point --source at your Relix data dir (e.g. dev-data)." >&2
    exit 1
fi

EXCLUDES=(--exclude='target' --exclude='target-audit' --exclude='node_modules'
          --exclude='dist' --exclude='.git' --exclude='*.log' --exclude='*.tmp')
if [[ "$INCLUDE_WS" -eq 0 ]]; then
    EXCLUDES+=(--exclude='workspaces' --exclude='runs')
fi
if [[ "$INCLUDE_SECRETS" -eq 0 ]]; then
    EXCLUDES+=(--exclude='dev-keys' --exclude='bridge-token' --exclude='dashboard-admin.json'
               --exclude='*.key' --exclude='*.aic' --exclude='*.pem'
               --exclude='.env' --exclude='.env.*' --exclude='mesh.pids')
fi

mkdir -p "$OUTDIR"
STAMP="$(date +%Y%m%d-%H%M%S)"
ARCHIVE="$OUTDIR/relix-backup-$STAMP.tar.gz"

echo "Creating backup of '$SOURCE'..."
tar -czf "$ARCHIVE" "${EXCLUDES[@]}" "$SOURCE"

SIZE="$(du -h "$ARCHIVE" | cut -f1)"
echo ""
echo "Backup written (local only):"
echo "  path : $ARCHIVE"
echo "  size : $SIZE"
[[ "$INCLUDE_SECRETS" -eq 0 ]] && echo "  note : secrets excluded — pass --include-secrets to include them."
[[ "$INCLUDE_WS" -eq 0 ]] && echo "  note : run workspaces excluded — pass --include-workspaces to include them."

if [[ "$LIST_CONTENTS" -eq 1 ]]; then
    echo ""
    echo "Contents:"
    tar -tzf "$ARCHIVE" | sed 's/^/  /'
fi

echo ""
echo "Restore (local) — stop the mesh first, then extract into place:"
echo "  ./scripts/relix-mesh-down.sh"
echo "  # inspect first:  tar -tzf '$ARCHIVE'"
echo "  # then extract (overwrites '$SOURCE' in CWD):"
echo "  tar -xzf '$ARCHIVE'"
echo "  ./scripts/relix-mesh-up.sh"
echo "Note: this script does NOT auto-restore (destructive) — extract the archive yourself. See docs/operations.md."
