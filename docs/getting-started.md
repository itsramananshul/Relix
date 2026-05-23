# Getting Started

You will end this guide with:

- the Relix mesh running on your machine,
- a successful chat through the bridge using the mock AI provider,
- Open WebUI (optional) talking to the mesh as if it were an OpenAI server,
- a successful `tool.web_fetch` against a public URL.

The whole thing takes a few minutes.

## Install

The simplest path is the install script:

```sh
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash

# Windows (PowerShell)
irm https://raw.githubusercontent.com/itsramananshul/Relix/main/install.ps1 | iex
```

Both install the `relix` binary under `~/.local/bin` (or the value of
`RELIX_INSTALL_DIR`) and add it to your PATH. Re-run them to upgrade.

Prefer to build from source? You need [rustup](https://rustup.rs)
and Git:

```sh
git clone https://github.com/itsramananshul/Relix.git
cd Relix
cargo build --workspace
```

The first build compiles libp2p and friends — budget 5–10 minutes on
a cold cache. Subsequent builds are seconds.

## Boot the mesh

```sh
# Default — mock AI provider, no credentials.
relix boot

# Real provider — set the env var first.
export OPENROUTER_API_KEY='<your key>'   # or OPENAI_API_KEY, ANTHROPIC_API_KEY, ...
relix boot --provider openrouter
```

The boot script spawns the mesh's controller processes (memory, ai,
tool, coordinator) plus the HTTP bridge, waits for each to come up,
and opens the dashboard in your default browser. Channels and plugins
are opt-in:

```sh
relix boot --with-telegram                    # needs RELIX_TELEGRAM_BOT_TOKEN
relix boot --with-discord                     # needs RELIX_DISCORD_BOT_TOKEN + ..._CHANNEL_ID
relix boot --with-slack                       # needs RELIX_SLACK_BOT_TOKEN + ..._CHANNEL_ID
relix boot --with-plugins --plugin-dir ./plugins
```

`relix boot` blocks on the bridge in the foreground. In another
terminal: `relix status` to see what's up, `relix stop` to tear it
down.

From-source users can call the underlying scripts directly:
`scripts/relix-mesh-up.ps1` on Windows, `scripts/relix-mesh-up.sh`
elsewhere. Both accept the same `--with-*` env vars `relix boot`
translates into.

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

## Inspect tasks (if the Coordinator is up)

When the bringup script includes a Coordinator peer, every chat
request becomes a durable Task with a lineage operators can
inspect after the fact. The chat response includes a `task_id`
field — top-level on `/chat`, under `relix.task_id` on the
OpenAI shim.

```bash
# List recent tasks as JSON.
curl http://127.0.0.1:19791/v1/tasks

# Inspect one task in full (header + chronicle).
curl http://127.0.0.1:19791/v1/tasks/<task_id>

# Quick operator summary (status, duration, failure class, retries).
curl http://127.0.0.1:19791/v1/tasks/<task_id>/summary
```

The CLI surface is richer; it prints a per-attempt chronology
timeline:

```bash
relix-cli task get --peer /ip4/127.0.0.1/tcp/19714 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --task-id <task_id> --pretty
```

Full operator playbook in
[`task-recovery.md`](task-recovery.md). The task lifecycle is
documented in [`runtime-lifecycle.md`](runtime-lifecycle.md);
per-attempt detail in [`attempt-lineage.md`](attempt-lineage.md).

## Operator dashboard (browser)

Open `http://127.0.0.1:19791/dashboard` for the operator
console. Sidebar nav with six routes:

- **Overview** — at-a-glance health KPIs (uptime, peer
  freshness, coordinator status, reconnect counters).
- **Tasks** — status-filtered task list with cursor
  pagination, per-task lineage + attempts + live SSE
  chronology, per-task **Export** button, and a
  **Chronicle retention** modal for dry-run candidate
  counting.
- **Topology** — full peer table with fresh/stale/expired
  badges and capability counts.
- **AI Providers** — per-provider cards (OpenAI,
  Anthropic, OpenRouter, xAI/Grok, Google/Gemini, mock)
  for API key + default model setup. No more hand-editing
  TOML.
- **Telegram** — Bot API token + delivery mode setup,
  with a copy-paste @BotFather walkthrough.
- **Bridge Config** — read-only snapshot of the bridge's
  effective config (secrets redacted).

Secrets supplied via the settings pages persist to a
local `bridge-secrets.toml` at mode 0600 and are
**never echoed back over HTTP**. Production: put a
reverse proxy with auth in front before exposing
beyond loopback.

The dashboard is static HTML — no build step, no JS
framework. Consumes the same `/v1/tasks*`,
`/v1/topology`, `/v1/health`, `/v1/config/*`, and
`/v1/tasks/compact_events` endpoints curl reaches above.

## See what the mesh can do

The bridge projects the discovered capability manifests as JSON,
and the CLI offers the same view:

```bash
# Every capability the bridge knows about, with descriptions.
curl http://127.0.0.1:19791/v1/capabilities

# Same data, scoped to one peer:
relix-cli capability ls --peer /ip4/127.0.0.1/tcp/19712 \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key
```

## Shutdown

Ctrl-C in the script's terminal. The script intercepts the signal and
stops the four child PIDs it tracked. Nothing else on your machine is
affected.

## Stream tokens over WebSocket

`/ws/chat` is the streaming endpoint — JSON `chunk` frames as the
provider emits text, a final `done` frame with the assembled
reply.

```js
const ws = new WebSocket("ws://127.0.0.1:19791/ws/chat", [], {
  headers: { Authorization: "Bearer dev-token" },
});
ws.onopen = () => ws.send(JSON.stringify({
  session_id: "demo",
  message:    "Hello",
}));
ws.onmessage = (ev) => {
  const f = JSON.parse(ev.data);
  if (f.type === "chunk") process.stdout.write(f.text);
  if (f.type === "done")  ws.close();
};
```

Full client examples + the auth contract: [`websocket.md`](websocket.md).

## What next

- [`architecture.md`](architecture.md) — how the pieces fit together.
- [`configuration.md`](configuration.md) — every config file, env var, and TOML key.
- [`sol.md`](sol.md) — write your own flow in SOL or .sflow.
- [`channels/index.md`](channels/index.md) — connect Telegram, Discord, or Slack.
- [`plugins.md`](plugins.md) — ship a plugin in Rust or Python.
- [`coordination.md`](coordination.md) — multi-agent tasks, delegation, messaging.
- [`memory.md`](memory.md) — chat history, vector search, persistent agent memory.
- [`security.md`](security.md) — threat model + admission pipeline.
- [`current-limitations.md`](current-limitations.md) — read before relying on Relix in production.
