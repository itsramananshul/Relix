# Deployment

This document is the operator's guide to running Relix in
something other than a single-developer-machine sandbox. Three
deployment modes are described: **local** (one machine, the
default), **multi-node** (peers on separate hosts), and
**production-readiness** (what to harden before exposing the
bridge beyond loopback).

Operator-prereq reading order:
1. [`getting-started.md`](getting-started.md) — first boot.
2. [`operator-guide.md`](operator-guide.md) — logs + common
   failures.
3. This doc — when you're past the laptop demo.
4. [`security.md`](security.md) — what the admission pipeline
   enforces.
5. [`current-limitations.md`](current-limitations.md) — what
   the alpha deliberately doesn't do.

## Three modes

### Local (default)

One machine, one bringup script. Everything talks over loopback;
`relix-mesh-up.ps1` / `.sh` spawns memory + AI + tool + bridge
(+ optionally coordinator, telegram) and parks until Ctrl-C.

This is the right mode for: development, demos, exploring the
runtime, writing flows.

**Identity:** auto-generated keys live under `dev-keys/<run>-*`.
The bringup script is idempotent — re-running it does NOT
overwrite existing keys.

**Networking:** all peers bind to `127.0.0.1`. No firewall
configuration needed.

**Storage:** SQLite databases under `dev-data/<run>/`. Audit +
flow logs under `dev-data/<run>-<node>/audit.log` and
`dev-data/flow-runner/flows/`.

### Multi-node

Peers run on separate hosts. The mesh discovers each other via
the `peers.toml` file referenced by each controller's config
(`[transport] peers_path`).

To stand up:

1. **Mint a shared org root.** `relix-cli identity init-org
   --root-key <PATH> --org <LABEL>` on an isolated machine;
   copy the resulting `<run>-org-root.pub` to every other host.
2. **Per host, generate an identity bundle** signed by the
   shared org root: `relix-cli identity mint --root-key
   <PATH> --name memory-1 --groups core --out
   dev-keys/memory-1.aic`.
3. **Edit each controller TOML** with that host's listen
   address (replace `127.0.0.1` with the bind address) and
   point `[trust] org_root_key_path` at the shared pub key.
4. **Build a shared `peers.toml`** listing every peer's
   `<alias> = "/ip4/<host>/tcp/<port>"`. Copy to each host.
5. **Open the listen ports** in the OS firewall — libp2p uses
   plain TCP. TLS isn't necessary (Noise XK encrypts the
   stream); TCP is enough.
6. **Start each controller** by hand (no shared bringup
   script across hosts; that's local-only).

**What this changes vs local:**
- DNS: peer hostnames must resolve from each peer.
- Time: clocks should be loosely synced (NTP is enough; we
  don't depend on precise ordering across peers).
- Audit logs are per-host; cross-host investigation needs
  `relix-flow-inspect --trace <id>` against each host's audit
  log file.

### Production-readiness

The alpha is a substrate freeze, not a hardened production
deployment. Before exposing the bridge beyond loopback, the
checklist below is mandatory. A standalone, operator-runnable
version lives at
[`production-checklist.md`](production-checklist.md) and
includes a smoke-test command sequence + an incident-response
pull-point table you can drop into a runbook.

#### Mandatory hardening

1. **Reverse proxy in front of the bridge.** The bridge
   enforces bearer-token authentication on all non-public
   routes (token stored at `~/.relix/bridge-token`; CSRF
   guard compares Origin against the listen address; rate
   limiting per principal). However, the bearer token
   provides loopback-scoped security only — it is not a
   substitute for a reverse proxy with TLS + external auth
   when the bridge must be reachable beyond loopback. Put
   nginx / Caddy / a cloud load balancer in front with TLS
   + auth (mTLS, OAuth, basic auth — whatever your
   environment requires) for any internet-facing deployment.
2. **Bind the bridge to loopback only.** `[bridge] listen_addr
   = "127.0.0.1:19791"`. The reverse proxy reaches it over
   the loopback interface.
3. **Tighten the policy file.** Default policy admits every
   capability to `chat-users`. Production should split: a
   `bridge` group for the HTTP layer, a `telegram` group for
   the channel, an `operator` group for the operator surfaces
   (`task.recover`, `task.retry`). Each group gets the
   minimum capability set it needs.
4. **Rotate identity bundles.** The bringup script mints
   24-hour bundles. Production deployments need a renewal
   loop (re-mint via `relix-cli identity mint` before
   expiry). A renewal cron / systemd-timer / k8s job is the
   right surface; the runtime side just consumes whatever's
   on disk.
5. **Secure the org root secret.** It's the trust anchor.
   Treat like a CA root: stored in an HSM / secret manager,
   absent from filesystem under normal operation. The
   bringup-script-default of leaving it in `dev-keys/` is
   development-only.
6. **Log rotation.** `audit.log` and per-flow logs are append-
   only. Use standard log-rotation tooling (logrotate / Windows
   Event Log policies). The runtime doesn't truncate either.
7. **Chronicle retention.** The Coordinator's `task_events`
   table grows unbounded. The retention design is in
   [`chronicle-retention.md`](chronicle-retention.md); the
   implementation needs operator export support first. Until
   then, plan disk capacity accordingly.

#### Network egress filtering

If the tool node has `tool.web_fetch` enabled, the SSRF guards
([`tool-node-security.md`](tool-node-security.md)) deny private
addresses at the application layer. **In addition**, configure
host-level egress filtering against RFC 1918 from the tool
node's UID. The two layers together make a single bug less
catastrophic:

```bash
# Linux example. UID is whatever you ran the tool controller as.
iptables -A OUTPUT -m owner --uid-owner relix-tool \
    -d 10.0.0.0/8 -j REJECT
iptables -A OUTPUT -m owner --uid-owner relix-tool \
    -d 172.16.0.0/12 -j REJECT
iptables -A OUTPUT -m owner --uid-owner relix-tool \
    -d 192.168.0.0/16 -j REJECT
iptables -A OUTPUT -m owner --uid-owner relix-tool \
    -d 169.254.0.0/16 -j REJECT
```

#### Resource limits

The alpha doesn't impose per-tenant resource caps. The
Coordinator's `max_runtime_secs` is a soft deadline (the
recovery scan flips overdue rows; it does not kill the
executor). Plan for:

- The AI node's provider API rate limits (paid externally;
  the AI node doesn't cap).
- The bridge's HTTP connection limit (default axum behaviour;
  no per-client cap).
