# Alpha Simplifications

This document is the bridge between what the alpha implements and what the substrate specs (`RELIX-1` through `RELIX-8`) define. Every alpha shortcut is recorded here with the gate at which it must be resolved. **No simplification is silent.**

If a behavior in the running code does not match a spec, either:
- it is listed here with a deadline, OR
- it is a bug.

## SIMP-001 — Synchronous `remote_call` opcode

**Spec target:** `RELIX-7` §7.4 (yield opcodes with durable suspension via event log).

**Alpha behavior:** SOL `remote_call(target, method, args)` blocks the VM thread until the RPC completes. The flow's event log still records `RemoteCallIssued` before the call and `RemoteCallCompleted` after, preserving the log-before-act invariant.

**Why:** The full yield/replay-equivalence model requires CPS-style compiler restructuring and durable promise state. The synchronous variant gets us working SOL routing across nodes in one day; the event-log shape is identical to the target so the migration is the VM internals only.

**Consequence:** A flow waiting on a slow AI call holds its VM thread. Acceptable for alpha demos; unacceptable for production with many concurrent long flows.

**Resolution gate:** Gate 2.

## SIMP-002 — Single-key trust model (no IA hierarchy)

**Spec target:** `RELIX-4` §4.11, `specs/identity-employees.md` §H.1 (three-tier Org Root → IA → AIC/GMC).

**Alpha behavior:** One Ed25519 key (`dev-keys/org-root.key`, gitignored) acts simultaneously as Org Root and Issuer Authority. `relix-cli identity mint` signs identities with it directly.

**Why:** A two-tier hierarchy with delegation chain validation doubles credential-related work for marginal alpha value. The bundle envelope already supports a delegation chain field; we ship with length-0 chains in the alpha.

**Consequence:** Org Root key is online. Compromise = total mesh compromise. Documented in `SECURITY.md`.

**Resolution gate:** Gate 2.

## SIMP-003 — No CRL gossip; revocation by expiry only

**Spec target:** `RELIX-4` §4.13 (CRL gossip + emergency revoke_now).

**Alpha behavior:** Identities have `not_after` (default 24h for AICs, 7d for node manifests). Revocation = wait for expiry or restart the node with a new policy excluding the compromised identity. No active revocation list propagation.

**Why:** CRL gossip requires a gossip channel beyond connection-time manifest exchange; out of week scope.

**Consequence:** Compromise-response window = max credential lifetime.

**Resolution gate:** Gate 2.

## SIMP-004 — Allowlist policy DSL instead of Cedar

**Spec target:** `RELIX-1` §1.13 step 9 + the policy architecture (Cedar embedded per `docs/code-reuse-map.md`).

**Alpha behavior:** Policy is a small TOML/YAML allowlist DSL: groups × method patterns × allow/deny. The `PolicyEngine` trait shape matches what Cedar will provide later (`evaluate(principal, action, resource, context) -> Decision`), so swap is non-disruptive.

**Why:** Cedar integration takes ~1 week solo; alpha cannot afford it.

**Consequence:** No `require_approval` outcome. No shadow-mode policy updates. No formally analyzable policies.

**Resolution gate:** Gate 2.

## SIMP-005 — No snapshots in event log

**Spec target:** `RELIX-3` §3.8.

**Alpha behavior:** The event log is append-only signed hash-chained. On recovery (or `relix-flow-inspect --replay-verify`), we replay from `event_seq = 0`.

**Why:** Snapshots are an optimization, not a correctness requirement (per the spec). Skipping them keeps the alpha smaller.

**Consequence:** Recovery time scales with log length. Acceptable for alpha (flows are short).

**Resolution gate:** Gate 2.

## SIMP-006 — Simplified streaming substream protocol

**Spec target:** `RELIX-2` (full credit-controlled bidi with heartbeats and resumption).

**Alpha behavior:** AI token streaming uses a minimal frame set: `open`, `chunk(seq, payload)`, `done`, `error`. No credit accounting (assume small chunks). No heartbeats. No resumption.

**Why:** The full RELIX-2 protocol takes days alone. Token streaming is the only use case in the alpha; a minimal protocol suffices.

