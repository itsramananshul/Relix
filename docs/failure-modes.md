# Failure modes — operator reference

What happens, observably, when one of the pieces of a running Relix
mesh fails or becomes unreachable. Read this when on-call;
treat the column headers as a single playbook row each.

Scope: this doc describes **shipped behavior** as of Phase 1, not
aspirational design. When a row says "operator action," that's a
manual step today — nothing in the runtime does it automatically.

Architectural framing:

- **SOL owns orchestration.** No "auto-retry daemon" runs anywhere.
- **Coordinator owns durable metadata only.** It does not schedule,
  lease, plan, or execute.
- **Bridge is translation/presentation only.** It never starts
  flows on its own, never persists, never enforces policy.

So "X failed" never produces hidden, asynchronous side effects —
either the failure is visible to the caller, or the runtime
fail-softs (the request continues without the optional dependency)
and emits something the operator can grep later.

## Quick reference

| Component down | Bridge behavior | Operator-visible signal | Recovery |
|---|---|---|---|
| Coordinator | Fail-soft: chat continues, `task_id` omitted; `task.*` HTTP endpoints return 503 | Bridge log `bridge.task_recorder=None`; `/v1/tasks` returns 503; `/v1/topology` shows coordinator `expired` after ~10min | Restart Coordinator; recovery scan auto-runs once at startup |
| Memory peer | Flow halts at first `memory.*` call with `mesh transport` error | Bridge logs `remote_call(memory, ...): unavailable`; per-flow event log has `capability.invoked` then `flow.halted` | Restart memory peer; bridge auto-reconnects on next call |
| AI peer | Same as Memory: `ai.chat` errors back to operator | `503` to `/v1/chat/completions`; chronicle has `task.attempt_finished failure_class=transient` | Restart AI peer; if provider key was stale, fix that first |
| Tool peer | `tool.web_fetch` (and friends) error back to caller | Per-flow log + caller HTTP response shows `kind=6 cause=tool.web_fetch ...` | Restart tool peer; cached redirect map is in-process only, no on-disk state to recover |
| Bridge | Operators see HTTP refused | `curl localhost:19791/v1/tasks` connection refused | Restart bridge; manifests re-discovered at startup |
| Telegram channel | Inbound messages stall; reply-side messages queue in the channel's session store | Telegram users see no reply; channel-controller log shows API errors | Restart channel; SqliteSessionStore preserves chat→task mappings across restarts |

The rest of this doc is the per-component long-form: what fails, how
it fails, what's still safe to use while it's down.

## Coordinator unavailable

### Detection signal

The bridge log line at startup that names the Coordinator alias
won't show its manifest discovery as `ok`. Beyond startup:

- `GET /v1/tasks*` returns `503 Service Unavailable` with body
  `{"error":"coordinator not configured ([coordinator] alias missing)"}`.
  (The error message is the same whether the bridge was never
  configured with a Coordinator or the configured Coordinator is
  unreachable — bridge state tracks the alias presence, not live
  reachability per request.)
- `GET /v1/topology` will show the coordinator peer's `freshness`
  flip from `fresh` → `stale` → `expired` as the 60s manifest
  refresh loop fails repeatedly.
- The bridge log emits `mesh: discovery: alias=coordinator: error: ...`
  on each failed refresh tick (at `debug` level — bump
  `RUST_LOG=relix_runtime::manifest=debug` to see them).

### Bridge behavior

The bridge fail-softs by design. Concretely:

- **Chat continues.** `/chat`, `/chat/stream`, `/chat_with_tool`,
  and `/v1/chat/completions` all run their SOL flows to completion
  without writing to the Coordinator. The response JSON omits
  the `task_id` field (also: `relix.task_id` on the OpenAI shim).
  This is the B1.9 fail-soft contract; see
  [`crates/relix-web-bridge/src/flow.rs`](../crates/relix-web-bridge/src/flow.rs)
  (`create_task_fail_soft`).
- **No new chronicle entries** are written for the duration. Per-flow
  event logs + per-node audit logs are unaffected — those are
  written directly by the responder peers, not by the bridge.
- **Existing `/v1/tasks/...` calls** fail with 503. The dashboard's
  task list pane shows "load failed: HTTP 503"; the rest of the
  dashboard (chat) still works.

### Recovery

Restart the Coordinator with the same config + DB path. On
startup, the Coordinator runs its recovery scan exactly once
(`[coordinator] recovery_scan = true`, default) which promotes
overdue `running` tasks to `interrupted` with
`failure_class=timeout`. The bridge's manifest-refresh loop
will re-discover it within 60s; until then `/v1/tasks*` still
returns 503.

No bridge restart is required.

The chronicle gap during the outage is permanent — chats
that ran during the down window have no Task row. If you need
to attribute those, the per-flow event logs (`flow_id` on the
chat response) are authoritative; they're written by the
peers themselves, not by the Coordinator.

