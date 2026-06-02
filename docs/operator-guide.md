# Operator Guide

How to run Relix, where everything lives on disk, what the logs say, and
what to do when something is wrong.

If you have not booted Relix before, start with
[`getting-started.md`](getting-started.md). This guide assumes you have
the workspace built and you want to know how to operate it.

## CLI quick-reference

`relix` (the installed wrapper for `relix-cli`) exposes 40+ subcommands grouped below. Each accepts `--help` for full flag documentation.

### Mesh lifecycle

| Command | Purpose |
|---|---|
| `relix setup` / `relix reconfigure` | Seven-page interactive wizard — provider, API key, channels, confidence, subsystems. |
| `relix boot` | Start the local mesh from `~/.relix/config.toml`; opens dashboard in browser. |
| `relix stop` | Kill `relix-controller` and `relix-web-bridge` by name (idempotent). |
| `relix status` | Bridge health + topology snapshot + local DB sizes. |
| `relix install [--check] [--fix]` | Check (and optionally install) Docker, Ollama, and Qdrant. |
| `relix update [--dry-run] [--yes]` | Self-update binary from GitHub releases. |
| `relix doctor [--json]` | One-command bridge health check; exits non-zero on any FAIL. |

### Identity

| Command | Purpose |
|---|---|
| `relix identity init-org --root-key <PATH> --org <LABEL>` | Generate an org-root Ed25519 keypair. |
| `relix identity mint --root-key <PATH> --name <N> [--groups <G,...>] [--out <PATH>]` | Mint a signed identity bundle. |
| `relix identity inspect --bundle <PATH> --root-key <PATH>` | Decode + verify a bundle. |
| `relix identity issue` / `verify` / `revoke` / `tokens` | Session-token management (bridge HTTP). |
| `relix identity research --subject <NAME>` | Web + LLM identity research; persists to memory. |

### Observability + operations

| Command | Purpose |
|---|---|
| `relix observe [--once] [--alerts] [--health]` | Live crossterm TUI dashboard; press `q` to quit. |
| `relix metrics summary / alerts / cost / timeseries` | Agent performance metrics. |
| `relix pii stats / events` | PII detection counters and recent events. |
| `relix ops snapshot` | One-shot observable state dump (health + topology + dispatch stats + denials). |
| `relix ops stuck [--threshold-secs 300]` | Tasks running longer than threshold. |
| `relix ops tail` | Live tail of the task firehose via poll loop. |
| `relix ops smoke` | End-to-end mesh smoke test; exits 1 on failure. |
| `relix ops capabilities [--filter PREFIX]` | All capabilities across cached topology peers. |
| `relix ops dispatch-stats` | Per-capability invocation + latency counters. |
| `relix ops policy-simulate / policy-denials` | Policy engine dry-run and recent deny ring. |
| `relix ops session-search --query <Q>` | Full-text search across chat-turn chronicle events. |
| `relix ops agent-memory --subject-id <ID>` | Read persistent agent + user memory for a subject. |
| `relix ops open-webui-setup` | Print copy-paste Open WebUI connection settings. |
| `relix ops events [--csv]` | Recent cross-task events. |
| `relix ops cron list / create / trigger / delete / enable / disable` | Cron job management. |
| `relix ops agent list / create / get / enable / suspend / disable` | Agent employee records. |
| `relix ops agent approvals-pending / approval-decide / standing-approval-*` | Agent approval lifecycle. |
| `relix ops delegate spawn / result / cancel / list` | Delegated child task management. |
| `relix ops msg send / inbox / read / thread / delete` | Agent-to-agent messaging. |
| `relix ops memory embed / search / ingest / dialectic / flush / quarantine-* / export` | Vector memory surfaces. |
| `relix ops plugin list / status / reload / disable` | Plugin host management. |
| `relix ops discord / slack` | Channel status + recent messages. |

### Workflow and flows

| Command | Purpose |
|---|---|
| `relix workflow list / run / validate / trace / reload` | Multi-agent workflow engine. |
| `relix flow yaml [--template <NAME>]` | Print a YAML flow template (chat / tool / multi-agent / sequential). |
| `relix sol templates / new` | SOL workflow authoring helpers. |
| `relix flow-run --flow <PATH.sol> --identity <BUNDLE> --client-key <KEY> --peers <TOML>` | Execute a SOL flow against a live mesh via libp2p. |
| `relix build [SPEC]` | Full planning pipeline with optional approval gate. |
| `relix planning agents / search / validate / plan` | Multi-agent planning pipeline. |

### AI + model surface

| Command | Purpose |
|---|---|
| `relix models list / health` | Provider + model inventory with quarantine counters. |
| `relix routing explain --message <TEXT>` | Classify a message with the ComplexityClassifier. |
| `relix reasoning status` | Per-component reasoning engine summary. |
| `relix judge verdicts / stats` | Judge model verdict ring and counters. |
| `relix confidence policies / history / reset` | Per-step confidence scoring inspector. |
| `relix belief show / reset` | LLM-driven belief tracker inspector. |
| `relix training stats / list / show / export / delete` | Training data pipeline. |
| `relix knowledge share / broadcast / groups / shared / revoke` | Agent-to-agent knowledge transfer. |