**Consequence:** Mid-stream connection drop = stream lost; caller restarts from scratch. No backpressure under fast-producer / slow-consumer.

**Resolution gate:** Gate 2.

## SIMP-007 — Capability advertisement by static manifest, no gossip

**Spec target:** `RELIX-5` + `RELIX-6` (gossipsub-based manifest digest propagation).

**Alpha behavior:** On connection establishment, peers exchange full manifests via a `node.manifest` RPC. Manifest changes are observed only on reconnect or explicit re-pull.

**Why:** Gossipsub channels add libp2p complexity; not needed for a 4-node mesh.

**Consequence:** Slow manifest convergence in larger meshes. Fine for 4 nodes.

**Resolution gate:** Gate 2.

## SIMP-008 — No replay-equivalence property test

**Spec target:** `RELIX-7` §7.15 (replay produces identical state to live execution).

**Alpha behavior:** `relix-flow-inspect --replay-verify` checks event log integrity (hash chain, signatures, deserializability). It does NOT re-execute the SOL bytecode to compare states.

**Why:** Building the property test framework requires the full yield model (SIMP-001). Until that lands, replay-equivalence isn't testable.

**Consequence:** We do not catch determinism violations in alpha SOL code. The alpha flows are simple enough to manually verify.

**Resolution gate:** Gate 2 (paired with SIMP-001).

## SIMP-009 — Open WebUI fork is a copy, not a submodule

**Spec target:** None (operational).

**Alpha behavior:** Selected subset of Open WebUI is copied into `relix-web/`. Upstream merges are manual.

**Why:** Submodule complicates the alpha (extra checkout steps, version pinning). We copy only what we need; the strip-and-replace is significant enough that upstream tracking adds little value.

**Consequence:** Loss of automatic upstream merges. Acceptable for alpha.

**Resolution gate:** Re-evaluate at Gate 3 (enterprise pilot).

## SIMP-010 — Tool-call convention is `<tool>...</tool>` text marker

**Spec target:** Not a substrate concern (this is a tool-use UX convention).

**Alpha behavior:** AI replies containing `<tool>web.fetch url="..."</tool>` are detected by the SOL flow, which calls the tool node and re-prompts with the result.

**Why:** Anthropic's real tool-use API is a structured JSON protocol; integrating it cleanly takes a day on its own. The text-marker convention is a one-evening implementation that exercises the architecture (AI → SOL → tool node → AI).

**Consequence:** Brittle parsing. Not production tool-use.

**Resolution gate:** Day 7 of the alpha if time permits; otherwise post-alpha.

## SIMP-011 — Hand-written SOL flows; no SolFlow integration

**Spec target:** SolFlow live mode (Phase 5 of the original roadmap).

**Alpha behavior:** Flows live in `flows/*.sol` as hand-written text.

**Why:** SolFlow live mode requires bidirectional graph↔SOL plus a way to push flows to running controllers. Out of week scope.

**Consequence:** No visual authoring in alpha.

**Resolution gate:** Post-alpha.

## SIMP-012 — No fuzz coverage in alpha CI

**Spec target:** `docs/execution-playbook.md` §2.5 (continuous fuzzing).

**Alpha behavior:** CI runs `fmt`, `clippy`, `test`. Fuzz targets are written but not run on CI.

**Why:** Fuzz infrastructure takes time to stand up; cuts into feature work.

**Consequence:** Parser/decoder edge cases may slip through.

**Resolution gate:** Gate 2.

## SIMP-014 — Synchronous dispatcher (`block_on` bridge)

**Spec target:** RELIX-7 §7.4 (yield opcodes with durable suspension).

**Alpha behavior:** `relix-runtime::flow_runner::RealDispatcher` implements `RemoteCallDispatcher::remote_call` synchronously. The VM runs on `tokio::task::spawn_blocking`, and the dispatcher uses the captured `tokio::runtime::Handle::current().block_on(async { client.call(...).await })` to issue the libp2p RPC. Requires a multi-threaded tokio runtime (the controller binary and `relix-cli` are both configured for that).

**Why:** the full yield/replay-equivalence model from RELIX-7 needs CPS-style compiler restructuring + durable promise state. The synchronous bridge gets a real distributed SOL flow running in one milestone.

