# scripts/alpha-bringup-m5.ps1
#
# End-to-end M5 demo on Windows PowerShell. Same flow as alpha-bringup-m5.sh.
# Usage:
#   .\scripts\alpha-bringup-m5.ps1
#   .\scripts\alpha-bringup-m5.ps1 -Keep

param(
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

New-Item -ItemType Directory -Force -Path dev-keys, dev-data | Out-Null
Get-ChildItem dev-keys -Filter 'm5demo-*' -ErrorAction SilentlyContinue | Remove-Item -Force

$OrgKey  = 'dev-keys/m5demo-org-root.key'
$OrgPub  = 'dev-keys/m5demo-org-root.pub'
$Alice   = 'dev-keys/m5demo-alice.aic'
$Bob     = 'dev-keys/m5demo-bob.aic'
$NodeKey = 'dev-keys/m5demo-node.key'
$DataDir = 'dev-data/m5demo'
$Port    = 19501
$Config  = Join-Path $env:TEMP 'relix-m5demo.toml'
$LogFile = Join-Path $env:TEMP 'relix-m5demo.log'

cargo run -q -p relix-cli -- identity init-org --root-key $OrgKey --org m5demo
# init-org writes the companion .pub file alongside the .key automatically.

cargo run -q -p relix-cli -- identity mint `
    --root-key $OrgKey --name alice --groups chat-users --out $Alice
cargo run -q -p relix-cli -- identity mint `
    --root-key $OrgKey --name bob --groups guest --out $Bob

@"
[controller]
name = "m5demo"
node_type = "demo"
listen_port = $Port

[identity]
key_path = "$NodeKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "configs/policies/m5demo.toml"

[peers]
"@ | Set-Content -Encoding utf8 $Config

New-Item -ItemType Directory -Force -Path 'configs/policies' | Out-Null
@"
[admit]
groups = ["chat-users", "guest"]

[[rules]]
name = "chat_users_health"
method = "node.health"
allow_groups = ["chat-users"]
"@ | Set-Content -Encoding utf8 'configs/policies/m5demo.toml'

if (Test-Path $DataDir) { Remove-Item $DataDir -Recurse -Force }

Write-Host "starting controller on tcp/$Port ..."
$env:RELIX_DATA_DIR = 'dev-data'
$env:RUST_LOG = 'relix_runtime=info,relix_cli=info'

$proc = Start-Process -PassThru -NoNewWindow `
    -FilePath cargo `
    -ArgumentList @('run', '-q', '-p', 'relix-controller', '--', '--config', $Config) `
    -RedirectStandardOutput $LogFile `
    -RedirectStandardError ($LogFile + '.err')

try {
    # Wait for "transport listening".
    $ready = $false
    for ($i = 0; $i -lt 60; $i++) {
        if (Test-Path $LogFile) {
            $tail = Get-Content $LogFile -Tail 20 -ErrorAction SilentlyContinue
            if ($tail -match 'transport listening') { $ready = $true; break }
        }
        Start-Sleep -Milliseconds 200
    }
    if (-not $ready) { throw "controller did not become ready (see $LogFile)" }

    Write-Host "=== ping as alice (chat-users) ==="
    cargo run -q -p relix-cli -- ping `
        --peer "/ip4/127.0.0.1/tcp/$Port" `
        --identity $Alice `
        --client-key $OrgKey

    Write-Host ""
    Write-Host "=== ping as bob (guest) — expect policy_denied ==="
    & cargo run -q -p relix-cli -- ping `
        --peer "/ip4/127.0.0.1/tcp/$Port" `
        --identity $Bob `
        --client-key $OrgKey
    if ($LASTEXITCODE -ne 2) {
        throw "expected exit code 2 from policy_denied, got $LASTEXITCODE"
    }
    Write-Host "(bob correctly denied)"

    Write-Host ""
    Write-Host "=== responder audit log ==="
    cargo run -q -p relix-flow-inspect -- --audit (Join-Path $DataDir 'audit.log')

    Write-Host ""
    Write-Host "M5 demo OK."
}
finally {
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -Force
    }
    if (-not $Keep) {
        Remove-Item -Force $OrgKey, $OrgPub, $Alice, $Bob, $NodeKey -ErrorAction SilentlyContinue
        Remove-Item -Force 'configs/policies/m5demo.toml', $Config, $LogFile, ($LogFile + '.err') -ErrorAction SilentlyContinue
        if (Test-Path $DataDir) { Remove-Item $DataDir -Recurse -Force }
    }
}
