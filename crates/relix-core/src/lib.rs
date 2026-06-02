//! # relix-core
//!
//! Shared substrate primitives for Relix. Every other crate depends on this one.
//!
//! Module layout mirrors the substrate specs:
//! - [`agent`] — AgentId, AgentRecord, AgentToken, AgentRegistry + TokenIssuer traits, HmacTokenIssuer (REL-18).
//! - [`codec`] — deterministic CBOR encoding/decoding (RELIX-4 §4.8).
//! - [`types`] — shared wire types (NodeId, RequestId, TraceId, ErrorEnvelope).
//! - [`bundle`] — Ed25519-signed CBOR bundle envelope (RELIX-4).
//! - [`identity`] — IdentityBundle, VerifiedIdentity, claim extraction (specs/identity-employees.md).
//! - [`policy`] — allowlist policy DSL (alpha simplification of RELIX-1 §1.13 step 9 / Cedar).
//! - [`eventlog`] — append-only hash-chained flow event log (RELIX-3).
//! - [`audit`] — per-responder audit record format and writer.
//! - [`capability`] — capability descriptor types (RELIX-6).
//! - [`approval`] — shared approval-delivery primitives (`ChannelKind`,
//!   `SingleChannelDispatch`, channel configs) — lives here so the
//!   channel crates can implement the trait without depending on
//!   `relix-runtime`.
//!
//! ## DETERMINISM
//!
//! Anything in [`codec`] and any signing/hashing path in this crate is required
//! to produce byte-identical output for inputs that are logically equal. Refactors
//! that touch these paths must preserve byte equality; tests in `tests/`
//! enforce this property.

// CORR-D1: relaxed from `forbid` to `deny` so the single
// `#[allow(unsafe_code)]` site on the Windows parent-dir
// fsync helper (`eventlog::fsync_parent_dir_windows`) can
// call into Win32's `CreateFileW + FlushFileBuffers +
// CloseHandle`. Every other site in this crate remains
// `deny`'d by default — only the load-bearing FFI call
// carries the allow.
#![deny(unsafe_code)]
#![warn(missing_docs)]
// Per CONTRIBUTING.md: unwrap/expect forbidden in non-test code paths. The
// cfg_attr scopes the lint so it does not fire on `#[cfg(test)]` modules.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]
#![cfg_attr(not(test), warn(clippy::expect_used))]

pub mod agent;
pub mod approval;
pub mod audit;
pub mod bundle;
pub mod capability;
pub mod channel_health;
pub mod channel_rate_limit;
pub mod clock;
pub mod codec;
pub mod eventlog;
pub mod identity;
pub mod policy;
pub mod redact;
pub mod retry;
pub mod router;
pub mod types;

/// Crate-wide result type alias for caller convenience.
pub type Result<T> = std::result::Result<T, CoreError>;

/// Crate-wide error type. Each module defines its own typed errors that
/// convert into this enum.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Codec failure (CBOR encode/decode).
    #[error("codec: {0}")]
    Codec(#[from] codec::CodecError),

    /// Signed bundle failure (signature, expiry, chain, format).
    #[error("bundle: {0}")]
    Bundle(#[from] bundle::BundleError),

    /// Identity verification failure.
    #[error("identity: {0}")]
    Identity(#[from] identity::IdentityError),

    /// Policy evaluation failure.
    #[error("policy: {0}")]
    Policy(#[from] policy::PolicyError),

    /// Event log failure (I/O, integrity, format).
    #[error("eventlog: {0}")]
    EventLog(#[from] eventlog::EventLogError),

    /// Audit failure.
    #[error("audit: {0}")]
    Audit(#[from] audit::AuditError),
}
