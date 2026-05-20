# Phase 1 Status

Cumulative status of Phase-1 foundations across all sub-tracks.
Read this when you want a single page that answers "is X done?"
without scrolling through commits.

Last updated: 2026-05-20.
Workspace tests: **395 passing**, `cargo clippy --workspace
--all-targets -- -D warnings` clean, `cargo fmt --all` clean.

## What's complete

### Runtime + capabilities

- libp2p `/relix/rpc/1` transport over TCP + Noise XK + Yamux.
- Ed25519 identity bundles + admission pipeline
  (identity → policy → handler → audit) on every responder.
- Allowlist policy engine, default-deny per method.
- Per-node hash-chained audit log + per-flow event log + a
  `relix-flow-inspect` operator binary that reads both.
- NodeManifest discovery + `capability:<method>` routing.
- A.4 reconnect-on-drop + 60s manifest refresh.
- M11 pooled `MeshClient` (bridge ↔ peers).

### Nodes

- **memory** — SQLite + FTS5; `memory.write_turn`,
  `memory.recent_for_session`, `memory.search`.
- **ai** — `ai.chat` (streaming); mock / OpenAI / OpenRouter /
  xAI / local / Anthropic providers.
- **tool** — `tool.web_fetch` (SSRF + DNS pin + per-hop
  redirect re-check), `tool.web_extract` (HTML), `tool.pdf`
  (lopdf), `tool.read_file` / `write_file` / `search_files`
  / `patch` (jailed FS).
- **coordinator** — `task.create` / `update` / `event` / `get`
  / `list` / `count` / `list_cursor` / `recover` / `attempts`
  / `retry` / `events` / `export` / `compact_events`
  (dry-run). SQLite-backed durable Task ledger.
- **web-bridge** — HTTP / SSE / OpenAI shim translation peer.

### Task runtime (C1 + C2)

- 8-state lifecycle (`pending` / `running` / `retrying` /
  `interrupted` / `awaiting_input` / `completed` / `failed`
  / `cancelled`) — runtime convention, free-string at the DB
  layer.
- Per-attempt lineage rows in `task_attempts` with
  `attempt_id`, `started_at`, `finished_at`, `trace_id`,
  outcome columns. Bridge drives `running` transitions
  through the proper open/close path.
- Recovery scan (startup + on-demand via `task.recover`)
  flips overdue `running` rows to `interrupted` and closes
  the open attempt with `failure_class=timeout`.
- Operator-initiated retry via `task.retry` with
  `--force` safety guard against `policy_denied` /
  `invalid_args` / `permanent` classes.
- `FailureClass` taxonomy with bridge auto-classification
  from `relix_core::types::error_kinds::*`.
- Trace_id propagation from bridge request → Coordinator
  attempt row → FlowRunner per-flow event log.

### Scale-grade event system (S1-S6)

- **S1 Cursor pagination** — `task.list_cursor` capability +
  `/v1/tasks/cursor` bridge endpoint. Stable under
  concurrent inserts/updates (no duplicate / skipped rows).
- **S2 Typed event envelopes** — `task_events` schema
  extended with `schema_version`, `attempt_id`, `trace_id`,
  `payload_json`. Runtime emitters (attempt_started /
  attempt_finished / task.interrupted / retry_requested /
  retry_exhausted) write structured JSON; legacy `payload`
  string preserved. Bridge surfaces typed fields via
  `serde_json::Value`.
- **S3 Reference docs** — `event-contract.md`, `task-api.md`,
  `runtime-observability.md`.
- **S4 Experimental SSE** — `/v1/tasks/:id/events/stream`
  bridge-polled long-poll wrapper. Bridge owns zero
  per-stream state.
- **S5 Retention design + Step 1 + Step 2** —
  `chronicle-retention.md` contract: 6 architectural
  constraints, 3 approach sketches, 5-step implementation
  order, operator export contract. Step 1 (`task.export`
  capability + `/v1/tasks/:id/export` with
  `Content-Disposition: attachment`) and Step 2
  (`task.compact_events` dry-run candidate counter +
  `/v1/tasks/compact_events?max_age_secs=N` +
  `relix-cli task compact`) shipped. No destructive code
  yet — Step 3 onward stays gated.
- **S6 Scale tests** — 10k-task cursor walk + 10k-event
  incremental walk smoke tests.

### Bridge `/v1` HTTP surface

- `GET /v1/models` + `POST /v1/chat/completions` (OpenAI shim).
- `POST /chat` + `POST /chat/stream` + `POST /chat_with_tool`
  (native chat endpoints).
- `GET /v1/tasks` (offset) + `GET /v1/tasks/cursor` (cursor)
  + `GET /v1/tasks/count`.
- `GET /v1/tasks/:id` + `/summary` + `/attempts` + `/events`
  (with `?type=` / `?order=`) + `/events/stream` (SSE) +
  `/lineage` (one-call combo) + `/export` (archival
  artifact, `Content-Disposition: attachment`).
- `GET /v1/tasks/compact_events?max_age_secs=N` —
  chronicle-retention dry-run candidate counter.
- `POST /v1/tasks/recover` (operator action).
- `GET /v1/capabilities` + `/:method` (capability discovery).
- `GET /dashboard` — single-page HTML operator dashboard.
  Live SSE chronology updates with a fall-back to polling;
  per-task export button uses the `/export` endpoint.

### CLI surface