## Memory peer unavailable

### Detection signal

Any chat that hits `memory.recent_for_session` or
`memory.write_turn` fails at that call. The caller sees an
HTTP 502/503 with body
`mesh transport: flow halted: remote_call(memory, memory.X): kind=8 cause=...`.

Bridge log:
`remote_call dispatcher: memory: unavailable`.

### Bridge behavior

Memory is in the request path for every chat flow that builds
context from history. There's no fail-soft for this — a chat
without memory is a different chat. The flow halts at the
unavailable `remote_call`. Per-flow event log records
`capability.invoked memory.X` followed by `flow.halted ...`.

If a Coordinator is configured, the bridge writes the
attempt's failure to the Coordinator AFTER the halt (the
`update` happens in the catch path). The Task ends in
`failed` / `last_failure_class=transient`.

### Recovery

Restart the memory peer with its config + SQLite DB. The
`MeshClient`'s call-with-reconnect path
(`crates/relix-runtime/src/manifest/mod.rs::call`) re-dials on
the next call, so no bridge restart is needed. Reconnect
counters surface via the bridge's `reconnect_counters()` (not
yet exposed over HTTP; logged on bridge shutdown).

User-visible: chats that failed during the outage need to be
re-driven by the operator or the user — there is no auto-retry.

## AI peer unavailable

### Detection signal

`ai.chat` errors at the responder; the bridge surfaces:

```
mesh transport: flow halted: remote_call(ai, ai.chat):
  kind=11 cause=ai.chat: <provider error>
```

`kind=11` is `ProviderError`. The cause includes the upstream
provider's error message verbatim (OpenAI, OpenRouter, etc.) so
operators can distinguish "AI peer down" from "provider returned
401."

### Bridge behavior

Same as memory: no fail-soft. Chat returns the error to the
caller. Task gets a `failed` row with `failure_class=transient`
(network blip) or `permanent` (provider rejection that won't
retry).

### Recovery

Restart the AI peer. Provider key changes need the controller
to be restarted; rotation isn't supported at runtime today
(deferred — see `docs/current-limitations.md`).

## Tool peer unavailable

### Detection signal

`tool.web_fetch` / `web_extract` / `pdf` / file ops error back:

```
mesh transport: flow halted: remote_call(tool, tool.X):
  kind=6 cause=tool.X: ssrf-rejected|...
```

`kind=6` is `ToolError`. Cause is verbatim from the tool node.

### Bridge behavior

Identical to AI/memory unavailability — flow halts at the call,
no fail-soft.

### Recovery

Restart the tool peer. The redirect-validation client pool is
in-process only; cold start re-validates per request.

## Bridge unavailable

### Detection signal

Operators see HTTP refused on `127.0.0.1:19791` (or whatever
loopback the bridge binds to).

### Bridge behavior

Nothing — it's down. Peers keep serving each other if any
mesh flows reach them, but the bridge is the only configured
caller in the default topology. So in practice, no callable
surface exists.

### Recovery

Restart the bridge with its config. On startup it:

1. Reads its identity bundle + signing key.
2. Dials every peer listed in its `peers.toml`.
3. Calls `node.manifest` on each.
4. Spawns the 60s manifest-refresh loop.
5. Binds the HTTP listener.

Manifest cache rebuilds from scratch — no on-disk cache to
recover or invalidate.

In-flight chats from before the restart are lost (operators
running `/chat/stream` see the SSE connection close). The
Coordinator chronicle, if configured, retains every Task
that was already at least created — but Tasks in `running`
that didn't finish are now zombies until the recovery scan
runs them through `interrupted`.

## Telegram channel unavailable

### Detection signal

- Inbound: Telegram users send messages but the channel
  controller's long-poll loop has stopped. Bot stays online
  in Telegram's view (no connection metric); silence is the
  signal.
- Outbound: the channel queues replies in its
  `SqliteSessionStore` (`docs/channel-node-architecture.md`)
  but never dispatches them.

### Bridge behavior

The bridge doesn't know about Telegram — channels live on
their own controller. Bridge HTTP traffic is unaffected.

### Recovery

Restart the channel controller. SqliteSessionStore is
restart-safe: `chat_id × message_id → task_id` mappings
persist, so replies dispatched after the restart still land
on the right Telegram thread. The mapping table uses
`UNIQUE(chat_id, message_id) ON CONFLICT REPLACE` so
idempotent re-deliveries are clean.

If the controller was down for longer than Telegram's
`getUpdates` retention (24h), updates from that window are
gone — there's no way to recover them. The user has to
re-send.

## Manifest refresh failures (per-peer)

### Detection signal

`GET /v1/topology` shows the peer's `freshness` flipped to
`stale` (≥120s since last successful refresh) or `expired`
(≥600s). Operators reading the log with
`RUST_LOG=relix_runtime::manifest=debug` see one
`manifest refresh: transport error` or
`manifest refresh: timed out` per failed tick.

