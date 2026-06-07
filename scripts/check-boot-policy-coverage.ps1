#!/usr/bin/env pwsh
# scripts/check-boot-policy-coverage.ps1
#
# Boot-policy capability-coverage + PowerShell/shell parity guard.
#
# WHY THIS EXISTS
#   The local mesh boot scripts (relix-mesh-up.ps1 / relix-mesh-up.sh) each
#   GENERATE the shared mesh policy (configs/policies/<run>.toml) inline. The
#   policy is fail-closed: a capability the bridge route calls is DENIED at
#   the mesh unless an explicit `[[rules]]` allow entry admits it. We have
#   shipped the same class of bug more than once -- a route/capability exists
#   in code + tests, but live HTTP fails because the boot script's generated
#   policy never admitted it, or only ONE of the two boot scripts did:
#     * company.starter_crew worked in code but live HTTP 403'd until the
#       allow rule was added.
#     * agent.approve_hire needed rules added to BOTH .ps1 and .sh.
#     * .sh lacked a run.discard rule that .ps1 had (observed in smoke).
#
#   This guard makes that drift impossible to land green. It parses the
#   policy allow rules out of BOTH boot scripts and asserts:
#     1. PARITY  -- the two scripts admit the EXACT same capability set
#                  (a rule in one but not the other is a failure, named).
#     2. COVERAGE -- a maintained manifest of capabilities that back live
#                  bridge HTTP routes is present in BOTH scripts.
#
# PARSER CONTRACT (kept deliberately simple + robust)
#   A policy allow rule is a line of the form:  method = "<capability>"
#   That spelling appears ONLY inside the generated policy block in each
#   boot script (the controller config blocks use node_type/key_path/etc.,
#   never `method =`), so a whitespace-tolerant line regex over the whole
#   file is sufficient and is not coupled to any one indentation style.
#   A sanity floor (MinMethods) fails loudly if the parser suddenly returns
#   far fewer methods than expected -- i.e. the policy block moved/renamed
#   out from under us -- so the guard can never pass vacuously.
#
# UPDATING THE MANIFEST
#   When you add a bridge route that calls a NEW mesh capability, add an
#   allow rule to BOTH boot scripts AND (if it backs a product/spine route)
#   add the capability to $RequiredCapabilities below. Parity alone catches
#   "added to one script only"; the manifest catches "added to neither".
#
# Exit code: 0 when parity holds and every required capability is covered;
#            1 otherwise, after printing every problem found.
#
# Usage:
#   pwsh -File scripts\check-boot-policy-coverage.ps1
#   .\scripts\check-boot-policy-coverage.ps1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$Ps1Path  = Join-Path $RepoRoot 'scripts/relix-mesh-up.ps1'
$ShPath   = Join-Path $RepoRoot 'scripts/relix-mesh-up.sh'

# A parse returning fewer than this many methods means the policy block was
# refactored/renamed and the parser no longer sees it -- fail, don't pass
# vacuously. Both scripts currently emit ~145 rules; 80 is a safe floor that
# tolerates real pruning while still catching a broken parser.
$MinMethods = 80

# Capabilities that back live bridge HTTP routes. Each MUST be admitted by
# BOTH boot scripts or the route 403s on the live mesh. Grouped by product
# surface. See the header note on how to keep this in sync.
$RequiredCapabilities = @(
    # Guild overview reads (/v1/spine/guild, /v1/spine/guild/detail) + canonical
    # Guild month-to-date spend (/v1/spine/guild/spend, the Costs page)
    'guild.counts', 'guild.get', 'guild.spend',
    # Company / Founder bootstrap (company.* routes)
    'company.status', 'company.actions', 'company.starter_crew', 'company.bootstrap_founder',
    # Prime hiring path + status (incl. the status-stream dependencies) + guided driver v1
    'prime.propose', 'prime.approve', 'prime.start', 'prime.status', 'prime.proposals', 'prime.proposal',
    'prime.next_step', 'prime.advance',
    # Mandate + strategy gate
    'mandate.create', 'mandate.list', 'mandate.tree', 'mandate.orchestrate',
    'mandate.strategy.propose', 'mandate.strategy.approve', 'mandate.strategy.reject', 'mandate.strategy.status',
    # Brief board: reads / moves / comments / fields / deps + events
    'brief.board', 'brief.board_summary', 'brief.detail', 'brief.create', 'brief.move', 'brief.set',
    'brief.comment', 'brief.unassigned', 'brief.unblocked', 'brief.run', 'brief.runs',
    # Brief thread interactions (answerable ask/confirm cards, §1.9)
    'brief.interaction_open', 'brief.interactions', 'brief.interaction_respond',
    # Interaction cancel + idempotency-aware create (§1.9)
    'brief.interaction_cancel', 'brief.interaction_create',
    # Approval-bound plan confirm (bind a confirm to the latest plan Dossier, §1.8)
    'brief.plan_confirm_open',
    # Brief suggest_tasks cards (propose + accept/reject a child-Brief tree, §1.9)
    'brief.suggest_open', 'brief.suggest_respond',
    # Plan package: plan Dossier + proposal + approval-bound confirm; accepting
    # the confirm materializes the linked proposal (§1.7/§1.8/§3.1)
    'brief.plan_package_open', 'brief.plan_confirm_respond',
    'task.events', 'task.recent_events',
    # Bridge-back operative callbacks (per-Shift brt_* token at the bridge, but
    # the bridge still calls the mesh as chat-users, so each needs an allow rule)
    'brief.dossier_add', 'brief.set_snags', 'brief.clearance_request', 'bridge_back.authorize',
    # Dossier authoring / revision-lock / fork (issue documents, §1.8)
    'brief.dossier_author', 'brief.dossier_latest',
    # Dossier (document) locking: lock / unlock / list active locks (§1.8)
    'brief.dossier_lock', 'brief.dossier_unlock', 'brief.dossier_locks',
    # Agent roster: CRUD + hiring + clearances + standing approvals
    'agent.list', 'agent.get', 'agent.create', 'agent.update', 'agent.delete',
    'agent.approve_hire', 'agent.reject_hire', 'agent.operatives', 'agent.keys', 'agent.assign_check',
    'agent.roster_summary', 'agent.effective_capabilities',
    'agent.standing_approval.create', 'agent.standing_approval.list', 'agent.standing_approval.revoke',
    'coord.approval.pending', 'coord.approval.decide',
    # Run review -> apply loop
    'run.get', 'run.events', 'run.diff', 'run.review', 'run.apply', 'run.discard',
    'run.artifacts', 'run.artifact_preview', 'run.artifact_diff', 'run.cancel',
    'rig.runtime_state.get', 'rig.runtime_state.list', 'rig.runtime_state.reset',
    # Messaging surface
    'msg.send', 'msg.inbox', 'msg.read', 'msg.thread', 'msg.delete'
)

