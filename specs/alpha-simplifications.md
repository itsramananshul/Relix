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

## SIMP-013 — Single AI provider (Anthropic), single model

**Spec target:** Provider-agnostic `ai.chat` capability.

**Alpha behavior:** AI node hardcodes Anthropic API endpoint and one model. Capability surface (`ai.chat`) is provider-agnostic; the implementation isn't.

**Why:** Multi-provider abstraction has no demo value when only one provider is wired.

**Consequence:** Adding OpenAI or Ollama post-alpha is a per-provider implementation, not an architectural change.

**Resolution gate:** Post-alpha (Phase 3 territory).

---

## How to Add a New Simplification

If during alpha implementation you find a shortcut is needed:

1. Add an entry here with `SIMP-NNN` numbering, gate, why, consequence.
2. Add a `// TODO(SIMP-NNN):` comment in the code at the point of simplification.
3. Mention SIMP-NNN in the PR description.
4. Do not commit the code change until SIMP-NNN exists.
