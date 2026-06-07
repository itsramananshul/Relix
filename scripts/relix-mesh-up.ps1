# scripts/relix-mesh-up.ps1
#
# Windows-safe PowerShell driver. Brings up the local Relix mesh and BLOCKS
# until the operator presses Ctrl-C.
#
# Nodes started (each is a normal peer; nothing is a "central gateway"):
#
#   memory controller  - SQLite + FTS5 session store
#   ai controller      - provider-agnostic ai.chat
#   tool controller    - tool.web_fetch (M9), SSRF-guarded
#   relix-web-bridge   - local HTTP -> SOL flow (/chat, /chat_with_tool, /v1/*)
#
# Safety contract:
#   * Launches pre-built binaries directly (target\debug\*.exe), so the PIDs
#     returned by Start-Process ARE the controller / bridge themselves - not
#     a `cargo run` wrapper. That lets us stop exactly what we started.
#   * On Ctrl-C, ONLY Stop-Process the PIDs collected during this run.
#     No taskkill /IM, no Get-Process | Where-Object name match, nothing that
#     could touch unrelated relix-*.exe instances, Claude Code, or terminals.
#
# Usage:
#   .\scripts\relix-mesh-up.ps1
#   .\scripts\relix-mesh-up.ps1 -Provider openrouter
#   .\scripts\relix-mesh-up.ps1 -Provider openai     -BaseUrl https://api.openai.com/v1
#   .\scripts\relix-mesh-up.ps1 -Provider anthropic
#   .\scripts\relix-mesh-up.ps1 -Provider local      -BaseUrl http://localhost:11434/v1
#   .\scripts\relix-mesh-up.ps1 -Run myrun -BridgePort 19800
#   .\scripts\relix-mesh-up.ps1 -ToolAllowHttp        # accept http:// (default https-only)
#   .\scripts\relix-mesh-up.ps1 -NoTool                # skip tool node + tool flow