- Disk usage from logs + chronicle.

A `ulimit` / cgroup wrapper per node is the standard answer
until per-tenant caps land at a future gate.

## Required environment variables for production

Before running the coordinator in production with approval tokens or
credential vault, two environment variables **must** be set. Both are
fail-closed: absent or invalid values cause every affected call to be
denied.

| Variable | Required when | Format | Effect if absent |
|---|---|---|---|
| `RELIX_APPROVAL_SIGNING_KEY` | Coordinator issues approval tokens | 64 hex chars (32 raw bytes, Ed25519 seed) | Every token-bearing call fails: `approval_token_missing_key` |
| `RELIX_CREDENTIAL_KEY` | `[credentials] enabled = true` | Arbitrary secret string (default `v1` vault key) | Vault refuses to open: `NoActiveKeyVersion` |

Generate a signing key seed (bash):

```bash
openssl rand -hex 32
# -> 64-char hex string; export as RELIX_APPROVAL_SIGNING_KEY
```

The bridge bearer token (`~/.relix/bridge-token`) is auto-generated
at first boot — no manual step needed. Expose it to HTTP clients via
your reverse-proxy auth layer rather than hard-coding it anywhere.

## Topology

```
                  ┌─────────────────────────┐
                  │  Operators / Open WebUI │
                  └───────────┬─────────────┘
                              │
                       TLS + auth (reverse proxy you provide)
                              │
                  ┌───────────▼─────────────┐
                  │   nginx / Caddy / ...   │
                  └───────────┬─────────────┘
                              │
                     HTTP over loopback
                              │
                  ┌───────────▼─────────────┐
                  │  relix-web-bridge       │
                  │  127.0.0.1:19791        │
                  └───────────┬─────────────┘
                              │
              libp2p /relix/rpc/1 (Noise XK + Yamux)
                              │
       ┌──────────┬───────────┼──────────┬──────────┬──────────┐
       │          │           │          │          │          │
       ▼          ▼           ▼          ▼          ▼          ▼
   memory      ai-prov      tool      coord    telegram     ...
   (sqlite)   (provider)  (sandbox) (sqlite)  (channel)
                                                   │
                                            Telegram Bot API
                                            (HTTPS, outbound)
```

Each box is one OS process. Each process has its own identity
bundle + policy file + audit log + (optional) SQLite database.
All inter-process traffic is libp2p; only the bridge and the
external channels speak anything else.

## Open WebUI integration

The OpenAI-compatible shim ([`streaming-and-openai-shim.md`](streaming-and-openai-shim.md))
makes the bridge a drop-in replacement for an OpenAI provider.
In Open WebUI:

