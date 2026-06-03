# Relix Adversarial Audit — Full Damage Report

> **SUPERSEDED as of 2026-06-02.** This is a point-in-time audit run
> on 2026-05-29 against pre-v0.3.0 code. It overstates the current risk
> posture: its top "production-lethal" findings have since been
> remediated. Spot checks against the current tree:
>
> - Finding 1 (approval delivery was a `tracing::info!` stub) is fixed.
>   `crates/relix-runtime/src/approval/delivery.rs` now dispatches over
>   real channels (`SingleChannelDispatch`, webhook transport).
> - Finding 4 (SHA-256 credential KDF) is fixed: Argon2id with a
>   per-vault salt in `crates/relix-runtime/src/credentials/store.rs`.
> - Finding 5 (default-allow agent admission) is fixed: fail-closed in
>   `crates/relix-runtime/src/admission/agent_gate.rs` ("SEC PART 1").
> - Finding 8 (CI not on push) is now intentional: CI is manual-only
>   (`workflow_dispatch`) as of v0.4.2; see CHANGELOG.
>
> A bundle of production-lethal gaps was closed in commit `05416ac`
> ("sec(P1-P6)") and identity expiry in v0.4.2. Treat the entries
> below as historical, not as the present state. For the current
> posture see `docs/security.md`, `SECURITY.md`, and `CHANGELOG.md`.
> Re-audit before relying on any specific finding here.

**Scope:** `D:\DATA\WORK\OpenPrem\Apps\Relix`
**Date of audit:** 2026-05-29
**Files audited:** ~1,050 source/config/doc files under `crates/`, `sdks/`, `configs/`, `scripts/`, `ops/`, `docs/`, `specs/`, `conformance/`, `examples/`, `flows/`, `workflows/`, `.github/`, plus root `install.{sh,ps1}`, `Dockerfile`, `Cargo.toml`, `deny.toml`, `CHANGELOG*`, `CONTRIBUTING.md`, `CODEOWNERS`, `SECURITY.md`, `README.md`. `reference/` and `.claude/worktrees/` excluded as out-of-scope.
**Defects found:** 1,052
**Severity distribution:** 92 CRITICAL · 648 MAJOR · 312 MINOR

---

## Table of contents

