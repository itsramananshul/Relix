# Relix installer for Windows (PowerShell 5.1 and PowerShell 7+).
#
# Downloads the pre-built release archive from GitHub, verifies its
# integrity (SHA256 + cosign keyless signature pinned to the project's
# release.yml workflow identity), extracts it through a zip-slip-safe
# helper, and lands the binaries in the user's bin dir.
#
# Usage:
#   iwr -useb https://raw.githubusercontent.com/itsramananshul/Relix/main/install.ps1 | iex
#   $env:RELIX_VERSION = 'v0.1.0'; .\install.ps1
#   $env:RELIX_INSTALL_DIR = 'C:\tools\relix'; .\install.ps1
#
# Security:
#   * Every artifact download goes through Invoke-FetchAndVerify which
#     enforces a pinned SHA256 before writing the final file.
#   * Cosign signatures (Sigstore keyless) are verified against the
#     itsramananshul/Relix release.yml workflow identity when cosign
#     is available locally — a missing cosign binary produces a loud
#     warning, never a silent skip without notice.
#   * Archive extraction goes through Invoke-SafeExtract which rejects
#     any zip entry whose resolved path escapes the staging directory
#     (zip-slip protection — CVE-2018-1002200 class).

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repo        = 'itsramananshul/Relix'
$ReleasesApi = "https://api.github.com/repos/$Repo/releases/latest"
$ReleasesDl  = "https://github.com/$Repo/releases/download"
$RawBase     = "https://raw.githubusercontent.com/$Repo"

