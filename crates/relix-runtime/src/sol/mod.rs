//! SOL — ported from OpenPrem `src/sol/`, extended with `RemoteCall` opcode (M6).
//!
//! M1 ships an empty namespace so the workspace compiles. M6 imports the
//! OpenPrem SOL modules verbatim and adds the cross-node call site.

/// Placeholder for the SOL bytecode VM. M6 lands the real port + RemoteCall extension.
pub struct VmStub;
