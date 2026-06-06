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
[`coordination.md`](coordination.md) and
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

### Run-workspace review/apply is inspect-and-copy, not a full VCS workflow

A Brief Shift (run) executes in a scoped sandbox workspace and its
changed files can be **inspected** (changed-file list, secret-redacted
text preview, bounded unified diff), **reviewed** (accept/reject),
**applied** back into the configured project root, or **discarded**. What
it does **not** do:

- **Diff needs an intact baseline.** The unified diff
  (`/v1/runs/:id/artifacts/:aid/diff`) reconstructs the "before" side from
  the live project-root file and only when it still hashes to the run's
  recorded baseline. If the project file changed since the run, or the run
  used the `empty` workspace context (no project copy), the diff is
  honestly reported unavailable and you fall back to the file preview.
- **Preview/diff are byte-capped** (64 KiB): a very large changed file is
  truncated, not paged.
- **Apply is whole-run, all-or-nothing.** There is no per-file or partial
  apply and no `force`; if *any* file is unsafe or conflicted the whole
  apply is refused. Conflict resolution is operator-driven (re-run, or fix
  the project and retry) — there is no three-way merge.
- **Discard does not free disk immediately.** It marks the run discarded
  (and non-applyable) and leaves the sandbox for the storage prune /
  scheduled autoprune to reclaim; it never deletes a `running`
  workspace. Disk is reclaimed by `maintenance.prune` (dry-run first), not
  synchronously.
- **`git_worktree` / `git_checkout` workspace context is deferred** — only
  `empty` and the capped/filtered `copy_repo` snapshot ship today.

### The Prime / company flow is governed + rule-based, not an autonomous CEO

Relix models a company — Founder, Prime (planning lead), Crew, Mandates,
Clearances — and the dashboard drives the whole loop (found the company →
hire a Prime → describe a goal so **Prime proposes a plan** → approve it to
create the Mandate + Briefs + crew assignments + pending hires → greenlight
spawn Clearances → **Start the work**). The `company.status` summary surfaces
the Founder, the Prime, and the crew breakdown (active / pending / by role).
The **Prime Assistant** (`POST /v1/spine/prime/propose` → `…/approve` →
`…/start`, the Chat page) turns a free-text request into a structured,
governed plan that creates nothing until approved, then starts the ready work
when you click Start. What it does **not** do:

- **The Prime is rule-based by default; a model can draft the plan opt-in,
  but the coordinator never calls a model itself.** The default plan is
  deterministic and request-aware — intent shapes the breakdown (a `fix` is a
  reproduce → fix → verify chain, `research` is investigate → synthesize,
  `build` is role tracks + integrate), and each Brief title carries the
  extracted deliverable, so two different requests no longer collapse to one
  shape (company-model §12.5A). **Model-assisted planning is now available but
  opt-in** (the Chat "Use AI" toggle → `mode:"ai"`): the *bridge* drafts a plan
  with the `ai` peer, then the *coordinator* validates + sanitizes +
  secret-redacts it server-side (`prime_plan::validate_model_plan`) before it is
  ever stored, and computes crew/hires/governance from the live roster — a model
  can shape only the interpretation. The coordinator handler still **never calls
  an LLM synchronously** (the AI node is a separate mesh peer); the response is
  honest about provenance via `ai_mode` (`deterministic_only` / `llm_used` /
  `fallback` / `unavailable`) + `ai_used` + `ai_status`, and **any** model
  failure (unreachable / oversized / malformed / invalid) degrades to the
  deterministic plan with an honest reason — never faked model output. What is
  **not** done: the live bridge→model→coordinator round trip is not
  integration-tested in CI (it needs a real provider; the validator + fallback
  that bound it are fully tested with fake output), and there is no
  conversational refinement — one message → one proposal.
- **A human is in the loop at every gate.** Prime PROPOSES; the operator must
  click **Approve & create** to create anything, greenlight each spawn
  Clearance, and click **Start the work** to run it. `prime.start`
  (company-model §12.5B) closes the loop — it turns the approved Mandate's
  ready Briefs into real Shifts through the same governed run path as a manual
  run (approved-only, ready-only, every skipped Brief reported with a reason) —
  but it is still an operator-initiated gate, not an auto-pilot. There is no
  driver that takes a goal end-to-end (propose → approve → staff → orchestrate
  → run) on its own.
- **Hiring is request → approve only.** Prime suggests *which roles* are
  missing and files them as `pending` hire requests on approval, but it does
  not decide *which person/identity* to hire or *which adapter* to assign,
  and a pending hire is inert until a separate Clearance activates it.
- **The autonomous heartbeat only executes**, it does not plan. It runs
  already-assigned Briefs on a timer — it never authors strategy, staffs a
  team, or orchestrates a Mandate.
- **No org-graph visual** beyond a shallow reports-to list.
- **The Live Shift Room now has a dedicated session stream, with polling as a
  fallback.** After `prime.start`, the Chat approved-plan card shows a live
  Shift Room (each Brief's latest Shift, blockers, review/apply state, and a
  next-action button) sourced from the READ-ONLY `prime.status` capability. The
  dashboard **prefers a dedicated per-session SSE stream**
  (`GET /v1/spine/prime/proposals/:id/status/stream`): the server emits the
  initial status snapshot immediately, **reuses** the existing run-event feed
  (`run.events.recent`) only as a cheap change-trigger so a Shift transition
  reflects within ~1 s, and **force-refreshes on a low (~3 s) interval** so the
  room still converges if an event is missed or the run-event source is absent.
  Identical frames are de-duped (keep-alive ping only), so the loop never spins.
  When the stream **isn't** connected the dashboard falls back to **polling the
  snapshot every 4 s** and the header badge honestly reads `polling` (it only
  says `live` when the stream is actually connected) — it never claims realtime
  when the stream is unavailable. A tenant-gated / unknown proposal emits a
  terminal `event: not_found` (no existence leak) and stops cleanly. The stream
  invents **no new state or event table** — it composes the same read
  capability the polling route uses. `prime.status` itself **never starts,
  applies, or discards** anything (those remain the existing explicit routes);
  when a relation is unknowable it returns honest partial data (e.g.
  `latest_run:null`), never a fabricated state. So a finished/blocked Shift
  still appears within ~1 s of its run event (or one forced-refresh / poll
  interval) — low-latency, not hard-realtime push of every field.
