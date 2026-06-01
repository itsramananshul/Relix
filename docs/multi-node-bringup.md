# Multi-node bringup

**Version: 0.4.1**

Concrete recipes for running the Relix mesh across more than one
machine. The single-host quickstart in
[`getting-started.md`](getting-started.md) is the right starting
point; this doc covers the deltas when you spread the peers across
hosts (or separate containers on one host).

## What "multi-node" actually means here

Relix is **peer-native**. Every Relix component — memory, AI, tool,
coordinator, bridge, channel — is a separate OS process with its
own identity, policy, and audit log. The single-host quickstart
runs all of them on `127.0.0.1` with different libp2p ports; the
"multi-node" version replaces those loopback addresses with
real network addresses.

There is **no orchestrator, no control plane, no swarm manager.**
You start each node with its config and address; the bridge dials
the addresses you give it. That's the whole topology.

## Five concrete topologies

Listed by complexity. Each one is operationally meaningful — don't
jump past your real need.

### T1 — Single host, single process group (the quickstart)

```
┌─────────────────────────────────────────────────────┐
│  one OS host (127.0.0.1)                            │
│                                                     │
│  bridge :19791 (HTTP)                               │
│      │                                              │
│      ▼ libp2p over loopback                         │
│  memory :19712   ai :19713   tool :19714  coord :19715
└─────────────────────────────────────────────────────┘
```

Use: development, demos, single-operator tooling.
Recipe: [`scripts/relix-mesh-up.sh`](../scripts/relix-mesh-up.sh)
(POSIX) or [`scripts/relix-mesh-up.ps1`](../scripts/relix-mesh-up.ps1)
(Windows). One command; brings up all peers; Ctrl-C stops them all.

### T2 — Single host, separate containers (isolation drill)

```
┌──────────────── one OS host ────────────────┐
│  bridge container :19791                    │
│      │                                      │
│      ▼ libp2p over docker bridge network    │
│  memory   ai   tool   coordinator           │
│  (each one is its own container, port-mapped)│
└─────────────────────────────────────────────┘
```

Use: dry-running the multi-host model on one machine. Catches
"works on loopback, fails on real network" surprises (e.g. a peer
binding to `127.0.0.1` by accident).

There is no shipped docker-compose today — see "What's not yet
in-tree" below. The recipe is mechanically the same as T3 below;
you replace SSH commands with `docker compose run`.

### T3 — Multi-host, peer-per-VM (the realistic deployment)

```
┌─────────── host A ─────────┐  ┌─────────── host B ─────────┐
│  bridge :19791 (HTTP)      │  │  memory peer (libp2p only)  │
│  + tool peer (egress)      │  │                             │
└────────────────┬───────────┘  └───────────────┬─────────────┘
                 │ libp2p over WAN/LAN          │
                 ▼                              ▼
┌─────────── host C ─────────┐  ┌─────────── host D ─────────┐
│  ai peer (provider key)    │  │  coordinator peer (SQLite)  │
└────────────────────────────┘  └────────────────────────────┘
```

Use: realistic production-style layout. Each host owns one
responsibility; failure of any one host degrades exactly one
capability (per
[`failure-modes.md`](failure-modes.md)).

Recipe is the per-host version of the single-host script: see
"Per-node configuration" below.

### T4 — Multi-host, multi-AI (provider redundancy)

```
┌─────────── host A ─────────┐  ┌─────────── host B ─────────┐
│  bridge :19791             │  │  memory peer                │
│  tool peer                 │  │                             │
└──────────────┬─────────────┘  └────────────────────────────┘
               │
   ┌───────────┴───────────┐
   ▼                       ▼
┌─── host C ───┐  ┌─── host D ───┐  ┌─── host E ───┐
│ ai-primary   │  │ ai-secondary │  │ coordinator   │
│ (openrouter) │  │ (anthropic)  │  │              │
└──────────────┘  └──────────────┘  └──────────────┘
```

Use: provider redundancy at the AI layer. The bridge picks an AI
peer by capability + descriptor; today selection is
first-match-in-cache, not score-based. Two AI peers will share
load only if their identity / categories differ enough that flows
explicitly target one.

