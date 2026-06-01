# Getting Started

You will end this guide with the Relix mesh running on your
machine, a chat round-tripping through the bridge, and the
operator dashboard open in your browser. End to end, three
commands — actually two if you count the wizard as part of
install.

## Install

**Mac / Linux:**

```sh
curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/itsramananshul/Relix/main/install.ps1 | iex
```

Both installers do the same four things:

1. Download `relix`, `relix-controller`, and `relix-web-bridge`
   from the latest GitHub release into `~/.local/bin` (POSIX) or
   `%USERPROFILE%\.local\bin` (Windows) and put that directory
   on your `PATH`.
2. Download the platform-appropriate mesh scripts
   (`relix-mesh-up.{sh,ps1}` + `relix-mesh-down.{sh,ps1}`) into
   `~/.local/scripts/` so the boot command can find them.
3. Run **`relix setup`** automatically — the guided wizard
   below.
4. Save your configuration to `~/.relix/config.toml`.

Set `RELIX_INSTALL_DIR=/opt/relix/bin` or
`RELIX_VERSION=v0.4.1` before piping if you want to override
defaults.

## The setup wizard

`relix setup` is an interactive seven-page wizard. It runs
automatically right after install; you can also run it any time
later to change provider, rotate keys, or add a channel.

**Non-terminal guard:** when stdin is not a TTY (for example in a
`curl | bash` or `irm | iex` pipeline), the wizard detects this,
saves the prior config or defaults, and exits cleanly — it prints a
message directing you to run `relix setup` in a terminal. No raw
mode is entered.

**Docker / Qdrant prompt (page 1, pre-flight):** before entering
the wizard proper, the setup checks Docker, Ollama, and Qdrant. If
Qdrant is not running you are offered:

```
[1] with memory   (requires Docker + Qdrant)
[2] without memory
```

Default is `[2]` on blank input or closed stdin. If you choose
`[1]` and Docker is not running, the wizard exits with an
actionable message — start Docker, then re-run `relix setup`.

The pages are:

```
╔══════════════════════════════════════════╗
║      RELIX — Relay Intelligence          ║
║              Exchange  v0.4.1            ║
║                                          ║
║         The OS for AI Agents             ║
║                                          ║
╚══════════════════════════════════════════╝
```

The Welcome page auto-advances — you do not need to press Enter.
Then:

```
Choose your AI provider
(arrow keys, Enter to confirm)

> OpenRouter   (recommended — access to all models)
  OpenAI
  Anthropic
  xAI (Grok)
  Gemini
  Local       (Ollama or any OpenAI-compatible endpoint)
  Mock        (no API key — for testing)
```

Pick a provider and press **Enter**. Unless you chose **Mock**
or **Local**, the wizard asks for an API key — input is hidden
(bullets per character), Backspace works:

```
Enter your openrouter API key
(input is hidden; Enter to confirm, Ctrl-C to cancel)

> ••••••••••••••••••••••••••
```

Then the channel multi-select:

```
Connect messaging channels (optional)
Space to toggle, arrow keys to move, Enter to continue

> [x] Telegram
  [ ] Discord
  [ ] Slack
```

