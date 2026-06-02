# Restart safety — per-component state survival

What survives when each Relix component is stopped and restarted.
Read this before designing recovery procedures or deciding which
on-disk paths matter for backup.

The shape of this doc mirrors [`failure-modes.md`](failure-modes.md):
one component per section, top-down by operator visibility. Each
section answers three questions:

1. **What persists?** — disk state that survives `kill -TERM` + restart.
2. **What's recomputed?** — in-memory state rebuilt on startup.
3. **What's lost?** — in-flight work or per-process state that does
   NOT survive.

## Quick reference

| Component | Persistent | Recomputed | Lost |
|---|---|---|---|
| Coordinator | SQLite ledger (`tasks`, `task_events`, `task_attempts`) | — | In-flight ⇒ `interrupted` via recovery scan |
| Memory peer | SQLite + FTS5 (`memory.db`) | — | Per-session vector cache (not yet shipped) |
| AI peer | Identity bundle, provider config | Provider HTTP client pool | In-flight `ai.chat` calls (caller sees transport error) |
| Tool peer | Identity bundle, policy file | Pinned-address resolver state | In-flight HTTP fetches |
| Bridge | Identity bundle, peers.toml | ManifestCache (re-discovered), MeshClient | In-flight chats, SSE streams, per-stream cursors |
| Telegram channel | SqliteSessionStore (`chat_id × message_id → task_id`) | Bot API long-poll offset (next batch sees missed updates if within Telegram's 24h retention) | Updates older than Telegram's retention if downtime exceeded it |

## Coordinator

### What persists

The Coordinator's authoritative state is one SQLite database (`coordinator.db`
under `[coordinator] db_path`). Three tables:

- **`tasks`** — one row per Task. Every column persists, including
  the lifecycle ones (`retry_policy`, `max_retries`, `started_at`,
  `last_failure_class`, etc.) added in C1.
- **`task_events`** — append-only chronicle. Every row persists.
  Operator-defined v0 events and runtime-emitted v1 envelopes
  alike.
- **`task_attempts`** — per-attempt lineage rows from C2a. Every
  row persists.

The schema is **idempotent-migration-safe**: each ALTER COLUMN
attempt is wrapped in a try-ignore for the "already exists" case,
so re-running against a migrated DB is a no-op. See the schema
block in `crates/relix-runtime/src/nodes/coordinator/mod.rs`.

### What's recomputed on startup

Nothing meaningful. The Coordinator opens its SQLite handle and
serves immediately.

### What's lost

Nothing in the durable sense — but the lifecycle of in-flight
tasks shifts:

- Any task in status `running` at the moment of restart will be
  promoted to `interrupted` by the recovery scan that runs once
  at startup (when `[coordinator] recovery_scan = true`, the
  default). The promotion appends a `task.interrupted` event with
  `failure_class=timeout` and closes the open attempt row.
- Tasks in non-running terminal states (`completed`, `failed`,
  `cancelled`) are untouched.

The recovery scan is **idempotent and safe to invoke on demand**
via `task.recover` (operator capability + `relix-cli task recover`
+ `POST /v1/tasks/recover`). Calling it multiple times in a row
makes no further changes.

Backup recipe: copy `coordinator.db` while the Coordinator is
stopped (or use SQLite's `.backup` command online). The on-disk
format is plain SQLite — readable with any SQLite client for
forensics.

## Memory peer

### What persists

One SQLite database (`memory.db` under `[memory] db_path`) with
the `turns` FTS5 virtual table and the underlying `turns_raw`
table. Both persist verbatim across restart.

### What's recomputed on startup

Nothing significant — SQLite reopens and FTS5 indexes are
on-disk, not in-memory.

### What's lost

Nothing in the shipped scope. Future per-session vector caches
or LLM-derived embeddings would be in-memory and ephemeral; they
are not yet shipped.

In-flight memory calls from the bridge (`memory.recent_for_session`,
`memory.write_turn`, `memory.search`) fail at the call site if
the peer is restarted mid-request. The caller (typically the
SOL flow) sees a transport error and halts.

## AI peer

### What persists

- Identity bundle (`<ai>.aic`) + client key (`<ai>.key`) on disk.
- Provider config in the controller TOML (provider name, base
  URL, model alias).
- Provider API key in the environment (or wherever the operator
  configured it).

### What's recomputed on startup

The provider's HTTP client pool. Each AI peer creates a fresh
`reqwest::Client` on startup; warm connections to the upstream
provider are lost and re-established on the first request.

### What's lost

In-flight `ai.chat` calls. The caller's SOL flow halts at the
unavailable `remote_call`, and (if a Coordinator is configured)
the Task's attempt closes with `failure_class=transient`.

There is no per-session streaming state on the AI peer to lose —
SSE streams are bridge-side; the AI peer serves request-response.

## Tool peer

### What persists

- Identity bundle + client key + policy file.
- The tool node's TOML config (jailed FS roots, SSRF allowlists,
  per-host fetch policy).

### What's recomputed on startup

- The pinned-address validator's redirect cache. Per-host
  validated address sets are in-process; cold start re-validates
  the first request to each host.
- The pooled `reqwest::Client`s per host (the `(host,
  validated-addrs)`-keyed pool from commit `a9acaa9`).

### What's lost

In-flight `tool.web_fetch` / `web_extract` / `pdf` calls. The
caller sees a transport error.

