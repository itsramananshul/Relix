# Full Session Audit

**Audit timestamp:** 2026-05-26
**Audit author:** continuation Claude
**Scope:** every item Anshul asked for in this session.
**Method:** read every prompt verbatim, read the actual source the commits
touched, run the call paths in head, mark partial/broken honestly.

The session covered three task batches:

1. **Observability batch** — Tasks 1–4 (two-sink, session debugger,
   provenance registry, OTel export).
2. **Eight wiring gaps** — W1–W8.
3. **This audit** itself.

Each batch's items are listed below. The rules for marking DONE are
strict, per the user's instructions:

- A capability registered but not wired to a real call path is NOT DONE.
- An endpoint that returns a placeholder is NOT DONE.
- A test that mocks the thing it is supposed to test is NOT DONE.
- A commit message that claims X works when X does not is NOT DONE.

---

## Per-item findings

### O1 — Two-sink observability architecture (Task 1)

- **WHAT WAS ASKED:** `MetadataSink` + `ContentSink` +
  `ObservabilityContext`. Long-retention metadata; short-retention
  prompt/response content; tests for split + retention.
- **WHAT WAS BUILT:** `crates/relix-runtime/src/observability/sinks.rs`
  with both stores, both `record_event` paths, retention pruning, and
  the bundle struct.
- **STATUS:** DONE.
- **EVIDENCE:** sinks.rs:62–304 implements both sinks; 11 unit tests
  in the same file (`record_persists_and_round_trips_metadata`,
  `prune_older_than_deletes_rows_past_cutoff`,
  `content_sink_prune_expired_drops_rows_past_retention`, …) pass.
- **GAP:** None.

### O2 — Session-centric debugger (Task 2)

- **WHAT WAS ASKED:** `SessionDebugger` primitive over Sink A; bridge
  endpoints `GET /v1/sessions`, `GET /v1/sessions/{id}`,
  `GET /v1/sessions/{id}/content/{event_id}`; elevated-header gate on
  the content endpoint.
- **WHAT WAS BUILT:** runtime side at
  `crates/relix-runtime/src/observability/session_debugger.rs`;
  bridge side at `crates/relix-web-bridge/src/sessions_obs.rs`. Three
  routes registered in `main.rs`.
- **STATUS:** DONE.
- **EVIDENCE:** session_debugger.rs:62–186 (timeline assembly + status
  classification); sessions_obs.rs:55–146 (logic functions over
  `&ObservabilityContext`); 7 + 6 tests pass; bridge route list in
  main.rs registers all three endpoints.
- **GAP:** None.

### O3 — Provenance registry (Task 3)

- **WHAT WAS ASKED:** `ProvenanceSnapshot`/`ProvenanceRegistry`/
  `ProvenanceDiff`/`ProvenanceChange`/`ProvenanceError`; SQLite
  backing; bridge endpoints `GET /v1/provenance/{trace_id}` and
  `GET /v1/provenance/diff?a=&b=`.
- **WHAT WAS BUILT:** runtime side at
  `crates/relix-runtime/src/observability/provenance.rs`; bridge side
  at `crates/relix-web-bridge/src/provenance.rs`. Two routes
  registered in `main.rs`.
- **STATUS:** DONE.
- **EVIDENCE:** provenance.rs:39–219 (record/get/diff with `BTreeMap`
  for deterministic diffs); 5 + 4 tests pass.
- **GAP:** The registry **had no producer** at the moment it landed —
  no code path actually wrote a `ProvenanceSnapshot`. The user
  flagged this as a wiring gap (W8). Closed in commit `917a70e`.

### O4 — OTel export (Task 4)

- **WHAT WAS ASKED:** OTel-shaped span buffer; opt-in event types;
  attribute whitelist; assertion that Sink B content never leaks.
- **WHAT WAS BUILT (at commit `2f0ba25`):** `OtelExporter` that
  buffers `OtelSpan` rows in memory; `record_event` / `flush` /
  `drain_pending` / dropped counters.
- **STATUS at commit time:** PARTIAL. The exporter had no HTTP
  transport, no config parsing, and no startup wiring; spans went
  into a `Vec` and stayed there.
- **CLOSED IN:** W7, commit `7b7de6f`.
- **EVIDENCE:** otel.rs:189–245 (async `flush` that POSTs
  OTLP/HTTP JSON); otel.rs:333–365 (`render_otlp_json` helper);
  controller_runtime.rs:342–360 spawns a 5s flush loop when
  `[observability.otel]` is enabled.
- **GAP (still PARTIAL after W7):** No production producer
  populates the controller-side exporter — Sink A is currently
  bridge-side only. Bridge does not parse `[observability.otel]`
  from bridge config or attach an exporter to its
  `ObservabilityContext`. See **fix item 1** below.

