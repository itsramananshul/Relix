//! Workflow engine unit tests. Cover the five behaviours
//! Part 2 of the foundation called out:
//!
//! - sequential two-agent workflow chains output → input.
//! - conditional workflow routes success vs. failure correctly.
//! - parallel workflow runs concurrently AND merges outputs.
//! - cycle in graph fails validation with a clear message.
//! - undefined variable reference fails validation with the var name.
//! - failed step with no failure handler produces `Failed` status.
//! - the execution trace captures every dispatch in order.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::sleep;

use super::ast::Workflow;
use super::chronicle::WorkflowChronicle;
use super::dispatcher::{DispatchError, DispatchResult, WorkflowDispatcher};
use super::executor::{ExecutionStatus, WorkflowEvent, execute, execute_with_events};
use super::parser::parse_str;
use super::store::WorkflowStore;
use super::validator::{ValidationError, validate};

/// Single dispatch call captured by [`StubDispatcher`].
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct DispatchCall {
    peer: String,
    capability: String,
    input: Vec<u8>,
}

/// Test dispatcher with programmable per-method responses and
/// a call log. Each `(peer, capability)` key maps to a vec of
/// responses; the dispatcher pops the front for each call so
/// tests can program a sequence of returns for retry-style
/// workflows.
struct StubDispatcher {
    responses: Mutex<BTreeMap<(String, String), Vec<DispatchResult>>>,
    calls: Mutex<Vec<DispatchCall>>,
    /// Optional per-method delay so parallel-execution tests
    /// can observe concurrent overlap.
    delays: Mutex<BTreeMap<(String, String), Duration>>,
}

impl StubDispatcher {
    fn new() -> Self {
        Self {
            responses: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            delays: Mutex::new(BTreeMap::new()),
        }
    }

    /// Programmable response. Each `respond` call appends one
    /// outcome; subsequent dispatches drain the queue
    /// front-first.
    async fn respond_ok(&self, peer: &str, cap: &str, body: &str) {
        self.responses
            .lock()
            .await
            .entry((peer.to_string(), cap.to_string()))
            .or_default()
            .push(Ok(body.as_bytes().to_vec()));
    }

    async fn respond_err(&self, peer: &str, cap: &str, cause: &str) {
        self.responses
            .lock()
            .await
            .entry((peer.to_string(), cap.to_string()))
            .or_default()
            .push(Err(DispatchError {
                peer: peer.to_string(),
                method: cap.to_string(),
                cause: cause.to_string(),
            }));
    }

    async fn set_delay(&self, peer: &str, cap: &str, d: Duration) {
        self.delays
            .lock()
            .await
            .insert((peer.to_string(), cap.to_string()), d);
    }

    async fn calls(&self) -> Vec<DispatchCall> {
        self.calls.lock().await.clone()
    }
}

#[async_trait]
impl WorkflowDispatcher for StubDispatcher {
    async fn dispatch(&self, peer: &str, capability: &str, input: &[u8]) -> DispatchResult {
        self.calls.lock().await.push(DispatchCall {
            peer: peer.to_string(),
            capability: capability.to_string(),
            input: input.to_vec(),
        });
        if let Some(d) = self
            .delays
            .lock()
            .await
            .get(&(peer.to_string(), capability.to_string()))
            .copied()
        {
            sleep(d).await;
        }
        let mut responses = self.responses.lock().await;
        let queue = responses
            .entry((peer.to_string(), capability.to_string()))
            .or_default();
        if queue.is_empty() {
            Err(DispatchError {
                peer: peer.to_string(),
                method: capability.to_string(),
                cause: "test dispatcher: no programmed response".to_string(),
            })
        } else {
            queue.remove(0)
        }
    }
}

fn parse_and_validate(src: &str) -> Workflow {
    let wf = parse_str(src).expect("parse workflow");
    validate(&wf, None).expect("validate workflow");
    wf
}

