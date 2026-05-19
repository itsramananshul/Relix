# Getting Started

You will end this guide with:

- the Relix mesh running on your machine,
- a successful chat through the bridge using the mock AI provider,
- Open WebUI (optional) talking to the mesh as if it were an OpenAI server,
- a successful `tool.web_fetch` against a public URL.

The whole thing takes a few minutes on a working Rust toolchain.

## Prerequisites

- **Rust 1.95** or newer (the workspace pins it in `rust-toolchain.toml`).
  Install via [rustup](https://rustup.rs/) if you don't already have it.
- **Git** to clone the repo.
- **A POSIX shell or PowerShell.** Windows users should use PowerShell
  (the `.ps1` bringup script is Windows-safe and uses no `taskkill`).
- **Optional**: Docker (only if you want to run Open WebUI in a
  container) and a provider API key
  ([OpenRouter](https://openrouter.ai/), [OpenAI](https://platform.openai.com/),
  Anthropic, etc.) if you want real model responses instead of the
  deterministic mock provider.

No other system dependencies. libp2p, SQLite (bundled), and reqwest's
TLS roots are all pulled in via Cargo.

## Build

```bash
git clone https://github.com/itsramananshul/Relix.git
cd Relix
cargo build --workspace
```

First build downloads + compiles libp2p and friends; budget 5–10 minutes
on a cold cache. Subsequent builds are seconds.

## Boot the mesh

The bringup script spawns four real OS processes — a memory peer, an AI
peer, a tool peer, and the HTTP bridge — and parks until you press
Ctrl-C.

```powershell
# Windows (PowerShell)
.\scripts\relix-mesh-up.ps1
```

```bash
# macOS / Linux / Git Bash
./scripts/relix-mesh-up.sh
```

The default provider is `mock` (deterministic, no API key needed). To
pick a real provider:

```powershell
.\scripts\relix-mesh-up.ps1 -Provider openrouter
$env:OPENROUTER_API_KEY = '<your key>'
```

```bash
./scripts/relix-mesh-up.sh --provider openrouter
export OPENROUTER_API_KEY='<your key>'
```

(The env var must be set in the shell that *runs the script*, since
the AI peer is the only process that ever sees it.)

When the mesh is up the script prints something like:

```
== Relix mesh up ==
  run:           local
  provider:      mock
  memory port:   tcp/19711
  ai port:       tcp/19712
  tool port:     tcp/19713  (allow_http=False)
  bridge HTTP:   http://127.0.0.1:19791
  data dir:      dev-data/local

mesh is UP.

Endpoints:
  http://127.0.0.1:19791/health
  http://127.0.0.1:19791/v1/models
  http://127.0.0.1:19791/v1/chat/completions
  http://127.0.0.1:19791/chat_with_tool

PIDs (this script will only stop these on Ctrl-C):
  relix-controller       pid 12345
  relix-controller       pid 12346
  relix-controller       pid 12347
  relix-web-bridge       pid 12348

Ctrl-C to stop the mesh.
```

Logs live at `dev-data/local/{memory,ai,tool,bridge}.log`. A clean
shutdown (Ctrl-C) stops only the four PIDs the script printed; nothing
else is touched.

## First chat

In another terminal:

```bash
# Health check.
curl http://127.0.0.1:19791/health
# -> ok

# Models the bridge knows about (mock plus anything else discovered).
curl http://127.0.0.1:19791/v1/models

# Native chat endpoint.
curl -X POST http://127.0.0.1:19791/chat \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"hello"}'

# OpenAI-compatible endpoint.
curl -X POST http://127.0.0.1:19791/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"relix-mock","messages":[{"role":"user","content":"hello"}]}'
```

Both responses include a `relix` provenance block (`flow_id`,
`trace_id`, `flow_log` path) so you can inspect what the orchestration
did:

```bash
cargo run -p relix-flow-inspect -- --flow dev-data/flow-runner/flows/<flow_id>.log
```

## Open WebUI hookup

Run Open WebUI any way you like (the official Docker image is the
simplest):

```bash
docker run -d -p 3000:8080 ghcr.io/open-webui/open-webui:main
```

Open `http://localhost:3000`. In **Settings → Connections → OpenAI API**:

| Field | Value |
|---|---|
| API Base URL | `http://host.docker.internal:19791/v1` (Docker) or `http://127.0.0.1:19791/v1` (native) |
| API Key | anything non-empty — the bridge ignores it (provider keys live on the AI node) |
| Model | `relix-mock` (default) or whatever your AI provider was set to (e.g. `relix-openrouter`) |

Send a message. The bridge translates it into a SOL flow render against
[`flows/chat_template.sol`](../flows/chat_template.sol), runs it
through the memory + AI peers, and projects the result back into the
OpenAI response shape.

## First tool fetch

`tool.web_fetch` is exposed via two paths:

1. **Native endpoint** — explicit URL parameter:

   ```bash
   curl -X POST http://127.0.0.1:19791/chat_with_tool \
     -H 'content-type: application/json' \
     -d '{"session_id":"demo","message":"summarize this page","url":"https://example.com/"}'
   ```

2. **OpenAI shim auto-route** — any user message containing an http(s)
   URL is routed through the tool flow instead of the chat flow:

   ```bash
   curl -X POST http://127.0.0.1:19791/v1/chat/completions \
     -H 'content-type: application/json' \
     -d '{"model":"relix-mock","messages":[{"role":"user","content":"please fetch https://example.com/ and summarize"}]}'
   ```

The tool peer fetches the URL, the body is spliced into the AI prompt,
and the resulting reply is persisted to memory. Try an SSRF target to
see the fail-closed posture:

```bash
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"x","url":"https://127.0.0.1/"}'
# -> 502 with policy_denied: ip 127.0.0.1 is in forbidden range 'ipv4 loopback (127/8)'
```

Full security details: [`tool-node.md`](tool-node.md) and
[`tool-node-security.md`](tool-node-security.md).

## Shutdown

Ctrl-C in the script's terminal. The script intercepts the signal and
stops the four child PIDs it tracked. Nothing else on your machine is
affected.

## What next

- [`architecture.md`](architecture.md) — how the pieces fit together.
- [`operator-guide.md`](operator-guide.md) — running the mesh, logs, common failures.
- [`flows-and-sol.md`](flows-and-sol.md) — write your own SOL flow.
- [`current-limitations.md`](current-limitations.md) — read before relying on Relix in production.
