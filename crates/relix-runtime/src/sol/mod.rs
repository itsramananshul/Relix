//! SOL — the orchestration language. Ported verbatim from OpenPrem
//! `Apps/INFRA/open-prem-main/src/sol/` so the diff against upstream stays
//! small and reviewable. Relix-specific additions (the `RemoteCall` opcode +
//! dispatcher trait) live alongside, in dedicated modules, so they can be
//! identified at a glance.
//!
//! See `docs/sol-runtime-analysis.md` for the integration strategy.
//
// Clippy is silenced at the module boundary for the verbatim port. Style
// changes are deferred to a coordinated upstream sync. Lints on *new* code
// (dispatcher.rs, anything touching RemoteCall) are NOT suppressed.
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    dead_code,
    unused_imports
)]

pub mod analyzer;
pub mod bytecode;
pub mod cli;
pub mod init;
pub mod lexer;
pub mod parser;
pub mod util;
pub mod vm;
