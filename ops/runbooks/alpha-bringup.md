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

## Smoke Test (Acceptance)

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