For each channel you tick, a follow-up prompt asks for the bot
token (and channel ID for Discord / Slack). The Telegram prompt
points you at [@BotFather](https://t.me/BotFather); the Discord
prompt points at the Developer Portal; the Slack prompt at the
app config's OAuth section.

**Page 6 — Confidence:** a single toggle enables the confidence
scoring subsystem. Displays defaults for `window_size` and
`p95_latency_baseline_ms`.

**Page 7 — Subsystems:** two toggles — credential vault and
approval delivery. Enabling the vault generates a 32-byte random
master key (printed once to stdout; save it — it is not
recoverable). The approval channel defaults to `dashboard`.

**Confirmation page:** shows a diff of what changed from the prior
config. Press **Enter** to save; left-arrow / `b` / `B` to go
back; Ctrl-C to cancel.

The wizard writes `~/.relix/config.toml` (`chmod 600` on POSIX — it
holds your secrets) and exits.

## Boot the mesh

```sh
relix boot
```

`relix boot` reads `~/.relix/config.toml` automatically — no
environment variables to export, no `--provider` flag to repeat.
The configured provider and channels are wired in, the AI node
picks up the API key, and the mesh comes up.

You'll see something like:

```
note: no `~/.relix/config.toml` found — using defaults.
      Run `relix setup` for guided configuration.
```

…if you somehow got here without running the wizard. Otherwise
the boot script prints per-node "ready" lines as memory, ai,
tool, coordinator, your channels, and the bridge come online,
then opens `http://127.0.0.1:19791/dashboard` in your default
browser.

`relix boot` blocks on the bridge in the foreground. In another
terminal:

```sh
relix status   # is it up? print the topology table.
relix stop     # kill the controllers + bridge by name.
```

## First chat

Once the dashboard is up, point any OpenAI-compatible client at
the bridge — the official Python SDK, Open WebUI, LobeChat,
Cursor, etc.:

The bridge requires a bearer token. After `relix boot`, the token is
printed to the terminal and stored in `~/.relix/bridge-token`. Pass it
in the `Authorization` header:

```python
from openai import OpenAI
import pathlib
token = pathlib.Path("~/.relix/bridge-token").expanduser().read_text().strip()
client = OpenAI(base_url="http://127.0.0.1:19791/v1", api_key=token)
client.chat.completions.create(
    model="relix-openrouter",            # or relix-openai, relix-mock, ...
    messages=[{"role": "user", "content": "hello"}],
)
```

The real provider key lives only on the AI node, sourced from
`~/.relix/config.toml`; the bridge token is a separate local
secret that guards the HTTP surface.

For a quick smoke test from the shell (replace `<token>` with the
value in `~/.relix/bridge-token`):

```sh
curl http://127.0.0.1:19791/health
# -> ok  (public, no auth required)

TOKEN=$(cat ~/.relix/bridge-token)

curl -X POST http://127.0.0.1:19791/chat \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"hello"}'
```

Each response carries a `flow_id`, `trace_id`, and `flow_log`
path so you can inspect exactly what the orchestration did:

```sh
# Using the relix-flow-inspect binary (reads per-flow event logs):
relix-flow-inspect --flow <flow_log path>

# Or execute a SOL flow live against a running mesh:
relix flow-run --flow flows/my-flow.sol \
    --identity dev-keys/local-bridge.aic \
    --client-key dev-keys/local-bridge.key \
    --peers dev-data/local/peers.toml
```

## Stream tokens over WebSocket

`/ws/chat` streams the reply chunk-by-chunk over a WebSocket
with bearer auth on the upgrade. JSON `chunk` frames as the
provider emits text, terminated by a `done` frame with the
assembled reply.

```js
// Read the token from ~/.relix/bridge-token (bridge validates it).
const token = "<contents of ~/.relix/bridge-token>";
const ws = new WebSocket("ws://127.0.0.1:19791/ws/chat", [], {
  headers: { Authorization: `Bearer ${token}` },
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

Full client examples + the auth contract:
[`websocket.md`](websocket.md).

## Connect Telegram

If you ticked Telegram during setup the controller is already
running and listening. Send your bot any message and it replies
through the same `ai.chat` flow the HTTP bridge uses.

If you skipped Telegram and want to add it now, re-run the
wizard — it pre-fills the existing config and you only have to
fill in the new fields:

```sh
relix setup
```

Or edit `~/.relix/config.toml` directly:

```toml
[channels]
telegram        = true
telegram_token  = "12345:AAEH..."   # from @BotFather
```

…then `relix stop && relix boot` to apply.

The first-time Telegram bot creation flow on BotFather is
literally:

1. Open Telegram and search for `@BotFather`.
2. Send `/newbot`, give it a display name, give it a `_bot`
   handle.
3. Copy the token BotFather hands back.
4. Re-run `relix setup`, tick Telegram, paste the token.

Same pattern for Discord (Developer Portal → Application → Bot
→ Reset Token → also copy the Channel ID by right-clicking the
channel with Developer Mode on) and Slack (App config → OAuth &
Permissions → Bot User OAuth Token → also copy the channel ID
from channel details).

## First tool fetch

`tool.web_fetch` is SSRF-guarded and runs on the tool node:

```sh
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"summarize this page","url":"https://example.com/"}'
```

Or let the OpenAI shim auto-route: any user message containing
an `http(s)://` URL flows through the tool template instead of
the plain chat template.