### W1 — Wire ToolDispatcher into handle_chat tool-call path

- **WHAT WAS ASKED:** Every `PlanStep::ToolCall` the planner produces
  must go through `ToolDispatcher` — broker → secret → output guard
  → gateway, in that order. Failures surface as structured errors.
- **WHAT WAS BUILT:** New `nodes::ai::execution::tool_runner`
  module + `handle_chat` walks plan steps and routes ToolCalls
  through it. Controller-runtime builds a per-controller
  `ToolDispatcher` and hands it to `ai::register`. Failed dispatches
  append a `[tool-dispatch-errors]` JSON trailer.
- **STATUS:** PARTIAL.
- **EVIDENCE:** tool_runner.rs:27–69 dispatches each ToolCall through
  the dispatcher; ai/mod.rs:801–905 walks plan.steps and folds
  results in; controller_runtime.rs:3185–3215 constructs the
  dispatcher with the shared broker.
- **GAP:** The handler closure passed to `ToolDispatcher::dispatch`
  is a **stub** that returns `format!("admitted: tool={} args_len={}", …)`.
  It does NOT actually execute the remote tool capability — the
  comments are honest about it ("the actual mesh hop sits on the
  tool-flow path"). For production agent autonomy the AI node would
  need an outbound `MeshClient` and the closure would dial the tool
  peer. This is the gap that turns the planner's ToolCall step into
  a no-op even after admission passes.

### W2 — Wire AgentAccessBroker into capability dispatch + parse [[execution.agents]]

- **WHAT WAS ASKED:** Broker check pre-handler in the admission
  pipeline; parse `[[execution.agents]]` from config.toml; startup
  loads policies into the broker.
- **WHAT WAS BUILT:** `DispatchBridge` gained `set_access_broker`,
  `access_broker_handle`, and a pre-handler check between
  PolicyEngine and dispatch. `controller_runtime` parses
  `[execution]` with `[[execution.agents]]` and builds the broker
  via `build_access_broker`. The same broker is shared with the
  per-controller `ToolDispatcher`.
- **STATUS:** DONE.
- **EVIDENCE:** dispatch/mod.rs:611–675 enforces broker check end to
  end; controller_runtime.rs:2861–2905 parses the config; three
  tests in dispatch/mod.rs cover allow / deny / config round-trip.
- **GAP:** None.

### W3 — Register ask_human in tool node capability list

- **WHAT WAS ASKED:** Add `tool.ask_human` registration; mesh call
  routes to `AskHumanTool::handle` instead of returning
  `CapabilityNotFound`.
- **WHAT WAS BUILT:** Registration in `nodes/tool/mod.rs::register`;
  manifest entry in `controller_runtime`.
- **STATUS:** PARTIAL.
- **EVIDENCE:** nodes/tool/mod.rs:966–1006 registers
  `tool.ask_human`; controller_runtime.rs:4651–4657 advertises the
  descriptor on the manifest; two tests in nodes/tool/mod.rs:1234–
  1340 prove the mesh route hits `AskHumanTool::handle` and returns
  `{"timeout": true}` not `UNKNOWN_METHOD`.
- **GAP:** The `operator_sender` closure passed to
  `AskHumanTool::handle` is a **stub** that always returns `None`,
  so every real call surfaces `{"timeout": true}` immediately. No
  operator channel (Telegram approval queue, dashboard intervention
  queue) is wired. Until that lands, the capability is
  registration-honest but operator-useless.

### W4 — Wire embedding dispatcher into coordinator drift hook

- **WHAT WAS ASKED:** Embed goal + recent activity; compute cosine
  similarity; write the score and a `drift_detected` flag into the
  chronicle entry.
- **WHAT WAS BUILT:** `DriftEmbedDispatcher` trait;
  `evaluate_drift_for_task` is async and takes
  `Option<Arc<dyn DriftEmbedDispatcher>>`; chronicle payload now
  carries `similarity=<f>` and `drift_detected=<bool>`. Three tests
  exercise aligned vectors, orthogonal vectors, and the no-embedder
  fallback.
- **STATUS:** PARTIAL.
- **EVIDENCE:** drift.rs:21–37 (trait + handle alias);
  coordinator/mod.rs:6919–7027 (async hook that writes the
  cosine score); coordinator/mod.rs:tests cover the three branches.
- **GAP:** Controller-runtime passes `None` for the
  `drift_embedder` argument (controller_runtime.rs:3402–3414). No
  production embedding dispatcher is wired. Real deployments still
  log `similarity=none` because the controller boots without an
  outbound `ai.embed` peer. The trait + the hook are honest, but
  the production-side wiring is one step away.

### W5 — Implement real per-message session export

- **WHAT WAS ASKED:** `task.session_export` coordinator capability
  that assembles real turn-by-turn history from chronicle events;
  bridge endpoint calls it; turns include role / content / timestamp
  / session_id.
- **WHAT WAS BUILT:** `task.session_export` handler on the
  coordinator; new `TaskStore::query_chat_turns`;
  `parse_chat_turn_payload` helper; `TaskRecorder::session_export`
  wraps the mesh call; bridge's `/v1/sessions/export?session=`
  projects into `SessionExport`.
- **STATUS:** PARTIAL.
- **EVIDENCE:** coordinator/mod.rs:1583–1626 (query method);
  coordinator/mod.rs:6900–6976 (handler + parser); export.rs:159–215
  (bridge projection); four tests in coordinator/mod.rs cover the
  5-turn happy path, empty session, JSON envelope, and pipe-in-
  content edge.
- **GAP:** **The production chat flow does not write
  `chat.user_turn` / `chat.assistant_turn` chronicle events.** The
  capability assembles real history from those events, but no
  producer populates them. Hitting `/v1/sessions/export?session=…`
  on a real chat session today returns an empty turns array because
  the chronicle is empty for those event types. The
  `memory.write_turn` path lands the content in the memory node's
  SQLite store, not in the coordinator's chronicle. The chat flow
  needs a `task.event` call per turn (or the coordinator needs to
  query memory directly).

### W6 — Implement update.rs binary download + atomic self-replace

- **WHAT WAS ASKED:** Download new binary to a sibling temp file;
  atomically rename to replace the installed binary; handle the
  Windows running-.exe lock; failed download leaves installed
  binary untouched.
- **WHAT WAS BUILT:** `download_to` and `atomically_replace_binary`
  in update.rs; orchestration in `run()` resolves
  `std::env::current_exe`, downloads to a sibling temp file, and
  calls the replace primitive. Five tests including a
  single-request HTTP mock for the download.
- **STATUS:** DONE.
- **EVIDENCE:** update.rs:222–262 (`run` orchestration);
  update.rs:275–325 (`atomically_replace_binary` with the Windows
  `.old` fallback); update.rs:327–375 (`download_to` with
  `.partial` write + atomic rename); tests 282–419 exercise both
  pipelines end to end.
- **GAP:** GitHub-released assets are usually `.tar.gz` or `.zip`
  archives, not raw binaries. The current orchestrator downloads the
  asset and tries to rename it onto `relix.exe` even when the asset
  is a `.zip`. Production update for the canonical release matrix
  needs archive extraction in between download and replace. The
  primitives are right; the orchestrator's archive step is missing.

### W7 — Implement real OTLP HTTP transport + parse [observability.otel]

- **WHAT WAS ASKED:** Parse `[observability.otel]` from config.toml;
  POST OTLP/HTTP JSON on flush; spawn exporter on enabled startup;
  handle unreachable endpoint without panic.
- **WHAT WAS BUILT:** `OtelConfig` gained `enabled` + `endpoint_url`;
  `flush` is async and POSTs OTLP/HTTP JSON; `render_otlp_json`
  produces a compliant body; controller_runtime parses
  `[observability.otel]` and spawns a 5s flush loop; six tests
  exercise enabled POST, disabled no-op, unreachable endpoint, JSON
  shape.
- **STATUS:** PARTIAL.
- **EVIDENCE:** otel.rs:182–245 (async flush + transport);
  otel.rs:333–365 (OTLP JSON renderer);
  controller_runtime.rs:2861–2920 (config parsing + build helper);
  controller_runtime.rs:342–360 (spawn loop).
- **GAP:** **The bridge** is the actual producer of
  `MetadataEvent`s today (via `record_chat_observability`). The
  bridge's `AppState::observability` is constructed via
  `ObservabilityContext::in_memory()` in `config.rs:451` and never
  has an exporter attached. Bridge config has no
  `[observability.otel]` parsing. So an operator who turns on OTel
  in the *controller* config sees zero spans because the bridge's
  events never feed an exporter. The controller-side flush loop
  spins for nothing.

### W8 — Record ProvenanceSnapshot on every chat completion

- **WHAT WAS ASKED:** After every `/v1/chat/completions`, record a
  `ProvenanceSnapshot` with `model_id`, `system_prompt_hash`
  (SHA-256 or empty), timestamp, `session_id`.
- **WHAT WAS BUILT:** `sha256_hex` helper; `record_chat_provenance`
  + `record_chat_provenance_into`; called from `chat_completions`
  alongside `record_chat_observability`. Four tests cover the
  digest, non-empty-hash, empty-hash, and trace-id-fallback paths.
- **STATUS:** DONE.
- **EVIDENCE:** openai.rs:271–296 records the snapshot inside the
  chat handler; openai.rs:498–545 (record helpers);
  openai.rs:817–877 (tests).
- **GAP:** `policy_version` field stays empty because Relix has no
  global policy-version concept yet. Acceptable: the field exists on
  the wire so future commits can populate it without a schema break.

### Curator status endpoint (user-flagged)

- **WHAT WAS FLAGGED:** "a GET endpoint that returned a bridge_note
  saying 'I cannot see the scheduler state.'"
- **CURRENT REPO STATE:** `crates/relix-web-bridge/src/memory_curator.rs:155–181`
  is wired to a real `memory.curator_status` capability and parses
  the pipe-delimited body into the structured `StatusResponse`. The
  docstring on lines 10–19 still describes the older scaffold; the
  code does NOT match that description.
- **STATUS:** WORKING-BUT-STALE-DOCS. The status endpoint is real;
  the module-level docstring is misleading.
- **GAP:** Update the module docstring so a reader doesn't trust the
  scaffold claim. (Cosmetic; the code is fine.)

---

## Honest summary

- **Total items asked for:** 12 (4 observability tasks + 8 wiring
  gaps).
- **DONE:** 5 (O1, O2, O3, W2, W6, W8) — wait, that's 6.
- **DONE:** 6 — O1, O2, O3, W2, W6, W8.
- **PARTIAL:** 6 — O4 / W7 (bridge-side OTel producer not wired),
  W1 (handler stub), W3 (operator-sender stub), W4 (no production
  embedder), W5 (chat flow doesn't write chronicle events).
- **NOT BUILT:** 0.
- **BROKEN:** 0.

(The curator-status finding is a documentation drift, not an item
that was asked-for in this session.)

### Three most critical gaps before anything new ships

1. **W5 — production chat flow does not write chronicle turn events.**
   `task.session_export` exists, the capability works against seeded
   chronicle data, but no real chat session populates the chronicle.
   `/v1/sessions/export?session=…` returns `[]` for every real
   session today. This is the most operator-visible PARTIAL.
2. **W7 — bridge has no exporter attached.** The bridge is the only
   producer of `MetadataEvent`s in the current deployment shape;
   without bridge-side OTel wiring, the controller-side flush loop
   ships zero spans.
3. **W1 — admitted ToolCall steps do not execute.** The dispatcher
   admits them and records to the gateway, but the handler closure
   does not actually dial the tool node. Operators relying on
   planner-emitted tool calls see "admitted" in chronicle and
   nothing else happens.

---

## NEXT ACTIONS

In priority order:

1. **W5 chat-flow chronicle wiring.** When the chat flow handler
   persists a user turn (currently via `memory.write_turn`), also
   call the coordinator's `task.event` with event_type
   `chat.user_turn` and payload
   `<session_id>|user|<timestamp_unix>|<content>`. Same for the
   assistant reply. One unit test that an end-to-end chat call
   results in two chronicle events on the right session_id.

2. **W7 bridge-side OTel wiring.** Parse `[observability.otel]` from
   `BridgeConfig`. Build the exporter when enabled, attach via
   `ObservabilityContext::with_otel(...)`, spawn a flush loop in
   bridge `main.rs`. One integration test that hitting
   `/v1/chat/completions` after a successful chat lands a span in
   the mock OTLP collector.

3. **W1 mesh-side tool dispatch.** Replace the admission-only
   handler closure in `tool_runner::dispatch_planner_tool_calls`
   with a real outbound mesh call when the AI controller has a
   coord/tool peer wired. Honest fallback: keep the current "admit
   + record" path when no mesh client is configured, but switch the
   default to "execute when possible". Test against a mock tool
   server.

4. **W3 operator-channel wiring.** Replace the
   `|_,_| async move { None }` operator-sender with a closure that
   forwards the question to the configured Telegram approval queue
   (existing crate) or the dashboard intervention surface. Test
   against a mock channel.

5. **W4 production embedding dispatcher.** Implement
   `DriftEmbedDispatcher` over the AI node's `ai.embed` capability
   and pass it to `coordinator::register` from
   `controller_runtime`. Test against a mock embed responder.

6. **W6 archive extract step.** Detect when the downloaded asset is
   a `.tar.gz` or `.zip`, extract to a temp dir, and pick out the
   actual binary before calling `atomically_replace_binary`. Test
   against both archive formats.

7. **Curator docstring refresh.** Update the module-level docstring
   in `crates/relix-web-bridge/src/memory_curator.rs` so it
   accurately describes the wired endpoint. Read-only doc fix.
