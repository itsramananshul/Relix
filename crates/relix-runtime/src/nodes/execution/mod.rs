//! Cross-cutting execution infrastructure — secret injection,
//! transactional action gateway, agent access broker.
//!
//! Distinct from [`crate::nodes::ai::execution`]:
//!
//! - `nodes::ai::execution` is the **per-call planner /
//!   policy / executor split** for the AI handler.
//! - This module is **execution-time runtime infrastructure**
//!   shared by every node that dispatches tool calls or
//!   serves capabilities (AI, coordinator, channels, the
//!   bridge).
//!
//! Each submodule is self-contained — they don't depend on
//! each other — so an operator can opt into the gateway
//! without enabling the access broker, or load secrets from
//! env without standing up either.

pub mod broker;
pub mod evidence;
pub mod gateway;
pub mod gateway_tier;
pub mod rollback;
pub mod secrets;
pub mod transaction_store;
