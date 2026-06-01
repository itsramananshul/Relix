# Alpha Bringup Runbook

This runbook brings up the full alpha mesh from scratch on a single host.

## Prerequisites

- Rust 1.95 (pinned in `rust-toolchain.toml`; installs automatically with `rustup`).
- Python 3.11+ and Node 20+ (for Relix Web).
- An Anthropic API key (paid account, kept private).
- Disk: ~5 GiB for build artifacts.

## One-Time Setup

```sh
# Initialize the org-root keypair (kept gitignored under dev-keys/)
mkdir -p dev-keys
cargo run -p relix-cli -- identity init-org \
    --org demo-org \
    --root-key dev-keys/org-root.key

# Mint per-identity AICs
cargo run -p relix-cli -- identity mint \
    --root-key dev-keys/org-root.key \
    --name alice \
    --groups chat-users,tool-users \
    --out dev-keys/alice.aic

cargo run -p relix-cli -- identity mint \
    --root-key dev-keys/org-root.key \
    --name bob \
    --groups guest \
    --out dev-keys/bob.aic
```

## Build

```sh
cargo build --release --workspace
```

First build downloads libp2p 0.54 and dependencies — several minutes. Subsequent builds are seconds.

## Configure

The repository ships example configs in `configs/`:

- `configs/memory-node.toml`
- `configs/ai-node.toml`
- `configs/tool-node.toml`

Each declares: node name, type, listen port, peers to dial, policy file path, capability registrations, and identity-key path.

The web bridge is NOT a controller node type — it is a separate
binary (`relix-web-bridge`) with its own config
(`configs/web-bridge.toml`). The controller hard-errors on
`node_type = "web_bridge"`, so do not boot the controller with a
web-bridge config.

**The AI node config** is the only place the Anthropic API key appears. Edit `configs/ai-node.toml` to set `[ai] api_key_path = "dev-keys/anthropic.key"` and create the file with your real key (gitignored).

## Start the Mesh

Open four terminals:

```sh
# Terminal 1 — memory node
RELIX_NODE_KEY=dev-keys/memory.key \
    cargo run --release -p relix-controller -- --config configs/memory-node.toml

# Terminal 2 — AI node
RELIX_NODE_KEY=dev-keys/ai.key \
    cargo run --release -p relix-controller -- --config configs/ai-node.toml

# Terminal 3 — tool node
RELIX_NODE_KEY=dev-keys/tool.key \
    cargo run --release -p relix-controller -- --config configs/tool-node.toml

# Terminal 4 — web bridge (SEPARATE binary, not the controller)
RELIX_NODE_KEY=dev-keys/web-bridge.key \
    cargo run --release -p relix-web-bridge -- --config configs/web-bridge.toml
```

Each controller on startup:
1. Loads/generates its identity keypair.
2. Builds and signs its manifest.
3. Binds to its libp2p TCP port.
4. Dials configured peers.
5. Exchanges manifests.
6. Becomes ready.

## Start Relix Web

```sh
cd relix-web

# Backend (Python)
RELIX_MODE=true \
RELIX_BRIDGE_URL=http://127.0.0.1:9100 \
    python -m relix_web.main

# Frontend (separate terminal)
npm install
npm run dev
```

Browse to `http://127.0.0.1:5173` (or whatever port Vite reports).

## M5 Two-Controller RPC Demo (alpha-current)

This is the path that actually works today (M5 milestone). M7+ adds the memory / AI / tool / web nodes.

