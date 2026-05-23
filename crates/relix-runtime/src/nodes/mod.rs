//! Node-type implementations selected by controller config.
//!
//! Each module provides:
//! - A `register_capabilities(...)` function exposing the node's capabilities.
//! - Native handlers invoked by the dispatch bridge.
//!
//! Controller config decides which to enable per binary instance.

pub mod ai;
pub mod coordinator;
pub mod discord;
pub mod memory;
pub mod router;
pub mod telegram;
pub mod tool;
pub mod web_bridge;
