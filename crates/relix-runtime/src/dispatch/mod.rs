//! Dispatch bridge — RELIX-1 §1.13 admission pipeline.
//!
//! Implements the 11-step (alpha subset) admission ordering:
//! 1. Decode envelope
//! 2. (stub) Protocol version
//! 3. Verify deadline
//! 4. (stub) Replay-cache check
//! 5. Verify identity bundle → VerifiedIdentity
//! 6. (stub) Signed envelope
//! 7. Capability lookup
//! 8. (stub) Args validation against CDDL
//! 9. Policy evaluation
//! 10. Dispatch to handler
//! 11. Write audit record
//!
//! M1 stubs; full implementation in M5.

/// Placeholder type for the dispatch bridge.
pub struct DispatchStub;
