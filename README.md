# Relix

Relix is a peer-to-peer runtime for AI orchestration. Memory, AI providers,
tools, and the local HTTP bridge are all separate peer **nodes** that
communicate over libp2p. Orchestration is expressed in **SOL**, a small
language whose `remote_call` opcode is the only way to invoke a capability
on another peer. There is no central gateway, no central credential store,
and no central tool registry.

This repository is the broad alpha. The architecture is the production
seed; the alpha scope is deliberately narrow and the simplifications are
listed honestly in [`specs/alpha-simplifications.md`](specs/alpha-simplifications.md)
and [`docs/current-limitations.md`](docs/current-limitations.md).

---

## What works today

| Component | Status |
|---|---|
| Controller daemon (`relix-controller`) | works |
| libp2p transport (`/relix/rpc/1` over TCP + Noise XK + Yamux) | works |
| Ed25519 identity bundles + per-call admission | works |
| Allowlist policy engine, default-deny per method | works |
| Per-node hash-chained audit log | works |
| Per-flow event log + `relix-flow-inspect` | works |
| Memory node (`memory.write_turn` / `recent_for_session` / `search`, SQLite + FTS5) | works |
| AI node (`ai.chat`) — `mock`, `openai`, `openrouter`, `xai`, `local`, `anthropic` providers | works |
| Tool node (`tool.web_fetch`) — SSRF-guarded, DNS-pinned, redirects re-screened | works |
| Local HTTP bridge (`relix-web-bridge`) — `/chat`, `/chat/stream`, `/chat_with_tool` | works |
| OpenAI-compatible shim — `/v1/models`, `/v1/chat/completions` (incl. SSE) | works |
| NodeManifest discovery + `capability:<method>` routing | works |
| Connection pool reuse on bridge ↔ peers and tool ↔ origins | works |
| MeshClient peer reconnect + 60s manifest refresh | works |
| Coordinator node (`task.create` / `update` / `event` / `get` / `list` / `count` / `list_cursor` / `recover` / `attempts` / `retry` / `events` / `export` / `compact_events`) with SQLite ledger | works (per-attempt lineage; operator-driven retry; recovery scan for stale tasks; cursor pagination stable under concurrent writes; typed event envelopes. Checkpointed re-run, not resumable replay — see [`docs/replay-model.md`](docs/replay-model.md)) |
| Bridge `/v1/tasks` HTTP surface (list / cursor / count / get / summary / attempts / events / events/stream SSE / lineage / export / compact_events / recover) | works |
| Operator dashboard `/dashboard` — sidebar + topbar shell, six routes (Overview / Tasks / Topology / AI Providers / Telegram / Bridge Config), live SSE chronology, per-task export, retention dry-run modal, AI provider key setup, Telegram token setup | works |
| Dashboard-driven config (`/v1/config/*`) — AI provider keys + Telegram bot token via the dashboard's settings pages. Local secrets file (mode 0600, gitignored); never echoed back over HTTP. | works |
| `relix-cli task` CLI (create / update / event / get / list / count / attempts / recover / retry / watch / compact / export) with `--pretty` chronology | works |
| `relix-cli capability` CLI (ls / get / validate, 7 manifest-lint rules) | works |
| Chronicle retention (operator export + dry-run candidate counter; destructive deletion gated by design) | works (Steps 1, 2, 5 of [`docs/chronicle-retention.md`](docs/chronicle-retention.md)) |
| Telegram channel scaffold (config + identity + session-store + `BotApi` trait + mock + `SqliteSessionStore`) | works (live HTTPS client awaits a Bot API token) |
| SOL VM with `remote_call`, hand-written `.sol` flows | works |
| Windows-safe PowerShell mesh bringup | works |

Known limitations are listed in
[`docs/current-limitations.md`](docs/current-limitations.md) — read that
before deploying anywhere that isn't a local developer machine.

---

## 60-second quickstart

Prereqs: Rust 1.95+, Git. Windows or POSIX.

```bash
git clone https://github.com/itsramananshul/Relix.git
cd Relix
cargo build --workspace
```

Boot the local mesh (memory + AI + tool + bridge):

```powershell
# Windows
.\scripts\relix-mesh-up.ps1
```

```bash
# macOS / Linux / Git Bash
./scripts/relix-mesh-up.sh
```

The bridge listens on `http://127.0.0.1:19791`. Hit it:

```bash
curl http://127.0.0.1:19791/health
curl http://127.0.0.1:19791/v1/models
curl -X POST http://127.0.0.1:19791/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"relix-mock","messages":[{"role":"user","content":"hello"}]}'
```

Ctrl-C the script to shut down. The script tracks and stops only the
processes it spawned.

Full walkthrough — including Open WebUI hookup and `tool.web_fetch` —
is in [`docs/getting-started.md`](docs/getting-started.md).

---

## Mental model

```
                  ┌──────────────────────────────────────┐
                  │              Open WebUI               │
                  │  (or any OpenAI-compatible client)    │
                  └──────────────────┬───────────────────┘
                       HTTP /v1/chat/completions
                                     │
                                     ▼
                  ┌──────────────────────────────────────┐
                  │           relix-web-bridge            │
                  │  HTTP → SOL flow template → libp2p    │
                  │   (peer #1, holds no provider key)    │
                  └──────────────────┬───────────────────┘
                                     │
       libp2p /relix/rpc/1  ┌────────┼────────┐
                            │        │        │
                            ▼        ▼        ▼
                       ┌────────┐┌──────┐┌────────┐
                       │ memory ││  ai  ││  tool  │
                       │ peer   ││ peer ││  peer  │
                       └────────┘└──────┘└────────┘
                        SQLite +    provider     reqwest +
                        FTS5        of choice    SSRF guard
                                    (key lives    + DNS pin
                                     here only)   + redirect
                                                  re-check
```

Every node is a real OS process. Every call between them runs the full
admission pipeline (identity → policy → handler → audit) on the
responder. The web bridge has its own identity bundle and is treated
like any other peer.

---

## Repository layout

| Path | Purpose |
|---|---|
| `crates/relix-core` | Wire types, identity, policy, audit, event log, capability descriptors. |
| `crates/relix-runtime` | libp2p transport, SOL VM with `remote_call`, dispatch bridge, manifest exchange, node implementations. |
| `crates/relix-controller` | The `relix-controller` daemon binary. |
| `crates/relix-web-bridge` | The local HTTP bridge binary (OpenAI shim + native endpoints). |
| `crates/relix-cli` | Operator CLI: `identity init-org`, `identity mint`, `ping`, `flow-run`, `task {create,update,event,get,list,count,attempts,recover,retry,watch,compact,export}`, `capability {ls,get,validate}`. |
| `crates/relix-telegram` | Task-native Telegram channel scaffold (config, identity, session store, BotApi trait, mock). |
| `crates/relix-flow-inspect` | Reads audit + flow event logs. |
| `flows/` | Hand-written `.sol` flows. |
| `configs/` | Example node config TOMLs. |
| `scripts/relix-mesh-up.{ps1,sh}` | Bring up the local mesh. |
| `specs/` | Substrate specifications (`RELIX-1`..`RELIX-8`) and `alpha-simplifications.md`. |
| `docs/` | Documentation (start with [`docs/getting-started.md`](docs/getting-started.md)). |

---

## Documentation

Start here, in this order:

- [`docs/phase-1-status.md`](docs/phase-1-status.md) — single page covering "what's done, what's deferred" across Phase 1. Read this if you want to know the system's scope without reading every reference doc.
- [`docs/getting-started.md`](docs/getting-started.md) — install, boot, first chat, Open WebUI hookup, first `tool.web_fetch`.
- [`docs/architecture.md`](docs/architecture.md) — peer model, request flows, the admission pipeline, why the bridge is not an orchestrator.
- [`docs/operator-guide.md`](docs/operator-guide.md) — running the mesh, logs, troubleshooting, common failure modes.
- [`docs/failure-modes.md`](docs/failure-modes.md) — single-page reference for "what happens when X is down": detection signals, bridge behavior, recovery steps for every component.
- [`docs/restart-safety.md`](docs/restart-safety.md) — per-component "what persists / what's recomputed / what's lost" across a restart. Read before designing recovery procedures or backup policy.
- [`docs/dashboard-redesign.md`](docs/dashboard-redesign.md) — design contract for the operator console redesign, including the secret-handling model used by the AI provider + Telegram settings pages.
- [`docs/deployment.md`](docs/deployment.md) — local / multi-node / production-readiness modes, mandatory hardening before public exposure, topology diagram, Open WebUI + Telegram integration.
- [`docs/multi-node-bringup.md`](docs/multi-node-bringup.md) — five concrete topologies (single-host → multi-host → channel-augmented), per-node config deltas, identity distribution recipe, boot order, multi-node health checks.
- [`docs/flows-and-sol.md`](docs/flows-and-sol.md) — what SOL is and isn't, how `remote_call` works, how to write a new flow.
- [`docs/security.md`](docs/security.md) — identities, policy, audit, what the alpha guarantees and what it doesn't.
- [`docs/tool-node.md`](docs/tool-node.md) — the external-action peer, the SSRF model, the DNS pin, the redirect re-check, the secure client pool.
- [`docs/coordinator.md`](docs/coordinator.md) — the durable Task ledger peer.
- [`docs/task-runtime.md`](docs/task-runtime.md) — Task schema + the `task.*` capabilities, wire-format exact.
- [`docs/replay-model.md`](docs/replay-model.md) — exactly what "checkpointed re-run" means and what it does *not* mean. Read this before assuming Tasks resume on retry.
- [`docs/current-limitations.md`](docs/current-limitations.md) — read before deploying.