Try a loopback URL to see the fail-closed posture:

```sh
curl -X POST http://127.0.0.1:19791/chat_with_tool \
  -H 'content-type: application/json' \
  -d '{"session_id":"demo","message":"x","url":"https://127.0.0.1/"}'
# -> 502 with policy_denied: ip 127.0.0.1 is in forbidden range 'ipv4 loopback (127/8)'
```

Full security details: [`tool-node.md`](tool-node.md) and
[`tool-node-security.md`](tool-node-security.md).

## Operator dashboard

`http://127.0.0.1:19791/dashboard` is opened for you when the
bridge becomes healthy. Sidebar nav:

- **Overview** — uptime, peer freshness, coordinator status,
  reconnect counters.
- **Tasks** — status-filtered task list with cursor pagination,
  per-task lineage + attempts + live SSE chronology.
- **Topology** — full peer table with fresh / stale / expired
  badges and capability counts.
- **AI Providers** — per-provider cards (read-only snapshot of
  what `config.toml` configured); rotate keys by re-running
  `relix setup`.
- **Telegram / Discord / Slack** — bot status, recent message
  ring, allowed-users list.
- **Bridge Config** — read-only snapshot of the bridge's
  effective config (secrets redacted).

The dashboard is static HTML — no build step, no JS framework.
Consumes the same `/v1/tasks*`, `/v1/topology`, `/v1/health`,
and `/v1/capabilities` endpoints `curl` reaches above.

## From source

If you'd rather build from source than install the release
binaries — for instance, you're contributing to Relix:

```sh
git clone https://github.com/itsramananshul/Relix.git
cd Relix
cargo build --workspace
relix setup     # writes ~/.relix/config.toml
relix boot      # uses your local target/debug binaries via the mesh script
```

The first build compiles libp2p and friends — budget 5–10
minutes on a cold cache. Subsequent builds are seconds. The
boot script discovers `target/debug/relix-controller` and
`target/debug/relix-web-bridge` automatically when run from the
repo root.

## Shutdown

`relix stop` kills every running `relix-controller` and
`relix-web-bridge` by name — `taskkill /F /IM` on Windows,
`pkill -x` elsewhere. Idempotent: exits 0 if nothing was
running. Your config and data under `~/.relix/` are preserved
so the next `relix boot` picks up exactly where you left off.

## What next

- [`architecture.md`](architecture.md) — how the pieces fit together.
- [`configuration.md`](configuration.md) — `~/.relix/config.toml`
  reference, every TOML key per node, every env-var override.
- [`memory.md`](memory.md) — chat history, vector search,
  persistent agent memory, the new RAG + auto-history paths.
- [`sol.md`](sol.md) — write your own flow in SOL or `.sflow`.
- [`channels/index.md`](channels/index.md) — Telegram, Discord,
  Slack: bot setup, slash commands, troubleshooting.
- [`plugins.md`](plugins.md) — ship a plugin in Rust or Python.
- [`coordination.md`](coordination.md) — multi-agent tasks,
  delegation, messaging, approvals.
- [`websocket.md`](websocket.md) — `/ws/chat` wire format and
  client examples.
- [`security.md`](security.md) — threat model + admission
  pipeline.
- [`current-limitations.md`](current-limitations.md) — read
  before relying on Relix in production.
