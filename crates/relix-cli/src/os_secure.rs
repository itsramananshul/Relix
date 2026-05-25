//! OS-specific file-permission hardening — mirror of the bridge's
//! `os_secure` module. Kept in relix-cli too so the wizard +
//! `relix doctor` can lock down `~/.relix/config.toml` and inspect
//! both that file and the bridge-owned ones without dragging the
//! bridge crate as a dependency.
//!
//! POSIX is `chmod 0600`. Windows shells out to the bundled
//! `icacls` to strip inherited ACEs and grant Full control only to
//! the current user. See `docs/security.md` for the threat model.

use std::path::Path;

/// Apply restrictive permissions to `path`. Best-effort — on
/// failure returns the human message so callers can surface it
/// without aborting.
pub fn restrict_to_current_user(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)
            .map_err(|e| format!("stat {}: {e}", path.display()))?
            .permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
        return Ok(());
    }

    #[cfg(windows)]
    {
        use std::process::Command;
        let user = std::env::var("USERNAME").map_err(|_| "USERNAME env var not set".to_string())?;
        let out = Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .output()
            .map_err(|e| format!("icacls /inheritance:r {}: {e}", path.display()))?;
        if !out.status.success() {
            return Err(format!(
                "icacls /inheritance:r failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let out = Command::new("icacls")
            .arg(path)
            .arg("/grant:r")
            .arg(format!("{user}:F"))
            .output()
            .map_err(|e| format!("icacls /grant {}: {e}", path.display()))?;
        if !out.status.success() {
            return Err(format!(
                "icacls /grant failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        return Ok(());
    }

    #[allow(unreachable_code)]
    Ok(())
}

/// Verdict for `relix doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermVerdict {
    /// Restrictive (POSIX 0600, or Windows: no inheritance + a
    /// single user ACE).
    Strict,
    /// Looser than recommended — operator should re-harden.
    Loose,
    /// File missing or unreadable.
    Unknown,
}

impl PermVerdict {
    #[allow(dead_code)]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Strict => "PASS",
            Self::Loose => "WARN",
            Self::Unknown => "INFO",
        }
    }
}

/// Inspect a file's permissions. Pure read — never mutates.
pub fn inspect_permissions(path: &Path) -> PermVerdict {
    if !path.exists() {
        return PermVerdict::Unknown;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(path) else {
            return PermVerdict::Unknown;
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 == 0 {
            return PermVerdict::Strict;
        }
        return PermVerdict::Loose;
    }

    #[cfg(windows)]
    {
        use std::process::Command;
        let Ok(out) = Command::new("icacls").arg(path).output() else {
            return PermVerdict::Unknown;
        };
        if !out.status.success() {
            return PermVerdict::Unknown;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let user = std::env::var("USERNAME").unwrap_or_default();
        let user_lc = user.to_ascii_lowercase();

        let mut saw_inherit = false;
        let mut saw_other_principal = false;
        for line in text.lines() {
            let line_trim = line.trim();
            if line_trim.is_empty() {
                continue;
            }
            if line_trim.contains("(I)") {
                saw_inherit = true;
            }
            if let Some(princ_end) = line_trim.find(':') {
                let principal = &line_trim[..princ_end];
                if principal.contains('\\') {
                    let lc = principal.to_ascii_lowercase();
                    if !user_lc.is_empty() && !lc.contains(&user_lc) {
                        saw_other_principal = true;
                    }
                }
            }
        }
        if saw_inherit || saw_other_principal {
            return PermVerdict::Loose;
        }
        return PermVerdict::Strict;
    }

    #[allow(unreachable_code)]
    PermVerdict::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_unknown() {
        let p = std::path::Path::new("does-not-exist.placeholder");
        assert_eq!(inspect_permissions(p), PermVerdict::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn unix_chmod_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.txt");
        std::fs::write(&path, b"x").unwrap();
        restrict_to_current_user(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(inspect_permissions(&path), PermVerdict::Strict);
    }
}
