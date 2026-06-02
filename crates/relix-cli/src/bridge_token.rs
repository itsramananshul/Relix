//! Shared bridge bearer-token resolution for operator CLI commands.
//!
//! Auth-gated `/v1/*` endpoints require `Authorization: Bearer <token>`.
//! The token is minted at first bridge boot and written to
//! `~/.relix/bridge-token`. Operator health checks (`doctor`,
//! `ops smoke`) hit those endpoints, so they must resolve and attach
//! the token or they report a healthy auth-enabled mesh as broken.
//!
//! Resolution precedence, highest first:
//!   1. an explicit `--token <value>` flag
//!   2. the `RELIX_BRIDGE_TOKEN` environment variable
//!   3. the `~/.relix/bridge-token` file
//!
//! When no source yields a non-empty token the resolver returns
//! `None`; the caller sends no header (auth may be disabled) and, on a
//! 401/403, surfaces [`missing_token_hint`] instead of a raw status
//! dump.

use std::path::PathBuf;

/// Where a resolved token came from. Reported to the operator so a
/// surprising token can be traced to its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    Flag,
    Env,
    File,
}

impl TokenSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "--token flag",
            Self::Env => "RELIX_BRIDGE_TOKEN env",
            Self::File => "~/.relix/bridge-token",
        }
    }
}

/// Conventional on-disk location of the bridge token. `None` only when
/// neither `USERPROFILE` (Windows) nor `HOME` (POSIX) is set.
pub fn token_file_path() -> Option<PathBuf> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home_var).map(|h| PathBuf::from(h).join(".relix").join("bridge-token"))
}

/// Resolve the bridge bearer token following the documented
/// precedence. Returns `None` when no source yields a non-empty
/// token. A present-but-empty source is skipped (treated as absent).
pub fn resolve(explicit: Option<&str>) -> Option<(String, TokenSource)> {
    resolve_core(
        explicit,
        std::env::var("RELIX_BRIDGE_TOKEN").ok(),
        token_file_path().and_then(|p| std::fs::read_to_string(p).ok()),
    )
}

/// Pure precedence logic, split out so tests exercise it without
/// mutating process-global environment / filesystem state.
fn resolve_core(
    explicit: Option<&str>,
    env: Option<String>,
    file: Option<String>,
) -> Option<(String, TokenSource)> {
    if let Some(t) = explicit {
        let t = t.trim();
        if !t.is_empty() {
            return Some((t.to_string(), TokenSource::Flag));
        }
    }
    if let Some(t) = env {
        let t = t.trim();
        if !t.is_empty() {
            return Some((t.to_string(), TokenSource::Env));
        }
    }
    if let Some(t) = file {
        let t = t.trim();
        if !t.is_empty() {
            return Some((t.to_string(), TokenSource::File));
        }
    }
    None
}

/// One-line actionable hint naming the token locations. Used in error
/// messages when an auth-gated probe returns 401/403 so the operator
/// sees what to fix instead of a raw status dump.
pub fn missing_token_hint() -> String {
    let path = token_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.relix/bridge-token".to_string());
    format!(
        "no bridge token resolved — pass --token <value>, set RELIX_BRIDGE_TOKEN, \
         or ensure the bridge wrote {path}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_flag_wins_over_env_and_file() {
        let got = resolve_core(
            Some("flag-tok"),
            Some("env-tok".into()),
            Some("file-tok".into()),
        );
        assert_eq!(got, Some(("flag-tok".into(), TokenSource::Flag)));
    }

    #[test]
    fn env_wins_over_file_when_no_flag() {
        let got = resolve_core(None, Some("env-tok".into()), Some("file-tok".into()));
        assert_eq!(got, Some(("env-tok".into(), TokenSource::Env)));
    }

    #[test]
    fn file_used_when_flag_and_env_absent() {
        let got = resolve_core(None, None, Some("file-tok\n".into()));
        assert_eq!(got, Some(("file-tok".into(), TokenSource::File)));
    }

    #[test]
    fn blank_sources_are_skipped_to_next_precedence() {
        // An empty flag / whitespace env must not shadow a real file
        // token — a present-but-empty source counts as absent.
        let got = resolve_core(Some("   "), Some("  ".into()), Some("file-tok".into()));
        assert_eq!(got, Some(("file-tok".into(), TokenSource::File)));
    }

    #[test]
    fn none_when_every_source_empty() {
        assert_eq!(resolve_core(None, None, None), None);
        assert_eq!(
            resolve_core(Some(""), Some("".into()), Some("\n".into())),
            None
        );
    }
}
