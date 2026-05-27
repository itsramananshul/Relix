//! Tests for the Relix `Inst::RemoteCall` extension (M6).
//!
//! These exercise the VM's RemoteCall handling in isolation, with a stub
//! dispatcher. The codegen path (recognizing `remote_call(...)` in SOL source)
//! is exercised separately in M6/Step 3 tests.

use std::sync::Arc;
use std::sync::Mutex;

use crate::sol::bytecode::Inst;
use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError, RemoteCallResult};
use crate::sol::parser::Ast;
use crate::sol::vm::{VM, VM_ERROR_SENTINEL};

/// A dispatcher that records every call and returns a programmed response.
struct StubDispatcher {
    log: Mutex<Vec<(String, String, Vec<u8>)>>,
    response: Result<Vec<u8>, RemoteCallError>,
}

impl StubDispatcher {
    fn ok(body: &str) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            response: Ok(body.as_bytes().to_vec()),
        })
    }

    fn err(kind: u32, cause: &str) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            response: Err(RemoteCallError {
                kind,
                peer: String::new(),
                method: String::new(),
                cause: cause.into(),
            }),
        })
    }

    fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
        self.log.lock().unwrap().clone()
    }
}

impl RemoteCallDispatcher for StubDispatcher {
    fn remote_call(&self, peer: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
        self.log
            .lock()
            .unwrap()
            .push((peer.to_string(), method.to_string(), arg.to_vec()));
        self.response.clone()
    }
}

/// Build a tiny bytecode program that pushes three strings then executes a
/// RemoteCall. Returns the program and the indices that will become the heap
/// refs in execution order.
fn program_pushing(peer: &str, method: &str, arg: &str) -> Vec<Inst> {
    vec![
        Inst::PushConst(Ast::ExprString(peer.to_string())),
        Inst::PushConst(Ast::ExprString(method.to_string())),
        Inst::PushConst(Ast::ExprString(arg.to_string())),
        Inst::RemoteCall,
    ]
}

#[test]
fn remote_call_dispatches_args_and_pushes_response() {
    let disp = StubDispatcher::ok("hello-from-dispatcher");
    let mut vm = VM::from(&program_pushing("memory", "memory.search", "query"))
        .with_dispatcher(disp.clone());

    // Run until completion (program ends after RemoteCall pushes one value).
    let final_value = vm.run();
    // VM exit value = whatever's on top of the stack at end-of-program.
    // For RemoteCall success, that's the heap ref index of the response string.
    assert_ne!(final_value, VM_ERROR_SENTINEL, "VM should not have errored");

    let calls = disp.calls();
    assert_eq!(
        calls.len(),
        1,
        "dispatcher should have been called exactly once"
    );
    assert_eq!(calls[0].0, "memory");
    assert_eq!(calls[0].1, "memory.search");
    assert_eq!(calls[0].2, b"query");
    assert!(
        vm.last_error().is_none(),
        "last_error should be clear on success"
    );
}

