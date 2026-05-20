# Current Limitations

This document lists what the Relix alpha **does not do**, in plain terms,
without hedge or roadmap promises. The goal is that anyone evaluating
Relix can decide quickly whether the limitations are acceptable for
their use case.

Read this **before** deploying to anything other than a local developer
machine.

The corresponding "what does work" surface is in the
[README](../README.md). The corresponding "documented alpha trade-offs
with rationale and resolution gate" is in
[`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md);
where a limitation here corresponds to a SIMP entry there, it's cited.

## Operations and resilience

### Coordinator is a Task ledger, not a flow scheduler

The Coordinator node-type owns a durable SQLite ledger of Task records
+ per-attempt execution rows + events (see
[`coordinator.md`](coordinator.md) and
[`attempt-lineage.md`](attempt-lineage.md)). It does **not**:

- Watch for peer health.
- Auto-launch interrupted tasks. The C1b recovery scan promotes
  overdue `running` rows to `interrupted` and closes the open
  attempt — that's it. Re-launch is operator-driven.
- Schedule or queue work — there is no auto-scheduler picking up
  `pending` tasks.
- Auto-retry. The C2c `task.retry` capability validates state +
  budget and flips metadata; the operator (or the bridge on a fresh
  request) still has to actually re-run the flow.
- Resume a flow mid-execution (the SOL VM is synchronous — see
  [`replay-model.md`](replay-model.md) for the honest framing).

What it gives you is durable per-attempt records of who tried to do
what, the chronicle of how it went, and the pointer to where the
per-flow event log lives. Retry decisions are operator-driven via
`relix-cli task retry`.

### Bridge persists every chat as a Task (fail-soft)

(Formerly a documented gap; **closed** in commit `<pending>`.)
All three chat-bearing endpoints (`/chat`, `/chat_with_tool`,
`/v1/chat/completions`) auto-create a Task on the Coordinator before
the flow runs, append `task.created` + `flow.started` (and
`capability.invoked` for the tool path), and write a terminal
`task.update(status=completed|failed)` + `task.completed` /
`task.failed` event when the flow finishes.

The integration is **fail-soft**: every `task.*` call from the bridge
warns-and-skips on Coordinator failure. Chat requests never block on
Coordinator availability. The `task_id` is omitted from the response
JSON entirely when persistence was not wired or failed, so strict
OpenAI clients never see a field they don't expect.

What's still **not** done:
- Per-`remote_call` events. The bridge writes flow-level events
  (`task.created`, `flow.started`, `task.completed`/`task.failed`)
  plus a single `capability.invoked` on the tool path. Per-call
  detail lives in the existing per-flow event log on disk, which
  `task.latest_flow_log_path` points at.
- Status transitions through `running`. The current path writes
  `pending` → `completed` (or `failed`); the intermediate `running`
  state is not used by the bridge. Operators driving tasks manually
  via `relix-cli task update --status running` use it; the canonical
  bridge path skips it.

### The bridge's `MeshClient` auto-reconnects on transient drops

(Formerly a documented limitation; **closed** in commit `<pending>`.)
The bridge holds an alias → `Multiaddr` address book alongside the
alias → `PeerId` map. When `MeshClient::call` sees a transport-class
error (`DialFailure`, `ConnectionClosed`, `Timeout`, `io`), it re-dials
the original address once, waits briefly for the swarm to settle, and
retries the call. Live-verified by killing the memory peer mid-session:
the next chat fails with `retry after redial failed`; restarting the
peer and re-issuing the chat succeeds without a bridge restart.
Controller keys are persistent on disk so `PeerId`s are stable across
peer restarts; the cached mapping stays valid.

What's still **not** handled: a peer whose Ed25519 key is regenerated
(by deleting `dev-keys/<run>-<node>.key` and restarting). The bridge's
cached `PeerId` would be stale and the redial would connect to a peer
with a different `PeerId`. The fix is "delete the bridge's cache too
(restart the bridge)"; documented behaviour, not silent failure.

### Discovery refreshes periodically

(Formerly a documented limitation; **closed** in commit `<pending>`.)
The bridge spawns a background task that re-runs `node.manifest`
against every peer in its address book every 60s, updating the
`ManifestCache`. A peer that comes online *after* the bridge will be
discovered within one refresh interval and become reachable via
`capability:<method>`. A peer whose capabilities change at runtime
(e.g. a node-type with a hot-swap registration; not currently used in
any built-in node) will also be picked up.

What's still **not** handled: peers whose Multiaddr changes (different
port, different host). The address book is populated from `peers.toml`
at bridge startup and is not refreshed. SIMP-007 keeps applying for
fully gossip-based discovery; the alpha refresh covers the in-`peers.toml`
case only.

### No gossip / DHT-based peer discovery

The libp2p Kademlia behaviour is **configured** in the transport
stack (`crates/relix-runtime/src/transport/rpc.rs`) and `bootstrap_kademlia`
is called once at controller startup, but there is no working
DHT-based peer-find or capability gossip in the alpha. Peer addresses
come from the static `peers.toml`. The DHT being present in the swarm
configuration is **not** the same as being useful.

### Static peer alias map (`peers.toml`) is still load-bearing

Even with capability discovery (M10), every peer the bridge talks to
must be in the `peers.toml` so the bridge has somewhere to dial.
`capability:<method>` routing chooses *between* aliases in that file;
it does not discover new peers from the network. SIMP-017.

### No standalone log rotation

`dev-data/<run>/{memory,ai,tool,bridge}.log` grow unbounded. The
audit log (`<run>-<node>/audit.log`) is the integrity-relevant one
and should be shipped off-host on any real deployment. The script
itself does not rotate.

### Provider `local` (Ollama / vLLM / llama.cpp) is not stress-tested

`-Provider local -BaseUrl http://...` works for the deterministic
prompts the alpha exercises. Local model failure modes (model not
loaded, context overflow, GPU OOM) surface as generic provider
errors; there is no graceful fallback.

### Tool node pool has no LRU eviction

The `PinnedClientPool` grows one entry per unique
`(hostname, validated_addrs)` route the flow visits. A soft cap of
256 emits a WARN; eviction lands in a follow-up if real workloads
push past that. Bound is operator-driven (the set of hosts your flows
actually fetch).

## Security gaps the alpha owns

### Manifests are not signed

`NodeManifest` is sent as plain CBOR. A peer can lie about its own
capabilities; the bridge trusts what it receives. Gate 2 wraps the
manifest in `Bundle(BundleType::NodeManifest)` and verifies against
the org root. Relevant for any deployment where mesh peers are not
all under one administrator.

### Identity bundles have one delegation level

The org root signs IdentityBundles directly. There is no Intermediate
Authority (IA) layer. Compromised org-root key = compromised mesh.
Mitigate by keeping the org-root secret out of any controller config
and using short-lived bundles. SIMP-002.

### No CRL or revocation gossip

The only way to invalidate an identity is to let it expire. Default
bundle lifetime from `relix-cli identity mint` is 24 hours. Tighten
it for higher-risk roles (`--hours 1`). SIMP-003.

### Tool node cross-host redirect window is narrow but not zero

The redirect `Policy::custom` re-runs the SSRF guard on every hop, so
a redirect to a forbidden IP or hostname is rejected pre-connect. But
once the guard validates a cross-host redirect target, reqwest
re-resolves and connects with the default OS resolver (no pin for the
new host) — there is a sub-millisecond window between policy check
and connect during which an attacker controlling DNS for the redirect
target could rebind. Per-hop pinning needs a custom hyper resolver,
tracked in [`tool-node-security.md`](tool-node-security.md). For
zero-window posture today, set `[tool] max_redirects = 0`.

### `tool.web_fetch` is GET-only and text-only

`POST` / `PUT` / `DELETE` are not exposed. Response bodies must
decode as UTF-8 and have a `text/*`, `application/json`,
`application/xml`, `application/xhtml+xml`, or `application/*+json`
content type. Bodies are read whole into memory subject to the
configured cap.

### No outbound mTLS / origin-side client auth from the tool node

The tool node verifies origin certificates via webpki, but does not
present a Relix-issued client cert to the origin. Use this when you
need bidirectional auth between Relix and an upstream service.

### Audit log is per-node, not federated

Each controller maintains its own hash-chained audit log
(`dev-data/<run>-<node>/audit.log`). Cross-node correlation is by
`request_id` / `trace_id` recorded in both the responder's audit
record and the caller's per-flow event log. There is no audit
aggregator; operators are expected to ship logs to a SIEM.

### No per-caller / per-method rate limiting

The policy engine is allow / deny only. Cost-class-aware throttling
(the `CapabilityDescriptor::cost_class` field exists for it) is not
implemented. A caller that floods the AI node will simply burn the
provider's per-key budget.

## Wire-format gaps

### `remote_call` args and returns are UTF-8 strings (SIMP-016)

The wire envelope itself is CBOR with full typing, but the alpha
keeps the SOL ↔ handler boundary as `String`-shaped to avoid
inventing a SOL type system for the alpha. Pipe-delim fields are the
per-method convention. Typed CDDL replaces this at Gate 2.

### Bridge template substitution is character-level (SIMP-018)

The bridge writes a rendered `.sol` file per request. The
substitution validator rejects `"`, `|`, and newlines so user input
can't escape the SOL string literal. This works but is not the same
as typed flow arguments — three characters are forbidden in user
input. `relix-web-bridge::validate::validate_input` shows the exact
rule.

### Streaming is bridge-level chunking, not provider tokens (SIMP-019)

`POST /chat/stream` and the `stream:true` variant of
`/v1/chat/completions` slice the *already-materialised* reply into
SSE chunks (`24` bytes by default, `chunk_delay_ms = 15`). The AI
provider's stream (if any) is consumed eagerly into a buffer first;
the bridge does not pass-through tokens. Open WebUI works correctly
with this; latency-sensitive UIs will not. Provider-native streaming
needs the durable yield model (Gate 2).

### OpenAI shim drops fields (SIMP-020)

`/v1/chat/completions` accepts the full request shape and ignores:

- `system` messages (only the last `user` message becomes the prompt;
  the first system + user message hashes into the session id).
- `tools` / `tool_choice` / `function_call`.
- `temperature`, `top_p`, `n`, `presence_penalty`, `frequency_penalty`,
  `max_tokens`, `logprobs`, `response_format`, ... (sampling and
  format controls are provider-side).
- Multimodal `content` arrays (only text-string content is supported).

The shim is a translation layer to make Open WebUI work, not a full
OpenAI API. Full surface is in
[`streaming-and-openai-shim.md`](streaming-and-openai-shim.md).

### Bridge holds no `Authorization` semantics

The OpenAI shim reads the `Authorization: Bearer ...` header and
**ignores it**. The bridge is bound to `127.0.0.1` only; auth in
front of the bridge is the operator's responsibility (reverse proxy,
local socket, etc.). Do not expose the bridge port to a network
without a fronting authn/authz layer.