### Security + credentials + approvals

| Command | Purpose |
|---|---|
| `relix credentials store / list / rotate / revoke / audit / migrate-kdf / rotate-vault-key` | Encrypted credential vault. |
| `relix approval delivery-status / get` | Approval delivery inspector. |
| `relix eval guardrails [--quick]` | Red-team guardrail eval suite. |

### Sessions + execution + provenance

| Command | Purpose |
|---|---|
| `relix sessions list / show / search` | Two-sink session debugger. |
| `relix execution rollback / transaction / evidence` | Transactional gateway + evidence. |
| `relix provenance show / diff / history / audit` | Provenance registry inspector. |

### Personas, skills, and memory

| Command | Purpose |
|---|---|
| `relix souls list / edit <AGENT>` | SOUL.md persona file management. |
| `relix skills list / run <NAME>` | SKILL.md library discovery and inspection. |
| `relix memory list / show / search / invalidate / stats` | Four-layer memory store inspector. |

### Data and export

| Command | Purpose |
|---|---|
| `relix export {--session\|--agent\|--all} [--format json\|markdown\|csv]` | Conversation history export. |
| `relix email send / status / test` | Email channel operator surface. |

### libp2p direct surfaces

| Command | Purpose |
|---|---|
| `relix ping --peer <ADDR>` | Raw libp2p health check against one peer. |
| `relix task create / get / list / update / watch / retry / recover / …` | Coordinator task ledger (libp2p). |
| `relix capability ls / show` | Peer capability inspection (libp2p). |
| `relix topology show` | Bridge topology snapshot. |
| `relix router status / peers / sessions` | Router Node control plane (libp2p). |
| `relix mcp servers / tools / audit` | MCP registry inspector. |
| `relix terminal sessions / audit / cancel` | Terminal tool control. |
| `relix fs audit` | Filesystem audit snapshot. |
| `relix web blocklist` | Web tool blocked-hosts snapshot. |
| `relix browser sessions` | Browser session list. |
| `relix tool screen` | Screen capture via the tool node. |

---

## Booting the mesh

The supported way to bring up the local mesh is the bringup script:

```powershell
.\scripts\relix-mesh-up.ps1           # default: provider=mock, run=local
.\scripts\relix-mesh-up.ps1 -Provider openrouter
.\scripts\relix-mesh-up.ps1 -Run myrun -BridgePort 19800
.\scripts\relix-mesh-up.ps1 -ToolAllowHttp                # accept http://
.\scripts\relix-mesh-up.ps1 -NoTool                       # skip the tool node
```

```bash
./scripts/relix-mesh-up.sh
./scripts/relix-mesh-up.sh --provider openrouter
./scripts/relix-mesh-up.sh --run myrun --bridge-port 19800
```

The script:

1. Mints the org root and the bridge's identity bundle if they don't
   exist (idempotent — re-running won't overwrite existing keys).
2. Generates per-node config under `dev-data/<run>/`.
3. Spawns memory, AI, and (unless `-NoTool`) tool controllers; waits
   for each to log `transport listening`.
4. Spawns the bridge; waits for `web bridge starting`.
5. Prints the four PIDs it owns.
6. Parks until Ctrl-C, then stops exactly those PIDs.

Default ports:

| Component | Port |
|---|---|
| memory | tcp/19711 |
| ai | tcp/19712 |
| tool | tcp/19713 |
| bridge | tcp/19791 (HTTP) |

Override via `-MemPort` / `-AiPort` / `-ToolPort` / `-BridgePort` (or
the `--mem-port` etc. equivalents in the `.sh` script).

## On-disk layout

```
dev-keys/
  <run>-org-root.key      # 32-byte Ed25519 secret. KEEP PRIVATE.
  <run>-org-root.pub      # 32-byte trust file. Referenced from every node config.
  <run>-bridge.aic        # The bridge's signed IdentityBundle.
  <run>-bridge.key        # The bridge's libp2p secret (auto-generated on first run).
  <run>-memory.key        # Each controller auto-generates its own libp2p secret.
  <run>-ai.key
  <run>-tool.key

dev-data/<run>/
  memory.toml             # generated per-node config TOMLs
  ai.toml
  tool.toml
  bridge.toml
  peers.toml              # alias -> /ip4/.../tcp/port map
  memory.db               # SQLite memory store
  memory.log / .err.log   # stdout + stderr per controller
  ai.log / .err.log
  tool.log / .err.log
  bridge.log / .err.log

dev-data/<run>-<node>/
  audit.log               # per-node hash-chained admission audit

dev-data/flow-runner/flows/
  <flow_id>.log           # per-flow event log (one per request)

configs/policies/
  <run>.toml              # the policy file the bringup script generates
```

Everything under `dev-data/` and `dev-keys/` is gitignored. Sharing
the org-root secret means sharing the ability to mint identities for
your mesh — treat it like a production CA secret.

## Provider configuration

The AI node's provider is one config line; the API key (if any) lives
on the AI node only.

```powershell
# OpenRouter (recommended for trying real models without OpenAI account)
$env:OPENROUTER_API_KEY = 'sk-or-...'
.\scripts\relix-mesh-up.ps1 -Provider openrouter

# Local Ollama / vLLM / llama.cpp (no key)
.\scripts\relix-mesh-up.ps1 -Provider local -BaseUrl http://localhost:11434/v1

# Anthropic
$env:ANTHROPIC_API_KEY = 'sk-ant-...'
.\scripts\relix-mesh-up.ps1 -Provider anthropic
```

Full provider matrix (TOML keys, default models, status of each
backend) is in [`provider-configuration.md`](provider-configuration.md).

## Stopping the mesh safely

**Inside the script's terminal: Ctrl-C.** The script intercepts the
signal, prints `stopping mesh (only PIDs started by this script)...`,
and `Stop-Process` / `kill`s exactly the four PIDs it tracked. Nothing
else on the machine is touched. The script does **not** use
`taskkill /IM` or any name-based sweep — unrelated `relix-*.exe`
processes you may have running won't be affected.

**If the script crashed or the terminal died before Ctrl-C**, the
children orphan. Find them with:

```powershell
Get-Process -Name 'relix-controller','relix-web-bridge'
```

```bash
pgrep -fa 'relix-controller|relix-web-bridge'
```

Then `Stop-Process -Id <pid>` / `kill <pid>` exactly those PIDs. The
script's stdout (captured under `$env:TEMP/relix-mesh-up.out.log` on
Windows or wherever you redirected it) lists the PIDs it printed.