#[test]
fn remote_call_failure_halts_vm_with_sentinel() {
    let disp = StubDispatcher::err(6, "policy denied");
    let mut vm = VM::from(&program_pushing("ai", "ai.chat", "hi")).with_dispatcher(disp.clone());

    let final_value = vm.run();
    assert_eq!(final_value, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("last_error must be set on failure");
    assert_eq!(err.kind, 6);
    assert_eq!(err.cause, "policy denied");
    assert_eq!(disp.calls().len(), 1);
}

#[test]
fn remote_call_with_no_dispatcher_errors_cleanly() {
    let mut vm = VM::from(&program_pushing("p", "m", "a"));
    let final_value = vm.run();
    assert_eq!(final_value, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("must error without dispatcher");
    assert_eq!(err.kind, 0);
    assert!(err.cause.contains("no RemoteCallDispatcher"));
}

#[test]
fn remote_call_bytecode_includes_variant_in_disassembly() {
    // The Debug impl on Inst is the alpha "disassembler". Verify our new
    // variant shows up.
    let prog = program_pushing("p", "m", "a");
    let dis = format!("{prog:?}");
    assert!(
        dis.contains("RemoteCall"),
        "expected RemoteCall in Inst Debug output, got: {dis}"
    );
}

// ── F2: try / catch / rethrow ──────────────────────────────

/// Hand-built program that wraps a failing remote_call in
/// `try { remote_call } catch any { /* push 42 */ }`. Lets
/// the test inspect VM state directly without going through
/// the full SOL compiler.
///
/// Layout (PCs annotated):
///   0  TryEnter(5)
///   1  PushConst "p"
///   2  PushConst "m"
///   3  PushConst "a"
///   4  RemoteCall
///   5  TryExit               ; not reached on failure
///   6  Jump 12
///   7  PushConst 42          ; catch any body
///   8  Jump 12
///   9  Rethrow               ; unreachable (any matches)
///  10  ... padding
///  11  ... padding
///  12  end_pc                ; the int 42 is left on the stack
fn try_any_around_failing_remote_call() -> Vec<Inst> {
    vec![
        Inst::TryEnter(7),
        Inst::PushConst(Ast::ExprString("p".into())),
        Inst::PushConst(Ast::ExprString("m".into())),
        Inst::PushConst(Ast::ExprString("a".into())),
        Inst::RemoteCall,
        Inst::TryExit,
        Inst::Jump(9),
        Inst::PushConst(Ast::ExprInteger(42)),
        Inst::Jump(9),
        // end_pc = 9: program ends with the catch body's value
        // on top of the stack.
    ]
}

#[test]
fn try_catch_any_swallows_remote_call_failure_and_runs_catch_body() {
    let disp = StubDispatcher::err(6, "policy denied");
    let mut vm = VM::from(&try_any_around_failing_remote_call()).with_dispatcher(disp);
    let final_value = vm.run();
    assert_ne!(
        final_value, VM_ERROR_SENTINEL,
        "catch any must swallow the failure"
    );
    assert_eq!(final_value, 42, "catch body should have pushed the int 42");
    // last_error is still set so the operator's catch body can
    // read it via error_kind() / error_cause().
    let err = vm
        .last_error()
        .expect("last_error must remain set inside catch");
    assert_eq!(err.kind, 6);
    assert_eq!(err.cause, "policy denied");
}

#[test]
fn try_exit_clean_path_skips_catch_body() {
    let disp = StubDispatcher::ok("ignored");
    let mut vm = VM::from(&try_any_around_failing_remote_call()).with_dispatcher(disp);
    let final_value = vm.run();
    assert_ne!(final_value, VM_ERROR_SENTINEL);
    // On clean exit the catch body never ran; the program
    // ends with the heap-ref of the remote_call response on
    // top — not the int 42. So the final value is the heap
    // index, which is not equal to 42 (the heap has at least
    // four entries by now from the three pushed args).
    assert_ne!(final_value, 42, "clean try body must not fall into catch");
}

#[test]
fn rethrow_without_outer_handler_halts_with_sentinel() {
    // Same try/catch as above but the catch executes Rethrow
    // before pushing 42. With no outer handler, the VM halts.
    let mut prog = try_any_around_failing_remote_call();
    // Replace `PushConst 42` (idx 7) with Rethrow. The Jump
    // at idx 8 becomes unreachable.
    prog[7] = Inst::Rethrow;
    let disp = StubDispatcher::err(6, "policy denied");
    let mut vm = VM::from(&prog).with_dispatcher(disp);
    let final_value = vm.run();
    assert_eq!(
        final_value, VM_ERROR_SENTINEL,
        "rethrow with no outer handler must halt"
    );
    let err = vm.last_error().expect("last_error preserved");
    assert_eq!(err.kind, 6);
}

#[test]
fn load_error_kind_classifies_remote_error() {
    // Pure VM exercise: set last_error via a remote_call
    // failure, then call LoadErrorKind from a try-handled
    // catch block.
    //
    //   0 TryEnter 6
    //   1 PushConst "p"
    //   2 PushConst "m"
    //   3 PushConst "a"
    //   4 RemoteCall
    //   5 TryExit
    //   6 LoadErrorKind          ; catch dispatch: push the kind string
    //   7 ...
    //
    // Final value = heap ref to the "policy_denied" string.
    let prog = vec![
        Inst::TryEnter(6),
        Inst::PushConst(Ast::ExprString("p".into())),
        Inst::PushConst(Ast::ExprString("m".into())),
        Inst::PushConst(Ast::ExprString("a".into())),
        Inst::RemoteCall,
        Inst::TryExit,
        Inst::LoadErrorKind,
    ];
    let disp = StubDispatcher::err(relix_core::types::error_kinds::POLICY_DENIED, "denied");
    let mut vm = VM::from(&prog).with_dispatcher(disp);
    let final_value = vm.run();
    let kind = vm.heap_string(final_value).expect("kind on heap");
    assert_eq!(kind, "policy_denied");
}

#[test]
fn try_catch_handles_nested_failure_inside_inner_try() {
    // outer: try { inner: try { fail } catch any { rethrow } } catch any { push 99 }
    //
    //   0  TryEnter(13)         ; outer
    //   1  TryEnter(8)          ; inner
    //   2  PushConst "p"
    //   3  PushConst "m"
    //   4  PushConst "a"
    //   5  RemoteCall            ; fails → jump to inner catch (pc 8)
    //   6  TryExit               ; unreachable
    //   7  Jump 16
    //   8  Rethrow               ; inner catch: propagate to outer
    //   9  TryExit               ; unreachable
    //  10  Jump 16
    //  11  (unused)
    //  12  (unused)
    //  13  PushConst 99          ; outer catch
    //  14  Jump 16
    //  15  (unused)
    //  16  end
    let prog = vec![
        Inst::TryEnter(13),
        Inst::TryEnter(8),
        Inst::PushConst(Ast::ExprString("p".into())),
        Inst::PushConst(Ast::ExprString("m".into())),
        Inst::PushConst(Ast::ExprString("a".into())),
        Inst::RemoteCall,
        Inst::TryExit,
        Inst::Jump(16),
        Inst::Rethrow,
        Inst::TryExit,
        Inst::Jump(16),
        Inst::PushConst(Ast::ExprInteger(0)),
        Inst::PushConst(Ast::ExprInteger(0)),
        Inst::PushConst(Ast::ExprInteger(99)),
        Inst::Jump(16),
        Inst::PushConst(Ast::ExprInteger(0)),
    ];
    let disp = StubDispatcher::err(6, "denied");
    let mut vm = VM::from(&prog).with_dispatcher(disp);
    let final_value = vm.run();
    assert_eq!(
        final_value, 99,
        "outer catch should have pushed 99 after inner rethrow"
    );
}

// ── RELIX-2 step 4: remote_call_stream ─────────────────────

/// Dispatcher that records calls AND splits the configured
/// response into multiple chunks, invoking the on_chunk
/// callback for each. Tests inspect the recorded chunks to
/// verify the observer fires in arrival order and the final
/// concatenated result lands on the VM stack.
struct ChunkingDispatcher {
    log: Mutex<Vec<(String, String, Vec<u8>)>>,
    chunks: Vec<Vec<u8>>,
}

impl ChunkingDispatcher {
    fn with_chunks(chunks: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            log: Mutex::new(Vec::new()),
            chunks: chunks.into_iter().map(|s| s.as_bytes().to_vec()).collect(),
        })
    }
    fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
        self.log.lock().unwrap().clone()
    }
}