```sh
# Terminal 1 — start a controller (any node config; memory used here).
RELIX_DATA_DIR=dev-data \
RUST_LOG=relix_runtime=info \
    cargo run --release -p relix-controller -- --config configs/memory-node.toml

# Terminal 2 — issue an org root and mint identities.
cargo run -p relix-cli -- identity init-org \
    --root-key dev-keys/org-root.key --org demo-org
# `init-org` ALSO writes the companion public key to
# dev-keys/org-root.pub (the 32-byte Ed25519 public key, derived
# from the keypair). Trust config points at the .pub.
# NEVER copy the secret .key over the .pub — that would publish
# the org-root SECRET key as if it were public.

cargo run -p relix-cli -- identity mint \
    --root-key dev-keys/org-root.key \
    --name alice --groups chat-users --out dev-keys/alice.aic
cargo run -p relix-cli -- identity mint \
    --root-key dev-keys/org-root.key \
    --name bob --groups guest --out dev-keys/bob.aic

# Ping the controller as alice (admit by `chat_users_health` rule):
#   On git-bash / MSYS: prefix with MSYS_NO_PATHCONV=1 to avoid path mangling.
cargo run -p relix-cli -- ping \
    --peer /ip4/127.0.0.1/tcp/9001 \
    --identity dev-keys/alice.aic \
    --client-key dev-keys/org-root.key

# Expect: OK from <node-id>, structured node.health payload (name, type, status, runtime).

# Same call as bob (denied — guest group not in `chat_users_health`):
cargo run -p relix-cli -- ping \
    --peer /ip4/127.0.0.1/tcp/9001 \
    --identity dev-keys/bob.aic \
    --client-key dev-keys/org-root.key

# Expect: ERR kind=6 cause=deny:default_deny:...

# Inspect the responder's audit log.
cargo run -p relix-cli -- ../  # (not needed; below is the inspector)
cargo run -p relix-flow-inspect -- --audit dev-data/<node-name>/audit.log
```

Two ready-made wrappers at the repo root:

- `scripts/alpha-bringup-m5.sh` — sets up keys + starts the controller + runs both ping cases (POSIX / git-bash).
- `scripts/alpha-bringup-m5.ps1` — same flow as a PowerShell script.

## M6 SOL Flow Demo — `flow-run` (alpha-current)

The M6/S4 milestone adds **real SOL `remote_call` orchestration** through the same libp2p RPC path proven in M5. A `.sol` file is compiled in `relix-cli`, attached to a libp2p-backed `RemoteCallDispatcher`, and executed against a real controller process.

```sh
# Single-command demo: mints alice + bob, starts the controller, runs
# flows/ping.sol as both identities, prints flow log + responder audit.
./scripts/alpha-bringup-m6.sh
```

Manual command shape:

```sh
cargo run -p relix-cli -- flow-run \
    --flow flows/ping.sol \
    --identity dev-keys/alice.aic \
    --client-key dev-keys/org.key \
    --peers configs/peers.toml \
    --deadline-secs 30
```

Where `configs/peers.toml` declares the peers the SOL flow may target:

```toml
[peers.controller]
addr = "/ip4/127.0.0.1/tcp/19501"
```

And the SOL flow itself (`flows/ping.sol`):

```sol
function start() -> str {
    let result: str = remote_call("controller", "node.health", "");
    print(result);
    return result;
}
```

The runner outputs:

```text
# Relix flow run
flow_id:       <16 hex bytes>
trace_id:      <16 hex bytes>
flow_log:      dev-data/flow-runner/flows/<flow_id>.log
status:        ok
return:        name=<node>
               type=<type>
               status=ok
               runtime=<semver>
```

Each invocation writes a flow log (`dev-data/flow-runner/flows/<flow_id>.log`) with `FlowStarted` → `RemoteCallIssued` → (`RemoteCallCompleted` or `RemoteCallFailed`) → (`FlowCompleted` or `FlowFailed`). Inspect with:

```sh
cargo run -p relix-flow-inspect -- --flow <path> --human
```

The responder's audit log shows one record per RPC, joinable across nodes by `request_id`.

### M6 chained orchestration — two-controller demo

`flows/chained_health.sol` calls `node.health` on a `memory` peer and then on an `ai` peer in sequence, proving real multi-peer SOL orchestration with trace continuity and per-call audit on each responder.

```sh
./scripts/alpha-bringup-m6-chained.sh
```

The script:
1. Mints alice (`chat-users`) and bob (`guest`).
2. Starts two controller processes — `m6chained-memory` on tcp/19501 and `m6chained-ai` on tcp/19502 — sharing the same trust root.
3. Runs `flows/chained_health.sol` as alice; expects success with a 6-event flow log:
   `FlowStarted` → `RemoteCallIssued(memory)` → `RemoteCallCompleted(memory)` → `RemoteCallIssued(ai)` → `RemoteCallCompleted(ai)` → `FlowCompleted`.
