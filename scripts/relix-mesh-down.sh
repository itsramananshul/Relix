#!/usr/bin/env bash
#
# scripts/relix-mesh-down.sh
#
# Stops a local Relix mesh by terminating ONLY the PIDs that
# scripts/relix-mesh-up.sh recorded in its pidfile. It never matches by
# process name, so a relix-controller or relix-web-bridge belonging to
# another mesh (or started by hand outside this run) is left untouched.
# This upholds the mesh-up contract: only kill the PIDs we started.
#
# Use this if you backgrounded the mesh and lost the terminal that
# relix-mesh-up.sh was blocking in. A mesh shut down with Ctrl-C in its
# own terminal is already torn down by that script's own cleanup; this
# is the out-of-band path for a backgrounded or crashed run.
#
# Sends SIGTERM first, waits briefly, then SIGKILL anything still up.
# Prints which PIDs it stopped and removes the pidfile when done.
# Idempotent: exits 0 when there is no pidfile or nothing left to stop.

set -euo pipefail

RUN="local"
DATA_DIR="${RELIX_DATA_DIR:-dev-data}"

usage() {
    cat <<'EOF'
Usage: scripts/relix-mesh-down.sh [options]

Options:
  --run <name>      Deployment label used at mesh-up      (default: local)
  --data-dir <dir>  Runtime data root used at mesh-up     (default: dev-data,
                    or $RELIX_DATA_DIR when set)
  -h, --help        Print this message

Stops only the PIDs scripts/relix-mesh-up.sh recorded under
<data-dir>/<run>/mesh.pids. Never kills by process name, so an unrelated
mesh on the same machine survives.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --run)       RUN="$2"; shift 2 ;;
        --data-dir)  DATA_DIR="$2"; shift 2 ;;
        -h|--help)   usage; exit 0 ;;
        *)           echo "unknown arg: $1" >&2; usage; exit 1 ;;
    esac
done

# Resolve a relative data dir against the same root mesh-up uses. mesh-up
# `cd`s to the script's parent before creating `dev-data/<run>`, so the
# pidfile lives under SCRIPT_DIR/.. - not the caller's CWD. Matching that
# hop here lets `relix stop` (which may run from anywhere) find the file.
# An absolute --data-dir / $RELIX_DATA_DIR is used as-is.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR/.."

PID_FILE="$DATA_DIR/$RUN/mesh.pids"

if [[ ! -f "$PID_FILE" ]]; then
    echo "no pidfile at $PID_FILE; nothing to stop."
    exit 0
fi

# Read recorded PIDs (one per line). Skip blank and non-numeric lines so a
# partially written or hand-edited file can never turn into a stray signal.
PIDS=()
while IFS= read -r line; do
    line="${line//[[:space:]]/}"
    [[ -z "$line" ]] && continue
    [[ "$line" =~ ^[0-9]+$ ]] || continue
    PIDS+=("$line")
done < "$PID_FILE"

if [[ ${#PIDS[@]} -eq 0 ]]; then
    echo "pidfile $PID_FILE held no usable PIDs; removing it."
    rm -f "$PID_FILE"
    exit 0
fi

STOPPED=()
for pid in "${PIDS[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        STOPPED+=("$pid")
    fi
done

if [[ ${#STOPPED[@]} -eq 0 ]]; then
    echo "no recorded mesh processes were still running."
    rm -f "$PID_FILE"
    exit 0
fi

sleep 0.5

for pid in "${STOPPED[@]}"; do
    if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
        echo "  hard-killed pid=$pid"
    else
        echo "  stopped     pid=$pid"
    fi
done

rm -f "$PID_FILE"
echo "mesh down."