| Field | Value |
|---|---|
| API Base URL | `https://relix.your.domain/v1` (whatever your reverse proxy exposes) |
| API Key | any non-empty string |
| Model | `relix-mock` / `relix-openrouter` / whatever your AI node serves |

The reverse proxy is responsible for auth — Open WebUI sends a
bearer token, your proxy validates it, the bridge sees only
post-auth requests on loopback.

## Telegram channel

Architecture: [`channel-node-architecture.md`](channel-node-architecture.md).
Scaffold: [`crates/relix-telegram`](../crates/relix-telegram).
Implementation status: scaffold + `SqliteSessionStore` shipped;
the live HTTPS client wiring lands once a `reqwest`-backed
`BotApi` impl is added alongside the existing `MockBotApi`. The
operator dashboard's Telegram settings page accepts the Bot API
token without it ever touching git or operator logs.

To enable Telegram:

1. Mint a bot via `@BotFather` and copy the token.
2. Open the operator dashboard's **Telegram** page and paste
   the token. The bridge persists it to a local config file
   (not committed) and validates presence without echoing the
   token value. See [`dashboard-redesign.md`](dashboard-redesign.md)
   for the secret-handling contract.
3. Add `channel-telegram` group to your policy file with the
   capabilities the channel's flow uses (typically
   `memory.*`, `ai.chat`, the `task.*` subset
   `create/update/event`).
4. Start the channel controller alongside the others.

## Chronicle retention on long-running deployments

Every chat hits the Coordinator's `task_events` table at
least five times; long-running deployments accumulate
millions of rows. The retention design + currently-shipped
tooling is in
[`chronicle-retention.md`](chronicle-retention.md).

What ships today (Phase 1):

- **Save before deleting.** `relix-cli task export
  --task-id ID --out FILE` (HTTP: `GET /v1/tasks/:id/export`)
  writes a single-JSON archival artifact (header + every
  attempt + every chronicle event). Pipe to `gzip` for
  long-term storage.
- **Plan before any future deletion.** `relix-cli task
  compact --max-age-secs N` counts what *would* be deleted
  under a max-age policy, broken down by parent task status.
  Honours the R5 invariant (terminal-state tasks only, never
  in-flight).

What does NOT ship today: destructive deletion of
chronicle rows. The dry-run counter exists so operators
can plan an archival policy now (e.g. nightly job that
exports tasks older than 30 days before any future
compaction lands). The destructive Step 3 capability
lands behind operator confirmation per the design.

Operator recipe for a nightly archive job (POSIX):

```bash
#!/usr/bin/env bash
# Export every task completed/failed/cancelled more than
# 30 days ago. Replace the curl URL + identity with
# your bridge / CLI path.
set -euo pipefail
out_dir="/var/relix/archive/$(date +%Y-%m-%d)"
mkdir -p "$out_dir"
# 1. Dry-run to see what's eligible.
curl -fsS "http://127.0.0.1:19791/v1/tasks/compact_events?max_age_secs=2592000" \
    | tee "$out_dir/candidates.json"
# 2. Iterate over candidate tasks and export each.
#    Drive task ids from your own bookkeeping or
#    /v1/tasks/cursor with the right status filter.
```

## What's NOT supported

Honest list of out-of-scope items:

- **No multi-bridge load balancing.** One bridge per
  deployment today. The bridge keeps no shared state, so two
  bridges fronting the same mesh would race on chat session
  ids. A consistent-hash router in front is the right answer
  when this matters.
- **No coordinator failover.** One coordinator owns the
  ledger. If it goes down, the bridge fail-soft skips
  persistence and chat continues; recovering durable Task
  state on the new coordinator is operator manual today.
- **No mesh-wide rate limiting.** Per-host limits via proxy /
  cgroups / ulimit.
- **No automated key rotation.** Build your own job.
- **No mTLS between peers.** Noise XK provides peer auth via
  static keys; certificate-based PKI is not the model.

## See also

- [`getting-started.md`](getting-started.md) — first boot.
- [`operator-guide.md`](operator-guide.md) — logs, common
  failures, the CLI surface.
- [`security.md`](security.md) — admission pipeline, audit
  log, identity model.
- [`bridge-invariants.md`](bridge-invariants.md) — what the
  bridge MAY and MUST NOT do.
- [`chronicle-retention.md`](chronicle-retention.md) — disk
  growth design (no automation yet).
- [`channel-node-architecture.md`](channel-node-architecture.md)
  — Telegram + other-channel architecture.
- [`current-limitations.md`](current-limitations.md) — every
  honest "not yet" in one place.