4. Runs the same flow as bob; expects exit 2 and a 4-event flow log:
   `FlowStarted` → `RemoteCallIssued(memory)` → `RemoteCallFailed` → `FlowFailed`. The flow short-circuits at the first denied call; the ai responder is never reached.
5. Prints each responder's audit log. Both records correlate to the flow events by `request_id`.

Peer alias map used by the SOL flow:

```toml
# configs/peers-chained.toml
[peers.memory]
addr = "/ip4/127.0.0.1/tcp/19501"

[peers.ai]
addr = "/ip4/127.0.0.1/tcp/19502"
```

## M7 memory node — `memory_demo.sol`

The first real Relix node ships in M7: a SQLite + FTS5 memory store registered behind the M5 admission pipeline. Three capabilities:

| Method                       | Arg (UTF-8, `|`-delimited)        | Return                                     |
|------------------------------|-----------------------------------|--------------------------------------------|
| `memory.write_turn`          | `session_id|role|body`            | `ok\n`                                     |
| `memory.recent_for_session`  | `session_id` or `session_id|N`    | One `role: body\n` per turn, oldest first  |
| `memory.search`              | `query` or `query|N`              | One `session_id\trole\tbody\n` per match   |

`body` may contain `|` since `write_turn` uses `splitn(3)`. SOL strings are taken verbatim per SIMP-016; typed CBOR plumbing lands at Gate 2.

Single-command demo:

```sh
./scripts/alpha-bringup-m7-memory.sh
```

The script mints alice + bob, starts a single memory controller (`m7memory-memory` on tcp/19501) with the SQLite database at `dev-data/m7memory/memory.db`, runs `flows/memory_demo.sol` as alice (writes two turns, reads history back, 8-event flow log), then runs the same flow as bob (denied at first `memory.write_turn`, 4-event flow log).

Enable a controller as a memory node by setting in its config:

```toml
[controller]
name      = "memory-node"
node_type = "memory"

[memory]
db_path = "dev-data/memory/sessions.db"
max_n   = 100   # max N for recent/search regardless of caller request
```

The controller's `register_node_type_handlers` automatically registers the three capabilities. Combine with a policy file allowing `memory.write_turn` / `memory.recent_for_session` / `memory.search` to the appropriate caller groups (see `configs/policies/memory.toml`).

## M7 first chat orchestration — `flows/chat.sol`

`flows/chat.sol` is the first end-to-end agent flow on the Relix mesh. Two real controller processes (memory + AI stub) and a 5-call SOL flow:

```sol
function start() -> str {
    let session: str  = "chat-session";
    let user_msg: str = "hello from alice";

    let history: str = remote_call("memory", "memory.recent_for_session", "chat-session");
    let reply:   str = remote_call("ai",     "ai.chat",                   "chat-session|" + user_msg);
    remote_call("memory", "memory.write_turn", "chat-session|user|"      + user_msg);
    remote_call("memory", "memory.write_turn", "chat-session|assistant|" + reply);

    print(reply);
    return reply;
}
```

Alice's happy-path flow log has **10 events** in order: `FlowStarted` → `Issued/Completed (recent)` → `Issued/Completed (ai.chat)` → `Issued/Completed (write user)` → `Issued/Completed (write assistant)` → `FlowCompleted`. Bob (`guest`) is denied at the first call with a 4-event flow log.

For M7 the AI node runs a deterministic stub responder (`[ai] mode = "stub"`). M8 swaps in Anthropic behind the same `ai.chat` capability without changing the SOL flow.

Single-command demo:

```sh
./scripts/alpha-bringup-m7-chat.sh
```

Enable a controller as the AI node with one of seven providers. Full reference: [`docs/provider-configuration.md`](../../docs/provider-configuration.md). Minimal example with the default mock provider:

```toml
[controller]
name      = "ai-node"
node_type = "ai"

[ai]
provider = "mock"
```

Switching to a real provider — only the AI-node config changes; SOL flows, bridge, web layer are untouched:

```toml
[ai]
provider = "openrouter"
model    = ""    # empty = use [ai.providers.openrouter] default_model

[ai.providers.openrouter]
base_url      = "https://openrouter.ai/api/v1"
api_key_env   = "OPENROUTER_API_KEY"
default_model = "openai/gpt-4o-mini"
```

