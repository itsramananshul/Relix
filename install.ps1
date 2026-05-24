# Relix installer for Windows (PowerShell 5.1 and PowerShell 7+).
# Downloads the latest pre-built release from GitHub and installs the
# `relix.exe` binary (and any sibling .exe files) into the user's bin dir.
#
# Usage:
#   iwr -useb https://raw.githubusercontent.com/itsramananshul/Relix/main/install.ps1 | iex
#   $env:RELIX_VERSION = 'v0.1.0'; .\install.ps1
#   $env:RELIX_INSTALL_DIR = 'C:\tools\relix'; .\install.ps1

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Repo        = 'itsramananshul/Relix'
$ReleasesApi = "https://api.github.com/repos/$Repo/releases/latest"
$ReleasesDl  = "https://github.com/$Repo/releases/download"

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

# Track temp paths for cleanup
$TmpZip     = $null
$TmpExtract = $null

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
    # 4. Build download URL
    # -----------------------------------------------------------------------
    $archiveName = "relix-$target.zip"
    $downloadUrl = "$ReleasesDl/$tag/$archiveName"
    Write-Host "Download URL:      $downloadUrl"

    # -----------------------------------------------------------------------
    # 5. Download + extract + install
    # -----------------------------------------------------------------------
    $TmpZip     = Join-Path $env:TEMP 'relix-install.zip'
    $TmpExtract = Join-Path $env:TEMP ("relix-install-" + [Guid]::NewGuid().ToString('N'))

    if (Test-Path -LiteralPath $TmpZip) {
        Remove-Item -LiteralPath $TmpZip -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $TmpExtract) {
        Remove-Item -LiteralPath $TmpExtract -Recurse -Force -ErrorAction SilentlyContinue
    }
    New-Item -ItemType Directory -Path $TmpExtract -Force | Out-Null

    Write-Host "Downloading archive..."
    try {
        Invoke-WebRequest -Uri $downloadUrl -OutFile $TmpZip -UseBasicParsing -Headers @{ 'User-Agent' = 'relix-installer' }
    } catch {
        Write-Error "download failed: $downloadUrl : $($_.Exception.Message)"
        return
    }

    if (-not (Test-Path -LiteralPath $TmpZip) -or (Get-Item -LiteralPath $TmpZip).Length -eq 0) {
        Write-Error "downloaded archive is empty: $TmpZip"
        return
    }

    Write-Host "Extracting archive..."
    try {
        Expand-Archive -LiteralPath $TmpZip -DestinationPath $TmpExtract -Force
    } catch {
        Write-Error "extraction failed: $($_.Exception.Message)"
        return
    }

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
    # 5b. Mesh scripts
    #
    # 'relix boot' spawns the mesh through scripts/relix-mesh-up.ps1;
    # users who installed via 'irm | iex' don't have a repo checkout.
    # Drop the two scripts in $env:USERPROFILE\.local\scripts\ — the
    # relix-cli locate_script helper falls back to this path after the
    # repo and binary-dir lookups.
    # -----------------------------------------------------------------------
    $ScriptsDir = Join-Path $env:USERPROFILE '.local\scripts'
    if (-not (Test-Path -LiteralPath $ScriptsDir)) {
        try {
            New-Item -ItemType Directory -Path $ScriptsDir -Force | Out-Null
        } catch {
            Write-Host "warning: could not create $ScriptsDir ($($_.Exception.Message))"
        }
    }
    $meshBaseUrl = "https://raw.githubusercontent.com/$Repo/main/scripts"
    foreach ($script in @('relix-mesh-up.ps1', 'relix-mesh-down.ps1')) {
        $target = Join-Path $ScriptsDir $script
        try {
            Invoke-WebRequest -Uri "$meshBaseUrl/$script" -OutFile $target `
                -UseBasicParsing -Headers @{ 'User-Agent' = 'relix-installer' }
            Write-Host "  installed: $target"
        } catch {
            Write-Host "warning: could not fetch $script ($($_.Exception.Message))"
            Write-Host "         relix boot will require a repo checkout"
        }
    }

    # -----------------------------------------------------------------------
    # 5c. Flow templates
    #
    # The bridge reads `flows/chat_template.sol` (and friends) at start
    # to wire its OpenAI-compat / tool-routing flow VMs. The mesh script
    # resolves the `flows/` directory next to itself first; drop the
    # templates in $env:USERPROFILE\.local\flows\ so that probe hits on
    # a clean binary install.
    # -----------------------------------------------------------------------
    $FlowsDir = Join-Path $env:USERPROFILE '.local\flows'
    if (-not (Test-Path -LiteralPath $FlowsDir)) {
        try {
            New-Item -ItemType Directory -Path $FlowsDir -Force | Out-Null
        } catch {
            Write-Host "warning: could not create $FlowsDir ($($_.Exception.Message))"
        }
    }
    $flowsBaseUrl = "https://raw.githubusercontent.com/$Repo/main/flows"
    foreach ($flow in @('chat_template.sol', 'chat.sol', 'chat_with_tool.sol', 'chat_with_retry.sflow')) {
        $target = Join-Path $FlowsDir $flow
        try {
            Invoke-WebRequest -Uri "$flowsBaseUrl/$flow" -OutFile $target `
                -UseBasicParsing -Headers @{ 'User-Agent' = 'relix-installer' }
            Write-Host "  installed: $target"
        } catch {
            Write-Host "warning: could not fetch $flow ($($_.Exception.Message))"
            Write-Host "         relix boot will need a repo checkout for flows"
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
}