**Consequence:** a slow remote call (e.g., approval pending) blocks the flow's VM thread. Many concurrent long-running flows would exhaust the blocking pool. Acceptable for alpha; unacceptable for production.

**Resolution gate:** Gate 2 (paired with SIMP-001).

## SIMP-015 — Client-side flow execution (no `node.run_flow` capability)

**Spec target:** any node can host a SOL flow on behalf of any other.

**Alpha behavior:** SOL flows are compiled and executed by the caller's process (`relix-cli flow-run`), not by a remote `node.run_flow` capability on the target. The dispatcher initiates real outbound RPCs from the caller's libp2p PeerId.

**Why:** a `node.run_flow` capability adds responder-side compilation, identity propagation, and replay-mode VM concerns. The alpha proves the routing-in-SOL invariant without that extra layer.

**Consequence:** flows orchestrated this way have the caller's libp2p PeerId as their initiator. The originating AIC still flows through every per-call `RequestEnvelope` so the responder's policy decisions are unaffected.

**Resolution gate:** post-alpha.

## SIMP-016 — `remote_call` args and returns are UTF-8 strings

**Spec target:** RELIX-1 §1.4 — `args: ByteBuf` (opaque CBOR), capability-typed via CDDL.

**Alpha behavior:** SOL `remote_call(peer: str, method: str, arg: str) -> str`. The SOL string is passed verbatim as the RPC `args` bytes; the responder's `Ok(body)` bytes are decoded as UTF-8 and pushed back as a `HeapObject::String`. Non-UTF-8 responses become a synthetic placeholder string.

**Why:** CDDL-typed args + the CDDL stdlib are Gate-2 substrate work. Strings are sufficient for the M6 demo (the alpha `node.health` capability is rewritten to return a plain `key=value\n` body for this same reason).

**Consequence:** capability authors targeting SOL flows must encode their response as UTF-8 text. The alpha `node.health` does this; future memory / AI / tool nodes will too until typed wire support lands.

**Resolution gate:** Gate 2 (with CDDL stdlib).

## SIMP-017 — Peer aliases via flat `peers.toml`

**Spec target:** RELIX-5 capability advertisement via signed manifests gossipped across the mesh.

**Alpha behavior:** `relix-cli flow-run --peers configs/peers.toml` loads a small flat-TOML map of `alias → libp2p multiaddr`. `remote_call("alias", ...)` resolves to the dialed `PeerId`. Unknown aliases return a structured `RemoteCallError`.

**Why:** signed-manifest gossip is RELIX-5 work; far beyond M6 scope.

**Consequence:** alias maps are not signed and not authenticated. Production deployments must NOT use the flat file; the manifest layer at Gate 2 supersedes it cleanly.

**Resolution gate:** Gate 2 (with manifest gossip).

## SIMP-013 — Single AI provider (Anthropic), single model (RESOLVED)

**Status:** Fully resolved by the M8a provider-agnostic refactor. The `ai.chat` capability now sits behind a `ChatProvider` trait with five real backends:

- `MockProvider` — deterministic; default; no secrets.
- `OpenAICompatibleProvider` — covers `openai`, `openrouter`, `xai`, `local` (Ollama/vLLM/llama.cpp).
- `AnthropicProvider` — native Messages API.
- `GeminiProvider` — placeholder; clean error path until M9+.

Provider selection is one config line (`[ai] provider = "..."`); per-provider settings live in `[ai.providers.<name>]` and the key is loaded from a named env var (`api_key_env`). The web bridge and other presentation peers never hold keys. See `docs/provider-configuration.md`.

## SIMP-019 — Bridge-level SSE chunking (not provider-native token streaming)

