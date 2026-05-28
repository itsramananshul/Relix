//! Node-type implementations selected by controller config.
//!
//! Each module provides:
//! - A `register_capabilities(...)` function exposing the node's capabilities.
//! - Native handlers invoked by the dispatch bridge.
//!
//! Controller config decides which to enable per binary instance.

pub mod ai;
pub mod channels;
pub mod coordinator;
pub mod discord;
pub mod email;
pub mod execution;
pub mod memory;
pub mod pii_gate;
pub mod pii_gate_coordinator;
pub mod router;
pub mod slack;
pub mod telegram;
pub mod tool;
pub mod web_bridge;