**Not shipped today:** descriptor-aware capability scoring /
sticky-session routing. The architecture admits it (every AI peer
advertises `ai.chat`); a follow-up planner change is required to
actually load-balance.

### T5 — Channel-augmented (Telegram in the mix)

```
┌──── operator host ────┐         ┌──── channel host ────┐
│ bridge :19791         │  ◄──    │ telegram channel      │
│ memory + ai + coord   │  libp2p │ controller            │
│ + tool                │         │ + SqliteSessionStore  │
└───────────────────────┘         └──────────┬───────────┘
                                              │
                                          Telegram Bot API (HTTPS out)
```

Use: production with a Telegram-driven channel. The channel is a
controller like any other — it just speaks Telegram on the
outside instead of HTTP. Same dial-and-call pattern to the rest
of the mesh.

Today the channel scaffold is shipped without the live HTTPS
client. Operators supply the Bot API token via the operator
dashboard's Telegram settings page; the live client implementation
lands once a `reqwest`-backed `BotApi` impl is added alongside
the existing `MockBotApi`.

## Per-node configuration deltas

Each controller config file (`<node>-controller.toml`) has three
fields that change between single-host and multi-host:

### `[transport]` — bind address

Single-host (loopback):

```toml
[transport]
listen = "/ip4/127.0.0.1/tcp/19712"
```

Multi-host (LAN):

```toml
[transport]
listen = "/ip4/10.0.1.7/tcp/19712"
```

Multi-host with NAT (cloud VM):

```toml
[transport]
listen = "/ip4/0.0.0.0/tcp/19712"
# Peers dial the public address; the controller binds to all
# interfaces.
```

### Bridge's `[peers.<alias>] addr`

The bridge needs the **public** multiaddr of each peer. From the
bridge's host, run:

```bash
# Find the peer-id-suffixed multiaddr the peer prints on startup,
# or check its identity bundle:
relix-cli identity inspect --bundle dev-keys/local-memory.aic
```

Then in the bridge's `bridge.toml`:

```toml
[peers.memory]
addr = "/ip4/10.0.1.7/tcp/19712/p2p/<PEER_ID>"

[peers.ai]
addr = "/ip4/10.0.1.8/tcp/19713/p2p/<PEER_ID>"
```

### `[policy]` — same file on every node

The policy file controls what each peer accepts from callers. In
multi-host you can either (a) copy the same `policy-shared.toml`
to every host (simplest, what the single-host script does) or (b)
maintain per-node policy files so each peer only allows the
callers it should see.

For (a), see the policy used by `scripts/relix-mesh-up.sh`.
For (b), every peer's policy must include `node.health` and
`node.manifest` for every caller that will discover it.

**Wire protocols used in this topology:**
- Unary RPC: `/relix/rpc/1` (libp2p `request_response`, CBOR)
- Streaming: `/relix/rpc/stream/1` (libp2p stream, length-prefixed CBOR frames)
- Manifest responses are fully Ed25519-signed `SignedManifest` envelopes.
  The bridge pins each peer's signing key via TOFU on first contact and
  rejects manifests with a different key on subsequent refreshes. TOFU
  pins are in-memory; a bridge restart clears them and re-pins from the
  first received manifest.

## Identity distribution

Each controller has its own identity bundle (`<node>.aic`) +
client key (`<node>.key`). These are NOT shared:

- **Identity bundle** (`.aic`) — public. Carries the peer's
  pubkey, name, org, role, groups. Distributed to anyone who needs
  to verify the peer.
- **Client key** (`.key`) — secret. Stays on the controller's
  host. Never copied off.

For a multi-host T3 topology with 5 hosts, you mint identities
once on a single trusted host (the bridge host is a good choice
since it already has `relix-cli`), then copy the bundle + matching
key to each target host's `dev-keys/` directory.

Standard pattern (POSIX):

