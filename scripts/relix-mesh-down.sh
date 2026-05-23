#!/usr/bin/env bash
#
# scripts/relix-mesh-down.sh
#
# Stops every running relix-controller and relix-web-bridge on this
# machine. Use this if you backgrounded the mesh and lost the PIDs the
# boot script printed, or if a stray process from a crashed run is
# still alive.
#
# Sends SIGTERM first, waits briefly, then SIGKILL anything still up.
# Prints which PIDs it stopped. Returns 0 if it stopped at least one
# process; 0 also if nothing was running (idempotent).

set -euo pipefail

PATTERNS=(relix-controller relix-web-bridge)
STOPPED=()

collect_pids() {
    local pattern="$1"
    if command -v pgrep >/dev/null 2>&1; then
        pgrep -x "$pattern" 2>/dev/null || true
    else
        ps -A -o pid=,comm= 2>/dev/null | awk -v p="$pattern" '$2 == p { print $1 }'
    fi
}

for pattern in "${PATTERNS[@]}"; do
    for pid in $(collect_pids "$pattern"); do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            STOPPED+=("$pid:$pattern")
        fi
    done
done

if [[ ${#STOPPED[@]} -eq 0 ]]; then
    echo "no relix-controller / relix-web-bridge processes were running."
    exit 0
fi

sleep 0.5

for entry in "${STOPPED[@]}"; do
    pid="${entry%%:*}"
    name="${entry##*:}"
    if kill -0 "$pid" 2>/dev/null; then
        kill -9 "$pid" 2>/dev/null || true
        echo "  hard-killed $name pid=$pid"
    else
        echo "  stopped     $name pid=$pid"
    fi
done

echo "mesh down."
