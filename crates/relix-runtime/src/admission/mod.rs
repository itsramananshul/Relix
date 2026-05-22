//! Admission-layer modules wedged between identity validation
//! and the policy engine.
//!
//! Today this hosts the **agent employee permission gate**
//! (`agent_gate`) — the categorical permission layer that
//! runs after the identity bundle is verified but before the
//! per-method PolicyEngine rules fire.
//!
//! The gate is intentionally narrow: it has read-only access
//! to an `AgentStore` snapshot and the request's
//! `RequestEnvelope`. It cannot reach the dispatch bridge's
//! handler registry or transport. Side effects (creating an
//! approval row, flipping a task to awaiting_input, writing
//! a chronicle event) are scheduled via the
//! [`agent_gate::GateSideEffects`] handle the dispatch
//! bridge provides at construction time.

pub mod agent_gate;