#[tokio::test]
async fn sequential_two_agents_chains_outputs() {
    let src = r#"
name: seq
version: 1
description: two agents in sequence
agents:
  first:
    peer: ai
    capability: chat
    input: "user said {{workflow.input}}"
    output: first
  second:
    peer: ai
    capability: chat
    input: "previous said {{first.output}}"
    output: second
flow:
  start: first
  edges:
    - { from: first, to: second, condition: success }
  result: "{{second.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "chat", "alpha").await;
    stub.respond_ok("ai", "chat", "beta").await;
    let result = execute(Arc::new(wf), stub.clone(), "hi").await;

    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.result, "beta");
    let calls = stub.calls().await;
    assert_eq!(calls.len(), 2, "two dispatches expected, got {calls:?}");
    assert_eq!(calls[0].input, b"user said hi");
    assert_eq!(calls[1].input, b"previous said alpha");
    assert_eq!(result.trace.steps.len(), 2);
    assert_eq!(result.trace.steps[0].agent, "first");
    assert_eq!(result.trace.steps[1].agent, "second");
}

#[tokio::test]
async fn conditional_routes_success_branch() {
    let src = r#"
name: cond
version: 1
agents:
  check:
    peer: ai
    capability: classify
    input: "{{workflow.input}}"
    output: check
  on_ok:
    peer: ai
    capability: handle_ok
    input: "ok branch saw {{check.output}}"
    output: ok
  on_err:
    peer: ai
    capability: handle_err
    input: "err branch saw {{check.output}}"
    output: err
flow:
  start: check
  edges:
    - { from: check, to: on_ok, condition: success }
    - { from: check, to: on_err, condition: failure }
  result: "{{ok.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "classify", "GOOD").await;
    stub.respond_ok("ai", "handle_ok", "HANDLED-OK").await;
    let result = execute(Arc::new(wf), stub.clone(), "?").await;

    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.result, "HANDLED-OK");
    let calls = stub.calls().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].capability, "handle_ok");
}

#[tokio::test]
async fn conditional_routes_failure_branch() {
    let src = r#"
name: cond
version: 1
agents:
  check:
    peer: ai
    capability: classify
    input: "{{workflow.input}}"
    output: check
  on_ok:
    peer: ai
    capability: handle_ok
    input: "ok branch"
    output: ok
  on_err:
    peer: ai
    capability: handle_err
    input: "err branch saw {{check.output}}"
    output: err
flow:
  start: check
  edges:
    - { from: check, to: on_ok, condition: success }
    - { from: check, to: on_err, condition: failure }
  result: "{{err.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_err("ai", "classify", "BAD").await;
    stub.respond_ok("ai", "handle_err", "HANDLED-ERR").await;
    let result = execute(Arc::new(wf), stub.clone(), "?").await;

    // A failure that gets recovered by a failure-handler
    // edge produces PartiallyFailed (the workflow completed
    // and the result resolved, but the trace contains a
    // failed step the operator should still see).
    assert_eq!(result.status, ExecutionStatus::PartiallyFailed);
    assert_eq!(result.result, "HANDLED-ERR");
    let calls = stub.calls().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].capability, "handle_err");
}

#[tokio::test]
async fn parallel_runs_concurrently_and_merges_outputs() {
    let src = r#"
name: par
version: 1
agents:
  start:
    peer: ai
    capability: noop
    input: "{{workflow.input}}"
    output: start
  branch_a:
    peer: ai
    capability: call_a
    input: "A saw {{start.output}}"
    output: a
  branch_b:
    peer: ai
    capability: call_b
    input: "B saw {{start.output}}"
    output: b
  combine:
    peer: ai
    capability: combine
    input: "{{a.output}} + {{b.output}}"
    output: combined
flow:
  start: start
  edges:
    - { from: start, to: branch_a, condition: parallel }
    - { from: start, to: branch_b, condition: parallel }
    - { from: branch_a, to: combine, condition: success }
    - { from: branch_b, to: combine, condition: success }
  result: "{{combined.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "noop", "init").await;
    stub.respond_ok("ai", "call_a", "AAA").await;
    stub.respond_ok("ai", "call_b", "BBB").await;
    stub.respond_ok("ai", "combine", "MERGED").await;
    // Each parallel branch sleeps 80ms; total wall time
    // should be ~80ms not ~160ms if they actually overlap.
    stub.set_delay("ai", "call_a", Duration::from_millis(80))
        .await;
    stub.set_delay("ai", "call_b", Duration::from_millis(80))
        .await;
    let t0 = std::time::Instant::now();
    let result = execute(Arc::new(wf), stub.clone(), "in").await;
    let elapsed = t0.elapsed();

    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.result, "MERGED");
    assert!(
        elapsed < Duration::from_millis(200),
        "parallel branches did not overlap: total elapsed {elapsed:?}"
    );
    // 4 calls: start, call_a, call_b, combine.
    assert_eq!(stub.calls().await.len(), 4);
}

