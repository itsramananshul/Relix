# scripts/setup.ps1 — Relix idempotent operator setup.
#
# Walks the operator through every optional API key Relix uses
# and writes the chosen values to the project-root `.env` file.
# Re-running the script keeps any value the operator does NOT
# overwrite -- every prompt accepts a blank line to skip and
# offers to keep an existing value when it is already set.
#
# Sections:
#   1. Web search (RELIX-7.18 / GAP 17):
#        TAVILY_API_KEY | BRAVE_SEARCH_API_KEY | PERPLEXITY_API_KEY
#   2. Document parsing and web reading (GAP 10 PART 1 / 2):
#        LLAMA_CLOUD_API_KEY | JINA_API_KEY | FIRECRAWL_API_KEY
#   3. Screen capture (GAP 10 PART 3): RELIX_SCREEN_ENABLED.
#
# Usage:
#   ./scripts/setup.ps1
#
# Environment:
#   $env:RELIX_ENV_FILE -- override the target `.env` path
#                          (default: <project-root>\.env).

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir '..')
$EnvFile     = if ($env:RELIX_ENV_FILE) { $env:RELIX_ENV_FILE } else { Join-Path $ProjectRoot '.env' }

if (-not (Test-Path $EnvFile)) {
    New-Item -ItemType File -Path $EnvFile -Force | Out-Null
}

function Get-EnvValue([string]$Var) {
    if (-not (Test-Path $EnvFile)) { return '' }
    $line = Select-String -Path $EnvFile -Pattern ("^{0}=" -f [regex]::Escape($Var)) -ErrorAction SilentlyContinue |
            Select-Object -Last 1
    if ($line) {
        return ($line.Line -replace ("^{0}=" -f [regex]::Escape($Var)), '')
    }
    return ''
}

function Set-EnvValue([string]$Var, [string]$Value) {
    $lines = @()
    if (Test-Path $EnvFile) {
        $lines = Get-Content -LiteralPath $EnvFile -ErrorAction SilentlyContinue
        if (-not $lines) { $lines = @() }
    }
    $written  = $false
    $rewritten = @()
    foreach ($line in $lines) {
        if ($line -like ("{0}=*" -f $Var)) {
            $rewritten += ("{0}={1}" -f $Var, $Value)
            $written = $true
        } else {
            $rewritten += $line
        }
    }
    if (-not $written) {
        $rewritten += ("{0}={1}" -f $Var, $Value)
    }
    Set-Content -LiteralPath $EnvFile -Value $rewritten -Encoding utf8
}

function Format-Masked([string]$Value) {
    if ($Value.Length -lt 8) { return '****' }
    return ("{0}...{1}" -f $Value.Substring(0, 4), $Value.Substring($Value.Length - 4, 4))
}