- **Shift-Room blockers are tenant-scoped.** `prime.status` reads a Brief's open
  blockers (Snags) through `list_snags_for_tenant`, which filters the related
  (blocker) Brief to the proposal's own Guild. Even a **legacy `blocked_on` edge
  that crosses Guilds** can never surface a cross-tenant blocker id or title in
  the Shift Room — pinned by a coordinator test that forces such an edge.

In short: the *governance rails* of a company are in place and tenant-safe,
the Shift Room makes the post-start loop legible (what ran / finished / is
blocked / needs review, with the next action one click away) over a dedicated
low-latency status stream (polling fallback, honest badge), and a model can
now draft the plan **opt-in** behind a server-authoritative validator — but the
default Prime is still rules, the model only shapes the *interpretation* (never
crew/governance), and there is no autonomous driver that reasons about strategy +
team + work end to end.

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

### Streaming is provider-native at the AI node, bridge-chunked at the HTTP edge (SIMP-019, partial)

Every active provider — `mock`, `openai`-compatible (OpenAI /
OpenRouter / xAI / local), Anthropic, Gemini — now implements
`ChatProvider::generate_reply_stream` against the provider's
native streaming endpoint:

- OpenAI-shape: `/chat/completions` with `stream: true`; parses
  `data: {...}` SSE frames into `choices[0].delta.content` deltas.
- Anthropic: `/v1/messages` with `stream: true`; parses
  `content_block_delta` events with `delta.type = "text_delta"`.
  Extended-thinking deltas are intentionally skipped (the
  assistant-visible reply text only).
- Gemini: `:streamGenerateContent?alt=sse`; emits the incremental
  suffix over a "cumulative running total" wire shape.

What still isn't end-to-end: the bridge's `POST /chat/stream` and
the `stream:true` variant of `/v1/chat/completions` invoke the
SOL chat flow via the mesh's request/response transport, which is
single-shot today. The bridge therefore still receives a fully-
materialised reply from the AI node and slices it into SSE
chunks at the HTTP edge. Provider→bridge streaming pass-through
needs a streaming `remote_call` primitive on the mesh transport
(Gate 2 spec target). Operators who want per-token streaming
today must call the AI node directly through a flow that returns
the streamed text.

### OpenAI shim drops fields (SIMP-020)

`/v1/chat/completions` accepts the full request shape. The current
behavior:

- `system` messages are **preserved** and prepended as
  `[SYSTEM N]\n<content>\n\n` blocks before the last user message.
- `tools` / `tool_choice` / `function_call` fields and `role:"tool"`
  messages are **rejected with 400** (not silently dropped).
- `temperature`, `top_p`, `n`, `presence_penalty`, `frequency_penalty`,
  `max_tokens`, `logprobs`, `response_format`, ... (sampling and
  format controls) are accepted but not forwarded; handled provider-side.
- Multimodal `content` arrays (only text-string content is supported).

The shim is a translation layer to make Open WebUI work, not a full
OpenAI API. Full surface is in
[`streaming-and-openai-shim.md`](streaming-and-openai-shim.md).

### Bridge bearer token is loopback-scoped, not internet-grade auth

The bridge enforces a bearer token on all non-public routes (stored
at `~/.relix/bridge-token`). For loopback-only deployments (the
default) this is sufficient. However:

- The `Authorization: Bearer` header must match the token exactly —
  any other value receives 401.
- For deployments exposed beyond loopback, a reverse proxy with
  TLS + external auth is still required. The bearer token is a local
  shared secret, not a substitute for mTLS or OAuth.
- `/health` and `/dashboard` are public (no auth) by design.

### Dashboard admin login is a single local account

The React dashboard authenticates with a username/password operator
login layered on top of the bridge token (see
[`relix-dashboard-design.md`](relix-dashboard-design.md)). What it does
**not** do:

- **One admin, not multi-user.** There is exactly one admin credential
  per bridge (`dashboard-admin.json`, Argon2id hash next to the bridge
  token). No roles, no per-operator accounts, no SSO.
- **Sessions are in-memory.** A logged-in session rides an HttpOnly
  `relix_session` cookie (12h TTL) held in the bridge process. Restart
  the bridge and every operator must log in again — by design, but it
  means a busy operator is logged out on every deploy.
- **No online password reset.** A forgotten password is recovered
  **only locally** on the host: `relix dashboard reset-admin` (or
  `relix-web-bridge reset-admin`, or
  `scripts/relix-dashboard-admin-reset.{ps1,sh}`). It rewrites just the
  admin credential — never the data — and there is deliberately no
  network/unauthenticated reset surface. Restart the bridge afterward.
- **Protected APIs stay protected.** The SPA shell (`/dashboard`) is
  public, but `/v1/tasks`, `/v1/spine/*`, `/v1/prime/*`, providers, etc.
  require the cookie (or the bearer). Before you log in — or after the
  session lapses — those calls return **401**; the dashboard now routes
  that to the login screen ("Your session expired — sign in again")
  instead of broken cards. A 401 on those routes is auth being
  **enforced**, not the spine being down — `relix dashboard doctor`
  distinguishes the two.

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