`api_key_env` names the env var the provider reads at startup. Keys NEVER live inline in TOML. `api_key_env = ""` (or unset) means "no auth" — used by `local` (Ollama-style) servers. The bridge and other presentation peers never hold keys.

**Flow contract.** The `chat.sol` flow performs four sequential `remote_call`s, in this order:

1. `memory.write_turn` — persist user turn FIRST (crash-safe: a mid-flow failure does not lose user input).
2. `memory.recent_for_session` — readback now includes the just-written user turn.
3. `ai.chat` — pass `session_id|prompt|history` to the AI peer.
4. `memory.write_turn` — persist assistant reply.

The script appends a fifth verification call (a tiny ad-hoc SOL flow) that re-reads `memory.recent_for_session` so operators can confirm both turns landed.

## M8 web bridge — local HTTP entry point

`relix-web-bridge` is a small axum service that exposes `POST /chat` and `GET /health` on `127.0.0.1` and turns chat requests into the existing SOL chat flow. The bridge is a normal Relix peer (its own identity bundle, its own libp2p `PeerId`); it owns NO AI provider key and never orchestrates in Rust.

Single-command demo:

```sh
./scripts/alpha-bringup-m8-web-bridge.sh
```

Request shape:

```sh
curl -sS -X POST http://127.0.0.1:9100/chat \
    -H "Content-Type: application/json" \
    -d '{"session_id": "demo", "message": "hello"}'
```

Response (JSON):

```json
{
  "reply":    "<provider reply text>",
  "flow_id":  "<hex>",
  "trace_id": "<hex>",
  "flow_log": "dev-data/flow-runner/flows/<hex>.log"
}
```

`GET /health` returns `200 OK` `ok\n`.

### Bridge config (`configs/web-bridge.toml`)

```toml
[bridge]
listen_addr = "127.0.0.1:9100"

[identity]
bundle_path     = "dev-keys/bridge.aic"      # `relix-cli identity mint` output
client_key_path = "dev-keys/bridge.key"      # auto-generated on first start

[transport]
peers_path    = "configs/peers-chained.toml" # memory + ai aliases
deadline_secs = 30

[flow]
template_path = "flows/chat_template.sol"    # placeholders: {{SESSION}} / {{MESSAGE}}
```

### How a request becomes a SOL run

1. Bridge accepts the JSON, validates `session_id` and `message` reject `"`, `|`, `\n` so the substitution stays inside a SOL string literal.
2. Bridge renders `flows/chat_template.sol` into a per-request tempfile.
3. Bridge calls `relix_runtime::flow_runner::FlowRunner::run(...)` with its own identity bundle.
4. The runner dials the configured `memory` and `ai` peers and runs the SOL flow. Each `remote_call` hits the responder's full M5 admission pipeline (identity + policy + audit).
5. Bridge returns the flow's final value as `reply`, plus `flow_id`/`trace_id`/`flow_log` for inspection.

Acceptance demonstrated by the bringup script:

- 10-event flow log per request (`FlowStarted` → 4× Issued/Completed → `FlowCompleted`).
- Memory responder audit shows 3 records per request (`write_turn` user → `recent_for_session` → `write_turn` assistant).
- AI responder audit shows 1 record per request.
- `caller=web-bridge` on every audit entry — the bridge's identity, not a sneaky pseudo-user.
- `grep` over the bridge crate + bridge config rejects any reference to `ANTHROPIC_API_KEY` or `sk-ant-` — secret containment proven.

## M8/S2 streaming bridge + Open WebUI integration