## Logs

| File | Contents |
|---|---|
| `dev-data/<run>/memory.log` | Memory controller stdout (`tracing` lines: startup, admission). |
| `dev-data/<run>/ai.log` | AI controller stdout. Provider call latencies / failures. |
| `dev-data/<run>/tool.log` | Tool controller stdout. **Includes structured WARN on every SSRF rejection and on every per-hop redirect rejection.** |
| `dev-data/<run>/bridge.log` | Bridge stdout. Discovery summary at startup, then per-request DEBUG. |
| `dev-data/<run>-<node>/audit.log` | Per-node admission audit (CBOR; read with `relix-flow-inspect --audit`). |
| `dev-data/flow-runner/flows/<flow_id>.log` | Per-flow event log (CBOR; read with `relix-flow-inspect --flow`). |

Tail the bridge during a request:

```powershell
Get-Content -Wait dev-data/local/bridge.log
```

```bash
tail -F dev-data/local/bridge.log
```

The HTTP response always includes the `flow_id` (or `flow_log` path)
in its JSON; cross-reference into `dev-data/flow-runner/flows/` to
inspect the exact RemoteCall sequence.

## Open WebUI

The bridge's OpenAI-compatible shim is a thin translation layer over
the same SOL flow that `/chat` uses. Setup is one Open WebUI screen.

In **Settings → Connections → OpenAI API**:

| Field | Value |
|---|---|
| API Base URL | `http://host.docker.internal:19791/v1` (Open WebUI in Docker on macOS/Windows) |
| | `http://127.0.0.1:19791/v1` (Open WebUI running natively) |
| API Key | the bridge bearer token from `~/.relix/bridge-token` (required; the bridge validates it) |
| Model picker | shows `relix-mock` by default, plus whatever your AI node was configured with (e.g. `relix-openrouter`) |

Conversations stick to a session id that is a stable hash of the first
system + user message — the same conversation lands in the same memory
bucket as it grows. Subsequent turns from the same conversation read
history from the memory node.

The shim preserves `system` messages (prepended as `[SYSTEM N]\n...\n\n`
blocks) and rejects `tools` / `tool_calls` / `role:"tool"` with 400. Sampling
controls (`temperature`, `top_p`, ...) are accepted but ignored at the
provider side. Full detail in
[`streaming-and-openai-shim.md`](streaming-and-openai-shim.md).

## Triggering `tool.web_fetch`

Two operator paths exist:

```bash
# Native: explicit URL parameter.
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"summarize","url":"https://example.com/"}'

# OpenAI shim: any http(s) URL in the user message auto-routes
# through the tool flow.
curl -X POST http://127.0.0.1:19791/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"relix-mock","messages":[{"role":"user","content":"fetch https://example.com/ and summarize"}]}'
```

The tool node is opt-in at script level: pass `-NoTool` to skip it,
in which case `/chat_with_tool` is 404 and the OpenAI shim won't
auto-route.

## Common failures and what they mean

### `client could not be built` on first request

The tool node failed its startup probe of `reqwest::Client::builder().build()`.
Almost always a missing system root-cert store. On minimal Linux
containers, install `ca-certificates` (or set `SSL_CERT_FILE`).

### `mesh transport: flow halted: remote_call(tool, tool.web_fetch): kind=6 cause=tool.web_fetch ssrf-rejected: ...`

Working as intended — the SSRF guard refused a URL. The exact reason
is in the `cause` string. If you got it on a URL you believe is safe:

- Hostname in `.local` / `.internal` / `.intranet` / `.lan` / `.corp`
  / `.home` / `.private` suffix? These are on the denylist. Use a
  public hostname.
- Hostname resolves to an RFC 1918 / link-local address? The DNS-resolved-
  IP check refuses. Run `nslookup <host>` to confirm what your resolver
  returned.

