# scripts/relix-mesh-up.ps1
#
# Windows-safe PowerShell driver. Brings up the local Relix mesh and BLOCKS
# until the operator presses Ctrl-C.
#
# Nodes started (each is a normal peer; nothing is a "central gateway"):
#
#   memory controller  - SQLite + FTS5 session store
#   ai controller      - provider-agnostic ai.chat
#   tool controller    - tool.web_fetch (M9), SSRF-guarded
#   relix-web-bridge   - local HTTP -> SOL flow (/chat, /chat_with_tool, /v1/*)
#
# Safety contract:
#   * Launches pre-built binaries directly (target\debug\*.exe), so the PIDs
#     returned by Start-Process ARE the controller / bridge themselves - not
#     a `cargo run` wrapper. That lets us stop exactly what we started.
#   * On Ctrl-C, ONLY Stop-Process the PIDs collected during this run.
#     No taskkill /IM, no Get-Process | Where-Object name match, nothing that
#     could touch unrelated relix-*.exe instances, Claude Code, or terminals.
#
# Usage:
#   .\scripts\relix-mesh-up.ps1
#   .\scripts\relix-mesh-up.ps1 -Provider openrouter
#   .\scripts\relix-mesh-up.ps1 -Provider openai     -BaseUrl https://api.openai.com/v1
#   .\scripts\relix-mesh-up.ps1 -Provider anthropic
#   .\scripts\relix-mesh-up.ps1 -Provider local      -BaseUrl http://localhost:11434/v1
#   .\scripts\relix-mesh-up.ps1 -Run myrun -BridgePort 19800
#   .\scripts\relix-mesh-up.ps1 -ToolAllowHttp        # accept http:// (default https-only)
#   .\scripts\relix-mesh-up.ps1 -NoTool                # skip tool node + tool flow

[CmdletBinding()]
param(
    [ValidateSet('mock','openai','openrouter','xai','anthropic','gemini','local')]
    [string]$Provider     = 'mock',
    [string]$BaseUrl      = '',
    [string]$Run          = 'local',
    [int]$BridgePort      = 19791,
    [int]$MemPort         = 19711,
    [int]$AiPort          = 19712,
    [int]$ToolPort        = 19713,
    [switch]$ToolAllowHttp,
    [switch]$NoTool
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$Root = (Get-Location).Path

$Cli        = Join-Path $Root 'target\debug\relix-cli.exe'
$Controller = Join-Path $Root 'target\debug\relix-controller.exe'
$Bridge     = Join-Path $Root 'target\debug\relix-web-bridge.exe'

foreach ($exe in @($Cli, $Controller, $Bridge)) {
    if (-not (Test-Path $exe)) {
        throw "missing binary: $exe`nRun:  cargo build --workspace"
    }
}

$DataBase   = "dev-data/$Run"
$OrgKey     = "dev-keys/$Run-org-root.key"
$OrgPub     = "dev-keys/$Run-org-root.pub"
$BridgeAic  = "dev-keys/$Run-bridge.aic"
$MemKey     = "dev-keys/$Run-memory.key"
$AiKey      = "dev-keys/$Run-ai.key"
$ToolKey    = "dev-keys/$Run-tool.key"
$BridgeKey  = "dev-keys/$Run-bridge.key"
$Policy     = "configs/policies/$Run.toml"
$BridgeHttp = "127.0.0.1:$BridgePort"

New-Item -ItemType Directory -Force -Path 'dev-keys', $DataBase, 'configs/policies' | Out-Null

# 1) Identities - idempotent: only mint if missing so restarts are cheap.
if (-not (Test-Path $OrgKey) -or -not (Test-Path $OrgPub)) {
    Write-Host "minting org root ..."
    & $Cli identity init-org --root-key $OrgKey --org $Run
    if ($LASTEXITCODE -ne 0) { throw "identity init-org failed (exit $LASTEXITCODE)" }
}
if (-not (Test-Path $BridgeAic)) {
    Write-Host "minting bridge identity bundle ..."
    & $Cli identity mint --root-key $OrgKey --name web-bridge --groups chat-users --out $BridgeAic
    if ($LASTEXITCODE -ne 0) { throw "identity mint failed (exit $LASTEXITCODE)" }
}

$MemConfig    = "$DataBase/memory.toml"
$AiConfig     = "$DataBase/ai.toml"
$ToolConfig   = "$DataBase/tool.toml"
$BridgeConfig = "$DataBase/bridge.toml"
$Peers        = "$DataBase/peers.toml"

# 2) Memory controller config.
@"
[controller]
name = "$Run-memory"
node_type = "memory"
listen_port = $MemPort

[identity]
key_path = "$MemKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[memory]
db_path = "$DataBase/memory.db"

[peers]
"@ | Set-Content -Encoding utf8 $MemConfig

# 3) AI controller config - base + provider-specific tail.
$aiBase = @"
[controller]
name = "$Run-ai"
node_type = "ai"
listen_port = $AiPort

[identity]
key_path = "$AiKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[ai]
provider = "$Provider"
model    = ""

[peers]
"@

$providerTail = switch ($Provider) {
    'openai' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://api.openai.com/v1' }
@"

[ai.providers.openai]
base_url      = "$b"
api_key_env   = "OPENAI_API_KEY"
default_model = "gpt-4o-mini"
"@
    }
    'openrouter' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://openrouter.ai/api/v1' }
@"

[ai.providers.openrouter]
base_url      = "$b"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"
"@
    }
    'xai' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://api.x.ai/v1' }