Note: the jailed FS paths are entirely on-disk; restarts do not
affect the filesystem state, so `tool.write_file` / `patch` /
`read_file` calls that completed before the restart are
authoritative.

## Bridge

### What persists

- Identity bundle (`<bridge>.aic`) + client key (`<bridge>.key`).
- Bridge config TOML (peers, flow template paths, OpenAI shim
  models, etc.).
- The SOL flow template files on disk.

### What's recomputed on startup

- **ManifestCache** — empty at startup, fully populated by the
  initial discovery pass before the HTTP listener binds. The 60s
  background refresh loop spawns after discovery completes.
- **MeshClient** — fresh libp2p client; re-dials every peer in
  `peers.toml` and caches their `PeerId`s by alias.
- **`AppState.started_at`** — set to `unix_secs()` at bridge
  startup; surfaces via `/v1/health`'s `started_at` + `uptime_secs`.

The bridge has no on-disk cache. Everything beyond config files
is rebuilt on each start.

### What's lost

- **In-flight HTTP requests.** Any chat that was mid-flow is
  cancelled; the operator sees a connection reset.
- **SSE streams.** Per-task chronicle SSE streams
  (`GET /v1/tasks/:id/events/stream`, consumed by `relix task
  watch` and API clients) drop. EventSource auto-reconnect
  kicks in with the
  `?since=<lastEventId>` semantics from the prefix-extract
  cursor advancement (see `extract_event_id_prefix` in
  `crates/relix-web-bridge/src/tasks.rs`).
- **Reconnect counters** — the cross-mesh
  `MeshClient::reconnect_counters` reset to zero. Operator
  scripts that scrape `/v1/health` for these counters need to
  treat a zero as "either no flapping yet, or the bridge just
  restarted." Compare `uptime_secs` to disambiguate.
- **`task.created` / `flow.started` events that hadn't been
  written yet** at the moment of crash. Tasks visible in the
  Coordinator chronicle but not in any bridge in-process state
  are unaffected — the per-flow event log on disk (written by
  the responder peers) is authoritative for those.

## Telegram channel

### What persists

- Identity bundle + client key + policy file (same shape as any
  other peer).
- **`SqliteSessionStore` database** (one SQLite file in the
  channel's data dir). Schema:
  ```sql
  CREATE TABLE sessions (
      chat_id    INTEGER NOT NULL,
      message_id INTEGER NOT NULL,
      task_id    TEXT    NOT NULL,
      created_at INTEGER NOT NULL,
      UNIQUE(chat_id, message_id) ON CONFLICT REPLACE
  );
  ```
  The `UNIQUE ... ON CONFLICT REPLACE` clause makes idempotent
  re-deliveries safe — if the channel restarts mid-flow and the
  same Telegram update arrives again, the row is overwritten with
  the new `task_id` (which is what the new flow execution
  produced, by definition the right answer for the second
  delivery).

### What's recomputed on startup

- The Telegram Bot API client (reqwest-based, when shipped).
- The long-poll offset is fetched from the bot's last-known state
  via `getUpdates(offset=0)` on first call. Telegram's server-
  side update queue covers up to 24h of history; updates within
  that window that arrived during downtime are delivered on the
  first poll after restart.

### What's lost

- **Updates older than 24h** if the channel was down for longer
  than Telegram's retention window. These cannot be recovered;
  the user has to re-send the message.
- **In-flight outbound `send_message` calls** if the channel
  crashed between the BotApi `send_message` call and the
  Telegram-side commit. Bot API rate-limit logic is per-process,
  so a restart resets the per-second rate budget.

## What we don't yet checkpoint

Honest list of state that COULD usefully persist across restart
but does not today:

- **Bridge ManifestCache.** A persistent cache would skip the
  startup discovery pass and let the bridge bind immediately
  with stale routing while the refresh loop catches up. Today
  the bridge blocks on initial discovery (~hundreds of ms with
  4 peers on loopback). For multi-host deployments with slow
  peers this could be the difference between "bridge ready
  in 1s" and "bridge ready in 30s."
- **Bridge reconnect counters.** Resetting to zero on each
  restart obscures long-term flapping behavior. Persisting to
  a small JSON file would survive restart for trend analysis.
- **Tool peer's pinned-address cache.** Cold-start re-validates
  every host. A persistent cache (with TTL) would speed up
  first-request latency after restart.

None of these are required for correctness; they're
performance + observability upgrades. The Phase 1 model is
"restart is cheap; cold start is acceptable." That holds
because the durable state (`coordinator.db`, `memory.db`,
`SqliteSessionStore`) is on-disk and the rest is fast to
rebuild.

## See also

- [`failure-modes.md`](failure-modes.md) — per-component
  recovery steps when one piece is unreachable.
- [`coordination.md`](coordination.md) — durable Task ledger
  schema + lifecycle details.
- [`channel-node-architecture.md`](channel-node-architecture.md)
  — SqliteSessionStore design + restart-safety contract.
- [`task-recovery.md`](task-recovery.md) — operator playbook for
  the `interrupted` Tasks the recovery scan creates after a
  Coordinator restart.
- [`interruption-semantics.md`](interruption-semantics.md) —
  exactly what the recovery scan does and does not do.
- [`tool-node-security.md`](tool-node-security.md) — what the
  pinned-address cache + validator contract guarantees per
  call (which is what makes cold-start re-validation safe).
