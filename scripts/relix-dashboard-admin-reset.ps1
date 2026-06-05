# scripts/relix-dashboard-admin-reset.ps1
#
# LOCAL operator recovery for a forgotten dashboard admin password.
#
# Wraps `relix-web-bridge reset-admin`, which rewrites the dashboard admin
# credential (dashboard-admin.json) with a fresh Argon2id-hashed password —
# the SAME storage the first-run setup uses. This is a local filesystem
# operation on THIS machine; there is no remote / unauthenticated reset.
#
# It does NOT touch your dev-data, Briefs, runs, or any other state — only
# the single admin credential file. Restart the bridge afterward for the new
# password to take effect (a restart also drops existing in-memory sessions).
#
# Usage:
#   # Generate a strong password, keep the existing username (or "admin"):
#   .\scripts\relix-dashboard-admin-reset.ps1
#
#   # Set a specific username + password:
#   .\scripts\relix-dashboard-admin-reset.ps1 -Username ops -Password 'my-strong-pass'
#
#   # Point at a specific admin file or bridge config (advanced):
#   .\scripts\relix-dashboard-admin-reset.ps1 -AdminFile "$env:USERPROFILE\.relix\dashboard-admin.json"
#   .\scripts\relix-dashboard-admin-reset.ps1 -Config dev-data\local\bridge.toml
#
# After it prints the new credentials: restart the bridge (Ctrl-C the
# mesh-up window + re-run it, or `.\scripts\relix-mesh-down.ps1` then
# `.\scripts\relix-mesh-up.ps1`), then log in at /dashboard.

[CmdletBinding()]
param(
    [string]$Username = '',
    [string]$Password = '',
    [string]$AdminFile = '',
    [string]$Config = ''
)

$ErrorActionPreference = 'Stop'

# Run from the repo root (this script lives in scripts/).
Set-Location (Join-Path $PSScriptRoot '..')

$Bridge = Join-Path 'target' (Join-Path 'debug' 'relix-web-bridge.exe')
if (-not (Test-Path -LiteralPath $Bridge)) {
    Write-Host "relix-web-bridge.exe not found at $Bridge" -ForegroundColor Yellow
    Write-Host "Build it first:  cargo build -p relix-web-bridge" -ForegroundColor Yellow
    exit 1
}

# Build the argument list. Only pass flags the operator actually set so the
# binary applies its own safe defaults (admin file -> ~/.relix; username ->
# existing or 'admin'; password -> generated + printed).
$cmdArgs = @('reset-admin')
if ($AdminFile -ne '') { $cmdArgs += @('--admin-file', $AdminFile) }
elseif ($Config -ne '') { $cmdArgs += @('--config', $Config) }
if ($Username -ne '') { $cmdArgs += @('--username', $Username) }
if ($Password -ne '') { $cmdArgs += @('--password', $Password) }

Write-Host "Resetting local dashboard admin credential..." -ForegroundColor Cyan
& $Bridge @cmdArgs
exit $LASTEXITCODE