Task lifecycle reference (C1 + C2):

- [`docs/runtime-lifecycle.md`](docs/runtime-lifecycle.md) — canonical status transitions across the eight Task states.
- [`docs/attempt-lineage.md`](docs/attempt-lineage.md) — per-attempt rows, when they open/close, attempt-aware event vocabulary, flow lineage mapping.
- [`docs/event-vocabulary.md`](docs/event-vocabulary.md) — stable contract for runtime-emitted event names + payload conventions. Read before adding a new emitter.
- [`docs/event-contract.md`](docs/event-contract.md) — typed event envelope schemas (S2 wire contract per `event_type`).
- [`docs/task-api.md`](docs/task-api.md) — bridge HTTP surface for tasks + events + capabilities. Single reference for dashboard authors.
- [`docs/runtime-observability.md`](docs/runtime-observability.md) — operator mental model + on-call workflow + observability primitives at a glance.
- [`docs/audit-trails.md`](docs/audit-trails.md) — operator reconstruction across the three audit surfaces (per-node audit log, per-flow event log, Coordinator chronicle), with `relix-flow-inspect` recipes.
- [`docs/chronicle-retention.md`](docs/chronicle-retention.md) — design contract for chronicle retention + compaction + operator export. Steps 1 (export), 2 (dry-run candidate counter), 5 (CLI tooling) shipped; no destructive deletion implemented yet.
- [`docs/bridge-invariants.md`](docs/bridge-invariants.md) — what the bridge MAY/MUST NOT do. Mechanical canary tests enforce the most-likely regressions; the rest is review-enforced.
- [`docs/interruption-semantics.md`](docs/interruption-semantics.md) — what the Coordinator's recovery scan does and deliberately doesn't.
- [`docs/retry-model.md`](docs/retry-model.md) — what `retry_policy` / `max_retries` / `task.retry` actually do today.
- [`docs/task-recovery.md`](docs/task-recovery.md) — operator playbook with CLI invocations for diagnosing and acting on interrupted / failed tasks.

Reference docs (deeper / narrower):

- [`docs/provider-configuration.md`](docs/provider-configuration.md) — every AI provider, its TOML, its env var.
- [`docs/streaming-and-openai-shim.md`](docs/streaming-and-openai-shim.md) — the bridge's SSE shape.
- [`docs/tool-node-security.md`](docs/tool-node-security.md) — the full SSRF model + DNS pin + redirect re-check + pool security invariants.
- [`docs/sol-runtime-analysis.md`](docs/sol-runtime-analysis.md) — SOL VM internals.
- [`docs/channel-node-architecture.md`](docs/channel-node-architecture.md) — design for task-native messaging channels (Telegram first; implementation pending credentials).
- [`docs/capability-discovery.md`](docs/capability-discovery.md) — how the mesh advertises capabilities and the planner-foundations contract any future planner must satisfy.
- [`docs/plugin-foundations.md`](docs/plugin-foundations.md) — packaging + loading model options and the architectural constraints any plugin system must respect.
- [`specs/`](specs/) — wire-format and architecture specifications.

---

## License

Apache License 2.0. See [`LICENSE`](LICENSE).

## Reporting Security Issues

See [`SECURITY.md`](SECURITY.md).

## Minimum Supported Rust Version

Rust 1.95 (pinned in `rust-toolchain.toml`). Earlier versions are not tested.
