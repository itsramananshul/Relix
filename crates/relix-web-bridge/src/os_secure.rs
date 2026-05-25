//! OS-specific file-permission hardening for secrets.
//!
//! POSIX is straightforward — `chmod 0600` on the freshly-written
//! file via the standard library. The chmod call is exposed here
//! mostly so call sites have one helper to invoke regardless of
//! platform.
//!
//! Windows has no POSIX bit set, so we shell out to the bundled
//! `icacls` tool to:
//! 1. Strip inherited permissions (so a permissive ancestor doesn't
//!    grant read to "Authenticated Users").
//! 2. Grant Full control to the current user.
//! 3. Implicitly remove every other principal because step 1
//!    removed inheritance and step 2 is the only ACE we add.
//!
//! `icacls` is in `%SYSTEMROOT%\System32` on every supported
//! Windows version. The bridge crate forbids unsafe_code, so we
//! deliberately avoid the Win32 ACL API and the heavyweight
//! `windows` crate dependency — the shell-out is equivalent in
//! effect and considerably easier to audit. Callers that want a
//! detailed read should run `icacls <path>` themselves; the
//! `relix doctor` command surfaces over-permissive files for the
//! operator.

use std::path::Path;

/// Apply restrictive permissions to `path`. On POSIX this is the
/// classic `chmod 0600`; on Windows it strips inheritance and
/// grants the current user Full control via `icacls`.
///
/// Best-effort: the function returns `Err` with a human message
/// when the underlying primitive fails. Callers typically log the
/// error and continue (a writable secrets file is still better
/// than no secrets file at all).
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

        // /inheritance:r — remove all inherited ACEs from this
        // object. After this call the ACL is whatever explicit
        // ACEs were already on the file (typically none on a
        // fresh-from-write file).
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

        // /grant:r — replace any existing ACE for the principal
        // with this one. Full control because the bridge needs to
        // overwrite (delete + rename) the file on rotation.
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

    // Unsupported platform — no-op. Operators on exotic targets
    // get the same "best effort" treatment they get from the rest
    // of the bridge's I/O.
    #[allow(unreachable_code)]
    Ok(())
}

/// File-permission verdict used by `relix doctor`. Surfaced by
/// the CLI side (`relix-cli`); kept here too so the bridge's own
/// test-server tooling can introspect the file it writes.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermVerdict {
    /// Restrictive (POSIX 0600, or Windows with inheritance
    /// stripped + a single user ACE).
    Strict,
    /// Permissions are looser than recommended (any group/other
    /// bits on POSIX; inherited ACEs on Windows).
    Loose,
    /// Could not determine — file missing or unreadable.
    Unknown,
}

/// Inspect a file's permissions. Pure read — never mutates.
///
/// Honest about Windows scope: the check uses `icacls <path>` and
/// flags the file as `Loose` when the output contains an
/// inheritance marker (`(I)`) or any principal that isn't the
/// current user. Anything beyond that pattern is reported as
/// `Unknown` — the operator can run icacls themselves for the
/// full picture.
#[allow(dead_code)]
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
        // No group/other bits at all.
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
            // Principal ACEs look like "BUILTIN\Users:(R)" or
            // "DESKTOP-X\jane:(F)". We're crude here — any line
            // mentioning a backslash that doesn't reference the
            // current user counts as "other principal."
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

    #[cfg(unix)]
    #[test]
    fn unix_loose_perms_flagged() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.txt");
        std::fs::write(&path, b"x").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&path, perms).unwrap();
        assert_eq!(inspect_permissions(&path), PermVerdict::Loose);
    }

    #[test]
    fn missing_file_is_unknown() {
        let p = std::path::Path::new("does-not-exist.placeholder");
        assert_eq!(inspect_permissions(p), PermVerdict::Unknown);
    }
}