The bridge ships three extra endpoints — `POST /chat/stream`, `POST /v1/chat/completions`, and `GET /v1/models` — so any OpenAI-compatible client (Open WebUI, the official `openai` SDKs, LangChain's OpenAI provider, etc.) can talk to Relix unchanged. **No Open WebUI fork required.**

Single-command demo (mock provider so it runs offline):

```sh
./scripts/alpha-bringup-m8-openwebui.sh --keep
```

Endpoint catalogue:

| Method | Path                     | Body / Output                                            | SIMP |
|--------|--------------------------|----------------------------------------------------------|------|
| `GET`  | `/health`                | `ok\n`                                                   |      |
| `POST` | `/chat`                  | JSON in / JSON out                                       | 018  |
| `POST` | `/chat/stream`           | JSON in / `text/event-stream` (Relix-native frames)      | 019  |
| `GET`  | `/v1/models`             | OpenAI-style models list                                 | 020  |
| `POST` | `/v1/chat/completions`   | OpenAI request → JSON or OpenAI-style SSE                | 019 + 020 |

Honest scope:

- **Streaming is bridge-level** (SIMP-019). The chat flow completes via the synchronous SOL dispatcher (SIMP-001 + SIMP-014); the bridge then slices the materialised reply into SSE chunks. UX matches Open WebUI's typewriter effect; latency-to-first-chunk is full flow latency.
- **OpenAI shim is request/response translation only** (SIMP-020). Bridge derives a stable `session_id` from `blake3(first_system || 0x00 || first_user)`, extracts the last `user` message as the prompt, sanitises `\n`/`\t` to spaces, rejects `"`/`|`, ignores `temperature`/`top_p`/etc., and runs the same SOL flow `/chat` uses. Provider keys live only on the AI node — the bridge does not authenticate the `Authorization` header. Bind to loopback only.
- **`GET /v1/models`** lists whatever the operator put under `[[openai_compat.models]]` in `configs/web-bridge.toml`. The model id is cosmetic — provider selection lives on the AI node.

Open WebUI configuration:

1. Run the bringup script with `--keep`.
2. Run Open WebUI locally (Docker is easiest):

   ```sh
   docker run -d -p 3000:8080 \
       -v open-webui:/app/backend/data \
       --name open-webui \
       ghcr.io/open-webui/open-webui:main
   ```
3. In Open WebUI: **Settings → Connections → OpenAI API**.
   - API Base URL: `http://host.docker.internal:19791/v1` (Docker on macOS/Windows) or `http://127.0.0.1:19791/v1` (native).
   - API Key: any non-empty string.
   - Save.
4. The model picker shows the configured ids (default demo ships `relix-mock`). Chat normally; each turn round-trips memory → ai → memory through the mesh and the same audit/flow-log infrastructure as the native `/chat` path.

Full integration story + curl examples + limitations: [`docs/streaming-and-openai-shim.md`](../../docs/streaming-and-openai-shim.md).

## Smoke Test (Acceptance, full alpha — M7+ work)

The acceptance criteria from `docs/alpha-plan.md` are verified by:

1. **CLI ping:** `cargo run -p relix-cli -- ping memory-node` succeeds with Alice's identity, fails with no identity.
2. **Browser chat:** Log in as `alice@demo-org`; send "Hello"; see streamed reply.
3. **Memory persistence:** Close browser; log in again; ask "What's my name?"; see correct recall.
4. **Tool use:** Ask "Fetch example.com and summarize"; see real fetched content.
5. **Policy denial:** Log in as Bob; send any chat; see "Access denied"; check audit log on memory or AI node — `policy_decision: deny`.
6. **Key isolation:** `grep -ri ANTHROPIC relix-web/ crates/relix-web-bridge/` returns nothing.
7. **No direct provider call:** `grep -r "api.anthropic.com\|api.openai.com" relix-web/` returns nothing.
8. **No routing in glue:** `grep -rn 'if.*method.*==.*"ai\.chat\|memory\.search"' crates/ relix-web/` returns nothing.
9. **Replay verify:** `cargo run -p relix-flow-inspect -- --flow <id> --replay-verify` prints `INTEGRITY OK`.
10. **Crash tolerance:** Kill the AI node mid-chat; see graceful failure in browser; check audit shows the failed RPC; restart AI node; retry; succeeds.

If any item fails, the alpha is not done.

## Common Issues

**libp2p TCP bind failure:** another process on the same port. Check `lsof -i :<port>` and edit the config.

**Identity verification fails:** make sure `--root-key` used to mint the AIC matches the org-root key the responder trusts (configured in node's policy file).

**Anthropic 401:** the AI node config points at a wrong/missing API-key file. Check `configs/ai-node.toml` and that the file exists.

**Web bridge 502:** the web bridge node isn't running, or its `[bridge] http_port` doesn't match Relix Web's `RELIX_BRIDGE_URL`.

## Teardown

```sh
# Ctrl+C each controller
# Wipe local data:
rm -rf dev-keys ~/.relix
```
