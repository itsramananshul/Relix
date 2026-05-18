//! Transport layer — RELIX-1 wire over libp2p.
//!
//! Wraps OpenPrem `network/rpc.rs` (`/rpc/1` request_response over TCP+Noise+Yamux).
//! Stubbed in M1; filled in during M5.

pub mod rpc;
