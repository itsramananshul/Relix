# scripts/relix-mesh-down.ps1
#
# Stops a local Relix mesh by terminating ONLY the PIDs that
# scripts/relix-mesh-up.ps1 recorded in its pidfile. It never matches by
# process name, so a relix-controller or relix-web-bridge belonging to
# another mesh (or started by hand outside this run) is left untouched.
# This upholds the mesh-up contract: only stop the PIDs we started.
#
# Use this if you backgrounded the mesh and lost the terminal that
# relix-mesh-up.ps1 was blocking in. A mesh stopped with Ctrl-C in its
# own terminal is already torn down by that script's own cleanup; this
# is the out-of-band path for a backgrounded or crashed run.
#
# Sends Stop-Process first, waits briefly, then Stop-Process -Force on
# anything still up. Prints which PIDs were stopped and removes the
# pidfile when done. Idempotent: exits 0 when there is no pidfile or
# nothing left to stop.

[CmdletBinding()]
param(
    [string]$Run     = 'local',
    [string]$DataDir = 'dev-data'
)

$ErrorActionPreference = 'Stop'

# Resolve a relative data dir against the same root mesh-up uses. mesh-up
# Set-Locations to the script's parent before creating `dev-data/<run>`,
# so the pidfile lives under $PSScriptRoot\.. - not the caller's CWD.
# Matching that hop here lets `relix stop` (which may run from anywhere)
# find the file. An absolute -DataDir is used as-is by Join-Path.
Set-Location (Join-Path $PSScriptRoot '..')

$PidFile = Join-Path $DataDir (Join-Path $Run 'mesh.pids')

if (-not (Test-Path -LiteralPath $PidFile)) {
    Write-Host "no pidfile at $PidFile; nothing to stop."
    exit 0
}

# Read recorded PIDs (one per line). Skip blank / non-numeric lines so a
# partially written or hand-edited file can never turn into a stray kill.
# ($PID is an automatic variable for THIS process, so the loop var is
# named $procId to avoid clobbering it.)
$Recorded = @()
foreach ($line in (Get-Content -LiteralPath $PidFile)) {
    $t = $line.Trim()
    if ($t -match '^[0-9]+$') { $Recorded += [int]$t }
}

if ($Recorded.Count -eq 0) {
    Write-Host "pidfile $PidFile held no usable PIDs; removing it."
    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
    exit 0
}

$Stopped = @()
foreach ($procId in $Recorded) {
    if (Get-Process -Id $procId -ErrorAction SilentlyContinue) {
        try {
            Stop-Process -Id $procId -ErrorAction Stop
            $Stopped += $procId
        } catch {
            Write-Warning "stop pid=${procId}: $($_.Exception.Message)"
        }
    }
}

if ($Stopped.Count -eq 0) {
    Write-Host "no recorded mesh processes were still running."
    Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
    exit 0
}

Start-Sleep -Milliseconds 500

foreach ($procId in $Stopped) {
    if (Get-Process -Id $procId -ErrorAction SilentlyContinue) {
        try {
            Stop-Process -Id $procId -Force -ErrorAction Stop
            Write-Host ("  hard-killed pid={0}" -f $procId)
        } catch {
            Write-Warning "force-kill pid=${procId}: $($_.Exception.Message)"
        }
    } else {
        Write-Host ("  stopped     pid={0}" -f $procId)
    }
}

Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue
Write-Host "mesh down."