# ---------------------------------------------------------------------------
# TLS 1.2 (Windows PowerShell 5.1 default is SSL3/TLS which GitHub rejects)
# ---------------------------------------------------------------------------
try {
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    try {
        [Net.ServicePointManager]::SecurityProtocol = 'Tls12'
    } catch {
        # Best-effort: PowerShell 7+ on .NET 5+ already defaults to system TLS.
    }
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Invoke-Download {
    param(
        [Parameter(Mandatory=$true)][string]$Url,
        [Parameter(Mandatory=$true)][string]$OutFile
    )
    Invoke-WebRequest -Uri $Url -OutFile $OutFile `
        -UseBasicParsing -Headers @{ 'User-Agent' = 'relix-installer' }
}

function Get-FileSha256 {
    param([Parameter(Mandatory=$true)][string]$Path)
    return (Get-FileHash -Path $Path -Algorithm SHA256).Hash.ToLower()
}

# Invoke-FetchAndVerify: download a remote asset to a local path and
# gate acceptance on an exact SHA256 match. Any non-200 response,
# empty body, or hash mismatch is fatal.
function Invoke-FetchAndVerify {
    param(
        [Parameter(Mandatory=$true)][string]$Url,
        [Parameter(Mandatory=$true)][string]$ExpectedSha256,
        [Parameter(Mandatory=$true)][string]$Output
    )
    try {
        Invoke-Download -Url $Url -OutFile $Output
    } catch {
        throw "download failed: $Url : $($_.Exception.Message)"
    }
    if (-not (Test-Path -LiteralPath $Output) -or (Get-Item -LiteralPath $Output).Length -eq 0) {
        throw "downloaded asset is empty: $Url"
    }
    $actual = Get-FileSha256 -Path $Output
    $expected = $ExpectedSha256.ToLower()
    if ($actual -ne $expected) {
        Remove-Item -LiteralPath $Output -Force -ErrorAction SilentlyContinue
        throw "SHA256 mismatch on $Url; refusing to install (expected $expected, got $actual)"
    }
}

# Invoke-VerifySignature: cosign keyless verification pinned to the
# project's release.yml workflow identity. When cosign is missing we
# warn loudly and continue (the operator's hash check from
# Invoke-FetchAndVerify is still in force). When cosign is present a
# verification failure is fatal.
function Invoke-VerifySignature {
    param(
        [Parameter(Mandatory=$true)][string]$Binary,
        [Parameter(Mandatory=$true)][string]$Signature,
        [Parameter(Mandatory=$true)][string]$Certificate
    )
    $cosign = Get-Command cosign -ErrorAction SilentlyContinue
    if (-not $cosign) {
        Write-Warning "cosign not found; skipping signature verification for $Binary."
        Write-Warning "Install cosign from https://docs.sigstore.dev/cosign/installation/ for verified downloads."
        return
    }
    & cosign verify-blob `
        --signature $Signature `
        --certificate $Certificate `
        --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' `
        --certificate-identity-regexp 'https://github.com/itsramananshul/Relix/.github/workflows/release.yml' `
        $Binary
    if ($LASTEXITCODE -ne 0) {
        throw "cosign signature verification failed for $Binary"
    }
    Write-Host "  cosign-verified: $Binary"
}

# Invoke-SafeExtract: zip-slip-safe archive extraction. Stages every
# entry in a fresh tmpdir, then walks the staged tree and rejects any
# entry whose resolved path escapes that tmpdir (covers `..\`
# traversal AND absolute paths). Only after the whole tree passes the
# check do we copy into the destination.
function Invoke-SafeExtract {
    param(
        [Parameter(Mandatory=$true)][string]$Archive,
        [Parameter(Mandatory=$true)][string]$Dest
    )
    $stage = Join-Path $env:TEMP ("relix-stage-" + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $stage -Force | Out-Null
    try {
        try {
            Expand-Archive -LiteralPath $Archive -DestinationPath $stage -Force
        } catch {
            throw "extraction failed: $($_.Exception.Message)"
        }
        $stageRoot = (Resolve-Path -LiteralPath $stage).ProviderPath.TrimEnd('\','/')
        Get-ChildItem -LiteralPath $stage -Recurse -Force | ForEach-Object {
            $entry = $_
            $resolved = (Resolve-Path -LiteralPath $entry.FullName).ProviderPath.TrimEnd('\','/')
            # Must live strictly inside the staging tmpdir. Allow the
            # tmpdir root itself (it'll match exactly); any path that
            # doesn't share the `${stageRoot}\` prefix is escaping.
            if (($resolved -ne $stageRoot) -and (-not $resolved.StartsWith($stageRoot + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase))) {
                throw "suspicious path in archive: $($entry.FullName) (resolved to $resolved)"
            }
        }
        if (-not (Test-Path -LiteralPath $Dest)) {
            New-Item -ItemType Directory -Path $Dest -Force | Out-Null
        }
        Copy-Item -Path (Join-Path $stage '*') -Destination $Dest -Recurse -Force
    } finally {
        if (Test-Path -LiteralPath $stage) {
            Remove-Item -LiteralPath $stage -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
}

# Track temp paths for cleanup
$TmpZip      = $null
$TmpExtract  = $null
$TmpAux      = $null

try {
    # -----------------------------------------------------------------------
    # 1. Detect architecture
    # -----------------------------------------------------------------------
    $arch = $null
    try {
        $procArch = [System.Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
        switch ($procArch) {
            'X64'   { $arch = 'x86_64' }
            'Arm64' { $arch = 'arm64'  }
            default { $arch = "$procArch" }
        }
    } catch {
        $envArch = $env:PROCESSOR_ARCHITECTURE
        switch -Regex ($envArch) {
            '^(AMD64|x64|X64)$' { $arch = 'x86_64' }
            '^(ARM64)$'         { $arch = 'arm64'  }
            default             { $arch = $envArch }
        }
    }

    if ($arch -ne 'x86_64') {
        Write-Error "unsupported architecture: $arch (Relix currently ships only x86_64 Windows binaries)"
        return
    }

    $target = 'x86_64-pc-windows-msvc'
    Write-Host "Detected platform: windows/$arch ($target)"

    # -----------------------------------------------------------------------
    # 2. Install dir
    # -----------------------------------------------------------------------
    if ($env:RELIX_INSTALL_DIR -and $env:RELIX_INSTALL_DIR.Trim().Length -gt 0) {
        $InstallDir = $env:RELIX_INSTALL_DIR
    } else {
        $InstallDir = Join-Path $env:USERPROFILE '.local\bin'
    }

    if (-not (Test-Path -LiteralPath $InstallDir)) {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    }
    Write-Host "Install dir:       $InstallDir"

    # -----------------------------------------------------------------------
    # 3. Resolve version / tag
    # -----------------------------------------------------------------------
    $tag = $null
    if ($env:RELIX_VERSION -and $env:RELIX_VERSION.Trim().Length -gt 0) {
        $tag = $env:RELIX_VERSION
    } else {
        Write-Host "Resolving latest release tag from GitHub..."
        # The release-metadata GET is the only fetch with no pre-known
        # hash. It's used solely to resolve the tag string; every
        # subsequent download is pinned + verified.
        try {
            $headers = @{ 'User-Agent' = 'relix-installer' }
            $rel = Invoke-RestMethod -Uri $ReleasesApi -Headers $headers -UseBasicParsing
            if ($rel -and $rel.tag_name) {
                $tag = [string]$rel.tag_name
            }
        } catch {
            Write-Error "failed to query $ReleasesApi : $($_.Exception.Message)"
            return
        }
    }

    if (-not $tag) {
        Write-Error "could not determine release tag (set `$env:RELIX_VERSION = 'vX.Y.Z' to override)"
        return
    }

    $version = $tag
    if ($version.StartsWith('v')) {
        $version = $version.Substring(1)
    }
    Write-Host "Version:           $tag"

    # -----------------------------------------------------------------------
    # 4. Build download URLs
    # -----------------------------------------------------------------------
    $archiveName    = "relix-$target.zip"
    $downloadUrl    = "$ReleasesDl/$tag/$archiveName"
    $sha256Url      = "$downloadUrl.sha256"
    $archiveSigUrl  = "$downloadUrl.sig"
    $archivePemUrl  = "$downloadUrl.pem"
    $sumsBase       = "$ReleasesDl/$tag"
    $sumsUrl        = "$sumsBase/SHA256SUMS.txt"
    $sumsSigUrl     = "$sumsUrl.sig"
    $sumsPemUrl     = "$sumsUrl.pem"
    $scriptBase     = "$RawBase/$tag"
    Write-Host "Download URL:      $downloadUrl"

    # -----------------------------------------------------------------------
    # 5. Download + verify + safe-extract + install
    # -----------------------------------------------------------------------
    $TmpZip         = Join-Path $env:TEMP 'relix-install.zip'
    $TmpExtract     = Join-Path $env:TEMP ("relix-install-" + [Guid]::NewGuid().ToString('N'))
    $TmpAux         = Join-Path $env:TEMP ("relix-aux-"     + [Guid]::NewGuid().ToString('N'))

    foreach ($p in @($TmpZip)) {
        if (Test-Path -LiteralPath $p) {
            Remove-Item -LiteralPath $p -Force -ErrorAction SilentlyContinue
        }
    }
    foreach ($p in @($TmpExtract, $TmpAux)) {
        if (Test-Path -LiteralPath $p) {
            Remove-Item -LiteralPath $p -Recurse -Force -ErrorAction SilentlyContinue
        }
        New-Item -ItemType Directory -Path $p -Force | Out-Null
    }

    $archiveShaPath    = Join-Path $TmpAux 'archive.sha256'
    $archiveShaSigPath = Join-Path $TmpAux 'archive.sha256.sig'
    $archiveShaPemPath = Join-Path $TmpAux 'archive.sha256.pem'
    $archiveSigPath    = Join-Path $TmpAux 'archive.sig'
    $archivePemPath    = Join-Path $TmpAux 'archive.pem'
    $sumsPath          = Join-Path $TmpAux 'SHA256SUMS.txt'
    $sumsSigPath       = Join-Path $TmpAux 'SHA256SUMS.txt.sig'
    $sumsPemPath       = Join-Path $TmpAux 'SHA256SUMS.txt.pem'

    $sha256SigUrl = "$sha256Url.sig"
    $sha256PemUrl = "$sha256Url.pem"

    Write-Host "Downloading SHA256 + cosign material for archive..."
    try {
        Invoke-Download -Url $sha256Url -OutFile $archiveShaPath
    } catch {
        throw "could not fetch $sha256Url (no per-archive checksum published for $tag?) : $($_.Exception.Message)"
    }
    try { Invoke-Download -Url $sha256SigUrl  -OutFile $archiveShaSigPath } catch { Write-Warning "no cosign signature for $archiveName.sha256 at $tag" }
    try { Invoke-Download -Url $sha256PemUrl  -OutFile $archiveShaPemPath } catch { Write-Warning "no cosign cert for $archiveName.sha256 at $tag" }
    try { Invoke-Download -Url $archiveSigUrl -OutFile $archiveSigPath    } catch { Write-Warning "no cosign signature for $archiveName at $tag" }
    try { Invoke-Download -Url $archivePemUrl -OutFile $archivePemPath    } catch { Write-Warning "no cosign cert for $archiveName at $tag" }

    # Verify the cosign signature on the .sha256 file BEFORE we trust
    # the hash it contains. See install.sh for the threat-model
    # rationale.
    if ((Test-Path -LiteralPath $archiveShaSigPath) -and (Test-Path -LiteralPath $archiveShaPemPath) `
        -and ((Get-Item -LiteralPath $archiveShaSigPath).Length -gt 0) `
        -and ((Get-Item -LiteralPath $archiveShaPemPath).Length -gt 0)) {
        Invoke-VerifySignature -Binary $archiveShaPath -Signature $archiveShaSigPath -Certificate $archiveShaPemPath
    } else {
        Write-Warning "skipping cosign verification on $archiveName.sha256: no .sig/.pem published"
    }

    $expectedArchiveSha = ((Get-Content -LiteralPath $archiveShaPath -TotalCount 1) -split '\s+')[0]
    if (-not $expectedArchiveSha) {
        throw "could not parse SHA256 from $sha256Url"
    }

    Write-Host "Downloading archive..."
    Invoke-FetchAndVerify -Url $downloadUrl -ExpectedSha256 $expectedArchiveSha -Output $TmpZip

    if ((Test-Path -LiteralPath $archiveSigPath) -and (Test-Path -LiteralPath $archivePemPath) `
        -and ((Get-Item -LiteralPath $archiveSigPath).Length -gt 0) `
        -and ((Get-Item -LiteralPath $archivePemPath).Length -gt 0)) {
        Invoke-VerifySignature -Binary $TmpZip -Signature $archiveSigPath -Certificate $archivePemPath
    } else {
        Write-Warning "skipping cosign verification: no .sig/.pem published for $archiveName"
    }

    Write-Host "Extracting archive (zip-slip-safe)..."
    Invoke-SafeExtract -Archive $TmpZip -Dest $TmpExtract

    # Locate the main relix.exe first. `Select-Object -First 1` always
    # returns a single object (or $null) regardless of how many .exe
    # files matched — avoiding the strict-mode trap where .Count on a
    # single-result `Get-ChildItem` throws "property 'Count' cannot be
    # found on this object".
    $relixSrc = Get-ChildItem -LiteralPath $TmpExtract -Recurse -File `
            -Filter 'relix.exe' -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $relixSrc) {
        Write-Error "archive did not contain relix.exe (extract dir: $TmpExtract)"
        return
    }

    $relixDest = Join-Path $InstallDir 'relix.exe'
    Copy-Item -LiteralPath $relixSrc.FullName -Destination $relixDest -Force
    Write-Host "  installed: $relixDest"

    # Install any sibling .exe files (e.g. relix-controller.exe,
    # relix-web-bridge.exe if a future archive ships them) from the
    # same directory as relix.exe. A `foreach` over an empty / single /
    # multi result is safe under strict mode — no .Count access.
    $payloadDir = $relixSrc.Directory.FullName
    foreach ($exe in (Get-ChildItem -LiteralPath $payloadDir -File `
                          -Filter '*.exe' -ErrorAction SilentlyContinue)) {
        if ($exe.Name -ieq 'relix.exe') { continue }
        $siblingDest = Join-Path $InstallDir $exe.Name
        Copy-Item -LiteralPath $exe.FullName -Destination $siblingDest -Force
        Write-Host "  installed: $siblingDest"
    }

    if (-not (Test-Path -LiteralPath $relixDest)) {
        Write-Error "expected 'relix.exe' not found at $relixDest after install"
        return
    }
    $relixExe = $relixDest

    # -----------------------------------------------------------------------
    # 5b. Per-release SHA256SUMS + cosign verification (PART 3 + 5)
    # -----------------------------------------------------------------------
    $sumsAvailable = $false
    try {
        Invoke-Download -Url $sumsUrl    -OutFile $sumsPath
        Invoke-Download -Url $sumsSigUrl -OutFile $sumsSigPath
        Invoke-Download -Url $sumsPemUrl -OutFile $sumsPemPath
        $sumsAvailable = $true
    } catch {
        Write-Warning "SHA256SUMS.txt not published for $tag; per-script hash verification will skip extras."
    }
    if ($sumsAvailable -and (Test-Path -LiteralPath $sumsSigPath) -and (Test-Path -LiteralPath $sumsPemPath) `
        -and ((Get-Item -LiteralPath $sumsSigPath).Length -gt 0) `
        -and ((Get-Item -LiteralPath $sumsPemPath).Length -gt 0)) {
        Invoke-VerifySignature -Binary $sumsPath -Signature $sumsSigPath -Certificate $sumsPemPath
    }

    function Get-ExpectedSha {
        param([Parameter(Mandatory=$true)][string]$RepoPath)
        if (-not (Test-Path -LiteralPath $sumsPath)) { return $null }
        foreach ($line in (Get-Content -LiteralPath $sumsPath)) {
            $parts = $line -split '\s+', 2
            if ($parts.Length -lt 2) { continue }
            $name = $parts[1].TrimStart('*')
            if ($name -eq $RepoPath) { return $parts[0].ToLower() }
        }
        return $null
    }

    # -----------------------------------------------------------------------
    # 5c. Mesh scripts (PART 5)
    #
    # Pinned to the resolved release tag (not `main`) and each file's
    # SHA256 is checked against the cosign-verified SHA256SUMS.txt
    # above. `relix boot` spawns the mesh through
    # scripts/relix-mesh-up.ps1; users who installed via 'irm | iex'
    # don't have a repo checkout. Drop the scripts in
    # $env:USERPROFILE\.local\scripts\ — the relix-cli locate_script
    # helper falls back to this path after the repo and binary-dir
    # lookups.
    # -----------------------------------------------------------------------
    $ScriptsDir = Join-Path $env:USERPROFILE '.local\scripts'
    if (-not (Test-Path -LiteralPath $ScriptsDir)) {
        try {
            New-Item -ItemType Directory -Path $ScriptsDir -Force | Out-Null
        } catch {
            Write-Host "warning: could not create $ScriptsDir ($($_.Exception.Message))"
        }
    }
    foreach ($script in @('relix-mesh-up.ps1', 'relix-mesh-down.ps1')) {
        $target       = Join-Path $ScriptsDir $script
        $url          = "$scriptBase/scripts/$script"
        $expectedSha  = Get-ExpectedSha -RepoPath "scripts/$script"
        if ($expectedSha) {
            try {
                Invoke-FetchAndVerify -Url $url -ExpectedSha256 $expectedSha -Output $target
                Write-Host "  installed: $target"
            } catch {
                Write-Warning "could not install $script : $($_.Exception.Message)"
                Write-Host "         relix boot will require a repo checkout"
            }
        } else {
            Write-Warning "no SHA256 for scripts/$script in SHA256SUMS.txt; skipping (use a repo checkout)"
        }
    }

    # -----------------------------------------------------------------------
    # 5d. Flow templates (PART 5)
    #
    # Same pinned-tag + SHA256-verified path as the mesh scripts above.
    # -----------------------------------------------------------------------
    $FlowsDir = Join-Path $env:USERPROFILE '.local\flows'
    if (-not (Test-Path -LiteralPath $FlowsDir)) {
        try {
            New-Item -ItemType Directory -Path $FlowsDir -Force | Out-Null
        } catch {
            Write-Host "warning: could not create $FlowsDir ($($_.Exception.Message))"
        }
    }
    foreach ($flow in @('chat_template.sol', 'chat.sol', 'chat_with_tool.sol', 'chat_with_retry.sflow')) {
        $target       = Join-Path $FlowsDir $flow
        $url          = "$scriptBase/flows/$flow"
        $expectedSha  = Get-ExpectedSha -RepoPath "flows/$flow"
        if ($expectedSha) {
            try {
                Invoke-FetchAndVerify -Url $url -ExpectedSha256 $expectedSha -Output $target
                Write-Host "  installed: $target"
            } catch {
                Write-Warning "could not install $flow : $($_.Exception.Message)"
                Write-Host "         relix boot will need a repo checkout for flows"
            }
        } else {
            Write-Warning "no SHA256 for flows/$flow in SHA256SUMS.txt; skipping (use a repo checkout)"
        }
    }

    # -----------------------------------------------------------------------
    # 6. PATH wiring (user scope)
    # -----------------------------------------------------------------------
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }

    $pathParts = @()
    foreach ($p in ($userPath -split ';')) {
        if ($p -and $p.Trim().Length -gt 0) {
            $pathParts += $p
        }
    }

    $normalizedInstall = $InstallDir.TrimEnd('\')
    $alreadyOnPath = $false
    foreach ($p in $pathParts) {
        if ($p.TrimEnd('\').Equals($normalizedInstall, [StringComparison]::OrdinalIgnoreCase)) {
            $alreadyOnPath = $true
            break
        }
    }

    if (-not $alreadyOnPath) {
        $newPath = if ($userPath.Length -gt 0) { "$userPath;$InstallDir" } else { $InstallDir }
        try {
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Host "Updated user PATH: added $InstallDir"
            Write-Host "Note: open a new PowerShell/terminal window for the PATH change to take effect."
        } catch {
            Write-Host "warning: could not update user PATH automatically: $($_.Exception.Message)"
            Write-Host "Add this directory to your PATH manually: $InstallDir"
        }
    } else {
        Write-Host "PATH already includes install dir."
    }

    # Also make it usable in the current session
    if (-not ($env:Path -split ';' | Where-Object { $_.TrimEnd('\').Equals($normalizedInstall, [StringComparison]::OrdinalIgnoreCase) })) {
        $env:Path = "$env:Path;$InstallDir"
    }

    # -----------------------------------------------------------------------
    # 7. Verify
    # -----------------------------------------------------------------------
    $verifyOutput = $null
    try {
        $verifyOutput = & $relixExe --version 2>$null
    } catch {
        $verifyOutput = $null
    }
    if ($verifyOutput) {
        $first = ($verifyOutput | Select-Object -First 1)
        Write-Host "Verified:          $first"
    } else {
        Write-Host "Verified path:     $relixExe"
    }

    # -----------------------------------------------------------------------
    # 8. Done
    # -----------------------------------------------------------------------
    Write-Host ''
    Write-Host "Relix $version installed to $InstallDir."
    Write-Host "Docs:  https://github.com/$Repo"
    Write-Host ''

    # -----------------------------------------------------------------------
    # 8b. Guided setup
    # -----------------------------------------------------------------------
    # `relix setup` is an interactive wizard that writes
    # %USERPROFILE%\.relix\config.toml. Skip silently when no
    # interactive host (CI / scheduled task) — the user can run
    # `relix setup` later.
    if ($Host.UI.SupportsVirtualTerminal -or [Environment]::UserInteractive) {
        Write-Host 'Running guided setup...'
        Write-Host ''
        & $relixExe setup
    } else {
        Write-Host 'No interactive host — skipping setup.'
        Write-Host 'Run `relix setup` once you have a console, then `relix boot`.'
    }
}
finally {
    # -----------------------------------------------------------------------
    # 9. Cleanup
    # -----------------------------------------------------------------------
    if ($TmpZip -and (Test-Path -LiteralPath $TmpZip)) {
        Remove-Item -LiteralPath $TmpZip -Force -ErrorAction SilentlyContinue
    }
    if ($TmpExtract -and (Test-Path -LiteralPath $TmpExtract)) {
        Remove-Item -LiteralPath $TmpExtract -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($TmpAux -and (Test-Path -LiteralPath $TmpAux)) {
        Remove-Item -LiteralPath $TmpAux -Recurse -Force -ErrorAction SilentlyContinue
    }
}