#[test]
fn cycle_in_success_chain_fails_validation() {
    let src = r#"
name: loop
version: 1
agents:
  a:
    peer: ai
    capability: x
    input: "{{workflow.input}}"
    output: a
  b:
    peer: ai
    capability: x
    input: "{{a.output}}"
    output: b
flow:
  start: a
  edges:
    - { from: a, to: b, condition: success }
    - { from: b, to: a, condition: success }
"#;
    let wf = parse_str(src).expect("parse");
    let err = validate(&wf, None).expect_err("cycle should be rejected");
    match err {
        ValidationError::CycleDetected { path } => {
            assert!(path.contains(&"a".to_string()) && path.contains(&"b".to_string()));
        }
        other => panic!("expected CycleDetected, got {other:?}"),
    }
}

#[test]
fn undefined_variable_fails_validation_with_var_name() {
    let src = r#"
name: bad
version: 1
agents:
  only:
    peer: ai
    capability: x
    input: "uses {{ghost.output}}"
    output: only
flow:
  start: only
"#;
    let wf = parse_str(src).expect("parse");
    let err = validate(&wf, None).expect_err("undefined var should be rejected");
    match err {
        ValidationError::UndefinedVariable { var, .. } => {
            assert_eq!(var, "ghost.output");
        }
        other => panic!("expected UndefinedVariable, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_step_with_no_handler_returns_failed_status() {
    let src = r#"
name: nh
version: 1
agents:
  only:
    peer: ai
    capability: x
    input: "{{workflow.input}}"
    output: only
flow:
  start: only
  result: "{{only.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_err("ai", "x", "boom").await;
    let result = execute(Arc::new(wf), stub.clone(), "go").await;

    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(
        result.result.contains("only") && result.result.contains("boom"),
        "expected agent name + error cause in failure message; got: {}",
        result.result
    );
    assert_eq!(result.trace.steps.len(), 1);
    assert!(result.trace.steps[0].outcome.is_err());
}

#[tokio::test]
async fn execution_trace_captures_every_dispatch_in_order() {
    let src = r#"
name: trace
version: 1
agents:
  a:
    peer: peer_a
    capability: cap_a
    input: "{{workflow.input}}"
    output: a
  b:
    peer: peer_b
    capability: cap_b
    input: "{{a.output}}"
    output: b
  c:
    peer: peer_c
    capability: cap_c
    input: "{{b.output}}"
    output: c
flow:
  start: a
  edges:
    - { from: a, to: b, condition: success }
    - { from: b, to: c, condition: success }
  result: "{{c.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("peer_a", "cap_a", "A").await;
    stub.respond_ok("peer_b", "cap_b", "B").await;
    stub.respond_ok("peer_c", "cap_c", "C").await;
    let result = execute(Arc::new(wf), stub.clone(), "x").await;

    assert_eq!(result.status, ExecutionStatus::Success);
    assert_eq!(result.result, "C");
    assert_eq!(result.trace.steps.len(), 3);
    assert_eq!(result.trace.steps[0].agent, "a");
    assert_eq!(result.trace.steps[0].peer, "peer_a");
    assert_eq!(result.trace.steps[0].capability, "cap_a");
    assert_eq!(result.trace.steps[1].agent, "b");
    assert_eq!(result.trace.steps[1].input, "A");
    assert_eq!(result.trace.steps[2].agent, "c");
    assert_eq!(result.trace.steps[2].input, "B");
    assert_eq!(result.trace.steps[2].output, "C");
    // Total latency >= sum of step latencies (always true).
    let sum: u64 = result.trace.steps.iter().map(|s| s.latency_ms).sum();
    assert!(result.trace.total_latency_ms >= sum);
    assert!(!result.trace.execution_id.0.is_empty());
    assert_eq!(result.trace.workflow_name, "trace");
}

#[test]
fn unknown_peer_when_set_is_provided_fails() {
    let src = r#"
name: p
version: 1
agents:
  only:
    peer: ghost-peer
    capability: x
    input: "{{workflow.input}}"
    output: only
flow:
  start: only
"#;
    let wf = parse_str(src).expect("parse");
    let mut peers = std::collections::BTreeSet::new();
    peers.insert("real-peer".to_string());
    let err = validate(&wf, Some(&peers)).expect_err("unknown peer should fail");
    match err {
        ValidationError::UnknownPeer { peer, .. } => assert_eq!(peer, "ghost-peer"),
        other => panic!("expected UnknownPeer, got {other:?}"),
    }
}

#[test]
fn parser_reports_line_and_column_for_bad_field() {
    let src = "name: x\nversion: 1\nagents:\n  a:\n    peer: p\n    capability: c\n    input: i\n    output: o\nflow:\n  start: nowhere\n  unknown_field: oops\n";
    // unknown_field at line 11 should be rejected.
    let err = parse_str(src).expect_err("unknown field should fail");
    assert!(err.line >= 10, "expected line ~11, got {}", err.line);
    assert!(
        err.message.contains("unknown_field"),
        "message should name the bad field, got: {}",
        err.message
    );
}

#[tokio::test]
async fn streaming_events_arrive_in_order() {
    let src = r#"
name: streamy
version: 1
agents:
  a:
    peer: ai
    capability: cap_a
    input: "{{workflow.input}}"
    output: a
  b:
    peer: ai
    capability: cap_b
    input: "{{a.output}}"
    output: b
flow:
  start: a
  edges:
    - { from: a, to: b, condition: success }
  result: "{{b.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "cap_a", "ALPHA").await;
    stub.respond_ok("ai", "cap_b", "BETA").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkflowEvent>();
    let result = execute_with_events(Arc::new(wf), stub.clone(), "in", tx).await;
    assert_eq!(result.status, ExecutionStatus::Success);

    // Drain the channel — the sender is dropped at the end
    // of `execute_with_events` so `recv()` returns None on
    // EOF, not blocked.
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    // Expected sequence: Started, StepStarted(a), StepCompleted(a),
    // StepStarted(b), StepCompleted(b), Finished.
    assert_eq!(events.len(), 6, "got events: {events:?}");
    assert!(matches!(events[0], WorkflowEvent::Started { .. }));
    assert!(
        matches!(&events[1], WorkflowEvent::StepStarted { agent, .. } if agent == "a"),
        "expected StepStarted(a), got {:?}",
        events[1]
    );
    assert!(
        matches!(&events[2], WorkflowEvent::StepCompleted { agent, output, .. } if agent == "a" && output == "ALPHA"),
        "expected StepCompleted(a, ALPHA), got {:?}",
        events[2]
    );
    assert!(
        matches!(&events[3], WorkflowEvent::StepStarted { agent, input, .. } if agent == "b" && input == "ALPHA"),
        "expected StepStarted(b, ALPHA), got {:?}",
        events[3]
    );
    assert!(
        matches!(&events[4], WorkflowEvent::StepCompleted { agent, output, .. } if agent == "b" && output == "BETA"),
        "expected StepCompleted(b, BETA), got {:?}",
        events[4]
    );
    assert!(matches!(&events[5], WorkflowEvent::Finished(r) if r.result == "BETA"));
}

#[tokio::test]
async fn streaming_emits_step_failed_on_dispatch_error() {
    let src = r#"
name: failstream
version: 1
agents:
  a:
    peer: ai
    capability: only
    input: "{{workflow.input}}"
    output: a
flow:
  start: a
  result: "{{a.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_err("ai", "only", "boom").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<WorkflowEvent>();
    let result = execute_with_events(Arc::new(wf), stub.clone(), "x", tx).await;
    assert_eq!(result.status, ExecutionStatus::Failed);
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkflowEvent::StepFailed { agent, error, .. } if agent == "a" && error.contains("boom"))),
        "expected a StepFailed(a, boom) event in {events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(WorkflowEvent::Finished(r)) if r.status == ExecutionStatus::Failed
    ));
}