## Provider gaps

### `gemini` provider is a placeholder

`-Provider gemini` produces an AI node that returns clean errors
(not a real Gemini call). Tracked; will land when the Anthropic and
Gemini providers share a cleaner abstraction.

### One model id per AI node

The `[ai] model = "..."` field on the AI node is one default; the
bridge exposes `relix-<provider>` as the model picker entry. There is
no multiplexing of multiple models on one AI node — run a second AI
controller with a different config if you need both.

## SOL VM gaps

### Synchronous `remote_call` only (SIMP-001 / SIMP-014)

`remote_call` blocks the VM thread. The host bridges to async libp2p
via `tokio::task::spawn_blocking` + `Handle::current().block_on(...)`.
The flow can't issue concurrent calls.

### No `Inst::FlowArg` (SIMP-018)

A SOL flow takes no first-class arguments. The bridge does template
substitution and writes a rendered file per request. CLI `flow-run`
takes a `.sol` file with no arguments at all.

### No durable replay / no flow snapshots (SIMP-005)

The per-flow event log records every `RemoteCall*` event with hash
chaining, which is the property the replay-equivalence property test
(SIMP-008) is supposed to assert at Gate 2. Today the log is
write-only for audit.

### Hand-written flows; no SolFlow editor (SIMP-011)

