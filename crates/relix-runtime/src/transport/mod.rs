//! Transport layer — RELIX-1 wire over libp2p.
//!
//! - [`rpc`] — ported libp2p `request_response` from OpenPrem; carries opaque envelopes.
//! - [`envelope`] — RELIX-1 request/response envelope shapes carried in the wire payload.

pub mod envelope;
pub mod rpc;