#[tokio::test]
async fn partially_failed_status_fires_for_recovered_failure() {
    let src = r#"
name: pf
version: 1
agents:
  check:
    peer: ai
    capability: classify
    input: "{{workflow.input}}"
    output: check
  recover:
    peer: ai
    capability: handle_err
    input: "saw {{check.output}}"
    output: recover
flow:
  start: check
  edges:
    - { from: check, to: recover, condition: failure }
  result: "{{recover.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_err("ai", "classify", "BAD").await;
    stub.respond_ok("ai", "handle_err", "RECOVERED").await;
    let result = execute(Arc::new(wf), stub.clone(), "?").await;
    assert_eq!(result.status, ExecutionStatus::PartiallyFailed);
    assert_eq!(result.result, "RECOVERED");
    assert_eq!(result.status.as_str(), "partially_failed");
}

#[tokio::test]
async fn chronicle_round_trip_persists_full_execution() {
    let src = r#"
name: persisted
version: 1
agents:
  step1:
    peer: peer_a
    capability: cap_a
    input: "{{workflow.input}}"
    output: step1
flow:
  start: step1
  result: "{{step1.output}}"
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("peer_a", "cap_a", "PAYLOAD").await;
    let result = execute(Arc::new(wf), stub.clone(), "in").await;
    assert_eq!(result.status, ExecutionStatus::Success);

    let chronicle = WorkflowChronicle::in_memory().expect("in-memory chronicle");
    chronicle
        .record(&result, "in", 1_700_000_000, 1_700_000_001, "default")
        .expect("record");

    let fetched = chronicle
        .get(&result.trace.execution_id.0)
        .expect("get")
        .expect("execution exists");

    assert_eq!(fetched.execution_id, result.trace.execution_id.0);
    assert_eq!(fetched.workflow_name, "persisted");
    assert_eq!(fetched.input, "in");
    assert_eq!(fetched.status, "success");
    assert_eq!(fetched.result, "PAYLOAD");
    assert_eq!(fetched.started_at, 1_700_000_000);
    assert_eq!(fetched.ended_at, 1_700_000_001);
    assert_eq!(fetched.steps.len(), 1);
    assert_eq!(fetched.steps[0].agent, "step1");
    assert_eq!(fetched.steps[0].peer, "peer_a");
    assert_eq!(fetched.steps[0].capability, "cap_a");
    assert_eq!(fetched.steps[0].output, "PAYLOAD");
    assert!(fetched.steps[0].error.is_none());

    // Unknown id → Ok(None).
    let miss = chronicle.get("00000000").expect("get unknown");
    assert!(miss.is_none());
}