- `relix-cli identity init-org` + `mint` + `ping` + `flow-run`.
- `relix-cli task create` / `update` / `event` / `get`
  (`--pretty --tail N`) / `list` (`--offset --status`) /
  `count` / `attempts` / `recover` / `retry` (`--force`) /
  `watch` (live tail) / `compact` (`--max-age-secs N`,
  dry-run candidate counter) / `export`
  (`--task-id ID --out -|FILE`, archival JSON).
- `relix-cli capability ls` (with `--category --tag` filters)
  + `get` + `validate` (manifest linter, 6 rules).

### Channel infrastructure

- `crates/relix-telegram` scaffold: config + derived identity
  + message types + session-store mapping + `BotApi` trait +
  `MockBotApi`. `SessionStorage` trait with both in-memory and
  `SqliteSessionStore` (restart-safe; idempotent schema
  migration; UNIQUE(chat_id, message_id) + ON CONFLICT
  REPLACE). Live HTTPS client wiring lands once a `reqwest`-
  backed `BotApi` impl is added alongside the existing
  `MockBotApi`. Operators configure the Bot API token via the
  dashboard's Telegram settings page — see
  [`dashboard-redesign.md`](dashboard-redesign.md).

### Capability metadata (T4)

- `CapabilityDescriptor` extended with `description`,
  `categories`, `environment_requirements` (P1).
- `/v1/capabilities` bridge endpoint (P2).
- `relix-cli capability` subcommand (P3).
- Every shipped capability annotated.

### Bridge contract enforcement

- `bridge-invariants.md` codifies the seven hard MUST-NOTs.
- 3 mechanical canary tests in
  `crates/relix-web-bridge/tests/invariants.rs` (no rusqlite
  dependency, no `PolicyEngine` instantiation, no `EventLog`
  emit).

### Documentation

22 reference docs under `docs/`. Highlights:

- **Lifecycle:** runtime-lifecycle, attempt-lineage,
  interruption-semantics, retry-model, task-recovery.
- **API:** task-runtime, task-api, event-contract,
  event-vocabulary, capability-discovery, runtime-observability.
- **Operator:** operator-guide, getting-started, deployment,
  audit-trails.
- **Security:** security, tool-node-security,
  channel-node-architecture, bridge-invariants.
- **Design:** chronicle-retention, plugin-foundations,
  replay-model.

## What's deliberately deferred

These are explicit non-goals for Phase 1 — landing them is a
later-gate decision, not a current omission.

- **Resumable VM.** The SOL VM is synchronous; "checkpointed
  re-run" is the alpha guarantee. Full durable yield is Gate 2.
- **Autonomous retry / scheduler.** `task.retry` is
  operator-initiated; there is no auto-retry daemon, no
  task-leasing system, no executor election.
- **Coordinator failover.** One coordinator per deployment;
  bridge fail-soft skips persistence if it's down.
- **Mesh-wide rate limiting.** Per-host (proxy / cgroup /
  ulimit) is the answer until a real per-tenant model lands.
- **Chronicle destructive deletion.** Design exists
  ([`chronicle-retention.md`](chronicle-retention.md));
  implementation gated on operator-export landing first.
- **Multi-bridge load balancing.** One bridge per deployment;
  consistent-hash routing in front is the right answer at
  scale.
- **Plugin loader.** Static linkage today; loading-model
  options sketched in [`plugin-foundations.md`](plugin-foundations.md);
  recommended path is out-of-process capability nodes (the
  pattern memory / ai / tool / coordinator already use).
- **Cross-trust-root audit correlation.** Audit logs are
  signed per-org; cross-org operators exchange pubkeys.
- **Live Telegram channel.** Scaffold complete; live HTTPS
  client awaits a Bot API token.

## Active blockers

None. If something blocks future work, the convention is
to ask the user directly rather than archive a writeup —
the `docs/internal/nightly-blockers/` directory has been
retired.

## Architecture invariants (verified preserved)

These have been mechanically + by-review verified across every
commit in Phase 1:

1. SOL owns orchestration semantics — no runtime concept
   bypasses the VM.
2. Coordinator owns durable metadata only — no scheduling,
   no leasing, no executor logic.
3. Bridge is translation/presentation only — mechanical
   canaries enforce no rusqlite / no PolicyEngine / no
   EventLog. The dashboard endpoint is static HTML;
   `/v1/tasks/cursor` round-trips an opaque cursor; SSE is
   a pure poll-wrapper.
4. No hidden autonomous loops — every emit is either request-
   driven or run-once-at-startup (recovery scan).
5. Capability-first — every cross-peer interaction goes
   through the admission pipeline.
6. Every capability preserves policy + identity + audit +
   bounded execution.

## Phase 1 → Phase 2 boundary (the honest line)

Phase 1 deliverable: **a coherent task-native distributed
runtime that an operator can deploy, observe, and operate at
small-to-medium scale**, with explicit honest non-goals around
auto-recovery, multi-instance HA, and resumable execution.

What unlocks Phase 2:

- Resumable VM (durable yield model). Unlocks bounded
  auto-retry and `awaiting_input` resume.
- Coordinator HA. Unlocks multi-host deployments without
  manual failover.
- Per-tenant resource caps. Unlocks shared-tenancy
  deployments.

None of these are required for the current Phase 1 contract
to hold. They're capabilities the Phase 2 spec will pin down.

## See also

The reference docs above. For commit-level history,
`git log --oneline` from `cd417e2` (start of C1) onwards.
