#!/usr/bin/env pwsh
# scripts/check-dashboard-dist.ps1 — dashboard dist parity gate.
#
# The React dashboard (apps/dashboard) is the ONLY dashboard the web-bridge
# serves; it serves the COMMITTED bundle at crates/relix-web-bridge/
# dashboard-dist. So if apps/dashboard/src changes but dashboard-dist is not
# rebuilt + committed, the product would serve a stale UI. This script makes
# that a hard, repeatable failure: it rebuilds the dashboard and fails if the
# committed dist drifts from a fresh build.
#
# Non-destructive: it only installs node_modules when they are MISSING (a
# fresh checkout / CI). When node_modules already exists it goes straight to
# the build, so it never wipes a developer's installed deps.
#
# Exit codes: 0 = in sync; 1 = drift (rebuild + commit the dist); 2 = npm/Node
# not available.
#
# Usage:
#   pwsh -File scripts\check-dashboard-dist.ps1
#   .\scripts\check-dashboard-dist.ps1

$ErrorActionPreference = 'Stop'

# Repo root = parent of scripts/.
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

$Dist = 'crates/relix-web-bridge/dashboard-dist'
$App = 'apps/dashboard'

$npm = Get-Command npm.cmd -ErrorAction SilentlyContinue
if (-not $npm) {
    $npm = Get-Command npm -ErrorAction SilentlyContinue
}
if (-not $npm) {
    Write-Host 'dashboard dist parity: SKIP/FAIL — npm not found.' -ForegroundColor Red
    Write-Host 'Install Node.js (npm) to build + verify the React dashboard bundle.' -ForegroundColor Yellow
    exit 2
}

function Invoke-Npm {
    param([Parameter(ValueFromRemainingArguments = $true)] [string[]]$NpmArgs)
    & $npm.Source @NpmArgs
}

Push-Location $App
try {
    if (-not (Test-Path 'node_modules')) {
        Write-Host '==> dashboard deps missing — installing (npm ci)' -ForegroundColor Cyan
        Invoke-Npm ci
        if ($LASTEXITCODE -ne 0) {
            Write-Host '    npm ci failed; falling back to npm install' -ForegroundColor Yellow
            Invoke-Npm install
            if ($LASTEXITCODE -ne 0) { throw 'npm install failed' }
        }
    }
    else {
        Write-Host '==> dashboard deps present — skipping install (non-destructive)' -ForegroundColor Cyan
    }
    Write-Host '==> npm run build' -ForegroundColor Cyan
    Invoke-Npm run build
    if ($LASTEXITCODE -ne 0) { throw 'dashboard build failed' }
}
finally {
    Pop-Location
}

# Drift check: porcelain covers modified, new (untracked), and deleted files
# under the committed dist directory.
$changes = git status --porcelain -- $Dist
if ($changes) {
    Write-Host ''
    Write-Host "DASHBOARD DIST DRIFT: the committed $Dist does not match a fresh build." -ForegroundColor Red
    Write-Host 'The React dashboard source changed without rebuilding the committed bundle.' -ForegroundColor Yellow
    Write-Host 'Fix: rebuild and commit the regenerated dist:' -ForegroundColor Yellow
    Write-Host '    cd apps/dashboard; npm run build' -ForegroundColor Yellow
    Write-Host "    git add $Dist" -ForegroundColor Yellow
    Write-Host ''
    Write-Host 'Changed paths:'
    $changes | ForEach-Object { Write-Host "  $_" }
    exit 1
}

Write-Host ''
Write-Host "dashboard dist parity: OK ($Dist matches a fresh build)." -ForegroundColor Green
exit 0
