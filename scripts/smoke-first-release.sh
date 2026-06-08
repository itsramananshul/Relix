#!/usr/bin/env bash
#
# scripts/smoke-first-release.sh
#
# POSIX-shell peer of scripts/smoke-first-release.ps1. LIVE first-release
# boot smoke: proves a user can actually START Relix and USE the first flow
# end-to-end, over real HTTP, with NO external model spend:
#
#   1. Build the binaries the mesh needs (relix-cli / -controller / -web-bridge).
#   2. Boot a fully isolated local mesh (reusing scripts/relix-mesh-up.sh,
#      backgrounded) and WAIT for the bridge to become ready (never hangs
#      forever - bounded poll).
#   3. Authenticate the SAME way the dashboard does - a username/password
#      session (HTTP-only relix_session cookie). No manual bridge-token paste.
#   4. Reach the core dashboard APIs through that session cookie WITHOUT 401/502:
#         GET /v1/info                  (bridge info)
#         GET /v1/spine/board           (spine summary)
#         GET /v1/adapters              (Rig adapters list)
#         GET /v1/config/providers      (chat providers list)
#         GET /v1/tasks                 (durable task ledger)
#         GET /v1/cron/jobs             (scheduled jobs)
#         GET /v1/spine/company         (company status)
#      Plus a NEGATIVE control: the same protected route with NO session must
#      be rejected (proves auth is genuinely enforced, not wide open).
#   4b. Prove PROVIDER / CHAT READINESS - the seam the dashboard Chat companion
#      ("Use AI") and Prime "Use AI" ride on. Drive ONE real ai.chat round trip
#      over HTTP (POST /v1/spine/companion {mode:"ai"}) and assert the AI peer
#      ANSWERED. With the mock provider (zero spend) a reachable peer reports
#      ai_mode="fallback"; an UNREACHABLE peer reports ai_mode="unavailable" -
#      that distinction catches the dashboard chat failure class a green board
#      read would otherwise hide.
#   5. (with --require-echo-flow, or best-effort otherwise) run ONE real product
#      flow on the safe local echo Rig (zero model spend):
#         starter-crew (echo) -> create Brief -> assign -> run -> poll runs ->
#         read the Chronicle, and verify a visible terminal result.
#   6. Print a concise PASS/FAIL report and STOP exactly the processes it
#      started (via scripts/relix-mesh-down.sh + the pidfile), restoring the
#      operator's real environment.
#
# Isolation contract (never touches the operator's real state):
#   * Runs under a TEMP $HOME, so the dashboard admin credential
#     (~/.relix/dashboard-admin.json) and bridge token land in a throwaway
#     directory - the operator's real ~/.relix is untouched.
#   * Uses a dedicated --run label + non-default ports, so a real local mesh
#     on the default ports is never disturbed.
#   * Tears down via the pidfile (only the PIDs this run started), then cleans
#     up the per-run config/data/keys it created.
#
# Exit codes: 0 = every REQUIRED step passed; 1 = at least one failed (the
# echo product flow is reported but, being best-effort over the full governed
# path, does not by itself fail the smoke unless --require-echo-flow is set).
#
# Usage:
#   ./scripts/smoke-first-release.sh
#   ./scripts/smoke-first-release.sh --skip-build           # use existing binaries
#   ./scripts/smoke-first-release.sh --require-echo-flow     # echo flow must pass too
#   ./scripts/smoke-first-release.sh --keep-up              # leave the mesh running

set -uo pipefail

# ---- CLI parsing (mirrors the .ps1 params) ----

RUN="smoke-posix"
BRIDGE_PORT=19891
MEM_PORT=19811
AI_PORT=19812
TOOL_PORT=19813
COORDINATOR_PORT=19814
PROVIDER="mock"
BOOT_TIMEOUT_SECS=150
ADMIN_USER="smoke-admin"
ADMIN_PASS="smoke-pass-123"
SKIP_BUILD=0
REQUIRE_ECHO_FLOW=0
KEEP_UP=0