[CmdletBinding()]
param(
    [ValidateSet('mock','openai','openrouter','xai','anthropic','gemini','local')]
    [string]$Provider     = 'mock',
    [string]$BaseUrl      = '',
    [string]$Run          = 'local',
    [int]$BridgePort        = 19791,
    [int]$MemPort           = 19711,
    [int]$AiPort            = 19712,
    [int]$ToolPort          = 19713,
    [int]$CoordinatorPort   = 19714,
    [int]$TelegramPort      = 19715,
    [int]$DiscordPort       = 19716,
    [int]$SlackPort         = 19717,
    [int]$PluginHostPort    = 19718,
    [switch]$ToolAllowHttp,
    [switch]$NoTool,
    [switch]$NoCoordinator,
    # Telegram channel is opt-in. Enable by setting
    #   $env:RELIX_TELEGRAM = "1"
    #   $env:RELIX_TELEGRAM_BOT_TOKEN = "<botfather-token>"
    # before invoking this script. Optional:
    #   $env:RELIX_TELEGRAM_OPERATOR_CHAT_ID = "<chat_id>"
    #   $env:RELIX_TELEGRAM_ALLOWED_USERS    = "42,1234"  # comma-separated
    # If RELIX_TELEGRAM=1 but RELIX_TELEGRAM_BOT_TOKEN is unset, the
    # telegram controller will boot but its long-poll loop will idle
    # (the bot stays offline; the dashboard reports `online=false`).
    [switch]$NoTelegram,
    # Discord channel is opt-in. Enable by setting
    #   $env:RELIX_DISCORD = "1"
    #   $env:RELIX_DISCORD_BOT_TOKEN = "<bot-token>"
    #   $env:RELIX_DISCORD_CHANNEL_ID = "<channel-snowflake>"
    # before invoking this script. Optional:
    #   $env:RELIX_DISCORD_OPERATOR_USER_ID = "<user-id>"
    #   $env:RELIX_DISCORD_ALLOWED_USERS    = "42,1234"  # comma-separated user_ids
    # Without a token + channel id the polling loop idles and the
    # dashboard reports `online=false`.
    [switch]$NoDiscord,
    # Slack channel is opt-in. Enable by setting
    #   $env:RELIX_SLACK = "1"
    #   $env:RELIX_SLACK_BOT_TOKEN = "xoxb-..."
    #   $env:RELIX_SLACK_CHANNEL_ID = "C01234567"
    # before invoking this script. Optional:
    #   $env:RELIX_SLACK_OPERATOR_USER_ID = "U01234"
    #   $env:RELIX_SLACK_ALLOWED_USERS    = "U01,U02"
    # Without a token + channel id the polling loop idles and the
    # dashboard reports `online=false`.
    [switch]$NoSlack,
    # Plugin host is opt-in. Enable by setting
    #   $env:RELIX_PLUGINS = "1"
    # Optional:
    #   $env:RELIX_PLUGIN_DIR = "./examples/plugins"   # default: ./plugins
    # The plugin_host scans the directory for plugin.toml files
    # and spawns each plugin subprocess.
    [switch]$NoPlugins
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$Root = (Get-Location).Path

# Locate the three binaries `relix boot` needs to spawn. Probed in
# the order an operator's setup is most likely to hit:
#
#   1. Same install prefix as the script — `$PSScriptRoot\..\bin\`.
#      This is the layout `install.ps1` produces: the script lives
#      at `~/.local/scripts/relix-mesh-up.ps1` next to
#      `~/.local/bin/relix.exe`, `~/.local/bin/relix-controller.exe`,
#      `~/.local/bin/relix-web-bridge.exe`.
#   2. `target\debug\`   relative to the repo root — repo checkout
#      with `cargo build --workspace`.
#   3. `target\release\` relative to the repo root — repo checkout
#      with `cargo build --release --workspace`.
#
# The CLI binary ships as `relix.exe` from the release archive (the
# `relix-cli` crate is renamed in `release.yml` so the installed
# command is just `relix`) but stays as `relix-cli.exe` under
# `target\...\`, so the CLI candidate list covers both.
function Resolve-Bin {
    param(
        [Parameter(Mandatory)] [string]$Name,
        [Parameter(Mandatory)] [string[]]$Candidates
    )
    foreach ($p in $Candidates) {
        if (Test-Path -LiteralPath $p -PathType Leaf) {
            return (Resolve-Path -LiteralPath $p).Path
        }
    }
    $lines = @("missing binary: $Name", "Searched:")
    foreach ($p in $Candidates) { $lines += "  - $p" }
    $lines += ""
    $lines += "Install the release binaries from https://github.com/itsramananshul/Relix/releases"
    $lines += "or run ``cargo build --workspace`` in a repo checkout."
    throw ($lines -join "`n")
}

$InstallBin = Join-Path $PSScriptRoot '..\bin'

$Cli = Resolve-Bin -Name 'relix-cli' -Candidates @(
    (Join-Path $InstallBin 'relix.exe'),
    (Join-Path $InstallBin 'relix-cli.exe'),
    (Join-Path $Root 'target\debug\relix-cli.exe'),
    (Join-Path $Root 'target\release\relix-cli.exe')
)
$Controller = Resolve-Bin -Name 'relix-controller' -Candidates @(
    (Join-Path $InstallBin 'relix-controller.exe'),
    (Join-Path $Root 'target\debug\relix-controller.exe'),
    (Join-Path $Root 'target\release\relix-controller.exe')
)
$Bridge = Resolve-Bin -Name 'relix-web-bridge' -Candidates @(
    (Join-Path $InstallBin 'relix-web-bridge.exe'),
    (Join-Path $Root 'target\debug\relix-web-bridge.exe'),
    (Join-Path $Root 'target\release\relix-web-bridge.exe')
)

# Locate the `flows/` directory the bridge + telegram controller read
# their SOL/sflow templates from. Probed in the same order as the
# binaries:
#
#   1. `$PSScriptRoot\..\flows\` — the install layout
#      (`~/.local/scripts/` next to `~/.local/flows/`).
#   2. `$Root\flows\`            — repo checkout (Set-Location to
#      $PSScriptRoot\.. above puts $Root at the repo root in dev).
function Resolve-Dir {
    param(
        [Parameter(Mandatory)] [string]$Name,
        [Parameter(Mandatory)] [string[]]$Candidates
    )
    foreach ($p in $Candidates) {
        if (Test-Path -LiteralPath $p -PathType Container) {
            return (Resolve-Path -LiteralPath $p).Path
        }
    }
    $lines = @("missing directory: $Name", "Searched:")
    foreach ($p in $Candidates) { $lines += "  - $p" }
    $lines += ""
    $lines += "Install with install.ps1 (which bundles the flow templates) or run from a repo checkout."
    throw ($lines -join "`n")
}

$FlowsDir = Resolve-Dir -Name 'flows' -Candidates @(
    (Join-Path $PSScriptRoot '..\flows'),
    (Join-Path $Root 'flows')
)
# TOML basic strings interpret `\U` as a Unicode escape, which trips
# any Windows path with a user-profile component (`C:\Users\...`).
# Forward slashes parse cleanly on every platform and `PathBuf` is
# happy with them on Windows too.
$FlowsToml = $FlowsDir -replace '\\','/'

$DataBase   = "dev-data/$Run"
$OrgKey     = "dev-keys/$Run-org-root.key"
$OrgPub     = "dev-keys/$Run-org-root.pub"
$BridgeAic  = "dev-keys/$Run-bridge.aic"
$MemKey         = "dev-keys/$Run-memory.key"
$AiKey          = "dev-keys/$Run-ai.key"
$ToolKey        = "dev-keys/$Run-tool.key"
$CoordinatorKey = "dev-keys/$Run-coordinator.key"
$TelegramKey    = "dev-keys/$Run-telegram.key"
$DiscordKey     = "dev-keys/$Run-discord.key"
$SlackKey       = "dev-keys/$Run-slack.key"
$PluginHostKey  = "dev-keys/$Run-plugin-host.key"
$BridgeKey      = "dev-keys/$Run-bridge.key"
# Telegram is opt-in. Default off so existing operator
# workflows are unaffected; explicitly set $env:RELIX_TELEGRAM=1
# to enable.
$TelegramEnabled = ($env:RELIX_TELEGRAM -eq '1') -and (-not $NoTelegram.IsPresent)
$DiscordEnabled  = ($env:RELIX_DISCORD -eq '1') -and (-not $NoDiscord.IsPresent)
$SlackEnabled    = ($env:RELIX_SLACK -eq '1') -and (-not $NoSlack.IsPresent)
$PluginsEnabled  = ($env:RELIX_PLUGINS -eq '1') -and (-not $NoPlugins.IsPresent)
$PluginDir       = if ($env:RELIX_PLUGIN_DIR) { $env:RELIX_PLUGIN_DIR } else { './plugins' }
$Policy     = "configs/policies/$Run.toml"
$BridgeHttp = "127.0.0.1:$BridgePort"

New-Item -ItemType Directory -Force -Path 'dev-keys', $DataBase, 'configs/policies' | Out-Null
# Workflows directory the coordinator reads for `workflow.list` (defaults
# to <coordinator-db-dir>/workflows). Create it empty so the Workflows
# panel lists zero workflows (200) instead of erroring it does not exist.
New-Item -ItemType Directory -Force -Path "$DataBase/workflows" | Out-Null

# Pidfile recording every process THIS run started. An out-of-band
# shutdown (scripts/relix-mesh-down.ps1 or `relix stop`) reads it and
# stops exactly these PIDs, never a name-based sweep, so an unrelated
# mesh on the same box survives. Written once the mesh is up; removed by
# the finally block on exit. Lives under DataBase so a per-run label
# isolates it.
$PidFile = "$DataBase/mesh.pids"

# 1) Identities. The org root is the trust anchor: mint once, never re-mint
#    (re-minting would change org_id and invalidate every leaf bundle).
if (-not (Test-Path $OrgKey) -or -not (Test-Path $OrgPub)) {
    Write-Host "minting org root ..."
    & $Cli identity init-org --root-key $OrgKey --org $Run
    if ($LASTEXITCODE -ne 0) { throw "identity init-org failed (exit $LASTEXITCODE)" }
}

# Days before not_after at which boot (and the renewal loop) re-mints a
# bundle. Matches relix-core DEFAULT_RENEWAL_WINDOW_SECS (30 days).
$RenewalWindowDays = if ($env:RELIX_IDENTITY_RENEWAL_WINDOW_DAYS) { [int]$env:RELIX_IDENTITY_RENEWAL_WINDOW_DAYS } else { 30 }

# Leaf identities are SELF-HEALING: `identity ensure` (re)mints a bundle
# when it is missing, expired, signed by a stale/foreign org root, or
# within the renewal window — otherwise a cheap no-op. This is why a fresh
# install always boots (no pre-minted, already-expired bundle can wedge it)
# and why a long-running mesh never lapses. Locally minted bundles get the
# 365-day relix-core default lifetime.
function Ensure-Identity($Name, $Out, $Groups = 'chat-users') {
    & $Cli identity ensure --root-key $OrgKey --name $Name --groups $Groups `
        --renewal-window-days $RenewalWindowDays --out $Out
    if ($LASTEXITCODE -ne 0) { throw "identity ensure for $Name failed (exit $LASTEXITCODE)" }
}
Ensure-Identity 'web-bridge' $BridgeAic

# Memory node's identity bundle. Needed so the memory node can dial the AI
# peer to call ai.embed for the vector-memory pipeline (and the agent_curate
# AI peer when [memory.curator] is enabled). Without this bundle the
# embedding dispatcher stays empty and memory.embed / memory.search return
# "embedding dispatcher not configured".
$MemoryAic = "dev-keys/$Run-memory.bundle"
Ensure-Identity 'memory' $MemoryAic

# Bundles the periodic renewal loop re-checks while the mesh runs.
$script:RenewableBundles = @(
    @{ Name = 'web-bridge'; Out = $BridgeAic },
    @{ Name = 'memory';     Out = $MemoryAic }
)

# Capture the web-bridge identity's verified subject id and hand it to
# the coordinator so it can provision the operator-console agent profile
# at startup. Without a profile the fail-closed agent gate denies the
# dashboard's Tasks/Workflows calls (agent_no_profile).
$inspectOut = & $Cli identity inspect --bundle $BridgeAic --root-key $OrgKey 2>$null
$BridgeSubject = ($inspectOut | Where-Object { $_ -match '^subject-id:\s+(\S+)' } | ForEach-Object { $Matches[1] } | Select-Object -First 1)
if ($BridgeSubject) {
    $env:RELIX_OPERATOR_CONSOLE_SUBJECT = $BridgeSubject
    Write-Host "operator-console subject: $BridgeSubject"
} else {
    Write-Host "warning: could not resolve web-bridge subject id; Tasks/Workflows may be agent-gated"
}

$MemConfig         = "$DataBase/memory.toml"
$AiConfig          = "$DataBase/ai.toml"
$ToolConfig        = "$DataBase/tool.toml"
$CoordinatorConfig = "$DataBase/coordinator.toml"
$TelegramConfig    = "$DataBase/telegram.toml"
$DiscordConfig     = "$DataBase/discord.toml"
$SlackConfig       = "$DataBase/slack.toml"
$PluginHostConfig  = "$DataBase/plugin-host.toml"
$BridgeConfig      = "$DataBase/bridge.toml"
$Peers             = "$DataBase/peers.toml"

# 2) Memory controller config.
@"
[controller]
name = "$Run-memory"
node_type = "memory"
listen_port = $MemPort

[identity]
key_path = "$MemKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[memory]
db_path = "$DataBase/memory.db"

# Vector-embedding wiring. Points at the AI peer's ai.embed
# capability. Defaults to the mock provider's 8-dim vectors so
# the full embed/search pipeline works without a real OpenAI
# key; switch to text-embedding-3-small (1536 dims) when the AI
# node runs against OpenAI-compatible.
[memory.embedding_peer]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
alias = "ai"
deadline_secs = 30
model = "mock-embed"
dimensions = 8

[peers]
"@ | Set-Content -Encoding utf8 $MemConfig

# 3) AI controller config - base + provider-specific tail.
$aiBase = @"
[controller]
name = "$Run-ai"
node_type = "ai"
listen_port = $AiPort

[identity]
key_path = "$AiKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[ai]
provider = "$Provider"
model    = ""

# Outbound memory wiring. With this block set, the AI node
# dials the memory peer at startup and ai.chat fetches recent
# conversation turns automatically — flows no longer have to
# call memory.recent_for_session manually. See docs/memory.md.
#
# Optional RAG retrieval (off by default) — when enabled the
# AI node embeds the user prompt locally and queries the
# vector memory for semantically related chunks across all
# past sessions, injecting them as a "Relevant context"
# block in the system prompt. To enable: set rag_enabled to
# true and tune rag_top_k / rag_min_score below. See
# docs/memory.md "RAG (Retrieval-Augmented Generation)".
[ai.memory_peer]
addr               = "/ip4/127.0.0.1/tcp/$MemPort"
alias              = "memory"
deadline_secs      = 5
max_history_turns  = 10
rag_enabled        = false           # set true to enable RAG
rag_top_k          = 5
rag_min_score      = 0.70

[peers]
"@

$providerTail = switch ($Provider) {
    'openai' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://api.openai.com/v1' }
        # RELIX_AI_MODEL (set from config by `relix boot`, or exported
        # by hand) picks the model; otherwise the provider default stands.
        $model = if ($env:RELIX_AI_MODEL) { $env:RELIX_AI_MODEL } else { 'gpt-4o-mini' }
@"

[ai.providers.openai]
base_url      = "$b"
api_key_env   = "OPENAI_API_KEY"
default_model = "$model"
"@
    }
    'openrouter' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://openrouter.ai/api/v1' }
        # Default to a `$0 free model so chat works out of the box without
        # burning credits; RELIX_AI_MODEL overrides it. See RELA-45.
        $model = if ($env:RELIX_AI_MODEL) { $env:RELIX_AI_MODEL } else { 'openai/gpt-oss-120b:free' }
@"

[ai.providers.openrouter]
base_url      = "$b"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "$model"
"@
    }
    'xai' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'https://api.x.ai/v1' }
        $modelLine = if ($env:RELIX_AI_MODEL) { "default_model = `"$($env:RELIX_AI_MODEL)`"" } else { '' }
@"

[ai.providers.xai]
base_url      = "$b"
api_key_env   = "XAI_API_KEY"
$modelLine
"@
    }
    'local' {
        $b = if ($BaseUrl) { $BaseUrl } else { 'http://localhost:11434/v1' }
        $modelLine = if ($env:RELIX_AI_MODEL) { "default_model = `"$($env:RELIX_AI_MODEL)`"" } else { '' }
@"

[ai.providers.local]
base_url      = "$b"
$modelLine
"@
    }
    'anthropic' {
        $model = if ($env:RELIX_AI_MODEL) { $env:RELIX_AI_MODEL } else { 'claude-3-5-sonnet-latest' }
@"

[ai.providers.anthropic]
api_key_env   = "ANTHROPIC_API_KEY"
default_model = "$model"
"@
    }
    'gemini' {
        $modelLine = if ($env:RELIX_AI_MODEL) { "default_model = `"$($env:RELIX_AI_MODEL)`"" } else { '' }
@"

[ai.providers.gemini]
api_key_env   = "GEMINI_API_KEY"
$modelLine
"@
    }
    default { '' }
}
($aiBase + $providerTail) | Set-Content -Encoding utf8 $AiConfig

# 4) Tool controller config (M9). The HTTP client lives inside the tool node;
#    the bridge never talks to external URLs directly.
if (-not $NoTool) {
    $allowHttp = if ($ToolAllowHttp) { 'true' } else { 'false' }
@"
[controller]
name = "$Run-tool"
node_type = "tool"
listen_port = $ToolPort

[identity]
key_path = "$ToolKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[tool]
max_bytes     = 262144
timeout_secs  = 15
max_redirects = 3
allow_http    = $allowHttp
user_agent    = "Relix-tool/0.1.0"
extract_max_input_bytes = 1048576

[tool.fs]
root = "$DataBase/fs-jail"
max_read_bytes = 10485760
max_write_bytes = 10485760
max_search_results = 200

[tool.pdf]
max_input_bytes = 20971520
max_pages = 200
max_output_chars = 200000

[peers]
"@ | Set-Content -Encoding utf8 $ToolConfig
    # Ensure the jail root exists before the controller starts.
    New-Item -ItemType Directory -Force -Path "$DataBase/fs-jail" | Out-Null
}

# 4.6) Telegram controller config. Opt-in via $env:RELIX_TELEGRAM=1.
#      The telegram node dials the memory/ai/coordinator peers
#      directly so it needs its own [peers] entries — its
#      outbound client only needs to know the addresses below.
if ($TelegramEnabled) {
    $tgAllowed = $env:RELIX_TELEGRAM_ALLOWED_USERS
    if (-not $tgAllowed) { $tgAllowed = '' }
    $allowedToml = if ($tgAllowed -eq '') {
        '[]'
    } else {
        '[' + $tgAllowed + ']'
    }
    $opChat = if ($env:RELIX_TELEGRAM_OPERATOR_CHAT_ID) {
        $env:RELIX_TELEGRAM_OPERATOR_CHAT_ID
    } else {
        '0'
    }
@"
[controller]
name = "$Run-telegram"
node_type = "telegram"
listen_port = $TelegramPort

[identity]
key_path = "$TelegramKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[telegram]
token_env = "RELIX_TELEGRAM_BOT_TOKEN"
allowed_users = $allowedToml
operator_chat_id = $opChat
messages_ring_capacity = 200
flow_template = "$FlowsToml/chat_template.sol"
session_db_path = "$DataBase/telegram_sessions.db"
poll_interval_secs = 1
approval_poll_interval_secs = 15

[telegram.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MemPort"

[telegram.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
deadline_secs = 60

[telegram.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$CoordinatorPort"

[peers]
"@ | Set-Content -Encoding utf8 $TelegramConfig
}

# 4.7) Discord controller config. Opt-in via $env:RELIX_DISCORD=1.
#      The discord node dials memory/ai/coordinator directly; without
#      a bot token + channel id the controller boots but idles.
if ($DiscordEnabled) {
    $dcAllowedRaw = $env:RELIX_DISCORD_ALLOWED_USERS
    if (-not $dcAllowedRaw) { $dcAllowedRaw = '' }
    # Discord snowflakes are STRINGS. Wrap each comma-separated id
    # in quotes so the TOML emits e.g. ["42", "1234"].
    $dcAllowed = if ($dcAllowedRaw -eq '') {
        '[]'
    } else {
        '[' + (($dcAllowedRaw -split ',' | ForEach-Object { '"' + $_.Trim() + '"' }) -join ', ') + ']'
    }
    $opUser = if ($env:RELIX_DISCORD_OPERATOR_USER_ID) {
        $env:RELIX_DISCORD_OPERATOR_USER_ID
    } else {
        ''
    }
    $dcChannel = if ($env:RELIX_DISCORD_CHANNEL_ID) {
        $env:RELIX_DISCORD_CHANNEL_ID
    } else {
        # Placeholder snowflake — passes the 10-digit numeric check
        # so the controller boots; the polling loop will get 404
        # from Discord and stay offline. Operator must supply a
        # real channel_id via the env var.
        '0000000000'
    }
@"
[controller]
name = "$Run-discord"
node_type = "discord"
listen_port = $DiscordPort

[identity]
key_path = "$DiscordKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[discord]
token_env = "RELIX_DISCORD_BOT_TOKEN"
channel_id = "$dcChannel"
allowed_users = $dcAllowed
operator_user_id = "$opUser"
messages_ring_capacity = 200
poll_interval_secs = 2

[discord.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MemPort"

[discord.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
deadline_secs = 60

[discord.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$CoordinatorPort"

[peers]
"@ | Set-Content -Encoding utf8 $DiscordConfig
}

# 4.8) Slack controller config. Opt-in via $env:RELIX_SLACK=1.
#      Same shape as Discord but with Slack-specific id forms
#      (allowed_users / operator_user_id are string ids like
#      "U01234567"; channel_id starts with C/G/D).
if ($SlackEnabled) {
    $slAllowedRaw = $env:RELIX_SLACK_ALLOWED_USERS
    if (-not $slAllowedRaw) { $slAllowedRaw = '' }
    $slAllowed = if ($slAllowedRaw -eq '') {
        '[]'
    } else {
        '[' + (($slAllowedRaw -split ',' | ForEach-Object { '"' + $_.Trim() + '"' }) -join ', ') + ']'
    }
    $slOperator = if ($env:RELIX_SLACK_OPERATOR_USER_ID) {
        $env:RELIX_SLACK_OPERATOR_USER_ID
    } else {
        ''
    }
    $slChannel = if ($env:RELIX_SLACK_CHANNEL_ID) {
        $env:RELIX_SLACK_CHANNEL_ID
    } else {
        # Placeholder Slack channel id — passes the prefix +
        # length check so the controller boots; the polling loop
        # will get an error from Slack and stay offline. Operator
        # must supply a real id via the env var.
        'C000000000'
    }
@"
[controller]
name = "$Run-slack"
node_type = "slack"
listen_port = $SlackPort

[identity]
key_path = "$SlackKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[slack]
token_env = "RELIX_SLACK_BOT_TOKEN"
channel_id = "$slChannel"
allowed_users = $slAllowed
operator_user_id = "$slOperator"
messages_ring_capacity = 200
poll_interval_secs = 2

[slack.memory_peer]
addr = "/ip4/127.0.0.1/tcp/$MemPort"

[slack.ai_peer]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
deadline_secs = 60

[slack.coord_peer]
addr = "/ip4/127.0.0.1/tcp/$CoordinatorPort"

[peers]
"@ | Set-Content -Encoding utf8 $SlackConfig
}

# 4.9) Plugin host controller config. Opt-in via $env:RELIX_PLUGINS=1.
#      Scans $PluginDir for plugin.toml files and spawns each plugin
#      subprocess. Registry persists at dev-data/plugin-registry.db.
if ($PluginsEnabled) {
@"
[controller]
name = "$Run-plugin-host"
node_type = "plugin_host"
listen_port = $PluginHostPort

[identity]
key_path = "$PluginHostKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[plugin_host]
plugin_dir       = "$PluginDir"
max_plugins      = 20
registry_db_path = "$DataBase/plugin-registry.db"

[peers]
"@ | Set-Content -Encoding utf8 $PluginHostConfig
}

# 4.5) Coordinator controller config. Owns the durable Task ledger
#      (SQLite). Optional -- pass -NoCoordinator to skip.
#
# Opt-in subsystem sections (enabled via `relix setup` -> forwarded by
# `relix boot`, or set the RELIX_* env vars directly). Emitting these
# registers the credential-vault / approval-delivery caps so the
# dashboard's Credentials and Approval panels return real data.
$CredBlock = ""
if ($env:RELIX_CREDENTIAL_VAULT -eq "1" -and $env:RELIX_CREDENTIAL_KEY) {
    $CredBlock = "`n[credentials]`nenabled = true`n"
}
$ApprovalBlock = ""
if ($env:RELIX_APPROVALS -eq "1") {
    $ApprovalChannel = if ($env:RELIX_APPROVAL_CHANNEL) { $env:RELIX_APPROVAL_CHANNEL } else { "dashboard" }
    $ApprovalBlock = "`n[approval]`n`n[approval.delivery]`ndefault_channel = `"$ApprovalChannel`"`n"
}
if (-not $NoCoordinator) {
@"
[controller]
name = "$Run-coordinator"
node_type = "coordinator"
listen_port = $CoordinatorPort

[identity]
key_path = "$CoordinatorKey"

[trust]
org_root_key_path = "$OrgPub"

[policy]
file = "$Policy"

[coordinator]
db_path = "$DataBase/tasks.db"
max_list = 200
$CredBlock$ApprovalBlock
[peers]
"@ | Set-Content -Encoding utf8 $CoordinatorConfig
}

# 5) Shared policy. Tool capability requires chat-users (same as ai/memory),
#    so the bridge's existing identity bundle is sufficient.
#    node.manifest is admitted for chat-users so the bridge can discover
#    each peer's capability set at startup (M10).
#    task.* admitted so the bridge (and operators via `relix-cli task`)
#    can manage durable Task records on the coordinator (A.1+A.2).
@"
[admit]
groups = ["chat-users"]

[[rules]]
name = "node_health"
method = "node.health"
allow_groups = ["chat-users"]

[[rules]]
name = "node_manifest"
method = "node.manifest"
allow_groups = ["chat-users"]

# Operator-console read surfaces (Dispatch-stats + Multi-tenant panels).
[[rules]]
name = "node_dispatch_stats"
method = "node.dispatch.stats"
allow_groups = ["chat-users"]

[[rules]]
name = "node_policy_tenant_list"
method = "node.policy.tenant_list"
allow_groups = ["chat-users"]

[[rules]]
name = "node_policy_tenant_get"
method = "node.policy.tenant_get"
allow_groups = ["chat-users"]

[[rules]]
name = "node_audit_tenant_list"
method = "node.audit.tenant_list"
allow_groups = ["chat-users"]

[[rules]]
name = "node_audit_tenant_recent"
method = "node.audit.tenant_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_recent"
method = "memory.recent_for_session"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_write"
method = "memory.write_turn"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_read"
method = "memory.agent_read"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_write"
method = "memory.agent_write"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_agent_curate"
method = "memory.agent_curate"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_curator_status"
method = "memory.curator_status"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_search_turns"
method = "memory.search_turns"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_embed"
method = "memory.embed"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_semantic_search"
method = "memory.search"
allow_groups = ["chat-users"]

[[rules]]
name = "mem_embed_all"
method = "memory.embed_all"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_chat"
method = "ai.chat"
allow_groups = ["chat-users"]

[[rules]]
name = "ai_embed"
method = "ai.embed"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_web_fetch"
method = "tool.web_fetch"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_web_extract"
method = "tool.web_extract"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_read_file"
method = "tool.read_file"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_write_file"
method = "tool.write_file"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_search_files"
method = "tool.search_files"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_patch"
method = "tool.patch"
allow_groups = ["chat-users"]

[[rules]]
name = "tool_pdf"
method = "tool.pdf"
allow_groups = ["chat-users"]

[[rules]]
name = "task_create"
method = "task.create"
allow_groups = ["chat-users"]

[[rules]]
name = "task_update"
method = "task.update"
allow_groups = ["chat-users"]

[[rules]]
name = "task_event"
method = "task.event"
allow_groups = ["chat-users"]

[[rules]]
name = "task_get"
method = "task.get"
allow_groups = ["chat-users"]

[[rules]]
name = "task_list"
method = "task.list"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_create"
method = "cron.create"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_list"
method = "cron.list"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_get"
method = "cron.get"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_update"
method = "cron.update"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_delete"
method = "cron.delete"
allow_groups = ["chat-users"]

[[rules]]
name = "cron_trigger"
method = "cron.trigger"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_spawn"
method = "delegate.spawn"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_result"
method = "delegate.result"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_cancel"
method = "delegate.cancel"
allow_groups = ["chat-users"]

[[rules]]
name = "delegate_list"
method = "delegate.list"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_create"
method = "agent.create"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_get"
method = "agent.get"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_list"
method = "agent.list"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_update"
method = "agent.update"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_delete"
method = "agent.delete"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_approve_hire"
method = "agent.approve_hire"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_reject_hire"
method = "agent.reject_hire"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_effective_capabilities"
method = "agent.effective_capabilities"
allow_groups = ["chat-users"]

[[rules]]
name = "agent_assign_check"
method = "agent.assign_check"
allow_groups = ["chat-users"]

[[rules]]
name = "coord_approval_pending"
method = "coord.approval.pending"
allow_groups = ["chat-users"]

[[rules]]
name = "coord_approval_decide"
method = "coord.approval.decide"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_create"
method = "agent.standing_approval.create"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_list"
method = "agent.standing_approval.list"
allow_groups = ["chat-users"]

[[rules]]
name = "standing_approval_revoke"
method = "agent.standing_approval.revoke"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_send"
method = "msg.send"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_inbox"
method = "msg.inbox"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_read"
method = "msg.read"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_thread"
method = "msg.thread"
allow_groups = ["chat-users"]

[[rules]]
name = "msg_delete"
method = "msg.delete"
allow_groups = ["chat-users"]

[[rules]]
name = "telegram_status"
method = "telegram.status"
allow_groups = ["chat-users"]

[[rules]]
name = "telegram_messages_recent"
method = "telegram.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "discord_status"
method = "discord.status"
allow_groups = ["chat-users"]

[[rules]]
name = "discord_messages_recent"
method = "discord.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "slack_status"
method = "slack.status"
allow_groups = ["chat-users"]

[[rules]]
name = "slack_messages_recent"
method = "slack.messages_recent"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_list"
method = "plugin.list"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_status"
method = "plugin.status"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_reload"
method = "plugin.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_disable"
method = "plugin.disable"
allow_groups = ["chat-users"]

# `.sflow` callers see the peer-prefixed alias on the wire
# because the parser preserves the dotted target the user
# typed (`step y: plugin_host.plugin.list ""`). The bridge
# registers each management cap under both names; the
# policy needs to allow both spellings too.
[[rules]]
name = "plugin_host_plugin_list"
method = "plugin_host.plugin.list"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_status"
method = "plugin_host.plugin.status"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_reload"
method = "plugin_host.plugin.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_plugin_disable"
method = "plugin_host.plugin.disable"
allow_groups = ["chat-users"]

[[rules]]
name = "hello_greet"
method = "hello.greet"
allow_groups = ["chat-users"]

# Same prefix-alias allow for the example hello plugin so
# `.sflow` can call it as `plugin_host.hello.greet`.
[[rules]]
name = "plugin_host_hello_greet"
method = "plugin_host.hello.greet"
allow_groups = ["chat-users"]

[[rules]]
name = "web_lookup_fetch"
method = "web_lookup.fetch"
allow_groups = ["chat-users"]

[[rules]]
name = "plugin_host_web_lookup_fetch"
method = "plugin_host.web_lookup.fetch"
allow_groups = ["chat-users"]

# Operator-console read surfaces (dashboard panels). Allowing an
# unregistered method is harmless — the responder still returns
# unknown_method, which the bridge renders as an empty panel. These
# ensure the operator console is not default-denied when the subsystem
# IS enabled.
[[rules]]
name = "workflow_list"
method = "workflow.list"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_run"
method = "workflow.run"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_status"
method = "workflow.status"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_validate"
method = "workflow.validate"
allow_groups = ["chat-users"]

[[rules]]
name = "workflow_reload"
method = "workflow.reload"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_report"
method = "metrics.cost_report"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_alerts_active"
method = "metrics.alerts_active"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_agents"
method = "metrics.agents"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_agent_summary"
method = "metrics.agent_summary"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_method_breakdown"
method = "metrics.method_breakdown"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_timeseries"
method = "metrics.timeseries"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_baselines"
method = "metrics.cost_baselines"
allow_groups = ["chat-users"]

[[rules]]
name = "metrics_cost_spike_history"
method = "metrics.cost_spike_history"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_health_summary"
method = "observability.health_summary"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_active_alerts"
method = "observability.active_alerts"
allow_groups = ["chat-users"]

[[rules]]
name = "obs_alert_history"
method = "observability.alert_history"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_search"
method = "memory.skill_search"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_stats"
method = "memory.skill_stats"
allow_groups = ["chat-users"]

[[rules]]
name = "skill_get"
method = "memory.skill_get"
allow_groups = ["chat-users"]

[[rules]]
name = "reasoning_status"
method = "reasoning.status"
allow_groups = ["chat-users"]

[[rules]]
name = "judge_recent_verdicts"
method = "judge.recent_verdicts"
allow_groups = ["chat-users"]

[[rules]]
name = "judge_stats"
method = "judge.stats"
allow_groups = ["chat-users"]

[[rules]]
name = "budget_status"
method = "budget.status"
allow_groups = ["chat-users"]

[[rules]]
name = "planning_get_approval"
method = "planning.get_approval"
allow_groups = ["chat-users"]

[[rules]]
name = "planning_find_agents"
method = "planning.find_agents"
allow_groups = ["chat-users"]

[[rules]]
name = "credentials_list"
method = "credentials.list"
allow_groups = ["chat-users"]

[[rules]]
name = "credentials_audit"
method = "credentials.audit"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_list_pending"
method = "approval.list_pending"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_failed_deliveries"
method = "approval.failed_deliveries"
allow_groups = ["chat-users"]

[[rules]]
name = "approval_delivery_status"
method = "approval.delivery_status"
allow_groups = ["chat-users"]

# Operator dashboard (/dashboard): product-spine Brief board / Mandate /
# roster capabilities the web bridge calls on the coordinator. The caller
# is the web-bridge identity in the chat-users group. Per-agent Key gates
# (tenant/manage/assign) still apply inside each capability; this only
# lifts the mesh default-deny so the operator console reaches the spine.
[[rules]]
name = "spine_guild_counts"
method = "guild.counts"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_guild_get"
method = "guild.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_guild_spend"
method = "guild.spend"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_board_summary"
method = "brief.board_summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_board"
method = "brief.board"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unassigned"
method = "brief.unassigned"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unblocked"
method = "brief.unblocked"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_blocked_list"
method = "brief.blocked_list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_stale_list"
method = "brief.stale_list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_overdue"
method = "brief.overdue"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_detail"
method = "brief.detail"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_cost_rollup"
method = "brief.cost_rollup"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_search"
method = "brief.search"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_by_label"
method = "brief.by_label"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_desk"
method = "brief.desk"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_claim_holder"
method = "brief.claim_holder"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_wakeups"
method = "brief.wakeups"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_create"
method = "brief.create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_move"
method = "brief.move"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set"
method = "brief.set"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set_due"
method = "brief.set_due"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_pin"
method = "brief.pin"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_comment"
method = "brief.comment"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_open"
method = "brief.interaction_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_confirm_open"
method = "brief.plan_confirm_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interactions"
method = "brief.interactions"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_respond"
method = "brief.interaction_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_cancel"
method = "brief.interaction_cancel"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_interaction_create"
method = "brief.interaction_create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_suggest_open"
method = "brief.suggest_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_suggest_respond"
method = "brief.suggest_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_package_open"
method = "brief.plan_package_open"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_plan_confirm_respond"
method = "brief.plan_confirm_respond"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_snag"
method = "brief.snag"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_unsnag"
method = "brief.unsnag"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_subbrief"
method = "brief.subbrief"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_add"
method = "brief.dossier_add"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_author"
method = "brief.dossier_author"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_latest"
method = "brief.dossier_latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_lock"
method = "brief.dossier_lock"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_unlock"
method = "brief.dossier_unlock"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_dossier_locks"
method = "brief.dossier_locks"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_set_snags"
method = "brief.set_snags"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_clearance_request"
method = "brief.clearance_request"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_bridge_back_authorize"
method = "bridge_back.authorize"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_roster_summary"
method = "agent.roster_summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_recent_events"
method = "task.recent_events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_stuck"
method = "task.stuck"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_recent_edges"
method = "task.recent_edges"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_task_events"
method = "task.events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_search"
method = "mandate.search"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_tree"
method = "mandate.tree"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_briefs"
method = "mandate.briefs"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_list"
method = "mandate.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_create"
method = "mandate.create"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestrate"
method = "mandate.orchestrate"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestration_latest"
method = "mandate.orchestration.latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_orchestration_list"
method = "mandate.orchestration.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_propose"
method = "prime.propose"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_approve"
method = "prime.approve"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_start"
method = "prime.start"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_proposals"
method = "prime.proposals"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_proposal"
method = "prime.proposal"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_status"
method = "prime.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_next_step"
method = "prime.next_step"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_prime_advance"
method = "prime.advance"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_plan"
method = "mandate.team_plan"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_plan_latest"
method = "mandate.team_plan.latest"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_team_readiness"
method = "mandate.team_readiness"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_status"
method = "mandate.strategy.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_propose"
method = "mandate.strategy.propose"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_approve"
method = "mandate.strategy.approve"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_mandate_strategy_reject"
method = "mandate.strategy.reject"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_rig_list"
method = "rig.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_rig_describe"
method = "rig.describe"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_wakeup"
method = "brief.wakeup"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_run"
method = "brief.run"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_brief_runs"
method = "brief.runs"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_status"
method = "company.status"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_actions"
method = "company.actions"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_bootstrap_founder"
method = "company.bootstrap_founder"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_company_starter_crew"
method = "company.starter_crew"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_operatives"
method = "agent.operatives"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_keys"
method = "agent.keys"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_agent_allowance_committed"
method = "agent.allowance_committed"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_workspace_config"
method = "run.workspace_config"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_get"
method = "run.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_events"
method = "run.events"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_cancel"
method = "run.cancel"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifacts"
method = "run.artifacts"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifact_preview"
method = "run.artifact_preview"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_artifact_diff"
method = "run.artifact_diff"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_review"
method = "run.review"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_diff"
method = "run.diff"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_apply"
method = "run.apply"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_discard"
method = "run.discard"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_run_events_recent"
method = "run.events.recent"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_get"
method = "rig.runtime_state.get"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_list"
method = "rig.runtime_state.list"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_runtime_state_reset"
method = "rig.runtime_state.reset"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_summary"
method = "maintenance.summary"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_prune"
method = "maintenance.prune"
allow_groups = ["chat-users"]
[[rules]]
name = "spine_maintenance_audit"
method = "maintenance.audit"
allow_groups = ["chat-users"]
"@ | Set-Content -Encoding utf8 $Policy

# 6) Peer alias map consumed by the bridge. Tool entry omitted when -NoTool.
$peersToml = @"
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/$MemPort"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/$AiPort"
"@
if (-not $NoTool) {
    $peersToml += @"


[peers.tool]
addr = "/ip4/127.0.0.1/tcp/$ToolPort"
"@
}
if (-not $NoCoordinator) {
    $peersToml += @"


[peers.coordinator]
addr = "/ip4/127.0.0.1/tcp/$CoordinatorPort"
"@
}
if ($TelegramEnabled) {
    $peersToml += @"


[peers.telegram]
addr = "/ip4/127.0.0.1/tcp/$TelegramPort"
"@
}
if ($DiscordEnabled) {
    $peersToml += @"


[peers.discord]
addr = "/ip4/127.0.0.1/tcp/$DiscordPort"
"@
}
if ($SlackEnabled) {
    $peersToml += @"


[peers.slack]
addr = "/ip4/127.0.0.1/tcp/$SlackPort"
"@
}
if ($PluginsEnabled) {
    $peersToml += @"


[peers.plugin_host]
addr = "/ip4/127.0.0.1/tcp/$PluginHostPort"
"@
}
$peersToml | Set-Content -Encoding utf8 $Peers

# 7) Bridge config - OpenAI shim on; tool template wired only when the tool
#    node is up so the bridge fails 404 cleanly when there's no peer.
$toolTemplateLine = if ($NoTool) { '' } else { "tool_template_path = `"$FlowsToml/chat_with_tool.sol`"" }

# Setup token guarding GET /v1/auth/token (the dashboard's bootstrap
# exchange). Honour an operator-supplied RELIX_SETUP_TOKEN; otherwise
# mint a strong random one. Never a hardcoded default — without a real
# token the dashboard cannot bootstrap. Printed at the end so the
# operator can paste it into the dashboard's Authentication screen.
if ($env:RELIX_SETUP_TOKEN) {
    $SetupToken = $env:RELIX_SETUP_TOKEN
} else {
    $rng = New-Object System.Security.Cryptography.RNGCryptoServiceProvider
    $tokenBytes = New-Object byte[] 32
    $rng.GetBytes($tokenBytes)
    $SetupToken = -join ($tokenBytes | ForEach-Object { $_.ToString('x2') })
}
@"
[bridge]
listen_addr = "$BridgeHttp"

[auth]
setup_token = "$SetupToken"

[identity]
bundle_path     = "$BridgeAic"
client_key_path = "$BridgeKey"

[transport]
peers_path    = "$Peers"
deadline_secs = 60

[flow]
template_path = "$FlowsToml/chat_template.sol"
$toolTemplateLine

[sse]
chunk_bytes    = 24
chunk_delay_ms = 15

[openai_compat]
default_model = "relix-$Provider"

[[openai_compat.models]]
id          = "relix-$Provider"
description = "Relix mesh route - AI node currently set to $Provider"
"@ | Set-Content -Encoding utf8 $BridgeConfig

# B1: optional coordinator integration. When the coordinator peer is
# enabled, append the [coordinator] section so the bridge persists chat
# flows as Tasks. When -NoCoordinator was passed, the bridge runs
# without persistence (fail-soft).
if (-not $NoCoordinator) {
    Add-Content -Encoding utf8 $BridgeConfig @"

[coordinator]
alias = "coordinator"
"@
}

$MemLog         = "$DataBase/memory.log"
$AiLog          = "$DataBase/ai.log"
$ToolLog        = "$DataBase/tool.log"
$CoordinatorLog = "$DataBase/coordinator.log"
$TelegramLog    = "$DataBase/telegram.log"
$DiscordLog     = "$DataBase/discord.log"
$SlackLog       = "$DataBase/slack.log"
$PluginHostLog  = "$DataBase/plugin-host.log"
$BridgeLog      = "$DataBase/bridge.log"
$MemErr         = "$DataBase/memory.err.log"
$AiErr          = "$DataBase/ai.err.log"
$ToolErr        = "$DataBase/tool.err.log"
$CoordinatorErr = "$DataBase/coordinator.err.log"
$TelegramErr    = "$DataBase/telegram.err.log"
$DiscordErr     = "$DataBase/discord.err.log"
$SlackErr       = "$DataBase/slack.err.log"
$PluginHostErr  = "$DataBase/plugin-host.err.log"
$BridgeErr      = "$DataBase/bridge.err.log"

$env:RELIX_DATA_DIR = 'dev-data'

function Start-Node {
    param(
        [Parameter(Mandatory)] [string]$Exe,
        [Parameter(Mandatory)] [string]$Cfg,
        [Parameter(Mandatory)] [string]$OutLog,
        [Parameter(Mandatory)] [string]$ErrLog,
        [Parameter(Mandatory)] [string]$RustLog
    )
    # Per-node env: writing $env:RUST_LOG just before spawn takes effect for
    # the child only via inheritance at process-start time.
    $env:RUST_LOG = $RustLog
    return Start-Process `
        -FilePath $Exe `
        -ArgumentList @('--config', $Cfg) `
        -NoNewWindow `
        -PassThru `
        -RedirectStandardOutput $OutLog `
        -RedirectStandardError  $ErrLog
}

function Wait-Log {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [string]$Needle,
        [Parameter(Mandatory)] [string]$Desc
    )
    for ($i = 0; $i -lt 150; $i++) {
        if (Test-Path $Path) {
            $hit = Select-String -Path $Path -Pattern $Needle -SimpleMatch -Quiet -ErrorAction SilentlyContinue
            if ($hit) { return $true }
        }
        Start-Sleep -Milliseconds 200
    }
    Write-Warning "$Desc never logged '$Needle' (see $Path)"
    if (Test-Path $Path)              { Get-Content $Path              -Tail 40 | ForEach-Object { Write-Host "  $_" } }
    $errPath = $Path -replace '\.log$','.err.log'
    if (Test-Path $errPath)           { Get-Content $errPath           -Tail 40 | ForEach-Object { Write-Host "  $_" } }
    return $false
}

Write-Host "== Relix mesh up =="
Write-Host "  run:           $Run"
Write-Host "  provider:      $Provider"
Write-Host "  memory port:   tcp/$MemPort"
Write-Host "  ai port:       tcp/$AiPort"
if (-not $NoTool) {
    Write-Host ("  tool port:     tcp/{0}  (allow_http={1})" -f $ToolPort, $ToolAllowHttp.IsPresent)
} else {
    Write-Host "  tool port:     (disabled - -NoTool)"
}
if (-not $NoCoordinator) {
    Write-Host ("  coord port:    tcp/{0}  (db={1}/tasks.db)" -f $CoordinatorPort, $DataBase)
} else {
    Write-Host "  coord port:    (disabled - -NoCoordinator)"
}
if ($TelegramEnabled) {
    $hasToken = if ($env:RELIX_TELEGRAM_BOT_TOKEN) { 'token=set' } else { 'token=MISSING' }
    Write-Host ("  telegram port: tcp/{0}  ({1})" -f $TelegramPort, $hasToken)
} else {
    Write-Host "  telegram port: (disabled - set RELIX_TELEGRAM=1 to enable)"
}
if ($DiscordEnabled) {
    $hasDcToken = if ($env:RELIX_DISCORD_BOT_TOKEN) { 'token=set' } else { 'token=MISSING' }
    $hasDcCh    = if ($env:RELIX_DISCORD_CHANNEL_ID) { 'channel=set' } else { 'channel=MISSING' }
    Write-Host ("  discord port:  tcp/{0}  ({1}, {2})" -f $DiscordPort, $hasDcToken, $hasDcCh)
} else {
    Write-Host "  discord port:  (disabled - set RELIX_DISCORD=1 to enable)"
}
if ($SlackEnabled) {
    $hasSlToken = if ($env:RELIX_SLACK_BOT_TOKEN) { 'token=set' } else { 'token=MISSING' }
    $hasSlCh    = if ($env:RELIX_SLACK_CHANNEL_ID) { 'channel=set' } else { 'channel=MISSING' }
    Write-Host ("  slack port:    tcp/{0}  ({1}, {2})" -f $SlackPort, $hasSlToken, $hasSlCh)
} else {
    Write-Host "  slack port:    (disabled - set RELIX_SLACK=1 to enable)"
}
Write-Host "  bridge HTTP:   http://$BridgeHttp"
Write-Host "  data dir:      $DataBase"
Write-Host ""

# Track ONLY the processes this script started. Stop-Process on shutdown
# is restricted to this exact list - never a name-based sweep.
$started = New-Object System.Collections.ArrayList

try {
    Write-Host "starting memory controller ..."
    [void]$started.Add( (Start-Node -Exe $Controller -Cfg $MemConfig -OutLog $MemLog -ErrLog $MemErr -RustLog 'relix_runtime=info') )

    Write-Host "starting ai controller ..."
    [void]$started.Add( (Start-Node -Exe $Controller -Cfg $AiConfig  -OutLog $AiLog  -ErrLog $AiErr  -RustLog 'relix_runtime=info') )

    if (-not $NoTool) {
        Write-Host "starting tool controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $ToolConfig -OutLog $ToolLog -ErrLog $ToolErr -RustLog 'relix_runtime=info') )
    }

    if (-not $NoCoordinator) {
        Write-Host "starting coordinator controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $CoordinatorConfig -OutLog $CoordinatorLog -ErrLog $CoordinatorErr -RustLog 'relix_runtime=info') )
    }

    if ($TelegramEnabled) {
        # Mint the telegram outbound-identity bundle on demand. The
        # controller process generates the .key file (ed25519) on
        # first boot — same idempotency pattern as memory/ai/tool —
        # but the .bundle has to be minted off the org root, so we
        # call relix-cli once if it's missing.
        $TelegramBundlePath = "dev-keys/$Run-telegram.bundle"
        Ensure-Identity 'telegram' $TelegramBundlePath
        $script:RenewableBundles += @{ Name = 'telegram'; Out = $TelegramBundlePath }
        Write-Host "starting telegram controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $TelegramConfig -OutLog $TelegramLog -ErrLog $TelegramErr -RustLog 'relix_runtime=info,relix_telegram=info') )
    }

    if ($DiscordEnabled) {
        # Same identity-bundle pattern as telegram: the controller
        # generates its .key on first boot, but the .bundle is
        # minted off the org root and persisted across restarts.
        $DiscordBundlePath = "dev-keys/$Run-discord.bundle"
        Ensure-Identity 'discord' $DiscordBundlePath
        $script:RenewableBundles += @{ Name = 'discord'; Out = $DiscordBundlePath }
        Write-Host "starting discord controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $DiscordConfig -OutLog $DiscordLog -ErrLog $DiscordErr -RustLog 'relix_runtime=info,relix_discord=info') )
    }

    if ($SlackEnabled) {
        $SlackBundlePath = "dev-keys/$Run-slack.bundle"
        Ensure-Identity 'slack' $SlackBundlePath
        $script:RenewableBundles += @{ Name = 'slack'; Out = $SlackBundlePath }
        Write-Host "starting slack controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $SlackConfig -OutLog $SlackLog -ErrLog $SlackErr -RustLog 'relix_runtime=info,relix_slack=info') )
    }

    if ($PluginsEnabled) {
        $PluginHostBundlePath = "dev-keys/$Run-plugin-host.bundle"
        Ensure-Identity 'plugin-host' $PluginHostBundlePath
        $script:RenewableBundles += @{ Name = 'plugin-host'; Out = $PluginHostBundlePath }
        Write-Host "starting plugin_host controller ..."
        [void]$started.Add( (Start-Node -Exe $Controller -Cfg $PluginHostConfig -OutLog $PluginHostLog -ErrLog $PluginHostErr -RustLog 'relix_runtime=info,relix_runtime::plugin=debug') )
    }

    if (-not (Wait-Log -Path $MemLog -Needle 'transport listening' -Desc 'memory controller')) { throw 'memory controller never came up' }
    if (-not (Wait-Log -Path $AiLog  -Needle 'transport listening' -Desc 'ai controller'))     { throw 'ai controller never came up' }
    if (-not $NoTool) {
        if (-not (Wait-Log -Path $ToolLog -Needle 'transport listening' -Desc 'tool controller')) { throw 'tool controller never came up' }
    }
    if (-not $NoCoordinator) {
        if (-not (Wait-Log -Path $CoordinatorLog -Needle 'transport listening' -Desc 'coordinator controller')) { throw 'coordinator controller never came up' }
    }
    if ($TelegramEnabled) {
        if (-not (Wait-Log -Path $TelegramLog -Needle 'transport listening' -Desc 'telegram controller')) { throw 'telegram controller never came up' }
    }
    if ($DiscordEnabled) {
        if (-not (Wait-Log -Path $DiscordLog -Needle 'transport listening' -Desc 'discord controller')) { throw 'discord controller never came up' }
    }
    if ($SlackEnabled) {
        if (-not (Wait-Log -Path $SlackLog -Needle 'transport listening' -Desc 'slack controller')) { throw 'slack controller never came up' }
    }
    if ($PluginsEnabled) {
        if (-not (Wait-Log -Path $PluginHostLog -Needle 'transport listening' -Desc 'plugin_host controller')) { throw 'plugin_host controller never came up' }
    }
    Start-Sleep -Milliseconds 400

    Write-Host "starting web bridge ..."
    [void]$started.Add( (Start-Node -Exe $Bridge -Cfg $BridgeConfig -OutLog $BridgeLog -ErrLog $BridgeErr -RustLog 'relix_web_bridge=info,relix_runtime=info') )

    if (-not (Wait-Log -Path $BridgeLog -Needle 'web bridge starting' -Desc 'web bridge')) {
        # The bridge binds its port BEFORE logging 'web bridge starting',
        # so a missing needle usually means the bind failed — most often
        # a stale bridge from a prior boot is already holding the port.
        # Surface the bridge's own error so the cause is obvious.
        Write-Host "web bridge did not come up. Bridge error log tail:"
        if (Test-Path $BridgeErr) { Get-Content $BridgeErr -Tail 30 | ForEach-Object { Write-Host "    $_" } }
        throw 'web bridge never came up (port may be shadowed by a stale instance; run relix-mesh-down)'
    }
    Start-Sleep -Milliseconds 400

    # Record every PID we started so an out-of-band shutdown can stop
    # exactly this mesh and nothing else. The finally block removes it.
    ($started | ForEach-Object { $_.Id }) | Set-Content -Encoding ascii $PidFile

    Write-Host ""
    Write-Host "mesh is UP."
    Write-Host ""
    Write-Host "Endpoints:"
    Write-Host "  http://127.0.0.1:$BridgePort/health"
    Write-Host "  http://127.0.0.1:$BridgePort/v1/models"
    Write-Host "  http://127.0.0.1:$BridgePort/v1/chat/completions"
    if (-not $NoTool) {
        Write-Host "  http://127.0.0.1:$BridgePort/chat_with_tool   (POST: {session_id, message, url})"
    }
    Write-Host ""
    Write-Host "Dashboard:   http://127.0.0.1:$BridgePort/dashboard"
    $AdminFile = Join-Path $env:USERPROFILE '.relix\dashboard-admin.json'
    if (Test-Path -LiteralPath $AdminFile) {
        Write-Host "  Log in with your dashboard admin username + password."
        Write-Host "  Forgot it? .\scripts\relix-dashboard-admin-reset.ps1  (local recovery; restart the bridge after)."
    }
    else {
        Write-Host "  First run: open the dashboard and CREATE the admin account (username + password)."
        Write-Host "  Prefer the CLI? .\scripts\relix-dashboard-admin-reset.ps1  pre-creates it locally."
    }
    Write-Host "  Verify the product loop:  .\target\debug\relix-cli.exe dashboard doctor"
    Write-Host ""
    Write-Host "Advanced (curl/scripts only — NOT the dashboard login):"
    Write-Host "  Setup token: $SetupToken"
    Write-Host "  ^ presented as 'Authorization: Bearer <setup_token>' to GET /v1/auth/token to fetch the"
    Write-Host "    bridge bearer for raw HTTP. The browser dashboard does NOT use this — it uses the"
    Write-Host "    admin username/password above."
    Write-Host ""
    Write-Host "Controllers (channels are opt-in and NEVER block boot — a missing token just stays offline):"
    Write-Host ("  tool         {0}" -f $(if (-not $NoTool) { 'started' } else { 'disabled (-NoTool)' }))
    Write-Host ("  coordinator  {0}" -f $(if (-not $NoCoordinator) { 'started' } else { 'disabled (-NoCoordinator)' }))
    Write-Host ("  telegram     {0}" -f $(if ($TelegramEnabled) { 'started' } else { 'disabled (set RELIX_TELEGRAM=1)' }))
    Write-Host ("  discord      {0}" -f $(if ($DiscordEnabled) { 'started' } else { 'disabled (set RELIX_DISCORD=1)' }))
    Write-Host ("  slack        {0}" -f $(if ($SlackEnabled) { 'started' } else { 'disabled (set RELIX_SLACK=1)' }))
    Write-Host ("  plugin_host  {0}" -f $(if ($PluginsEnabled) { 'started' } else { 'disabled (set RELIX_PLUGINS=1)' }))
    Write-Host ""
    Write-Host "Open WebUI config:"
    Write-Host "  API Base URL: http://127.0.0.1:$BridgePort/v1"
    Write-Host "  API Key:      anything non-empty"
    Write-Host "  model:        relix-$Provider"
    if (-not $NoTool) {
        Write-Host "  Note:         messages containing an http(s) URL auto-route through the tool flow."
    }
    Write-Host ""
    Write-Host "Smoke tests:"
    Write-Host "  Invoke-RestMethod http://127.0.0.1:$BridgePort/health"
    Write-Host "  Invoke-RestMethod http://127.0.0.1:$BridgePort/v1/models"
    Write-Host "  Invoke-RestMethod -Method Post http://127.0.0.1:$BridgePort/v1/chat/completions ``"
    Write-Host "    -ContentType 'application/json' ``"
    Write-Host "    -Body (@{ model='relix-$Provider'; messages=@(@{role='user';content='hello'}) } | ConvertTo-Json)"
    if (-not $NoTool) {
        Write-Host ""
        Write-Host "  # Tool flow:"
        Write-Host "  Invoke-RestMethod -Method Post http://127.0.0.1:$BridgePort/chat_with_tool ``"
        Write-Host "    -ContentType 'application/json' ``"
        Write-Host "    -Body (@{ session_id='demo'; message='summarize this page'; url='https://example.com/' } | ConvertTo-Json)"
    }
    if (-not $NoCoordinator) {
        Write-Host ""
        Write-Host "  # Coordinator (durable Task ledger):"
        Write-Host "  .\target\debug\relix-cli.exe task list ``"
        Write-Host "    --peer /ip4/127.0.0.1/tcp/$CoordinatorPort ``"
        Write-Host "    --identity $BridgeAic ``"
        Write-Host "    --client-key $BridgeKey"
    }
    Write-Host ""
    Write-Host "Logs:"
    Write-Host "  $MemLog"
    Write-Host "  $AiLog"
    if (-not $NoTool)        { Write-Host "  $ToolLog" }
    if (-not $NoCoordinator) { Write-Host "  $CoordinatorLog" }
    if ($TelegramEnabled)    { Write-Host "  $TelegramLog" }
    if ($DiscordEnabled)     { Write-Host "  $DiscordLog" }
    if ($SlackEnabled)       { Write-Host "  $SlackLog" }
    if ($PluginsEnabled)     { Write-Host "  $PluginHostLog" }
    Write-Host "  $BridgeLog"
    Write-Host ""
    Write-Host "PIDs (this script will only stop these on Ctrl-C):"
    foreach ($p in $started) { Write-Host ("  {0,-22} pid {1}" -f $p.ProcessName, $p.Id) }
    Write-Host ""
    Write-Host "Ctrl-C to stop the mesh."
    Write-Host ""

    # Block here until either:
    #
    #   * The operator presses Ctrl-C in this terminal — PowerShell's
    #     default handler interrupts Start-Sleep with a
    #     PipelineStoppedException. The `catch` below logs it and
    #     falls through to the outer `finally`, which Stop-Processes
    #     every PID this script started.
    #
    #   * One of the spawned controllers (or the bridge) exits on its
    #     own - typically because `relix stop` signalled them by their
    #     recorded PID from another terminal. We break the loop so the
    #     outer `finally`
    #     tears down whatever's still running, then exit with code 1
    #     so `relix boot` returns to the prompt instead of hanging.
    #
    # We deliberately don't use [Console]::TreatControlCAsInput +
    # KeyAvailable here: the `relix boot` parent spawns this script
    # via `Command::spawn` which can leave the console handle in a
    # state where KeyAvailable misses keys and the loop never exits
    # on Ctrl-C. Polling every 500ms is cheap and behaves the same
    # across Windows Terminal, ISE, background jobs, and stdin-
    # redirected hosts.
    # Re-check identity bundles this often while running (seconds). 12h default.
    $renewIntervalSecs = if ($env:RELIX_IDENTITY_RENEW_INTERVAL_SECS) { [int]$env:RELIX_IDENTITY_RENEW_INTERVAL_SECS } else { 43200 }
    $lastRenew = Get-Date
    try {
        while ($true) {
            $exited = $started | Where-Object { $_.HasExited } | Select-Object -First 1
            if ($null -ne $exited) {
                Write-Host ""
                Write-Host "$($exited.ProcessName) (pid $($exited.Id)) exited with code $($exited.ExitCode) — tearing down the rest of the mesh."
                break
            }
            # Periodic identity renewal: re-mint any bundle within its
            # renewal window so a mesh running for months never lapses.
            # Cheap no-op when healthy. The refreshed bundle on disk is
            # adopted by a node on its next restart (identity is loaded at
            # boot; no hot-reload yet) — but with the 365-day lifetime and
            # boot-time self-heal this keeps a long-running mesh valid with
            # no operator action.
            if (((Get-Date) - $lastRenew).TotalSeconds -ge $renewIntervalSecs) {
                foreach ($b in $script:RenewableBundles) {
                    try {
                        & $Cli identity ensure --root-key $OrgKey --name $b.Name `
                            --groups chat-users --renewal-window-days $RenewalWindowDays `
                            --out $b.Out *> $null
                    } catch { }
                }
                $lastRenew = Get-Date
            }
            Start-Sleep -Milliseconds 500
        }
    }
    catch {
        Write-Host ""
        Write-Host "interrupted: $($_.Exception.Message)"
    }
}
finally {
    Write-Host ""
    Write-Host "stopping mesh (only PIDs started by this script) ..."
    foreach ($p in @($started)) {
        if ($p -and -not $p.HasExited) {
            try {
                Stop-Process -Id $p.Id -ErrorAction Stop
                Write-Host "  stopped $($p.ProcessName) (pid $($p.Id))"
            } catch {
                Write-Warning "could not stop pid $($p.Id): $_"
            }
        }
    }
    if ($PidFile) { Remove-Item -LiteralPath $PidFile -Force -ErrorAction SilentlyContinue }
    Write-Host "mesh down."
}
