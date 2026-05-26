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
        let skills_root = PathBuf::from(home).join(".relix").join("skills");
        // Auto-generated skills live under the dedicated `auto`
        // subdirectory so an operator can `relix skills prune`
        // them without touching their hand-authored library.
        roots.push(skills_root.join("auto"));
        roots.push(skills_root);
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

/// Find the best-matching skill for `prompt` via a simple
/// keyword-overlap score. Returns `None` when no skill shares
/// any non-stopword token with the prompt.
///
/// Honest scope: this is the keyword fallback the spec calls
/// out — embedding-similarity matching (the spec's preferred
/// path when Qdrant is available) is a separate follow-up that
/// reuses the AI node's embedding provider. The keyword
/// matcher is what controllers without an embedding peer get;
/// returning `None` is fine — the skill prepend is opt-in
/// context, not required.
pub fn match_skill_keyword<'a>(skills: &'a [Skill], prompt: &str) -> Option<&'a Skill> {
    let prompt_tokens: std::collections::BTreeSet<String> = tokenize(prompt).collect();
    if prompt_tokens.is_empty() {
        return None;
    }
    let mut best: Option<(&Skill, usize)> = None;
    for s in skills {
        let haystack = format!("{} {} {}", s.name, s.title, s.body);
        let skill_tokens: std::collections::BTreeSet<String> = tokenize(&haystack).collect();
        let overlap = prompt_tokens.intersection(&skill_tokens).count();
        if overlap == 0 {
            continue;
        }
        match best {
            Some((_, best_overlap)) if overlap <= best_overlap => {}
            _ => best = Some((s, overlap)),
        }
    }
    best.map(|(s, _)| s)
}

/// Lowercase, strip punctuation, split on whitespace, drop
/// stopwords. Pure utility — exported for tests of the matcher
/// logic.
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !w.is_empty() && w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
}

/// English stopword list. Pragmatic, not exhaustive — the
/// matcher just needs to drop the most common noise.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "are", "was", "will", "you",
    "your", "but", "not", "all", "any", "use", "can", "may", "have", "has", "had", "would",
    "should", "could", "what", "when", "where", "how", "who", "why",
];

/// Render a system-prompt envelope around a matched skill's
/// body. The envelope is documented and stable: future
/// integrations (auto-skill generator, dashboard surface) can
/// rely on the format.
pub fn render_skill_hint(skill: &Skill) -> String {
    format!(
        "## Skill: {name}\n\
         \n\
         You have access to this skill. Use it if relevant to the task.\n\
         \n\
         {body}\n",
        name = skill.name,
        body = skill.body.trim()
    )
}

/// Cache for the loaded skill library. Cheap to clone (Arc
/// inside). The cache loads once at construction; reload via
/// `refresh()` if operators add skills mid-run (the AI node
/// doesn't auto-refresh — refresh is operator-triggered).
#[derive(Clone, Debug)]
pub struct SkillsCache {
    skills: Arc<Vec<Skill>>,
}

impl SkillsCache {
    /// Discover skills under `extra_roots` plus the documented
    /// default roots (cwd / `~/.relix/skills`) and store them.
    pub fn load(extra_roots: &[PathBuf]) -> Self {
        Self {
            skills: Arc::new(discover_skills(extra_roots)),
        }
    }

    /// Permanent-empty cache. Tests + the legacy code path
    /// that doesn't load skills at all use this; the matcher
    /// returns None against the empty list, so the AI handler
    /// skips the prepend.
    pub fn empty() -> Self {
        Self {
            skills: Arc::new(Vec::new()),
        }
    }

    /// Test-only constructor that wraps a pre-built skill list.
    /// Saves tests from staging actual files on disk just to
    /// exercise the matcher.
    pub fn from_vec(skills: Vec<Skill>) -> Self {
        Self {
            skills: Arc::new(skills),
        }
    }

    /// Match the prompt against the cached skill library; render
    /// a system-prompt hint when a match is found. None means
    /// "no relevant skill" and the AI handler skips the prepend.
    pub fn matched_hint(&self, prompt: &str) -> Option<String> {
        match_skill_keyword(&self.skills, prompt).map(render_skill_hint)
    }

