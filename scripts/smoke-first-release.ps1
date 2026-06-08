# scripts/smoke-first-release.ps1
#
# LIVE first-release boot smoke. Proves a user can actually START Relix and
# USE the first flow end-to-end, over real HTTP, with NO external model spend:
#
#   1. Build the binaries the mesh needs (relix-cli / -controller / -web-bridge).
#   2. Boot a fully isolated local mesh (reusing scripts/relix-mesh-up.ps1) and
#      WAIT for the bridge to become ready (never hangs forever - bounded poll).
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
#   5. Run ONE real product flow on the safe local echo Rig (zero model spend):
#         starter-crew (echo) -> create Brief -> assign -> run -> poll runs ->
#         read the Chronicle, and verify a visible terminal result.
#   6. Print a concise PASS/FAIL report and STOP exactly the processes it
#      started (via scripts/relix-mesh-down.ps1 + the pidfile), restoring the
#      operator's real environment.
#
# Isolation contract (never touches the operator's real state):
#   * Runs under a TEMP %USERPROFILE%, so the dashboard admin credential
#     (~/.relix/dashboard-admin.json) and bridge token land in a throwaway
#     directory - the operator's real ~/.relix is untouched.
#   * Uses a dedicated -Run label + non-default ports, so a real local mesh
#     on the default ports is never disturbed.
#   * Tears down via the pidfile (only the PIDs this run started), then cleans
#     up the per-run config/data/keys it created.
#
# Exit codes: 0 = every REQUIRED step passed; 1 = at least one failed (the
# echo product flow is reported but, being best-effort over the full governed
# path, does not by itself fail the smoke unless -RequireEchoFlow is set).
#
# Usage:
#   .\scripts\smoke-first-release.ps1
#   .\scripts\smoke-first-release.ps1 -SkipBuild           # use existing binaries
#   .\scripts\smoke-first-release.ps1 -RequireEchoFlow     # echo flow must pass too
#   .\scripts\smoke-first-release.ps1 -KeepUp              # leave the mesh running