```bash
# On the trust-root host:
relix-cli identity init-org --org-id myorg \
    --bundle dev-keys/org-root.aic --key dev-keys/org-root.key

for node in memory ai tool coordinator bridge channel-telegram; do
    relix-cli identity mint \
        --org-bundle dev-keys/org-root.aic \
        --org-key dev-keys/org-root.key \
        --name "$node" --role "node" --groups "chat-users" \
        --bundle "dev-keys/$node.aic" --key "dev-keys/$node.key"
done

# Then SCP each (.aic, .key) pair to its target host.
# The bridge host keeps EVERY .aic (for discovery + dialling)
# but only its OWN .key.
```

## Node boot order

There's a soft order. Nothing breaks if you violate it — peers
just log discovery failures and the bridge's manifest-refresh loop
heals when the missing peers come online. But to get clean logs
the first time:

1. **Coordinator first** if you want chronicle from the very first
   chat. (If it comes up after, chats during the gap have no Task
   row — see [`failure-modes.md`](failure-modes.md).)
2. **Memory + AI + tool + channel** in any order; they don't
   depend on each other at startup.
3. **Bridge last.** On startup it dials every peer in `peers.toml`
   and pulls each one's manifest. Bringing it up before the
   responder peers exist means each `node.manifest` call fails and
   the bridge logs `discovery: alias=X: error: ...`. The 60s
   refresh loop will catch up, but startup logs are noisier.

For systemd users: add `After=relix-coordinator.service` to
`relix-bridge.service`. The bridge is the last hop in the chain.

## Verifying multi-node health

Three checks per peer, top-down from the bridge:

```bash
# 1. Does the bridge think the peer is alive?
relix-cli topology health --bridge http://bridge-host:19791
# Look for: peers=N (fresh=N stale=0 expired=0)

# 2. Per-peer detail (alias, last refresh, capability count):
relix-cli topology show --bridge http://bridge-host:19791

# 3. End-to-end roundtrip via the peer's capability:
curl -X POST http://bridge-host:19791/chat \
    -d '{"session_id":"smoke","message":"hello"}'
```

Common "I configured it wrong" signals:

- `freshness=expired` on first refresh → peer is unreachable. Try
  `relix-cli ping --peer <addr>` from the bridge host. If that
  fails, the bridge can't dial the address.
- `coordinator_configured=false` while a `[coordinator]` block
  exists in bridge config → bridge couldn't dial the Coordinator
  on startup. Check the address and peer-id suffix.
- `peers=0` → no `[peers.*]` blocks were valid in `peers.toml`.

## What's NOT yet in-tree

Honest list of follow-ups for richer multi-node operations:

- **No shipped docker-compose recipe.** The scripts only do
  single-host bringup. A `deploy/docker/` directory with a
  `docker-compose.yml` covering T3 in containers is a clean
  follow-up.
- **No mesh-bringup helper for >1 host.** `relix-cli mesh-up
  --hosts host-A,host-B,...` would dial out via SSH and replicate
  the single-host script's per-node bringup. Today operators do
  this with their own configuration management (Ansible /
  Terraform / hand-rolled scripts).
- **No live capability-score routing.** Multiple AI peers
  advertise `ai.chat`; the bridge picks first-match. See "T4" above.
- **No automatic identity rotation.** Identity bundles are minted
  once; rotation is a manual mint-and-redeploy cycle. The audit
  log signs every interaction so revocation is observable, but the
  runtime doesn't refuse old bundles automatically.

## See also

- [`deployment.md`](deployment.md) — three deployment modes +
  topology diagrams + Open WebUI / Telegram integration.
- [`failure-modes.md`](failure-modes.md) — per-component recovery
  steps when one piece is unreachable in a multi-node setup.
- [`getting-started.md`](getting-started.md) — single-host
  quickstart this doc is the multi-host follow-up to.
- [`bridge-invariants.md`](bridge-invariants.md) — what the bridge
  may/must-not do (constrains the topology, e.g. no multi-bridge
  load balancing in Phase 1).
- [`channel-node-architecture.md`](channel-node-architecture.md) —
  T5 topology details (channel-as-peer model).
- [`scripts/relix-mesh-up.sh`](../scripts/relix-mesh-up.sh) — the
  single-host script every per-host config below derives from.
