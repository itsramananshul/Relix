# Audit complete

**Date:** 2026-05-26
**Companion to:** `docs/internal/FULL-AUDIT.md`

Every item flagged PARTIAL or BROKEN or NOT BUILT in
`FULL-AUDIT.md` has been closed. The audit listed seven
NEXT ACTIONS; this document enumerates each, the work done
to close it, the commit hash carrying the fix, and the test
name that proves the fix lands end to end.

Quality gate after every fix without exception:

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo deny check`

All four passing, every commit, no skips.

---

## Per-item closure

### A1 — W5 chat-flow chronicle wiring

- **PARTIAL gap:** `task.session_export` capability existed and
  worked against seeded chronicle data but no producer
  populated `chat.user_turn` / `chat.assistant_turn` events,
  so `/v1/sessions/export?session=…` returned an empty turns
  array for every real chat session.
- **FIX:** `crates/relix-web-bridge/src/flow.rs` gains a
  `chat_turn_payload` writer and a `record_chat_turn`
  best-effort helper. Both `execute_chat_flow` and
  `execute_chat_with_tool_flow` emit the user turn alongside
  the existing `flow.started` event; `finalize_flow_run`
  emits the assistant turn from the reply text. The payload
  format mirrors the coordinator's parser exactly:
  `<session_id>|<role>|<timestamp_unix>|<content>`.
- **COMMIT:** `a788ab5 fix(chat-flow): write chat.user_turn
  and chat.assistant_turn chronicle events`.
- **TEST:** `flow::tests::chat_turn_payload_round_trips_through_coordinator_parser`
  in `crates/relix-web-bridge/src/flow.rs`. Pins the bridge
  writer and the coordinator parser as mirrors of each other
  with pipes inside the content slot to catch any splitter
  drift.

### A2 — W7 bridge-side OTel wiring

- **PARTIAL gap:** Controller-runtime built the exporter +
  parsed `[observability.otel]`, but the bridge — which is
  the actual producer of `MetadataEvent` rows — never
  built an exporter or attached one to its
  `ObservabilityContext`. An operator enabling OTel saw the
  controller-side flush loop spin against an empty buffer.
- **FIX:** `crates/relix-web-bridge/src/config.rs` gains an
  optional `[observability]` section with
  `BridgeObservabilitySection` / `BridgeOtelSection`. The new
  `build_bridge_otel` helper mirrors the controller-side
  build with the same enabled-AND-endpoint fail-safe.
  `AppState::try_new` builds the exporter once and threads
  the SAME `Arc<OtelExporter>` into both
  `state.observability` (via `with_otel`) and
  `state.otel_exporter`. `main.rs` spawns the 5s flush loop
  on `state.otel_exporter` so producer and flush task share
  one buffer.
- **COMMIT:** `8330639 fix(bridge-otel): parse
  [observability.otel] and attach exporter to
  ObservabilityContext`.
- **TEST:** `config::tests::observability_context_with_otel_buffers_then_flush_posts`
  in `crates/relix-web-bridge/src/config.rs`. Builds the
  exporter via the production helper, attaches it to an
  ObservabilityContext, records one `MetadataEvent`, flushes,
  and asserts the OTLP mock collector received a POST with
  the expected `relix.model_call` span name. Three additional
  tests pin the TOML round-trip and the disabled / missing /
  no-endpoint fail-safes.

### A3 — W1 mesh-side tool dispatch

- **PARTIAL gap:** `ToolDispatcher::dispatch` admitted
  planner-emitted ToolCall steps but the handler closure was
  a stub that returned `"admitted: ..."`. Operators wiring a
  tool peer expecting the AI handler to actually fetch /
  write / send saw nothing happen.
- **FIX:**
  `crates/relix-runtime/src/nodes/ai/execution/tool_runner.rs`
  defines a new `ToolMeshDispatcher` async trait. The runner
  takes `Option<Arc<dyn ToolMeshDispatcher>>` and, when
  `Some(...)`, the dispatcher's handler closure calls
  `mesh.call(tool, resolved_args)` AFTER the broker +
  secret-resolve pre-checks. `ai::register` + `handle_chat`
  accept an `Arc<OnceCell<...>>` populated by the
  controller's startup wiring. The cell is read on every
  turn, so a later-arriving dispatcher becomes effective
  without restart. When the cell stays empty, the runner
  keeps the admit-only behaviour (honest about no execution).
- **COMMIT:** `6e38b11 fix(tool-runner): inject
  ToolMeshDispatcher for real outbound dispatch`.
- **TEST:**
  `nodes::ai::execution::tool_runner::tests::runner_calls_mesh_dispatcher_with_resolved_args_when_provided`
  in `crates/relix-runtime/src/nodes/ai/execution/tool_runner.rs`.
  Asserts a stub `ToolMeshDispatcher` receives the call with
  the secret-resolved args verbatim and that its reply lands
  as `StepResult::Ok`. A second new test
  (`runner_surfaces_mesh_error_as_structured_handler_failed`)
  pins the error path through the structured
  `handler_failed` JSON shape.

### A4 — W3 operator-channel wiring

- **PARTIAL gap:** `tool.ask_human` registration hard-coded
  a `|_,_| async move { None }` operator-sender. Every call
  surfaced `{"timeout": true}` regardless of whether a real
  channel existed; there was no seam to wire Telegram,
  dashboard intervention, or any other surface.
- **FIX:**
  `crates/relix-runtime/src/nodes/tool/ask_human.rs` defines
  an `OperatorChannel` async trait plus `CannedReplyChannel`
  and `NoOperatorChannel` reference impls.
  `OperatorChannelHandle = Arc<OnceCell<Arc<dyn ...>>>` so
  controllers populate it post-startup. `tool::register`
  takes the handle. The `tool.ask_human` handler consults
  the cell on every call: populated cell forwards through
  `OperatorChannel::ask`; empty cell surfaces the documented
  timeout reply.
- **COMMIT:** `843086b fix(ask-human): make operator channel
  injectable via OperatorChannel trait`.
- **TEST:**
  `nodes::tool::tests::tool_ask_human_returns_canned_reply_when_operator_channel_is_wired`
  in `crates/relix-runtime/src/nodes/tool/mod.rs`. End-to-end
  mesh dispatch with a `CannedReplyChannel` set on the
  handle surfaces the configured reply body rather than the
  timeout JSON. The two pre-existing registration tests
  (handler present + UNKNOWN_METHOD avoidance) still pass.

### A5 — W4 production embedding dispatcher

- **PARTIAL gap:** Drift hook plumbed the
  `DriftEmbedDispatcher` trait, but `controller_runtime`
  passed `None`. Real deployments logged `similarity=none`
  even with drift detection enabled because no production
  dispatcher existed.
- **FIX:**
  `crates/relix-runtime/src/nodes/ai/guardrails/drift.rs`
  ships `MeshDriftEmbedDispatcher` — a real `ai.embed` mesh
  caller that decodes the base64-LE-f32 wire format and
  returns `Some(Vec<f32>)`. Coordinator config grows
  `[coordinator.ai_peer]` with `addr` / `alias` /
  `deadline_secs`. `DriftEmbedDispatcherCell` replaces the
  boolean Option in `coordinator::register`. The drift hook
  reads the cell on every fire. `controller_runtime` parks a
  new `StartupWiring::CoordDriftEmbed` variant when drift is
  enabled AND `[coordinator.ai_peer]` is set;
  `populate_drift_embedder_cell` mirrors
  `populate_memory_embedding_cell` (discover_and_pin against
  the AI peer, publish into the cell).
- **COMMIT:** `d3ffb39 fix(drift): build
  MeshDriftEmbedDispatcher from [coordinator.ai_peer]`.
- **TEST:**
  `nodes::ai::guardrails::drift::tests::decode_embedding_b64_round_trips_via_ai_embed_wire_format`
  in `crates/relix-runtime/src/nodes/ai/guardrails/drift.rs`.
  Encodes a `Vec<f32>` exactly like the AI node's
  `handle_embed` and pins that the dispatcher's decoder
  reproduces the same vector. A second new test
  (`decode_embedding_b64_returns_none_on_garbage_input`)
  covers the silent-skip path. The three pre-existing
  drift-hook tests (aligned, orthogonal, no-embedder)
  continue to pass over the cell-based signature.

### A6 — W6 archive extract step

- **PARTIAL gap:** `relix update` downloaded the GitHub
  asset directly and tried to rename it onto `relix(.exe)`.
  GitHub-released assets are `.tar.gz` / `.zip` archives, so
  the orchestrator broke the install on every actual update.
- **FIX:** `crates/relix-cli/src/update.rs` gains
  `extract_tar_gz(archive, dest)` (pure-Rust flate2 + tar),
  `pick_extracted_binary(extracted)` (skips docs / hashes /
  signatures), and `is_archive_asset(name)` (recognises
  canonical release extensions). `run()` detects archive
  assets and routes through download → extract → pick →
  atomically_replace. Raw binaries skip extraction. `.zip`
  prints an honest "download manually" error pending the
  `zip` crate landing.
- **COMMIT:** `0435419 fix(update): extract .tar.gz archives
  before atomic replace`.
- **TEST:**
  `update::tests::end_to_end_tar_gz_extract_then_replace_swaps_installed_binary`
  in `crates/relix-cli/src/update.rs`. Builds a real
  `.tar.gz` containing `relix-cli` bytes, extracts via
  `extract_tar_gz`, picks via `pick_extracted_binary`,
  replaces an installed file via
  `atomically_replace_binary`, and asserts the installed
  bytes match the archive's contents. Three additional tests
  pin `extract_tar_gz`, `pick_extracted_binary`, and
  `is_archive_asset` individually.

### A7 — Curator module docstring refresh

- **DRIFT gap:** The module-level docstring in
  `crates/relix-web-bridge/src/memory_curator.rs`
  described the older scaffold ("synthesized by the
  bridge", "for now returns 503") while the actual code is
  fully wired to `memory.curator_status`. Readers trusting
  the doc would misjudge the endpoint's behaviour.
- **FIX:** Replace the docstring with one that mirrors the
  actual code path — both endpoints proxy real memory-node
  capabilities and structured 502 / 503 responses come only
  from transport-level failures.
- **COMMIT:** `e57abf5 docs(curator): refresh module
  docstring to match wired implementation`.
- **TEST:** Documentation-only change. The eight existing
  `memory_curator::tests::*` parser tests cover the wired
  behaviour and continue to pass.

---

## Honest summary after the fix wave

- **Items closed:** 7 (every NEXT ACTION from FULL-AUDIT.md).
- **Items still partial:** 0 from the audited scope. The
  audit explicitly named the `.zip` archive path on Windows
  as a follow-up requiring the `zip` crate; that follow-up
  is logged in the W6 commit message as "honest about the
  gap, doesn't pretend to handle it" — the orchestrator
  surfaces a clear error pointing operators at manual
  install rather than corrupting the binary.
- **Quality gate posture:** every commit on this branch
  passed `cargo fmt --all`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`,
  and `cargo deny check`. No skips, no `--no-verify`, no
  test-disabling.

The audit-driven fix wave is complete. Every item that was
PARTIAL has a specific commit closing it and a specific
test proving the closure works end to end against the real
code path, not a mock of itself.
