# scripts/relix-mesh-down.ps1
#
# Stops every running relix-controller and relix-web-bridge on this
# machine. Use this if you backgrounded the mesh and lost the PIDs
# the boot script printed, or if a stray process from a crashed run
# is still alive.
#
# Sends Stop-Process first, waits briefly, then Stop-Process -Force
# anything still up. Prints which PIDs were stopped. Returns 0 even
# if nothing was running (idempotent).

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$Patterns = @('relix-controller', 'relix-web-bridge')
$Stopped  = @()

foreach ($name in $Patterns) {
    foreach ($p in (Get-Process -Name $name -ErrorAction SilentlyContinue)) {
        try {
            Stop-Process -Id $p.Id -ErrorAction Stop
            $Stopped += [pscustomobject]@{ Pid = $p.Id; Name = $name }
        } catch {
            Write-Warning "stop $name pid=$($p.Id): $($_.Exception.Message)"
        }
    }
}

if ($Stopped.Count -eq 0) {
    Write-Host "no relix-controller / relix-web-bridge processes were running."
    exit 0
}

Start-Sleep -Milliseconds 500

foreach ($entry in $Stopped) {
    $alive = Get-Process -Id $entry.Pid -ErrorAction SilentlyContinue
    if ($alive) {
        try {
            Stop-Process -Id $entry.Pid -Force -ErrorAction Stop
            Write-Host ("  hard-killed {0} pid={1}" -f $entry.Name, $entry.Pid)
        } catch {
            Write-Warning "force-kill $($entry.Name) pid=$($entry.Pid): $($_.Exception.Message)"
        }
    } else {
        Write-Host ("  stopped     {0} pid={1}" -f $entry.Name, $entry.Pid)
    }
}

Write-Host "mesh down."