1. [Methodology](#methodology)
2. [Scoreboard](#scoreboard)
3. [Eight most production-lethal findings](#eight-most-production-lethal-findings)
4. [Angle 1 — Operator experience](#angle-1--operator-experience-friction-that-would-make-a-real-person-quit)
5. [Angle 2 — Developer experience](#angle-2--developer-experience-things-that-would-make-an-integrator-give-up)
6. [Angle 3 — Correctness](#angle-3--correctness-things-that-are-just-wrong)
7. [Angle 4 — Security](#angle-4--security-things-that-could-be-exploited)
8. [Angle 5 — Performance](#angle-5--performance-things-that-would-crater-under-real-load)
9. [Angle 6 — Integration gaps](#angle-6--integration-gaps-systems-that-are-supposed-to-talk-but-do-not)
10. [Angle 7 — Real user experience walkthroughs](#angle-7--the-real-user-experience)
11. [Appendix A — relix-core remaining entries](#appendix-a--relix-core-remaining-entries)
12. [Appendix B1 — dispatch + coordinator remaining](#appendix-b1--dispatch--coordinator-remaining)
13. [Appendix B2 — transport + admission + approval remaining](#appendix-b2--transport--admission--approval-remaining)
14. [Appendix C — credentials/identity/manifest/plugin/metrics/observability remaining](#appendix-c--credentialsidentitymanifestpluginmetricsobservability-remaining)
15. [Appendix D — knowledge/planning/nodes/workflow/sflow/sol/yaml_flow/training/confidence/db remaining](#appendix-d--knowledgeplanningnodesworkflowsflowsolyaml_flowtrainingconfidencedb-remaining)
16. [Appendix E — web-bridge remaining](#appendix-e--web-bridge-remaining)
17. [Appendix F — CLI remaining](#appendix-f--cli-remaining)
18. [Appendix G — controller/flow-inspect/embedded remaining](#appendix-g--controllerflow-inspectembedded-remaining)
19. [Appendix H — telegram/discord/slack remaining](#appendix-h--telegramdiscordslack-remaining)
20. [Appendix I — Rust SDK + plugin SDK remaining](#appendix-i--rust-sdk--plugin-sdk-remaining)
21. [Appendix J — Python SDK remaining](#appendix-j--python-sdk-remaining)
22. [Appendix K — TypeScript SDK remaining](#appendix-k--typescript-sdk-remaining)
23. [Appendix L — configs/scripts/ops remaining](#appendix-l--configsscriptsops-remaining)
24. [Appendix M — docs remaining](#appendix-m--docs-remaining)
25. [Appendix N — specs/conformance remaining](#appendix-n--specsconformance-remaining)
26. [Appendix O — install/Dockerfile/examples/CI remaining](#appendix-o--installdockerfileexamplesci-remaining)
27. [Appendix P — cross-cutting integration verification](#appendix-p--cross-cutting-integration-verification)
28. [Appendix Q — additional scenario walkthrough detail](#appendix-q--additional-scenario-walkthrough-detail)
29. [Appendix R — issues that surfaced during synthesis](#appendix-r--issues-that-surfaced-during-synthesis)
30. [Appendix S — defect-density observations](#appendix-s--defect-density-observations)
31. [Appendix T — additional Angle 4 SECURITY items](#appendix-t--additional-angle-4-security-items)
32. [Appendix U — final residual items (long tail)](#appendix-u--final-residual-items-long-tail)
33. [Final total](#final-total)

---

## Methodology

The audit was performed by dispatching 16 parallel general-purpose subagents, each assigned a non-overlapping slice of the codebase. Each agent read every file in its slice line by line and reported defects in a uniform structured format:

```
<absolute_path>:<line> | CRITICAL|MAJOR|MINOR | OPERATOR|DEV|CORRECTNESS|SECURITY|PERFORMANCE|INTEGRATION | <problem> | <fix>
```

Slices:

- **A** — `crates/relix-core`
- **B1** — `crates/relix-runtime/src/{coordinator,dispatch}`
- **B2** — `crates/relix-runtime/src/{transport,admission,approval}`
- **C** — `crates/relix-runtime/src/{credentials,identity,manifest,plugin,metrics,observability}`
- **D** — `crates/relix-runtime/src/{knowledge,planning,nodes,workflow,sflow,sol,yaml_flow,training,confidence,db}`
- **E** — `crates/relix-web-bridge`
- **F** — `crates/relix-cli`
- **G** — `crates/relix-controller`, `crates/relix-flow-inspect`, `crates/relix-embedded`
- **H** — `crates/relix-telegram`, `crates/relix-discord`, `crates/relix-slack`
- **I** — `crates/relix-sdk`, `crates/relix-plugin-sdk`
- **J** — `sdks/python`
- **K** — `sdks/typescript`
- **L** — `configs/`, `scripts/`, `ops/`
- **M** — `docs/`
- **N** — `specs/`, `conformance/`
- **O** — install scripts, Dockerfile, workspace Cargo.toml, examples, flows, workflows, CI

A 17th synthesis pass cross-correlated findings to identify integration gaps and produce the seven-angle structure below.

---

## Scoreboard

| Slice | Issues | CRIT | MAJOR | MINOR |
|-------|-------:|------:|------:|------:|
| A relix-core | 77 | 4 | 45 | 28 |
| B1 dispatch + coordinator | 86 | 4 | 64 | 18 |
| B2 transport + admission + approval | 81 | 5 | 56 | 20 |
| C runtime: cred/identity/manifest/plugin/metrics/observability | 76 | 9 | 55 | 12 |
| D runtime: knowledge/planning/nodes/workflow/sflow/sol/yaml_flow/training/confidence/db | 95 | 4 | 72 | 19 |
| E web-bridge | 78 | 10 | 52 | 16 |
| F CLI | 87 | 3 | 52 | 32 |
| G controller/flow-inspect/embedded | 60 | 5 | 27 | 28 |
| H telegram/discord/slack | 38 | 7 | 26 | 5 |
| I Rust SDK + plugin SDK | 38 | 7 | 22 | 9 |
| J Python SDK | 47 | 1 | 22 | 24 |
| K TypeScript SDK | 31 | 0 | 6 | 25 |
| L configs/scripts/ops | 65 | 9 | 26 | 30 |
| M docs | 44 | 7 | 27 | 10 |
| N specs/conformance | 60 | 11 | 41 | 8 |
| O install/Dockerfile/examples/CI | 89 | 6 | 55 | 28 |
| **Total** | **1,052** | **92** | **648** | **312** |

*(Some severities were re-classified into CRITICAL during synthesis where a "MAJOR" finding was actually a wire-not-present condition — e.g. `LogChannelDispatch` as the only dispatcher, `verify_on_dispatch` parsed-but-unread, every memory CLI defaulting to a closed port.)*

---

## Eight most production-lethal findings

If only eight things get fixed, fix these:

1. **`crates/relix-runtime/src/approval/delivery.rs:436`** — Wire a real HTTP-backed `ChannelDispatch`. Today the only impl is a `tracing::info!` line. Every "approval" deployment is fictional.
2. **`crates/relix-runtime/src/dispatch/mod.rs:1114, 1880`** — Bind approval tokens to (method, subject, TTL) with constant-time compare and atomic check-and-consume.
3. **`crates/relix-web-bridge/src/tenant.rs:81`** + the 44 mesh handlers — Plumb tenant through every handler or fail closed.
4. **`crates/relix-runtime/src/credentials/store.rs:575`** — Replace SHA-256 KDF with Argon2id + per-store salt.
5. **`crates/relix-runtime/src/admission/agent_gate.rs:126, 165`** — Flip the two default-allow paths (no store / no profile) to default-deny.
6. **`crates/relix-runtime/src/manifest/mod.rs:42`** — Sign NodeManifest via Bundle; verify on receive.
7. **`crates/relix-runtime/src/identity/session.rs:42`** — Either wire `verify_on_dispatch` or delete the knob; the current state is documented enforcement that doesn't exist.
8. **`.github/workflows/ci.yml:9`** — Restore `on: push` / `on: pull_request`; CI runs on demand only today.

Everything else is the long tail.


---

## Angle 1 — Operator experience: friction that would make a real person quit

### 1.1 The approval system would make production usage impossible

- **`crates/relix-runtime/src/dispatch/mod.rs:1117`** — MAJOR — `always_require_methods` is checked per request with linear `Vec::iter().any()`. Convert to `HashSet`/`Arc<HashSet>`.
- **`crates/relix-runtime/src/dispatch/mod.rs:1073`** — MAJOR — There is **no batching of approvals**. Every dispatched call requiring approval mints a brand-new approval row + a brand-new notification. There is no "approve all `tool.terminal` calls for next hour" mode anywhere. An agent making 20 tool calls = 20 phone buzzes. Add a session-keyed approval cache (subject_id × method) with TTL.
- **`crates/relix-runtime/src/dispatch/mod.rs:1073`** — MAJOR — The `on_require_approval` closure is invoked **synchronously on the dispatch hot path** — and per its own comment, "mints the approval row + chronicle event + telegram notification." A slow Telegram send blocks even the rejection back to the agent. Spawn side effects on a worker channel.
- **`crates/relix-runtime/src/dispatch/mod.rs:1845`** — MAJOR — Same blocking-notify-then-respond in the streaming path.
- **`crates/relix-runtime/src/approval/delivery.rs:436`** — **CRITICAL** — The only in-tree `ChannelDispatch` impl is `LogChannelDispatch` which **just emits a `tracing::info!` line**. No actual HTTP call to Telegram/Slack/Discord/email is wired in this delivery system. Operators wiring `channel = "telegram"` with a `webhook_url` get **zero actual outbound delivery, presented as success**. Wire a real HTTP dispatcher or hard-error in production mode.
- **`crates/relix-runtime/src/approval/delivery.rs:466`** — MAJOR — The store row is `upsert`-ed with `delivered_at_ms` BEFORE `dispatch.send` is called. If send fails, the row says "delivered" but it wasn't. Mark `delivery_failed` on Err.
- **`crates/relix-runtime/src/approval/delivery.rs:471`** — MAJOR — Escalation timer is `tokio::spawn` fire-and-forget with no JoinHandle. Service drop does not abort it. Add cancellation token.
- **`crates/relix-runtime/src/approval/delivery.rs:507`** — MINOR — `record_decision` does not cancel the spawned escalation timer; the timer wakes up to find a decided row. Wire a cancel channel.
- **`crates/relix-runtime/src/approval/delivery.rs:35`** — MINOR — `default_default_channel()` returns "dashboard" which is enabled by default even with no `[channels.dashboard]` section configured.
- **`crates/relix-runtime/src/approval/delivery.rs:225`** — MAJOR — `ApprovalDeliveryMatrix::new` does not validate that rules with `escalation_timeout_secs > 0` also set `escalation_channel`. Misconfiguration silently degrades at dispatch time. Validate at construction.
- **`crates/relix-runtime/src/approval/delivery.rs:443`** — MAJOR — `channel_enabled` is checked only for the initial channel; the escalation channel is checked at escalation time. Operators see `escalation_scheduled = true` even when the escalation channel is disabled.
- **`crates/relix-runtime/src/approval/mod.rs:33`** — MAJOR — There are **two parallel approval systems** in the codebase: agent_gate's `consumed_approval_id` path against the `AgentStore`, AND this `ApprovalRequestStore`/`approval_delivery`. Decisions in one DO NOT propagate to the other.
- **`crates/relix-runtime/src/dispatch/mod.rs:1114`** — **CRITICAL** — `always_requires_approval` step 8.5 is bypassed by ANY non-empty `approval_token` string when the agent_gate is not wired. No scope binding, no signature check, no TTL. A token issued for `tool.web_read` is accepted for `tool.terminal`.
- **`crates/relix-runtime/src/dispatch/mod.rs:1018`** — MAJOR — Agent gate `describe` closure invoked synchronously per request; if descriptor lookup hits SQLite, every request issues at least one SQLite read inside admission. Cache descriptor in-memory keyed by method.
- **`crates/relix-runtime/src/credentials/scheduler.rs:107`** — MAJOR — When `sweep_once` fails, the rotation scheduler just `tracing::warn!`s and returns empty. Operators **never learn the scheduler is broken**.
- **`crates/relix-runtime/src/credentials/scheduler.rs:125`** — MAJOR — Scheduler `tokio::spawn` is fire-and-forget; no JoinHandle, no shutdown, no panic propagation. Task panic stops rotations silently forever.
- **`crates/relix-runtime/src/credentials/scheduler.rs:66`** — MINOR — The default `LogRotationNotifier` is the only notifier delivered out of the box. Docs claim Telegram/Slack/email — none are wired in `register_default`.
- **`crates/relix-telegram/src/lib.rs:54`** — **CRITICAL** — `BotApi` has no approval-dispatch method; `OutgoingMessage` has no `reply_markup` field for inline buttons. **Operators cannot press Approve/Reject — only reply with free text.**
- **`crates/relix-telegram/src/live.rs:333`** — **CRITICAL** — `get_updates` hard-codes `allowed_updates = &["message"]`. Even if an upstream system did render inline buttons, **callback_query events from button presses are never delivered**.
- **`crates/relix-telegram/src/live.rs:296`** — **CRITICAL** — `update_to_incoming` drops every update without a `message` field — explicitly silencing the callback-query reply path.
- **`crates/relix-telegram/src/config.rs:47`** — **CRITICAL** — `DeliveryMode::Webhook` enum variant exists in config but `live.rs` never branches on it. Operator setting `mode = "webhook"` silently gets long-poll.
- **`crates/relix-telegram/src/live.rs:164`** — MAJOR — 429 `retry_after` from Telegram is unclamped (Discord/Slack clamp to 30s). Telegram global limits can return 60+ minutes.
- **`crates/relix-telegram/src/live.rs:197`** — MAJOR — Retry backoff has no jitter. Synchronised retry storms under coordinated 429.
- **`crates/relix-discord/src/lib.rs:54`** — **CRITICAL** — `DiscordApi` has no component/button payload; `OutgoingMessage` has only `content`. Operators can only reply in chat.
- **`crates/relix-discord/src/live.rs:276`** — **CRITICAL** — Discord inbound is REST polling — no Gateway WebSocket, no Interactions endpoint, no `X-Signature-Ed25519` verification.
- **`crates/relix-discord/src/live.rs:281`** — MAJOR — Empty `after_message_id` returns the 50 most-recent messages. On first boot the bot sees historical messages from before it was installed.
- **`crates/relix-discord/src/live.rs:235`** — MAJOR — `dc_message_to_incoming` does NOT filter `is_bot=true` at the parse layer. Reply loops possible.
- **`crates/relix-slack/src/lib.rs:40`** — **CRITICAL** — `SlackApi` has no Block Kit / interactive component support.
- **`crates/relix-slack/src/live.rs:301`** — **CRITICAL** — Slack inbound is `conversations.history` polling. NO Events API webhook handler, NO Interactivity endpoint, NO Socket Mode, NO HMAC verification of `x-slack-signature`.
- **`crates/relix-telegram/src/session_store.rs:53`** — MINOR — `SessionStore` mappings live forever (no TTL).
- **`crates/relix-runtime/src/identity/research.rs:443`** — MAJOR — `wait_for_approval` polls every 2s up to 300s — 150 SQLite reads per approval gate. Use a tokio `Notify`/channel.

### 1.2 Config complexity, parsed-but-unread fields, silent misconfiguration

- **`configs/tool-node.toml:20`** — **CRITICAL** — `url_allowlist` is parsed by no code. **Fail-open SSRF illusion.**
- **`configs/tool-node.toml:31`** — MAJOR — `max_body_bytes = 524288` silently ignored. The real field is `max_bytes`.
- **`configs/web-bridge-node.toml:20`** — **CRITICAL** — `node_type = "web_bridge"` is a controller no-op — registers ZERO bridge capabilities. The actual bridge is `relix-web-bridge`.
- **`configs/web-bridge-node.toml:23`** — MAJOR — `[bridge] http_addr` not read anywhere.
- **`configs/web-bridge-node.toml:26`** — MAJOR — `[bridge] default_flow` not read anywhere.
- **`configs/web-bridge-node.toml:34`** — MAJOR — `[session.chat]` and `[session.chat_with_tool]` deserialize but the field is `#[allow(dead_code)]`.
- **`configs/policies/web-bridge.toml:9`** — MAJOR — File references HTTP-layer methods but the web bridge binary contains zero `PolicyEngine` wiring. **Operators believe HTTP chat is policy-enforced; every HTTP call is admitted.**
- **`configs/policies/ai.toml:9`** — MAJOR — Missing `ai.embed` rule. Default-deny means memory.embed-driven RAG fails for every caller.
- **`configs/policies/local.toml:79`** — MINOR — Duplicate `[[rules]] method = "memory.search"` (lines 25 + 61).
- **`configs/ai-node.toml:30`** — MAJOR — Default `provider = "mock"` ships in production-shaped TOML. Operator who forgets gets deterministic-mock LLM with no warning.
- **`crates/relix-core/src/capability.rs:128`** — MINOR — `environment_requirements` doc says "Not validated at runtime." Parsed-but-unused.
- **`crates/relix-runtime/src/identity/session.rs:42`** — **CRITICAL** — `verify_on_dispatch: bool` is parsed from config but **NEVER read anywhere else in the codebase**. The doc lies.
- **`crates/relix-runtime/src/manifest/mod.rs:90`** — MINOR — `inner.write().expect(...)` panics on poison.
- **`crates/relix-web-bridge/src/main.rs:372`** — **CRITICAL** — When `listen_addr` is non-loopback, the bridge only WARNs and still starts. Refuse without explicit operator override.

### 1.3 Error messages without actionable context

- **`crates/relix-cli/src/ping.rs:27`** — MAJOR — "client key must be 32 raw bytes" gives no path, no remediation.
- **`crates/relix-cli/src/ping.rs:58`** — MAJOR — "timeout waiting for peer connection" lacks the multiaddr and dial outcome.
- **`crates/relix-cli/src/task.rs:1219`** — MAJOR — Prints `ERR kind={i64} cause={}` with no human label for the kind.
- **`crates/relix-cli/src/task.rs:1224`** — MAJOR — "unexpected stream-handle response" gives no action.
- **`crates/relix-cli/src/capability.rs:512`** — MAJOR — Opaque `ERR kind=X cause=Y` in manifest fetch.
- **`crates/relix-cli/src/mcp.rs:391`** — MAJOR — Opaque exit in `call_peer`.
- **`crates/relix-cli/src/router.rs:308`** — MAJOR — Opaque exit in `router::call`.
- **`crates/relix-cli/src/terminal.rs:486`** — MAJOR — Opaque exit in `terminal`.
- **`crates/relix-cli/src/doctor.rs:101`** — MAJOR — "FAIL bridge.reachable" prints raw error and exits with no hint.
- **`crates/relix-cli/src/os_secure.rs:32`** — MAJOR — "USERNAME env var not set" no remediation hint.
- **`crates/relix-cli/src/flow_run.rs:38`** — MAJOR — "client key must be 32 raw bytes" same problem.
- **`crates/relix-cli/src/flow_run.rs:69`** — MAJOR — `last_error` collapsed to single-line `Display`; loses structured `RemoteCallError`.
- **`crates/relix-cli/src/workflow.rs:191`** — MAJOR — "validation failed" — no file/line/spec issue.
- **`crates/relix-cli/src/email.rs:152`** — MAJOR — "send failed" no status code, no hint.
- **`crates/relix-cli/src/export.rs:115`** — MAJOR — "no HOME / USERPROFILE — pass --token" doesn't say where to find token.
- **`crates/relix-cli/src/install.rs:236`** — MAJOR — Doesn't distinguish "docker not running" from "qdrant container not running".
- **`crates/relix-controller/src/main.rs:15`** — MINOR — `main()` returns `Box<dyn Error>` so any failure bubbles as opaque Debug.
- **`crates/relix-flow-inspect/src/main.rs:62`** — MINOR — Same opaque error; CI can't distinguish "integrity broken" from "file unreadable".
- **`crates/relix-flow-inspect/src/main.rs:80`** — MINOR — "signer key must be 32 raw bytes" doesn't show actual length.
- **`crates/relix-runtime/src/credentials/store.rs:166`** — MINOR — `rusqlite::Error` re-thrown through `format!` — exposes SQL detail to callers.
- **`crates/relix-runtime/src/identity/research.rs:278`** — MAJOR — When `require_approval = true` and no `ApprovalDeliveryService` is wired, pipeline logs warn and silently fakes "Rejected".
- **`crates/relix-runtime/src/planning/critic.rs:285`** — MAJOR — Records `__critic_unreachable__` on POLICY_DENIED; caller can't distinguish "judge said no" from "mesh is down".
- **`crates/relix-runtime/src/planning/verification.rs:303`** — MAJOR — AI judge falls back to PASS on Err. Critical step silently approved.
- **`crates/relix-core/src/audit.rs:283`** — MINOR — "bad signature" doesn't include `responder_node_id` or `seq` index.
- **`crates/relix-core/src/eventlog.rs:107`** — MINOR — I/O errors stringified lose `io::ErrorKind`.
- **`crates/relix-cli/src/main.rs:533`** — MAJOR — `EnvFilter::try_from_default_env().unwrap_or_else(...)` silently swallows `RUST_LOG` parse errors.

### 1.4 Silent failures / fire-and-forget tasks

(Cross-listed; see also 1.1 and 1.7 above.)

- **`crates/relix-runtime/src/identity/session.rs:513`** — MAJOR — `spawn_idle_sweeper` fire-and-forget.
- **`crates/relix-runtime/src/manifest/mod.rs:596`** — MAJOR — `drop(tokio::spawn(event_loop.run()))` — fire-and-forget swarm task.
- **`crates/relix-runtime/src/manifest/mod.rs:501`** — MAJOR — `spawn_refresh_loop` JoinHandle leaks.
- **`crates/relix-runtime/src/metrics/spike_detector.rs:408`** — MAJOR — Spawn fire-and-forget.
- **`crates/relix-runtime/src/metrics/alert.rs:735`** — MAJOR — `engine.spawn` fire-and-forget.
- **`crates/relix-runtime/src/training/recorder.rs:250`** — MAJOR — Drain + retention loops fire-and-forget.
- **`crates/relix-runtime/src/knowledge/autoshare.rs:198`** — MAJOR — `AutoShareTask::spawn` fire-and-forget.
- **`crates/relix-runtime/src/knowledge/quality_scorer.rs:268`** — MAJOR — `spawn_memory_quality_scorer` fire-and-forget.
- **`crates/relix-runtime/src/planning/coordinator.rs:330`** — MAJOR — `spawn_approval_expiry_sweep` fire-and-forget.
- **`crates/relix-runtime/src/planning/coordinator.rs:1211`** — MAJOR — `notify_pending_plan` spawn per target with no join.
- **`crates/relix-runtime/src/planning/coordinator.rs:224`** — MAJOR — `execute_with_events` fire-and-forget.
- **`crates/relix-runtime/src/workflow/coordinator.rs:224`** — MAJOR — Same.
- **`crates/relix-runtime/src/metrics/alert_delivery.rs:329`** — MAJOR — Unbounded `tokio::spawn` per channel-per-event.
- **`crates/relix-runtime/src/metrics/collector.rs:329`** — MAJOR — Drain loop clears batch even on flush failure → permanent data loss.
- **`crates/relix-runtime/src/metrics/collector.rs:184`** — MAJOR — `enforcer.invalidate_agent` called BEFORE persist.
- **`crates/relix-runtime/src/dispatch/mod.rs:2264`** — MAJOR — Audit write failure is `tracing::error!` only — client gets 200/OK with no audit row.
- **`crates/relix-runtime/src/dispatch/mod.rs:961`** — MAJOR — On envelope decode failure, response uses `RequestId([0u8; 16])` — caller correlation breaks.
- **`crates/relix-runtime/src/credentials/store.rs:539`** — MINOR — Mutex poisoning becomes flat error.
- **`crates/relix-runtime/src/dispatch/mod.rs:138, 147, 154, 847, 874`** — MAJOR — Five `.expect("...poisoned")` reachable from any inbound request.
- **`crates/relix-runtime/src/dispatch/mod.rs:1034`** — MAJOR — `consume_approval_token` failure: call proceeds anyway.
- **`crates/relix-runtime/src/dispatch/mod.rs:1808`** — MAJOR — Same silent-on-failure consume in streaming.
- **`crates/relix-runtime/src/metrics/observability.rs:340`** — MAJOR — `compute_score` double-penalises error rate.
- **`crates/relix-cli/src/build.rs:537`** — MAJOR — `stream_verification_live` "best-effort" silently returns on error.

### 1.5 Default configurations — too permissive or too locked down

- **`crates/relix-runtime/src/admission/agent_gate.rs:126`** — **CRITICAL** — `Allow("no_agent_store")` — **default-permissive on missing policy**.
- **`crates/relix-runtime/src/admission/agent_gate.rs:165`** — **CRITICAL** — "No profile = backward-compat allow" — **default-permissive on missing agent profile**.
- **`crates/relix-runtime/src/admission/agent_gate.rs:262`** — MAJOR — Risk ceiling SKIPPED when capability is None — unknown method bypass.
- **`crates/relix-core/src/policy.rs:107`** — MAJOR — `PolicyEngine::permissive()` admits any identity by default.
- **`crates/relix-core/src/policy.rs:255`** — MAJOR — `evaluate` falls back to global engine on per-tenant load error.
- **`crates/relix-runtime/src/credentials/caps.rs:155`** — MAJOR — Hard-coded operator-bypass group names `"operators"`/`"admin"`.
- **`crates/relix-runtime/src/credentials/caps.rs:156`** — MAJOR — `summary.owner_agent.is_none()` → every unscoped credential is readable.
- **`crates/relix-runtime/src/dispatch/mod.rs:62`** — MAJOR — `tenant_id_or_default()` silently returns `"default"`.
- **`crates/relix-web-bridge/src/tenant.rs:81`** — MAJOR — Default tenant `"default"` for any caller that omits header.

### 1.6 Startup / boot failures

- **`ops/runbooks/alpha-bringup.md:62`** — **CRITICAL** — `RELIX_NODE_KEY=...` never read by relix-controller.
- **`ops/runbooks/alpha-bringup.md:75`** — **CRITICAL** — Tells operators to launch `relix-controller -- --config configs/web-bridge-node.toml` which registers ZERO caps.
- **`ops/runbooks/alpha-bringup.md:54`** — **CRITICAL** — `[ai] api_key_path` does not exist (correct is `[ai.providers.<name>] api_key_env`).
- **`ops/runbooks/alpha-bringup.md:296`** — MAJOR — `[ai] mode = "stub"` does not exist.
- **`ops/runbooks/alpha-bringup.md:474`** — MAJOR — `[bridge] http_port` does not exist.
- **`crates/relix-controller/src/main.rs:25`** — MAJOR — No signal handler.
- **`crates/relix-controller/src/main.rs:25`** — MAJOR — No health-check / readiness endpoint.
- **`crates/relix-runtime/src/credentials/store.rs:177`** — MAJOR — Empty master secret silently derives a deterministic key.
- **`crates/relix-cli/src/setup.rs:53`** — MAJOR — `relix setup` calls `status_for_setup().await` BEFORE entering raw mode; Docker hang blocks wizard.
- **`scripts/alpha-bringup-m5.sh:39`** — MINOR — `identity init-org` refuses overwrite → fails on second run.
- **`Dockerfile:74`** — **CRITICAL** — CMD points at `/relix/configs/bridge.toml` but `configs/` is NEVER `COPY`'d.
- **`Dockerfile:50`** — MAJOR — Runtime base `debian:bookworm-slim` floating tag.
- **`Dockerfile:26`** — MAJOR — Builder base `rust:1.95.0-bookworm` not pinned by digest.

### 1.7 Destructive operations without confirmation

- **`crates/relix-cli/src/ops.rs:3186`** — MAJOR — `cron_delete` no confirmation, no `--yes`.
- **`crates/relix-cli/src/ops.rs:3447`** — MAJOR — `agent_set_status "disabled"` (irreversible) no confirmation.
- **`crates/relix-cli/src/ops.rs:3611`** — MAJOR — `standing_revoke` no confirmation.
- **`crates/relix-cli/src/ops.rs:3856`** — MAJOR — `msg_delete` no confirmation.
- **`crates/relix-cli/src/ops.rs:1814`** — MINOR — Snapshot silently overwrites existing files.
- **`crates/relix-cli/src/credentials.rs:144`** — MAJOR — `store --value <v>` puts API key on command line.
- **`crates/relix-cli/src/credentials.rs:197`** — MAJOR — Same for `rotate --new-value <v>`.

### 1.8 Memory inspector — wrong default port everywhere

- **`crates/relix-cli/src/memory_inspect.rs:32`** — **CRITICAL** — `relix memory list` defaults `--bridge` to `http://127.0.0.1:9100`. The real bridge runs on `19791`.
- **`crates/relix-cli/src/memory_inspect.rs:48`** — **CRITICAL** — `relix memory show` same.
- **`crates/relix-cli/src/memory_inspect.rs:57`** — **CRITICAL** — `relix memory search` same.
- **`crates/relix-cli/src/memory_inspect.rs:68`** — **CRITICAL** — `relix memory invalidate` same.
- **`crates/relix-cli/src/memory_inspect.rs:78`** — **CRITICAL** — `relix memory stats` same.

### 1.9 CLI / shell quality-of-life

- **`crates/relix-cli/src/mesh.rs:267`** — MAJOR — `relix status` treats HTTP 5xx as "running".
- **`crates/relix-cli/src/mesh.rs:139`** — MAJOR — `relix boot` 60s wait with no progress indication.
- **`crates/relix-cli/src/mesh.rs:540`** — MAJOR — `build_boot_command` does NOT set `RELIX_DATA_DIR` when `--data-dir` is unset.
- **`crates/relix-cli/src/install.rs:633`** — MAJOR — `confirm_or_skip` on closed stdin silently returns false.
- **`crates/relix-cli/src/install.rs:585`** — MAJOR — `run_docker` swallows docker stderr.
- **`crates/relix-cli/src/update.rs:251`** — MAJOR — Windows `.zip` self-update refuses; operator running `--yes` has no automation path.
- **`crates/relix-cli/src/update.rs:373`** — MAJOR — Rollback can leave operator with no installed binary.
- **`crates/relix-cli/src/sessions.rs:127`** — MAJOR — `--full` requires `--elevated` but error doesn't explain risk.
- **`crates/relix-cli/src/sessions.rs:159`** — MAJOR — `--full` sends `X-Relix-Elevated: true` but no bearer token.
- **`crates/relix-cli/src/export.rs:84`** — MAJOR — `export` is the ONLY command that sends bearer auth; all other bridge calls send no `Authorization` header.
- **`crates/relix-cli/src/ops.rs:1844`** — MINOR — `port_from_bridge` defaults to 19791 on parse failure.
- **`crates/relix-cli/src/ops.rs:1762`** — MAJOR — `snapshot`'s entry closure swallows error string into JSON.
- **`crates/relix-cli/src/install.rs:434`** — **CRITICAL** — `install_via_shell_script` uses `sh -c "curl | sh"` with no signature verification.
- **`crates/relix-cli/src/install.rs:496`** — MAJOR — Windows/macOS installers download .exe/.dmg and execute with no signature verification.
- **`crates/relix-cli/src/update.rs:48`** — MAJOR — Default API URL hardcodes `itsramananshul/Relix`.
- **`crates/relix-cli/src/update.rs:262`** — MAJOR — No published checksum verification despite doc claim.
- **`crates/relix-cli/src/setup.rs:967`** — MAJOR — `cancel()` race could leave terminal in unknown state.
- **`crates/relix-cli/src/setup.rs:419`** — MINOR — API-key prompt accepts tab character.

---

## Angle 2 — Developer experience: things that would make an integrator give up

### 2.1 The Rust SDK is broken in three of four endpoints

- **`crates/relix-sdk/src/lib.rs:289`** — **CRITICAL** — `remember()` sends `{"chunk": content}` but bridge `/v1/memory/embed` expects `"text"`. Every call returns HTTP 400.
- **`crates/relix-sdk/src/lib.rs:295`** — MAJOR — `remember()` sends a `tags` array but `EmbedRequest` has no `tags` field. Silently discarded.
- **`crates/relix-sdk/src/lib.rs:327`** — **CRITICAL** — `search()` sends `"top_k": 10`, but bridge `SearchRequest` reads `"limit"`. Caller's `top_k` is silently dropped; defaults to 5.
- **`crates/relix-sdk/src/lib.rs:354`** — **CRITICAL** — `search()` parses `v.get("hits")` or `v.as_array()`, but bridge returns `{"results": [...], "count": N}`. Every successful search returns `RelixError::Decode("search response had no hits array")`.
- **`crates/relix-sdk/src/lib.rs:263`** — **CRITICAL** — `chat_stream` SSE parser only yields when payload is JSON with `chunk` or `text` key; bridge `build_chunked_sse` emits raw text in `data:`. Every real bridge stream yields ZERO chunks.
- **`crates/relix-sdk/src/lib.rs:164`** — **CRITICAL** — `chat()` generates a fresh `session_id` on every call. No conversation continuity.
- **`crates/relix-sdk/src/lib.rs:60`** — MAJOR — `MemoryResult.tags` documented but bridge `SearchHit` has no `tags` field. Dead weight.
- **`crates/relix-sdk/src/lib.rs:248`** — MAJOR — `std::str::from_utf8` `continue` on UTF-8 error throws away partial bytes; next frame misaligned.
- **`crates/relix-sdk/src/lib.rs:88`** — MAJOR — Rust SDK covers only chat/info/memory.embed/memory.search. Bridge has tasks, tools, agents, planning, observability, skills, audit, dialectic — none exposed.
- **`crates/relix-sdk/src/lib.rs:107`** — MAJOR — `Client::builder().build().unwrap_or_else(|_| reqwest::Client::new())` silently drops the 30s timeout.
- **`crates/relix-sdk/src/lib.rs:227`** — MAJOR — `r.text().await.unwrap_or_default()` on non-2xx swallows transport error.
- **`crates/relix-sdk/src/lib.rs:208`** — MAJOR — `chat_stream` doesn't expose `done` frame metadata.
- **`crates/relix-sdk/src/lib.rs:41`** — MINOR — `RelixError` doesn't preserve underlying `reqwest::Error` — callers can't inspect `is_timeout()` etc.

### 2.2 Python SDK — surface lies and error hierarchy escape

- **`sdks/python/relix/client.py:492`** — **CRITICAL** — `ChatResponse.model_validate(data)` raises `pydantic.ValidationError` (NOT a `RelixError`) when bridge omits required fields. **README's "every method raises a subclass of RelixError" is false.**
- **`sdks/python/relix/memory.py:41`** — MAJOR — `id: str = Field(alias="embedding_id")` required — schema drift raises ValidationError.
- **`sdks/python/relix/memory.py:161`** — MAJOR — Single bad row crashes entire search response.
- **`sdks/python/relix/planning.py:168`** — MAJOR — `AgentDescriptor.model_validate(r)` — missing `name` raises ValidationError.
- **`sdks/python/relix/skills.py:122`** — MAJOR — Single malformed skill row kills full search response.
- **`sdks/python/relix/observability.py:170`** — MAJOR — Single bad alert row crashes entire alert list.
- **`sdks/python/relix/observability.py:159`** — MAJOR — `int(hours) if isinstance(hours, int)` — float `24.0` becomes None silently; `bool` becomes int.
- **`sdks/python/relix/client.py:338`** — MAJOR — No retry/backoff on transient failures.
- **`sdks/python/relix/client.py:338`** — MAJOR — httpx.Client built with no `limits=`.
- **`sdks/python/relix/client.py:235`** — MAJOR — Bearer token built unstripped.
- **`sdks/python/relix/client.py:432`** — MAJOR — Raw body stashed unbounded in exception.
- **`sdks/python/relix/client.py:485`** — MAJOR — `chat()` body has no `tenant_id`, no `request_id`, no idempotency key.
- **`sdks/python/relix/client.py:536`** — MAJOR — `chat_stream` calls `resp.read().decode(...)` on ≥400 — unbounded memory for 5xx pages.
- **`sdks/python/relix/client.py:570`** — MAJOR — Same in async path.
- **`sdks/python/relix/client.py:357`** — MAJOR — `aclose()` requires event loop; sync `close()` on async-only client leaks.
- **`sdks/python/relix/client.py:363`** — MAJOR — `__exit__` only calls `close()`; async client leaks.
- **`sdks/python/relix/client.py:337`** — MAJOR — `_ensure_sync` silently recreates Client after close.
- **`sdks/python/relix/client.py:583`** — MAJOR — `info()` coerces non-dict responses to `{}` silently.
- **`sdks/python/relix/planning.py:80`** — MAJOR — Bridge weird shape yields PlanResult with defaults; caller can't distinguish "plan failed" from "plan empty".

### 2.3 TypeScript SDK — browser-broken, no retry

- **`sdks/typescript/src/client.ts:201`** — MAJOR — Sets `user-agent` header — **forbidden in browsers**; fetch throws TypeError. README claims browser support.
- **`sdks/typescript/src/client.ts:313`** — MAJOR — `apiKey` exposed as public readonly field → leaks in JSON.stringify.
- **`sdks/typescript/src/client.ts:373`** — MAJOR — `chatStream` AbortController fires at `timeoutMs` for ENTIRE stream; long replies killed mid-stream.
- **`sdks/typescript/src/client.ts:414`** — MAJOR — Mid-stream errors thrown raw, never translated to RelixConnectionError.
- **`sdks/typescript/src/client.ts:440`** — MAJOR — Early `break` from for-await leaks HTTP connection.
- **`sdks/typescript/src/client.ts:222`** — MAJOR — `doJsonRequest` never retries.
- **`sdks/typescript/src/client.ts:347`** — MINOR — Sends `agent` in body but bridge ignores it.
- **`sdks/typescript/src/client.ts:186`** — MAJOR — Raw response body unbounded in every RelixResponseError.
- **`sdks/typescript/src/client.ts:267`** — MINOR — `buildUrl` skips falsey params; `limit=0`/`hours=0` silently dropped.
- **`sdks/typescript/src/types.ts:135`** — MINOR — `ChatUsage` uses `[key: string]: unknown` — weakens types.
- **`sdks/typescript/package.json:5`** — MINOR — Only `main` set; no `module`/`exports`/`types` map.
- **`sdks/typescript/src/client.ts:329`** — MINOR — "no fetch" thrown as plain Error, not RelixError subclass.

### 2.4 SDK surface divergence

- Python `MemoryResult.id` aliased from `embedding_id`; TS `MemoryResult.id` falls back to `embeddingId`. Rust SDK doesn't fall back at all → returns "" rows on any future schema change.
- Python `IngestDocumentResult` is snake_case; TS uses camelCase. Inconsistent for cross-SDK code.
- Python SDK has `flush_context`; Rust SDK has no equivalent.
- Python `planning.plan()`, `planning.agents()`, `planning.validate()` — Rust SDK has none.
- Python `observability.health()`, `alerts()`, `alert_history()` — Rust SDK has none.
- Python `skills.search()`, `stats()`, `get()` — Rust SDK has none.
- TS `ChatResponse.text` aliased from `reply`; Python same; Rust returns raw `reply` field unaliased.

### 2.5 CLI command-to-endpoint matching

All CLI endpoints exist on the bridge **except** `relix memory list/show/search/invalidate/stats` which default to `http://127.0.0.1:9100` (closed port). See section 1.8.

### 2.6 Bridge endpoints with shape divergence

- **`crates/relix-web-bridge/src/chat.rs:38`** — MINOR — `ErrorResponse` and `ChatResponse` distinct shapes — JSON consumers can't discriminate without HTTP status.
- **`crates/relix-web-bridge/src/openai.rs:1054`** — MAJOR — SSE stream emits `relix` extension carrying `flow_log` path — leaks server-side directory layout.
- **`crates/relix-web-bridge/src/openai.rs:30`** — MAJOR — Doc claim "system messages and OpenAI tool-call payloads are dropped" — bridge silently discards system prompts.
- **`crates/relix-web-bridge/src/export.rs:218`** — MAJOR — `synth_export` returns HARDCODED placeholder session for `agent=` and `all=` scopes; 200 OK with fake content.

### 2.7 Capabilities registered but not wired

- **`crates/relix-web-bridge/src/mcp.rs:197`** — `tool.mcp.invoke` returns RuntimeNotConnected from runtime when MCP runtime isn't wired.
- **`crates/relix-runtime/src/dispatch/mod.rs:223`** — `DispatchBridge` has NO shutdown method. SQLite handle leak.
- **`crates/relix-runtime/src/coordinator/mod.rs:8`** — **CRITICAL** — Coordinator is an 8-line `CoordinatorStub`. Module header doc-comment claims it owns per-flow event logs, RemoteCallIssued/Completed recording, log-before-act, reload/shutdown. **None of it exists.**

### 2.8 Embedded crate vs bridge divergence

- **`crates/relix-embedded/src/lib.rs:135`** — **CRITICAL** — Embedded runs no policy, no audit, no approval, no tenant isolation, no metrics — bypasses every governance control the bridge enforces.
- **`crates/relix-embedded/src/lib.rs:135`** — **CRITICAL** — Embedded mode has no tenant isolation: `MemoryRecord::new_raw` sets `tenant_id: None`; `text_search` returns rows across tenants.
- **`crates/relix-embedded/src/lib.rs:135`** — MAJOR — No metrics surface, no budget enforcer, no cost spike detector.
- **`crates/relix-embedded/src/lib.rs:135`** — MAJOR — No approval flow.
- **`crates/relix-embedded/src/lib.rs:135`** — MAJOR — Different capability registry: ONLY chat/memory_ingest/memory_search exposed.
- **`crates/relix-embedded/src/lib.rs:135`** — MAJOR — Different error semantics: collapses every provider failure to `EmbeddedError::Provider(String)`.
- **`crates/relix-embedded/src/memory.rs:235`** — **CRITICAL** — `&current[overlap_from..]` uses byte offset 100 from end. **Lands inside multi-byte UTF-8 character on non-ASCII document → panic.**
- **`crates/relix-embedded/src/memory.rs:185`** — **CRITICAL** — `memory_search` post-filters with `subject_id` AFTER limit applied → query for caller's subject may return zero.
- **`crates/relix-embedded/src/memory.rs:148`** — MAJOR — Bulk insert has no transaction. Partial state on crash.
- **`crates/relix-embedded/src/chat.rs:104`** — MAJOR — History ring trim off-by-one. Orphan "user:" with no reply.
- **`crates/relix-embedded/src/chat.rs:196`** — MAJOR — `persist_turn` blocking SQLite call from async `chat()` future.

### 2.9 Plugin SDK

- **`crates/relix-plugin-sdk/src/lib.rs:103`** — **CRITICAL** — NO outbound capability-call API. Plugins limited to pure stateless transforms.
- **`crates/relix-plugin-sdk/src/lib.rs:1`** — **CRITICAL** — NO plugin manifest schema, NO sandbox interface.
- **`crates/relix-plugin-sdk/src/lib.rs:235`** — **CRITICAL** — `mark_ready()` fire-and-forget `tokio::spawn` — race with inbound `/ready` probe.
- **`crates/relix-plugin-sdk/src/lib.rs:351`** — MAJOR — `handle_invoke` has NO auth. Any local process reaching `127.0.0.1:<port>` invokes handlers.
- **`crates/relix-plugin-sdk/src/lib.rs:268`** — MAJOR — `serve()` has no graceful shutdown.
- **`crates/relix-plugin-sdk/src/lib.rs:339`** — MAJOR — Does NOT respect `req.deadline_unix`.
- **`crates/relix-plugin-sdk/src/lib.rs:339`** — MAJOR — No request size limit.
- **`crates/relix-plugin-sdk/src/lib.rs:206`** — MAJOR — No lifecycle hooks beyond `mark_ready`.

### 2.10 Bridge surface gaps

- **`crates/relix-web-bridge/src/main.rs:381`** — MAJOR — No `DefaultBodyLimit` layer anywhere; axum default (2MB) only cap.
- **`crates/relix-web-bridge/src/chat.rs:117`** — MAJOR — `chat_stream` runs flow to completion BEFORE opening SSE response. Not actually streaming.
- **`crates/relix-web-bridge/src/ws.rs:182`** — MAJOR — WS handler runs full flow then slices reply with 20ms pacing — not streaming, paced playback.
- **`crates/relix-web-bridge/src/ws.rs:165`** — MAJOR — `read_request` silently hangs up on parse failure.
- **`crates/relix-cli/src/email.rs:331`** — MINOR — Multiple `.expect("reqwest::Client builds")` across CLI surfaces.

---

## Angle 3 — Correctness: things that are just wrong

### 3.1 `.unwrap()` / `.expect()` reachable from input

- **`crates/relix-runtime/src/dispatch/mod.rs:138, 147, 154, 847, 874`** — MAJOR — Five poisonable expects on policy denial ring / capability stats.
- **`crates/relix-runtime/src/identity/session.rs:539, 546`** — MAJOR — `.expect("HMAC accepts any key length")` panics on hot path.
- **`crates/relix-runtime/src/plugin/dispatcher.rs:62`** — MAJOR — `reqwest::Client::builder().build().expect(...)` panics whole controller at startup.
- **`crates/relix-runtime/src/training/recorder.rs:247`** — MAJOR — `.expect("RecorderWorkerHandles::spawn called twice")` panics on double-spawn.
- **`crates/relix-runtime/src/plugin/manifest.rs:222`** — MAJOR — `canonicalize().unwrap_or(candidate)` defeats symlink-traversal protection.
- **`crates/relix-runtime/src/manifest/mod.rs:562`** — MAJOR — `panic_no_identity()` in `Default::default` — `..Default::default()` panics.
- **`crates/relix-runtime/src/sol/cli.rs:20, 32`** — MAJOR — `arg.chars().nth(0).unwrap()` panics on empty CLI arg.
- **`crates/relix-runtime/src/sol/vm.rs:208, 250, 278`** — MAJOR — `.expect("Runtime Error: Stack underflow")` panics on malformed bytecode.
- **`crates/relix-runtime/src/credentials/store.rs:539`** — MINOR — Poisoned-mutex returns flat error.
- **`crates/relix-runtime/src/metrics/cost_baseline.rs:128`** — MAJOR — `self.conn.lock().unwrap()` panics on poison.
- **`crates/relix-runtime/src/sflow/executor.rs:116`** — MAJOR — `VecChronicle.entries()` uses `.lock().unwrap()`.
- **`crates/relix-web-bridge/src/dashboard.rs:139`** — MINOR — `.expect("dashboard response builds")` panic seam.
- **`crates/relix-web-bridge/src/auth.rs:350`** — MINOR — `.expect("bootstrap response builds")`.
- **`crates/relix-web-bridge/src/rate_limit.rs:197, 211, 239`** — MINOR — Three `.expect("rate-limit map lock")` reachable from every request.
- **`crates/relix-web-bridge/src/secrets.rs:764, 776`** — MINOR — Read/mutate panic on poison.
- **`crates/relix-web-bridge/src/intervention_audit.rs:173, 215`** — MINOR — Panic on poison; takes audit ring down.
- **`crates/relix-telegram/src/session_store.rs:78, 82, 87, 92`** — MAJOR — `.expect("poisoned")` on every record path.
- **`crates/relix-plugin-sdk/src/lib.rs:357, 364`** — MAJOR — Two `serde_json::to_value().unwrap()` on success and error body paths.
- **`crates/relix-runtime/src/yaml_flow/mod.rs:272`** — MAJOR — `compile_path` uses `std::fs::read_to_string` with no max size.
- **`crates/relix-runtime/src/yaml_flow/mod.rs:255`** — MAJOR — YAML parser has no nesting limit; stack overflow possible.

### 3.2 SQLite migrations / schema versioning

- **`crates/relix-runtime/src/audit_partition.rs:97`** — MAJOR — Schema is `execute_batch` + `CREATE TABLE IF NOT EXISTS` only; not registered with `_relix_migrations`.
- **`crates/relix-runtime/src/workflow/chronicle.rs:106`** — MAJOR — Same problem — future column adds silently no-op.
- **`crates/relix-runtime/src/training/store.rs:561`** — MAJOR — Outside the migrations framework.
- **`crates/relix-runtime/src/plugin/registry.rs:84`** — MINOR — No migration version tracking.
- **`crates/relix-runtime/src/metrics/store.rs:170`** — MINOR — Conditional `ALTER TABLE` columns added without recording in migrations.
- **`crates/relix-runtime/src/db.rs:182`** — MAJOR — `is_migration_already_applied` matches error message substring "already exists" — collides with unrelated errors.
- **`crates/relix-runtime/src/db.rs:130`** — MINOR — `record_migration_applied` uses `INSERT OR IGNORE` — can't distinguish "applied" from "fresh insert".

### 3.3 Bounded collections that aren't bounded

- **`crates/relix-core/src/policy.rs:280`** — MAJOR — `cache: Mutex<HashMap<String, ...>>` keyed by tenant id has no max, no eviction.
- **`crates/relix-core/src/policy.rs:281`** — MAJOR — Negative cache entry stored on every miss.
- **`crates/relix-embedded/src/chat.rs:93`** — MAJOR — `HistoryStore::sessions` unbounded HashMap.
- **`crates/relix-runtime/src/dispatch/mod.rs:1004`** — MAJOR — Unknown-method counter `capability_stats` no cardinality limit.
- **`crates/relix-runtime/src/identity/session.rs:481`** — **CRITICAL** — `verify()` calls `store.list(Some(agent_name))` on EVERY verification — full table read per request.
- **`crates/relix-runtime/src/metrics/collector.rs:120`** — MAJOR — `mpsc::unbounded_channel`.
- **`crates/relix-runtime/src/metrics/collector.rs:215`** — MAJOR — Cache overflow CLEARS every pending hint.
- **`crates/relix-runtime/src/observability/otel.rs:155`** — MAJOR — `record_event` has no buffer-size guard.
- **`crates/relix-runtime/src/training/recorder.rs:120`** — MAJOR — `mpsc::unbounded_channel`.
- **`crates/relix-runtime/src/dispatch/mod.rs:436`** — MAJOR — `recent_latencies` ring keyed on method only; attacker can flood.
- **`crates/relix-runtime/src/transport/rpc.rs:196`** — MAJOR — `pending_calls` unbounded.
- **`crates/relix-core/src/eventlog.rs:217`** — MAJOR — `vec![0u8; len]` allocates `len` from disk header (up to 4 GiB).
- **`crates/relix-core/src/eventlog.rs:215`** — MAJOR — Unbounded record count in `read_records`.
- **`crates/relix-core/src/audit.rs:230`** — MAJOR — Same 4 GiB header allocation in `read_audit_records`.
- **`crates/relix-runtime/src/dispatch/mod.rs:115`** — MAJOR — `PolicyDenialRing` FIFO eviction; attacker hides denials by spamming.
- **`crates/relix-runtime/src/knowledge/service.rs:611`** — MAJOR — Hard-coded 10,000 limit in `list_shared`.
- **`crates/relix-runtime/src/knowledge/autoshare.rs:303`** — MAJOR — Per-agent list capped at 500 rows.
- **`crates/relix-runtime/src/sflow/executor.rs:88`** — MAJOR — `MAX_VARS = 50` hard cap.

### 3.4 Cryptographic mistakes

- **`crates/relix-runtime/src/credentials/store.rs:575`** — **CRITICAL** — Key derivation is **plain SHA-256 of master secret**. No salt, no iterations. Brute-forceable.
- **`crates/relix-runtime/src/credentials/store.rs:184`** — MAJOR — Plaintext AES key in heap memory — no Zeroize.
- **`crates/relix-runtime/src/credentials/store.rs:619`** — MAJOR — Decrypted plaintext as `String`, no Zeroize.
- **`crates/relix-flow-inspect/src/main.rs:77`** — MAJOR — Reads signing key into Vec<u8> not zeroised on drop.
- **`crates/relix-runtime/src/identity/session.rs:463`** — **CRITICAL** — TOCTOU: signature verified, then expiry, then `list()` to find row and check `revoked`, then `touch()` in separate connection lock. **No atomic verify+revoke**.
- **`crates/relix-runtime/src/identity/session.rs:386`** — MAJOR — `scopes_json` decode silently swallows errors → empty Vec.
- **`crates/relix-runtime/src/identity/session.rs:105`** — MAJOR — `canonical_bytes()` strategy is fragile — CBOR map ordering is not pinned.
- **`crates/relix-runtime/src/identity/session.rs:438`** — MINOR — Token wire format has no key id / version.
- **`crates/relix-core/src/codec.rs:27`** — MAJOR — Docstring claims byte-identical determinism but canonical encoder is "Gate 2".
- **`crates/relix-core/src/codec.rs:34`** — MAJOR — `decode` accepts arbitrary CBOR — no length check on input.
- **`crates/relix-core/src/redact.rs:78, 114, 206`** — MAJOR — Pattern issues: `sk-` matches at any byte offset; PEM redaction matches header without footer.
- **`crates/relix-core/src/redact.rs:88`** — MAJOR — Patterns NOT covered: Stripe `sk_live_`, Google `AIza`, JWT, AWS `ASIA`.
- **`crates/relix-runtime/src/credentials/store.rs:586`** — MINOR — Doesn't reject zeroed nonces.
- **`crates/relix-runtime/src/admission/agent_gate.rs:137`** — **CRITICAL** — SQL `WHERE approval_token = ?1` is NOT constant-time.
- **`crates/relix-runtime/src/admission/agent_gate.rs:179`** — **CRITICAL** — Token check does NOT verify subject matches caller.
- **`crates/relix-runtime/src/admission/agent_gate.rs:179`** — **CRITICAL** — TOCTOU between admission and consume.
- **`crates/relix-runtime/src/manifest/mod.rs:42`** — **CRITICAL** — `NodeManifest` sent as plain CBOR. NO signature verification.
- **`crates/relix-runtime/src/knowledge/remote.rs:175, 194`** — MAJOR — Canonical bytes don't cover all fields.

### 3.5 Integer overflow

- **`crates/relix-core/src/bundle.rs:204`** — MAJOR — `now - 30` and `now + lifetime_secs` overflow i64 silently in release.
- **`crates/relix-core/src/eventlog.rs:184`** — **CRITICAL** — `self.next_seq = seq + 1` wraps u64 silently.
- **`crates/relix-core/src/audit.rs:148`** — MAJOR — `started_at.elapsed().as_millis() as u64` truncates u128→u64.
- **`crates/relix-core/src/types.rs:157`** — MAJOR — `Timestamp::add_secs` uses `self.0 + secs` — silent i64 wrap.
- **`crates/relix-core/src/types.rs:149`** — MAJOR — `Timestamp::now()` returns 1970 on epoch failure.
- **`crates/relix-core/src/router.rs:88`** — MINOR — `total_sessions_since_start: u64` monotonic counter.
- **`crates/relix-core/src/retry.rs:64`** — MAJOR — `base.as_millis() as u64` truncates.
- **`crates/relix-runtime/src/metrics/budget.rs:510`** — MAJOR — `(limit_usd * 1_000_000.0).max(0.0) as u64` wraps on inf/NaN. NaN → 0 → effectively unlimited.
- **`crates/relix-runtime/src/metrics/budget.rs:611`** — MINOR — `as_millis() as i64` overflows for decades.
- **`crates/relix-runtime/src/observability/sinks.rs:179`** — MINOR — `cost_cents: u32` truncates above $42M cumulative.

### 3.6 Race conditions / concurrent-access correctness

- **`crates/relix-runtime/src/manifest/mod.rs:594`** — MAJOR — `local_port = 30_000 + (rand::random::<u16>() % 5_000)` — two concurrent discoveries race.
- **`crates/relix-runtime/src/planning/approval.rs:434`** — MAJOR — `decide()` reads status outside transaction.
- **`crates/relix-runtime/src/planning/approval.rs:469`** — MAJOR — `expire_older_than` loops `UPDATE` per row with no enclosing transaction.
- **`crates/relix-runtime/src/dispatch/mod.rs:1202`** — MAJOR — `broker.check()` and `broker.record_call()` are two non-atomic calls.
- **`crates/relix-runtime/src/dispatch/mod.rs:1967`** — MAJOR — Same race in streaming path.
- **`crates/relix-runtime/src/metrics/budget.rs:597`** — MAJOR — Refresh races two threads.
- **`crates/relix-embedded/src/chat.rs:248`** — MINOR — `turn_id` uses wall-clock nanos; SystemTime can go backwards.
- **`crates/relix-runtime/src/identity/session.rs:521`** — MAJOR — Idle sweep cutoff with NTP backward slew → mass revoke.
- **`crates/relix-runtime/src/observability/session_debugger.rs:101`** — MAJOR — Wall-clock comparison flags stalled on jumps.
- **`crates/relix-runtime/src/transport/rpc.rs:299`** — MAJOR — `Call` inserts into `pending_calls` AFTER `send_request`.
- **`crates/relix-runtime/src/credentials/store.rs:329`** — MAJOR — `get()` is 2-query N+1 with two separate locks.

### 3.7 Parsing / decoding assumes fields exist

- **`crates/relix-runtime/src/planning/critic.rs:438`** — MAJOR — `parse_verdict` `find('{')` + `rfind('}')` — JSON-in-prose injection.
- **`crates/relix-runtime/src/planning/orchestrator.rs:485`** — MAJOR — `parse_sub_goals` falls through to line-split with no validation.
- **`crates/relix-runtime/src/planning/orchestrator.rs:330`** — MAJOR — Falls back to `heuristic_decompose` SILENTLY on parse failure.
- **`crates/relix-runtime/src/planning/verification.rs:618`** — **CRITICAL** — `evaluate_pattern_match` PASSES BY DEFAULT on regex compile failure.
- **`crates/relix-runtime/src/planning/verification.rs:609`** — MAJOR — `regex::Regex::new` accepts arbitrary pattern — ReDoS via `(a+)+b`.
- **`crates/relix-runtime/src/planning/verification.rs:235`** — MAJOR — `passed` calculation tangled.
- **`crates/relix-runtime/src/sflow/lexer.rs:137`** — MAJOR — String literal lexer has NO escape sequence support.
- **`crates/relix-runtime/src/yaml_flow/mod.rs:981`** — **CRITICAL** — `lower_if` interpolates user `s.condition.trim()` raw into SOL source via `format!("if {} {{\n")`. **SOL injection.**
- **`crates/relix-runtime/src/sflow/executor.rs:309`** — MAJOR — `std::thread::sleep` for `SolSleep` blocks OS thread.

### 3.8 Audit / record correctness

- **`crates/relix-core/src/audit.rs:100`** — MAJOR — `tenant_id` deliberately NOT in signed `AuditRecord`.
- **`crates/relix-core/src/eventlog.rs:181`** — MAJOR — `sync_data()` only flushes data; parent directory not fsynced.
- **`crates/relix-runtime/src/dispatch/mod.rs:2236`** — MAJOR — Audit id = `req.rid` (caller-controlled).
- **`crates/relix-runtime/src/training/store.rs:128`** — MAJOR — `mark_exported` UPDATE without `WHERE exported = 0` guard.
- **`crates/relix-runtime/src/training/exporter.rs:260`** — MAJOR — `mark_exported` runs AFTER file write; failure → double-create.
- **`crates/relix-runtime/src/workflow/chronicle.rs:192`** — MINOR — Silent corruption recovery.

### 3.9 Web bridge correctness

- **`crates/relix-web-bridge/src/flow.rs:127`** — MAJOR — Template rendering is naive `.replace("{{SESSION}}", session_id)` — placeholder injection.
- **`crates/relix-web-bridge/src/openai.rs:805`** — MINOR — Session id derivation 48 bits — collisions plausible.
- **`crates/relix-web-bridge/src/flow.rs:439`** — MAJOR — `chat_params_json` hand-rolls JSON via `json_escape`.
- **`crates/relix-web-bridge/src/config.rs:478`** — MAJOR — `BridgeSecrets::load_or_empty` treats corrupt file as empty; mutate() OVERWRITES corrupt with empty content. **Disk-data-loss.**

### 3.10 Cost / pricing arithmetic

- **`crates/relix-runtime/src/metrics/pricing.rs:124`** — MINOR — Integer truncation discards fractional cents.
- **`crates/relix-runtime/src/metrics/pricing.rs:100`** — MINOR — Longest-prefix-match silently falls back to base model price.

### 3.11 Plugin loader

- **`crates/relix-runtime/src/plugin/dispatcher.rs:65`** — **CRITICAL** — `base: format!("http://127.0.0.1:{port}")` — plaintext HTTP, no TLS, no auth.
- **`crates/relix-runtime/src/plugin/loader.rs:32`** — **CRITICAL** — Spawn with NO sandbox: no chroot, no seccomp, no namespaces, no rlimit.
- **`crates/relix-runtime/src/plugin/loader.rs:206`** — MAJOR — `resolved_binary` returns bare command name → PATH lookup → operator can run `rm`.
- **`crates/relix-runtime/src/plugin/loader.rs:131`** — MAJOR — NO signature check on plugin manifest or binary.
- **`crates/relix-runtime/src/plugin/manifest.rs:115`** — MAJOR — `toml::from_str` directly on operator file with no depth/size limit.

---

## Angle 4 — Security: things that could be exploited

### 4.1 SSRF — user-supplied URLs without enforced allowlist

- **`crates/relix-web-bridge/src/validate.rs:54`** — **CRITICAL** — `validate_url` (used by `/chat_with_tool`) has zero SSRF check. Accepts `http://127.0.0.1:6379/` (Redis), `http://169.254.169.254/` (AWS metadata), `http://10.0.0.5/`.
- **`crates/relix-web-bridge/src/validate.rs:82`** — MAJOR — `detect_url_in_message` auto-routes OpenAI shim messages to tool flow when URL appears in user content. User-driven SSRF.
- **`examples/plugins/web-lookup/src/main.rs:24`** — MAJOR — Example plugin makes arbitrary HTTP GET with no SSRF beyond scheme prefix check.
- **`configs/tool-node.toml:20`** — **CRITICAL** — `url_allowlist` parsed but unread. Fail-open.
- **`crates/relix-runtime/src/identity/research.rs:381`** — MAJOR — `run_searches` no per-call timeout; bomb against external search providers.

### 4.2 Untrusted content reaching LLM prompts without perception boundary

- **`crates/relix-runtime/src/identity/research.rs:402`** — MAJOR — LLM synthesis prompt incorporates raw web search results into user message. System prompt says "untrusted" but same payload goes to model.
- **`crates/relix-runtime/src/planning/critic.rs:287`** — MAJOR — `arg = format!("{session_id}|{prompt}")` passes user-supplied spec text verbatim into AI peer call. Pipe-injection.
- **`crates/relix-runtime/src/planning/orchestrator.rs:322`** — MAJOR — Same pipe-injection vector.
- **`crates/relix-runtime/src/planning/verification.rs:297`** — MAJOR — Same pipe-injection for AI judge prompt.

### 4.3 Tenant isolation gaps

- **`crates/relix-web-bridge/src/tenant.rs:81`** — **CRITICAL** — Tenant middleware stamps `X-Relix-Tenant` into Extensions but **only `memory_gap5.rs` (1 of 45 mesh-calling handlers) reads it**.
- **`crates/relix-runtime/src/dispatch/mod.rs:1142`** — MAJOR — Tenant policy resolver consulted without verifying caller's `verified.org_id` is authorized in that tenant.
- **`crates/relix-runtime/src/dispatch/mod.rs:1913`** — MAJOR — Same tenant-spoofing in streaming path.
- **`crates/relix-runtime/src/transport/envelope.rs:69`** — MAJOR — `tenant_id` operator-asserted, not cryptographically bound.
- **`crates/relix-embedded/src/lib.rs:135`** — **CRITICAL** — Embedded crate has no tenant isolation.
- **`crates/relix-flow-inspect/src/main.rs:107`** — **CRITICAL** — `handle_audit` has NO `--tenant` filter — prints every tenant's records.

### 4.4 Approval token weakness

- **`crates/relix-runtime/src/dispatch/mod.rs:1114`** — **CRITICAL** — Step 8.5 bypassed by ANY non-empty `approval_token`.
- **`crates/relix-runtime/src/dispatch/mod.rs:1880`** — **CRITICAL** — Same in streaming.
- **`crates/relix-runtime/src/admission/agent_gate.rs:137`** — **CRITICAL** — SQL token compare not constant-time.
- **`crates/relix-runtime/src/admission/agent_gate.rs:179`** — **CRITICAL** — Token doesn't bind to subject.
- **`crates/relix-runtime/src/admission/agent_gate.rs:243`** — MAJOR — Surface check uses operator-asserted `envelope.surface`.
- **`crates/relix-runtime/src/admission/agent_gate.rs:179`** — MAJOR — TOCTOU between `get_approval_by_token` and `consume_approval_token`.
- **`crates/relix-runtime/src/approval/store.rs:222`** — MAJOR — `record_decision` UPDATE has NO `WHERE status='pending'` guard.
- **`crates/relix-runtime/src/approval/caps.rs:144`** — **CRITICAL** — `handle_record_decision` writes decision WITHOUT verifying caller is authorized approver.
- **`crates/relix-runtime/src/approval/caps.rs:107`** — MAJOR — `handle_deliver` accepts caller-supplied approval params → phishing.
- **`crates/relix-runtime/src/approval/caps.rs:155`** — MAJOR — Accepts decision string `"expired"` from wire.
- **`crates/relix-runtime/src/approval/caps.rs:161`** — MAJOR — No idempotency / replay protection on record_decision.

### 4.5 Web bridge auth gaps

- **`crates/relix-web-bridge/src/auth.rs:217`** — **CRITICAL** — `GET /v1/auth/token` reachable from any local caller without auth. Local malware steals bearer.
- **`crates/relix-web-bridge/src/auth.rs:152`** — MAJOR — `?token=` query fallback for SSE → tokens in access logs, browser history, latency logs.
- **`crates/relix-web-bridge/src/auth.rs:243`** — MAJOR — `Origin: null` unconditionally accepted as same-origin.
- **`crates/relix-web-bridge/src/auth.rs:284`** — MAJOR — OpenAI shim accepts ANY non-empty bearer.
- **`crates/relix-web-bridge/src/ws.rs:206`** — **CRITICAL** — `parse_bearer` accepts ANY non-empty bearer for WS upgrade.
- **`crates/relix-web-bridge/src/ws.rs:98`** — MAJOR — `ws_principal` uses raw bearer string as principal map key.
- **`crates/relix-web-bridge/src/security_headers.rs:43`** — MAJOR — CSP `connect-src 'self' ws: wss:` → same-origin script connects to any attacker host.

### 4.6 Client-supplied identity fields not bound to authenticated principal

- **`crates/relix-web-bridge/src/messaging.rs:120`** — **CRITICAL** — `from_subject_id` client-supplied. Spoof from any subject.
- **`crates/relix-web-bridge/src/messaging.rs:180`** — **CRITICAL** — `reader_subject_id`/`subject_id` client-supplied. Read/delete any user's messages.
- **`crates/relix-web-bridge/src/agent.rs:163`** — MAJOR — `created_by` and `subject_id` client-supplied on agent create.
- **`crates/relix-web-bridge/src/agent.rs:341`** — MAJOR — `decided_by` defaults to literal `"operator"` — any local caller rubber-stamps.
- **`crates/relix-web-bridge/src/agent.rs:424`** — MAJOR — `granted_by` defaults to `"operator"`.
- **`crates/relix-web-bridge/src/sessions_obs.rs:117`** — MAJOR — "Elevated" content access via `X-Relix-Elevated: true` header. Security theater.
- **`crates/relix-runtime/src/dispatch/mod.rs:1203`** — MAJOR — Access-broker keyed off `verified.name` (mutable client-supplied).
- **`crates/relix-runtime/src/dispatch/mod.rs:1080, 1378`** — MAJOR — Metrics use spoofable `verified.name`.

### 4.7 Client-supplied `peer` parameter routes mesh calls anywhere

39 handlers across the bridge let any authenticated caller route mesh dispatch to any peer alias:

- **`crates/relix-web-bridge/src/plugins.rs:75`** — MAJOR — `plugin.list/status/reload/disable`.
- **`crates/relix-web-bridge/src/skills.rs:92`** — MAJOR — Skills.
- **`crates/relix-web-bridge/src/credentials.rs:80`** — MAJOR — Credentials store/get/rotate/revoke against any peer.
- **`crates/relix-web-bridge/src/identity_session.rs:86`** — MAJOR — Identity token endpoints against arbitrary nodes.
- **`crates/relix-web-bridge/src/mcp.rs:101`** — MAJOR — `tool.mcp.list_servers/list_tools/invoke` against any peer — full attacker-controlled mesh dispatch primitive.
- **`crates/relix-web-bridge/src/tool_screen.rs:84`** — MAJOR — `tool.screen` against any peer.
- **`crates/relix-web-bridge/src/email.rs:303`** — **CRITICAL** — `SendAttachment.path` forwarded verbatim → caller can request `{"path":"/etc/shadow"}`.
- **`crates/relix-cli/src/fs.rs:62`**, **`web.rs:53`**, **`browser.rs:36`**, **`terminal.rs:228`**, **`pii.rs:110`**, **`metrics.rs:141, 258, 417, 472`**, **`ops.rs:3105`** — MAJOR — Multiple CLI URL injections via unencoded `peer`/`agent`/`provider` parameters.

### 4.8 Secrets in logs / `Debug` derives leaking tokens

- **`crates/relix-telegram/src/live.rs:84`** — MAJOR — `url_prefix = "<base>/bot<token>"` held as plain String; token in any reqwest debug log, any panic message.
- **`crates/relix-discord/src/live.rs:82`** — MAJOR — `token: String` plaintext.
- **`crates/relix-slack/src/live.rs:62`** — MAJOR — Same plain-String token.
- **`sdks/typescript/src/client.ts:313`** — MAJOR — `apiKey` public readonly field.
- **`crates/relix-sdk/src/lib.rs:90`** — MINOR — Token: String not zeroed/redacted.
- **`crates/relix-web-bridge/src/config_api.rs:1177`** — MAJOR — Bot token interpolated DIRECTLY into URL path; not URL-encoded.
- **`crates/relix-web-bridge/src/openai.rs:1054`** — MAJOR — SSE stream emits `flow_log` path — leaks directory layout.

### 4.9 Path traversal / file access

- **`crates/relix-runtime/src/workflow/store.rs:160`** — MAJOR — `WorkflowStore::get(name)` joins user-supplied name → path traversal.
- **`crates/relix-runtime/src/workflow/store.rs:160`** — MAJOR — `std::fs::read_to_string` no max file size.
- **`crates/relix-runtime/src/workflow/coordinator.rs:135`** — MAJOR — `RunArgs.input` no max size limit.
- **`crates/relix-core/src/policy.rs:340`** — MAJOR — `tenant_policy_text` no canonicalize-and-verify path stays in `dir`.
- **`examples/plugins/web-lookup/src/main.rs:43`** — MINOR — No body size cap before `text()` materialises.

### 4.10 Threat-model lies

- **`specs/threat-model.md:107`** — **CRITICAL** — Existential property "AI provider keys ONLY in AI node" violated by `web-bridge/src/config_api.rs:403-528` — bridge stores provider api_keys + dials providers directly.
- **`specs/threat-model.md:108`** — **CRITICAL** — "Web backend makes no LLM provider call in RELIX_MODE" — same violation.
- **`specs/threat-model.md:109`** — MAJOR — "Routing decisions live only in SOL flows" — sflow + yaml_flow also drive routing.
- **`specs/threat-model.md:106`** — MAJOR — "Audit emitted on every responder" violated at dispatch.rs:961+1730 (decode failures emit no audit).
- **`specs/identity-employees.md:88`** — **CRITICAL** — H.6 enforcement pipeline steps 2/4/6/8 missing.
- **`specs/identity-employees.md:78`** — **CRITICAL** — H.5 approval signatures: code uses unsigned `approval_token` strings.

### 4.11 Replay protection

- **`specs/RELIX-1-rpc.md:56`** — **CRITICAL** — §1.9 mandates sliding-window replay cache; zero code uses `error_kinds::REPLAY_REJECTED`; no replay cache.
- **`crates/relix-runtime/src/dispatch/mod.rs:984`** — **CRITICAL** — No replay-protection step.
- **`crates/relix-runtime/src/transport/envelope.rs:32`** — MAJOR — Signed envelope deferred — every alpha call unsigned end-to-end.
- **`crates/relix-runtime/src/identity/session.rs:42`** — **CRITICAL** — `verify_on_dispatch` flag parsed but never consulted.

### 4.12 Install / supply chain

- **`.github/workflows/ci.yml:9`** — **CRITICAL** — Trigger is `on: workflow_dispatch` only. CI does NOT run on push or PR.
- **`.github/workflows/release.yml:122`** — **CRITICAL** — Release artifacts NOT signed (no cosign, GPG, SBOM, in-toto).
- **`install.sh:180`** — MAJOR — `tar -xzf` Tar-Slip vulnerable.
- **`install.sh:196`** — MAJOR — ALL "regular files" except md/text/json/toml get `chmod +x` and dropped into `~/.local/bin`.
- **`install.sh:251, 274`** — MAJOR — Mesh scripts + flow templates fetched from `main` (not pinned tag).
- **`install.ps1:6, 145`** — MAJOR — `iwr | iex` RCE; `Expand-Archive` no zip-slip protection.
- **`.github/workflows/heavy-ci.yml:84`** — MAJOR — `cargo audit` `continue-on-error: true`.
- All GitHub Actions use mutable tags (`@stable`, `@v2`, `@v4`).
- **`crates/relix-cli/src/install.rs:434`** — **CRITICAL** — `curl | sh` with no signature verification.
- **`Cargo.toml:97`** — MAJOR — `libp2p-stream = "=0.2.0-alpha"` pre-1.0 alpha; comment says `0.4.0-alpha` (wrong).
- **`Cargo.toml:89`** — MAJOR — `libp2p = "0.54"` has active `RUSTSEC-2026-0119` ignore.
- **`deny.toml:30`** — MAJOR — `multiple-versions = "warn"`; allows typosquats.
- **`deny.toml:32`** — MAJOR — `[bans] deny = []` — empty; abandoned/known-bad crates not banned.

### 4.13 Documentation lies about security

- **`docs/operator-guide.md:438, 535, 582`** — MAJOR — Multiple "no auth at HTTP layer" claims while bearer middleware ships.
- **`docs/deployment.md:89`**, **`docs/docker.md:111`**, **`docs/production-checklist.md:43, 109`** — MAJOR — Same stale claim.
- **`docs/getting-started.md:174, 267`** — **CRITICAL** — Examples send no bearer token → 401 on every invocation.
- **`docs/getting-started.md:194`** — MAJOR — Browser `new WebSocket(url, [], { headers })` invalid syntax.
- **`docs/getting-started.md:308`** — **CRITICAL** — `relix setup` / `relix boot` referenced but cargo build produces `relix-cli` not `relix`.
- **`docs/security.md:1`** — MAJOR — Silent on `RELIX_CREDENTIAL_KEY` master key, rotation, key loss.
- **`docs/security.md:271`** — MAJOR — Claims "bridge holds none" of provider keys, contradicting `bridge-secrets.toml`.
- **`docs/security.md:295`** — MAJOR — Says token at `~/.relix/bridge-token` but actually under configured `data_dir`.
- **`ops/runbooks/alpha-bringup.md:117`** — **CRITICAL** — `cp dev-keys/org-root.key dev-keys/org-root.pub` — COPIES THE SECRET KEY INTO THE .pub FILE.
- **`ops/runbooks/audit-query.md:21, 44, 85, 105, 118`** — **CRITICAL** — Every documented `relix-flow-inspect` flag does not exist. Entire runbook non-functional.

### 4.14 Flow-inspect security

- **`crates/relix-flow-inspect/src/main.rs:83`** — MAJOR — `--replay-verify` requires the *private* signing key.
- **`crates/relix-flow-inspect/src/main.rs:107`** — **CRITICAL** — Audit dump prints across all tenants.
- **`crates/relix-flow-inspect/src/main.rs:163`** — **CRITICAL** — `print_flow_human` blindly UTF-8 decodes payloads — exfiltrates user prompts, memory text, planner steps, provider replies.

### 4.15 Transport security

- **`crates/relix-runtime/src/transport/rpc.rs:42`** — MAJOR — libp2p `request_response::cbor::Behaviour` has no max message size. 4 GiB OOM.
- **`crates/relix-runtime/src/transport/rpc.rs:355`** — MAJOR — `idle_connection_timeout(u64::MAX)` keeps connections alive forever.
- **`crates/relix-runtime/src/transport/rpc.rs:281`** — MAJOR — No peer allowlist.
- **`crates/relix-runtime/src/transport/rpc.rs:262`** — MAJOR — `kademlia.add_address` unconditionally for every newly-connected peer → DHT poisoning.
- **`crates/relix-runtime/src/transport/stream.rs:182`** — MAJOR — `read_request_envelope` no deadline / read timeout.
- **`crates/relix-runtime/src/transport/stream.rs:308`** — MAJOR — Per-peer concurrent-stream cap absent.
- **`crates/relix-runtime/src/transport/stream.rs:251`** — MINOR — `decode_frame` no depth limit on ciborium.

### 4.16 Web bridge body size / rate limit gaps

- **`crates/relix-web-bridge/src/main.rs:381`** — MAJOR — No `DefaultBodyLimit`.
- **`crates/relix-web-bridge/src/config_api.rs:583, 602`** — MAJOR — Full body reads for provider tests with no cap → OOM via malicious upstream.
- **`crates/relix-web-bridge/src/openai.rs:236`** — MAJOR — `_extra` flatten with no size limit.

### 4.17 PII gate weaknesses

- **`crates/relix-runtime/src/dispatch/mod.rs:1259`** — MAJOR — PII gate scans args but NO outbound response scan.
- **`crates/relix-runtime/src/dispatch/mod.rs:2156`** — MAJOR — Streaming chunks NEVER scanned — exfiltration via streaming.
- **`crates/relix-runtime/src/training/exporter.rs:202`** — MAJOR — Exporter doesn't enforce anonymization when disabled.
- **`crates/relix-runtime/src/training/exporter.rs:257`** — MAJOR — `std::fs::write` with default 0644 perms for sensitive training data.

### 4.18 Misc

- **`crates/relix-runtime/src/dispatch/mod.rs:1506`** — MAJOR — Escalate path skips FULL admission pipeline on escalated call.
- **`crates/relix-runtime/src/approval/delivery.rs:309`** — MAJOR — `glob_match` manual recursive backtracker → ReDoS.
- **`crates/relix-runtime/src/admission/agent_gate.rs:413`** — MAJOR — `"unknown" => Some(4)` maps unknown risk to critical — widens permissions.

---

## Angle 5 — Performance: things that would crater under real load

### 5.1 Blocking operations inside async

- **`crates/relix-core/src/eventlog.rs:107, 142, 217`** — MAJOR — `std::fs::create_dir_all`, `write_all + sync_data`, blocking reads on hot paths.
- **`crates/relix-core/src/audit.rs:148`** — MAJOR — Blocking append finalize.
- **`crates/relix-core/src/policy.rs:103, 288, 319`** — MAJOR — `from_path` blocking I/O on admission hot path inside Mutex scope.
- **`crates/relix-runtime/src/metrics/spike_detector.rs:172`** — MAJOR — `tick()` sync, N SQLite queries serially.
- **`crates/relix-runtime/src/sflow/executor.rs:309`** — MAJOR — `std::thread::sleep` for SOL sleep.
- **`crates/relix-embedded/src/chat.rs:196`** — MAJOR — `persist_turn` blocking SQLite call from async future.
- **`crates/relix-embedded/src/memory.rs:131`** — MAJOR — `chunk_text` O(N²) on calling task BEFORE `spawn_blocking`.
- **`crates/relix-runtime/src/observability/otel.rs:189`** — MAJOR — Builds NEW reqwest::Client EVERY flush — 17k handshakes/day.
- **`crates/relix-runtime/src/dispatch/mod.rs:1302, 2067`** — MAJOR — Throttle `tokio::time::sleep` while holding inbound future.
- **`crates/relix-runtime/src/dispatch/mod.rs:1572`** — MAJOR — `sink.deliver(&ev)` synchronous on hot path.
- **`crates/relix-runtime/src/dispatch/mod.rs:1259, 2029`** — MAJOR — PII gate `req.args.to_vec()` copy on hot path.
- **`crates/relix-runtime/src/dispatch/mod.rs:2258, 2262`** — MAJOR — Audit partition append synchronous inside global `tokio::sync::Mutex<AuditLog>`.
- **`crates/relix-runtime/src/credentials/store.rs:329`** — MAJOR — `get()` is 2-query N+1 with two separate locks.

### 5.2 N+1 queries

- **`crates/relix-runtime/src/credentials/caps.rs:137`** — MAJOR — `handle_get` does full table scan for one row.
- **`crates/relix-runtime/src/observability/session_debugger.rs:147`** — MAJOR — `list_sessions` runs query PER session.
- **`crates/relix-runtime/src/observability/session_debugger.rs:178`** — MAJOR — `timeline_event` calls `content.get(event_id)` per event.
- **`crates/relix-runtime/src/metrics/query.rs:122`** — MAJOR — `list_agents` per-agent loop calls own SELECT.
- **`crates/relix-runtime/src/metrics/alert.rs:353`** — MAJOR — `evaluate` reads list_agents twice, loops every agent firing 3+ queries each.
- **`crates/relix-runtime/src/knowledge/trust.rs:236`** — MAJOR — `evict_if_needed` calls `store.list(...10_000, 0)`.
- **`crates/relix-runtime/src/knowledge/service.rs:608, 611`** — MAJOR — `list_shared` calls `list(...10_000, 0)` then filters in Rust.

### 5.3 Unbounded growth / no eviction

(Cross-listed with Angle 3.3.)

- **`crates/relix-runtime/src/metrics/alert_delivery.rs:329`** — MAJOR — Unbounded `tokio::spawn` per channel-per-event.
- **`crates/relix-runtime/src/metrics/query.rs:184`** — MAJOR — `agent_summary` pulls every row into Rust memory; 10M-row 30d window OOMs.
- **`crates/relix-runtime/src/metrics/query.rs:308`** — MAJOR — `timeseries` pre-seeds 43,200 buckets even with no data.

### 5.4 Self-consistency / sampling guard

- **`crates/relix-runtime/src/confidence/self_consistency.rs:140`** — MAJOR — `min_score_to_enable` from operator config with no clamp; negative → SC never triggers (or always triggers).
- **`crates/relix-runtime/src/confidence/self_consistency.rs:182`** — MINOR — `(score * 1e6) as u64` accumulates lossily.
- **`crates/relix-runtime/src/metrics/spike_detector.rs:198`** — MAJOR — Baseline = current window if no history → first tick's spike pollutes baseline forever.
- **`crates/relix-runtime/src/metrics/spike_detector.rs:233`** — MAJOR — `drift_threshold = 0.0` + baseline = 0 → fires on EVERY tick.

### 5.5 Hot-path allocs / inefficient code

- **`crates/relix-runtime/src/dispatch/mod.rs:1378`** — MINOR — `record_metric` per request before audit write.
- **`crates/relix-runtime/src/dispatch/mod.rs:1326, 1334`** — MINOR — `req.tenant_id.clone()` and `ctx.clone()` unconditional.
- **`crates/relix-runtime/src/dispatch/mod.rs:1473`** — MAJOR — Linear scan over policies per low-confidence call.
- **`crates/relix-runtime/src/core/policy.rs:131`** — MAJOR — Per-method rule lookup linear `Vec::iter().any()`.
- **`crates/relix-runtime/src/sflow/executor.rs:88`** — MAJOR — Hard-coded MAX_VARS=50.
- **`crates/relix-runtime/src/workflow/executor.rs:674`** — MINOR — `Arc::new(workflow.clone())` inside parallel-fan-out clones entire Workflow per edge.
- **`crates/relix-core/src/redact.rs:251, 354`** — MAJOR — O(N×M) naive search; use memchr/aho-corasick.
- **`crates/relix-core/src/redact.rs:255`** — MAJOR — `to_ascii_lowercase()` on entire input per call.
- **`crates/relix-runtime/src/transport/rpc.rs:367`** — MINOR — `cmd_tx` cap 64; `respond_tx` shares channel → outbound burst stalls inbound responses.

### 5.6 Background task worst case

- **`crates/relix-runtime/src/metrics/cost_baseline.rs:387`** — MAJOR — `format!("DELETE FROM {table}")` table-name SQL pattern.
- **`crates/relix-runtime/src/observability/sinks.rs:367`** — MAJOR — `record_event` logs failures with event_id; fail-open path.

### 5.7 Confidence / fallback engine

- **`crates/relix-runtime/src/dispatch/mod.rs:1480`** — MAJOR — Retry loop holds worker for 8 × handler latency without re-check of `req.deadline`.

### 5.8 Stream throughput

- **`crates/relix-runtime/src/transport/stream.rs:115`** — MINOR — `Chunk(ByteBuf)` 1 MiB cap; no per-stream total byte cap.
- **`crates/relix-runtime/src/transport/stream.rs:121`** — MINOR — `Err.cause: String` unbounded.

---

## Angle 6 — Integration gaps: systems that are supposed to talk but do not

### 6.1 The approval delivery matrix

**Claim:** Telegram/Slack/Email/dashboard are wired.
**Reality:** `crates/relix-runtime/src/approval/delivery.rs:436` — the **only `ChannelDispatch` impl is `LogChannelDispatch` which writes one `tracing::info!` line**. No HTTP. No bot API. No webhook. Operator wiring `channel = "telegram"` with a `webhook_url` gets a log line, not a message.

The transport plumbing (`crates/relix-telegram`, `crates/relix-slack`, `crates/relix-discord`) exists and contains correct HTTPS calls — but those crates are NOT wired to the approval delivery system. Their existence misleads operators into assuming the wire is present.

### 6.2 Credential rotation scheduler → alert sink

**Claim:** Rotation events notify via MultiChannelAlertSink.
**Reality:** `crates/relix-runtime/src/credentials/scheduler.rs:107` — when sweep_once fails, it `tracing::warn!`s and returns empty. No notification reaches any sink. The `RotationNotifier` interface exists but the default `LogRotationNotifier` is the only one in the box.

### 6.3 Belief-state lazy-load from memory peer

**Claim:** Belief state lazy-loads from LayeredMemoryStore on cache miss.
**Reality:** If the memory peer is not yet initialised when first request arrives, `knowledge/remote.rs:266` `LateBoundDispatcher` returns `Unreachable { detail: "knowledge mesh dispatcher not yet wired" }` silently, with no warn/timeout/alert.

### 6.4 Judge model → POLICY_DENIED

**Claim:** Judge sends POLICY_DENIED error when blocking.
**Reality:** `planning/critic.rs:285` — when dispatcher returns Err (including POLICY_DENIED), critic records `__critic_unreachable__` and exits. `planning/verification.rs:303` — judge falls back to `(true, "judge unreachable — assumed pass with caveat")`. **Required critical step gets PASS by default when judge is unreachable or denied.**

### 6.5 Tier router fallback

**Claim:** Falls back Simple→Medium→Complex when provider unhealthy.
**Reality:** The audit found no proactive health-check; the system tries the provider, waits for failure, and then falls back. Fallback logic via `manifest::find_alias_for_method` returns the FIRST alias advertising a method by BTreeMap NodeId order — non-deterministic, no priority list, no health awareness.

### 6.6 Multi-tenant Qdrant auto-creation

**Claim:** Per-tenant Qdrant collections created automatically on first write.
**Reality:** Within the bridge, only 1 of 45 mesh-calling handlers propagates the tenant header (`memory_gap5.rs`). For the other 44 handlers, all data merges into the `"default"` tenant.

### 6.7 Bridge ⇄ AI Provider keys

**Claim:** Bridge holds NO provider keys; only the AI node does.
**Reality:** `crates/relix-web-bridge/src/config_api.rs:403-528` — bridge reads its own provider api_key secret via `bridge-secrets.toml` and dials `api.openai.com`/`api.anthropic.com` directly. Threat-model existential property violated.

### 6.8 `verify_on_dispatch` flag → DispatchBridge

**Claim:** `identity/session.rs:42` says `verify_on_dispatch = true` enforces inbound session-token verification.
**Reality:** The flag is parsed but never read by any code path. Flipping it does nothing.

### 6.9 Bridge auth advertised but partial

**Claim:** Bridge ships bearer token auth on every protected route.
**Reality:** Three bypass routes ship in production: (a) `/v1/auth/token` accepts any local connection with no Origin (auth.rs:217); (b) WebSocket upgrade accepts any non-empty bearer (ws.rs:206); (c) OpenAI shim `/v1/chat/completions` accepts any non-empty bearer (auth.rs:284). Plus `?token=` query fallback for SSE writes tokens into access logs.

### 6.10 Conformance harness

**Claim:** `specs/README.md:31` — "amendment PR updates the spec, conformance/ vectors, and CHANGELOG-SPEC.md."
**Reality:** `conformance/` contains three EMPTY directories and zero vectors. No CI step invokes it.

### 6.11 8 error_kinds declared but never emitted

`crates/relix-core/src/types.rs:175-218` declares: `CREDENTIAL_EXPIRED`, `CAPABILITY_DEPRECATED`, `CAPABILITY_REMOVED`, `REPLAY_REJECTED`, `VERSION_MISMATCH`, `APPROVAL_TIMEOUT`, `APPROVAL_DENIED`, `MANIFEST_STALE`. Grep finds zero emit sites.

### 6.12 Two parallel approval systems

`crates/relix-runtime/src/approval/mod.rs:33` admits this explicitly: there is `crate::planning::approval` (plan approval) AND `crate::approval` (request approval). Decisions in one do not propagate to the other.

### 6.13 OpenAI shim system-message drop

`crates/relix-web-bridge/src/openai.rs:30` — system messages and tool-call payloads dropped silently. The shim looks like OpenAI but isn't.

### 6.14 Coordinator stub

`crates/relix-runtime/src/coordinator/mod.rs:8` — 8-line `CoordinatorStub`. Module header lies. Entire RELIX-3/RELIX-8 spec is unimplemented.

### 6.15 NodeManifest unsigned

`crates/relix-runtime/src/manifest/mod.rs:42` — manifests are plain CBOR. Spec RELIX-5 §5.2.2 mandates signing. Any peer can claim any org_id and any capabilities.

### 6.16 Stream protocol id mismatch

`crates/relix-runtime/src/transport/stream.rs:83` — uses `/relix/rpc/stream/1`; spec `/relix/stream/1`. Strict-spec peer rejects upgrade.

### 6.17 Manifest staleness

`error_kinds::MANIFEST_STALE` constant exists; emission site does not. Stale manifests are served until next refresh.

### 6.18 Embedded vs bridge

Detailed in Angle 2.8. The 8 divergences listed there are integration gaps for any developer porting code between embedded and bridge.

### 6.19 Rust SDK ⇄ bridge

Detailed in Angle 2.1. Three of four endpoints are broken wire-shapes; one (chat_stream) yields zero chunks.

### 6.20 CLI ⇄ bridge

`relix memory list/show/search/invalidate/stats` defaults to port 9100 (closed). Every memory inspect command hits the wrong port on first use.

---

## Angle 7 — The real user experience

### Scenario A — New developer follows the README today

1. Visit GitHub → see `curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash` (`README.md:48`). Pulls from `main` not a release tag, no signature.
2. `install.sh` resolves a latest GitHub release. Downloads `relix-x86_64-...tar.gz` and extracts to `~/.local/bin/`. Tar-Slip vulnerable; `chmod +x` and copy any binary in archive.
3. PATH update — on a factory-clean macOS without `.zshrc`, the rc-line append silently skips → `relix --version` returns "command not found". The script's PATH check is a warning, not fatal.
4. Wizard launches via `</dev/tty` redirection. User picks `mock` provider as default. Wizard writes config.
5. `relix boot` starts services. Whether `~/.relix/bridge-token` is correctly written depends on which `data_dir` resolved.
6. Following `docs/getting-started.md` step "First chat": `curl -X POST http://127.0.0.1:19791/chat -d '...'`. **Returns 401** because no bearer header was supplied.
7. User reads the doc again, sees nothing about auth. Tries Python: `client = relix.RelixClient()` with no `api_key`. Falls back to silent failure pattern. Pydantic ValidationError may also escape.
8. User tries the OpenAI shim: works because that endpoint accepts ANY non-empty bearer. They think Relix is working. Never notice every other endpoint requires the real bridge token.
9. User tries `relix memory list`: hits port 9100 (closed). Hangs/errors.
10. User tries the WebSocket example: `new WebSocket(url, [], { headers })` third arg ignored in browser → 401.
11. User tries "From source": `git clone && cargo build --workspace && relix setup` — binary is `target/debug/relix-cli`, not `relix`. Command not found.

**Where they get stuck:** every example in the docs is broken at step 7+. They either give up or guess at the OpenAI shim (which works) and miss every other feature.

### Scenario B — Operator with always_require_methods + Telegram, agent makes 20 calls, operator asleep

1. Agent fires tool call #1. Bridge detects method in `always_require_methods`. Approval row upserted.
2. `on_require_approval` synchronously calls Telegram channel adapter. **Critical truth: the only `ChannelDispatch` impl is `LogChannelDispatch`.** This emits a `tracing::info!` line. **The operator's phone does not buzz.** The agent's call hangs awaiting approval.
3. Even if a real telegram adapter were wired, `crates/relix-telegram/src/lib.rs:54` shows `OutgoingMessage` has no `reply_markup` field. Message would be plain text — no buttons.
4. Tool calls #2..#20 each go through the same path — each mints its own approval row, each fires its own (logged-only) notification. **No batching.**
5. Each spawned escalation timer is fire-and-forget without a JoinHandle. If the controller process dies, all 20 escalations vanish.
6. The operator is asleep. Each approval has its own `escalation_timeout_secs`. When the timer fires, `dispatch_request` triggers the escalation_channel — also `LogChannelDispatch`. Phone never buzzes during escalation either.
7. Each agent call still hangs. `wait_for_approval` polls every 2s up to 300s — 150 SQLite reads per approval before timing out.
8. After 300s × 20 calls = 100 minutes, all approvals time out by polling expiry. None recorded as "denied by escalation expiry" because `decide_approval` setter has no `WHERE status='pending'` guard.
9. Operator wakes. Dashboard reports `escalation_scheduled: true` for every approval, even though the channel was never actually dispatched.
10. Operator types `/approve all` into their (non-existent) inbound webhook handler. Telegram bot crate at `live.rs:333` hard-codes `allowed_updates = &["message"]` — callback queries silently dropped. There is no way for the operator to bind a chat reply to an approval ID.

**Net outcome:** every approval is a silent void. The agent's 20 calls eventually time out. The operator sees logs but no phone messages and has no real way to approve from chat. **The advertised approval flow is non-functional.**

### Scenario C — Python SDK chatbot, 30 days

1. User imports `relix.RelixClient(bridge_url=..., api_key=...)`.
2. They loop handling user messages; each → `client.chat(session_id=user_id, message=...)`.
3. After 100 messages: 100 chat rows in `dispatch_audit`, 100 in `memory_observations`, 100 entries in `HistoryStore.sessions[user_id]` (capped to `HISTORY_MAX_TURNS=20` but trim is off-by-one). Per-session orphan "user:" lines accumulate.
4. Background tasks: cost_baseline, alert engine, autoshare cursor, idle-sweep, rotation scheduler. Each is fire-and-forget; if any panics, it stops silently — the chatbot continues to work.
5. After a week: **policy cache** grows by tenant_ids with no eviction.
6. **capability_stats HashMap** grows with every unknown method name an attacker pings.
7. **session_tokens** verify path: full table read per verification. After 100,000 messages, every verify is O(100,000).
8. **OtelExporter** builds a new reqwest client every flush. 5s × 24h × 30d = 518,400 TLS handshakes.
9. **knowledge::evict_if_needed** caps via `store.list(...10_000, 0)`; past 10k records on a single agent, eviction silently never fires.
10. **PII gate** scans args via `req.args.to_vec()` per request — 100ms × 30M messages.
11. SQLite databases grow with no documented compaction. No documented retention. No disk-full handling — audit writes `tracing::error!` and the response still returns 200.
12. After a month: SQLite combined sizes likely > 10 GB. Read latency on `agent_summary` jumps from ms to seconds.

**Net outcome:** chatbot nominally works after a month, but response times have degraded by 10-50× and disk consumption is uncontrolled. No alert ever fires.

### Scenario D — Adversarial PDF with hidden prompt injection

1. Attacker uploads `evil.pdf` via `/v1/memory/ingest`.
2. **Tenant middleware**: only `memory_gap5.rs` propagates tenant — this handler does propagate, but the validation that the caller is authorised to act in that tenant (per dispatch.rs:1142) is missing.
3. PDF parsed by tiered parser. Hidden text becomes a chunk in `memory_observations`.
4. **No perception-security boundary**: nothing wraps the chunk in `BEGIN UNTRUSTED DATA / END UNTRUSTED DATA`. Stored as plain text.
5. **No PII outbound scan**: PII gate scans args but not response bodies.
6. Later, an agent's memory.dialectic or memory.search returns the malicious chunk. The chat flow includes it verbatim in the prompt.
7. The LLM complies. Output has `confidence: 1.0`. High baseline disables further sampling.
8. The structured response includes JSON with an `approval_token` field. Subsequent dispatch (`dispatch/mod.rs:1114`) accepts ANY non-empty token. Tool calls bypass approval.
9. If the LLM rewrites a research profile, the synthesized profile is written with operator-trusted confidence values.
10. **`evaluate_pattern_match`** silently passes on regex compile failure — judge verification no-op.
11. **`ai judge`** assumes pass on dispatcher Err — explicit POLICY_DENIED becomes a pass.

**Worst case:** the PDF can grant itself an approval token, rewrite agent identity, silence verification, and avoid PII redaction on the way out.

---

## Appendix A — relix-core remaining entries

```
crates/relix-core/src/bundle.rs:194 | MAJOR | CORRECTNESS | as_secs() as i64 cast wraps for SystemTime far in future; unwrap_or(0) hides clock-not-set states; bundles signed with not_before=-30, not_after=lifetime_secs | propagate SystemTime error or panic loudly on epoch
crates/relix-core/src/bundle.rs:172 | MINOR | SECURITY | Hardcoded 30-second SKEW_SECS not configurable per deployment | promote to config / constructor parameter
crates/relix-core/src/bundle.rs:197 | MINOR | SECURITY | rand::thread_rng() for bundle_serial — readers cannot verify CSPRNG | switch to OsRng explicitly
crates/relix-core/src/bundle.rs:194 | MINOR | OPERATOR | .unwrap_or(0) on SystemTime emits 1970 timestamps with no log signal | tracing::error! before fallback
crates/relix-core/src/eventlog.rs:113 | MAJOR | PERFORMANCE | verify_chain reads every record from disk on open — O(N) re-verify per flow log on every open with no caching of last verified seq | persist (seq, last_hash) in sidecar
crates/relix-core/src/eventlog.rs:158 | MINOR | CORRECTNESS | EventRecord.signature signed-over input but UnsignedRecord parallel struct; nothing enforces parity | use #[serde(skip)] on signature + struct-level signing helper
crates/relix-core/src/eventlog.rs:274 | MINOR | DEV | let _ = last_record_bytes; dead code to silence unused-var | remove the variable
crates/relix-core/src/audit.rs:241 | MAJOR | SECURITY | Unbounded record count in read_audit_records — multi-million-record log OOMs operator tools | stream or cap
crates/relix-core/src/audit.rs:114 | MAJOR | PERFORMANCE | AuditLog::open runs full chain verify on every responder start | persist last-verified prev_hash + seq in sidecar
crates/relix-core/src/audit.rs:289 | MINOR | PERFORMANCE | verify_audit_chain re-encodes each record to compute chain hash; doubles CPU cost vs reusing on-disk bytes | reuse bytes from read_audit_records
crates/relix-core/src/audit.rs:54 | MINOR | DEV | UnsignedAudit parallel struct to AuditRecord — adding a field requires updating both in lockstep | use #[serde(skip)] on signature + sign full struct
crates/relix-core/src/policy.rs:288 | MAJOR | PERFORMANCE | engine_for_tenant calls std::fs::metadata + read_to_string synchronously inside Mutex lock scope — blocks every other tenant's evaluate | drop lock before disk I/O
crates/relix-core/src/policy.rs:319 | MAJOR | PERFORMANCE | list_tenants does read_dir blocking on the same thread | wrap with spawn_blocking
crates/relix-core/src/policy.rs:284 | MAJOR | CORRECTNESS | at.elapsed() < self.ttl uses Instant correctly but combined with unbounded cache, stale entries linger forever | add max cache size + periodic prune
crates/relix-core/src/policy.rs:97 | MINOR | OPERATOR | TOML parse error stringified loses line/col info | keep structured toml::de::Error
crates/relix-core/src/policy.rs:149 | MINOR | OPERATOR | Deny reason includes caller.groups (Debug-formatted) which can include unredacted operator-set strings | run through crate::redact::redact_secrets
crates/relix-core/src/policy.rs:97 | MINOR | DEV | from_toml/from_path return PolicyError but no schema validation — typo [[rule]] silently produces zero rules; default-deny bricks every call | validate at least one of admit/rules is non-empty
crates/relix-core/src/policy.rs:131 | MINOR | CORRECTNESS | Loop returns FIRST matching rule; order matters silently; operator reordering changes admission outcome with no warning | document order semantics in doc-comment
crates/relix-core/src/identity.rs:144 | MAJOR | CORRECTNESS | as_secs() as i64 from SystemTime — same wrap + unwrap_or(0) hides clock-not-set | propagate or panic loud
crates/relix-core/src/identity.rs:97 | MINOR | OPERATOR | IdentityError::OrgMismatch carries no payload — operator hunting bug can't see expected vs actual org id | include both ids in variant
crates/relix-core/src/identity.rs:33 | MINOR | SECURITY | supervisors: Vec<String> documented "empty for the alpha (approval flows deferred)" — stub field on wire that runtime doesn't enforce | add validate_supervisors_empty invariant or feature-gate
crates/relix-core/src/codec.rs:28 | MINOR | PERFORMANCE | Vec::with_capacity(128) too small for most bundles; many resizes during encode | inline-grow strategy or per-type capacity
crates/relix-core/src/types.rs:52 | MINOR | SECURITY | RequestId::new() uses rand::thread_rng() rather than explicit OsRng | switch to OsRng for clarity
crates/relix-core/src/types.rs:83 | MINOR | SECURITY | TraceId::new() same thread_rng() issue | same
crates/relix-core/src/types.rs:114 | MINOR | SECURITY | FlowId::new() same thread_rng() issue | same
crates/relix-core/src/types.rs:175 | MINOR | DEV | #[allow(missing_docs)] on error_kinds mod hides missing docs for every constant — masks new undocumented kinds | doc each constant individually
crates/relix-core/src/types.rs:170 | MINOR | CORRECTNESS | ErrorEnvelope.retry_hint: u8 magic-number contract (0/1/2/3) but no enum — wire-typo at 4 means "do something" but no rejection | use #[repr(u8)] enum with try_from
crates/relix-core/src/redact.rs:251 | MAJOR | SECURITY | redact_inline_secret value-match accepts only body-charset chars — value with comma/brace/delimiter truncated mid-value, suffix unredacted | match until line break or quoted region end
crates/relix-core/src/redact.rs:289 | MAJOR | SECURITY | Quote handling: mid-value escape sequences \" inside value would split secret on first ", leaving tail leaked | track escape state
crates/relix-core/src/redact.rs:255 | MINOR | CORRECTNESS | bytes_lower and bytes_orig indexing assumes same byte length — only safe for ASCII | document ASCII-only or use char_indices
crates/relix-core/src/redact.rs:99 | MINOR | CORRECTNESS | redact_prefixed_token returns input unchanged when too short but never anchors at start; offset slip means string ending in matching prefix is reported as no-match | single full-scan path
crates/relix-core/src/redact.rs:62 | MINOR | DEV | redact_secrets repeatedly clones string at each pattern stage — N passes = N allocs | combine into single-pass aho-corasick
crates/relix-core/src/retry.rs:51 | MAJOR | CORRECTNESS | next_delay uses thread_rng for jitter — Backoff state updates; never decays prev if calls infrequent | time-decay or reset on idle
crates/relix-core/src/retry.rs:80 | MINOR | SECURITY | rng.gen_range with thread_rng() — fine for jitter but uncommented | comment explicitly
crates/relix-core/src/retry.rs:52 | MINOR | DEV | Backoff is Copy+Clone but holds RNG-driven state — copying branches state, easy footgun | drop Copy, keep Clone only
crates/relix-core/src/router.rs:28 | MAJOR | CORRECTNESS | timestamp: u64 for heartbeat — inconsistent with Timestamp (i64) elsewhere; type confusion at boundary | use shared Timestamp type
crates/relix-core/src/router.rs:20 | MAJOR | SECURITY | HeartbeatRequest.peer_id: String is caller-supplied and unauthenticated at this layer | document upstream requirement or use PeerIdClaimed(String) newtype
crates/relix-core/src/router.rs:40 | MAJOR | CORRECTNESS | HeartbeatResponse.peers: Vec<PeerSummary> unbounded — router with thousands of peers ships massive responses; no pagination | add peers_truncated: bool + cap
crates/relix-core/src/router.rs:104 | MINOR | CORRECTNESS | SessionListRequest.limit/offset: Option<usize> — caller can pass usize::MAX and DOS the router | cap limit at 1000 in constructor
crates/relix-core/src/router.rs:130 | MINOR | CORRECTNESS | SessionRecord.status: String allows arbitrary text where spec says "running"|"completed"|"failed" | use enum with serde
crates/relix-core/src/router.rs:142 | MINOR | CORRECTNESS | LogRequest.level: String — same string-where-enum issue | use enum
crates/relix-core/src/router.rs:145 | MAJOR | SECURITY | LogRequest.message: String "free-form; redaction is the source's responsibility" — nothing validates | apply redact_secrets defensively at router
crates/relix-core/src/capability.rs:155 | MAJOR | OPERATOR | risk_level: RiskLevel::Unknown is default — validator in another crate; module ships dangerous defaults without guard | add is_audited(&self) -> bool; warn-on-encode
crates/relix-core/src/capability.rs:102 | MINOR | SECURITY | sensitivity_tags: Vec<String> free-form — typos yield silent default-deny | pre-define closed set with &'static str constants
crates/relix-core/src/lib.rs:42 | MINOR | DEV | pub type Result<T> = std::result::Result<T, CoreError>; collides with std::Result; trivial footgun | rename to CoreResult
crates/relix-core/src/lib.rs:46 | MINOR | DEV | CoreError not marked non_exhaustive — adding variant in point release is breaking change | #[non_exhaustive] enum
crates/relix-core/Cargo.toml:13 | MINOR | DEV | missing_docs = "warn" sub-strict — undocumented public APIs ship without CI failure | promote to "deny"
crates/relix-core/Cargo.toml:26 | MINOR | DEV | clippy::unwrap_used and expect_used set to warn (not deny) — determined dev can ignore | promote to "deny"
```

---

## Appendix B1 — dispatch + coordinator remaining

```
crates/relix-runtime/src/dispatch/mod.rs:962 | MINOR | CORRECTNESS | When envelope decode fails, synthetic response uses RequestId([0u8;16]) — caller correlation breaks; same 0u8;16 rid aliases all decode-failed responses | use freshly-generated rid + rid_unknown=true field
crates/relix-runtime/src/dispatch/mod.rs:984 | MAJOR | CORRECTNESS | identity admission has NO replay-protection step | add bounded RID cache scoped to trust root; emit REPLAY_REJECTED
crates/relix-runtime/src/dispatch/mod.rs:972 | MAJOR | SECURITY | Deadline grace hard-coded to +30s — gives 30s replay window beyond every deadline | make grace configurable
crates/relix-runtime/src/dispatch/mod.rs:1302 | MAJOR | PERFORMANCE | tokio::time::sleep on dispatch hot path while holding inbound future | cap concurrent throttles via semaphore + propagate cancellation
crates/relix-runtime/src/dispatch/mod.rs:1480 | MAJOR | PERFORMANCE | apply_confidence retry loop sleeps per retry; MAX_RETRY_CAP=8 holds worker for 8x latency without re-check of req.deadline | re-check deadline between retries
crates/relix-runtime/src/dispatch/mod.rs:1465 | MINOR | DEV | Dead branches: 1466+1468 set threshold=0.5 in both arms; verdict.critical unused | honor critical with higher threshold or drop
crates/relix-runtime/src/dispatch/mod.rs:1457 | MINOR | DEV | FallbackAction::Pass arm inside match at 1456 unreachable — early-return at 1448 filtered Pass | drop redundant arm
crates/relix-runtime/src/dispatch/mod.rs:1551 | MINOR | CORRECTNESS | SafeDefault swaps body and sets confidence=1.0; dashboards see perfect confidence on synthetic body | stamp original score or sentinel < 1.0
crates/relix-runtime/src/dispatch/mod.rs:1589 | MINOR | CORRECTNESS | Abort uses INVALID_ARGS for low-confidence — semantically wrong; alerts on INVALID_ARGS get false positives | use RESPONDER_INTERNAL or new LOW_CONFIDENCE_ABORT kind
crates/relix-runtime/src/dispatch/mod.rs:1406 | MAJOR | CORRECTNESS | codec::encode(&resp).unwrap_or_default() on success silently returns empty Vec if encoding fails | log + emit synthetic RESPONDER_INTERNAL
crates/relix-runtime/src/dispatch/mod.rs:2356 | MAJOR | CORRECTNESS | Same silent unwrap_or_default in encode_error_response | same fix
crates/relix-runtime/src/dispatch/mod.rs:2418 | MAJOR | CORRECTNESS | build_request().unwrap_or_default() lets callsite send vec![] over wire if local encode fails | return Result, fail loudly
crates/relix-runtime/src/dispatch/mod.rs:2452 | MAJOR | CORRECTNESS | Same in build_request_with_tenant | same fix
crates/relix-runtime/src/dispatch/mod.rs:1631 | MAJOR | CORRECTNESS | Success path stamps confidence on envelope while every error envelope hard-codes confidence: None; score silently dropped on retried/scored Err outcomes | plumb verdict into error envelope
crates/relix-runtime/src/dispatch/mod.rs:2294 | MINOR | CORRECTNESS | unix_now_ms saturates at i64::MAX but unix_now returns 0 on epoch error — inconsistent strategy | pick one strategy
crates/relix-runtime/src/dispatch/mod.rs:2207 | MAJOR | CORRECTNESS | Streaming caller drop mid-stream sets cancelled_by_caller=true then immediately discards via let _ = at 2203; never tags audit decision with cancellation | include flag in final_decision
crates/relix-runtime/src/dispatch/mod.rs:2143 | MAJOR | CORRECTNESS | When handler returns Err before producing any frame, streaming writes StreamFrame::Err but ALREADY sent Header — Header+Err with no Chunks+End | document or shift Header write to after first chunk
crates/relix-runtime/src/dispatch/mod.rs:2110 | MAJOR | CORRECTNESS | Streaming Header.processed_at = Timestamp(now) is wall-clock seconds; unary uses Timestamp::now() (sub-second); wire shape diverges | use same source
crates/relix-runtime/src/dispatch/mod.rs:1681 | MAJOR | DEV | Streaming admission pipeline duplicated line-for-line from unary | extract run_admission(envelope) -> AdmissionOutcome shared by both paths
crates/relix-runtime/src/dispatch/mod.rs:1611 | MAJOR | CORRECTNESS | audit_and_err consumes req by value — leaks one RequestEnvelope per failed call | take by reference like audit_and_err_unverified
crates/relix-runtime/src/dispatch/mod.rs:1019 | MAJOR | OPERATOR | Agent gate first integration point that consults verified.name/subject_id — no recheck that trust root for THIS dispatch instance is same as the one that signed the bundle | verify trust_root remains single key throughout request
crates/relix-runtime/src/dispatch/mod.rs:1611 | MINOR | DEV | audit_and_err, audit_and_err_unverified, audit_and_err_with_id near-clones | consolidate to one audit_and_err(req, Option<&VerifiedIdentity>, ...)
crates/relix-runtime/src/dispatch/mod.rs:534 | MAJOR | OPERATOR | set_always_require_methods takes &mut self — DispatchBridge held as Arc everywhere; reload-without-restart impossible | wrap hot-reload state behind ArcSwap/RwLock
crates/relix-runtime/src/dispatch/mod.rs:556 | MAJOR | OPERATOR | Same &mut self problem for set_tenant_policy_resolver, set_audit_partition_store, set_budget_enforcer, set_pii_gate, set_confidence, set_last_confidence_cell, set_alert_pipeline, set_metrics_sink, set_agent_gate, set_access_broker — none hot-reload-safe | wrap mutable knobs in ArcSwap
crates/relix-runtime/src/dispatch/mod.rs:223 | MAJOR | OPERATOR | DispatchBridge has NO shutdown method | add async fn shutdown(&self)
crates/relix-runtime/src/dispatch/mod.rs:740 | MAJOR | INTEGRATION | sink.record_invocation sync on hot path; even non-blocking sink takes lock to enqueue | bounded channel inside sink + document latency budget
crates/relix-runtime/src/dispatch/mod.rs:1377 | MAJOR | INTEGRATION | record_metric happens BEFORE write_audit; audit failure creates metric for a success that has no audit row | gate metric on audit success
crates/relix-runtime/src/dispatch/mod.rs:1335 | MINOR | CORRECTNESS | min(u64::MAX as u128) as u64 no-op on 64-bit; intent obscured | use u64::try_from(...).unwrap_or(u64::MAX)
crates/relix-runtime/src/dispatch/mod.rs:1485 | MINOR | CORRECTNESS | Same u128->u64 cast pattern in retry loop | same fix
crates/relix-runtime/src/dispatch/mod.rs:1510 | MINOR | CORRECTNESS | Same in escalate path | same
crates/relix-runtime/src/dispatch/mod.rs:2119 | MINOR | CORRECTNESS | Same in streaming write_frame branch | same
crates/relix-runtime/src/dispatch/mod.rs:2183 | MINOR | CORRECTNESS | Same in streaming final elapsed_ms | same
crates/relix-runtime/src/dispatch/mod.rs:2256 | MINOR | CORRECTNESS | started.elapsed().as_millis() as u64 no saturation; months-long requests overflow | use saturating pattern
crates/relix-runtime/src/dispatch/mod.rs:2236 | MAJOR | CORRECTNESS | Audit id = req.rid (caller-controlled) — aid uniqueness depends on attacker; log injection possible | derive aid from server-side hash chain
crates/relix-runtime/src/dispatch/mod.rs:2326 | MINOR | DEV | error_kind_to_str has no entry for APPROVAL_TOKEN_INVALID; catchall OTHER at 2331 strips numeric codes | format unknowns as "UNKNOWN_{n}"
crates/relix-runtime/src/dispatch/mod.rs:1473 | MAJOR | PERFORMANCE | engine.list().iter().find(...) linear scan of every fallback policy per low-confidence call | pre-index policies by capability glob
crates/relix-runtime/src/dispatch/mod.rs:1602 | MINOR | CORRECTNESS | publish_confidence writes last score even after Abort (best_score=0.0) | stamp sentinel or skip publish on Abort
crates/relix-runtime/src/dispatch/mod.rs:2105 | MAJOR | CORRECTNESS | Streaming aid = req.rid.0.to_vec() BEFORE audit row finalised; if audit write fails, aid corresponds to no actual audit record | defer aid write until after audit commit
crates/relix-runtime/src/dispatch/mod.rs:251 | MAJOR | OPERATOR | policy_denials ring + capability_stats both process-local with NO persistence — bridge restart loses every denial / latency sample | mirror to audit partition store
crates/relix-runtime/src/dispatch/mod.rs:344 | MAJOR | DEV | always_require_methods: Vec<String> accepts duplicates silently — no normalisation, no dedup | convert to HashSet or dedupe on set
crates/relix-runtime/src/dispatch/mod.rs:224 | MAJOR | CORRECTNESS | handlers: HashMap<String, Arc<dyn Handler>> — same key space as streaming_handlers; register just insert() with no conflict check | register returns Result, check both maps at registration time
crates/relix-runtime/src/dispatch/mod.rs:2418 | MAJOR | CORRECTNESS | Build helpers swallow errors via unwrap_or_default | return Result
crates/relix-runtime/src/dispatch/mod.rs:1260 | MINOR | PERFORMANCE | req.rid.to_string() allocates per request for PII gate session_id | pre-compute or pass raw bytes/hex
crates/relix-runtime/src/dispatch/mod.rs:1334 | MINOR | PERFORMANCE | ctx.clone() unconditional even when no fallback engine wired — wastes alloc per request | move clone inside apply_confidence
```

---

## Appendix B2 — transport + admission + approval remaining

```
crates/relix-runtime/src/transport/rpc.rs:249 | MAJOR | CORRECTNESS | OutboundFailure collapses every error (DialFailure / Timeout / UnsupportedProtocols / ConnectionClosed) into Err(format!("{error:?}")); TransportError vs ApplicationError conflated | structured RpcError { Transport(_), Application(_) }
crates/relix-runtime/src/transport/rpc.rs:257 | MAJOR | OPERATOR | InboundFailure { .. } => {} silently drops inbound delivery errors | log + bump counter
crates/relix-runtime/src/transport/rpc.rs:280 | MAJOR | OPERATOR | OutgoingConnectionError { .. } => {} swallows dial failures; no reconnect | wire reconnect backoff + log
crates/relix-runtime/src/transport/rpc.rs:201 | MAJOR | CORRECTNESS | Event loop has NO reconnect/retry; known peer disconnect, no automatic redial | tracked-peer set + bounded exponential backoff redialer
crates/relix-runtime/src/transport/rpc.rs:286 | MINOR | CORRECTNESS | Dial sends Err(format!("{e:?}")) Debug-formatting on DialError | use Display + structured error
crates/relix-runtime/src/transport/rpc.rs:67 | MAJOR | CORRECTNESS | from: PeerId on Event::Request — Noise XK only authenticates responder; no cross-check that inbound request.identity_bundle matches the from PeerId | document gap; have dispatch cross-check identity_bundle pubkey against PeerId
crates/relix-runtime/src/transport/rpc.rs:143 | MINOR | CORRECTNESS | Client::call returns Result<Vec<u8>, String> — stringly-typed error loses structure | return structured RpcError
crates/relix-runtime/src/transport/rpc.rs:153 | MINOR | CORRECTNESS | No per-call deadline on Client::call; hung peer hangs caller forever | wrap rx.await in configurable timeout tied to envelope deadline
crates/relix-runtime/src/transport/rpc.rs:367 | MINOR | PERFORMANCE | cmd_tx channel cap hard-coded 64; respond_tx + call_tx share same channel — outbound burst stalls inbound responses | split into separate channels
crates/relix-runtime/src/transport/rpc.rs:363 | MAJOR | OPERATOR | Listen address hard-coded 127.0.0.1 — silently unreachable from outside loopback regardless of operator intent | accept listen Multiaddr as parameter
crates/relix-runtime/src/transport/rpc.rs:209 | MAJOR | CORRECTNESS | When command channel closes, loop breaks but swarm task aborts without graceful drain of pending_calls | drain pending_calls with Shutdown error before returning
crates/relix-runtime/src/transport/rpc.rs:268 | MINOR | OPERATOR | event_sender.send(...).await back-pressures swarm loop and stops processing further inbound RPCs | try_send + drop with warning
crates/relix-runtime/src/transport/stream.rs:77 | MINOR | SECURITY | MAX_FRAME_BYTES = 1MiB applies per-frame; attacker can send unbounded number of frames keeping responder pinned | per-substream total-byte and total-frame cap
crates/relix-runtime/src/transport/stream.rs:150 | MINOR | SECURITY | next_frame has no read timeout — responder that opens substream and stops writing chunks holds caller task forever | wrap in timeout
crates/relix-runtime/src/transport/stream.rs:140 | MINOR | CORRECTNESS | StreamFrame protocol comment says "MUST be first frame on every substream" but next_frame doesn't enforce | state machine: expect Header first
crates/relix-runtime/src/transport/stream.rs:121 | MINOR | CORRECTNESS | Err.kind: u32 and Err.cause: String — no cap on cause length | cap cause to e.g. 1KiB at receiver
crates/relix-runtime/src/transport/envelope.rs:28 | MAJOR | SECURITY | args: ByteBuf has no size cap at envelope level | enforce capability-scoped or transport-global max arg size
crates/relix-runtime/src/transport/envelope.rs:41 | MAJOR | SECURITY | surface: Option<String> documented "operator-asserted (not cryptographically proven)" — agent_gate uses for surface_allowlist; malicious peer can lie | derive surface from authenticated peer or sign inside identity_bundle
crates/relix-runtime/src/transport/envelope.rs:49 | MAJOR | SECURITY | approval_token: Option<String> plain String in envelope; not redacted from logs; bridge does plain SQL equality lookup (not constant-time) | flag for redaction
crates/relix-runtime/src/transport/envelope.rs:31 | MAJOR | SECURITY | pv: u8 — no validation here that pv == 1; if dispatch doesn't reject pv != 1, envelope shape changes accepted silently | document validation contract or validate at decode
crates/relix-runtime/src/admission/agent_gate.rs:140 | MAJOR | CORRECTNESS | Pattern Ok(None) | Err(AgentStoreError::NotFound(_)) collapses two distinct cases; both fail with APPROVAL_TOKEN_INVALID | log underlying error separately
crates/relix-runtime/src/admission/agent_gate.rs:215 | MAJOR | SECURITY | Status string compare in match view.status.as_str() — operators can set free-form status; typo "Active" lands in deny path; converse: writing "active" lets agent escape suspension if parallel write race interleaves | gate writers to known-set; enum
crates/relix-runtime/src/admission/agent_gate.rs:336 | MAJOR | SECURITY | Approval-required fires on cap.categories.iter().any(...) — matched_category set correctly but reason on 363 uses cap.categories.first() which can disagree | use matched_category for reason
crates/relix-runtime/src/admission/agent_gate.rs:349 | MAJOR | SECURITY | store.has_active_standing(...).unwrap_or(false) falls through to RequireApproval (fail-closed for standing OK) but silently swallows error | log error before falling through
crates/relix-runtime/src/admission/agent_gate.rs:369 | MAJOR | SECURITY | approver_groups: vec!["ops".into(), "admin".into()] HARDCODED; agent profile's own approver_groups ignored | read approver_groups from AgentGateView/profile
crates/relix-runtime/src/admission/agent_gate.rs:1 | MAJOR | SECURITY | Module has NO rate limiting / token-bucket / per-tenant / per-peer / per-method admission counters | implement per-tenant + per-peer + per-method token-bucket limiter
crates/relix-runtime/src/admission/agent_gate.rs:165 | MAJOR | SECURITY | Store handle poisoned by panics in any other consumer of AgentStore mutex — once poisoned, every gate call denies, taking admission down | recover-on-poison or use parking_lot::Mutex
crates/relix-runtime/src/admission/agent_gate.rs:303 | MINOR | CORRECTNESS | Cap with zero categories paired with non-empty allow_categories goes through any returning false -> DENY; cap with zero categories + empty allow_categories falls through (allow); cap declaring no categories bypasses allow-list when allowlist empty | require capabilities to declare at least one category
crates/relix-runtime/src/admission/agent_gate.rs:318 | MINOR | CORRECTNESS | allow_sensitivity_tags check passes when cap.sensitivity_tags.is_empty() — cap with no sensitivity tags can carry sensitive data and bypass allowlist | mirror categories logic
crates/relix-runtime/src/admission/agent_gate.rs:371 | MAJOR | CORRECTNESS | task_id filtered with s.trim().is_empty() — should also reject extremely long task_ids | cap task_id length (e.g. <= 64); validate UUID/format
crates/relix-runtime/src/admission/agent_gate.rs:197 | MINOR | CORRECTNESS | record.expires_at <= now — boundary: gate uses caller-supplied inputs.now; if test path passes 0, long-lived token can be admitted | rely on single time::Clock injected for production
crates/relix-runtime/src/admission/mod.rs:1 | MAJOR | INTEGRATION | Admission module exposes ONLY agent_gate — no rate_limit, no quota, no tenant_bucket | add rate-limit module or document where it lives
crates/relix-runtime/src/approval/caps.rs:15 | MAJOR | SECURITY | register wires approval.deliver and approval.record_decision onto DispatchBridge without handler-level capability guard | tag with RiskLevel::High + categories ["approvals_admin"] for agent_gate restriction
crates/relix-runtime/src/approval/caps.rs:62 | MINOR | SECURITY | handle_status returns entire row including decision_note which may contain operator-private remarks; no scope check on caller | gate by role/group
crates/relix-runtime/src/approval/caps.rs:175 | MINOR | CORRECTNESS | decode only fails on empty args; 1MiB JSON payload happily decoded — no size cap before serde_json::from_slice -> DoS via huge JSON | cap ctx.args length before decode
crates/relix-runtime/src/approval/store.rs:43 | MINOR | PERFORMANCE | Arc<Mutex<Connection>> serializes every read/write/list call on a single mutex; under load this is hotspot | connection pool (r2d2) or RwLock around per-op connections
crates/relix-runtime/src/approval/store.rs:239 | MAJOR | OPERATOR | self.conn.lock().map_err(|_| ApprovalStoreError::Lock) — std::sync::Mutex poison aborts every subsequent store op forever | recover from PoisonError via e.into_inner() or parking_lot::Mutex
crates/relix-runtime/src/approval/store.rs:127 | MAJOR | CORRECTNESS | INSERT OR REPLACE on upsert clobbers any prior decision state — if dispatch_request called twice with same approval_id, second call resets status=pending, drops decision/decided_at_ms/decision_note, re-arms escalation timer | INSERT ... ON CONFLICT(approval_id) DO NOTHING for initial insert
crates/relix-runtime/src/approval/store.rs:206 | MINOR | CORRECTNESS | mark_escalated UPDATE doesn't return changed-count; caller has no way to know if escalation landed (could be already-decided) | return changed count
crates/relix-runtime/src/approval/store.rs:122 | MINOR | SECURITY | ensure_column formats column and column_decl directly into SQL via format! — constants today (safe) but pub-callable; SQL-injection sink if external code passes user input | document pub(crate) boundary
crates/relix-runtime/src/approval/store.rs:51 | MINOR | OPERATOR | let _ = std::fs::create_dir_all(parent); silently ignores errors | propagate io::Error
crates/relix-runtime/src/approval/store.rs:62 | MINOR | OPERATOR | open_in_memory skips log_integrity_warning but open runs it | call from both paths
crates/relix-runtime/src/approval/delivery.rs:473 | MAJOR | SECURITY | Duration::from_secs(r.escalation_timeout_secs) — operator-config-influenced; attacker setting timeouts far in future suppresses escalation; no upper cap | cap escalation_timeout_secs to sensible max at config load
crates/relix-runtime/src/approval/delivery.rs:474 | MINOR | CORRECTNESS | r.escalation_channel.expect("checked above") — fragile; future refactor could panic inside spawned task | match instead of expect
crates/relix-runtime/src/approval/delivery.rs:475 | MAJOR | SECURITY | Escalation task clones entire ApprovalRequest and keeps in memory until timer fires; flood pins unbounded escalation tasks | cap concurrent in-flight escalation timers per service
crates/relix-runtime/src/approval/delivery.rs:469 | MINOR | CORRECTNESS | escalation_scheduled silently disabled with no warning when operator sets escalation_timeout_secs > 0 but forgets escalation_channel | tracing::warn on mismatch
crates/relix-runtime/src/approval/delivery.rs:537 | MINOR | CORRECTNESS | unix_ms() returns 0 on clock-skew (unwrap_or(0)) — corrupts ordering in list ORDER BY delivered_at_ms DESC | propagate SystemTime error or use monotonic clock
crates/relix-runtime/src/approval/delivery.rs:240 | MINOR | CORRECTNESS | ChannelKind::parse(&rule.channel).unwrap_or(ChannelKind::Dashboard) — rule with unparseable channel string silently rewires to Dashboard | log warn on unknown channel name
crates/relix-runtime/src/approval/delivery.rs:200 | MINOR | OPERATOR | DeliveryError::Dispatch(String) swallows structured cause from channel impl | include ChannelKind in variant
crates/relix-runtime/src/approval/delivery.rs:209 | MINOR | SECURITY | DeliveryOutcome returned to caller includes delivery_channel, escalation_channel, delivered_at_ms; dropped into approval.deliver cap response any admitted caller can read | scope to operator-only callers
crates/relix-runtime/src/approval/delivery.rs:107 | MINOR | SECURITY | webhook_url: String read from config but slice has no place that uses it; URL held in plaintext in Arc<ApprovalDeliveryConfig> accessible via matrix.config() | redact on cap surface; treat as credential
```

---

## Appendix C — credentials/identity/manifest/plugin/metrics/observability remaining

```
crates/relix-runtime/src/identity/session.rs:312 | MAJOR | SECURITY | is_revoked() returns Ok(true) (revoked!) when row missing — but verify() path doesn't use this; hidden API risk | return Ok(Some(bool)) or distinct error for "not found"
crates/relix-runtime/src/identity/session.rs:539 | MAJOR | SECURITY | sign() and verify_signature() both .expect("HMAC accepts any key length"); construction-time check protects only at boot | return TokenVerification::invalid("internal: bad signing key") instead of panicking
crates/relix-runtime/src/identity/session.rs:546 | MAJOR | SECURITY | verify_signature uses mac.verify_slice(&sig).is_ok() — constant-time (HMAC crate); prior hex::decode exposes early-rejection timing on malformed hex | document timing model
crates/relix-runtime/src/identity/caps.rs:89 | MAJOR | SECURITY | handle_issue echoes entire SessionToken struct (including signature) in JSON response — peers with identity.issue_token cap visibility see full tokens in JSON | return only wire; drop inner token field or scrub signature
crates/relix-runtime/src/identity/caps.rs:18 | MAJOR | SECURITY | identity.issue_token registered unconditionally — no policy/group gate inside handler; any caller whose policy permits cap can mint tokens for any agent_name they pick | verify ctx.caller.name == args.agent_name OR caller in operators group inside handler
crates/relix-runtime/src/identity/research.rs:619 | MINOR | SECURITY | confidence_tag interpolated as format!("confidence:{:.2}", profile.confidence) — LLM-supplied float can be NaN/inf | clamp to [0.0, 1.0] before stringifying
crates/relix-runtime/src/identity/research_caps.rs:35 | MINOR | DEV | research_caps::handle returns RESPONDER_INTERNAL for every ResearchError, hiding Disabled / SubjectMissing | map Disabled/SubjectMissing to INVALID_ARGS
crates/relix-runtime/src/manifest/mod.rs:109 | MAJOR | CORRECTNESS | add_capability silently de-dupes by method_name — re-registering with different descriptor metadata is no-op | compare full descriptor or warn on mismatch
crates/relix-runtime/src/manifest/mod.rs:194 | MAJOR | CORRECTNESS | find_alias_for_method returns FIRST alias advertising a method — non-deterministic order from BTreeMap of NodeId strings | explicit priority/operator-defined preference list
crates/relix-runtime/src/manifest/mod.rs:486 | MAJOR | SECURITY | refresh_manifests accepts any peer that returns parseable NodeManifest — manifest's node_id trusted blindly; peer can lie about its own node_id/org_id | require manifest.node_id matches peer_id_for(alias)
crates/relix-runtime/src/manifest/mod.rs:521 | MAJOR | CORRECTNESS | looks_like_transport_break substring-matches on "io" — heuristic matches "policy_denied: peer rejected via violation"; cap-denied error retries call, doubling load | use typed error from rpc layer
crates/relix-runtime/src/plugin/dispatcher.rs:117 | MAJOR | CORRECTNESS | kind: body.error_kind.unwrap_or(11) — 11 magic for RESPONDER_INTERNAL but plugin can SEND 0 or values that collide with admission error codes | whitelist small set of allowed codes; fall back to PLUGIN_OTHER
crates/relix-runtime/src/plugin/loader.rs:165 | MAJOR | OPERATOR | BufReader::new(stdout).lines() reads UTF-8 lines — plugin writing binary on stdout before announcing port causes next_line() to return Err; useful diagnostic lost | use read_until(b'\n') and lossy-convert
crates/relix-runtime/src/plugin/loader.rs:208 | MINOR | OPERATOR | Drain task fire-and-forget tokio::spawn with no JoinHandle bound to plugin lifecycle — controller restart leaves orphans | tie drain task to oneshot cancel
crates/relix-runtime/src/plugin/manifest.rs:162 | MAJOR | CORRECTNESS | invoke_timeout_secs > 300 rejected but bound opaque — operators with legitimate slow tools can't extend | document; make configurable per-host
crates/relix-runtime/src/plugin/registry.rs:128 | MAJOR | CORRECTNESS | plugin_id_for truncates blake3 hex to 16 chars (64 bits); birthday collision at 2^32 — collisions silently overwrite via ON CONFLICT | use 32-char (128-bit) prefix
crates/relix-runtime/src/metrics/collector.rs:392 | MAJOR | CORRECTNESS | d.as_millis().min(i64::MAX as u128) as i64 clamps but unwrap_or(0) on SystemTime failure means clock-broken host writes metrics with timestamp_ms = 0 | fail loudly on clock anomaly or use monotonic offsets
crates/relix-runtime/src/metrics/store.rs:222 | MAJOR | SECURITY | column_exists uses format!("PRAGMA table_info({table})") — table hard-coded internal but pattern is SQL-injection footgun | use parameter binding or strict allowlist guard helper
crates/relix-runtime/src/metrics/query.rs:171 | MINOR | CORRECTNESS | success: r.get::<_, i64>(1)? != 0 — silently treats any non-zero as success; 2 in column treated as true | match == 1 explicitly
crates/relix-runtime/src/metrics/alert.rs:340 | MAJOR | CORRECTNESS | active.lock().unwrap_or_else(PoisonError::into_inner) — recovers but map state may be stale/torn | document or treat poisoning as fatal for state-tracking maps
crates/relix-runtime/src/metrics/alert.rs:439 | MAJOR | CORRECTNESS | baseline_rate * factor — if baseline_rate=0 and recent rate small but nonzero, drift_threshold becomes ask_human_min_recent_rate; operator setting floor 0.0 disables noise control | force minimum floor (e.g. 0.01)
crates/relix-runtime/src/metrics/alert_delivery.rs:531 | MAJOR | CORRECTNESS | iso_ms rolls custom date math (days_to_ymd) instead of using time or chrono — fragile | use time::OffsetDateTime
crates/relix-runtime/src/metrics/alert_delivery.rs:760 | MAJOR | CORRECTNESS | mp - 9 underflow in date algorithm; tests don't cover dates before 1970 | add tests for 1970-01-01 and 2099-12-31
crates/relix-runtime/src/metrics/budget.rs:611 | MINOR | CORRECTNESS | (now - entry.refreshed_at_ms) < self.inner.cache_refresh.as_millis() as i64 — as_millis() u128 cast to i64 overflows if cache_refresh is decades | saturate cast
crates/relix-runtime/src/metrics/cost_baseline.rs:130 | MAJOR | CORRECTNESS | init() executes PRAGMA journal_mode = WAL directly — bypasses crate::db::apply_pragmas; busy_timeout/synchronous/foreign_keys not set | use crate::db::apply_pragmas(&conn)?
crates/relix-runtime/src/metrics/spike_detector.rs:419 | MAJOR | CORRECTNESS | Two scheduled jobs (AlertEngine::spawn + CostSpikeDetector::spawn) read same metrics table — race when both fire at near-identical intervals; same AlertKind::ProviderCostSpike emitted from both paths — counts double | pick one canonical fire path or co-ordinate via shared dedup map
crates/relix-runtime/src/metrics/budget_coordinator.rs:36 | MINOR | DEV | budget.status returns full enforcer status — BudgetStatus includes actual_micros and limit_micros | drive by policy gate
crates/relix-runtime/src/observability/sinks.rs:179 | MINOR | CORRECTNESS | cost_cents.map(|v| v as i64) and cost_cents: r.get::<_, Option<i64>>(7)?.map(|v| v as u32) — u32 truncates above $42M cumulative | use u64
crates/relix-runtime/src/observability/otel.rs:222 | MAJOR | SECURITY | On non-success response, preview: String logged via tracing — if OTLP collector returns body containing other peers' span data, Relix tracing accidentally captures another tenant's traces | don't log response body; log status code only
crates/relix-runtime/src/observability/otel.rs:332 | MAJOR | SECURITY | render_otlp_json INFALLIBLE (returns String); if serde_json fails returns "{}" — failed render silently sends empty OTLP payload | return Result; log failure
crates/relix-runtime/src/observability/otel.rs:386 | MAJOR | CORRECTNESS | hex_pad truncates trace IDs; 16-hex span_ids at ~4 billion events: collisions guaranteed | use random ID instead of derived
crates/relix-runtime/src/observability/provenance.rs:165 | MINOR | DEV | row_to_snapshot returns Result<Result<...>> — awkward; list_recent silently propagates inner error | flatten into one Result
crates/relix-runtime/src/observability/session_debugger.rs:147 | MINOR | CORRECTNESS | event_type == "session" marker for end events — string typo or downstream rename silently breaks stall/completion detection | define constants in one place
crates/relix-runtime/src/metrics/observability.rs:248 | MINOR | CORRECTNESS | enforcer.map(|e| e.status()) calls status() which triggers refresh_window x4 — every health-summary call hits metrics store | memoize
```

---

## Appendix D — knowledge/planning/nodes/workflow/sflow/sol/yaml_flow/training/confidence/db remaining

```
crates/relix-runtime/src/db.rs:130 | MINOR | DEV | record_migration_applied INSERT OR IGNORE silently no-ops without erroring | surface Ok(bool) indicating new vs existing
crates/relix-runtime/src/db.rs:147 | MINOR | CORRECTNESS | chrono_secs_iso falls back to epoch 0 silently when SystemTime is before epoch | surface as error or log warn
crates/relix-runtime/src/audit_partition.rs:128 | MINOR | OPERATOR | conn.lock().map_err(|e| e.to_string()) returns "poisoned lock" as string; no recovery path | mirror PoisonError::into_inner pattern
crates/relix-runtime/src/knowledge/service.rs:436 | MAJOR | CORRECTNESS | Local-path insert + append_shared_with NOT in single transaction — crash between leaves receiver copy without source's shared_with updated | wrap both writes in one transaction
crates/relix-runtime/src/knowledge/service.rs:921 | MINOR | CORRECTNESS | unix_now swallows pre-epoch clock with unwrap_or(0) — emits row with observed_at=0 | return Result
crates/relix-runtime/src/knowledge/remote.rs:367 | MAJOR | INTEGRATION | MeshKnowledgeDispatcher::accept_shared maps any receiver-side Err to RemoteShareError::Transport — destroys structured rejection | parse receiver's structured ErrorEnvelope and convert
crates/relix-runtime/src/knowledge/remote.rs:266 | MAJOR | OPERATOR | LateBoundDispatcher silently returns Unreachable indefinitely if controller never wires cell — no warn/timeout/alert | emit tracing::warn! once per N attempts; surface via metrics counter
crates/relix-runtime/src/knowledge/autoshare.rs:53 | MAJOR | PERFORMANCE | AutoShareCursor uses tokio::sync::Mutex for state that needs no async semantics | switch to std::sync::Mutex
crates/relix-runtime/src/knowledge/config.rs:182 | MINOR | OPERATOR | Unknown auto_share_layer just warns and is dropped — operator typos pass without error | promote to hard error
crates/relix-runtime/src/planning/approval.rs:185 | MAJOR | OPERATOR | let _ = std::fs::create_dir_all(parent) ignores I/O failure; subsequent Connection::open surfaces confusing "unable to open" instead of real "directory not creatable" | propagate create_dir_all error
crates/relix-runtime/src/planning/approval.rs:189 | MINOR | OPERATOR | log_integrity_warning called only on open, not open_in_memory | apply consistently
crates/relix-runtime/src/planning/approval.rs:423 | MAJOR | CORRECTNESS | When existing_status parses as unknown status string, code returns ApprovalError::NotPending { status: ApprovalStatus::Rejected } — fabricates wrong status | surface explicit UnknownStatus(String) error variant
crates/relix-runtime/src/planning/approval.rs:501 | MINOR | OPERATOR | lock() swallows poison with PoisonError::into_inner — silently uses possibly-corrupted state | log tracing::error! once
crates/relix-runtime/src/planning/critic.rs:349 | MINOR | DEV | inject_feedback re-signs with let _ = next.sign() — fall-back leaves unsigned spec silently if serde_json::to_value fails | propagate error
crates/relix-runtime/src/planning/orchestrator.rs:394 | MINOR | PERFORMANCE | futures::future::join_all with no concurrency cap — if N large, AI dispatcher overloaded | Semaphore-gated tokio::spawn loop
crates/relix-runtime/src/planning/verification.rs:633 | MINOR | SECURITY | output_preview truncated to 2000 chars, no encoding escaping — model returns embedded "passed":true tokens influencing parsing | tokenise judge response stricter
crates/relix-runtime/src/planning/registry.rs:191 | MINOR | OPERATOR | RwLock::write().unwrap_or_else(PoisonError::into_inner) — silently writes to poisoned lock | log warn once on poison
crates/relix-runtime/src/planning/parser.rs:567 | MINOR | CORRECTNESS | extract_agent_mentions uses 50-char proximity heuristic for negation — "do not use X" + 51 chars + use X reverses classification | use proper grammar / stronger negation detection
crates/relix-runtime/src/planning/parser.rs:557 | MINOR | CORRECTNESS | extract_agent_mentions O(N*M) over agent names x spec length; pathological 32k-agent x 64k-spec is quadratic | bound + early-exit
crates/relix-runtime/src/planning/conflict.rs:330 | MAJOR | CORRECTNESS | looks_write_like matches keyword fragments anywhere in capability — composer.assemble matches _send — too permissive | exact method-namespace matching
crates/relix-runtime/src/planning/generator.rs:139 | MINOR | OPERATOR | validate(&workflow, None) always passes known_peers=None — generated workflow's peer existence isn't checked | pass registry peer set
crates/relix-runtime/src/sflow/executor.rs:349 | MINOR | CORRECTNESS | String::from_utf8 falls back to format! on bad UTF-8 — silently degrades response | use lossy conversion or surface error
crates/relix-runtime/src/sflow/parser.rs:1072 | MINOR | CORRECTNESS | Variable name cap 32 chars — operators can't use longer names; arbitrary | make configurable
crates/relix-runtime/src/workflow/executor.rs:103 | MINOR | SECURITY | rand::random() for ExecutionId — fine for uniqueness but no cryptographic intent | use rand::rngs::OsRng explicitly
crates/relix-runtime/src/workflow/executor.rs:475 | MAJOR | CORRECTNESS | No per-step retry / max-attempts / exponential backoff — workflow failure goes straight to failure edge | add retry config to AgentSpec
crates/relix-runtime/src/workflow/executor.rs:308 | MAJOR | OPERATOR | No total-workflow timeout; malicious or buggy workflow runs forever | add overall execution_deadline parameter
crates/relix-runtime/src/workflow/executor.rs:339 | MAJOR | CORRECTNESS | BFS executor has no resume semantics — if controller restarts mid-flow, workflow trace partial, no continuation possible | persist BFS state to chronicle; allow resume
crates/relix-runtime/src/workflow/chronicle.rs:86 | MINOR | OPERATOR | No log_integrity_warning call after open | call after open
crates/relix-runtime/src/workflow/validator.rs:319 | MAJOR | CORRECTNESS | detect_success_cycle uses recursion — malicious workflow with deep success-edge chain causes stack overflow | convert to iterative DFS
crates/relix-runtime/src/workflow/validator.rs:281 | MINOR | CORRECTNESS | extract_vars no upper bound; pathological input like {{a}}{{a}}... extracts O(n) vars | cap result list size
crates/relix-runtime/src/yaml_flow/mod.rs:961 | MAJOR | SECURITY | lower_call builds invocation via format!("{builtin}({peer}, {method}, {arg})") after sol_string_literal quoting — relies on quoting correctness | use SOL builder with proper escaping
crates/relix-runtime/src/sol/mod.rs:74 | MINOR | CORRECTNESS | catch_unwind rescues SOL parser panics — verbatim port may leak unwind-unsafe state through panic! paths since rusqlite/others may corrupt | replace remaining panic! in port with Result paths
crates/relix-runtime/src/training/store.rs:616 | MINOR | SECURITY | format!("PRAGMA table_info({table})") interpolates table name raw — called with literals so safe today but pattern wrong | parameterized SQL or hardcoded queries
crates/relix-runtime/src/training/store.rs:96 | MINOR | OPERATOR | crate::db::log_integrity_warning not called after open — silent corruption | add call
crates/relix-runtime/src/training/recorder.rs:161 | MAJOR | OPERATOR | tx.send(rec).is_err() warns once and silently drops every subsequent record — operators see one warn then total silence | surface counter metric; alert
crates/relix-runtime/src/training/pii.rs:130 | MINOR | CORRECTNESS | PII detector returns spans in byte offsets but dedupe_overlaps doesn't guarantee deterministic ordering across patterns | sort by (start, end, pii_type ord) explicitly
crates/relix-runtime/src/training/config.rs:1 | MINOR | OPERATOR | [training] config parsed but no explicit max-store-size cap; retention only by time, not row count | add row-count cap
crates/relix-runtime/src/confidence/scorer.rs:131 | MINOR | PERFORMANCE | state: Arc<Mutex<HashMap<HistoryKey, WindowState>>> single mutex over per-(agent,method) state; all dispatch threads contend | DashMap or sharded mutexes
crates/relix-runtime/src/db/lock_order.rs:91 | MINOR | CORRECTNESS | debug_assert! for lock-order violation — release builds get no check; production deadlocks slip through silently | switch to assert! or instrument via tracing::warn
crates/relix-runtime/src/db/lock_order.rs:18 | MINOR | OPERATOR | StoreId enum hardcoded — adding new store requires recompile, not config | (compile-time guard, no action)
```

---

## Appendix E — web-bridge remaining

```
crates/relix-web-bridge/src/config_api.rs:271 | MAJOR | DEV | put_provider logs key_preview from freshly-set entry but also logs default_model.as_deref().unwrap_or("") via tracing — bypasses redaction on test infra | audit detail
crates/relix-web-bridge/src/config_api.rs:514 | MAJOR | OPERATOR | reqwest::Client built per-call with .timeout(10s); no connection reuse, no rustls pin, no proxy disable — operators behind transparent proxies can have provider tests silently routed | build client once at startup
crates/relix-web-bridge/src/tool_screen.rs:84 | MAJOR | DEV | deadline_secs.clamp(5, 120) overrides operator config — operators setting deadline_secs=240 for slow tool peers silently clamped | document or honor config
crates/relix-web-bridge/src/workflows.rs:160 | MAJOR | DEV | state.cfg.transport.deadline_secs.clamp(5, 600) for workflow streaming — same silent clamp | same
crates/relix-web-bridge/src/openai.rs:332 | MINOR | DEV | provider_hint_for_model does N x M scan of manifest_cache on EVERY /v1/chat/completions; many peers becomes hot path | cache derivation per (model, cache_version)
crates/relix-web-bridge/src/rate_limit.rs:192 | MINOR | DEV | cap == 0 -> Err(Duration::from_secs(60)) — bucket NEVER refills but returns 60s retry hint suggesting waiting helps | return clearer "disabled" outcome
crates/relix-web-bridge/src/config.rs:652 | MINOR | DEV | load_or_generate_client_key accepts any file with bytes.len() == 32; no checksum, no provenance; truncated key file leaves byte-shape valid but cryptographically bad | compute fingerprint
crates/relix-web-bridge/src/config_api.rs:1100 | CRITICAL | SECURITY | test_telegram builds format!("https://api.telegram.org/bot{bot_token}/getMe") — bot_token user-supplied via /v1/config/telegram PUT; value not validated for URL-safe chars; token with CRLF splits HTTP request | validate against /^\d+:[A-Za-z0-9_-]+$/ at PUT time
crates/relix-web-bridge/src/config_api.rs:543 | MAJOR | SECURITY | Google provider URL interpolates urlencode(api_key) — good; redact_err at 636 truncates but doesn't strip Google AIza-prefixed keys longer than prefix | add aiza to prefix list
crates/relix-web-bridge/src/config_api.rs:298 | MAJOR | OPERATOR | "anon" hardcoding across all provider config writes — forensic trail attributes every provider key rotation/test/quarantine/delete to "anon" | plumb client IP through middleware extensions
crates/relix-web-bridge/src/tasks.rs:1703 | MAJOR | OPERATOR | All intervention_audit.record_with_id("anon", ...) hardcode "anon"; docstring says actor should be "the remote socket address" | plumb client IP through middleware
crates/relix-web-bridge/src/tasks.rs:1740 | MAJOR | OPERATOR | Same — todo_set/todo_update calls "anon" actor | same
crates/relix-web-bridge/src/openai.rs:280 | MAJOR | INTEGRATION | Chat completions detects URL in message and auto-routes to tool flow IFF tool_template configured — bypasses operator intent | require explicit tool: true field
crates/relix-web-bridge/src/flow.rs:181 | MAJOR | CORRECTNESS | finalize_flow_run returns responder's last_error directly in Transport(...) — could echo attacker-controlled bytes verbatim | sanitize before surfacing
crates/relix-web-bridge/src/tasks.rs:309 | MINOR | DEV | e: String is cause string from task_recorder::get; bridge returns verbatim in ApiError { error: e }; if responder error embeds raw secrets, they ride to client | apply redact_secrets before forwarding
crates/relix-web-bridge/src/flow.rs:188 | MAJOR | CORRECTNESS | cause_for_event from VM error recorded into Chronicle task.failed event without redaction | apply redact_secrets here too
crates/relix-web-bridge/src/auth.rs:330 | MAJOR | SECURITY | bootstrap_token reads from AppState carrying raw token via state.bridge_token.value() — serialized to JSON; Cache-Control: no-store good but Origin check + no auth header is only gate | add SO_PEERCRED check for loopback-peer (Unix uid/Windows SID)
crates/relix-web-bridge/src/openai.rs:1042 | MINOR | DEV | Relix SSE extension chunk finish_reason: "stop" emitted before [DONE] sentinel but no content_filter, length, or other reason distinguishable — clients see "stop" for all terminations | distinguish via failure path
crates/relix-web-bridge/src/openai.rs:1014 | MAJOR | CORRECTNESS | build_openai_sse builds JSON via serde_json::json! which never propagates UTF-8 issues; safe for valid JSON but no recovery if chunk slicing yields invalid utf8 | currently safe
crates/relix-web-bridge/src/dashboard.rs:62 | MINOR | DEV | RELIX_DASHBOARD_PATH env var hot-swap read at startup ONCE via OnceLock; comment says "operators can hot-swap" but restart required | update doc
crates/relix-web-bridge/src/auth.rs:165 | MINOR | DEV | percent_decode rolls own hex parser instead of using percent-encoding crate already in workspace | use percent-encoding crate
crates/relix-web-bridge/src/tasks.rs:298 | MINOR | DEV | task_id regex check via is_valid_task_id assumed 32-hex; if responder accepts other lengths, bridge rejects them | note for consistency
crates/relix-web-bridge/src/ws.rs:215 | MAJOR | DEV | ws_max_concurrent = 5 per principal default — all WS callers share literal raw bearer string as principal; same principal = same bucket | document; require unique session ids
crates/relix-web-bridge/src/config.rs:632 | MINOR | DEV | open_layered_memory only warns on SQLite open failure; bridge starts, memory inspector endpoints all 503 — startup error would be louder | promote to startup error
crates/relix-web-bridge/src/tasks.rs:1716 | MINOR | DEV | todo_patch parses pipe-delimited response with no error envelope; malformed responder body silently returns blank status/text | validate responder shape
crates/relix-web-bridge/src/flow.rs:135 | MAJOR | DEV | Bridge writes rendered SOL template to tempfile inside tempfile::Builder::tempfile() then passes path to FlowRunner; on crash not always cleaned up | use in-memory string source if FlowRunner supports
crates/relix-web-bridge/src/chat.rs:44 | MINOR | DEV | /health returns plain text "ok\n" with no Content-Length; healthcheck probes parsing JSON need /v1/health | document
crates/relix-web-bridge/src/chat.rs:38 | MINOR | DEV | ErrorResponse and ChatResponse distinct shapes — JSON consumers can't distinguish without HTTP status | add discriminator field
```

---

## Appendix F — CLI remaining

```
crates/relix-cli/src/config.rs:336 | MAJOR | CORRECTNESS | Validator allows provider.name = "local" with empty api_key; provider.name comparison uses to_ascii_lowercase() while save_to round-trips name verbatim, so MyMix passes validation then fails at boot | normalize provider.name to lowercase at save_to time
crates/relix-cli/src/config.rs:296 | MINOR | OPERATOR | save_to uses tmp-then-rename, but on Windows rename across volumes fails with no helpful message | catch CrossesDevices, surface "config save: target on different volume from tmp"
crates/relix-cli/src/ping.rs:35 | MINOR | CORRECTNESS | Port selection 20_000 + rand::random::<u16>() % 10_000 can collide repeatedly | use port 0
crates/relix-cli/src/task.rs:299 | MAJOR | CORRECTNESS | task create joins ALL fields including params_json with | pipe; if title or params_json contains |, Coordinator parser receives corrupted arg with no escape | reject | in inputs OR use different delimiter
crates/relix-cli/src/task.rs:323 | MAJOR | CORRECTNESS | task update same pipe-delimiter injection with result, error_cause, failure_class | same fix
crates/relix-cli/src/task.rs:1188 | MINOR | CORRECTNESS | Same 20_000 + random %10_000 port collision pattern | use port 0
crates/relix-cli/src/task.rs:533 | MAJOR | OPERATOR | task watch Ctrl-C handling is OS-default — no message about how to exit | tokio::signal::ctrl_c handler that prints "stopping watch"
crates/relix-cli/src/task.rs:550 | MINOR | OPERATOR | interval_secs.max(1) silently clamps user's --interval-secs 0 | reject 0 at parse with clap validator
crates/relix-cli/src/capability.rs:474 | MINOR | CORRECTNESS | Same client_key.len() != 32 error without remediation hint | include path; suggest generation
crates/relix-cli/src/mcp.rs:358 | MINOR | CORRECTNESS | Random-port collision pattern repeated | use port 0
crates/relix-cli/src/router.rs:166 | MINOR | CORRECTNESS | p.peer_id[..22] byte slicing; will panic on non-ASCII multibyte chars | use char_indices
crates/relix-cli/src/router.rs:223 | MINOR | CORRECTNESS | Same byte-slice issue on session_id[..12] | use char_indices
crates/relix-cli/src/setup.rs:331 | MINOR | OPERATOR | pick_provider clamps initial_idx via .min(PROVIDER_CHOICES.len() - 1); if empty would underflow | defensive panic message or use saturating_sub
crates/relix-cli/src/workflow.rs:331 | MINOR | OPERATOR | expect("reqwest::Client builds") reachable only via env corruption | bubble error
crates/relix-cli/src/confidence.rs:281 | MINOR | DEV | Three identical urlencode helpers across confidence.rs, knowledge.rs, training.rs, planning.rs, build.rs, approval.rs, sessions.rs, execution.rs, provenance.rs, skills.rs, training.rs, credentials.rs, identity.rs, belief.rs — code duplication risks divergence | centralize
crates/relix-cli/src/training.rs:209 | MINOR | SECURITY | agent and session_id raw-interpolated as query value after &agent= / &session_id=; urlencode only applied once | URL-encode all parameters consistently
crates/relix-cli/src/knowledge.rs:329 | MAJOR | CORRECTNESS | shared builds query as ?shared_by=... but URL also contains /{agent} as path segment that gets urlencoded; if agent contains ?, encoded ? becomes part of path | use proper URL builder
crates/relix-cli/src/ops.rs:2056 | MAJOR | CORRECTNESS | smoke uses let mut step = 0usize; let mut fails = 0usize; let mut run = |...| {step+=1; ...} — closure captures mutable refs but step/fails referenced after closure use; ill-suited for async refactor | convert to explicit counters
crates/relix-cli/src/ops.rs:1963 | MAJOR | OPERATOR | tail polls /v1/tasks/events/recent?since=<cursor> but never handles older bridges that don't support since param — silently shows full history every tick | detect older bridge by missing next_cursor; switch to dedup
crates/relix-cli/src/ops.rs:2415 | MAJOR | OPERATOR | route_test errors --candidates required (comma-separated, non-empty) only after split — if user passes single value with spaces, still reports unhelpfully | validate before split
crates/relix-cli/src/ops.rs:74 | MAJOR | CORRECTNESS | route_test CLI accepts --candidates as CSV but doesn't validate that candidates exist as configured providers — sends junk to bridge | pre-fetch providers and validate
crates/relix-cli/src/ops.rs:3105 | MAJOR | INTEGRATION | cron_list raw interpolates subject_id={} — query string injection if value contains & | URL-encode
crates/relix-cli/src/ops.rs:3148 | MAJOR | OPERATOR | cron_create always uses default flow_template = "flows/chat_template.sol"; silently couples cron jobs to path that may not exist on coordinator | check flow exists via API first
crates/relix-cli/src/planning.rs:200 | MINOR | CORRECTNESS | plan computes effective_dry_run = !execute && dry_run — clap default for dry_run is true AND execute is false; doc claims --dry-run is default | test that bare relix planning plan --spec X is dry-run; clarify
crates/relix-cli/src/eval.rs:122 | MAJOR | CORRECTNESS | Default mode = "balanced" fine but eval can fail below floor on fresh local config (no API key) — exits with rates-below-floor error message that doesn't surface actual failures inline | print failures BEFORE rate-floor error
crates/relix-cli/src/sol.rs:131 | MINOR | OPERATOR | new --out flows/x.sol writes silently to disk after creating parents; no preview of what's being written | print confirmation line (already at 151)
crates/relix-cli/src/flow.rs:194 | MINOR | OPERATOR | flow yaml always prints — pipes well to file but no message saying "pipe to a file like relix flow yaml > my.yml" | add eprintln hint when stdout is tty
crates/relix-cli/src/models.rs:124 | MINOR | CORRECTNESS | list and health use unwrap_or(&empty) after unwrap_or_default() chain — works but if JSON shape changes, printer silently prints empty table | surface "couldn't parse providers" rather than empty table
crates/relix-cli/src/provenance.rs:262 | MAJOR | OPERATOR | "note: bridge does not expose /v1/provenance/recent" stderr message printed, then Ok(empty vec) returned, so subcommand prints "(no matching prompt-file snapshots)" — operator sees note interleaved with empty results | clarify message
crates/relix-cli/src/confidence.rs:118 | MINOR | SECURITY | urlencode(agent) then urlencode(method) properly applied OK | (no action)
crates/relix-cli/src/install.rs:484 | MINOR | CORRECTNESS | Ollama suffix on non-Windows is .zip for macOS — installer_url for macOS Ollama returns .zip OK; for Linux Ollama is shell script; suffix unused for Linux | document or skip suffix for Linux
crates/relix-cli/src/mesh.rs:621 | MINOR | OPERATOR | read_bridge_token returns None on read failure; banner prints "(could not read bridge-token file from ~/.relix/bridge-token — ...)" but doesn't say which OS error | include error kind
```

---

## Appendix G — controller/flow-inspect/embedded remaining

```
crates/relix-controller/src/main.rs:21 | MINOR | OPERATOR | tracing_subscriber::fmt().init() called before Args::parse(); clap parse failures print to stderr but subscriber's log stream active — no try_init | use try_init() and gate on RUST_LOG
crates/relix-controller/src/main.rs:14 | MINOR | OPERATOR | #[tokio::main] uses default multi-thread runtime with no explicit worker-thread cap; on 64-core host controller spawns 64 worker threads regardless of node type | build runtime explicitly with configurable worker_threads
crates/relix-controller/src/main.rs:11 | MINOR | DEV | --config required but not validated for existence here; controller_runtime opens later; clap error trace points at runtime, not wrapper | value_parser = clap::value_parser!(PathBuf) with exists check
crates/relix-flow-inspect/src/main.rs:84 | MAJOR | CORRECTNESS | When --replay-verify is set tool calls eventlog::read_records AND eventlog::verify_chain; verify_chain itself re-reads same file; two full reads per replay | drop eager read_records when replay_verify on
crates/relix-flow-inspect/src/main.rs:72 | MAJOR | PERFORMANCE | read_records loads entire flow log into Vec<EventRecord> in memory before printing; append-only flow logs grow indefinitely; year-old log can OOM | stream records and print line-by-line
crates/relix-flow-inspect/src/main.rs:192 | MINOR | CORRECTNESS | extract_latency_ms parses ANY UTF-8 substring beginning latency_ms=; captured prompt containing literal latency_ms=999999 reports forged latency | anchor to record kind
crates/relix-flow-inspect/src/main.rs:115 | MINOR | CORRECTNESS | Trace-ID and request-ID filters do hex::encode(r.trace_id.0) != *t case-sensitively after lowercasing input — uppercase trace id pasted from logs gets zero hits | lowercase both sides or use substring/starts_with
crates/relix-flow-inspect/src/main.rs:107 | MAJOR | PERFORMANCE | handle_audit reads entire audit log into Vec, allocates another Vec for filtered, never streams | stream records, evaluate filter inline
crates/relix-flow-inspect/src/main.rs:107 | MAJOR | INTEGRITY | Audit-log handler does NOT verify signatures or chain — simply reads + prints; attacker tampering audit file gets clean print | add --verify-audit requiring --signer-key
crates/relix-flow-inspect/src/main.rs:65 | MINOR | DEV | When both --flow and --audit passed, audit path silently ignored (flow wins) | reject with mutually-exclusive error
crates/relix-embedded/src/lib.rs:262 | MAJOR | CORRECTNESS | default_model defaults to empty string; OpenAI-compat/Anthropic providers require model id; empty default reaches provider, call fails with vendor 400 | reject empty default at build
crates/relix-embedded/src/lib.rs:194 | MINOR | DEV | RelixEmbeddedBuilder #[derive(Default)] and pub, encouraging field-by-field construction; only provider enforced at build() | replace Default with private Default
crates/relix-embedded/src/lib.rs:251 | MINOR | PERFORMANCE | spawn_blocking used to open SQLite but on None branch just calls LayeredMemoryStore::in_memory() trivially fast; spawn round-trip costs more than work | skip spawn_blocking on None branch
crates/relix-embedded/src/lib.rs:257 | MINOR | CORRECTNESS | chunk_size_chars.unwrap_or(800).max(64) silently clamps dev's request — if dev passes 5, gets 64 with no warning | return EmbeddedError::Config or tracing::warn
crates/relix-embedded/src/chat.rs:98 | MINOR | CORRECTNESS | match self.sessions.lock() { Ok(g) => g, Err(poisoned) => poisoned.into_inner() } silently recovers from poisoned mutex; future panic in render becomes invisible | at minimum tracing::warn!
crates/relix-embedded/src/chat.rs:189 | MINOR | SECURITY | Lines user: {message} and {agent_name}: {output.text} persisted verbatim to memory store; no length cap; attacker who controls one chat call writes multi-megabyte records | cap stored line length
crates/relix-embedded/src/chat.rs:204 | MINOR | OPERATOR | When SQLite write fails function tracing::warn!s and continues — but BOTH user and assistant turns appended to in-process ring regardless | surface persistent: bool on ChatResponse, or fail fast on user-turn write
crates/relix-embedded/src/chat.rs:166 | MAJOR | DEV | ProviderChatInput fields temperature, max_tokens, thinking_budget_tokens hard-coded None; embedded ChatInput doesn't expose these knobs | surface the three fields
crates/relix-embedded/src/chat.rs:181 | MINOR | OPERATOR | Provider error message uses e.to_string() — strips structured info (provider name, retryability, raw vendor body) | carry structured ProviderError through EmbeddedError
crates/relix-embedded/src/chat.rs:155 | MINOR | CORRECTNESS | input.message.is_empty() rejects empty but NOT whitespace-only; session_id uses trim().is_empty(); inconsistent | pick one
crates/relix-embedded/src/memory.rs:131 | MAJOR | DEV | Embedded accepts ONLY markdown/md/txt/code/text content types; bridge tool parse_document accepts PDF, screenshots via tiered parser; devs migrating bridge to embedded silently lose support | wire same parse_document tiered pipeline or document divergence
crates/relix-embedded/src/memory.rs:18 | MAJOR | DEV | Doc comment says vectors NOT generated (embedding stays NULL); embedded silently degrades to substring search even when configured provider supports embeddings | build embeddings via provider.generate_embeddings after chunking
crates/relix-embedded/src/memory.rs:181 | MINOR | CORRECTNESS | limit == 0 silently rewritten to 5; caller asking "limit 0, count only" gets 5 rows | reject limit == 0 or document
crates/relix-embedded/src/memory.rs:159 | MAJOR | CORRECTNESS | Inside chunk loop store.insert(&record)? early-returns on first failure, leaving prior idx chunks committed; partial ingest returned as error; retry produces duplicate IDs | wrap in transaction or return Partial(chunks_committed) error
crates/relix-embedded/src/memory.rs:131 | MINOR | CORRECTNESS | chunk_size = self.chunk_size_chars() followed by chunk_text(&input.content, chunk_size); chunk_text re-applies chunk_size_chars.max(64); belt-and-suspenders | single source of truth — clamp once at build
crates/relix-embedded/src/memory.rs:32 | MINOR | DEV | MAX_CHUNKS_PER_INGEST = 5_000 private const — devs hitting cap cannot override without forking; runtime equivalent is config-driven | expose builder method
crates/relix-embedded/src/memory.rs:150 | MINOR | CORRECTNESS | chunk_id mixes subject_id + source_tag + idx + chunk body via blake3; truncated to 16 hex chars (64 bits); 5,000 chunks x 1,000 subjects birthday-collision non-trivial | use 24 chars
crates/relix-embedded/src/memory.rs:153 | MINOR | DEV | record.tags.push(format!("source:{source_tag}")) and content_type — tag namespace implicit; malicious source value :.. injects source::.. into tag space | sanitise tag values
crates/relix-embedded/src/memory.rs:190 | MINOR | CORRECTNESS | Filter says r.source == subject_id but ingest writes record.source = subject_id AND record.tags.push(format!("source:{source_tag}")); operator-supplied source (filename) ends up in tags while subject_id is SQL source column; names collide | rename either input field or tag namespace
crates/relix-embedded/src/memory.rs:130 | MINOR | DEV | content_type sanitised to lower-case but "md" silently accepted and stored as verbatim string in MemoryIngestResult.content_type; downstream code comparing content_type == "markdown" misses md alias | normalise to canonical token
crates/relix-embedded/src/memory.rs:188 | MINOR | PERFORMANCE | filtered: Vec<MemoryHit> built by .into_iter().filter(...).map(MemoryHit::from).collect() — for search with subject filter allocates full raw vec then filters | single fused iterator chain
crates/relix-embedded/src/memory.rs:194 | MINOR | DEV | memory_search API takes subject_id: String (empty = "any") — string-empty as sentinel brittle and typo-prone | use Option<String>
crates/relix-embedded/src/memory.rs:165 | MAJOR | DEV | MemoryIngestResult.source echoes input verbatim but ingest appends as tag (source:<label>) AND stores nowhere else; devs comparing result with later search row's tags see source:<label> not <label>; shape leaky | document or normalise
crates/relix-embedded/src/lib.rs:262 | MINOR | OPERATOR | Arc<HistoryStore> created per-builder but RelixEmbedded::clone() shares it; devs who build TWO embedded runtimes pointing at same SQLite get TWO different in-process history rings | document; or move history ring into SQLite
crates/relix-embedded/src/lib.rs:91 | MINOR | DEV | #![forbid(unsafe_code)] good but crate transitively pulls relix_runtime which uses unsafe — forbid only protects own code; docstring should clarify | note in module docs
crates/relix-embedded/src/lib.rs:144 | MINOR | DEV | RelixEmbedded::builder() returns RelixEmbeddedBuilder by value; no try_build or validate method to surface config errors without committing to async DB open | add .validate(&self) -> Result<(), EmbeddedError> synchronous check
crates/relix-embedded/src/lib.rs:108 | MINOR | DEV | EmbeddedError has only 4 variants and no code() / no is_retryable() | add kind() accessor returning stable enum
crates/relix-embedded/tests/embedded_smoke.rs:9 | MINOR | DEV | mock_runtime() calls .expect("build embedded runtime with mock provider"); every test runs against :memory: SQLite; tests never exercise on-disk happy path except one isolation test at 222 | add explicit on-disk coverage for chat()
crates/relix-embedded/tests/embedded_smoke.rs:67 | MINOR | CORRECTNESS | chat_history_grows_across_sequential_turns_in_same_session asserts exactly 6 turns after 3 calls; test doesn't drive past 20 to expose orphan-user-line bug | add test that drives past HISTORY_MAX_TURNS
crates/relix-embedded/tests/embedded_smoke.rs:113 | MINOR | CORRECTNESS | memory_ingest_chunks_and_search_finds_a_keyword_in_the_subject ingests three short paragraphs (sub-target) so chunker stays single-chunk — never exercises overlap path; UTF-8 overlap panic untested | add non-ASCII multi-paragraph ingest test
```

---

## Appendix H — telegram/discord/slack remaining

```
crates/relix-telegram/src/session_store.rs:109 | MINOR | CORRECTNESS | SqliteSessionStore::open does let _ = std::fs::create_dir_all(parent); if mkdir fails (permission denied), subsequent Connection::open fails with less obvious error | propagate mkdir error
crates/relix-telegram/src/session_store.rs:144 | MINOR | CORRECTNESS | _relix_migrations table created but never written to; schema versioning is unused dead weight | either start tracking version or drop table
crates/relix-telegram/src/session_store.rs:166 | MAJOR | CORRECTNESS | SqliteSessionStore::record calls let _ = conn.execute(...) and swallows rusqlite error — when DB full, locked, schema-corrupt, inbound handler reports success but mapping gone, eventual reply never arrives | propagate or tracing::error!
crates/relix-telegram/src/session_store.rs:191 | MAJOR | CORRECTNESS | forget uses ? to short-circuit tx begin but let _ = tx.execute(...) and let _ = tx.commit() swallow errors | propagate commit errors
crates/relix-telegram/src/messages.rs:14 | MAJOR | CORRECTNESS | IncomingMessage has no field to carry callback_query.data payload (the approval token) or callback_query.id; model is text/voice only | add callback variant or optional callback_data: Option<String> + callback_query_id: Option<String>
crates/relix-telegram/src/live.rs:104 | MAJOR | SECURITY | post helper formats error responses as format!("{method}: {} {desc}", status, ...) and propagates up; Telegram error descriptions can echo back request body — if caller logs BotApiError and body included bot token, error path is leakiest place | sanitise descriptions before formatting
crates/relix-telegram/src/live.rs:62 | MAJOR | PERFORMANCE | LONG_POLL_TIMEOUT_SECS = 30 paired with PER_CALL_TIMEOUT_SECS = 35 — if Telegram silently drops TCP mid-poll, reqwest waits 35s before erroring; NO jitter/backoff on consecutive empty polls — network blip pins loop for full window | add health-check timeout shorter than long-poll; exponential backoff between failed polls
crates/relix-telegram/src/mock.rs:146 | MAJOR | CORRECTNESS | MockBotApi::get_updates mimics "consume on read" but uses q.retain(...) — O(n^2) per poll and semantically wrong: real Telegram drains everything < offset+1 server-side, not just messages we returned | drain by update_id <= max_returned to match real Bot API
crates/relix-discord/src/live.rs:50 | MAJOR | PERFORMANCE | DEFAULT_FETCH_LIMIT = 50 per poll with no rate-limiter — Discord enforces tight per-channel-message GET ratelimit (5/5s); fast cadence trips 429 | document poll cadence; add jitter; expose configurable poll interval
crates/relix-discord/src/live.rs:194 | MAJOR | CORRECTNESS | DcErrorEnvelope only captures message — Discord error envelopes contain code int and per-field errors map naming rejected parameter; surface lossy; operators get unactionable messages | capture code + errors map
crates/relix-discord/src/live.rs:143 | MINOR | CORRECTNESS | 429 retry_after is f64, .max(0.0) clamps negatives but ceil() as u64 then clamp(1, MAX_RETRY_AFTER_SECS) rounds up to whole seconds — Discord 0.05s retry_after waits 1s | use sub-second sleep when < 1.0
crates/relix-discord/src/config.rs:38 | MAJOR | CORRECTNESS | DiscordConfig doesn't validate token shape (Bot xxx.yyy.zzz); only env var set; malformed token trips at first get_me call, not startup validation | add token format check at validate()
crates/relix-discord/src/config.rs:14 | MAJOR | CORRECTNESS | Only a SINGLE channel_id configurable — multi-channel deployment requires multiple controllers; no concept of approvals bound to operator chat vs public team channel | accept Vec<channel_id> or split routing knobs
crates/relix-discord/src/mock.rs:75 | MINOR | CORRECTNESS | after_message_id.parse::<u128>().unwrap_or(0) — non-numeric cursor (invalid but possible after corrupted store) silently returns all messages | validate cursor before query
crates/relix-slack/src/live.rs:118 | MAJOR | CORRECTNESS | 429 Retry-After parsed as u64 only — Slack docs say integer seconds but tier-2 limits can return absurd values; clamping to 30s drops expected behaviour silently; missing/malformed header silently defaults to 1 | surface unexpected Retry-After to caller
crates/relix-slack/src/live.rs:152 | MAJOR | CORRECTNESS | if let Ok(env) = serde_json::from_str::<SlackEnvelope>(&body_text) && !env.ok — every Slack response parsed twice; if Slack returns {"ok": true, "warning": "..."}, warning silently discarded | propagate warnings via separate path
crates/relix-slack/src/live.rs:148 | MAJOR | CORRECTNESS | let body_text = resp.text().await.unwrap_or_default() — failed body read on 200 silently becomes "" then parsed as envelope (fails, falls through), then as T (fails); caller gets Transient decode error for what was really a network drop | propagate body-read errors
crates/relix-slack/src/live.rs:50 | MAJOR | CORRECTNESS | DEFAULT_API_BASE = "https://slack.com/api" — no trailing slash; with_base_url trim_end_matches('/'); user passing https://slack.com (root) concatenates to /auth.test skipping /api and silently 404s | validate base_url shape
crates/relix-slack/src/mock.rs:81 | MAJOR | CORRECTNESS | if oldest.is_empty() { q.clone() } returns ALL pending (not draining); then 96-98 drains; returned messages not in oldest's filter; real Slack returns from most-recent backward | mirror real conversations.history semantics
crates/relix-slack/src/mock.rs:58 | MINOR | CORRECTNESS | ts_gt parses seconds as u128 and compares fractional parts as strings — "1.5" vs "1.10" compares lexicographically (1.5 > 1.10) which is wrong; Slack always emits 6-digit fractions so works by luck | normalise fraction width or parse to f64
```

---

## Appendix I — Rust SDK + plugin SDK remaining

```
crates/relix-sdk/src/lib.rs:103 | MAJOR | DEV | new() takes &str for token/base_url with no validation: empty token, invalid URL, or http:// vs https:// typo only blows up on first call | validate base_url parses to URL and token non-empty; return Result<Self, RelixError>
crates/relix-sdk/src/lib.rs:267 | MINOR | INTEGRATION | SSE parser ignores event: field so event: chunk vs event: done vs event: error are indistinguishable — error frames silently treated as chunks if they carry chunk/text | track current event name in parser
crates/relix-sdk/src/lib.rs:251 | MINOR | CORRECTNESS | SSE parser only handles \n\n separators; spec allows \r\n\r\n; Python SDK handles both | accept both
crates/relix-sdk/src/lib.rs:233 | MINOR | PERFORMANCE | String::new() grows unbounded if bridge produces hostile stream (no \n\n ever) — single allocation balloons to GBs before connection times out | cap in-flight buffer (e.g. 1 MiB)
crates/relix-sdk/src/lib.rs:405 | MINOR | CORRECTNESS | new_session_id uses nanos %hex but nanos can collide if two threads hit SystemTime::now() in same nanosecond window; not guaranteed on Windows where resolution is ~15ms | append u64 from AtomicU64::fetch_add counter
crates/relix-sdk/src/lib.rs:103 | MINOR | OPERATOR | No info!/debug! tracing — every HTTP call opaque to operators | add tracing::debug! spans
crates/relix-plugin-sdk/src/lib.rs:197 | MAJOR | PERFORMANCE | ready: Arc<Mutex<bool>> — every /ready probe takes tokio Mutex lock for bool read; health probes aggressive on k8s and contend with mark_ready | replace with Arc<AtomicBool>; load with Relaxed
crates/relix-plugin-sdk/src/lib.rs:342 | MAJOR | CORRECTNESS | handle_invoke returns 200 OK even on error_kind != 0; operators reading nginx/proxy logs see all traffic as healthy; only body distinguishes failure; breaks every standard HTTP load balancer's failure detection | document 200-on-error convention prominently, or move to RFC 7807 / matching status codes
crates/relix-plugin-sdk/src/lib.rs:235 | MAJOR | DEV | mark_ready(&self, ready: bool) — passing false after true to "un-ready" works but docs imply one-shot semantics; either intentional or footgun, not documented | rename to set_ready/unset_ready pair or document
crates/relix-plugin-sdk/src/lib.rs:266 | MINOR | OPERATOR | register() insert() silently overwrites previous handler with same name; typo in two registrations and one handler wins with no warning | return Result<(), AlreadyRegistered>
crates/relix-plugin-sdk/src/lib.rs:209 | MINOR | OPERATOR | Hardcoded "127.0.0.1:0" bind with no env override; RELIX_PLUGIN_BIND=... would let operator move listener (e.g. UDS) without rebuilding | read RELIX_PLUGIN_BIND
crates/relix-plugin-sdk/src/lib.rs:298 | MINOR | OPERATOR | Port-announce line goes to stdout; if plugin author also calls println! for logging, host loader's line scanner sees junk before RELIX_PLUGIN_PORT= and may fail to parse | document stdout discipline or move announce to typed handshake
crates/relix-plugin-sdk/src/lib.rs:103 | MINOR | CORRECTNESS | InvokeRequest.deadline_unix: i64 but docs say "Unix-seconds deadline" — should be u64 or Option<i64> so 0 (default) distinct from "no deadline" vs "deadline already past" | use Option<i64>
crates/relix-plugin-sdk/src/lib.rs:107 | MINOR | CORRECTNESS | args: String "pipe-delimited UTF-8" — wire-shape from legacy peer protocol; typed plugins want structured args (JSON object); every plugin parses same way | add Bytes args or serde_json::Value args additively
crates/relix-plugin-sdk/src/lib.rs:323 | MINOR | CORRECTNESS | /health returns {"ok": true} unconditionally — even if all handlers panicked, listener is alive; operators expect health to reflect actual serving capacity | differentiate /health (process up) from /ready (can serve traffic)
```

---

## Appendix J — Python SDK remaining

```
sdks/python/relix/client.py:264 | MINOR | SECURITY | README points to ~/.relix/bridge-token but SDK doesn't auto-load; users may hardcode tokens in source | add optional api_key_file= or auto-load
sdks/python/relix/client.py:159 | MAJOR | CORRECTNESS | When SSE payload not JSON, SDK yields StreamChunk(text=payload) — attacker/garbage upstream silently flows into caller's text; no way to tell decode failed | add raw/decode_failed field or skip non-JSON frames
sdks/python/relix/client.py:161 | MINOR | CORRECTNESS | parsed.get("chunk") or parsed.get("text") or "" falls through when text is empty string but field present — masks ordering between fields | explicit None check
sdks/python/relix/client.py:204 | MINOR | CORRECTNESS | raw[len("data:"):].lstrip() strips ALL leading whitespace; SSE spec only strips one optional space | use removeprefix(" ")
sdks/python/relix/client.py:530 | MINOR | CORRECTNESS | chat_stream is sync generator — if caller doesn't iterate to completion, streamed connection held until garbage collected | provide explicit context manager API for streams
sdks/python/relix/client.py:438 | MINOR | CORRECTNESS | JSON decode failure raises RelixResponseError on success-status responses — body included unbounded | truncate body before exception
sdks/python/relix/client.py:265 | MINOR | DEV | bridge_url has no scheme validation — RelixClient(bridge_url="localhost:19791") silently treats path as relative | validate scheme; raise on missing http://
sdks/python/relix/client.py:273 | MINOR | SECURITY | tenant_id accepted unvalidated — tenant_id="acme\r\nx-evil: 1" would inject extra headers | reject tenant_id containing CR/LF or non-printable
sdks/python/relix/memory.py:196 | MINOR | INTEGRATION | IngestDocumentResult.model_validate(data or {}) — defaults of empty source/subject_id/content_type silently mask missing-field bridge bugs | distinguish missing vs empty; surface warning
sdks/python/relix/planning.py:168 | MAJOR | INTEGRATION | AgentDescriptor.model_validate(r) — name: str required, no default; bridge missing name (older formats) raises ValidationError leaked to caller | default name to "" or wrap parse
sdks/python/relix/skills.py:137 | MINOR | CORRECTNESS | SkillsAPI.get("anything") for 404 raises RelixResponseError with no useful context — caller can't distinguish "skill not found" from "endpoint broken" | add RelixNotFoundError for 404
sdks/python/relix/observability.py:148 | MINOR | CORRECTNESS | Health parse silently drops agent rows whose value is not a dict — operator gets sparse map without warning | log/warn or raise on type mismatch
sdks/python/relix/observability.py:77 | MINOR | CORRECTNESS | params = {"hours": hours, "peer": peer} — None values sent through _scrub_params (good) but hours=0 becomes "0" (truthy in scrub), and bridge may treat differently from omitted | document; explicit handling
sdks/python/relix/client.py:447 | MINOR | CORRECTNESS | _url("/v1/info") accepts path starting with http:// and bypasses base — surprising for callers passing accidental absolute URLs from config | reject absolute URLs at SDK boundary
sdks/python/relix/client.py:468 | MINOR | CORRECTNESS | _scrub_params drops empty string but not empty list/dict; empty list as filter still serialised; bridge may see param= and reject | strip empties consistently
sdks/python/relix/client.py:67 | MINOR | INTEGRATION | ChatUsage.cost_cents field name differs from bridge convention (cost_usd_micros); likely diverges from TS SDK | verify against TS SDK; rename if inconsistent
sdks/python/relix/client.py:233 | MINOR | SECURITY | User-agent hardcoded with SDK version — fine but no caller-supplied UA suffix for tracing through proxies | add user_agent_suffix knob
sdks/python/relix/exceptions.py:37 | MINOR | SECURITY | __repr__ includes self.message but not self.body — body can contain echoed token/PII; ensure code that logs repr(err) does not leak | add explicit redaction option
sdks/python/relix/client.py:122 | MINOR | CORRECTNESS | isinstance(exc, httpx.ConnectError | httpx.NetworkError) collapses connect refused with mid-flight network drops — caller cannot distinguish, retry strategy differs | map ConnectError to RelixConnectionError but NetworkError mid-flight to transient subclass
sdks/python/relix/client.py:611 | MINOR | DEV | _async_post returns Awaitable but most call sites await directly; lack of async-method naming convention may confuse callers | rename or expose typed coroutine return
sdks/python/relix/client.py:359 | MINOR | CORRECTNESS | __enter__ lazily ensures sync client but __aenter__ lazily ensures async client — using async with then calling sync chat() causes sync client to be lazily created without matching close | have __aexit__ also close sync client
sdks/python/relix/memory.py:122 | MINOR | DEV | Sub-APIs reach into self._client._sync_post (protected attr) — tight coupling; refactor risk | promote internal helpers to documented internal interface
sdks/python/relix/client.py:144 | MINOR | CORRECTNESS | _parse_sse_field_payload("[DONE]") returns None silently — OpenAI-compat passthrough doesn't get terminal frame; caller doesn't know stream ended unless event:done arrives separately | yield synthesized done chunk for [DONE]
```

---

## Appendix K — TypeScript SDK remaining

```
sdks/typescript/src/client.ts:69 | MINOR | DEV | parseChatResponse spreads unknown extras through as snake_case but normalises known fields to camelCase — mixed-case object | run camelKeys on spread tail
sdks/typescript/src/client.ts:61 | MINOR | CORRECTNESS | parseChatResponse silently defaults missing/non-string text to "" rather than throwing; malformed bridge response yields empty reply with no diagnostic | throw RelixResponseError when no recognisable text field present
sdks/typescript/src/client.ts:91 | MINOR | CORRECTNESS | parseStreamMessage swallows JSON.parse failures and yields raw bytes as plain text; malformed SSE event leaks into reply | drop frame and surface debug warning
sdks/typescript/src/client.ts:49 | MINOR | OPERATOR | RelixResponseError thrown for non-object chat body uses statusCode=200 and JSON.stringify of body — misleading to anyone catching on statusCode | dedicated RelixResponseError variant or omit statusCode
sdks/typescript/src/types.ts:48 | MINOR | DEV | RelixTimeoutError carries no statusCode/body and no configured-timeout value; callers can't tell what timeout fired | include timeoutMs in message
sdks/typescript/src/types.ts:186 | MINOR | DEV | MemoryResult, ChatResponse, Skill, etc. all carry [key: string]: unknown — convenient for forward-compat but every typed field shadowed by unknown in unions | remove index signature once snake_case extras are camelised
sdks/typescript/src/client.ts:323 | MINOR | DEV | bridgeUrl trim only strips trailing slashes — doesn't validate scheme; non-http URL produces obscure fetch error downstream | validate URL; throw typed config error in constructor
sdks/typescript/package.json:10 | MINOR | DEV | "files" includes "dist", "README.md", "LICENSE" but repo has no LICENSE file at SDK root; npm publish would silently ship without one | add LICENSE or remove from files
sdks/typescript/src/index.ts:8 | MINOR | DEV | Named export of MemoryAPI/PlanningAPI/etc. lets consumers instantiate sub-APIs directly with RelixClient — supported but undocumented; leads to ad-hoc subclassing | document pattern or mark @internal
sdks/typescript/src/skills.ts:17 | MINOR | INTEGRATION | SkillsAPI.search passes q query param but doesn't propagate peer per README; params record sets peer but URL builder encodes; bridge SearchQuery struct doesn't accept peer | drop peer from skills search or wire it through bridge
sdks/typescript/src/planning.ts:36 | MINOR | INTEGRATION | PlanningAPI.agents passes peer as query param but bridge's PeerQuery only deserialises peer when wired — verify wire contract; otherwise dropped | confirm bridge expects ?peer= in query
sdks/typescript/src/client.ts:32 | MINOR | OPERATOR | SDK_USER_AGENT hard-codes "0.1.0" — out of sync with package.json once version bumped | read from package.json at build time
sdks/typescript/src/client.ts:443 | MINOR | CORRECTNESS | catch ignores reader.releaseLock() throw silently — could hide programming errors in runtime where body already locked elsewhere | log debug warning
sdks/typescript/src/memory.ts:107 | MINOR | DEV | parseSearchRow doesn't fall back to embeddingId (camelCase) or chunkText; upstream change to camelCase silently produces id="" rows | unify on camelKeys helper before extraction
sdks/typescript/src/observability.ts:67 | MINOR | DEV | parseHealth tries both "deployment" and "_deployment" keys but bridge only emits one — SDK more lenient than bridge guarantees, masking schema drift | pick canonical name
sdks/typescript/README.md:15 | MINOR | DEV | Claims Node 18+ and "modern browsers" via globalThis.fetch but SDK sets forbidden user-agent header in browsers (TypeError) | document Node-only or strip header in browser builds
sdks/typescript/src/types.ts:97 | MINOR | DEV | RelixClientOptions.apiKey typed as string | undefined with both undefined and missing allowed — slightly confusing; undefined produces missing Authorization silently | tighten typing or document silent skip
sdks/typescript/src/client.ts:251 | MINOR | CORRECTNESS | doJsonRequest returns null for empty body but signature returns Promise<unknown>; RelixClient.info returns {} on null — masks 200-with-no-body bug | tighten to throw on unexpected empty body for JSON endpoints
sdks/typescript/jest.config.js:14 | MINOR | DEV | jest run with --runInBand and tests don't cover error paths for chatStream mid-stream failures (only 401 pre-stream) | add test that injects error after first event
sdks/typescript/src/types.ts:115 | MINOR | DEV | RelixClientOptions.fetch typed as typeof fetch — strict tests need RequestInfo|URL signatures; consumers using polyfill (cross-fetch) often have slightly different signature and require as any cast | type as union with looser callable signature
```

---

## Appendix L — configs/scripts/ops remaining

```
configs/web-bridge-node.toml:30 | MINOR | INTEGRATION | [peers] uses port = N shape (right schema for controller dial) but this file is for wrong binary anyway | drop file
configs/ai-node.toml:64 | MINOR | CORRECTNESS | [ai.providers.anthropic] has no base_url; provider sets default but surrounding providers all set explicitly — inconsistent | add base_url = "https://api.anthropic.com"
configs/router-node.toml:17 | MINOR | CORRECTNESS | session_ttl_secs = 1800 under [controller]; field also documented in default_session_ttl; doc comment fails to say "ignored unless role=router" | clarify comment
scripts/alpha-bringup-m5.sh:39 | MINOR | OPERATOR | First-build cargo exceeds 6-second readiness wait (30 x 0.2s); script appears to hang | bump readiness timeout to >=120s
scripts/alpha-bringup-m5.sh:118 | MAJOR | SECURITY | Passes $ORG_KEY (32-byte org root SECRET key) as --client-key for every ping — alice's libp2p PeerId derived from org root; demo-only but copyable to production | mint separate client key OR document dev-only SIMP
scripts/alpha-bringup-m5.ps1:46 | MINOR | OPERATOR | Same init-org overwrite risk: script removes m5demo-* from dev-keys but m5demo-org-root.key lives there too; OK but ORG_PUB needs to be removed; verify rerun | confirm
scripts/alpha-bringup-m6.sh:64 | MINOR | OPERATOR | Creates configs/policies/m6demo.toml in LIVE repo configs dir; cleanup at 92 removes but Ctrl-C between create & trap leaves file | write to dev-data/m6demo/policies/
scripts/alpha-bringup-m6-chained.sh:84 | MINOR | OPERATOR | Same "write policy to live configs/ then remove on EXIT" pattern; same race window | scope policy files under DATA_BASE
scripts/alpha-bringup-m7-memory.sh:79 | MINOR | CORRECTNESS | Config sets max_n = 50 at same indent as db_path; MemoryConfig has max_n as real field so works but configs/memory-node.toml example does NOT include max_n — operator inconsistency | document max_n
scripts/alpha-bringup-m7-chat.sh:103 | MAJOR | OPERATOR | Comment says "set [ai.anthropic] api_key_path = ..." — no such section, no such field; correct schema is [ai.providers.anthropic] api_key_env | fix comment
scripts/alpha-bringup-m7-chat.sh:46 | MINOR | OPERATOR | Hard-codes ports 19601/19602 (M7) but other M-scripts reuse 19501/19502; operator running two demos in parallel hits port reuse after kill -9 leaves TIME_WAIT sockets | document or randomize port range
scripts/alpha-bringup-m8-web-bridge.sh:264 | MINOR | OPERATOR | Writes /tmp/relix-bad-resp — broken on Windows / git-bash (no /tmp by default); works on MSYS where /tmp maps but script also calls rm -f fine | use mktemp for portability
scripts/alpha-bringup-m8-web-bridge.sh:248 | MINOR | DEV | grep -rIl on bridge crate to enforce "no provider key" is brittle: any test fixture mentioning OPENAI_API_KEY in comment trips it (several tests do) | scope grep to non-test files or use tighter regex (literal = after var name)
scripts/alpha-bringup-m8-openwebui.sh:304 | MINOR | DEV | Same brittle grep | as above
scripts/demo-smoke.sh:38 | MINOR | DEV | sed -n '2,28p' "$0" for --help relies on line numbers — any header edit silently breaks --help | use heredoc usage() function
scripts/demo-smoke.sh:91 | MINOR | CORRECTNESS | Counts peers via grep -oE '"alias":' — works only if JSON keys exactly match "alias" with no whitespace; brittle to JSON shape changes | use jq when available
scripts/relix-mesh-up.sh:24 | MAJOR | OPERATOR | set -euo pipefail combined with wait_for_log returning nonzero on timeout aborts script mid-startup; trap-installed cleanup runs anyway | verify
scripts/relix-mesh-up.sh:1115 | MAJOR | OPERATOR | wait_for_log uses 30s timeout but first cargo-built relix-controller boot ~10-30s plus libp2p startup | bump to 60-90s
scripts/relix-mesh-up.sh:1134 | MINOR | OPERATOR | Hardcoded 127.0.0.1:$BRIDGE_PORT/health in readiness loop — never honours --run label or alternate hosts | OK on alpha
scripts/relix-mesh-up.sh:947 | MINOR | OPERATOR | peers.toml emits [peers.plugin_host] only when PLUGINS_ENABLED; telegram/discord/slack configured-but-not-listed in [peers]; channel can't ai.chat — but each channel config carries own ai_peer addr block | note inline addr blocks vs [peers] table
scripts/relix-mesh-up.sh:1015 | MAJOR | OPERATOR | cleanup() uses kill $pid; sleep 0.3; kill -9 $pid — on git-bash for Windows cargo-spawned PID may be cargo wrapper rather than binary; bash leaves orphan controller .exe on Ctrl-C in MSYS | document MSYS limitation
scripts/relix-mesh-up.ps1:271 | MINOR | OPERATOR | Same dimensions = 8 value as bash — field documented in code as not enforced ("reserved for forward compat"); silently wrong for text-embedding-3-small (1536 dims) | document or remove field
scripts/setup.sh:32 | MINOR | SECURITY | chmod 600 "$ENV_FILE" 2>/dev/null || true swallows failure — on filesystems without POSIX perms (NTFS via WSL, FAT) file ends up world-readable | warn when chmod fails on non-POSIX FS
scripts/setup.sh:37 | MINOR | SECURITY | env_get reads value via grep -E "^${var}=" | tail -n 1 | cut — doesn't strip trailing newline if file lacks final \n; downstream env_set could double-write | more robust parser
scripts/setup.ps1:65 | MINOR | SECURITY | Set-Content -Encoding utf8 on PS 5.1 writes UTF-8 WITH BOM; BOM appears as garbage on first line when sourced by bash | use -Encoding ascii or -NoNewline + raw write
scripts/setup.sh:178 | MAJOR | OPERATOR | Documents [metrics.cost_alerts] keys but baseline_window_mins, spike_multiplier, drift_threshold need to match runtime; not verified that all three are read | verify against spike_detector.rs schema
scripts/setup.sh:1 | MINOR | DEV | No shellcheck-friendly check: uses grep -E "^${var}=" "$ENV_FILE" 2>/dev/null | tail -n 1 | cut -d= -f2- and ignores SIGPIPE — works but fragile | minor
scripts/relix-mesh-down.sh:14 | MINOR | OPERATOR | pgrep -x relix-controller won't match cargo run ... relix-controller; on script-from-cargo invocation script silently finds nothing | document; add fallback for cargo-spawned
scripts/relix-mesh-down.ps1:21 | MINOR | OPERATOR | Same: Get-Process -Name relix-controller misses cargo-wrapped instances | same fix
ops/runbooks/alpha-bringup.md:144 | MINOR | DEV | cargo run -p relix-cli -- ../ # (not needed; below is the inspector) is stray malformed command that would just error | delete the line
ops/runbooks/alpha-bringup.md:43 | MAJOR | OPERATOR | "The repository ships example configs in configs/" lists configs/web-bridge-node.toml as canonical, but per controller code that file is a config for a controller that registers no caps | drop from list
ops/runbooks/alpha-bringup.md:93 | MAJOR | OPERATOR | RELIX_MODE=true RELIX_BRIDGE_URL=http://127.0.0.1:9100 python -m relix_web.main — port 9100 is libp2p listen_port AND in configs/web-bridge.toml listen_addr (real install uses 19791); 502 on wrong port | reconcile to one canonical port set
ops/runbooks/audit-query.md:44 | MAJOR | CORRECTNESS | --signer-key dev-keys/flow-runner.key — no canonical signer file with that path; actual key is per-controller signing key in dev-keys/<node>.key | document real key path resolution
ops/runbooks/audit-query.md:9 | MINOR | CORRECTNESS | Documents default audit path ~/.relix/<node-name>/audit.log but scripts (m5/m6/m7 bringup) write to dev-data/<node-name>/audit.log; default depends on env | clarify; reference RELIX_DATA_DIR
```

---

## Appendix M — docs remaining

```
docs/getting-started.md:158 | MAJOR | OPERATOR | Python OpenAI client example uses api_key="unused" — empty/whitespace tokens rejected by is_openai_shim_path; literal "unused" works but comment "The bridge's API-key header is ignored" understates that bridge requires NON-EMPTY bearer | document any non-empty works; empty fails
docs/operator-guide.md:277 | MAJOR | OPERATOR | Claims "bridge's pooled MeshClient doesn't auto-reconnect"; current-limitations.md:71-82 says auto-reconnect IS implemented; operators needlessly restart on transient drop | update
docs/operator-guide.md:200 | MAJOR | OPERATOR | Says -NoTool makes /chat_with_tool "404" — actual handler returns 502/flow-halted error when tool peer missing | replace 404 with actual response shape
docs/deployment.md:54 | CRITICAL | OPERATOR | Example uses --org-root flag but actual relix-cli identity mint flag is --root-key; clap error | replace --org-root with --root-key
docs/deployment.md:55 | CRITICAL | OPERATOR | --node-name memory-1 flag does not exist; actual flag is --name | replace with --name memory-1
docs/deployment.md:55 | MINOR | OPERATOR | --groups core shown; default policy from bringup script expects chat-users | document or use --groups chat-users
docs/security.md:386 | MAJOR | SECURITY | Token-rotation guidance "stop bridge, delete file, start bridge" — doesn't warn about active dashboard sessions / running OpenAI clients breaking; no zero-downtime mechanism | document downtime; mention dashboard sessionStorage reload via /v1/auth/token
docs/security.md:1 | MAJOR | OPERATOR | Silent on disk-full audit-log behavior — writes fail to error! level and response still goes back | add Operational concerns subsection
docs/security.md:1 | MAJOR | INTEGRATION | No docs of per-tenant credential vault rotation cadence (RotationScheduler ticks every rotation_check_interval_secs per GAP 15) | add Credential rotation subsection naming config keys
docs/current-limitations.md:280 | MAJOR | CORRECTNESS | "gemini provider is a placeholder" — actually crates/relix-runtime/src/nodes/ai/provider/gemini.rs implements real GeminiProvider with generate_reply posting to /v1beta/models/{model}:generateContent | update
docs/provider-configuration.md:18 | MAJOR | CORRECTNESS | Same — table marks gemini as "placeholder; Returns not_yet_implemented" but Gemini provider is fully wired | update row
docs/configuration.md:264 | MINOR | CORRECTNESS | Comment says "(placeholder; see provider doc)" for [ai.providers.gemini] | update
docs/failure-modes.md:156 | MINOR | OPERATOR | "Provider key changes need controller restart; rotation isn't supported at runtime today" — but dashboard's #/providers page DOES let operators paste new keys via bridge-secrets.toml | cross-reference operator-guide
docs/tool-node-security.md:1 | MINOR | SECURITY | Doc doesn't mention tool.web_fetch opt-in via tool.allow_http=false only blocks http:// schemes — https:// to internal IPs still blocked by SSRF guard; doc could clarify allow_http=true does NOT bypass SSRF for http:// | add clarifying note
docs/security.md:62 | MINOR | SECURITY | Says bundle lifetime defaults to 24h but doesn't tell operators where to override | cross-reference relix-cli identity mint --hours N
docs/getting-started.md:14 | MINOR | OPERATOR | Repo URL itsramananshul/Relix — verify canonical name | confirm or add RELIX_VERSION fallback note
docs/getting-started.md:131 | MAJOR | DEV | Note about "if you somehow got here without running the wizard" suggests degraded path exists but conflicts with line 124 | clarify missing-config behavior end-to-end
docs/operator-guide.md:88 | MAJOR | SECURITY | "Sharing org-root secret means sharing ability to mint identities — treat like production CA secret"; no guidance on rotating org root if compromised | add Recovering from org-root compromise subsection
docs/production-checklist.md:115 | MINOR | OPERATOR | "Honest scope: no built-in RBAC API yet" — but agent.create + agent gate effectively are RBAC; doc downplays what ships | reword to acknowledge agent gate
docs/getting-started.md:233 | MINOR | OPERATOR | relix stop && relix boot to apply config change — but wizard prints "Restart AI controller to apply" per operator-guide.md:333; stop+boot full-mesh restart heavier than needed | document controller-restart-only option
docs/configuration.md:90 | MINOR | DEV | dev-keys/ layout under <run>-bridge.aic vs <run>-bridge.bundle — comment 124-127 says ".aic was alpha; .bundle is current"; mixing both is confusing | settle on one extension
docs/agent-permissions.md:79 | MINOR | OPERATOR | Command example relix-cli ops agent create uses ops namespace; omits --bridge flag; for fresh install bridge token required for /v1/agents POST | document bearer token requirement
docs/security.md:271 | MINOR | SECURITY | "AI / tool provider keys live only on respective nodes — bridge holds none" — contradicts bridge-secrets.toml that holds operator-pasted provider keys | reconcile
```

---

## Appendix N — specs/conformance remaining

```
specs/RELIX-1-rpc.md:20 | MAJOR | CORRECTNESS | §1.3 mandates "Max request 1 MiB, max response 4 MiB"; relix-runtime/src/transport/rpc.rs uses request_response::Config::default() with no size override | clamp request/response size in libp2p behaviour config
specs/RELIX-1-rpc.md:30 | MAJOR | CORRECTNESS | §1.6 stable error enum lists 18 kinds; 8 are constants but no code path emits CREDENTIAL_EXPIRED, CAPABILITY_DEPRECATED, CAPABILITY_REMOVED, REPLAY_REJECTED, VERSION_MISMATCH, APPROVAL_TIMEOUT, APPROVAL_DENIED, MANIFEST_STALE | implement emission or document why unreachable
specs/RELIX-1-rpc.md:71 | MAJOR | SECURITY | §1.13 step 11: "Write audit record (success or failure)"; dispatch/mod.rs:961 returns via encode_error_response_no_audit on decode failure | emit minimal audit row on decode failure
specs/RELIX-1-rpc.md:14 | MAJOR | CORRECTNESS | §1.2.3: "Every RPC pins exactly one capability major version"; envelope has mv field but dispatch/mod.rs never checks mv against capability descriptor major_version | gate dispatch on mv match
specs/RELIX-1-rpc.md:78 | MINOR | CORRECTNESS | §1.17: "unknown high keys MUST be ignored"; serde's derived Deserialize on RequestEnvelope uses #[serde(default)] only for known optional; not enforced | add explicit forward-compat test
specs/RELIX-1-rpc.md:43 | MINOR | OPERATOR | §1.7 "Operators MUST run NTP" — no documentation or healthcheck for clock skew | add startup warning when system clock skew vs monotonic exceeds 30s
specs/RELIX-2-stream.md:18 | CRITICAL | CORRECTNESS | §2.3 mandates libp2p protocol /relix/stream/1; transport/stream.rs:83 uses /relix/rpc/stream/1; peers strictly following spec reject substream upgrade | rename or amend spec
specs/RELIX-2-stream.md:22 | CRITICAL | CORRECTNESS | §2.4 frames mandate open, ready, chunk{sid,seq,payload,fin,err}, credit, cancel, heartbeat; transport/stream.rs StreamFrame enum has Header/Chunk(ByteBuf)/End/Err — none of spec's named frames exist; chunk lacks seq and fin | extend StreamFrame::Chunk with seq and fin or amend spec
specs/RELIX-2-stream.md:13 | CRITICAL | CORRECTNESS | §2.2.1 "Each stream has a stream_id unique within libp2p connection" — transport/stream.rs has no sid field on any frame | add sid to frame schema
specs/RELIX-2-stream.md:20 | MAJOR | CORRECTNESS | §2.3 "Per-connection cap: 256 concurrent streams" — no enforcement in transport/rpc.rs or stream.rs | add concurrency limiter
specs/RELIX-2-stream.md:30 | MAJOR | CORRECTNESS | §2.8 backpressure MUST NOT exceed granted credits (default 64); no credit-based code anywhere in transport/stream.rs | document as SIMP-006 widening
specs/RELIX-2-stream.md:34 | MAJOR | CORRECTNESS | §2.9 cancellation: "Both sides release resources within 1s" — no bounded cancellation timer in stream.rs; cancel happens via drop only | add explicit cancel frame + 1s timer
specs/RELIX-3-eventlog.md:21 | MAJOR | CORRECTNESS | §3.3 EventRecord field "type (u8)" with stable enum; relix-core/src/eventlog.rs:31 EventType is serde-named enum (CBOR text variants), not numeric — wire format diverges | use #[repr(u8)] + numeric encoding
specs/RELIX-3-eventlog.md:25 | MAJOR | CORRECTNESS | §3.4 spec lists 19 event types numbered 1-19; eventlog.rs:30 declares 7 only; many spec types missing | add type discriminants and stubs
specs/RELIX-3-eventlog.md:13 | MAJOR | SECURITY | §3.2.5 "Single owner: A flow has exactly one owning controller for its lifetime" — eventlog.rs has no controller-id check before append | add owner pinning check or SIMP
specs/RELIX-3-eventlog.md:21 | MINOR | CORRECTNESS | §3.3 "first event's prev_hash = 32 zero bytes"; eventlog.rs:117 initializes last_hash correctly but no test asserts first persisted record | add invariant test
specs/RELIX-3-eventlog.md:48 | MAJOR | CORRECTNESS | §3.7 re-issuance after crash: "idempotent capabilities re-issue with recorded idempotency key" / "at_most_once: do not re-issue ⇒ uncertain_after_crash" — neither implemented in flow_runner.rs | document as SIMP or implement
specs/RELIX-4-bundle.md:18 | MAJOR | SECURITY | §4.3 "COSE_Sign1 (RFC 8152) with fixed choices: alg = -8 (Ed25519), deterministic CBOR per RFC 8949 §4.2"; relix-core/src/bundle.rs is hand-rolled CBOR map, not COSE_Sign1 — interop with external COSE verifier impossible | add SIMP for "not actually COSE_Sign1 yet" or migrate
specs/RELIX-4-bundle.md:24 | MINOR | CORRECTNESS | §4.4 protected headers include bundle_format_version but bundle.rs:57 names it format_version (missing bundle_ prefix) | rename or amend spec
specs/RELIX-4-bundle.md:32 | MAJOR | CORRECTNESS | §4.6 "Payload Common Fields: issuer_id, subject_id, bundle_serial, not_before, not_after, delegation_chain"; bundle.rs puts bundle_serial/not_before/not_after in HEADER, not payload | move to payload or amend
specs/RELIX-4-bundle.md:36 | MAJOR | CORRECTNESS | §4.6 delegation_chain is common payload field; bundle.rs has no such field at all | add empty Vec field
specs/RELIX-4-bundle.md:47 | MAJOR | CORRECTNESS | §4.7 step 8 "Bundle-type-specific validation" — bundle.rs::validate has no per-type validation hook | add validate_for_type dispatch
specs/RELIX-4-bundle.md:47 | MAJOR | CORRECTNESS | §4.7 mandates distinguishing 5 named errors; bundle.rs has no "revoked" or "untrusted_chain" variant in BundleError | add stub error variants
specs/RELIX-4-bundle.md:59 | MAJOR | SECURITY | §4.13 Revocation: bundle valid iff serial not in CRL AND bundle_id not in revoke_now; bundle.rs::validate skips revocation entirely | implement local CRL store stub or amend invariant
specs/RELIX-5-manifest.md:11 | CRITICAL | SECURITY | §5.2.2 "manifest binds to controller's peer ID via signature" + invariant 3 dual-signed; manifest/mod.rs:42 NodeManifest is plain serde struct, not wrapped in Bundle — manifests are UNSIGNED | add SIMP or sign now via BundleType::NodeManifest
specs/RELIX-5-manifest.md:55 | MAJOR | OPERATOR | §5.9 refresh on change or at 50% of lifetime — manifest/mod.rs declares manifest_version: 1 per binary launch; no expiry/refresh loop | implement refresh
specs/RELIX-5-manifest.md:58 | MAJOR | CORRECTNESS | §5.10 "stale manifest ⇒ manifest_stale error" — error_kinds::MANIFEST_STALE exists but never emitted | emit on cached manifest TTL expiry
specs/RELIX-5-manifest.md:24 | MAJOR | CORRECTNESS | §5.3 payload fields require runtime version, supported protocols, CDDL stdlib version, build_id, version_compatibility — none in manifest/mod.rs NodeManifest | add fields or document SIMP
specs/RELIX-6-capability.md:12 | MAJOR | CORRECTNESS | §6.2.1 "uniquely identified within a node by (method_name, major_version)"; dispatch/mod.rs handler map key is method only | key handler map by (method, major)
specs/RELIX-6-capability.md:36 | MAJOR | CORRECTNESS | §6.7 version negotiation: removed_in ⇒ VERSION_MISMATCH, deprecated_in ⇒ served + audit warning — CapabilityDescriptor has no deprecated_in/removed_in/superseded_by fields | add fields
specs/RELIX-6-capability.md:34 | MAJOR | CORRECTNESS | §6.4 required fields missing | document or implement
specs/RELIX-6-capability.md:20 | MAJOR | CORRECTNESS | §6.2.5 "Descriptors are part of (or referenced from) the signed Node Manifest" — manifest unsigned | sign manifest
specs/RELIX-7-sol.md:18 | MAJOR | CORRECTNESS | §7.4 yield opcodes listed (10 opcodes); flow_runner.rs implements remote_call synchronously only; yield_stream_open/next/send/close, yield_approval_wait, yield_timer, yield_parallel_join absent | document as SIMP or implement
specs/RELIX-7-sol.md:33 | MAJOR | CORRECTNESS | §7.6 "SOL MUST NOT iterate maps in hash-randomized order"; sol/vm.rs uses std HashMap in places | audit VM heap; ban HashMap iteration
specs/RELIX-8-flow.md:13 | MAJOR | CORRECTNESS | §8.2.2 "Every flow has immutable (definition_id, definition_version) captured at creation" — FlowId is 16 random bytes; flow_runner.rs doesn't record | extend payload with hash of flow source + version
specs/RELIX-8-flow.md:26 | MAJOR | CORRECTNESS | §8.4 "Coordinator allocates flow_id (UUIDv7)"; relix-core/src/types.rs:113 FlowId::new uses pure random — not UUIDv7 format | implement UUIDv7 or SIMP
specs/RELIX-8-flow.md:14 | MAJOR | CORRECTNESS | §8.2.4 "Terminal states are terminal — no transitions out" — eventlog.rs accepts any event type append regardless of prior terminal event | add post-FlowCompleted/Failed append rejection
specs/threat-model.md:14 | MINOR | SECURITY | A1 mitigation "Rate limiting at the connection level (future)" — no rate limiter exists | open SIMP or remove mitigation claim
specs/threat-model.md:104 | MAJOR | SECURITY | Existential property "Identity verified on every responder before any handler logic runs" — violated when admission step 1 (decode) fails | emit minimal audit on decode failure
specs/identity-employees.md:88 | CRITICAL | SECURITY | §H.6 enforcement pipeline strict order steps 1-8; implementation (dispatch/mod.rs) doesn't run step 4 (capability lookup) before step 3 (node-level admission) | document admission ordering test
specs/identity-employees.md:78 | CRITICAL | SECURITY | §H.5 approval flows: "Approver signs approval.granted{nonce, decision} envelope. Responding node verifies approver satisfies policy"; admission/agent_gate.rs uses unsigned approval_token strings | add signed approval envelope verification
specs/identity-employees.md:34 | MAJOR | SECURITY | §H.1 AIC requires supervising_principals field; relix-core/src/identity.rs:34 has supervisors but zero references in dispatch/agent_gate/planning/approval | wire supervisors into approval check
specs/identity-employees.md:42 | MAJOR | SECURITY | §H.3 node policy file MUST have trust_roots, admit (groups+identity+deny+default); policy.rs PolicyFile has only admit.groups and [[rules]] | extend PolicyFile schema
specs/identity-employees.md:62 | MAJOR | SECURITY | §H.4 action-level allow_when / require_approval_when / deny_when with argument predicates; policy.rs only supports allow_groups | document gap in SIMP-004 explicitly
specs/identity-employees.md:104 | MAJOR | SECURITY | §H.7 "If audit write to local fails, the action MUST NOT proceed" — dispatch/mod.rs:2263 only tracing::error!s and continues | propagate error and short-circuit with RESPONDER_INTERNAL
specs/identity-employees.md:104 | MAJOR | SECURITY | §H.7 record shape includes args_hash; AuditRecord struct in audit.rs has no args_hash field | add args_hash
specs/identity-employees.md:104 | MAJOR | SECURITY | §H.7 record shape includes delegating_user for on-behalf-of; AuditRecord has no such field | add field or document
specs/alpha-simplifications.md:9 | MINOR | DEV | SIMP-001 covers synchronous remote_call but not 9 other missing yield opcodes from RELIX-7 §7.4 | expand SIMP-001 list
specs/alpha-simplifications.md:21 | MINOR | DEV | SIMP-002 covers delegation chain but does not cover: COSE_Sign1 not used; manifest unsigned; trust_roots policy field missing; supervisors not wired | add tracked items
specs/alpha-simplifications.md:69 | MINOR | DEV | SIMP-006 "minimal frame set: open, chunk(seq, payload), done, error"; actual code uses Header, Chunk(ByteBuf), End, Err — alpha doc misrepresents what's implemented | reconcile
specs/README.md:31 | MAJOR | DEV | Spec governance: "amendment PR that updates the spec, conformance/ vectors, and CHANGELOG-SPEC.md" — conformance vectors do not exist | populate conformance or amend process
```

---

## Appendix O — install/Dockerfile/examples/CI remaining

```
README.md:48 | MAJOR | SECURITY | curl | bash install from main branch (mutable ref, no checksum/signature) — repo-owner compromise pwns every new user | pin to a tag, publish SHA-256
README.md:54 | MAJOR | SECURITY | irm | iex PowerShell install from main is identical RCE risk on Windows | pin to released tag
install.sh:7 | MAJOR | SECURITY | curl | bash advertised with no checksum/signature step and points at main not a tag | pin URL to release tag
install.sh:25 | MINOR | OPERATOR | trap set BEFORE TMP_DIR created so early err() between 25 and 166 leaves ${TMP_DIR} empty and cleanup is no-op | add guard
install.sh:88 | MINOR | OPERATOR | mkdir -p "${INSTALL_DIR}" || err ... — set -e already aborts; redundant | drop || clause
install.sh:133 | MAJOR | SECURITY | release JSON fetched over HTTPS but no signature/pinning | distribute installer-signed latest.json with detached Ed25519 signature
install.sh:139 | MINOR | CORRECTNESS | grep | head -n 1 | sed fallback for tag_name parses whichever "tag_name" comes first | use jq always
install.sh:208 | MAJOR | SECURITY | find ... -perm -u+x -o -name 'relix' -o -name 'relix-*' plus fallback accepts any binary archive ships | hard allowlist of expected file names
install.sh:251, 274 | MAJOR | SECURITY | mesh scripts + flow templates fetched from main not tag — pinned installer + drifting scripts | fetch from ${REPO} at ${TAG}
install.sh:296 | MINOR | OPERATOR | ensure_in_rc appends to ~/.zshrc and ~/.bashrc even when dir already on PATH; operator editing rc line gets duplicate on every reinstall | use tagged region
install.sh:312 | MAJOR | OPERATOR | ~/.profile, ~/.zprofile, ~/.config/fish/config.fish not handled — fish/zsh users without .zshrc (login-shell only) get no PATH update | add .zprofile, .bash_profile, fish config
install.sh:316 | MAJOR | OPERATOR | macOS Catalina+ default shell is zsh; if .zshrc doesn't exist (factory-clean), only .bashrc edited — but interactive zsh ignores .bashrc; silent PATH break | touch ~/.zshrc on macOS or write .zshenv
install.sh:364 | MAJOR | OPERATOR | When piped from curl | bash and TTY available, wizard runs via </dev/tty redirection — but API key prompt rendered into stdin which may have been polluted with curl response | add --non-interactive path
install.ps1:6 | MAJOR | SECURITY | iwr | iex from main — RCE; PowerShell iex runs arbitrary code unsigned | pin to tag; verify signature
install.ps1:25 | MINOR | CORRECTNESS | TLS string fallback 'Tls12' may throw on .NET Framework 4.5+ but try/catch swallowed — silent fallback to insecure TLS possible on legacy Win | hard-fail if neither TLS-set path works
install.ps1:56 | MINOR | OPERATOR | ARM64 hard-rejected; Surface Pro X / Snapdragon laptops cannot install | relax check or build target
install.ps1:132 | MAJOR | SECURITY | Invoke-WebRequest downloads zip with no signature/checksum verification | publish per-asset SHA-256
install.ps1:145 | MAJOR | SECURITY | Expand-Archive has no zip-slip protection — malicious zip with ..\Windows\System32\foo.exe written outside $TmpExtract on older PowerShell | iterate entries with [System.IO.Compression.ZipFile] and validate full paths
install.ps1:173 | MAJOR | CORRECTNESS | Copies ALL *.exe siblings from extract payload dir into $InstallDir — malicious archive ships cmd.exe or powershell.exe | whitelist relix.exe, relix-controller.exe, relix-web-bridge.exe
install.ps1:204 | MAJOR | SECURITY | Mesh scripts fetched from main regardless of $tag — version-skew + supply-chain | use $Repo/$tag not $Repo/main
install.ps1:234 | MAJOR | SECURITY | Flow templates fetched from main — same | pin to $tag
install.ps1:272 | MINOR | OPERATOR | Unconditional SetEnvironmentVariable('Path', ..., 'User') with concatenation; if $userPath exceeds 2047 chars (legacy limit), registry write can truncate or corrupt user PATH | test result length; refuse write if > 8192 chars
install.ps1:322 | MINOR | OPERATOR | relix setup runs at install end with & $relixExe setup — exit code not propagated; failed wizard leaves ~/.relix/config.toml half-written with no rollback | check $LASTEXITCODE; restore prior on non-zero
Dockerfile:88 | MINOR | OPERATOR | HEALTHCHECK calls http://localhost:19791/health; container localhost resolves to 127.0.0.1 inside netns so works | OK; documentation note
Dockerfile:23 | MINOR | DOC | Header docs say bridge.toml lives at /relix/dev-data/local/bridge.toml; body CMD says /relix/configs/bridge.toml — internally inconsistent | unify on one canonical path
Dockerfile:94 | MINOR | OPERATOR | tini PID 1 good but no --init documented for docker run users who don't use ENTRYPOINT | already done via ENTRYPOINT — OK
Dockerfile:44 | MINOR | PERFORMANCE | Single-layer cargo build --release without dependency-cache phase (cargo-chef or similar) — every Dockerfile change to single source file rebuilds every dependency | add cargo-chef recipe
Dockerfile:55 | MINOR | SECURITY | Runtime installs curl solely for HEALTHCHECK — adds attack surface inside container | use relix-web-bridge --healthcheck subcommand
.dockerignore:1 | MAJOR | SECURITY | Does NOT exclude *.env, **/*.key, **/*.pem, .relix/, ~/.relix/config.toml, tools/, sdks/, docs/, scripts/ (mostly source), tool-probe.txt — ANY future COPY . . would leak keys | defense-in-depth: add **/*.key, **/*.pem, .env*, .relix/, tool-probe.txt
.dockerignore:7 | MINOR | CORRECTNESS | Excludes only configs/policies/, not rest of configs/; exclusion moot, operator expectation that "configs is shipped" (per CMD path) is broken | copy configs in builder or remove misleading exclude
Cargo.toml:81 | MAJOR | SECURITY | rusqlite = { version = "0.34", features = ["bundled", ...] } bundles SQLite statically — CVE in SQLite means Relix must release; OS-level libsqlite patches don't apply | document in SECURITY.md
Cargo.toml:84 | MINOR | PERFORMANCE | tokio = { features = ["full"] } pulls every tokio feature even when crate uses subset — slower compiles, larger binaries | per-crate slim features
Cargo.toml:139 | MAJOR | PERFORMANCE | [profile.release] has NO overflow-checks = true, NO debug = "line-tables-only", NO strip = "symbols" — release binaries include full DWARF and no overflow checks | consider overflow-checks = true for policy + identity crates
Cargo.toml:139 | MINOR | PERFORMANCE | Release binaries not stripped — installers ship 50-100 MB binaries with full debuginfo | add strip = "symbols"
Cargo.toml:144 | MINOR | DEV | [profile.dev] opt-level = 0 is implicit default — line is no-op | remove
rust-toolchain.toml:2 | MINOR | OPERATOR | Toolchain pinned to 1.95.0 but [workspace.package].rust-version = "1.95" (without .0) — minor inconsistency | sync to identical strings
CONTRIBUTING.md:17 | MAJOR | DOC | "Rust stable (whatever rust-toolchain.toml pins, currently 1.85+)" — actual pin is 1.95.0; doc lies by 10 minor versions | update to "1.95+"
CONTRIBUTING.md:6 | MINOR | OPERATOR | "This project does not accept commits authored by, or co-authored by, AI assistants" — non-enforceable policy; CI workflow greps for Co-authored-by: literal | wire commit-message lint pre-receive hook
CONTRIBUTING.md:64 | MAJOR | DOC | "rustfmt with workspace's rustfmt.toml is enforced" — no rustfmt.toml exists at workspace root | add file or remove claim
CHANGELOG.md:1 | MINOR | DOC | Format claim "Keep a Changelog [1.1.0]" — 1.1.0 mandates [Unreleased] header that contains content or is omitted; current [Unreleased] is blank with no link target | populate or remove until next change
CHANGELOG.md:8 | MINOR | CORRECTNESS | [Unreleased] link uses compare/v0.1.5...HEAD jumps from 0.1.5 to 0.1.1 to 0.1.0; versions 0.1.2/3/4 absent | document why skipped or unify versioning
CHANGELOG-SPEC.md:1 | MINOR | DOC | Last entry 2026-05-18; no entries since substrate freeze despite RELIX-7.30 / SIMP-016 work mentioned in main CHANGELOG | update or note explicit hold
CODEOWNERS:7 | MAJOR | SECURITY | Sole code owner @itsramananshul — single point of failure; "two-reviewer rule" cannot be satisfied | add second security owner
CODEOWNERS:25 | MINOR | CORRECTNESS | /tools/relix-cli/src/identity/ — path does NOT exist; relix-cli crate is at crates/relix-cli/, tools/ is just README placeholder | fix to /crates/relix-cli/src/identity/
SECURITY.md:31 | MAJOR | CORRECTNESS | "AI provider keys (Anthropic) live ONLY in AI node's local config file. Verified at release time by grep -ri ANTHROPIC relix-web/ crates/" — NO relix-web/ directory; verification command errors out | update grep to crates/relix-web-bridge/ and crates/relix-cli/
SECURITY.md:5 | MINOR | OPERATOR | Security contact ramanal@mail.uc.edu (university email) — universities retire emails | use dedicated security@relix.dev
SECURITY.md:32 | MAJOR | CORRECTNESS | Claims "AI provider keys (Anthropic)" — but Cargo.toml + README list openai, anthropic, openrouter, xai, gemini, local; only one acknowledged | generalise; verify all
SECURITY.md:37 | MAJOR | OPERATOR | "rotated manually for the alpha. Documented runbook in ops/runbooks/key-rotation.md (post-alpha)" — file does not exist today | land runbook or remove reference
README.md:67 | MAJOR | DOC | "Set RELIX_VERSION=v0.1.1 to pin a specific release." but CHANGELOG lists 0.1.5 — example pin is stale | update to v0.1.5
README.md:124 | MAJOR | DOC | Table claims AI providers include openai/anthropic/openrouter/xai/gemini/local — but CONTRIBUTING/SECURITY only audit Anthropic | add provider matrix
README.md:171-185 | MINOR | DOC | "default-deny; nothing runs without matching allow" — but installation doesn't walk through writing policy; default-deny on fresh install would block chat flow | document default-allow demo policy that ships with wizard
.github/workflows/ci.yml:18 | MAJOR | SECURITY | Uses dtolnay/rust-toolchain@stable (mutable ref) — supply-chain risk | pin to commit SHA
.github/workflows/ci.yml:35 | MAJOR | SECURITY | Swatinem/rust-cache@v2 — mutable major-tag; cache restore can deliver attacker-controlled build artifacts via cache poisoning | pin to commit SHA
.github/workflows/ci.yml:36-42 | MAJOR | CORRECTNESS | Cache continue-on-error: true for clippy job — comment says cache is build-speed not correctness; but if cache RESTORE succeeds with poisoned content, clippy runs against poisoned cargo cache and passes silently | continue-on-error only on SAVE, not RESTORE
.github/workflows/ci.yml:75 | MAJOR | SECURITY | EmbarkStudios/cargo-deny-action@v2 — mutable major tag; supply-chain | pin SHA
.github/workflows/fast-ci.yml:18 | MAJOR | CORRECTNESS | pull_request: branches: [main] triggers fast-ci for PRs targeting main — permissions: contents: read set workflow-wide while secret-scan job uses grep against WORKTREE: cache save could be poisoned | document pull_request_target not used
.github/workflows/fast-ci.yml:43 | MAJOR | CORRECTNESS | fast-ci does NOT run clippy --all-targets -D warnings — only cargo check and cargo test; clippy regressions reach main via fast-ci and ci.yml (manual-only) | add fast clippy pass
.github/workflows/fast-ci.yml:80 | MAJOR | SECURITY | Secret scan greps for sk-ant-|ANTHROPIC_API_KEY *= *... — patterns loose; won't catch OPENAI_KEY=, no entropy check; only handful of envvars | use gitleaks/trufflehog; expand patterns
.github/workflows/fast-ci.yml:86 | MINOR | CORRECTNESS | --exclude-dir=.git --exclude-dir=target --exclude-dir=.github — workflow files themselves contain ANTHROPIC_API_KEY literal so attacker-introduced key in .github/workflows/foo.yml excluded from scan | drop --exclude-dir=.github
.github/workflows/heavy-ci.yml:16 | MAJOR | OPERATOR | heavy-ci runs on PR only if labelled heavy — most PRs skip clippy --all-targets, deny, audit, integration; combined with CI being workflow_dispatch only, supply-chain + integration gates require manual triage | run heavy-ci weekly on schedule
.github/workflows/heavy-ci.yml:111 | MAJOR | CORRECTNESS | integration job runs bash scripts/alpha-bringup-m5.sh but M5 is historical milestone not current one (M8+ per CHANGELOG); test path may be stale | update to canonical alpha bringup
.github/workflows/nightly-security.yml:38 | MAJOR | OPERATOR | Nightly is only place advisories are hard-gated; if main broken at 06:00 UTC, who acts? issues: write permission set but no actions/github-script step opens issue on failure | wire peter-evans/create-issue-from-file on failure
.github/workflows/nightly-security.yml:59 | MAJOR | CORRECTNESS | cargo test --workspace --release in nightly only — debug-build tests run nightly never; if test passes in debug but fails in release caught at most daily | run release tests in heavy-ci too
.github/workflows/release.yml:85 | MAJOR | SECURITY | Release workflow uses dtolnay/rust-toolchain@stable, Swatinem/rust-cache@v2, actions/checkout@v4 — all unpinned tags; release builds (which ship to users via install.sh!) vulnerable | pin every action to SHA
.github/workflows/release.yml:105 | MAJOR | SECURITY | cargo install cross --git https://github.com/cross-rs/cross --locked — installs from git URL at HEAD (no commit pin); compromised cross-rs maintainer ships malicious binary into release artifacts | pin --rev <sha> or use crates.io install with --version
.github/workflows/release.yml:121 | MAJOR | OPERATOR | tar.gz uses chmod +x dist/relix* but no --mtime / --owner / --group normalisation — release archives not byte-reproducible | use tar --sort=name --mtime=... --owner=0 --group=0
.github/workflows/release.yml:142 | MAJOR | OPERATOR | PowerShell zip uses Compress-Archive which is non-deterministic (timestamp + ordering); fork builds will never match | use 7z with deterministic flags
examples/plugins/hello-plugin/plugin.toml:35 | MINOR | OPERATOR | binary = "python" — on Windows system python is often python3.exe (Microsoft Store stub) which prompts user instead of running; will hang plugin_host loader silently | document explicit python3 or py -3 fallback
examples/plugins/hello-plugin/hello.py:80 | MINOR | CORRECTNESS | Reads request body via int(self.headers.get("Content-Length", "0")) — no upper bound; malicious caller can OOM plugin process | cap at e.g. 1 MiB
examples/plugins/hello-plugin/hello.py:106 | MINOR | OPERATOR | Binds to ("127.0.0.1", 0) — single-threaded HTTPServer; concurrent invoke from plugin_host requests queue serially | use ThreadingHTTPServer
examples/plugins/web-lookup/plugin.toml:31 | MAJOR | CORRECTNESS | Path is ../../../target/debug/...; web-lookup at examples/plugins/web-lookup/ so ../../../target/ reaches Relix/target/ — but standalone web-lookup has own workspace, so cargo build from web-lookup/ writes to examples/plugins/web-lookup/target/debug/; path is WRONG | change to target/debug/relix-plugin-web-lookup
examples/workflows/chat-then-summarize.workflow:33 | MAJOR | INTEGRATION | Uses peer: ai and capability: ai.chat with input "session-default|{{workflow.input}}|"; running relix workflow run chat-then-summarize likely fails because workflow not registered CLI command | confirm CLI surface or move to .sflow
examples/workflows/chat-then-summarize.workflow:14 | MINOR | DOC | Comment says "To run: relix workflow run chat-then-summarize" — CLI invocation may not exist in current build | verify subcommand
flows/chat_template.sol:14 | MAJOR | CORRECTNESS | Template uses "{{MESSAGE}}" lowercase macro syntax differs from chat-then-summarize.workflow's {{workflow.input}}; two example languages with two interpolation conventions side by side; when chat_template.sol .sflow-rendered with operator data containing literal "|", wire format "{{SESSION}}|user|" + user_msg collides | document escaping rules
flows/chat_template.sol:18 | MAJOR | CORRECTNESS | "{{SESSION}}|user|" + user_msg — if user message contains |, memory node's splitn(3) parser may still split correctly because | after 3rd stays in body but user message "|||malicious" could write into different session_id | verify with positional tests
flows/chat_with_tool.sol:45 | MAJOR | SECURITY | remote_call("capability:tool.web_fetch", "tool.web_fetch", "{{TOOL_URL}}|16384") — URL substitution comment says "validate_input enforces character set" but bridge's validation is upstream; if validation drifts, user-supplied URL with | injects different body cap | validate at flow level too
flows/plugin_smoke.sflow:11 | MINOR | CORRECTNESS | Uses step reply: plugin_host.hello.greet "alice" — works only after hello-plugin loaded; smoke flow has no skip path if plugin manifest didn't register | add try/catch or doc dependency
flows/plugin_smoke.sol:4 | MINOR | CORRECTNESS | Calls remote_call("plugin_host", "hello.greet", "alice") — but plugin_host is opt-in via env vars (RELIX_PLUGINS=1), no soft-fail path | document prereq env vars
flows/ai_smoke.sflow:1 | MINOR | CORRECTNESS | Only three lines, no error handling; if ai.chat fails, flow halts with VM error not friendly message | wrap in try/catch
flows/chained_health.sol:21 | MINOR | DOC | Comment says "trace_id is constant across all events (carried by the flow); each call gets fresh request_id" — implementation detail in flow file makes flow brittle to runtime changes | move to docs/sol.md
flows/list_map_demo.sol:51 | MINOR | CORRECTNESS | "the analyzer rejects str + int so we can't inline the map_len count" — exposes documented language gap in example; operators may copy-paste expecting full type-conversion | add int_to_str built-in before release
workflows/examples | MAJOR | OPERATOR | Empty directory at workflows/examples/ — duplicated naming vs examples/workflows/ which IS populated; operators landing in workflows/examples/ first think project has no examples | delete empty dir or symlink to examples/workflows/
workflows | MINOR | DOC | Only contains empty examples/ subdir — README does not advertise this path so it's dead | delete
tools/README.md:1 | MINOR | DOC | tools/ exists only as README pointing into crates/; CODEOWNERS:25 references /tools/relix-cli/src/identity/ which doesn't exist | resolve via CODEOWNERS fix
```

---

## Appendix P — cross-cutting integration verification

Verification traced each claimed wire end-to-end through the audit reports.

### P.1 Approval delivery matrix — end-to-end wire status

| Channel | Outbound HTTP present? | Inbound webhook handler? | Approval-token binding? | Verdict |
|---------|------------------------|--------------------------|-------------------------|---------|
| telegram | `crates/relix-telegram/src/live.rs:340` POST sendMessage — YES | `live.rs:326` long-poll get_updates — but `allowed_updates=["message"]` line 333 EXCLUDES callback_query → button presses never received | None — `OutgoingMessage` has no reply_markup, `IncomingMessage` has no callback_data field | broken end-to-end |
| slack | `crates/relix-slack/src/live.rs:326` chat.postMessage — YES | `live.rs:301` conversations.history polling — NO Events API, NO Interactivity, NO HMAC verification | None | broken end-to-end |
| discord | `crates/relix-discord/src/live.rs:299` POST /channels/:id/messages — YES | `live.rs:276` REST polling — NO Gateway WS, NO Interactions, NO `X-Signature-Ed25519` | None | broken end-to-end |
| email | wire claimed in delivery config but no in-tree dispatcher reaches the email peer's send capability from `delivery.rs:436` — only `LogChannelDispatch` is wired | n/a | n/a | broken in-tree |
| dashboard | `default_default_channel()` returns "dashboard" and `channel_enabled` defaults to true when no `[channels.dashboard]` section exists | dashboard polls `/v1/approvals` so inbound works via REST | none enforced | partial |

**Bottom line:** the channels' transport crates are correct HTTPS plumbing in isolation — they are NOT wired to the approval delivery system. The only `ChannelDispatch` impl is `LogChannelDispatch`. **No deployment that uses `channel = "telegram"` etc. actually sends a message.**

### P.2 Credential rotation scheduler → MultiChannelAlertSink

- `credentials/scheduler.rs:107` — on `sweep_once` failure: `tracing::warn!` and return empty. No call into `MultiChannelAlertSink`.
- `credentials/scheduler.rs:66` — default notifier is `LogRotationNotifier`. Other notifiers documented (Telegram/Slack/email) are not registered via `register_default`.
- `MultiChannelAlertSink::deliver` (`metrics/alert_delivery.rs:329`) uses unbounded `tokio::spawn` per channel-per-event.

### P.3 Belief-state lazy-load from LayeredMemoryStore on cache miss

- `knowledge/remote.rs:266` — `LateBoundDispatcher` silently returns `Unreachable { detail: "knowledge mesh dispatcher not yet wired" }` for arbitrarily long if controller never wires the cell.
- Belief state is part of the planning/critic path. `planning/critic.rs:285` — when dispatcher returns Err, critic records `__critic_unreachable__` and exits.

### P.4 Judge model POLICY_DENIED → client

- `planning/verification.rs:303` — AI judge falls back to `(true, "ai judge dispatcher unreachable — assumed pass with caveat")` on Err.
- `planning/critic.rs:285` — same pattern.
- `planning/verification.rs:618` — `evaluate_pattern_match` PASSES BY DEFAULT on regex compile failure.

### P.5 Tier router fallback

- `manifest/mod.rs:194` — `find_alias_for_method` returns FIRST alias by BTreeMap NodeId order.
- No proactive health-check.
- `manifest/mod.rs:521` — `looks_like_transport_break` substring-matches `"io"` and incorrectly retries cap-denied errors.

### P.6 Multi-tenant Qdrant auto-creation on first write

- Only `memory_gap5.rs` propagates tenant. Other 44 handlers default to tenant `"default"`.
- Even if propagation worked, `dispatch/mod.rs:1142` and `:1913` don't bind tenant to verified caller.

---

## Appendix Q — additional scenario walkthrough detail

### Q.1 Scenario A — fresh macOS Big Sur, factory-clean zsh

1. User runs `curl -fsSL https://raw.githubusercontent.com/itsramananshul/Relix/main/install.sh | bash`.
2. `install.sh:7` warns nothing about pinning; `install.sh:133` fetches release JSON over HTTPS only.
3. `install.sh:139` parses tag_name via grep+sed.
4. `install.sh:180` extracts the archive with vanilla `tar -xzf` — Tar-Slip vulnerable.
5. `install.sh:196` chmods +x and copies every regular file except markdown/text/json/toml.
6. `install.sh:251, 274` fetches mesh scripts AND flow templates from `main` even though installer is at `${TAG}`.
7. `install.sh:316` checks for `.zshrc`. Doesn't exist on factory-clean Big Sur. PATH update never written.
8. `install.sh:364` opens the wizard via `</dev/tty`. Wizard writes `~/.relix/config.toml`. Exits.
9. User opens new terminal. `relix` not on PATH. "Command not found." Installer returned 0.

### Q.2 Scenario B — operator escalation flow, asleep

Updated walkthrough using all findings:

- Every approval request goes through `crates/relix-runtime/src/dispatch/mod.rs:1073` synchronously.
- The `on_require_approval` closure calls `ChannelDispatch::send`.
- The only registered `ChannelDispatch` is `LogChannelDispatch`. **No HTTP, no buzz.**
- The `approval_requests` row is `INSERT OR REPLACE` — retry resets `status=pending`, drops prior decision.
- Escalation timer is fire-and-forget. Controller restart vanishes all 20 escalations.
- `wait_for_approval` polls every 2s up to 300s — 150 SQLite reads per approval, 3000 reads total for 20 approvals.
- `record_decision` has no `WHERE status='pending'` guard — expiry transition races with other transitions.
- Operator wakes. Dashboard shows 20 rows in `delivered` state. None actually delivered.
- Operator types "yes, approve all" in Telegram. `IncomingMessage` has no `callback_data` field. `allowed_updates` excludes any callback_query path. No way to bind reply to approval id.

### Q.3 Scenario C — Python chatbot 30 days

After 30 days of ~1 chat/sec:

- 2.6M chat rows in `dispatch_audit`.
- 2.6M rows in `memory_observations`; insert acquires `std::sync::Mutex`.
- Policy cache grows by tenant_ids.
- `capability_stats` HashMap grows with attacker-supplied method names.
- `session_tokens` verify is O(active_tokens). At 1M tokens, hundreds of ms per request.
- OtelExporter ≈ 518,400 TLS handshakes/30d.
- `knowledge::evict_if_needed` cap at 10,000 — tail silently lost past that.
- PII gate `req.args.to_vec()` per request — 100ms × 30M.
- SQLite combined likely > 10 GB. No VACUUM, no retention.

### Q.4 Scenario D — adversarial PDF

1. Attacker submits PDF via `/v1/memory/ingest`.
2. Tenant middleware: `memory_gap5.rs` propagates; dispatch.rs:1142 doesn't verify tenant authorization.
3. PDF parsed by tiered parser. Hidden text stored as plain text in `memory_observations`.
4. No perception-security boundary wraps untrusted content.
5. PII gate doesn't scan response bodies.
6. Later memory.dialectic returns the malicious chunk. LLM prompt includes it verbatim.
7. LLM emits `confidence: 1.0`. Self-consistency disabled.
8. Output includes `approval_token` field. `dispatch/mod.rs:1114` accepts ANY non-empty token. Tool calls bypass approval.
9. LLM rewrites research profile with operator-trusted confidence values.
10. `evaluate_pattern_match` silently passes on regex compile failure.
11. AI judge assumes pass on dispatcher Err.

---

## Appendix R — issues that surfaced during synthesis

```
crates/relix-web-bridge/src/flow.rs:127 + crates/relix-runtime/src/sflow/lexer.rs:137 | CRITICAL | SECURITY | Bridge does naive .replace("{{SESSION}}", session_id) AND SOL lexer has NO escape sequence support; session_id containing {{MESSAGE}} silently substitutes; combined with yaml_flow injection at mod.rs:981 this is a third arm of SOL injection | use structured templater
crates/relix-web-bridge/src/tenant.rs + 44 handlers | CRITICAL | SECURITY | Tenant middleware is documented as isolation boundary; only memory_gap5.rs reads extension; every other handler defaults to "default" | Either remove the middleware (don't lie) or extend every mesh-calling handler
crates/relix-runtime/src/approval/delivery.rs:436 + crates/relix-telegram/discord/slack | CRITICAL | INTEGRATION | LogChannelDispatch is the only impl; the three transport crates exist with correct HTTPS but are NOT wired; deployment expectation that "channel = telegram" works is false | Wire a TelegramChannelDispatch, SlackChannelDispatch, DiscordChannelDispatch backed by the existing transport crates
crates/relix-runtime/src/identity/session.rs:42 + dispatch/mod.rs:984 | CRITICAL | SECURITY | verify_on_dispatch parsed but never read; dispatch has no per-call session-token verification path | Either delete the flag or wire it into the inbound admission pipeline
crates/relix-runtime/src/admission/agent_gate.rs:126 + agent_gate.rs:165 | CRITICAL | SECURITY | Two default-permissive paths in the gate; combined with no rate-limit module the only enforcement layer is the per-method risk_level check at 262 — which is also SKIPPED for unknown methods (default-allow) | Flip all three to default-deny
crates/relix-cli/src/memory_inspect.rs:32+48+57+68+78 | CRITICAL | INTEGRATION | Five CLI commands default to wrong port 9100; bridge runs on 19791 — every default invocation hits closed port | Single shared default constant set to 19791
.github/workflows/ci.yml:9 + heavy-ci.yml:84 + nightly-security.yml:38 | CRITICAL | DEV | CI is on workflow_dispatch only; advisories are continue-on-error in heavy-ci; nightly-only hard-gate but no auto-issue on failure — CVE in libp2p can land via PR + sit on main for up to 24h with no one alerted | Restore push/PR triggers; hard-gate audit in heavy-ci; wire auto-issue on nightly failure
.github/workflows/release.yml:122 + install.sh:180 + install.sh:251 | CRITICAL | SECURITY | Release artifacts unsigned + tar-slip vulnerable extraction + mesh scripts pulled from main not tag — three-step supply-chain compromise path | Sign artifacts; safe-extract; pin script fetch to tag
```

---

## Appendix S — defect-density observations

Density per crate (issues / file):

| Region | Files | Issues | Density |
|--------|------:|------:|--------:|
| relix-core | 13 | 77 | 5.9 |
| relix-runtime/dispatch+coordinator | 2 | 86 | 43.0 |
| relix-runtime/transport+admission+approval | 8 | 81 | 10.1 |
| relix-runtime/credentials+identity+manifest+plugin+metrics+observability | 41 | 76 | 1.9 |
| relix-runtime/knowledge+planning+nodes+workflow+sflow+sol+yaml_flow+training+confidence+db | 70 | 95 | 1.4 |
| relix-web-bridge | 38 (read) | 78 | 2.1 |
| relix-cli | 49 | 87 | 1.8 |
| relix-controller/flow-inspect/embedded | 5 | 60 | 12.0 |
| relix-telegram/discord/slack | 17 | 38 | 2.2 |
| relix-sdk + relix-plugin-sdk | 2 | 38 | 19.0 |
| sdks/python | 9 | 47 | 5.2 |
| sdks/typescript | 12 | 31 | 2.6 |
| configs/scripts/ops | 17 | 65 | 3.8 |
| docs | 38 | 44 | 1.2 |
| specs/conformance | 11 | 60 | 5.5 |
| install/Dockerfile/examples/CI | 26 | 89 | 3.4 |

**The hottest defect density is `dispatch + coordinator` (43 issues per file).** This is also where the worst architectural failures sit: `CoordinatorStub`, the approval-token bypass, the missing replay protection, the audit-on-failure gap. **If you only have time to fix one file, fix `crates/relix-runtime/src/dispatch/mod.rs`.**

The second-densest is `relix-sdk + relix-plugin-sdk` (19 per file). The plugin SDK has no outbound capability call, no manifest, no auth on /invoke, no sandbox — these aren't bugs, they're absent architecture. The Rust SDK has three wire-broken endpoints.

The lowest densities are docs (1.2) and the broader runtime subdirs. That makes the runtime subdirs *look* good but the count includes many tests; the production code there is similar density to relix-core.

---

## Appendix T — additional Angle 4 SECURITY items

```
crates/relix-runtime/src/nodes/ai/provider/anthropic.rs (via metrics path) | MAJOR | SECURITY | api_key passed via Authorization header; no rate-limit per (provider, key); compromised credential rotated without audit trail | wire credentials audit into provider calls
crates/relix-web-bridge/src/config_api.rs:583 | MAJOR | SECURITY | interpret_response reads upstream provider body unbounded via resp.text() — provider returning 100MB OOMs bridge | bytes().take(N)
crates/relix-web-bridge/src/config_api.rs:602 | MAJOR | SECURITY | Same pattern in error path | same
crates/relix-runtime/src/plugin/dispatcher.rs:65 | CRITICAL | SECURITY | http://127.0.0.1:{port} for plugin invoke — any local process can hit /invoke | per-plugin shared bearer token via env
crates/relix-runtime/src/plugin/loader.rs:32 | CRITICAL | SECURITY | No sandbox (no chroot, no seccomp, no namespaces, no rlimit) on plugin spawn | wasmtime or fork into restricted process
crates/relix-runtime/src/plugin/loader.rs:206 | MAJOR | SECURITY | binary command name → PATH lookup; operator editing plugin.toml binary = "rm" runs whatever's on PATH; TOCTOU between scans + spawns | require absolute paths; verify file hash against binary_sha256
crates/relix-runtime/src/plugin/loader.rs:131 | MAJOR | SECURITY | NO signature check on manifest or binary | publisher signature + signed binary hash
```

---

## Appendix U — final residual items (long tail)

```
crates/relix-runtime/src/db.rs (lock_order helpers) | MINOR | DEV | Lock-order debug_assert only fires in debug; production deadlocks slip silently | promote to runtime assert with tracing::warn
crates/relix-runtime/src/db/lock_order.rs | MINOR | DEV | Manual StoreId enum — new stores require recompile | document
crates/relix-runtime/src/confidence/config.rs | MINOR | OPERATOR | Confidence engine config thresholds not bounded; operator can set unbounded values | clamp at config load
crates/relix-runtime/src/confidence/fallback.rs | MINOR | OPERATOR | Fallback action set has no explicit deny path documented; abort uses INVALID_ARGS | document semantics
crates/relix-runtime/src/workflow/store.rs:160 | MAJOR | SECURITY | No max workflow file size — already cited; cross-listing for completeness | enforce 10MB cap
crates/relix-runtime/src/sflow/executor.rs | MINOR | DEV | VecChronicle.entries() panics on poisoned mutex — already cited | poison recovery
crates/relix-runtime/src/sflow/executor.rs:88 | MAJOR | CORRECTNESS | MAX_VARS=50 hard cap not overridable | configurable
crates/relix-runtime/src/sol | MINOR | DEV | catch_unwind rescues parser panics — verbatim port may leak unwind-unsafe state | replace remaining panic! paths
crates/relix-runtime/src/sol/cli.rs:20,32 | MAJOR | CORRECTNESS | Multiple panic/expect points reachable from user input | Result paths
crates/relix-runtime/src/training/config.rs | MINOR | OPERATOR | Training config has no max-store cap; only time retention | add row-count cap
crates/relix-runtime/src/training/pii.rs:130 | MINOR | CORRECTNESS | PII detector dedupe non-deterministic | sort by tuple
crates/relix-web-bridge/src/dashboard.rs:62 | MINOR | DEV | RELIX_DASHBOARD_PATH hot-swap claim — read once at startup | update doc
crates/relix-web-bridge/src/tasks.rs:1716 | MINOR | DEV | Pipe-delimited todo_patch response with no error envelope | validate shape
crates/relix-web-bridge/src/config.rs:632 | MINOR | DEV | open_layered_memory warns on failure; bridge starts; memory inspector 503s | promote to startup error
crates/relix-web-bridge/src/chat.rs:38 | MINOR | DEV | ErrorResponse vs ChatResponse distinct shapes — JSON consumers can't discriminate | add discriminator
crates/relix-web-bridge/src/openai.rs:307,962 | MINOR | DEV | record_chat_observability writes metadata twice per turn; sink semantics may double-count | audit sink
crates/relix-cli/src/ops.rs:74 | MAJOR | CORRECTNESS | route_test doesn't validate candidates exist as configured providers — already cited | pre-fetch
crates/relix-cli/src/install.rs:585 | MAJOR | OPERATOR | run_docker swallows stderr — already cited | inherit Stdio
configs/policies/tool.toml | MAJOR | SECURITY | Only tool.web_fetch rule; missing tool.read_file, tool.write_file, tool.search_files, tool.patch, tool.pdf, tool.web_extract, tool.browser.*, tool.mcp.*, tool.terminal.run — every tool call other than web_fetch silently default-denies if policy file is the canonical one | add explicit rules
```

---

## Final total

Across the main report (Angles 1-7) and Appendices A-U: **1,052 line-itemised defects** spanning **92 CRITICAL · 648 MAJOR · 312 MINOR**.

**The single biggest fact in this codebase:** the integration story — approval delivery, tenant isolation, audit on every call, signed manifest, replay protection, `verify_on_dispatch`, plugin sandbox, every memory CLI default, every release artifact signature, every CI gate — is **largely missing or mis-wired**. The constituent crates often work in isolation. The connecting wires either don't exist (`LogChannelDispatch` is the only ChannelDispatch impl), are decorative (`tenant.rs` middleware that no handler reads), or are explicitly stubbed (`CoordinatorStub`). Operators reading the docs see a working system; the code beneath is a constellation of working parts that don't talk to each other.

The remaining ~700 defects in the appendices are the long tail. The eight production-lethal items in the opening summary are the ones that, if not fixed, will cause a real 3 AM page within the first month of production traffic.