### `mesh transport: flow halted: remote_call(ai, ai.chat): kind=11 cause=ai.chat: ...`

The AI provider returned an error. Check `dev-data/<run>/ai.log`. Most
common: missing or invalid API key (the AI node logs which env var it
expected to find).

### `policy_denied: no allow rule for method X matches caller Y`

The policy file has no `[[rules]]` entry that admits caller's groups
for that method. Default is to deny. The bringup script generates a
policy that admits `chat-users` for every alpha method; if you wrote
your own policy you need explicit rules for `node.health`,
`node.manifest`, every memory method, `ai.chat`, and `tool.web_fetch`.

### Bridge prints `bridge discovery did not return a mesh client`

The bridge's startup discovery pass failed to dial any peer (every
configured peer alias timed out). The bridge stays up and chat
requests fall back to the per-request ephemeral transport path. Check
that the peer ports are actually listening:

```powershell
Test-NetConnection 127.0.0.1 -Port 19711
Test-NetConnection 127.0.0.1 -Port 19712
Test-NetConnection 127.0.0.1 -Port 19713
```

```bash
ss -ltn '( sport = 19711 or sport = 19712 or sport = 19713 )'
```

### Bridge log says `discovery: peer returned error ... no allow rule for method node.manifest`

Your policy file is missing the `[[rules]]` entry for `node.manifest`.
The bringup script's generated policy includes one. If you copied an
older policy, add:

```toml
[[rules]]
name = "node_manifest"
method = "node.manifest"
allow_groups = ["chat-users"]
```

### `address already in use` on startup

Another process is on one of the ports. Use the script's port-override
flags or `Get-NetTCPConnection -LocalPort 19791` / `lsof -iTCP:19791` to
find the squatter.

### Peer dropped mid-session and now /chat fails

The bridge's pooled `MeshClient` doesn't auto-reconnect on peer
disappearance in the alpha. Restart the bridge (Ctrl-C the script and
re-run it). This is a documented limitation
([`current-limitations.md`](current-limitations.md)).

### Open WebUI shows `Network error` or no models

- API Base URL must end in `/v1` (not just the host:port).
- If Open WebUI is in Docker on macOS/Windows, use `host.docker.internal`
  instead of `127.0.0.1`.
- If `curl http://127.0.0.1:19791/v1/models` works but Open WebUI
  doesn't, the issue is in the Open WebUI container's network — check
  its container logs.

## Operator dashboard

Open `http://127.0.0.1:19791/dashboard` for the operator
console. The sidebar lists twenty-two panels (selected by
click; there is no `#/...` hash routing). Source of truth:
the `SECTIONS` array in `crates/relix-web-bridge/src/dashboard.html`.

| Panel | What it shows |
|---|---|
| Overview | KPI grid (24h requests/cost, pending approvals, recent sessions), a System Health card grid that rolls up `/v1/topology` peer counts + per-agent scores, and a Recent Activity table. The first stop for triage. |
| Tasks | Task-ledger summary + a table with status filter, search, and Spawn Task. Backed by `/v1/tasks` (with `/v1/tasks/:id/*` for detail and actions). |
| Scheduled Jobs | Cron job table with subject filter, New Job, and trigger. Backed by `/v1/cron/jobs` and `/v1/cron/jobs/:job_id/trigger`. |
| Chat | Send a message through a configured provider and read the reply + stats. |
| Memory | Search, ingest, inspector, and dialectic tabs over the memory store. |
| Approvals | Pending / history / failed-delivery / channels tabs. |
| Skills | Skill catalogue + statistics; create/deprecate. |
| Sessions | Recent sessions + content search. |
| Reasoning | Smart routing, self-consistency, belief state, judge verdicts. |
| Credentials | Vault, rotation schedule, per-credential audit log. |
| Identity | Active session tokens + research identity. |
| Cost & Metrics | Cost by provider/agent, 24h trend, baselines, alerts, spend caps. |
| Observability | OTel/sink status, per-agent health, session debugger, provenance, alert history. |
| Policy Denials | Recent admission denials with a peer filter. Backed by `/v1/policy/denials`. |
| Multi-Tenant | Tenant list + per-tenant detail. |
| Planning | Create/inspect plans (planner + critic). |
| Workflows | Active + registered workflows; reload from disk. |
| Email | SMTP/IMAP status + recent inbound messages. |
| Plugins | Installed subprocess plugins. |
| MCP Servers | Registered MCP servers with a peer filter, tool listing, and invoke. Backed by `/v1/mcp/servers`, `/v1/mcp/tools`, `/v1/mcp/invoke`. |
| Configuration | Providers, routing tiers, effective (redacted) config. |
| Logs | Live `/v1/logs/stream` tail with level/text/target filters. |

There is no standalone Audit-log panel; audit data is reachable
through the Credentials, MCP, and Multi-Tenant panels and the
hash-chained `audit.log` files (read with `relix-flow-inspect`).
The `#/tasks` task-detail "causality surfaces" described later in
this guide are a separate, pre-v0.3.0 hash-routed design and do not
match the current Tasks panel; the underlying data is on
`/v1/tasks/:id/*` and `relix task`.

### Configuring providers (dashboard-first)