#[test]
fn store_reload_clears_cache_so_disk_edits_pick_up() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("demo.workflow");
    let v1 = r#"name: demo
version: 1
description: v1
agents:
  a:
    peer: ai
    capability: cap
    input: "{{workflow.input}}"
    output: a
flow:
  start: a
  result: "{{a.output}}"
"#;
    std::fs::write(&path, v1).expect("write v1");
    let store = WorkflowStore::new(dir.path().to_path_buf());
    let first = store.get("demo").expect("get v1");
    assert_eq!(first.description, "v1");

    // Edit the file in place. Without `clear_cache` the
    // store would still return the cached v1 ast.
    let v2 = v1.replacen("description: v1", "description: v2", 1);
    let mut f = std::fs::File::create(&path).expect("recreate file");
    f.write_all(v2.as_bytes()).expect("write v2");
    drop(f);

    let still_cached = store.get("demo").expect("get cached");
    assert_eq!(still_cached.description, "v1", "cache should still be warm");

    store.clear_cache();
    let after_reload = store.get("demo").expect("get reloaded");
    assert_eq!(after_reload.description, "v2");
}

#[test]
fn shipped_example_workflows_validate() {
    // The three example .workflow files under
    // examples/workflows/ MUST parse + validate so operators
    // can copy them as starting templates.
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let examples_dir = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("examples").join("workflows"))
        .expect("examples/workflows dir resolvable from runtime crate");
    if !examples_dir.exists() {
        // When the crate is consumed outside the repo (e.g.
        // crates.io publish) the examples aren't shipped. The
        // test is skipped rather than failed.
        return;
    }
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&examples_dir).expect("read examples dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("workflow") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let wf =
            parse_str(&source).unwrap_or_else(|e| panic!("parse {} failed: {e}", path.display()));
        validate(&wf, None).unwrap_or_else(|e| panic!("validate {} failed: {e}", path.display()));
        checked += 1;
    }
    assert!(
        checked >= 3,
        "expected ≥ 3 example workflows under examples/workflows/, found {checked}"
    );
}

