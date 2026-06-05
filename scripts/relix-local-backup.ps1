# scripts/relix-local-backup.ps1
#
# Create a LOCAL zip backup of important Relix state (SQLite DBs + configs).
# It never uploads anywhere — the zip stays on this machine.
#
# By default it EXCLUDES regenerable / large / sensitive material:
#   * build output:      target, target-audit, node_modules, dist
#   * VCS:               .git
#   * run workspaces:    workspaces, runs        (disposable sandboxes)
#   * logs:              *.log
#   * secrets:           bridge-token, dashboard-admin.json, *.key, *.aic,
#                        dev-keys, .env / .env.*
#
# For a CONSISTENT database backup, stop the mesh first
# (`.\scripts\relix-mesh-down.ps1`) so the SQLite files aren't mid-write.
#
# Usage:
#   .\scripts\relix-local-backup.ps1                       # backs up dev-data
#   .\scripts\relix-local-backup.ps1 -Source dev-data -OutDir backups
#   .\scripts\relix-local-backup.ps1 -IncludeWorkspaces    # also include run workspaces
#   .\scripts\relix-local-backup.ps1 -IncludeSecrets       # also include tokens/keys (careful!)

[CmdletBinding()]
param(
    [string]$Source = 'dev-data',
    [string]$OutDir = 'backups',
    [switch]$IncludeWorkspaces,
    [switch]$IncludeSecrets,
    [switch]$ListContents
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

if (-not (Test-Path -LiteralPath $Source)) {
    Write-Host "Source '$Source' does not exist. Point -Source at your Relix data dir (e.g. dev-data)." -ForegroundColor Yellow
    exit 1
}

# Always-excluded dirs/files; secrets + workspaces excluded unless opted in.
$excludeDirs = @('target', 'target-audit', 'node_modules', 'dist', '.git')
if (-not $IncludeWorkspaces) { $excludeDirs += @('workspaces', 'runs') }
if (-not $IncludeSecrets)    { $excludeDirs += @('dev-keys') }

$excludeFiles = @('*.log', '*.tmp')
if (-not $IncludeSecrets) {
    $excludeFiles += @('bridge-token', 'dashboard-admin.json', '*.key', '*.aic',
                       '*.pem', '.env', '.env.*', 'mesh.pids')
}

# Stage a filtered copy with Robocopy (built-in; /XD excludes dirs, /XF files),
# then zip the staging dir so the archive preserves the directory structure.
$stamp   = Get-Date -Format 'yyyyMMdd-HHmmss'
$staging = Join-Path ([System.IO.Path]::GetTempPath()) "relix-backup-$stamp"
New-Item -ItemType Directory -Force -Path $staging | Out-Null
New-Item -ItemType Directory -Force -Path $OutDir  | Out-Null

$rcArgs = @($Source, $staging, '/E', '/NFL', '/NDL', '/NJH', '/NJS', '/NP', '/R:1', '/W:1')
$rcArgs += '/XD'; $rcArgs += $excludeDirs
$rcArgs += '/XF'; $rcArgs += $excludeFiles

Write-Host "Staging filtered copy of '$Source'..." -ForegroundColor Cyan
& robocopy @rcArgs | Out-Null
# Robocopy exit codes 0-7 are success; 8+ is a real error.
if ($LASTEXITCODE -ge 8) {
    Write-Host "robocopy failed (exit $LASTEXITCODE)" -ForegroundColor Red
    Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
    exit 1
}

$zip = Join-Path $OutDir "relix-backup-$stamp.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue

$size = (Get-Item $zip).Length
$mb = [Math]::Round($size / 1MB, 2)
$zipPath = (Resolve-Path $zip).Path
Write-Host ""
Write-Host "Backup written (local only):" -ForegroundColor Green
Write-Host "  path : $zipPath"
Write-Host "  size : $mb MB"
if (-not $IncludeSecrets)    { Write-Host "  note : secrets (tokens/keys) excluded — pass -IncludeSecrets to include them." -ForegroundColor DarkGray }
if (-not $IncludeWorkspaces) { Write-Host "  note : run workspaces excluded — pass -IncludeWorkspaces to include them." -ForegroundColor DarkGray }

if ($ListContents) {
    Write-Host ""
    Write-Host "Contents:" -ForegroundColor Cyan
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try { $archive.Entries | ForEach-Object { Write-Host "  $($_.FullName)" } }
    finally { $archive.Dispose() }
}

Write-Host ""
Write-Host "Restore (local) — stop the mesh first, then expand into place:" -ForegroundColor Cyan
Write-Host "  .\scripts\relix-mesh-down.ps1"
Write-Host "  # inspect first:  Expand-Archive '$zipPath' -DestinationPath restore-preview"
Write-Host "  # then move the restored '$Source' folder back where it belongs, or:"
Write-Host "  Expand-Archive '$zipPath' -DestinationPath . -Force   # overwrites '$Source' in CWD"
Write-Host "  .\scripts\relix-mesh-up.ps1"
Write-Host "Note: this script does NOT auto-restore (destructive) — expand the zip yourself. See docs/operations.md."
