//! SEC §12: read CLI secrets / tokens from stdin or a `0600`
//! file — NEVER from an argv flag value.
//!
//! A secret passed as `--value <v>` is visible to every local
//! user via `ps`, lands in shell history, and is captured by
//! journald / process accounting. This module is the single
//! choke point the credential value, the rotate value, the
//! identity verify token, and the bridge bearer all read through:
//! an explicit `--*-file <PATH>` (required `0600` on POSIX) or, if
//! no file is given, the process's stdin.

use std::io::Read;
use std::path::Path;

use zeroize::Zeroizing;

/// Read a secret from `file` when `Some`, otherwise from stdin.
/// A single trailing newline (`\n` or `\r\n`) is stripped so both
/// `printf %s secret | relix …` and an editor-saved file work.
/// An empty secret is rejected.
pub fn read_secret(file: Option<&Path>) -> Result<Zeroizing<String>, String> {
    let secret = match file {
        Some(path) => read_secret_file(path)?,
        None => read_secret_from_reader(std::io::stdin().lock())
            .map_err(|e| format!("read secret from stdin: {e}"))?,
    };
    if secret.is_empty() {
        return Err(
            "empty secret — provide a non-empty value via the --*-file flag or stdin".to_string(),
        );
    }
    Ok(secret)
}

/// Read + trim a secret from any reader. Exposed for tests and the
/// stdin path.
pub fn read_secret_from_reader<R: Read>(mut r: R) -> std::io::Result<Zeroizing<String>> {
    let mut buf = Zeroizing::new(String::new());
    r.read_to_string(&mut buf)?;
    Ok(Zeroizing::new(trim_one_newline(&buf)))
}

/// Read a secret from a file, enforcing `0600` (owner-only) on
/// POSIX so a group/other-readable secret file is refused.
pub fn read_secret_file(path: &Path) -> Result<Zeroizing<String>, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("stat secret file {}: {e}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "secret file {} has mode {mode:o}; must be 0600 (owner-only) — \
                 group/other access is refused",
                path.display()
            ));
        }
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("read secret file {}: {e}", path.display()))?;
    Ok(Zeroizing::new(trim_one_newline(&raw)))
}

/// Strip exactly one trailing line terminator (`\r\n` or `\n`) so
/// a file/echo that appends a newline does not corrupt the secret,
/// while a secret that legitimately contains internal whitespace
/// is preserved.
fn trim_one_newline(s: &str) -> String {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reader_trims_single_trailing_newline() {
        let s = read_secret_from_reader(Cursor::new(b"hunter2\n".to_vec())).unwrap();
        assert_eq!(s.as_str(), "hunter2");
        let s = read_secret_from_reader(Cursor::new(b"hunter2\r\n".to_vec())).unwrap();
        assert_eq!(s.as_str(), "hunter2");
        // No newline → unchanged.
        let s = read_secret_from_reader(Cursor::new(b"hunter2".to_vec())).unwrap();
        assert_eq!(s.as_str(), "hunter2");
        // Internal/trailing spaces preserved (only newline stripped).
        let s = read_secret_from_reader(Cursor::new(b"a b \n".to_vec())).unwrap();
        assert_eq!(s.as_str(), "a b ");
    }

    #[test]
    fn file_reads_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, b"file-secret\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let s = read_secret_file(&path).unwrap();
        assert_eq!(s.as_str(), "file-secret");
    }

    #[cfg(unix)]
    #[test]
    fn file_with_group_other_perms_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("loose.txt");
        std::fs::write(&path, b"secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_secret_file(&path).unwrap_err();
        assert!(err.contains("must be 0600"), "got: {err}");
    }
}