usage() {
    sed -n '2,52p' "$0"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --run)               RUN="$2"; shift 2 ;;
        --bridge-port)       BRIDGE_PORT="$2"; shift 2 ;;
        --mem-port)          MEM_PORT="$2"; shift 2 ;;
        --ai-port)           AI_PORT="$2"; shift 2 ;;
        --tool-port)         TOOL_PORT="$2"; shift 2 ;;
        --coordinator-port)  COORDINATOR_PORT="$2"; shift 2 ;;
        --provider)          PROVIDER="$2"; shift 2 ;;
        --boot-timeout)      BOOT_TIMEOUT_SECS="$2"; shift 2 ;;
        --admin-user)        ADMIN_USER="$2"; shift 2 ;;
        --admin-pass)        ADMIN_PASS="$2"; shift 2 ;;
        --skip-build)        SKIP_BUILD=1; shift ;;
        --require-echo-flow) REQUIRE_ECHO_FLOW=1; shift ;;
        --keep-up)           KEEP_UP=1; shift ;;
        -h|--help)           usage; exit 0 ;;
        *)                   echo "unknown arg: $1" >&2; usage; exit 1 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"
BASE="http://127.0.0.1:$BRIDGE_PORT"

# ---- tiny reporting harness (parallels Record/Info in the .ps1) ----

# Each result is "ok|name|detail"; the summary parses these back out.
RESULTS=()
record() {  # name  ok(0|1)  detail
    local name="$1" ok="$2" detail="$3" tag
    RESULTS+=("$ok|$name|$detail")
    if [[ "$ok" -eq 1 ]]; then tag="PASS"; else tag="FAIL"; fi
    printf '  %-4s %-26s %s\n' "$tag" "$name" "$detail"
}
info() { printf '  ---- %s\n' "$1"; }

# ---- HTTP via curl (does NOT abort on non-2xx; carries cookies) ----
# A shared cookie jar captures the relix_session cookie from setup/login and
# re-sends it on every subsequent request - exactly what the browser dashboard
# does. No Origin header is sent, so the bridge's CSRF guard admits the call.
HTTP_STATUS=""
HTTP_BODY=""
http_call() {  # method  path  [json-body]  [cookie-jar]
    local method="$1" path="$2" body="${3:-}" jar="${4:-$COOKIE_JAR}"
    local tmp status
    tmp="$(mktemp)"
    local args=(-sS -o "$tmp" -w '%{http_code}' -X "$method" --max-time 20)
    # An empty jar file is fine (sends no cookies); a real jar both sends and
    # captures, so the session cookie sticks across calls.
    args+=(-c "$jar" -b "$jar")
    if [[ "$method" == "POST" ]]; then
        [[ -z "$body" ]] && body='{}'
        args+=(-H 'content-type: application/json' --data "$body")
    fi
    status="$(curl "${args[@]}" "$BASE$path" 2>/dev/null || echo 000)"
    HTTP_STATUS="$status"
    HTTP_BODY="$(cat "$tmp")"
    rm -f "$tmp"
}

# ---- minimal JSON field extraction (no jq dependency) ----
# These match the flat shapes the bridge returns for the fields we read; we
# always want the FIRST occurrence (e.g. the first crew member, the latest run).
json_str() {   # key  json  ->  first string value for "key"
    printf '%s' "$2" \
        | grep -oE "\"$1\"[[:space:]]*:[[:space:]]*\"[^\"]*\"" \
        | head -1 | sed -E "s/.*:[[:space:]]*\"([^\"]*)\"/\1/"
}
json_bool() {  # key  json  ->  first true|false for "key"
    printf '%s' "$2" \
        | grep -oE "\"$1\"[[:space:]]*:[[:space:]]*(true|false)" \
        | head -1 | grep -oE '(true|false)$'
}
events_nonempty() {  # json  ->  0 if the array has at least one object
    local compact
    compact="$(printf '%s' "$1" | tr -d '[:space:]')"
    [[ -n "$compact" && "$compact" != "[]" && "$compact" == *"{"* ]]
}

