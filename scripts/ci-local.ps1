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
        # Supply-chain gate for the default first-release graph. Deferred
        # optional feature families (browser-headless-chrome,
        # browser-webdriver, terminal-pty) currently pull policy-blocked
        # transitives and are documented in docs/dependency-policy.md; do not
        # ship them until their dependency review/replacement work is closed.
        # Requires `cargo install cargo-deny`.
        Name   = 'cargo deny check'
        Script = { cargo deny check }
    },
    @{
        # LIVE first-release boot smoke. This is the release gate the GitHub
        # matrix deliberately does NOT run: it boots a real isolated mesh +
        # bridge over HTTP, authenticates the dashboard session path, hits the
        # core dashboard APIs (with a no-session negative control), and runs one
        # real Brief end-to-end on the safe local echo Rig — zero external model
        # spend. -SkipBuild reuses the binaries the serial test gate above just
        # built; -RequireEchoFlow makes the echo product flow a hard failure, so
        # a regression that breaks the first user-visible loop fails CI here.
        # Kept local (not in ci.yml) because a live multi-process mesh boot is
        # not a reliable hosted-runner gate; see docs/ci-strategy.md.
        Name   = 'first-release live smoke (smoke-first-release.ps1 -SkipBuild -RequireEchoFlow)'
        Script = { & (Join-Path $RepoRoot 'scripts/smoke-first-release.ps1') -SkipBuild -RequireEchoFlow }
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
Write-Host 'CI-LOCAL: PASS  (fmt + clippy + serial test + deny + first-release live smoke all green)' -ForegroundColor Green
exit 0