[CmdletBinding()]
param(
    [string]$Run         = 'smoke',
    [int]$BridgePort     = 19891,
    [int]$MemPort        = 19811,
    [int]$AiPort         = 19812,
    [int]$ToolPort       = 19813,
    [int]$CoordinatorPort = 19814,
    [ValidateSet('mock','openai','openrouter','xai','anthropic','gemini','local')]
    [string]$Provider    = 'mock',
    [int]$BootTimeoutSecs = 150,
    [string]$AdminUser   = 'smoke-admin',
    [string]$AdminPass   = 'smoke-pass-123',
    [switch]$SkipBuild,
    [switch]$RequireEchoFlow,
    [switch]$KeepUp
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$Root = (Get-Location).Path
$Base = "http://127.0.0.1:$BridgePort"

# System.Net.Http is not auto-loaded in Windows PowerShell 5.1.
Add-Type -AssemblyName System.Net.Http -ErrorAction SilentlyContinue

# -- tiny reporting harness ------------------------------------------
$script:Results = New-Object System.Collections.ArrayList
function Record([string]$name, [bool]$ok, [string]$detail) {
    [void]$script:Results.Add([pscustomobject]@{ Name = $name; Ok = $ok; Detail = $detail })
    $tag = if ($ok) { 'PASS' } else { 'FAIL' }
    $color = if ($ok) { 'Green' } else { 'Red' }
    Write-Host ("  {0,-4} {1,-26} {2}" -f $tag, $name, $detail) -ForegroundColor $color
}
function Info([string]$msg) { Write-Host "  ---- $msg" -ForegroundColor DarkGray }

# -- HTTP via HttpClient (does NOT throw on non-2xx; carries cookies) -
# A shared CookieContainer captures the relix_session cookie from setup/login
# and re-sends it on every subsequent request to the bridge - exactly what the
# browser dashboard does. No Origin header is sent, so the bridge's CSRF guard
# admits the call (same as curl).
function New-HttpClient {
    $handler = New-Object System.Net.Http.HttpClientHandler
    $handler.CookieContainer = New-Object System.Net.CookieContainer
    $handler.UseCookies = $true
    $handler.AllowAutoRedirect = $false
    $client = New-Object System.Net.Http.HttpClient($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(20)
    return $client
}

function Invoke-Http {
    param(
        [Parameter(Mandatory)] [System.Net.Http.HttpClient]$Client,
        [Parameter(Mandatory)] [ValidateSet('GET','POST')] [string]$Method,
        [Parameter(Mandatory)] [string]$Path,
        [string]$JsonBody
    )
    try {
        if ($Method -eq 'GET') {
            $resp = $Client.GetAsync("$Base$Path").GetAwaiter().GetResult()
        } else {
            $payload = if ($null -ne $JsonBody) { $JsonBody } else { '{}' }
            $content = New-Object System.Net.Http.StringContent($payload, [System.Text.Encoding]::UTF8, 'application/json')
            $resp = $Client.PostAsync("$Base$Path", $content).GetAwaiter().GetResult()
        }
        $body = $resp.Content.ReadAsStringAsync().GetAwaiter().GetResult()
        return [pscustomobject]@{ Status = [int]$resp.StatusCode; Body = $body }
    } catch {
        # Transport failure (connection refused, timeout). Status 0 = no reply.
        return [pscustomobject]@{ Status = 0; Body = $_.Exception.Message }
    }
}

# -- pre-clean any leftovers from a prior run (idempotent) -----------
$DataBase = "dev-data/$Run"
$PolicyFile = "configs/policies/$Run.toml"
function Remove-RunArtifacts {
    & (Join-Path $PSScriptRoot 'relix-mesh-down.ps1') -Run $Run *> $null
    Remove-Item -Recurse -Force -LiteralPath $DataBase -ErrorAction SilentlyContinue
    Remove-Item -Force -LiteralPath $PolicyFile -ErrorAction SilentlyContinue
    Get-ChildItem -Path 'dev-keys' -Filter "$Run-*" -ErrorAction SilentlyContinue |
        Remove-Item -Force -ErrorAction SilentlyContinue
}

# State captured for the finally block.
$job = $null
$TempHome = $null
$OldUserProfile = $env:USERPROFILE
$OldHome = $env:HOME

Write-Host ""
Write-Host "== Relix first-release boot smoke ==" -ForegroundColor Cyan
Write-Host "  run label:   $Run"
Write-Host "  bridge:      $Base"
Write-Host "  provider:    $Provider (echo Rig used for the product flow - no model spend)"
Write-Host ""

try {
    # -- 0) build (required behavior #1) -----------------------------
    if (-not $SkipBuild) {
        Write-Host "Building binaries (relix-cli, relix-controller, relix-web-bridge) ..." -ForegroundColor Cyan
        & cargo build -p relix-cli -p relix-controller -p relix-web-bridge
        if ($LASTEXITCODE -ne 0) { Record 'build' $false 'cargo build failed'; throw 'build failed' }
        Record 'build' $true 'cargo build -p relix-cli -p relix-controller -p relix-web-bridge'
    } else {
        Info 'build skipped (-SkipBuild)'
    }

    # -- isolate: temp USERPROFILE so ~/.relix is a throwaway dir -----
    $TempHome = Join-Path ([System.IO.Path]::GetTempPath()) ("relix-smoke-" + [System.Guid]::NewGuid().ToString('N').Substring(0,8))
    New-Item -ItemType Directory -Force -Path $TempHome | Out-Null
    $env:USERPROFILE = $TempHome
    $env:HOME = $TempHome
    Info "isolated home: $TempHome"

    Remove-RunArtifacts

    # -- 1) boot the mesh in the background (reuses relix-mesh-up.ps1) -
    Write-Host ""
    Write-Host "Booting isolated mesh ..." -ForegroundColor Cyan
    $job = Start-Job -Name "relix-smoke-$Run" -ScriptBlock {
        param($root, $homeDir, $run, $bp, $mp, $ap, $tp, $cp, $provider)
        Set-Location $root
        $env:USERPROFILE = $homeDir
        $env:HOME = $homeDir
        & (Join-Path $root 'scripts/relix-mesh-up.ps1') `
            -Provider $provider -Run $run `
            -BridgePort $bp -MemPort $mp -AiPort $ap -ToolPort $tp -CoordinatorPort $cp
    } -ArgumentList $Root, $TempHome, $Run, $BridgePort, $MemPort, $AiPort, $ToolPort, $CoordinatorPort, $Provider

    # -- 2) wait for readiness (bounded - never hang forever) --------
    $probe = New-HttpClient
    $deadline = (Get-Date).AddSeconds($BootTimeoutSecs)
    $ready = $false
    while ((Get-Date) -lt $deadline) {
        if ($job.State -in 'Failed','Completed','Stopped') {
            Info "mesh job ended early (state=$($job.State)); output:"
            Receive-Job -Job $job 2>&1 | Select-Object -Last 25 | ForEach-Object { Write-Host "    $_" }
            break
        }
        $h = Invoke-Http -Client $probe -Method GET -Path '/health'
        if ($h.Status -eq 200) { $ready = $true; break }
        Start-Sleep -Milliseconds 750
    }
    if (-not $ready) {
        Record 'boot.ready' $false "bridge /health not 200 within ${BootTimeoutSecs}s"
        $blog = Join-Path $DataBase 'bridge.err.log'
        if (Test-Path $blog) { Info 'bridge.err.log tail:'; Get-Content $blog -Tail 25 | ForEach-Object { Write-Host "    $_" } }
        throw 'mesh did not become ready'
    }
    Record 'boot.ready' $true "$Base/health responded 200"

    # -- 3) dashboard SESSION auth - the path the SPA uses -----------
    Write-Host ""
    Write-Host "Authenticating via the dashboard session path ..." -ForegroundColor Cyan
    $client = New-HttpClient

    $st = Invoke-Http -Client $client -Method GET -Path '/v1/auth/status'
    $needsSetup = $false
    if ($st.Status -eq 200) {
        try { $needsSetup = ([bool](($st.Body | ConvertFrom-Json).needs_setup)) } catch {}
    }
    Record 'auth.status' ($st.Status -eq 200) "/v1/auth/status -> $($st.Status) (needs_setup=$needsSetup)"

    # First run on a fresh temp home => setup creates the admin AND logs in
    # (sets the cookie). If an admin somehow already exists, fall back to login.
    $creds = (@{ username = $AdminUser; password = $AdminPass } | ConvertTo-Json -Compress)
    if ($needsSetup) {
        $login = Invoke-Http -Client $client -Method POST -Path '/v1/auth/setup' -JsonBody $creds
        Record 'auth.setup' ($login.Status -eq 200) "POST /v1/auth/setup -> $($login.Status) (creates admin + session, no token paste)"
    } else {
        $login = Invoke-Http -Client $client -Method POST -Path '/v1/auth/login' -JsonBody $creds
        Record 'auth.login' ($login.Status -eq 200) "POST /v1/auth/login -> $($login.Status)"
    }

    $me = Invoke-Http -Client $client -Method GET -Path '/v1/auth/me'
    Record 'auth.me' ($me.Status -eq 200) "GET /v1/auth/me -> $($me.Status) (session cookie carried automatically)"

    # Negative control: a SEPARATE client with NO session must be rejected.
    $anon = New-HttpClient
    $denied = Invoke-Http -Client $anon -Method GET -Path '/v1/adapters'
    Record 'auth.enforced' ($denied.Status -eq 401 -or $denied.Status -eq 403) "GET /v1/adapters with no session -> $($denied.Status) (auth enforced)"

    # -- 4) core dashboard APIs through the session (no 401/502) -----
    Write-Host ""
    Write-Host "Reaching core dashboard APIs through the session cookie ..." -ForegroundColor Cyan
    $endpoints = @(
        @{ Name = 'api.info';       Path = '/v1/info' }
        @{ Name = 'api.spine';      Path = '/v1/spine/board' }
        @{ Name = 'api.adapters';   Path = '/v1/adapters' }
        @{ Name = 'api.providers';  Path = '/v1/config/providers' }
        @{ Name = 'api.tasks';      Path = '/v1/tasks' }
        @{ Name = 'api.cron';       Path = '/v1/cron/jobs' }
        @{ Name = 'api.company';    Path = '/v1/spine/company' }
    )
    foreach ($e in $endpoints) {
        $r = Invoke-Http -Client $client -Method GET -Path $e.Path
        $ok = ($r.Status -ge 200 -and $r.Status -lt 300)
        Record $e.Name $ok ("GET {0} -> {1}" -f $e.Path, $r.Status)
    }

    # -- 4b) provider / chat readiness - the dashboard chat companion seam --
    # The core-read checks above prove the board APIs answer, and the echo flow
    # below proves the Rig path - but NEITHER exercises the AI provider seam the
    # dashboard's Chat companion ("Use AI") and Prime "Use AI" both ride on
    # (relix-dashboard-design.md SS13). A broken / unreachable AI peer leaves
    # every read green while the chat surface dies with 502 / "ai peer
    # unreachable", so we drive ONE real ai.chat round trip over HTTP and assert
    # the AI peer ANSWERED.
    #
    # With the safe mock provider (zero model spend) a reachable AI peer returns
    # a deterministic reply that does not validate as an action, so the companion
    # honestly reports ai_mode="fallback" (model answered, choice unusable). An
    # UNREACHABLE AI peer instead reports ai_mode="unavailable". That distinction
    # IS the readiness signal: "fallback"/"llm_used" = the provider/chat seam is
    # live; "unavailable" (or a 5xx) = the dashboard chat failure class. A bounded
    # retry tolerates the AI node coming up a beat after the bridge.
    Write-Host ""
    Write-Host "Proving provider / chat readiness (mock ai.chat round trip, no model spend) ..." -ForegroundColor Cyan
    $chatBody = @{ message = 'what needs attention'; mode = 'ai' } | ConvertTo-Json -Compress
    $aiMode = ''
    $aiReason = ''
    $chatStatus = 0
    $cdeadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $cdeadline) {
        $chat = Invoke-Http -Client $client -Method POST -Path '/v1/spine/companion' -JsonBody $chatBody
        $chatStatus = $chat.Status
        if ($chat.Status -ge 200 -and $chat.Status -lt 300) {
            try {
                $parsed = $chat.Body | ConvertFrom-Json
                $aiMode = if ($parsed.ai_mode) { [string]$parsed.ai_mode } else { '' }
                $aiReason = if ($parsed.ai_reason) { [string]$parsed.ai_reason } else { '' }
            } catch { $aiMode = ''; $aiReason = '' }
            if ($aiMode -eq 'fallback' -or $aiMode -eq 'llm_used') { break }
        }
        Start-Sleep -Milliseconds 750
    }
    $chatOk = ($chatStatus -ge 200 -and $chatStatus -lt 300)
    Record 'chat.companion' $chatOk "POST /v1/spine/companion {mode:ai} -> $chatStatus (chat companion reachable)"

    # The AI peer answered iff ai_mode is a model-answered verdict. "unavailable"
    # (or an empty body / a 5xx above) means the provider/chat seam is NOT ready.
    $providerReady = ($aiMode -eq 'fallback' -or $aiMode -eq 'llm_used')
    $rdetail = "ai_mode=$(if ($aiMode) { $aiMode } else { '(none)' })"
    if (-not $providerReady -and $aiReason) { $rdetail += " reason: $aiReason" }
    Record 'chat.provider_ready' $providerReady "$rdetail (AI peer answered ai.chat; not 'unavailable')"

    # -- 5) one real product flow on the safe local echo Rig ---------
    Write-Host ""
    Write-Host "Running the echo product flow (no external model spend) ..." -ForegroundColor Cyan
    $echoOk = $true

    # 5a) starter crew on echo -> returns operative agent_ids.
    $crew = Invoke-Http -Client $client -Method POST -Path '/v1/spine/company/starter-crew' -JsonBody (@{ rig = 'echo' } | ConvertTo-Json -Compress)
    $assignee = $null
    if ($crew.Status -eq 200) {
        try { $assignee = (($crew.Body | ConvertFrom-Json).crew | Select-Object -First 1).agent_id } catch {}
    }
    $crewOk = ($crew.Status -eq 200 -and $assignee)
    Record 'echo.crew' $crewOk "POST /v1/spine/company/starter-crew {rig:echo} -> $($crew.Status) (assignee=$assignee)"
    if (-not $crewOk) { $echoOk = $false }

    # 5b) create a Brief assigned to the echo operative.
    $briefId = $null
    if ($assignee) {
        $ts = (Get-Date).ToString('s')
        $body = @{ title = "first-release smoke $ts"; assignee = $assignee } | ConvertTo-Json -Compress
        $cb = Invoke-Http -Client $client -Method POST -Path '/v1/spine/briefs' -JsonBody $body
        if ($cb.Status -eq 200) { try { $briefId = ($cb.Body | ConvertFrom-Json).task_id } catch {} }
        $cbOk = ($cb.Status -eq 200 -and $briefId)
        Record 'echo.brief' $cbOk "POST /v1/spine/briefs -> $($cb.Status) (brief=$briefId)"
        if (-not $cbOk) { $echoOk = $false }
    }

    # 5c) run the Brief through the echo Rig (forced override).
    if ($briefId) {
        $rb = Invoke-Http -Client $client -Method POST -Path "/v1/spine/briefs/$briefId/run" -JsonBody (@{ rig = 'echo' } | ConvertTo-Json -Compress)
        $runStatus = ''
        if ($rb.Status -eq 200) { try { $runStatus = ($rb.Body | ConvertFrom-Json).status } catch {} }
        Record 'echo.run' ($rb.Status -eq 200) "POST /v1/spine/briefs/$briefId/run {rig:echo} -> $($rb.Status) (status=$runStatus)"
        if ($rb.Status -ne 200) { $echoOk = $false }

        # 5d) poll the run ledger until the Shift reaches a terminal state.
        $terminal = @('done','failed','refused','continued')
        $rdeadline = (Get-Date).AddSeconds(30)
        $finalStatus = $runStatus
        while ((Get-Date) -lt $rdeadline) {
            $runs = Invoke-Http -Client $client -Method GET -Path "/v1/spine/briefs/$briefId/runs"
            if ($runs.Status -eq 200) {
                try {
                    $arr = $runs.Body | ConvertFrom-Json
                    $latest = $arr | Select-Object -First 1
                    if ($latest -and $latest.status) {
                        $finalStatus = $latest.status
                        if ($terminal -contains $finalStatus) { break }
                    }
                } catch {}
            }
            Start-Sleep -Milliseconds 750
        }
        $doneOk = ($finalStatus -eq 'done')
        Record 'echo.terminal' $doneOk "run reached terminal state: $finalStatus"
        if (-not $doneOk) { $echoOk = $false }

        # 5e) the Chronicle records the run.
        $ev = Invoke-Http -Client $client -Method GET -Path "/v1/spine/briefs/$briefId/events"
        $hasEvents = $false
        if ($ev.Status -eq 200) { try { $hasEvents = (($ev.Body | ConvertFrom-Json).Count -gt 0) } catch {} }
        Record 'echo.chronicle' ($ev.Status -eq 200 -and $hasEvents) "GET /v1/spine/briefs/$briefId/events -> $($ev.Status) (events present=$hasEvents)"
        if (-not ($ev.Status -eq 200 -and $hasEvents)) { $echoOk = $false }
    }

    if (-not $echoOk) { Info 'echo product flow had a non-PASS step (see above)' }

    if ($KeepUp) {
        Write-Host ""
        Write-Host "Mesh left running (-KeepUp). Dashboard: $Base/dashboard  (login: $AdminUser / $AdminPass)" -ForegroundColor Yellow
        Write-Host "Stop it with: .\scripts\relix-mesh-down.ps1 -Run $Run" -ForegroundColor Yellow
    }

} finally {
    if (-not $KeepUp) {
        Write-Host ""
        Write-Host "Tearing down ..." -ForegroundColor Cyan
        try { & (Join-Path $PSScriptRoot 'relix-mesh-down.ps1') -Run $Run } catch { Write-Warning "mesh-down: $_" }
        if ($job) {
            Stop-Job -Job $job -ErrorAction SilentlyContinue
            Remove-Job -Job $job -Force -ErrorAction SilentlyContinue
        }
        # Restore the operator's real environment + clean per-run artifacts.
        $env:USERPROFILE = $OldUserProfile
        $env:HOME = $OldHome
        if ($TempHome -and (Test-Path $TempHome)) { Remove-Item -Recurse -Force -LiteralPath $TempHome -ErrorAction SilentlyContinue }
        Remove-Item -Recurse -Force -LiteralPath $DataBase -ErrorAction SilentlyContinue
        Remove-Item -Force -LiteralPath $PolicyFile -ErrorAction SilentlyContinue
        Get-ChildItem -Path 'dev-keys' -Filter "$Run-*" -ErrorAction SilentlyContinue |
            Remove-Item -Force -ErrorAction SilentlyContinue
    }
}

# -- summary ---------------------------------------------------------
Write-Host ""
$required = $script:Results | Where-Object { $_.Name -notlike 'echo.*' -and $_.Name -ne 'build' }
$echo = $script:Results | Where-Object { $_.Name -like 'echo.*' }
$reqFail = @($required | Where-Object { -not $_.Ok }).Count
$echoFail = @($echo | Where-Object { -not $_.Ok }).Count
$total = $script:Results.Count
$pass = @($script:Results | Where-Object { $_.Ok }).Count

Write-Host ("first-release smoke: {0}/{1} checks passed" -f $pass, $total) -ForegroundColor Cyan
if ($echoFail -gt 0) {
    Write-Host ("  echo product flow: {0} step(s) not PASS" -f $echoFail) -ForegroundColor Yellow
}

$fail = $reqFail
if ($RequireEchoFlow) { $fail += $echoFail }

if ($fail -eq 0) {
    Write-Host "RESULT: PASS" -ForegroundColor Green
    exit 0
} else {
    Write-Host "RESULT: FAIL ($fail required check(s) failed)" -ForegroundColor Red
    exit 1
}