### Bridge behavior

The bridge keeps using the cached capabilities for routing.
**This is intentional fail-soft behavior**: brief network
blips shouldn't force a re-discovery storm. The only
operationally meaningful effect is that the cached entry
gets older; new capabilities added on the peer won't be
discovered until a refresh succeeds.

### Recovery

Usually self-healing — the next refresh tick (60s) re-tries
and updates `last_refreshed_at`. If the peer's actually
down, escalate to that peer's playbook above.

Per-peer reconnect counters
(`MeshClient::reconnect_counters()`) increment on successful
recovery; a high `attempts - successes` delta is operator's
hint that one of the peers is flapping.

## SSE stream interruption

### Detection signal

The dashboard's "live ●" status line goes away and the
EventSource transport-level reconnect kicks in (default
~3s in browsers).

### Bridge behavior

The bridge's `/v1/tasks/:id/events/stream` is a long-poll
wrapper around `task.events` — when the connection drops,
the per-stream tokio task on the bridge dies cleanly. No
per-stream state is leaked; the bridge owns zero per-stream
state by design (see
[`docs/bridge-invariants.md`](bridge-invariants.md)).

On reconnect, the dashboard re-opens the URL with
`?since=<lastEventId>`. The bridge's prefix-extract cursor
advancement (see
[`crates/relix-web-bridge/src/tasks.rs`](../crates/relix-web-bridge/src/tasks.rs)
`extract_event_id_prefix`) ensures the new connection picks
up strictly after the snapshot point even if the
Coordinator's chronicle has hostile content.

### Recovery

No operator action — the dashboard auto-reconnects.

## libp2p transport blip / reconnect

### Detection signal

`MeshClient::call` invoked the call-with-reconnect path. The
bridge logs (at `debug`) `mesh: peer reconnected: ...`. The
`reconnect_counters()` getter shows attempts incrementing.

### Bridge behavior

The first call after the peer comes back is transparently
re-dialed; the caller never sees the disconnect. The
`reconnect_counters` are the only operator-visible signal
(today only on bridge shutdown logs).

### Recovery

Self-healing. No operator action.

## Network partition between bridge and a peer

### Detection signal

The peer's `freshness` in `/v1/topology` flips to `expired`
but the peer log shows it's healthy. Manifest refresh logs
show transport errors. Flows that try to route through that
peer halt at the `remote_call`.

### Bridge behavior

Same as "peer unavailable" — the bridge treats the symptom,
not the cause. From its perspective, an unreachable peer
and a partitioned peer are indistinguishable.

### Recovery

Fix the network. Bridge + peer auto-reconnect on the next
attempt; the manifest cache updates on the next 60s tick.

## What recovery is NOT today

These are honest non-goals that affect Phase 1 deployments:

- **No automatic flow re-execution** when a peer comes back.
  A Task that failed during an outage stays `failed`. Operators
  drive `task.retry` (which updates metadata) + their own
  re-execution mechanism. See
  [`docs/retry-model.md`](retry-model.md).
- **No active liveness probes** beyond the 60s manifest
  refresh. The bridge does not ping idle peers.
- **No Coordinator failover.** One Coordinator owns the
  ledger. If it goes down, persistence stops; restart restores it.
- **No multi-bridge HA.** Two bridges fronting the same mesh
  would race on chat sessions. Put a consistent-hash router
  in front when this matters.
- **No automatic chronicle replay.** A flow that completed
  without a Coordinator (during a Coordinator outage) cannot
  have its chronicle reconstructed. The per-flow event log on
  disk is the authoritative record for that case.
- **Task cancellation is metadata-only.** `POST /v1/tasks/:id/cancel`
  appends a `task.cancelled` chronicle event and updates the
  ledger status. The runtime has no flow-side cancellation
  protocol today — a currently-executing flow continues, and
  its eventual write-back may overwrite the `cancelled` status.
  The bridge returns `flow_still_running: true` on the response
  when prior status was `running` / `retrying` so dashboards
  warn operators explicitly. Real flow-side cancellation is
  Phase 2 work.

## See also

- [`operator-guide.md`](operator-guide.md) — running the mesh + common
  failure modes with curl recipes.
- [`deployment.md`](deployment.md) — multi-node + production topology.
- [`bridge-invariants.md`](bridge-invariants.md) — what the bridge
  may/must-not do (the architectural constraint set that justifies
  most fail-soft choices here).
- [`task-recovery.md`](task-recovery.md) — per-failure-class
  playbook for failed/interrupted Tasks.
- [`retry-model.md`](retry-model.md) — what `task.retry` actually
  does (and doesn't).
- [`current-limitations.md`](current-limitations.md) — the
  full honest list of non-goals.