function Read-Secret([string]$Prompt) {
    $secure = Read-Host $Prompt -AsSecureString
    $bstr   = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        return [System.Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
    } finally {
        [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
    }
}

function Prompt-Secret([string]$Var, [string]$Label, [string]$Url) {
    $existing = Get-EnvValue $Var
    if ($existing -ne '') {
        Write-Host ("  {0} already set ({1})." -f $Var, (Format-Masked $existing))
        $val = Read-Host "    Enter a new value (or press Enter to keep)"
        if ([string]::IsNullOrWhiteSpace($val)) {
            Write-Host "    kept existing value."
            return
        }
        Set-EnvValue $Var $val
        Write-Host ("    wrote {0}." -f $Var)
    } else {
        Write-Host "  $Label"
        Write-Host "    See: $Url"
        $val = Read-Secret "    Enter key (or press Enter to skip)"
        if ([string]::IsNullOrWhiteSpace($val)) {
            Write-Host "    skipped."
            return
        }
        Set-EnvValue $Var $val
        Write-Host ("    wrote {0}." -f $Var)
    }
}

function Prompt-YesNo([string]$Var, [string]$Label) {
    $current = Get-EnvValue $Var
    $hint = if ($current -eq 'true') { '[Y/n]' } else { '[y/N]' }
    $ans = Read-Host ("  {0} {1}" -f $Label, $hint)
    switch -Regex ($ans) {
        '^(y|Y|yes|YES)$' { Set-EnvValue $Var 'true';  Write-Host "    enabled." }
        '^(n|N|no|NO)$'   { Set-EnvValue $Var 'false'; Write-Host "    disabled." }
        '^$' {
            if ($current -eq 'true') {
                Write-Host "    kept enabled."
            } else {
                Set-EnvValue $Var 'false'
                Write-Host "    kept disabled."
            }
        }
        default {
            Set-EnvValue $Var 'false'
            Write-Host "    treated as no."
        }
    }
}

Write-Host '=============================================================='
Write-Host ' Relix operator setup'
Write-Host '--------------------------------------------------------------'
Write-Host ' Walks every optional API key + feature toggle Relix supports.'
Write-Host ' Press Enter at any prompt to skip; existing values are kept'
Write-Host ' unless you overwrite them. The script writes'
Write-Host ("   {0}" -f $EnvFile)
Write-Host '=============================================================='

Write-Host ''
Write-Host '=== Web search (research-backed identity, GAP 17) ==='
Write-Host ''
Write-Host 'Pick ONE of the three providers below -- the controller'
Write-Host 'auto-selects the first non-empty key (Tavily -> Brave ->'
Write-Host 'Perplexity). Skipping every prompt keeps the cap dormant.'
Write-Host ''
Prompt-Secret 'TAVILY_API_KEY' 'Tavily -- research-tuned, generous free tier' 'https://tavily.com'
Prompt-Secret 'BRAVE_SEARCH_API_KEY' 'Brave Search -- privacy-first, pay-as-you-go' 'https://api.search.brave.com'
Prompt-Secret 'PERPLEXITY_API_KEY' 'Perplexity -- citation-rich answers' 'https://docs.perplexity.ai'

Write-Host ''
Write-Host '=== Document parsing and web reading (GAP 10) ==='
Write-Host ''
Write-Host 'Cloud document parsing dramatically improves quality on'
Write-Host 'complex PDFs and rich web pages. Skipping every prompt'
Write-Host 'keeps the local-only tier (already on).'
Write-Host ''
Prompt-Secret 'LLAMA_CLOUD_API_KEY' 'LlamaParse -- best-in-class scanned-PDF parsing' 'https://cloud.llamaindex.ai'
Prompt-Secret 'JINA_API_KEY' 'Jina Reader -- clean markdown extraction for URLs' 'https://jina.ai/reader'
Prompt-Secret 'FIRECRAWL_API_KEY' 'Firecrawl -- JS-rendered URL scraping' 'https://firecrawl.dev'

Write-Host ''
Write-Host '=== Screen capture (GAP 10 PART 3) ==='
Write-Host ''
Write-Host "Screen capture lets agents see the host's screen via"
Write-Host 'scrot / screencapture / PowerShell. Opt-in by design'
Write-Host 'because the cap reads the screen.'
Write-Host ''
Prompt-YesNo 'RELIX_SCREEN_ENABLED' 'Enable [tool.screen]?'

Write-Host ''
Write-Host '=============================================================='
Write-Host ("Done. Wrote selections to {0}." -f $EnvFile)
Write-Host ''
Write-Host 'To activate the features whose keys you provided, set the'
Write-Host 'corresponding TOML sections in your controller config:'
Write-Host ''
Write-Host '  [session_identity.research]'
Write-Host '  enabled = true'
Write-Host '  [session_identity.web_search]'
Write-Host '  enabled  = true'
Write-Host '  provider = "auto"'
Write-Host ''
Write-Host '  [tool.parse_document]'
Write-Host '  enabled       = true'
Write-Host '  prefer_cloud  = true'
Write-Host ''
Write-Host '  [tool.web_read]'
Write-Host '  enabled      = true'
Write-Host '  prefer_cloud = true'
Write-Host ''
Write-Host '  [tool.screen]'
Write-Host '  enabled = true   # iff you answered yes above'
Write-Host ''
Write-Host '  [metrics.cost_alerts]'
Write-Host '  enabled               = true'
Write-Host '  baseline_window_mins  = 60'
Write-Host '  spike_multiplier      = 2.0'
Write-Host '  drift_threshold       = 0.3'
Write-Host '=============================================================='