// ── RELIX-7.24 cancellation primitive ────────────────────

#[tokio::test]
async fn cancellation_flag_aborts_workflow_between_steps() {
    use super::execute_with_cancellation;
    use super::executor::CancellationFlag;
    let src = r#"
name: cancel_seq
version: 1
agents:
  first:
    peer: ai
    capability: chat
    input: "{{workflow.input}}"
    output: first
  second:
    peer: ai
    capability: chat
    input: "after {{first.output}}"
    output: second
flow:
  start: first
  edges:
    - { from: first, to: second, condition: success }
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "chat", "alpha").await;
    stub.respond_ok("ai", "chat", "beta").await;
    let cancel = CancellationFlag::new();
    // Pre-cancel BEFORE we start so the first step is the
    // one the flag sees.
    cancel.cancel_with_reason("operator pulled the plug");
    let result = execute_with_cancellation(Arc::new(wf), stub.clone(), "hi", None, cancel).await;
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert_eq!(result.result, "operator pulled the plug");
    // No dispatches happened — flag was set before the first
    // step's check.
    assert!(stub.calls().await.is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_flag_post_first_step_lets_first_dispatch_complete() {
    use super::execute_with_cancellation;
    use super::executor::CancellationFlag;
    // Single-threaded runtime + the executor's pre-step
    // yield_now() gives the listener a deterministic window
    // to set the cancel flag between events. On
    // multi-threaded runtimes the race is best-effort; we
    // pin this test to current_thread so it's
    // deterministic.
    let src = r#"
name: cancel_mid
version: 1
agents:
  first:
    peer: ai
    capability: chat
    input: "{{workflow.input}}"
    output: first
  second:
    peer: ai
    capability: chat
    input: "after {{first.output}}"
    output: second
flow:
  start: first
  edges:
    - { from: first, to: second, condition: success }
"#;
    let wf = parse_and_validate(src);
    let stub = Arc::new(StubDispatcher::new());
    stub.respond_ok("ai", "chat", "alpha").await;
    stub.respond_ok("ai", "chat", "beta").await;
    let cancel = CancellationFlag::new();
    // Cancel AFTER the first step finishes — listen on the
    // event stream for StepCompleted on `first` and flip the
    // flag.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel_for_listener = cancel.clone();
    tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            if let WorkflowEvent::StepCompleted { agent, .. } = ev
                && agent == "first"
            {
                cancel_for_listener.cancel_with_reason("verification critical");
                break;
            }
        }
    });
    let result =
        execute_with_cancellation(Arc::new(wf), stub.clone(), "hi", Some(tx), cancel).await;
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert_eq!(result.result, "verification critical");
    // The first step ran; the second never did.
    let calls = stub.calls().await;
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one dispatch, got {calls:?}"
    );
    assert_eq!(calls[0].input, b"hi");
}

#[test]
fn cancellation_flag_reason_round_trips() {
    use super::executor::CancellationFlag;
    let f = CancellationFlag::new();
    assert!(!f.is_cancelled());
    assert!(f.reason().is_none());
    f.cancel_with_reason("operator quit");
    assert!(f.is_cancelled());
    assert_eq!(f.reason().as_deref(), Some("operator quit"));
}

#[test]
fn cancellation_flag_clones_share_state() {
    use super::executor::CancellationFlag;
    let f1 = CancellationFlag::new();
    let f2 = f1.clone();
    assert!(!f1.is_cancelled());
    assert!(!f2.is_cancelled());
    f1.cancel();
    assert!(f2.is_cancelled(), "clone should see the cancel");
}