> **Stale (2026-06-02).** The shipped Configuration panel is
> read-only: it lists each provider with its default model and
> `configured` / `enabled` / `is_default` flags
> (`config-providers` in `dashboard.html`). There is no
> provider card, no key-entry field, no per-provider "Test
> connection", and no "Test all configured" matrix; the only
> live connection test is Telegram (`/v1/config/telegram/test`).
> Set provider keys with `relix setup` (the wizard) or in
> `~/.relix/config.toml`; see the README "Quick start" and
> [configuration.md](configuration.md). The section below
> describes a pre-v0.3.0 dashboard and is kept for history;
> RELA-25 tracks rewriting it.

You **do not** need to hardcode keys, export env vars, or
edit TOML. Open `#/providers`, pick a card, paste the
key, click Save. Keys persist to a local
`bridge-secrets.toml` at mode 0600 (gitignored). The
dashboard never echoes the key back over HTTP — only
`…last4` previews + a `configured` flag.

Each card has a **Test connection** button after the
first save. It hits the upstream provider's models
endpoint with the saved key and reports success/failure
+ elapsed_ms inline. The page also ships a **Test all
configured** batch button that fires the per-provider
test endpoint in parallel for every configured provider
and renders a matrix (provider, status, elapsed, detail)
so operators can preflight a fresh environment in one
click. Disabled providers are tested too — their key is
still stored and routing may be re-enabled at any moment.

The **default model** input on each card carries a
curated `<datalist>` of common model ids per provider so
the typical setup is one click instead of a doc lookup.
The input stays free-text — operators using newer or
unlisted models can type any value.

When a key changes, the dashboard shows
`saved · restart AI controller to apply` — provider keys
are read at controller startup, not at every chat. Stop
+ start the AI controller for the change to take effect.

Each configured card carries an **Enable / Disable**
button. A disabled provider stays in the secrets file
(key preserved across the toggle, and across overwrites)
but the AI controller treats it as routing-ineligible.
Useful for cycling between providers without losing a
key, or for quarantining a key that's misbehaving while
you investigate.

### Configuring Telegram

> **Stale (2026-06-02).** The shipped console has no Telegram
> page or form; the Configuration panel is read-only. Set the
> Telegram bot token and mode with `relix setup` or in
> `~/.relix/config.toml`. A connection test is available over
> HTTP at `POST /v1/config/telegram/test` (calls Telegram
> `getMe`). The walkthrough below describes a pre-v0.3.0
> dashboard; RELA-25 tracks rewriting it.

`#/telegram` ships a copy-paste @BotFather walkthrough
and a single form: paste the token, pick `polling` mode,
click Save. **Test connection** calls Telegram's
`getMe` and reports back the bot's @username on success
so operators verify they wired the right bot. Same
secret handling as providers — token never echoed back,
URL token-fragments scrubbed from any error string.

`webhook` mode reveals an additional **Webhook URL**
input. The bridge persists both the mode and the URL,
and the response toasts an honest pending-implementation
note: the URL is stored so operators can pre-configure,
but the live HTTPS receiver wiring is still pending.
Until that lands, the channel controller continues using
polling regardless of the saved mode.

### Live runtime feel

When the Overview panel is open it polls
`/v1/observability/health`, `/v1/topology`,
`/v1/metrics/cost`, `/v1/approval/pending`, `/v1/sessions`,
`/v1/observability/alerts`, and `/v1/intervention/recent`,
and re-runs the active panel on a 30s auto-refresh
(`AUTOREFRESH_MS` in `dashboard.html`; approvals refresh on a
separate 10s timer). The Recent Activity table merges recent
intervention-log entries and active cost alerts, newest first
(subsystem, kind, description, relative time).

The earlier draft of this guide described a diffing "activity
rail" with clickable task/peer navigation and a peer drawer.
That design predates the v0.3.0 console rebuild and is not in
the shipped dashboard; the current Recent Activity table is
read-only.

The overview also surfaces a **Top retried tasks (15 min)**
card that groups recent `retried_from` edges by task_id
and links each row directly into the task detail view —
so when the retry-storm anomaly fires, you can name the
actual offending tasks without searching.

The task list ships an **age** column derived from
`updated_at`. Running/retrying rows older than 120s
gain a `stuck?` accent. A matching **stuck?** quick-
filter chip narrows the list to only those rows
(client-side post-filter on top of `status=running`).
The flag persists into the URL as `?stuck=1` so the
filter survives reloads + sharing.

### Per-task causality surfaces

> **Stale (2026-06-02).** The console does have a Tasks panel
> (a ledger table with status filter, search, and Spawn Task), but
> it has no `#/tasks` hash route and does not render the
> specific causality surfaces described below; those are a separate
> pre-v0.3.0 design. The underlying data is on the task HTTP API
> (`/v1/tasks/:id`, `.../lineage`, `.../attempts`, `.../edges`,
> `.../events/stream`, `.../export`) and the `relix task` CLI
> (`get`, `watch`, `lineage`, `export`). RELA-25 tracks rewriting
> this section against the shipped Tasks panel and those surfaces.

Open any task on `#/tasks` to see four causality
surfaces stacked above the chronicle:

