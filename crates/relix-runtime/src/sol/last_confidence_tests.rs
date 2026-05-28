//! RELIX-7.19 — `last_confidence()` SOL builtin tests.
//!
//! Exercises the parser → analyzer → codegen → VM path for
//! the zero-arg `last_confidence()` builtin alongside the
//! host-side setter (`set_last_confidence`) and the shared
//! cell (`LastConfidenceCell`).

use std::sync::Arc;
use std::sync::Mutex;

use crate::confidence::LastConfidenceCell;
use crate::sol::bytecode::Inst;
use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallResult};
use crate::sol::parser::Ast;
use crate::sol::vm::{VM, VM_ERROR_SENTINEL};

/// Dispatcher that responds Ok and lets the test fire a
/// confidence value into the VM cell before the next call.
struct CellDispatcher {
    cell: LastConfidenceCell,
    responses: Mutex<Vec<(f32, Vec<u8>)>>,
}

impl CellDispatcher {
    fn new(cell: LastConfidenceCell, responses: Vec<(f32, Vec<u8>)>) -> Arc<Self> {
        Arc::new(Self {
            cell,
            responses: Mutex::new(responses),
        })
    }
}

impl RemoteCallDispatcher for CellDispatcher {
    fn remote_call(&self, _peer: &str, _method: &str, _arg: &[u8]) -> RemoteCallResult {
        let mut g = self.responses.lock().unwrap();
        if g.is_empty() {
            return Ok(Vec::new());
        }
        let (conf, body) = g.remove(0);
        self.cell.set(conf);
        Ok(body)
    }
}

fn remote_call_program() -> Vec<Inst> {
    vec![
        Inst::PushConst(Ast::ExprString("peer".into())),
        Inst::PushConst(Ast::ExprString("method".into())),
        Inst::PushConst(Ast::ExprString("arg".into())),
        Inst::RemoteCall,
        Inst::Pop, // discard the response ref; we only care about side effects
        Inst::LoadLastConfidence,
    ]
}

#[test]
fn last_confidence_returns_one_dot_zero_before_any_remote_call() {
    let program = vec![Inst::LoadLastConfidence];
    let mut vm = VM::from(&program);
    let bits = vm.run();
    assert_ne!(bits, VM_ERROR_SENTINEL);
    let v = f64::from_bits(bits) as f32;
    assert!((v - 1.0).abs() < 1e-6, "got {v}, want 1.0");
}

#[test]
fn last_confidence_reads_the_value_set_via_set_last_confidence() {
    let program = vec![Inst::LoadLastConfidence];
    let mut vm = VM::from(&program);
    vm.set_last_confidence(0.42);
    let bits = vm.run();
    let v = f64::from_bits(bits) as f32;
    assert!((v - 0.42).abs() < 1e-6, "got {v}");
}

#[test]
fn last_confidence_reads_the_value_after_each_remote_call_in_a_sequence() {
    // Program: remote_call(); _ = last_confidence(); remote_call(); last_confidence();
    let program = vec![
        // First call.
        Inst::PushConst(Ast::ExprString("p".into())),
        Inst::PushConst(Ast::ExprString("m".into())),
        Inst::PushConst(Ast::ExprString("a".into())),
        Inst::RemoteCall,
        Inst::Pop,
        // Second call.
        Inst::PushConst(Ast::ExprString("p".into())),
        Inst::PushConst(Ast::ExprString("m".into())),
        Inst::PushConst(Ast::ExprString("b".into())),
        Inst::RemoteCall,
        Inst::Pop,
        // Final read.
        Inst::LoadLastConfidence,
    ];
    let cell = LastConfidenceCell::new();
    let disp = CellDispatcher::new(
        cell.clone(),
        vec![(0.30, b"first".to_vec()), (0.75, b"second".to_vec())],
    );
    let mut vm = VM::from(&program).with_dispatcher(disp.clone());
    vm.set_last_confidence_cell(cell);
    let bits = vm.run();
    let v = f64::from_bits(bits) as f32;
    assert!(
        (v - 0.75).abs() < 1e-6,
        "got {v}, expected the second call's confidence"
    );
}

#[test]
fn shared_cell_drives_load_last_confidence_when_attached() {
    // The host updates the shared cell; the VM reads it.
    let program = vec![Inst::LoadLastConfidence];
    let cell = LastConfidenceCell::with_initial(0.5);
    let mut vm = VM::from(&program);
    vm.set_last_confidence_cell(cell.clone());
    let bits = vm.run();
    let v = f64::from_bits(bits) as f32;
    assert!((v - 0.5).abs() < 1e-6);
    // Mutate the cell mid-flight; the next program run sees it.
    cell.set(0.91);
    let mut vm = VM::from(&program);
    vm.set_last_confidence_cell(cell);
    let bits = vm.run();
    let v = f64::from_bits(bits) as f32;
    assert!((v - 0.91).abs() < 1e-6, "got {v}");
}

#[test]
fn dispatcher_writing_to_shared_cell_is_visible_to_subsequent_last_confidence() {
    let cell = LastConfidenceCell::new();
    let disp = CellDispatcher::new(
        cell.clone(),
        vec![(
            0.20,
            b"some body that's long enough to not look empty".to_vec(),
        )],
    );
    let program = remote_call_program();
    let mut vm = VM::from(&program).with_dispatcher(disp.clone());
    vm.set_last_confidence_cell(cell);
    let bits = vm.run();
    let v = f64::from_bits(bits) as f32;
    assert!((v - 0.20).abs() < 1e-6, "got {v}");
}

#[test]
fn source_compiles_last_confidence_to_load_opcode() {
    let src = "function start() -> float { return last_confidence(); }\n";
    let program = crate::sol::compile_source(src).expect("compile");
    assert!(
        program
            .iter()
            .any(|i| matches!(i, Inst::LoadLastConfidence)),
        "expected LoadLastConfidence in: {program:?}"
    );
}