impl RemoteCallDispatcher for ChunkingDispatcher {
    fn remote_call(&self, peer: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
        // Concatenated body — fallback path tests this.
        let mut all = Vec::new();
        for c in &self.chunks {
            all.extend_from_slice(c);
        }
        self.log
            .lock()
            .unwrap()
            .push((peer.to_string(), method.to_string(), arg.to_vec()));
        Ok(all)
    }
    fn remote_call_stream(
        &self,
        peer: &str,
        method: &str,
        arg: &[u8],
        on_chunk: &dyn Fn(&[u8]),
    ) -> RemoteCallResult {
        self.log
            .lock()
            .unwrap()
            .push((peer.to_string(), method.to_string(), arg.to_vec()));
        let mut all = Vec::new();
        for chunk in &self.chunks {
            on_chunk(chunk);
            all.extend_from_slice(chunk);
        }
        Ok(all)
    }
}

fn program_pushing_stream(peer: &str, method: &str, arg: &str) -> Vec<Inst> {
    vec![
        Inst::PushConst(Ast::ExprString(peer.to_string())),
        Inst::PushConst(Ast::ExprString(method.to_string())),
        Inst::PushConst(Ast::ExprString(arg.to_string())),
        Inst::RemoteCallStream,
    ]
}

#[test]
fn remote_call_stream_concatenates_chunks_into_single_heap_string() {
    let disp = ChunkingDispatcher::with_chunks(vec!["alpha", "beta", "gamma"]);
    let mut vm = VM::from(&program_pushing_stream("ai", "ai.chat.stream", "hi"))
        .with_dispatcher(disp.clone());
    let final_value = vm.run();
    assert_ne!(final_value, VM_ERROR_SENTINEL);
    let s = vm.heap_string(final_value).expect("heap string");
    assert_eq!(s, "alphabetagamma");
    assert_eq!(disp.calls().len(), 1);
    assert_eq!(disp.calls()[0].0, "ai");
    assert_eq!(disp.calls()[0].1, "ai.chat.stream");
}