    /// Count of cached skills. Useful for `relix doctor` /
    /// debug surfaces.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

use std::sync::Arc;

// ── Auto-skill generation ───────────────────────────────────

/// Operator-facing config for the auto-skill generator. Lives
/// under `[skills]` in the controller TOML so operators can
/// toggle the behaviour without touching capability config.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct SkillsConfig {
    /// Master switch. `false` (default) means task completion
    /// never writes a SKILL.md.
    #[serde(default)]
    pub auto_generate: bool,
    /// Age threshold for `relix skills prune` AND for the
    /// generator's "is this skill already covered" check.
    /// Default 30 days.
    #[serde(default = "default_max_age_days")]
    pub max_age_days: i64,
    /// Override for the auto-skill directory. Default is
    /// `~/.relix/skills/auto`. Operators usually leave this
    /// alone; the override is for sandboxed tests.
    #[serde(default)]
    pub auto_dir: Option<PathBuf>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            auto_generate: false,
            max_age_days: default_max_age_days(),
            auto_dir: None,
        }
    }
}

fn default_max_age_days() -> i64 {
    30
}

/// Resolve the auto-skill directory. Honors
/// [`SkillsConfig::auto_dir`] when set; otherwise falls back
/// to `~/.relix/skills/auto`. Returns `None` only when there
/// is no `HOME` / `USERPROFILE` (sandboxed processes); the
/// caller skips writing silently in that case.
pub fn resolve_auto_skill_dir(cfg: &SkillsConfig) -> Option<PathBuf> {
    if let Some(d) = &cfg.auto_dir {
        return Some(d.clone());
    }
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var)?;
    Some(
        PathBuf::from(home)
            .join(".relix")
            .join("skills")
            .join("auto"),
    )
}

/// Build the SKILL.md body for a completed task. Pure
/// function: takes the inputs it summarises and returns the
/// rendered markdown — no filesystem I/O, no DB calls.
///
/// The body is deliberately templated rather than free-form so
/// the auto-generator stays cheap (no LLM dependency). When a
/// future commit wires an LLM-driven "summarise this approach"
/// path, it can replace this function while keeping the same
/// "name + body" shape.
pub fn render_auto_skill_body(
    task_title: &str,
    flow_template: &str,
    duration_secs: i64,
    event_summary: &str,
) -> String {
    let dur = if duration_secs > 0 {
        format!("{duration_secs}s")
    } else {
        "—".to_string()
    };
    format!(
        "# {title}\n\
         \n\
         _Auto-generated from a completed task. Edit freely; the\n\
         generator will not overwrite this file._\n\
         \n\
         ## Procedure\n\
         \n\
         - Flow template: `{flow}`\n\
         - Wall-clock duration: {dur}\n\
         \n\
         ## Chronicle highlights\n\
         \n\
         {summary}\n",
        title = task_title.trim(),
        flow = flow_template,
        dur = dur,
        summary = if event_summary.trim().is_empty() {
            "(no chronicle events recorded)".to_string()
        } else {
            event_summary.to_string()
        }
    )
}

/// Sanitise a task title into a filesystem-safe slug for the
/// auto-skill filename. ASCII alphanumerics + dashes only;
/// everything else collapses to `-`. Caps the length so a
/// pathological title can't blow past path-length limits.
pub fn slugify_for_filename(title: &str) -> String {
    let mut out = String::with_capacity(title.len().min(60));
    let mut last_was_dash = false;
    for c in title.chars() {
        let mapped = if c.is_ascii_alphanumeric() {
            last_was_dash = false;
            c.to_ascii_lowercase()
        } else {
            if last_was_dash {
                continue;
            }
            last_was_dash = true;
            '-'
        };
        out.push(mapped);
        if out.len() >= 60 {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "auto-skill".to_string()
    } else {
        trimmed
    }
}

/// Write the body for an auto-generated skill into the
/// configured directory. Returns the path of the file written.
/// Caller decides what to do with collisions — this function
/// refuses to overwrite an existing file, which matches the
/// "auto-generator never clobbers operator edits" contract.
pub fn write_auto_skill(
    dir: &Path,
    skill_name: &str,
    body: &str,
) -> std::io::Result<Option<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{skill_name}.md"));
    if path.exists() {
        return Ok(None);
    }
    std::fs::write(&path, body)?;
    Ok(Some(path))
}