@"

[ai.providers.xai]
base_url      = "$b"
api_key_env   = "XAI_API_KEY"
"@
    }
    'local' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'http://localhost:11434/v1' }
@"

[ai.providers.local]
base_url      = "$b"
"@
    }
    'anthropic' {
@"

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "claude-3-5-sonnet-latest"
"@
    }
    'gemini' {
@"

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
"@
    }
    default { '' }
}
($aiBase + $providerTail) | Set-Content -Encoding utf8 $AiConfig

# 4) Tool controller config (M9). The HTTP client lives inside the tool node;
#    the bridge never talks to external URLs directly.
if (-not $NoTool) {
    $allowHttp = if ($ToolAllowHttp) { 'true' } else { 'false' }
@"
[controller]
name = "$Run-tool"
node_type = "tool"
listen_port = $ToolPort

[identity]
key_path = "$ToolKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[tool]
max_bytes     = 262144
timeout_secs  = 15
max_redirects = 3
allow_http    = $allowHttp
user_agent    = "Relix-tool/0.1.0"

[peers]
"@ | Set-Content -Encoding utf8 $ToolConfig
}

# 5) Shared policy. Tool capability requires chat-users (same as ai/memory),
#    so the bridge's existing identity bundle is sufficient.
@"
[admit]
groups = ["chat-users"]

[[rules]]
name = "mem_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_chat"
method = "ai.chat"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_web_fetch"
method = "tool.web_fetch"
allow_groups = ["chat-users"]
"@ | Set-Content -Encoding utf8 $Policy

# 6) Peer alias map consumed by the bridge. Tool entry omitted when -NoTool.
$peersToml = @"
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MemPort"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
"@
if (-not $NoTool) {
    $peersToml += @"


[peers.tool]
addr = "/ip4/127.0.0.1/tcp/$ToolPort"
"@
}
$peersToml | Set-Content -Encoding utf8 $Peers

# 7) Bridge config - OpenAI shim on; tool template wired only when the tool
#    node is up so the bridge fails 404 cleanly when there's no peer.
$toolTemplateLine = if ($NoTool) { '' } else { 'tool_template_path = "flows/chat_with_tool.sol"' }
@"
[bridge]
listen_addr = "$BridgeHttp"

[identity]
bundle_path     = "$BridgeAic"
client_key_path = "$BridgeKey"

[transport]
peers_path    = "$Peers"
deadline_secs = 60

[flow]
template_path = "flows/chat_template.sol"
$toolTemplateLine

[sse]
chunk_bytes    = 24
chunk_delay_ms = 15

[openai_compat]
default_model = "relix-$Provider"

[[openai_compat.models]]
id          = "relix-$Provider"
description = "Relix mesh route - AI node currently set to $Provider"
"@ | Set-Content -Encoding utf8 $BridgeConfig

$MemLog    = "$DataBase/memory.log"
$AiLog     = "$DataBase/ai.log"
$ToolLog   = "$DataBase/tool.log"
$BridgeLog = "$DataBase/bridge.log"
$MemErr    = "$DataBase/memory.err.log"
$AiErr     = "$DataBase/ai.err.log"
$ToolErr   = "$DataBase/tool.err.log"
$BridgeErr = "$DataBase/bridge.err.log"

