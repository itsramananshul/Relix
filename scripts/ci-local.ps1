#!/usr/bin/env pwsh
# scripts/ci-local.ps1 — Windows-local CI gate. Run this before tagging
# a release (git tag vX.Y.Z).
#
# GitHub Actions (ci.yml) now runs ONLY the macOS + Linux workspace test
# on push/PR — the platform coverage this Windows box cannot reproduce.
# The platform-independent gates (rustfmt, clippy, cargo deny) and the
# Windows test leg were moved here so they no longer burn Actions
# minutes on every commit. This script reproduces all of them locally.
#
# Behaviour: runs each gate in order, STOPS on the first failure, prints
# each step's exit code, and ends with a single PASS / FAIL line.
# Exit code: 0 when every gate passed, 1 otherwise.
#
# Usage:
#   pwsh -File scripts\ci-local.ps1
#   .\scripts\ci-local.ps1

$ErrorActionPreference = 'Continue'

# Run from the repo root regardless of where the script is invoked from
# ($PSScriptRoot is the scripts/ dir; its parent is the repo root).
$RepoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $RepoRoot

# Each gate is a name + a scriptblock. Order matters: cheapest/fastest
# feedback first (fmt), then clippy, then the full serial test, then the
# supply-chain check.
$Steps = @(
    @{
        # Boot-policy coverage + parity. Pure text parse (no compile), so it is
        # the cheapest gate and runs first: it fails fast when a live bridge
        # route's capability is not admitted by BOTH relix-mesh-up.ps1 and
        # relix-mesh-up.sh, which would 403 on the live mesh.
        Name   = 'boot-policy coverage (check-boot-policy-coverage.ps1)'
        Script = { & (Join-Path $RepoRoot 'scripts/check-boot-policy-coverage.ps1') }
    },
    @{
        Name   = 'cargo fmt --all -- --check'
        Script = { cargo fmt --all -- --check }
    },
    @{
        Name   = 'cargo clippy --workspace --all-targets -- -D warnings'
        Script = { cargo clippy --workspace --all-targets -- -D warnings }
    },
    @{
        # Dashboard dist parity: the committed React bundle
        # (crates/relix-web-bridge/dashboard-dist) is the runtime artifact the
        # web-bridge serves, so it must never drift from apps/dashboard/src.
        # This rebuilds the dashboard and fails if the committed dist changed.
        # Non-destructive (only installs deps when node_modules is missing).
        Name   = 'dashboard dist parity (check-dashboard-dist.ps1)'
        Script = { & (Join-Path $RepoRoot 'scripts/check-dashboard-dist.ps1') }
    },
    @{
        # Serial build/test (CARGO_BUILD_JOBS=1 + --test-threads=1)
        # avoids the Windows target-dir flake where parallel rustc
        # invocations race antivirus file locks and fail the link step
        # with "invalid metadata / rlib not found" (E0463) on an
        # otherwise-green tree.
        Name   = 'cargo test --workspace (serial)'
        Script = { $env:CARGO_BUILD_JOBS = '1'; cargo test --workspace -- --test-threads=1 }
    },
    @{
        # Supply-chain gate. --all-features matches what ci.yml's manual
        # `deny` job and the release path check, so license / advisory
        # rejections (e.g. a feature-gated GPL/MPL dependency) surface
        # here BEFORE tagging, not after. Requires `cargo install
        # cargo-deny`.
        Name   = 'cargo deny check --all-features'
        Script = { cargo deny check --all-features }
    }
)

$Failed = $null
foreach ($Step in $Steps) {
    Write-Host ''
    Write-Host "==> $($Step.Name)" -ForegroundColor Cyan
    & $Step.Script
    $Code = $LASTEXITCODE
    Write-Host "    exit code: $Code"
    if ($Code -ne 0) { $Failed = $Step.Name; break }
}

Write-Host ''
if ($null -ne $Failed) {
    Write-Host "CI-LOCAL: FAIL  (first failing gate: $Failed)" -ForegroundColor Red
    exit 1
}
Write-Host 'CI-LOCAL: PASS  (fmt + clippy + serial test + deny all green)' -ForegroundColor Green
exit 0