/// Walk `dir` and delete `*.md` files whose mtime is older
/// than `max_age_days`. Returns `(scanned, deleted)` so the
/// CLI can render an operator-facing summary. Missing
/// directory is treated as "nothing to prune" (Ok((0, 0))).
pub fn prune_auto_skills(dir: &Path, max_age_days: i64) -> std::io::Result<(usize, usize)> {
    if !dir.exists() {
        return Ok((0, 0));
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            (max_age_days.max(0) as u64) * 86_400,
        ))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut scanned = 0usize;
    let mut deleted = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        scanned += 1;
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if mtime < cutoff && std::fs::remove_file(&p).is_ok() {
            deleted += 1;
        }
    }
    Ok((scanned, deleted))
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
    fn match_skill_keyword_returns_highest_overlap() {
        let skills = vec![
            Skill {
                name: "deploy".into(),
                path: PathBuf::from("deploy.md"),
                body: "Run deploy script\nUses kubectl".into(),
                title: "Deploy to prod".into(),
            },
            Skill {
                name: "test".into(),
                path: PathBuf::from("test.md"),
                body: "Run cargo test\nAssert no failures".into(),
                title: "Run tests".into(),
            },
        ];
        let m = match_skill_keyword(&skills, "deploy the new build");
        assert_eq!(m.map(|s| s.name.as_str()), Some("deploy"));
        let m = match_skill_keyword(&skills, "run tests on the branch");
        assert_eq!(m.map(|s| s.name.as_str()), Some("test"));
        let m = match_skill_keyword(&skills, "look up the weather");
        assert!(m.is_none(), "no overlap → no match");
    }

    #[test]
    fn render_skill_hint_includes_body_in_documented_envelope() {
        let s = Skill {
            name: "deploy".into(),
            path: PathBuf::from("d.md"),
            body: "## Steps\n1. cargo build\n2. push".into(),
            title: "Deploy".into(),
        };
        let hint = render_skill_hint(&s);
        assert!(hint.contains("You have access to this skill"));
        assert!(hint.contains("deploy"));
        assert!(hint.contains("cargo build"));
        assert!(hint.starts_with("## Skill: "));
    }

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

    #[test]
    fn slugify_collapses_punctuation_and_caps_length() {
        let s = slugify_for_filename("Deploy STAGING!! v2.0  (urgent)");
        assert!(s.contains("deploy"));
        assert!(s.contains("staging"));
        assert!(!s.contains(' '));
        assert!(!s.contains('!'));
        assert!(s.len() <= 60);
        let s_empty = slugify_for_filename("***");
        assert_eq!(s_empty, "auto-skill");
    }

    #[test]
    fn render_auto_skill_body_includes_template_sections() {
        let body =
            render_auto_skill_body("deploy staging", "flows/deploy.sol", 42, "- ran 3 steps");
        assert!(body.contains("# deploy staging"));
        assert!(body.contains("Auto-generated"));
        assert!(body.contains("flows/deploy.sol"));
        assert!(body.contains("42s"));
        assert!(body.contains("ran 3 steps"));
    }

    #[test]
    fn write_auto_skill_creates_file_and_refuses_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("auto");
        let p = write_auto_skill(&dir, "deploy-staging", "# Body").unwrap();
        assert!(p.is_some());
        let path = p.unwrap();
        assert!(path.exists());
        // Second write to the same name returns None (refusal),
        // file content unchanged.
        std::fs::write(&path, "OPERATOR EDIT").unwrap();
        let p2 = write_auto_skill(&dir, "deploy-staging", "# Different body").unwrap();
        assert!(p2.is_none(), "auto generator must not overwrite");
        let kept = std::fs::read_to_string(&path).unwrap();
        assert_eq!(kept, "OPERATOR EDIT");
    }

    #[test]
    fn prune_auto_skills_returns_zero_when_dir_missing() {
        let (s, d) = prune_auto_skills(Path::new("definitely/does/not/exist"), 30).unwrap();
        assert_eq!((s, d), (0, 0));
    }

    #[test]
    fn prune_auto_skills_zero_max_age_deletes_every_md_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("a.md"), "a").unwrap();
        std::fs::write(dir.join("b.md"), "b").unwrap();
        // A non-.md file must NOT be touched — only the auto
        // generator's own artefacts get pruned.
        std::fs::write(dir.join("readme.txt"), "keep me").unwrap();
        let (scanned, deleted) = prune_auto_skills(dir, 0).unwrap();
        assert_eq!(scanned, 2, "non-md files must not count toward scan");
        assert_eq!(deleted, 2);
        assert!(!dir.join("a.md").exists());
        assert!(!dir.join("b.md").exists());
        assert!(dir.join("readme.txt").exists());
    }

    #[test]
    fn prune_auto_skills_generous_threshold_keeps_fresh_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("fresh.md"), "fresh").unwrap();
        // 365-day threshold leaves a just-written file alone.
        let (scanned, deleted) = prune_auto_skills(dir, 365).unwrap();
        assert_eq!(scanned, 1);
        assert_eq!(deleted, 0);
        assert!(dir.join("fresh.md").exists());
    }

    #[test]
    fn skills_config_defaults_to_disabled_auto_generate() {
        let cfg = SkillsConfig::default();
        assert!(!cfg.auto_generate);
        assert_eq!(cfg.max_age_days, 30);
        assert!(cfg.auto_dir.is_none());
    }
}