# Parse the set of policy `method = "..."` capabilities out of a boot script.
function Get-PolicyMethods {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "boot script not found: $Path"
    }
    $set = [System.Collections.Generic.SortedSet[string]]::new([System.StringComparer]::Ordinal)
    foreach ($line in (Get-Content -LiteralPath $Path)) {
        $m = [regex]::Match($line, '^\s*method\s*=\s*"([^"]+)"\s*$')
        if ($m.Success) { [void]$set.Add($m.Groups[1].Value) }
    }
    return , $set
}

$ps1Methods = Get-PolicyMethods -Path $Ps1Path
$shMethods  = Get-PolicyMethods -Path $ShPath

$problems = New-Object System.Collections.Generic.List[string]

# (0) Parser sanity -- never pass vacuously.
if ($ps1Methods.Count -lt $MinMethods) {
    $problems.Add("parser sanity: only $($ps1Methods.Count) methods parsed from relix-mesh-up.ps1 (expected >= $MinMethods) -- the policy block likely moved/renamed")
}
if ($shMethods.Count -lt $MinMethods) {
    $problems.Add("parser sanity: only $($shMethods.Count) methods parsed from relix-mesh-up.sh (expected >= $MinMethods) -- the policy block likely moved/renamed")
}

# (1) Parity -- the two scripts must admit the identical capability set.
foreach ($m in $ps1Methods) {
    if (-not $shMethods.Contains($m)) {
        $problems.Add("parity: '$m' is allowed in relix-mesh-up.ps1 but MISSING from relix-mesh-up.sh")
    }
}
foreach ($m in $shMethods) {
    if (-not $ps1Methods.Contains($m)) {
        $problems.Add("parity: '$m' is allowed in relix-mesh-up.sh but MISSING from relix-mesh-up.ps1")
    }
}

# (2) Coverage -- every required live-route capability present in BOTH.
foreach ($cap in $RequiredCapabilities) {
    $inPs1 = $ps1Methods.Contains($cap)
    $inSh  = $shMethods.Contains($cap)
    if (-not $inPs1 -and -not $inSh) {
        $problems.Add("coverage: required capability '$cap' is MISSING from BOTH boot scripts")
    } elseif (-not $inPs1) {
        $problems.Add("coverage: required capability '$cap' missing from relix-mesh-up.ps1")
    } elseif (-not $inSh) {
        $problems.Add("coverage: required capability '$cap' missing from relix-mesh-up.sh")
    }
}

Write-Host "boot-policy coverage guard"
Write-Host ("  relix-mesh-up.ps1 : {0} policy methods" -f $ps1Methods.Count)
Write-Host ("  relix-mesh-up.sh  : {0} policy methods" -f $shMethods.Count)
Write-Host ("  required manifest : {0} capabilities" -f $RequiredCapabilities.Count)
Write-Host ""

if ($problems.Count -gt 0) {
    Write-Host "BOOT-POLICY COVERAGE: FAIL" -ForegroundColor Red
    foreach ($p in $problems) { Write-Host "  - $p" -ForegroundColor Red }
    Write-Host ""
    Write-Host "Fix: add the missing [[rules]] allow entry to the named boot script(s)." -ForegroundColor Yellow
    Write-Host "Both relix-mesh-up.ps1 and relix-mesh-up.sh must admit the same capability set." -ForegroundColor Yellow
    exit 1
}

Write-Host "BOOT-POLICY COVERAGE: PASS  (parity holds; all required capabilities admitted in both scripts)" -ForegroundColor Green
exit 0