# ---- state captured for cleanup ----
MESH_PID=""
TEMP_HOME=""
MESH_OUT=""
DATA_BASE="dev-data/$RUN"
POLICY_FILE="configs/policies/$RUN.toml"
COOKIE_JAR=""
ANON_JAR=""

remove_run_artifacts() {
    "$SCRIPT_DIR/relix-mesh-down.sh" --run "$RUN" >/dev/null 2>&1 || true
    rm -rf "$DATA_BASE" 2>/dev/null || true
    rm -f "$POLICY_FILE" 2>/dev/null || true
    # dev-keys/$RUN-*  (org root, leaf keys, bundles)
    find dev-keys -maxdepth 1 -name "$RUN-*" -exec rm -f {} + 2>/dev/null || true
}

cleanup() {
    if [[ "$KEEP_UP" -eq 1 ]]; then
        [[ -n "$COOKIE_JAR" ]] && rm -f "$COOKIE_JAR" 2>/dev/null || true
        [[ -n "$ANON_JAR" ]] && rm -f "$ANON_JAR" 2>/dev/null || true
        return
    fi
    echo
    echo "Tearing down ..."
    # Signal the backgrounded mesh-up wrapper: its own EXIT trap kills exactly
    # the controller/bridge PIDs it started. Then mesh-down is a belt-and-
    # suspenders sweep of the recorded pidfile (idempotent if already gone).
    if [[ -n "$MESH_PID" ]] && kill -0 "$MESH_PID" 2>/dev/null; then
        kill "$MESH_PID" 2>/dev/null || true
        # Give the wrapper's trap a moment to reap its children.
        local waited=0
        while kill -0 "$MESH_PID" 2>/dev/null && [[ "$waited" -lt 10 ]]; do
            sleep 0.5; waited=$((waited + 1))
        done
        kill -9 "$MESH_PID" 2>/dev/null || true
    fi
    "$SCRIPT_DIR/relix-mesh-down.sh" --run "$RUN" >/dev/null 2>&1 || true
    [[ -n "$COOKIE_JAR" ]] && rm -f "$COOKIE_JAR" 2>/dev/null || true
    [[ -n "$ANON_JAR" ]] && rm -f "$ANON_JAR" 2>/dev/null || true
    [[ -n "$MESH_OUT" ]] && rm -f "$MESH_OUT" 2>/dev/null || true
    rm -rf "$DATA_BASE" 2>/dev/null || true
    rm -f "$POLICY_FILE" 2>/dev/null || true
    find dev-keys -maxdepth 1 -name "$RUN-*" -exec rm -f {} + 2>/dev/null || true
    [[ -n "$TEMP_HOME" && -d "$TEMP_HOME" ]] && rm -rf "$TEMP_HOME" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo
echo "== Relix first-release boot smoke =="
echo "  run label:   $RUN"
echo "  bridge:      $BASE"
echo "  provider:    $PROVIDER (echo Rig used for the product flow - no model spend)"
echo

# ---- 0) build (required behavior #1) ----
if [[ "$SKIP_BUILD" -eq 0 ]]; then
    echo "Building binaries (relix-cli, relix-controller, relix-web-bridge) ..."
    if cargo build -p relix-cli -p relix-controller -p relix-web-bridge; then
        record 'build' 1 'cargo build -p relix-cli -p relix-controller -p relix-web-bridge'
    else
        record 'build' 0 'cargo build failed'
        echo "RESULT: FAIL (build)"; exit 1
    fi
else
    info 'build skipped (--skip-build)'
fi

# ---- isolate: temp HOME so ~/.relix is a throwaway dir ----
TEMP_HOME="$(mktemp -d "${TMPDIR:-/tmp}/relix-smoke-XXXXXXXX")"
export HOME="$TEMP_HOME"
# XDG/USERPROFILE kept in lockstep so any platform's home-dir resolver lands in
# the throwaway dir too.
export USERPROFILE="$TEMP_HOME"
info "isolated home: $TEMP_HOME"

COOKIE_JAR="$(mktemp)"
ANON_JAR="$(mktemp)"

remove_run_artifacts

# ---- 1) boot the mesh in the background (reuses relix-mesh-up.sh) ----
echo
echo "Booting isolated mesh ..."
MESH_OUT="$(mktemp)"
"$SCRIPT_DIR/relix-mesh-up.sh" \
    --provider "$PROVIDER" --run "$RUN" \
    --bridge-port "$BRIDGE_PORT" --mem-port "$MEM_PORT" --ai-port "$AI_PORT" \
    --tool-port "$TOOL_PORT" --coordinator-port "$COORDINATOR_PORT" \
    >"$MESH_OUT" 2>&1 &
MESH_PID=$!

# ---- 2) wait for readiness (bounded - never hang forever) ----
deadline=$(( $(date +%s) + BOOT_TIMEOUT_SECS ))
ready=0
while [[ "$(date +%s)" -lt "$deadline" ]]; do
    if ! kill -0 "$MESH_PID" 2>/dev/null; then
        info "mesh wrapper exited early; output tail:"
        tail -n 25 "$MESH_OUT" | sed 's/^/    /'
        break
    fi
    http_call GET /health
    if [[ "$HTTP_STATUS" == "200" ]]; then ready=1; break; fi
    sleep 0.75
done
if [[ "$ready" -ne 1 ]]; then
    record 'boot.ready' 0 "bridge /health not 200 within ${BOOT_TIMEOUT_SECS}s"
    blog="$DATA_BASE/bridge.err.log"
    if [[ -f "$blog" ]]; then info 'bridge.err.log tail:'; tail -n 25 "$blog" | sed 's/^/    /'; fi
    echo "RESULT: FAIL (mesh did not become ready)"; exit 1
fi
record 'boot.ready' 1 "$BASE/health responded 200"

# ---- 3) dashboard SESSION auth - the path the SPA uses ----
echo
echo "Authenticating via the dashboard session path ..."

http_call GET /v1/auth/status
needs_setup="false"
if [[ "$HTTP_STATUS" == "200" ]]; then
    nb="$(json_bool needs_setup "$HTTP_BODY")"
    [[ -n "$nb" ]] && needs_setup="$nb"
fi
if [[ "$HTTP_STATUS" == "200" ]]; then aok=1; else aok=0; fi
record 'auth.status' "$aok" "/v1/auth/status -> $HTTP_STATUS (needs_setup=$needs_setup)"

# First run on a fresh temp home => setup creates the admin AND logs in (sets
# the cookie). If an admin somehow already exists, fall back to login.
creds="{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASS\"}"
if [[ "$needs_setup" == "true" ]]; then
    http_call POST /v1/auth/setup "$creds"
    if [[ "$HTTP_STATUS" == "200" ]]; then ok=1; else ok=0; fi
    record 'auth.setup' "$ok" "POST /v1/auth/setup -> $HTTP_STATUS (creates admin + session, no token paste)"
else
    http_call POST /v1/auth/login "$creds"
    if [[ "$HTTP_STATUS" == "200" ]]; then ok=1; else ok=0; fi
    record 'auth.login' "$ok" "POST /v1/auth/login -> $HTTP_STATUS"
fi

http_call GET /v1/auth/me
if [[ "$HTTP_STATUS" == "200" ]]; then ok=1; else ok=0; fi
record 'auth.me' "$ok" "GET /v1/auth/me -> $HTTP_STATUS (session cookie carried automatically)"

# Negative control: a SEPARATE client with NO session must be rejected.
http_call GET /v1/adapters "" "$ANON_JAR"
if [[ "$HTTP_STATUS" == "401" || "$HTTP_STATUS" == "403" ]]; then ok=1; else ok=0; fi
record 'auth.enforced' "$ok" "GET /v1/adapters with no session -> $HTTP_STATUS (auth enforced)"

# ---- 4) core dashboard APIs through the session (no 401/502) ----
echo
echo "Reaching core dashboard APIs through the session cookie ..."
core_names=(api.info api.spine api.adapters api.providers api.tasks api.cron api.company)
core_paths=(/v1/info /v1/spine/board /v1/adapters /v1/config/providers /v1/tasks /v1/cron/jobs /v1/spine/company)
for i in "${!core_names[@]}"; do
    http_call GET "${core_paths[$i]}"
    if [[ "$HTTP_STATUS" -ge 200 && "$HTTP_STATUS" -lt 300 ]]; then ok=1; else ok=0; fi
    record "${core_names[$i]}" "$ok" "GET ${core_paths[$i]} -> $HTTP_STATUS"
done

# ---- 4b) provider / chat readiness - the dashboard chat companion seam ----
# The core-read checks prove the board APIs answer and the echo flow proves the
# Rig path - but NEITHER exercises the AI provider seam the dashboard's Chat
# companion ("Use AI") and Prime "Use AI" both ride on (relix-dashboard-design
# SS13). A broken / unreachable AI peer leaves every read green while the chat
# surface dies with 502 / "ai peer unreachable", so we drive ONE real ai.chat
# round trip over HTTP and assert the AI peer ANSWERED. With the safe mock
# provider (zero model spend) a reachable peer yields ai_mode="fallback"
# (answered, choice unusable); an UNREACHABLE peer yields ai_mode="unavailable".
# That distinction is the readiness signal. A bounded retry tolerates the AI
# node coming up a beat after the bridge.
echo
echo "Proving provider / chat readiness (mock ai.chat round trip, no model spend) ..."
ai_mode=""
ai_reason=""
chat_status=0
cdeadline=$(( $(date +%s) + 20 ))
while [[ "$(date +%s)" -lt "$cdeadline" ]]; do
    http_call POST /v1/spine/companion '{"message":"what needs attention","mode":"ai"}'
    chat_status="$HTTP_STATUS"
    if [[ "$HTTP_STATUS" -ge 200 && "$HTTP_STATUS" -lt 300 ]]; then
        ai_mode="$(json_str ai_mode "$HTTP_BODY")"
        ai_reason="$(json_str ai_reason "$HTTP_BODY")"
        if [[ "$ai_mode" == "fallback" || "$ai_mode" == "llm_used" ]]; then break; fi
    fi
    sleep 0.75
done
if [[ "$chat_status" -ge 200 && "$chat_status" -lt 300 ]]; then ok=1; else ok=0; fi
record 'chat.companion' "$ok" "POST /v1/spine/companion {mode:ai} -> $chat_status (chat companion reachable)"

# The AI peer answered iff ai_mode is a model-answered verdict. "unavailable"
# (or an empty body / a 5xx above) means the provider/chat seam is NOT ready.
if [[ "$ai_mode" == "fallback" || "$ai_mode" == "llm_used" ]]; then chat_ready=1; else chat_ready=0; fi
rdetail="ai_mode=${ai_mode:-(none)}"
if [[ "$chat_ready" -ne 1 && -n "$ai_reason" ]]; then rdetail="$rdetail reason: $ai_reason"; fi
record 'chat.provider_ready' "$chat_ready" "$rdetail (AI peer answered ai.chat; not 'unavailable')"

# ---- 5) one real product flow on the safe local echo Rig ----
echo
echo "Running the echo product flow (no external model spend) ..."
echo_ok=1

# 5a) starter crew on echo -> returns operative agent_ids.
http_call POST /v1/spine/company/starter-crew '{"rig":"echo"}'
assignee=""
if [[ "$HTTP_STATUS" == "200" ]]; then assignee="$(json_str agent_id "$HTTP_BODY")"; fi
if [[ "$HTTP_STATUS" == "200" && -n "$assignee" ]]; then ok=1; else ok=0; echo_ok=0; fi
record 'echo.crew' "$ok" "POST /v1/spine/company/starter-crew {rig:echo} -> $HTTP_STATUS (assignee=$assignee)"

# 5b) create a Brief assigned to the echo operative.
brief_id=""
if [[ -n "$assignee" ]]; then
    ts="$(date +%Y-%m-%dT%H:%M:%S)"
    http_call POST /v1/spine/briefs "{\"title\":\"first-release smoke $ts\",\"assignee\":\"$assignee\"}"
    if [[ "$HTTP_STATUS" == "200" ]]; then brief_id="$(json_str task_id "$HTTP_BODY")"; fi
    if [[ "$HTTP_STATUS" == "200" && -n "$brief_id" ]]; then ok=1; else ok=0; echo_ok=0; fi
    record 'echo.brief' "$ok" "POST /v1/spine/briefs -> $HTTP_STATUS (brief=$brief_id)"
fi

# 5c) run the Brief through the echo Rig (forced override).
if [[ -n "$brief_id" ]]; then
    http_call POST "/v1/spine/briefs/$brief_id/run" '{"rig":"echo"}'
    run_status=""
    if [[ "$HTTP_STATUS" == "200" ]]; then run_status="$(json_str status "$HTTP_BODY")"; fi
    if [[ "$HTTP_STATUS" == "200" ]]; then ok=1; else ok=0; echo_ok=0; fi
    record 'echo.run' "$ok" "POST /v1/spine/briefs/$brief_id/run {rig:echo} -> $HTTP_STATUS (status=$run_status)"

    # 5d) poll the run ledger until the Shift reaches a terminal state.
    final_status="$run_status"
    rdeadline=$(( $(date +%s) + 30 ))
    while [[ "$(date +%s)" -lt "$rdeadline" ]]; do
        http_call GET "/v1/spine/briefs/$brief_id/runs"
        if [[ "$HTTP_STATUS" == "200" ]]; then
            latest="$(json_str status "$HTTP_BODY")"
            if [[ -n "$latest" ]]; then
                final_status="$latest"
                case "$final_status" in
                    done|failed|refused|continued) break ;;
                esac
            fi
        fi
        sleep 0.75
    done
    if [[ "$final_status" == "done" ]]; then ok=1; else ok=0; echo_ok=0; fi
    record 'echo.terminal' "$ok" "run reached terminal state: $final_status"

    # 5e) the Chronicle records the run.
    http_call GET "/v1/spine/briefs/$brief_id/events"
    if [[ "$HTTP_STATUS" == "200" ]] && events_nonempty "$HTTP_BODY"; then ok=1; else ok=0; echo_ok=0; fi
    record 'echo.chronicle' "$ok" "GET /v1/spine/briefs/$brief_id/events -> $HTTP_STATUS (events present)"