$env:RELIX_DATA_DIR = 'dev-data'

function Start-Node {
    param(
        [Parameter(Mandatory)] [string]$Exe,
        [Parameter(Mandatory)] [string]$Cfg,
        [Parameter(Mandatory)] [string]$OutLog,
        [Parameter(Mandatory)] [string]$ErrLog,
        [Parameter(Mandatory)] [string]$RustLog
    )
    # Per-node env: writing $env:RUST_LOG just before spawn takes effect for
    # the child only via inheritance at process-start time.
    $env:RUST_LOG = $RustLog
    return Start-Process `
        -FilePath $Exe `
        -ArgumentList @('--config', $Cfg) `
        -NoNewWindow `
        -PassThru `
        -RedirectStandardOutput $OutLog `
        -RedirectStandardError  $ErrLog
}

function Wait-Log {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [string]$Needle,
        [Parameter(Mandatory)] [string]$Desc
    )
    for ($i = 0; $i -lt 150; $i++) {
        if (Test-Path $Path) {
            $hit = Select-String -Path $Path -Pattern $Needle -SimpleMatch -Quiet -ErrorAction SilentlyContinue
            if ($hit) { return $true }
        }
        Start-Sleep -Milliseconds 200
    }
    Write-Warning "$Desc never logged '$Needle' (see $Path)"
    if (Test-Path $Path)              { Get-Content $Path              -Tail 40 | ForEach-Object { Write-Host "  $_" } }
    $errPath = $Path -replace '\.log$','.err.log'
    if (Test-Path $errPath)           { Get-Content $errPath           -Tail 40 | ForEach-Object { Write-Host "  $_" } }
    return $false
}

Write-Host "== Relix mesh up =="
Write-Host "  run:           $Run"
Write-Host "  provider:      $Provider"
Write-Host "  memory port:   tcp/$MemPort"
Write-Host "  ai port:       tcp/$AiPort"
if (-not $NoTool) {
    Write-Host ("  tool port:     tcp/{0}  (allow_http={1})" -f $ToolPort, $ToolAllowHttp.IsPresent)
} else {
    Write-Host "  tool port:     (disabled - -NoTool)"
}
Write-Host "  bridge HTTP:   http://$BridgeHttp"
Write-Host "  data dir:      $DataBase"
Write-Host ""

# Track ONLY the processes this script started. Stop-Process on shutdown
# is restricted to this exact list - never a name-based sweep.
$started = New-Object System.Collections.ArrayList