**Spec target:** `RELIX-2` full credit-controlled bidirectional substream protocol + `RELIX-7` §7.4 durable yield model (so the AI provider's token stream can flow back through the VM frame-by-frame).

**Alpha behavior:** The chat flow completes fully via the synchronous SOL dispatcher (SIMP-001 + SIMP-014). The bridge then slices the materialised reply into UTF-8-safe chunks and emits them over SSE. Two endpoint shapes exist:

- `POST /chat/stream` — Relix-native (`event: chunk` × N, then `event: done` with a JSON payload carrying `flow_id` / `trace_id` / `flow_log`).
- `POST /v1/chat/completions` with `stream:true` — OpenAI shape (`data: {role…}`, `data: {content…}` × N, `data: {finish_reason:stop, relix:{…}}`, `data: [DONE]`).

Per-chunk size and inter-chunk delay are configurable under `[sse]` in `configs/web-bridge.toml`.

**Why:** True token-by-token streaming requires (a) the AI node's provider implementations to surface a stream, and (b) the SOL VM to yield mid-flow so chunks can traverse `RemoteCall` while the flow is still running. (b) is Gate-2 work paired with SIMP-001. Until then, "stream-shaped" output at the HTTP edge buys real UX value (Open WebUI's typewriter effect works today) at zero substrate cost.

**Consequence:** Latency to first chunk = full flow latency. The UI animates a reply that has already been computed. Documented in `docs/streaming-and-openai-shim.md`.

**Resolution gate:** Gate 2 (with SIMP-001 + SIMP-014 + RELIX-2 substream protocol).

## SIMP-020 — OpenAI-compatible shim is request/response translation only

**Spec target:** None — the OpenAI shape is not a Relix substrate concern. Tracked here because the shim is the smallest stable integration path with Open WebUI.

**Alpha behavior:** `POST /v1/chat/completions` and `GET /v1/models` accept OpenAI-shape JSON and translate to/from a single Relix chat turn:

- **Session id derivation.** OpenAI requests carry the full message history every turn. The bridge derives a stable `session_id` = `oa-<12 hex>` from `blake3(first_system_content || 0x00 || first_user_content)`. As the conversation grows, the prefix stays constant, so subsequent turns land in the same memory bucket on the memory node.
- **Prompt extraction.** Only the *last* `user` message becomes the SOL prompt. Prior history sent by the client is acknowledged but ignored — Relix memory (`memory.recent_for_session`) is the source of truth for what the AI sees.
- **Sanitisation.** Newlines and tabs in user content collapse to single spaces (so the SIMP-018 SOL string boundary holds). `"` and `|` are rejected with 400 because silently rewriting them would change what the user said.
- **Ignored OpenAI fields.** `temperature`, `top_p`, `max_tokens`, `n`, `presence_penalty`, `tool_choice`, `logprobs`, etc. are parsed and dropped — those are provider-side concerns on the AI node.
- **Models endpoint.** `GET /v1/models` returns whatever the operator listed under `[openai_compat.models]` in the bridge config. The bridge does **not** dispatch by model id — provider selection is the AI node's job. Model ids are cosmetic so OpenAI clients show something in their picker.
- **Streaming.** Bridge-level (SIMP-019).
- **No provider key in the bridge.** The shim does not authenticate calls (the `Authorization` header is ignored in alpha). Bind to loopback only.

**Why:** Open WebUI and most "OpenAI-compatible" tools speak this exact shape. A 250-line translation layer in the bridge unlocks them without coupling Relix to any frontend and without forking Open WebUI.

**Consequence:** System messages, multi-modal content, OpenAI tool/function-call payloads, and per-call sampling controls are dropped in the alpha. Power users who want those should target `POST /chat` (native) or wait for typed flow inputs (Gate 2).

**Resolution gate:** Gate 2 (multimodal + structured tool calls); shim itself is permanent.

## SIMP-021 — `tool.web_fetch` SSRF guard: DNS pinned + per-hop redirect re-validation (RESOLVED)

**Spec target:** a tool-node external-action capability that is safe to expose to chat users.

**Status:** fully resolved as of M9-hardening commit. The two original gaps (DNS rebind between guard and connect; cross-hostname redirect re-validation) both closed in code with live tests.

**Alpha behavior:** `tool.web_fetch` runs the SSRF guard in `relix_runtime::nodes::tool::security` before any I/O — scheme allowlist, literal-IP rejection, hostname denylist, full DNS resolution where *every* returned address must be safe. The actual TCP connect is then **pinned** to those validated addresses via `reqwest::ClientBuilder::resolve_to_addrs(host, &[SocketAddr; n])`, so the address the connector dials cannot diverge from the address the guard inspected. The URL retains the hostname so `Host` and TLS SNI keep targeting the original origin.

