//! CLI-wide defaults.
//!
//! Every CLI command that talks to the local bridge over HTTP reads
//! its default URL / port from here. Hard-coding the literal port in
//! a clap `default_value` attribute drifts: the user's prompt found
//! 118 sites carrying the same string across 34 files, including one
//! stale `9100` that nothing else used. One module owning these
//! constants means a port bump becomes a one-line edit and `relix
//! foo --bridge` always defaults to the same place.
//!
//! Use:
//! ```ignore
//! use crate::defaults::{DEFAULT_BRIDGE_URL, DEFAULT_BRIDGE_PORT};
//!
//! #[arg(long, default_value = DEFAULT_BRIDGE_URL)]
//! bridge: String,
//! ```

/// Default bridge HTTP base URL. The bridge listens on loopback by
/// default; the CLI follows the same posture. Operators with a
/// reverse proxy in front override via `--bridge <url>`.
pub const DEFAULT_BRIDGE_URL: &str = "http://127.0.0.1:19791";

/// Default bridge TCP port. Used by clap defaults that take a
/// `u16` instead of a full URL (e.g. `relix mesh up --bridge-port`).
pub const DEFAULT_BRIDGE_PORT: u16 = 19791;
