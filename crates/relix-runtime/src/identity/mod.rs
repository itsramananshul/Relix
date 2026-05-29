//! RELIX-7.30 PART 3 — per-session JWT-style identity tokens.
//!
//! A SQLite-backed token vault that issues lightweight signed
//! identity tokens for every agent session. Tokens are NOT
//! full JWTs — they are CBOR-encoded structs signed with
//! HMAC-SHA256 using a key sourced from
//! `[identity.session] signing_key_env`. The CBOR + HMAC
//! choice keeps the wire size small + the verify path
//! constant-time and dependency-light.
//!
//! Surfaces:
//!
//! - [`session::SessionIdentityService`] — issue / verify /
//!   revoke / list. Cheap to clone (couple of Arcs).
//! - [`caps::register`] — wires the four
//!   `identity.*` caps onto a `DispatchBridge`.
//!
//! Existing `relix_core::identity::IdentityBundle` covers the
//! org-level Identity bundle issuance — this module sits ON
//! TOP for per-session capability scoping.

pub mod caps;
pub mod research;
pub mod research_caps;
pub mod session;

pub use session::{
    IssueRequest, SessionIdentityConfig, SessionIdentityService, SessionToken, TokenError,
    TokenStore, TokenSummary, TokenVerification,
};