Redirects are intercepted by a `reqwest::redirect::Policy::custom` closure that (a) enforces `[tool] max_redirects` as a hard cap and (b) re-runs `resolve_safe_url_blocking` (the sync twin of the async guard) on every redirect target. A `Location:` pointing at `127.0.0.1`, an RFC 1918 literal, or a hostname that resolves to a forbidden range is rejected before reqwest follows it.

Verified end-to-end by live tests (`cargo test -p relix-runtime --lib nodes::tool`):
- `pin_forces_connect_to_validated_ip_not_dns` / `pin_to_one_ip_ignores_other_addresses_in_dns` / `unpinned_hostname_fails_dns_proving_pin_is_load_bearing` — DNS pinning.
- `redirect_to_loopback_literal_is_rejected_per_hop` / `redirect_to_rfc1918_literal_is_rejected_per_hop` / `redirect_cap_zero_blocks_any_redirect` — redirect re-validation.

**Why:** `tool.web_fetch` is the first capability that can dial arbitrary outbound endpoints. Without the pin the SSRF guard was advisory — reqwest re-resolved at connect time. Without per-hop re-validation, a `302 Location: http://127.0.0.1/` would have bypassed the guard entirely.

**Consequence (deliberate trades):**
- The tool node maintains a `PinnedClientPool` keyed by `(hostname,
  sorted_validated_addrs)` so repeat fetches against the same safe
  route reuse the same `reqwest::Client` (and its hyper connection
  pool). Earlier cuts of the M9 hardening rebuilt a fresh `Client`
  per request; that gave correctness but burned ~140 ms / fetch on
  TLS + connect setup. The pool restores ~60% of that cost (live
  benchmark: cold 229 ms, warm steady ~90 ms) without weakening any
  guarantee — the cached `Client` only ever serves requests whose
  validated route matches what's pinned inside it. Naive global
  reuse would have collapsed the DNS-pin guarantee; the pool key is
  the validated route itself, so reuse cannot widen the connect set.
- The pool has no LRU eviction in alpha — entries accumulate, soft
  cap 256 (WARN over). Bound is operator-driven (set of hosts the
  operator's flows fetch). LRU lands in a future milestone if needed.
- The redirect policy closure is sync (reqwest's API). DNS validation
  for redirect targets runs synchronously via `std::net::ToSocketAddrs::to_socket_addrs`
  on the calling thread. This briefly blocks one Tokio worker thread
  per redirect. Acceptable because redirects are rare; documented for
  operators on small worker pools.

**Resolution gate:** fully closed today; revisit only if reqwest's redirect machinery changes shape or we move the entire tool path to a streaming pipeline.

## SIMP-018 — Bridge renders SOL flow via template substitution (web bridge → SOL)

**Spec target:** typed SOL flow arguments crossing the wire (RELIX-7 §7.4 yield model + CDDL stdlib).

**Alpha behavior:** `relix-web-bridge`'s `POST /chat` endpoint takes `{session_id, message}` JSON, substitutes the values into `flows/chat_template.sol`'s `{{SESSION}}` / `{{MESSAGE}}` placeholders, writes the rendered SOL to a per-request tempfile, and asks `relix_runtime::flow_runner::FlowRunner` to execute it. The bridge rejects inputs containing `"`, `|`, or `\n` so the substitution stays inside a single SOL string literal.

**Why:** the alpha SOL VM has no flow-arguments mechanism; the FlowRunner takes a `--flow <path>` only. Template substitution is the smallest architecturally honest path from "HTTP request" to "parameterized SOL execution" without inventing a new VM surface.

**Consequence:** the validator forbids three characters in user input. Production typed flow inputs (Gate 2) supersede this.

**Resolution gate:** Gate 2 (with `Inst::FlowArg` opcode + typed CDDL inputs).

---

## How to Add a New Simplification

If during alpha implementation you find a shortcut is needed:

1. Add an entry here with `SIMP-NNN` numbering, gate, why, consequence.
2. Add a `// TODO(SIMP-NNN):` comment in the code at the point of simplification.
3. Mention SIMP-NNN in the PR description.
4. Do not commit the code change until SIMP-NNN exists.
