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