Every flow under `flows/` is hand-written. There is no visual
authoring surface in the alpha.

## CI and quality gaps

### `cargo deny` is wired but not enforced in CI (alpha policy)

The `deny.toml` exists and `cargo deny check` runs cleanly locally,
but the alpha CI matrix doesn't fail PRs on `cargo deny` regressions
yet.

### No fuzz coverage (SIMP-012)

Wire format and SSRF parser are obvious fuzz targets and have none in
the alpha. Property-test coverage of codec determinism exists; fuzz
ships after Gate 2.

### Conformance tests exist but are alpha-narrow

`conformance/` holds wire-format vectors. Cross-language interop
testing (the test that proves the protocol is portable) is not in
scope until Gate 2.

## Platform gaps

### Windows-specific cleanup quirk

`scripts/relix-mesh-up.ps1` intercepts Ctrl-C and stops only the
PIDs it spawned. If the launching PowerShell process is *itself*
killed externally (not Ctrl-C), the script's `finally` doesn't run
and the children orphan — they have to be cleaned up manually.
Documented in
[`operator-guide.md`](operator-guide.md#stopping-the-mesh-safely).

### Open WebUI in Docker on Linux

On Linux Docker, `host.docker.internal` does not resolve by default.
Use `--add-host=host.docker.internal:host-gateway` when starting the
Open WebUI container, or use `--network=host` and `127.0.0.1`.

## How to think about all this

The alpha exists to prove the architecture — peer-native nodes, SOL
orchestration, per-call admission, audit, SSRF-guarded external
actions — works end to end. It is **not** trying to be production-
grade in any single dimension. Every gap above either:

1. has a clear path to closure in a later milestone, **or**
2. is a deliberate scope cut so the alpha could ship a coherent
   architecture rather than a 1.0 in one feature area.

If you find a gap that isn't in this document, that's a documentation
bug — please file it.

## See also

- [`security.md`](security.md) — what the admission pipeline does
  enforce.
- [`tool-node-security.md`](tool-node-security.md) — full SSRF /
  DNS-pin / redirect model with the exact remaining windows.
- [`specs/alpha-simplifications.md`](../specs/alpha-simplifications.md)
  — every SIMP, with rationale and resolution gate.