#[test]
fn remote_call_stream_invokes_observer_per_chunk_in_arrival_order() {
    let disp = ChunkingDispatcher::with_chunks(vec!["chunk-0", "chunk-1", "chunk-2"]);
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_cb = observed.clone();
    let observer: Arc<dyn Fn(&[u8]) + Send + Sync> = Arc::new(move |bytes: &[u8]| {
        observed_for_cb
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(bytes).into_owned());
    });
    let mut vm = VM::from(&program_pushing_stream("ai", "ai.chat.stream", "hi"))
        .with_dispatcher(disp.clone())
        .with_chunk_observer(observer);
    let final_value = vm.run();
    assert_ne!(final_value, VM_ERROR_SENTINEL);
    let collected = observed.lock().unwrap().clone();
    assert_eq!(
        collected,
        vec![
            "chunk-0".to_string(),
            "chunk-1".to_string(),
            "chunk-2".to_string()
        ]
    );
}

#[test]
fn remote_call_stream_falls_back_to_remote_call_for_default_dispatcher() {
    // Default impl of remote_call_stream calls remote_call
    // and reports the whole body as a single chunk. The
    // existing `StubDispatcher` only overrides remote_call,
    // so calling RemoteCallStream against it exercises the
    // default impl in the trait.
    let disp = StubDispatcher::ok("the-full-response");
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_cb = observed.clone();
    let observer: Arc<dyn Fn(&[u8]) + Send + Sync> = Arc::new(move |bytes: &[u8]| {
        observed_for_cb
            .lock()
            .unwrap()
            .push(String::from_utf8_lossy(bytes).into_owned());
    });
    let mut vm = VM::from(&program_pushing_stream("memory", "memory.search", "q"))
        .with_dispatcher(disp.clone())
        .with_chunk_observer(observer);
    let final_value = vm.run();
    assert_ne!(final_value, VM_ERROR_SENTINEL);
    let s = vm.heap_string(final_value).expect("heap string");
    assert_eq!(s, "the-full-response");
    // Default impl invokes on_chunk once with the full body.
    let collected = observed.lock().unwrap().clone();
    assert_eq!(collected, vec!["the-full-response".to_string()]);
}

#[test]
fn remote_call_stream_failure_halts_vm_with_sentinel() {
    let disp = StubDispatcher::err(6, "policy denied");
    let mut vm = VM::from(&program_pushing_stream("ai", "ai.chat.stream", "hi"))
        .with_dispatcher(disp.clone());
    let final_value = vm.run();
    assert_eq!(final_value, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("last_error must be set on failure");
    assert_eq!(err.kind, 6);
    assert_eq!(err.cause, "policy denied");
}