- **Retry chain** — horizontal pills, one per attempt,
  with the inter-attempt wait between them and a
  click-to-jump anchor to the triggering chronicle
  event. A "queued for Xs before attempt 1 started"
  line appears when the task waited noticeably between
  creation and the first attempt — flagged as
  `(backpressure?)` when the wait crosses 30s.
- **Execution graph** — SVG of attempts + recorded
  edges (today: `retried_from`; others reserved with
  honest "edge not recorded" / "no producer yet"
  labels). Nodes are clickable: clicking a node toggles
  the timeline filter to that attempt, mirroring the
  chain-pill affordance. The card header includes
  total wall clock, the terminal attempt's duration,
  and a **retry tax** measurement (wall time spent in
  retries that didn't produce the final result).
- **Failure panel** — for `failed`/`interrupted`/
  `cancelled` tasks, the latest failure class +
  reason + the canonical operator playbook for that
  class (no invented recovery steps).
- **Cross-references** — peer correlations, execution
  path, and edge feeds populated asynchronously after
  the detail loads.

### Recovery scan results

The global **Recover** button fires
`POST /v1/tasks/recover`, which promotes overdue
`running` tasks to `interrupted` (`failure_class=timeout`).
The dashboard now pins a **Last recovery** panel above
the tasks list showing the scan's outcome: timestamp,
count, and clickable task-id links for each row that was
promoted. The panel overwrites on each scan, dismissable.

### Production note

The `/v1/config/*` endpoints have **no authentication**
at the HTTP layer. Put a reverse proxy with auth in
front before exposing the bridge beyond loopback. The
dashboard banners on the providers / telegram pages
restate this. See
[`docs/deployment.md`](deployment.md) for the
production-hardening checklist.

## Inspecting tasks (durable orchestration ledger)

When the Coordinator peer is up, every chat request becomes a Task on
its SQLite ledger. The response includes a `task_id` (top-level on
native endpoints, under `relix.task_id` on the OpenAI shim).

The Coordinator is **fail-soft** from the bridge's perspective: if
it dies mid-session, chat still works — the `task_id` is omitted
from the response and a structured WARN line hits the bridge log.

### Basic inspection

```bash
# Recent tasks, most-recently-updated first.
relix-cli task list   --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key

# One task — raw key=value (grep-friendly for scripts).
relix-cli task get    --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --task-id <id-from-response>

# Same, but pretty-printed with a chronology timeline.
relix-cli task get    --peer ... --identity ... --client-key ... \
    --task-id <id> --pretty
```

### HTTP-side inspection (`/v1/tasks`)

For browser / curl / scripting flows that don't want a libp2p hop,
the bridge exposes the same data as JSON:

```bash
# List recent tasks. Server-side pagination + filter (Priority A).
curl http://127.0.0.1:19791/v1/tasks
curl 'http://127.0.0.1:19791/v1/tasks?status=interrupted&limit=20'
curl 'http://127.0.0.1:19791/v1/tasks?status=failed&limit=50&offset=100'

# Total count for the "N of M" pagination footer.
curl http://127.0.0.1:19791/v1/tasks/count
curl 'http://127.0.0.1:19791/v1/tasks/count?status=failed'
# -> {"count":17}

# One task with full chronicle.
curl http://127.0.0.1:19791/v1/tasks/<task_id>

# Per-attempt rows.
curl http://127.0.0.1:19791/v1/tasks/<task_id>/attempts

# Incremental chronicle fetch (long-poll friendly):
curl 'http://127.0.0.1:19791/v1/tasks/<task_id>/events?since=0&limit=200'
# -> JSON array of events with event_id > since, oldest first.
# Remember the largest event_id and poll again with since=<that>
# to get only new events. Returns [] when nothing is newer.

# One-line operator-friendly summary (same shape as the CLI's
# --pretty first line, JSON-typed for dashboard projection).
curl http://127.0.0.1:19791/v1/tasks/<task_id>/summary
# -> {"task_id":"...","status":"failed","attempt_count":2,
#     "duration_secs":12,"started_at":1700000000,
#     "last_failure_class":"transient","retries":"1/3",
#     "retry_policy":"bounded"}

# Full reconstruction in one round-trip: task + summary + attempts.
# Use when a dashboard needs the whole picture on initial render.
curl http://127.0.0.1:19791/v1/tasks/<task_id>/lineage
# -> {"task": {...}, "summary": {...}, "attempts": [...]}

# Operator-triggered recovery scan — promotes overdue running
# tasks to interrupted. Idempotent.
curl -X POST http://127.0.0.1:19791/v1/tasks/recover
# -> {"recovered":["abc...","def..."],"count":2}
```

### Browser dashboard (`/dashboard`)

For visual triage of the task ledger, the bridge serves a
single-page dashboard at `http://127.0.0.1:19791/dashboard`.
Open in a browser; click any task to see its lineage + chronology.
Tick the `auto (5s)` checkbox for live refresh.

The page is a vanilla HTML/JS dashboard with no build step. It
consumes the same `/v1/tasks*` JSON endpoints you'd hit from
curl, so anything visible in the dashboard is reproducible from
a script. The bridge introduces no per-session state to support
it — see [`docs/bridge-invariants.md`](bridge-invariants.md).

Same auth model as the rest of the bridge: none at the HTTP
layer. Put a reverse proxy in front before exposing beyond
loopback.

### Capability discovery (`/v1/capabilities`)

The bridge projects its already-discovered `ManifestCache` as
JSON so dashboards and operator scripts can answer "what's the
mesh capable of right now?" without a libp2p call:

```bash
# Every capability across every peer the bridge knows about.
curl http://127.0.0.1:19791/v1/capabilities

# Filter by planner category (added in T4 P1).
curl 'http://127.0.0.1:19791/v1/capabilities?category=fetch'

# Filter by sensitivity tag.
curl 'http://127.0.0.1:19791/v1/capabilities?tag=external:network'

# Scope to one method (returns all peers that advertise it).
curl http://127.0.0.1:19791/v1/capabilities/tool.web_fetch
```

Each entry includes the peer alias, node id, node type, the
descriptor fields (kind / idempotency / cost_class / sensitivity
tags / requires_groups / policy attachment point), and the T4 P1
advisory fields (description / categories / environment_requirements)
when set.

This endpoint is **read-only** and exposes information already
visible to any peer via `node.manifest`. No invariant is changed
by serving it. See
[`docs/capability-discovery.md`](capability-discovery.md) for the
planner-foundations design.

Response shape is documented inline in
[`crates/relix-web-bridge/src/tasks.rs`](../crates/relix-web-bridge/src/tasks.rs).
The endpoints return `503` when the bridge has no Coordinator
configured, `404` when the task id is unknown, `400` on malformed
ids, and `502` for other Coordinator errors (the cause string is in
the JSON body for triage).

The bridge stays translation-only: each route is a thin forwarder
to the same `task.*` capability the CLI calls, with the same
admission pipeline running on the Coordinator. **There is no
authentication at the HTTP layer** — put a reverse proxy in front
if you expose this surface beyond loopback.

`--pretty` reformats the response as a header block plus a timeline
of events with absolute timestamps and `+Δs` deltas. The header
includes `retry_count`, `retry_policy`, `max_retries`,
`max_runtime_secs`, `started_at`, `last_failure_class`, and
`last_failure_reason` when set — everything an operator needs to
triage.

### Status filtering

C1c adds client-side `--status` filtering on `task list`:

```bash
# Anything the recovery scan flipped from running to interrupted.
relix-cli task list --peer ... --identity ... --client-key ... \
    --status interrupted

# Outright failures.
relix-cli task list --peer ... --identity ... --client-key ... \
    --status failed

# Tasks the bridge marked as waiting on a human / async dependency
# (recorded today; resume primitive is Gate 2).
relix-cli task list --peer ... --identity ... --client-key ... \
    --status awaiting_input
```

The full status convention is in
[`docs/runtime-lifecycle.md`](runtime-lifecycle.md). Valid filters:
`pending`, `running`, `retrying`, `interrupted`, `awaiting_input`,
`completed`, `failed`, `cancelled`.

### Attempt lineage

Every transition through `running` opens an attempt row on the
Coordinator. To see the per-attempt timeline:

```bash
relix-cli task attempts --peer ... --identity ... --client-key ... \
    --task-id <hex>
#   #  status       started     duration     failure       flow_id
#   1  failed       1700000000  5s           transient     flowA...
#   2  completed    1700000020  3s           -             flowB...
```

Each attempt carries its own `flow_id` + `flow_log_path` + `trace_id`
so operator forensics work across retries. Detail and contract live
in [`docs/attempt-lineage.md`](attempt-lineage.md).

### Interruption recovery

The Coordinator promotes overdue `running` tasks to `interrupted`
once at startup (when `[coordinator] recovery_scan = true`, default)
and any time an operator runs:

```bash
relix-cli task recover --peer ... --identity ... --client-key ...
# Prints one task id per recovered task, then `recovered=N`.
```

### Operator-initiated retry

```bash
# Validates state + retry_policy budget on the Coordinator. Refused
# by default for non-retryable failure classes (policy_denied /
# invalid_args / permanent); use --force after operator inspection.
relix-cli task retry --peer ... --identity ... --client-key ... \
    --task-id <hex>
# Prints: accepted attempt=2 of_budget=3
#         or: exhausted retry_count=3 budget=3
#         or: refused: last_failure_class=policy_denied ...
```

`task retry` only updates metadata + emits `task.retry_requested`.
Re-execution is the operator's job (typically `relix-cli flow-run`
with the same flow_template + params, or by re-driving through the
bridge). The bridge does NOT auto-retry today — see
[`docs/retry-model.md`](retry-model.md) for the rationale.

