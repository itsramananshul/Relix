# Operator Guide

How to run Relix, where everything lives on disk, what the logs say, and
what to do when something is wrong.

If you have not booted Relix before, start with
[`getting-started.md`](getting-started.md). This guide assumes you have
the workspace built and you want to know how to operate it.

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
| API Key | any non-empty string — the bridge ignores it |
| Model picker | shows `relix-mock` by default, plus whatever your AI node was configured with (e.g. `relix-openrouter`) |

Conversations stick to a session id that is a stable hash of the first
system + user message — the same conversation lands in the same memory
bucket as it grows. Subsequent turns from the same conversation read
history from the memory node.

The shim drops `system` messages, OpenAI tool-call payloads, and
sampling controls (`temperature`, `top_p`, ...) in the alpha. They are
accepted in the request but not forwarded; full detail in
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

## Inspecting tasks (durable orchestration ledger)

When the Coordinator peer is up, every chat request becomes a Task on
its SQLite ledger. The response includes a `task_id` (top-level on
native endpoints, under `relix.task_id` on the OpenAI shim). Operator
inspection:

```bash
relix-cli task list   --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key

relix-cli task get    --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --task-id <id-from-response>
```

`task get` returns multi-line `key=value` plus a JSON `events=[...]`
chronicle. The Coordinator is **fail-soft** from the bridge's
perspective: if it dies mid-session, chat still works — the
`task_id` is omitted from the response and a structured WARN line
hits the bridge log. See [`docs/coordinator.md`](coordinator.md) and
[`docs/replay-model.md`](replay-model.md).

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
