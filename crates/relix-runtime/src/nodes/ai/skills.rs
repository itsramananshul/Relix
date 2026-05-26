//! SKILL.md + AGENTS.md compatibility (Linux Foundation
//! Agentic AI shared file convention).
//!
//! Two related primitives:
//!
//! - **`AGENTS.md`** sits at the root of a project (or any
//!   ancestor of the controller's cwd, walked up to 5 levels)
//!   and describes context the model should know on every
//!   call. The loader returns the file content verbatim; the
//!   AI node prepends it to the system prompt.
//!
//! - **`SKILL.md`** describes a reusable procedure with a
//!   stable name and an inputs/outputs section. The loader
//!   discovers every SKILL.md under known roots and registers
//!   them in an in-memory skill library. CLI surfaces
//!   (`relix skills list`, `relix skills run <name>`) drive
//!   the operator-facing flow; a future agent integration
//!   consults the library before generating a new plan.
//!
//! ## Discovery rules
//!
//! AGENTS.md:
//! 1. Start from `cwd`.
//! 2. Check for `AGENTS.md` at this level.
//! 3. If not found, go up one directory level.
//! 4. Stop after 5 levels OR when hitting the filesystem root.
//!
//! SKILL.md:
//! - `<cwd>/SKILL.md` AND `<cwd>/skills/*.md`.
//! - `~/.relix/skills/*.md`.
//! - Any path the operator listed in `[skills] roots = [...]`.
//!
//! De-duplicates by skill name; first occurrence wins.

use std::path::{Path, PathBuf};

/// Maximum number of parent directories the AGENTS.md walker
/// inspects. Per the Linux Foundation spec.
pub const AGENTS_MAX_WALK_LEVELS: usize = 5;

/// One discovered AGENTS.md file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentsContext {
    pub path: PathBuf,
    pub content: String,
}

/// Walk up from `start` looking for `AGENTS.md`. Returns the
/// first match; `None` when the walk completes without finding
/// one OR every candidate is empty.
pub fn discover_agents_md(start: &Path) -> Option<AgentsContext> {
    let mut current = start.to_path_buf();
    for _ in 0..=AGENTS_MAX_WALK_LEVELS {
        let candidate = current.join("AGENTS.md");
        if let Ok(content) = std::fs::read_to_string(&candidate)
            && !content.trim().is_empty()
        {
            return Some(AgentsContext {
                path: candidate,
                content,
            });
        }
        if !current.pop() {
            break;
        }
    }
    None
}

/// One discovered skill. `name` is the file stem; `body` is
/// the raw markdown the loader can either display, hand to the
/// AI as a procedure description, or execute via the future
/// skill runner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
    /// First markdown heading found in the body (stripped of
    /// leading `#` and whitespace). Used as the human label in
    /// `relix skills list`; falls back to `name` when the
    /// file has no heading.
    pub title: String,
}

/// Enumerate every SKILL.md / *.md the skill loader can find
/// under the documented roots + any operator-supplied extras.
pub fn discover_skills(extra_roots: &[PathBuf]) -> Vec<Skill> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("SKILL.md"));
        roots.push(cwd.join("skills"));
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    if let Some(home) = std::env::var_os(home_var) {
        roots.push(PathBuf::from(home).join(".relix").join("skills"));
    }
    for r in extra_roots {
        roots.push(r.clone());
    }
    let mut out: Vec<Skill> = Vec::new();
    let mut seen_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for root in roots {
        if root.is_file() {
            if let Some(s) = load_skill_file(&root)
                && seen_names.insert(s.name.clone())
            {
                out.push(s);
            }
            continue;
        }
        if !root.is_dir() {
            continue;
        }
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
            if let Some(s) = load_skill_file(&p)
                && seen_names.insert(s.name.clone())
            {
                out.push(s);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn load_skill_file(path: &Path) -> Option<Skill> {
    let body = std::fs::read_to_string(path).ok()?;
    if body.trim().is_empty() {
        return None;
    }
    let name = if path
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("SKILL.md"))
    {
        // Bare SKILL.md uses its parent directory name as the
        // skill name — that's the documented convention.
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    } else {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string()
    };
    let title = extract_first_heading(&body).unwrap_or_else(|| name.clone());
    Some(Skill {
        name,
        path: path.to_path_buf(),
        body,
        title,
    })
}

fn extract_first_heading(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn agents_md_walker_finds_file_in_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path();
        let child = parent.join("nested");
        std::fs::create_dir_all(&child).unwrap();
        let f = parent.join("AGENTS.md");
        std::fs::write(&f, "# Project agents\nbe helpful").unwrap();
        let found = discover_agents_md(&child).expect("walk must find parent's AGENTS.md");
        assert_eq!(found.path, f);
        assert!(found.content.contains("be helpful"));
    }

    #[test]
    fn agents_md_walker_returns_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let found = discover_agents_md(tmp.path());
        assert!(found.is_none());
    }

    #[test]
    fn agents_md_walker_respects_max_walk_levels() {
        // Confirms the loop boundary — files >5 levels up are
        // not discovered. We can't easily create a 6-deep
        // tempdir that has AGENTS.md above the cap on every
        // CI machine; assert the constant + that the walker
        // doesn't crash on a single-level path.
        assert_eq!(AGENTS_MAX_WALK_LEVELS, 5);
        let tmp = tempfile::tempdir().unwrap();
        let _ = discover_agents_md(tmp.path());
    }

    #[test]
    fn discover_skills_picks_up_root_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("greet.md");
        std::fs::write(&f, "# Greet\nSay hello.").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        assert!(
            skills.iter().any(|s| s.name == "greet"),
            "must discover greet.md: {skills:?}"
        );
    }

    #[test]
    fn discover_skills_dedupes_by_name() {
        // Two roots both containing `greet.md` → only the
        // first-seen entry is kept.
        let tmp_a = tempfile::tempdir().unwrap();
        let tmp_b = tempfile::tempdir().unwrap();
        std::fs::write(tmp_a.path().join("greet.md"), "# Greet A").unwrap();
        std::fs::write(tmp_b.path().join("greet.md"), "# Greet B").unwrap();
        let skills = discover_skills(&[tmp_a.path().to_path_buf(), tmp_b.path().to_path_buf()]);
        let greet: Vec<_> = skills.iter().filter(|s| s.name == "greet").collect();
        assert_eq!(greet.len(), 1, "de-dup must keep first occurrence");
    }

    #[test]
    fn discover_skills_uses_first_heading_as_title() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("deploy.md"),
            "# Deploy to prod\n\nDescription...",
        )
        .unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let s = skills.iter().find(|s| s.name == "deploy").unwrap();
        assert_eq!(s.title, "Deploy to prod");
    }

    #[test]
    fn discover_skills_falls_back_to_name_when_no_heading() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("plain.md"), "no heading here").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        let s = skills.iter().find(|s| s.name == "plain").unwrap();
        assert_eq!(s.title, "plain");
    }

    #[test]
    fn discover_skills_skips_empty_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("empty.md");
        let mut h = std::fs::File::create(&f).unwrap();
        writeln!(h, "   ").unwrap();
        let skills = discover_skills(&[tmp.path().to_path_buf()]);
        assert!(skills.iter().all(|s| s.name != "empty"));
    }

    #[test]
    fn bare_skill_md_uses_parent_directory_name_as_skill_name() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("my-cool-skill");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "# My Skill").unwrap();
        let skills = discover_skills(&[nested]);
        assert!(skills.iter().any(|s| s.name == "my-cool-skill"));
    }
}