The scan only touches rows that have BOTH `started_at` and
`max_runtime_secs` set — a `running` row without a deadline is
indistinguishable from a long-running flow and is left alone.

It does NOT re-launch the flow. Re-launch needs a durable VM resume
model (Gate 2). The scan just re-labels so dashboards stay honest.
Full contract: [`docs/interruption-semantics.md`](interruption-semantics.md).

### Failure classification

Failed tasks carry a `last_failure_class` so operators can pattern-
match on what kind of failure they're looking at, without parsing
the cause string:

| Class | When the bridge writes it | Retry advice |
|---|---|---|
| `transient` | Network blip, peer unreachable, responder overloaded | Safe to re-run (if flow is idempotent) |
| `timeout` | Deadline exceeded or recovery scan flipped the row | Re-run with a higher `max_runtime_secs` |
| `unavailable` | Capability deprecated / removed; manifest stale | Wait, check the responder peer, re-run |
| `policy_denied` | Admission pipeline refused | Do NOT re-run; fix policy or identity first |
| `invalid_args` | Caller-side input was malformed | Do NOT re-run; fix the caller |
| `permanent` | Logic / contract error inside the flow | Do NOT re-run; investigate |

Operator playbook with concrete CLI invocations for each case is in
[`docs/task-recovery.md`](task-recovery.md). The retry model
(what `retry_policy` / `max_retries` mean today, why nothing
auto-retries yet) is in [`docs/retry-model.md`](retry-model.md).

