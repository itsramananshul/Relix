//! # relix-runtime
//!
//! Extended OpenPrem runtime: transport (libp2p), SOL VM with `RemoteCall`,
//! dispatch bridge, capability registry, manifest exchange, and node-type
//! implementations.
//!
//! Module layout:
//! - [`transport`] — libp2p wrapper inherited from OpenPrem INFRA (RELIX-1 transport).
//! - [`sol`] — SOL VM with cross-node `remote_call` extension (RELIX-7 alpha).
//! - [`dispatch`] — inbound RPC → SOL session OR native handler (RELIX-1 §1.13).
//! - [`manifest`] — node manifest construction + on-connect exchange (RELIX-5).
//! - [`coordinator`] — per-flow event-log ownership (RELIX-3 / RELIX-8 alpha).
//! - [`nodes`] — node-type implementations (memory, ai, tool, web_bridge).

// Unsafe is denied crate-wide by default. The ONLY exception is the
// Linux plugin sandbox in `plugin::loader` (seccomp + setrlimit via a
// `pre_exec` hook), which is inherently FFI: those two functions carry a
// narrowly-scoped `#[allow(unsafe_code)]` with `// SAFETY:` notes. We use
// `deny` (overridable per-item) rather than `forbid` (un-overridable) so
// that audited island can compile on Unix while every other module stays
// unsafe-free.
#![deny(unsafe_code)]

pub mod admission;
pub mod approval;
pub mod audit_partition;
pub mod confidence;
pub mod controller_runtime;
pub mod coordinator;
pub mod credentials;
pub mod db;
pub mod dispatch;
pub mod flow_runner;
pub mod identity;
pub mod knowledge;
pub mod manifest;
pub mod metrics;
pub mod nodes;
pub mod observability;
pub mod planning;
pub mod plugin;
pub mod sflow;
pub mod sol;
pub mod training;
pub mod transport;
pub mod workflow;
pub mod yaml_flow;

pub use relix_core;
