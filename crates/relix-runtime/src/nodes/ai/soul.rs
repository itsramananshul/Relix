//! Agent persona loader (`SOUL.md`).
//!
//! Operators describe an agent's personality, communication
//! style, knowledge domain, and constraints in a single
//! markdown file. The AI node loads the file at startup (or
//! at every call, depending on `Soul::load_mode`) and prepends
//! its content to the system prompt for every `ai.chat`
//! request the agent issues.
//!
//! ## Discovery
//!
//! The loader searches in this order:
//! 1. The explicit `[agent] soul_path = "..."` config knob, if
//!    set.
//! 2. `~/.relix/souls/<agent_name>.md`.
//! 3. `./souls/<agent_name>.md` (relative to cwd).
//!
//! The first match wins; subsequent locations are ignored so
//! an operator-set explicit path can't be silently overridden.
//!
//! ## SOUL.md format
//!
//! The loader treats the markdown as opaque text — every byte
//! of the file becomes prefix on the system prompt verbatim.
//! Operators can structure it however they like; the
//! documented convention is:
//!
//! ```markdown
//! # Agent Name
//! ## Personality
//! [description]
//! ## Communication Style
//! [description]
//! ## Knowledge Domain
//! [description]
//! ## Constraints
//! [description]
//! ```
//!
//! There's no schema validation today — the file is text the
//! AI provider sees verbatim. That's intentional: the format
//! evolves with prompt engineering best practices, not with
//! Relix releases.

use std::path::{Path, PathBuf};

/// One loaded persona. `name` is the agent slug (derived from
/// the file name); `content` is the full markdown body the AI
/// node prepends to the system prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Soul {
    pub name: String,
    pub content: String,
    /// Where the file was discovered. Surfaced by
    /// `relix souls list` so operators can see which file is
    /// active when multiple candidates exist.
    pub path: PathBuf,
}

impl Soul {
    /// Compose the AI node's effective system prompt:
    /// `<soul.content>\n\n<existing_system_prompt>`. When the
    /// caller passed no system prompt, the soul content stands
    /// alone.
    pub fn into_system_prompt(self, existing: Option<&str>) -> String {
        match existing {
            Some(s) if !s.trim().is_empty() => format!("{}\n\n{}", self.content.trim_end(), s),
            _ => self.content,
        }
    }
}

/// Discover the soul file for `agent_name`. Returns `None`
/// when no candidate path holds a readable file.
pub fn discover(agent_name: &str, explicit: Option<&Path>) -> Option<Soul> {
    for candidate in candidate_paths(agent_name, explicit) {
        if let Ok(content) = std::fs::read_to_string(&candidate)
            && !content.trim().is_empty()
        {
            return Some(Soul {
                name: agent_name.to_string(),
                content,
                path: candidate,
            });
        }
    }
    None
}

/// Enumerate the paths the discoverer probes. Public so the
/// `relix souls list` CLI can show the operator the search
/// order verbatim.
pub fn candidate_paths(agent_name: &str, explicit: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        out.push(p.to_path_buf());
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        out.push(
            PathBuf::from(home)
                .join(".relix")
                .join("souls")
                .join(format!("{agent_name}.md")),
        );
    }
    out.push(PathBuf::from("souls").join(format!("{agent_name}.md")));
    out
}

/// Walk `~/.relix/souls/` and `./souls/` and return every soul
/// file we find. Used by `relix souls list` so operators see
/// every authored persona at a glance, not just the one a
/// running agent picked. Silently skips unreadable / empty
/// files.
pub fn list_all() -> Vec<Soul> {
    let mut out: Vec<Soul> = Vec::new();
    let mut roots: Vec<PathBuf> = Vec::new();
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        roots.push(PathBuf::from(home).join(".relix").join("souls"));
    }
    roots.push(PathBuf::from("souls"));
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Some(ext) = p.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if !ext.eq_ignore_ascii_case("md") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&p) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            // De-dup by name; first hit wins (matches the
            // `discover` precedence).
            if out.iter().any(|s| s.name == name) {
                continue;
            }
            out.push(Soul {
                name,
                content,
                path: p,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn candidate_paths_includes_explicit_then_home_then_cwd() {
        let explicit = PathBuf::from("/some/custom/path.md");
        let paths = candidate_paths("alice", Some(&explicit));
        assert!(!paths.is_empty());
        // Explicit path always first.
        assert_eq!(paths[0], explicit);
        // The cwd fallback always lands somewhere later.
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("souls/alice.md") || p.ends_with("souls\\alice.md"))
        );
    }

    #[test]
    fn discover_returns_none_when_no_file_exists() {
        let nonexistent = PathBuf::from("/zzz/very/unlikely/path/missing.md");
        let s = discover("not-an-agent-xyzzy", Some(&nonexistent));
        assert!(s.is_none());
    }

    #[test]
    fn discover_loads_explicit_path_when_present() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp.as_file(), "# Alice\n## Personality\nCurious.").unwrap();
        let s = discover("alice", Some(tmp.path())).expect("must discover");
        assert_eq!(s.name, "alice");
        assert!(s.content.contains("Curious."));
        assert_eq!(s.path, tmp.path().to_path_buf());
    }

    #[test]
    fn discover_skips_empty_files() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // File exists but is whitespace-only — must be skipped.
        writeln!(tmp.as_file(), "   \n\n").unwrap();
        let s = discover("alice", Some(tmp.path()));
        assert!(s.is_none(), "empty soul file must not be returned");
    }

    #[test]
    fn into_system_prompt_prepends_soul_when_existing_present() {
        let s = Soul {
            name: "alice".into(),
            content: "# Alice\nFriendly tone.".into(),
            path: PathBuf::from("alice.md"),
        };
        let combined = s.into_system_prompt(Some("Answer concisely."));
        assert!(combined.starts_with("# Alice"));
        assert!(combined.ends_with("Answer concisely."));
        assert!(combined.contains("\n\n"));
    }

    #[test]
    fn into_system_prompt_returns_soul_alone_when_existing_empty() {
        let s = Soul {
            name: "alice".into(),
            content: "soul body".into(),
            path: PathBuf::from("alice.md"),
        };
        assert_eq!(s.clone().into_system_prompt(None), "soul body");
        assert_eq!(s.into_system_prompt(Some("   ")), "soul body");
    }
}