See also: [`docs/coordination.md`](coordination.md),
[`docs/task-runtime.md`](task-runtime.md),
[`docs/attempt-lineage.md`](attempt-lineage.md),
[`docs/runtime-lifecycle.md`](runtime-lifecycle.md),
[`docs/interruption-semantics.md`](interruption-semantics.md),
[`docs/retry-model.md`](retry-model.md),
[`docs/replay-model.md`](replay-model.md),
[`docs/failure-modes.md`](failure-modes.md) —
single-page "what happens when X is down" reference for on-call use.

### Chronicle retention (save + plan, no destructive deletion yet)

The Coordinator's `task_events` table grows unbounded by design;
two operator surfaces help you plan before the destructive Step 3
capability lands. Full design in
[`docs/chronicle-retention.md`](chronicle-retention.md).

**Export one task's full chronicle for archival:**

```bash
relix-cli task export --peer ... --identity ... --client-key ... \
    --task-id <hex> --out task-snapshot.json
# Or stream to stdout for piping (jq, gzip, …):
relix-cli task export --peer ... --identity ... --client-key ... \
    --task-id <hex> --out -
```

HTTP equivalent: `GET /v1/tasks/:id/export` returns the same
single-JSON archive with `Content-Disposition: attachment`.

**Plan a retention policy without deleting:**

```bash
relix-cli task compact --peer ... --identity ... --client-key ... \
    --max-age-secs 2592000   # 30 days
# Prints a JSON object: candidate_events, candidate_tasks,
# by_task_status breakdown, oldest/newest candidate ts.
```

Browser equivalent: the **Chronicle retention** widget at the
top of the dashboard. Both call `task.compact_events` with
`mode=dry-run`. The Coordinator currently rejects any other
mode value with INVALID_ARGS — the destructive Step 3
capability is not shipped, by design.

R5 guarantee: only events whose parent task is in a terminal
state (`completed` / `failed` / `cancelled` / `interrupted`)
are counted. In-flight tasks are never candidates.

## Inspecting flows after the fact

Every chat response includes `flow_id` and `flow_log`. To replay what
the orchestration did:

```bash
cargo run -p relix-flow-inspect -- --flow dev-data/flow-runner/flows/<flow_id>.log
```

For the responder side of the same call:

```bash
cargo run -p relix-flow-inspect -- --audit dev-data/local-memory/audit.log
cargo run -p relix-flow-inspect -- --audit dev-data/local-ai/audit.log
cargo run -p relix-flow-inspect -- --audit dev-data/local-tool/audit.log
```

The two logs cross-reference by `request_id`.

## Env vars

| Var | Read by | Effect |
|---|---|---|
| `RELIX_DATA_DIR` | every controller, bridge, CLI | Override the `dev-data/` root. |
| `RUST_LOG` | every binary | Tracing filter. Default `info`. |
| `OPENAI_API_KEY` | AI node when `provider = "openai"` | Provider auth. |
| `OPENROUTER_API_KEY` | AI node when `provider = "openrouter"` | Provider auth. |
| `XAI_API_KEY` | AI node when `provider = "xai"` | Provider auth. |
| `ANTHROPIC_API_KEY` | AI node when `provider = "anthropic"` | Provider auth. |
| `GEMINI_API_KEY` | AI node when `provider = "gemini"` (placeholder; see provider doc) | Provider auth. |

The bridge does **not** read any provider env var. Provider keys
never leave the AI node's process memory.

## Upgrading

- Pull, `cargo build --workspace`, restart the mesh.
- Re-running the bringup script is idempotent: it does not overwrite
  existing `dev-keys/*`. Old identity bundles continue to work until
  expiry; the bringup-script-minted ones default to 24h, so a
  long-running mesh occasionally needs `relix-cli identity mint`
  re-runs.
- Schema changes (memory DB, audit log format) are wire-format
  changes that bump a workspace version. Read `CHANGELOG-SPEC.md`
  before upgrading across one.

## See also

- [`getting-started.md`](getting-started.md) — first boot.
- [`security.md`](security.md) — what the admission pipeline enforces.
- [`tool-node.md`](tool-node.md) — the tool peer in depth.
- [`current-limitations.md`](current-limitations.md) — what to expect from the alpha.