try {
    Write-Host "starting memory controller ..."
    [void]$started.Add( (Start-Node -Exe $Controller -Cfg $MemConfig -OutLog $MemLog -ErrLog $MemErr -RustLog 'relix_runtime=info') )

    Write-Host "starting ai controller ..."
    [void]$started.Add( (Start-Node -Exe $Controller -Cfg $AiConfig  -OutLog $AiLog  -ErrLog $AiErr  -RustLog 'relix_runtime=info') )

    if (-not $NoTool) {
        Write-Host "starting tool controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $ToolConfig -OutLog $ToolLog -ErrLog $ToolErr -RustLog 'relix_runtime=info') )
    }

    if (-not (Wait-Log -Path $MemLog -Needle 'transport listening' -Desc 'memory controller')) { throw 'memory controller never came up' }
    if (-not (Wait-Log -Path $AiLog  -Needle 'transport listening' -Desc 'ai controller'))     { throw 'ai controller never came up' }
    if (-not $NoTool) {
        if (-not (Wait-Log -Path $ToolLog -Needle 'transport listening' -Desc 'tool controller')) { throw 'tool controller never came up' }
    }
    Start-Sleep -Milliseconds 400

    Write-Host "starting web bridge ..."
    [void]$started.Add( (Start-Node -Exe $Bridge -Cfg $BridgeConfig -OutLog $BridgeLog -ErrLog $BridgeErr -RustLog 'relix_web_bridge=info,relix_runtime=info') )

    if (-not (Wait-Log -Path $BridgeLog -Needle 'web bridge starting' -Desc 'web bridge')) { throw 'web bridge never came up' }
    Start-Sleep -Milliseconds 400

    Write-Host ""
    Write-Host "mesh is UP."
    Write-Host ""
    Write-Host "Endpoints:"
    Write-Host "  http://127.0.0.1:$BridgePort/health"
    Write-Host "  http://127.0.0.1:$BridgePort/v1/models"
    Write-Host "  http://127.0.0.1:$BridgePort/v1/chat/completions"
    if (-not $NoTool) {
        Write-Host "  http://127.0.0.1:$BridgePort/chat_with_tool   (POST: {session_id, message, url})"
    }
    Write-Host ""
    Write-Host "Open WebUI config:"
    Write-Host "  API Base URL: http://127.0.0.1:$BridgePort/v1"
    Write-Host "  API Key:      anything non-empty"
    Write-Host "  model:        relix-$Provider"
    if (-not $NoTool) {
        Write-Host "  Note:         messages containing an http(s) URL auto-route through the tool flow."
    }
    Write-Host ""
    Write-Host "Smoke tests:"
    Write-Host "  Invoke-RestMethod http://127.0.0.1:$BridgePort/health"
    Write-Host "  Invoke-RestMethod http://127.0.0.1:$BridgePort/v1/models"
    Write-Host "  Invoke-RestMethod -Method Post http://127.0.0.1:$BridgePort/v1/chat/completions ``"
    Write-Host "    -ContentType 'application/json' ``"
    Write-Host "    -Body (@{ model='relix-$Provider'; messages=@(@{role='user';content='hello'}) } | ConvertTo-Json)"
    if (-not $NoTool) {
        Write-Host ""
        Write-Host "  # Tool flow:"
        Write-Host "  Invoke-RestMethod -Method Post http://127.0.0.1:$BridgePort/chat_with_tool ``"
        Write-Host "    -ContentType 'application/json' ``"
        Write-Host "    -Body (@{ session_id='demo'; message='summarize this page'; url='https://example.com/' } | ConvertTo-Json)"
    }
    Write-Host ""
    Write-Host "Logs:"
    Write-Host "  $MemLog"
    Write-Host "  $AiLog"
    if (-not $NoTool) { Write-Host "  $ToolLog" }
    Write-Host "  $BridgeLog"
    Write-Host ""
    Write-Host "PIDs (this script will only stop these on Ctrl-C):"
    foreach ($p in $started) { Write-Host ("  {0,-22} pid {1}" -f $p.ProcessName, $p.Id) }
    Write-Host ""
    Write-Host "Ctrl-C to stop the mesh."
    Write-Host ""

    # Intercept Ctrl-C so cleanup runs exactly once, hits only our PIDs, and
    # leaves the parent terminal untouched. In non-interactive hosts (jobs,
    # hidden windows) there's no key input - fall back to blocking on the
    # bridge process; Stop-Process from outside will trigger the finally.
    $interactive = $true
    try { $null = [Console]::TreatControlCAsInput } catch { $interactive = $false }

    if ($interactive) {
        $prevCtrl = [Console]::TreatControlCAsInput
        [Console]::TreatControlCAsInput = $true
        try {
            while ($true) {
                if ([Console]::KeyAvailable) {
                    $k = [Console]::ReadKey($true)
                    if ( ($k.Modifiers -band [ConsoleModifiers]::Control) -and ($k.Key -eq 'C') ) { break }
                }
                foreach ($p in @($started)) {
                    if ($p.HasExited) {
                        Write-Warning "child PID $($p.Id) ($($p.ProcessName)) exited early with code $($p.ExitCode)"
                        throw 'a child process exited unexpectedly'
                    }
                }
                Start-Sleep -Milliseconds 250
            }
        }
        finally {
            [Console]::TreatControlCAsInput = $prevCtrl
        }
    } else {
        # No console: park on the bridge until it exits (externally stopped).
        $bridgeProc = $started[$started.Count - 1]
        $bridgeProc.WaitForExit()
    }
}
finally {
    Write-Host ""
    Write-Host "stopping mesh (only PIDs started by this script) ..."
    foreach ($p in @($started)) {
        if ($p -and -not $p.HasExited) {
            try {
                Stop-Process -Id $p.Id -ErrorAction Stop
                Write-Host "  stopped $($p.ProcessName) (pid $($p.Id))"
            } catch {
                Write-Warning "could not stop pid $($p.Id): $_"
            }
        }
    }
    Write-Host "mesh down."
}