fi

if [[ "$echo_ok" -ne 1 ]]; then info 'echo product flow had a non-PASS step (see above)'; fi

if [[ "$KEEP_UP" -eq 1 ]]; then
    echo
    echo "Mesh left running (--keep-up). Dashboard: $BASE/dashboard  (login: $ADMIN_USER / $ADMIN_PASS)"
    echo "Stop it with: ./scripts/relix-mesh-down.sh --run $RUN"
fi

# ---- summary (cleanup runs via the EXIT trap) ----
echo
req_fail=0
echo_fail=0
total=0
pass=0
for r in "${RESULTS[@]}"; do
    ok="${r%%|*}"
    rest="${r#*|}"
    name="${rest%%|*}"
    total=$((total + 1))
    [[ "$ok" -eq 1 ]] && pass=$((pass + 1))
    case "$name" in
        build) ;;  # informational, not a required gate
        echo.*) [[ "$ok" -ne 1 ]] && echo_fail=$((echo_fail + 1)) ;;
        *)      [[ "$ok" -ne 1 ]] && req_fail=$((req_fail + 1)) ;;
    esac
done

echo "first-release smoke: $pass/$total checks passed"
if [[ "$echo_fail" -gt 0 ]]; then
    echo "  echo product flow: $echo_fail step(s) not PASS"
fi

fail=$req_fail
[[ "$REQUIRE_ECHO_FLOW" -eq 1 ]] && fail=$((fail + echo_fail))

if [[ "$fail" -eq 0 ]]; then
    echo "RESULT: PASS"
    exit 0
else
    echo "RESULT: FAIL ($fail required check(s) failed)"
    exit 1
fi
