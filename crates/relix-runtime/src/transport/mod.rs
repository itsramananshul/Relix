//! Transport layer — RELIX-1 wire over libp2p.
//!
//! - [`rpc`] — ported libp2p `request_response` from OpenPrem; carries opaque envelopes.
//! - [`envelope`] — RELIX-1 request/response envelope shapes carried in the wire payload.
//! - [`stream`] — RELIX-2 streaming substream protocol for
//!   capabilities that emit a sequence of frames (chat tokens,
//!   long-running tool output). Sits on the same libp2p swarm
//!   as `rpc` so a single TCP+Noise+Yamux session multiplexes
//!   unary and streaming calls.

pub mod envelope;
pub mod rpc;
pub mod stream;
