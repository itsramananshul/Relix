# Bridge Invariants

The `relix-web-bridge` crate translates between HTTP/JSON and
the Coordinator's pipe-delimited wire format. It is NOT an
orchestrator, scheduler, planner, executor, or gateway. This
document is the contract — every new endpoint and every
refactor MUST keep these invariants true.

If a contributor finds themselves wanting to violate one, the
right move is to push the responsibility into either the
Coordinator (for state) or SOL (for orchestration) — not to
quietly grow the bridge.

## What the bridge MAY do

The bridge is allowed to:

- Accept and validate HTTP requests (path / query / body shape).
- Translate between HTTP-friendly JSON and the Coordinator's
  pipe-delimited wire format.
- Call existing capabilities on existing peers via the existing
  admission pipeline (`identity → policy → handler → audit`).
- Render a SOL flow template (substitute `{{SESSION}}`,
  `{{MESSAGE}}`, etc.) before handing it to FlowRunner.
- Stamp a `trace_id` per HTTP request and propagate it.
- Persist nothing of its own beyond the `ManifestCache` (which is
  a pure projection of `node.manifest` discovery) and per-request
  state on the open HTTP socket.
- Provide presentation surfaces (JSON shapes, SSE streams) as
  thin wrappers over Coordinator capabilities.

## What the bridge MUST NOT do

The bridge MUST NOT:

1. **Own task state.** Status, retry counters, attempt rows,
   chronicle events all live on the Coordinator. The bridge
   does not cache or duplicate them.
2. **Make orchestration decisions.** Which SOL flow runs for a
   given request is a template-selection step (config-driven);
   anything beyond that — branching, retry logic, fan-out — is
   SOL's job.
3. **Mutate state outside admission-pipeline calls.** Every
   write goes through a capability the Coordinator (or another
   responder) admits via its policy + identity check. The
   bridge does not bypass.
4. **Hold a persistent task-state store.** SQLite is a
   Coordinator concept. The bridge's `TaskRecorder` is a
   stateless RPC client; there is no on-disk bridge ledger.
5. **Run background processes.** No scheduler, no autonomous
   retry daemon, no chronicle reaper, no health-check loop
   that mutates anyone else's state. The bridge is purely
   request-driven (HTTP request → some RPC calls → response).
6. **Make policy decisions.** Policy lives on the Coordinator
   (and every other responder). The bridge cannot admit a
   call its identity isn't allowed to make.
7. **Spawn flows or tasks autonomously.** Every task is created
   in response to an incoming HTTP request. There is no
   "watchdog" or "supervisor" inside the bridge.

## Mechanical checks

The crate-level contract is partially enforced by the build:

- **No SQLite dependency.** `relix-web-bridge` does not depend
  on `rusqlite` directly. Persistent storage is a Coordinator
  concern; the bridge would have no reason to pull this in.
  Verified by the `bridge_has_no_sqlite_dependency` test (see
  `crates/relix-web-bridge/tests/invariants.rs`).
- **No event log emit.** The bridge does not call into
  `relix_core::eventlog::EventLog` to write its own event log.
  Per-flow event logs are written by `FlowRunner` (which the
  bridge calls into but doesn't own).
- **No policy engine instantiation.** The bridge holds no
  `PolicyEngine` of its own; each responder evaluates its own
  policy on every inbound RPC.

These are minimal mechanical guardrails — most of the contract
is still source-review enforced. The `bridge_has_no_sqlite_dependency`
test exists as a single canary: if it fails, someone is on the
wrong track.

## Where to extend instead

If you want to add behaviour to the bridge that touches these
invariants, the right path is:

| Desired behaviour | Right home |
|---|---|
| New persistent metadata about tasks | Coordinator capability + schema column |
| New decision step in flow execution | SOL flow template |
| New retry / scheduling logic | NO — Gate 2 work; not allowed today |
| New chronicle event type | Coordinator's runtime emitter |
| New view shape over existing data | Bridge handler ✓ |
| New filter / projection over existing data | Bridge handler ✓ |
| New SSE / WebSocket presentation | Bridge handler ✓ (presentation-only) |

A bridge handler is allowed to introduce a new HTTP shape; it is
not allowed to introduce a new behaviour the underlying mesh
doesn't already support.

## Refactor checklist

Before merging a bridge change, confirm:

- [ ] No new `rusqlite` import in `crates/relix-web-bridge`.
- [ ] No new background `tokio::spawn` that doesn't die with
      the current request's task.
- [ ] No new file/directory written to disk other than logs
      (the `tracing` framework already handles this).
- [ ] No new mutable state on `AppState` beyond cached
      discovery (or anything that survives a bridge restart).
- [ ] Every new write path calls an existing capability through
      the existing admission pipeline.
- [ ] The new behaviour is documented in `task-api.md`,
      `capability-discovery.md`, or the appropriate reference
      doc.

## See also

- [`architecture.md`](architecture.md) — the peer model the
  bridge participates in.
- [`task-api.md`](task-api.md) — the HTTP surface the bridge
  exposes today.
- [`coordination.md`](coordination.md) — where task state actually
  lives.
- [`runtime-observability.md`](runtime-observability.md) — the
  observability primitives the bridge surfaces (it does not
  produce them).
- [`crates/relix-web-bridge/`](../crates/relix-web-bridge/) —
  the crate the contract applies to.
