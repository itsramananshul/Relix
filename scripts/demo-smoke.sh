#!/usr/bin/env bash
# scripts/demo-smoke.sh
#
# W2-008b — operator smoke test for an already-running Relix
# mesh (started via `./scripts/relix-mesh-up.sh`). Exercises
# the canonical end-to-end path + every Wave 2 observability
# surface, so operators can answer "is the mesh actually
# working?" in 5 seconds without opening the dashboard.
#
# Steps:
#   1. Bridge liveness  (GET /health)
#   2. Topology snapshot (GET /v1/topology) — count peers
#   3. Chat round-trip  (POST /v1/chat/completions) —
#      exercises the AI / memory peers + DispatchBridge
#   4. Dispatch stats   (GET /v1/dispatch/stats)
#   5. Policy denials   (GET /v1/policy/denials)
#
# Exit codes:
#   0 — every step returned 2xx
#   1 — at least one step failed (bridge down, decode error,
#       non-2xx HTTP, etc.). Stderr names the failing step.
#
# Usage:
#   ./scripts/demo-smoke.sh                       # default port 19791
#   ./scripts/demo-smoke.sh --bridge http://127.0.0.1:19800
#   ./scripts/demo-smoke.sh --provider relix-openai

set -euo pipefail

BRIDGE="http://127.0.0.1:19791"
PROVIDER_MODEL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bridge)   BRIDGE=$2; shift 2 ;;
        --provider) PROVIDER_MODEL=$2; shift 2 ;;
        -h|--help)
            sed -n '2,28p' "$0"
            exit 0 ;;
        *)
            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

red()    { printf '\033[31m%s\033[0m\n' "$*"; }
green()  { printf '\033[32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[33m%s\033[0m\n' "$*"; }
bold()   { printf '\033[1m%s\033[0m\n' "$*"; }

# Capture HTTP status + body separately so we can fail fast
# on a non-2xx without losing the bridge's error JSON.
curl_status_body() {
    local url=$1
    shift
    local body status
    body=$(mktemp)
    status=$(curl -s -o "$body" -w "%{http_code}" "$@" "$url" || echo "000")
    printf '%s\n' "$status"
    cat "$body"
    rm -f "$body"
}

step=0
fail=0
ok_check() {
    step=$((step + 1))
    local desc=$1 status=$2 body=$3
    if [[ "$status" =~ ^2 ]]; then
        green "  step $step OK ($status) — $desc"
    else
        red   "  step $step FAIL ($status) — $desc"
        echo "$body" | head -5 >&2
        fail=$((fail + 1))
    fi
}

bold "Relix smoke (bridge=$BRIDGE)"

# ── 1. liveness ─────────────────────────────────────────────
resp=$(curl_status_body "$BRIDGE/health")
status=$(echo "$resp" | head -1)
body=$(echo "$resp" | tail -n +2)
ok_check "GET /health" "$status" "$body"

# ── 2. topology ─────────────────────────────────────────────
resp=$(curl_status_body "$BRIDGE/v1/topology")
status=$(echo "$resp" | head -1)
body=$(echo "$resp" | tail -n +2)
ok_check "GET /v1/topology" "$status" "$body"
if [[ "$status" =~ ^2 ]]; then
    peer_count=$(echo "$body" | grep -oE '"alias":' | wc -l | tr -d ' ')
    echo "         peers discovered: $peer_count"
fi

# ── 3. chat completion ──────────────────────────────────────
# Pick a model — operator override > pretty-printed default.
if [[ -z "$PROVIDER_MODEL" ]]; then
    # The relix-mesh-up.sh script sets up `relix-mock` /
    # `relix-openai` / `relix-anthropic` etc. depending on
    # the active provider. The mock provider always works
    # because it doesn't need an API key.
    PROVIDER_MODEL="relix-mock"
fi
chat_body=$(cat <<JSON
{"model":"$PROVIDER_MODEL","messages":[{"role":"user","content":"smoke test ping"}]}
JSON
)
resp=$(curl_status_body "$BRIDGE/v1/chat/completions" \
    -H "content-type: application/json" -d "$chat_body")
status=$(echo "$resp" | head -1)
body=$(echo "$resp" | tail -n +2)
ok_check "POST /v1/chat/completions (model=$PROVIDER_MODEL)" "$status" "$body"

# ── 4. dispatch stats ───────────────────────────────────────
resp=$(curl_status_body "$BRIDGE/v1/dispatch/stats?peer=tool")
status=$(echo "$resp" | head -1)
body=$(echo "$resp" | tail -n +2)
ok_check "GET /v1/dispatch/stats?peer=tool (W2-006c)" "$status" "$body"

# ── 5. policy denials ───────────────────────────────────────
resp=$(curl_status_body "$BRIDGE/v1/policy/denials?peer=tool&max=10")
status=$(echo "$resp" | head -1)
body=$(echo "$resp" | tail -n +2)
ok_check "GET /v1/policy/denials?peer=tool (W2-007e)" "$status" "$body"
if [[ "$status" =~ ^2 ]]; then
    denial_count=$(echo "$body" | grep -oE '"count":[0-9]+' | head -1 | sed 's/[^0-9]//g')
    if [[ "${denial_count:-0}" -gt 0 ]]; then
        yellow "         ⚠  ${denial_count} recent denial(s) on tool — investigate via dashboard or:"
        echo   "         relix-cli ops policy-denials --peer tool"
    else
        echo "         denial ring empty on tool"
    fi
fi

# ── summary ─────────────────────────────────────────────────
echo
if [[ "$fail" -eq 0 ]]; then
    green "smoke PASS — $step/$step steps OK"
    exit 0
else
    red "smoke FAIL — $fail/$step step(s) failed"
    exit 1
fi
