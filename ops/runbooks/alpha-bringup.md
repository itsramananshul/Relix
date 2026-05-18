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
- `configs/web-bridge-node.toml`

Each declares: node name, type, listen port, peers to dial, policy file path, capability registrations, and identity-key path.

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

# Terminal 4 — web bridge node
RELIX_NODE_KEY=dev-keys/web-bridge.key \
    cargo run --release -p relix-controller -- --config configs/web-bridge-node.toml
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
# The org-root public key for trust verification:
cp dev-keys/org-root.key dev-keys/org-root.pub   # alpha: pub derived from secret-file bytes

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
