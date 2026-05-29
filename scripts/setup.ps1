# scripts/setup.ps1 -- RELIX-7.18 / GAP 17 PART 3
#
# Idempotent operator setup for the research-backed identity
# pipeline. Prompts for ONE of three web-search API keys and
# writes the chosen value to the project-root `.env` file.
#
# Re-running the script after a key is already present in `.env`
# leaves the existing value untouched unless the operator types
# a new value at the prompt.
#
# Usage:
#   ./scripts/setup.ps1
#
# Environment:
#   $env:RELIX_ENV_FILE -- override the target `.env` path
#                         (default: <project-root>\.env).

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir '..')
$EnvFile     = if ($env:RELIX_ENV_FILE) { $env:RELIX_ENV_FILE } else { Join-Path $ProjectRoot '.env' }

$Providers    = @('tavily', 'brave', 'perplexity')
$EnvVars      = @('TAVILY_API_KEY', 'BRAVE_SEARCH_API_KEY', 'PERPLEXITY_API_KEY')
$Descriptions = @(
    'Tavily (https://tavily.com -- research-tuned, generous free tier)',
    'Brave Search (https://api.search.brave.com -- privacy-first, pay-as-you-go)',
    'Perplexity (https://docs.perplexity.ai -- citation-rich answers)'
)

Write-Host '=============================================================='
Write-Host ' Relix research-backed identity setup (RELIX-7.18 / GAP 17)'
Write-Host '--------------------------------------------------------------'
Write-Host ' Pick a search provider and paste its API key. The chosen key'
Write-Host ' is written to .env at the project root. Re-running this'
Write-Host ' script keeps any value you do not overwrite.'
Write-Host '=============================================================='
Write-Host ''
Write-Host 'Available providers:'
for ($i = 0; $i -lt $Providers.Length; $i++) {
    $n = $i + 1
    Write-Host ("  {0}) {1,-11} -- {2}" -f $n, $Providers[$i], $Descriptions[$i])
}

$choice = ''
while ($choice -notmatch '^[1-3]$') {
    $choice = Read-Host 'Pick [1-3]'
    if ($choice -notmatch '^[1-3]$') {
        Write-Host '  please enter 1, 2, or 3'
    }
}

$idx      = [int]$choice - 1
$provider = $Providers[$idx]
$var      = $EnvVars[$idx]

# Check if the chosen var is already populated in .env.
$existing = ''
if (Test-Path $EnvFile) {
    $existingLine = Select-String -Path $EnvFile -Pattern ("^{0}=" -f [regex]::Escape($var)) -ErrorAction SilentlyContinue |
                    Select-Object -Last 1
    if ($existingLine) {
        $existing = $existingLine.Line -replace ("^{0}=" -f [regex]::Escape($var)), ''
    }
}

if ($existing -ne '') {
    $masked = if ($existing.Length -ge 8) {
        "{0}...{1}" -f $existing.Substring(0, 4), $existing.Substring($existing.Length - 4, 4)
    } else {
        '****'
    }
    $replace = Read-Host ("  {0} already set ({1}). Replace? [y/N]" -f $var, $masked)
    if ($replace -notmatch '^(y|Y|yes|YES)$') {
        Write-Host '  keeping existing value; nothing to do.'
        exit 0
    }
}

$key = ''
while ([string]::IsNullOrWhiteSpace($key)) {
    $secure = Read-Host ("  Paste your {0} API key" -f $provider) -AsSecureString
    $bstr   = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        $key = [System.Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
    } finally {
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
    if ([string]::IsNullOrWhiteSpace($key)) {
        Write-Host '  key cannot be empty'
    }
}

if (-not (Test-Path $EnvFile)) {
    New-Item -ItemType File -Path $EnvFile -Force | Out-Null
}

# Rewrite the file with the chosen line replaced (or appended).
$lines = @()
if (Test-Path $EnvFile) {
    $lines = Get-Content -LiteralPath $EnvFile -ErrorAction SilentlyContinue
    if (-not $lines) { $lines = @() }
}

$written  = $false
$rewritten = @()
foreach ($line in $lines) {
    if ($line -like ("{0}=*" -f $var)) {
        $rewritten += ("{0}={1}" -f $var, $key)
        $written = $true
    } else {
        $rewritten += $line
    }
}
if (-not $written) {
    $rewritten += ("{0}={1}" -f $var, $key)
}

Set-Content -LiteralPath $EnvFile -Value $rewritten -Encoding utf8

Write-Host ''
Write-Host ("Wrote {0} to {1}." -f $var, $EnvFile)
Write-Host 'Enable the pipeline by setting:'
Write-Host ''
Write-Host '  [session_identity.research]'
Write-Host '  enabled = true'
Write-Host ''
Write-Host '  [session_identity.web_search]'
Write-Host '  enabled  = true'
Write-Host '  provider = "auto"'
Write-Host ''
Write-Host 'in your controller config TOML.'
