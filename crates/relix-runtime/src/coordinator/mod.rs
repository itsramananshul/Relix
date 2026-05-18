//! Flow coordinator (RELIX-3, RELIX-8).
//!
//! Owns per-flow event logs; records `RemoteCallIssued` before outbound RPCs
//! and `RemoteCallCompleted` after responses, preserving the log-before-act
//! invariant. M1 stub; M6 fills.

/// Placeholder for the per-controller coordinator.
pub struct CoordinatorStub;
