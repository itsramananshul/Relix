//! `relix skills list` and `relix skills run <name>`.
//!
//! Thin CLI front-end over
//! `relix_runtime::nodes::ai::skills`. The runtime owns the
//! discovery logic so the bridge, the CLI, and any future SDK
//! all agree on where SKILL.md files live.

use std::path::PathBuf;

use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List discovered SKILL.md files + their inferred titles.
    /// Walks the documented roots: cwd/SKILL.md, cwd/skills/,
    /// ~/.relix/skills/, plus any `--root` entries.
    List(ListArgs),
    /// Print the body of the named skill to stdout. A future
    /// commit wires this through an AI dispatch that uses the
    /// skill body as the procedure description; the current
    /// surface lets operators inspect skills + pipe them into
    /// their own runners.
    Run(RunArgs),
    /// Delete auto-generated SKILL.md files older than
    /// `--max-age-days` (default 30) from
    /// `~/.relix/skills/auto/`. The hand-authored skills under
    /// `~/.relix/skills/` are never touched. Use `--dry-run`
    /// to preview without deleting.
    Prune(PruneArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Extra root directory to scan. Repeatable.
    #[arg(long)]
    pub root: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Skill name (file stem, or parent dir for SKILL.md).
    pub name: String,
    /// Extra root directory to scan. Repeatable.
    #[arg(long)]
    pub root: Vec<PathBuf>,
}

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// Max age in days. Files in the auto directory whose
    /// mtime is older than this get deleted.
    #[arg(long, default_value_t = 30)]
    pub max_age_days: i64,
    /// Show what would be deleted without removing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Override the auto directory (default:
    /// `~/.relix/skills/auto`). Repeatable for ad-hoc cleanup
    /// of operator-curated mirror directories.
    #[arg(long)]
    pub dir: Option<PathBuf>,
}

pub fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List(args) => list(&args.root),
        Cmd::Run(args) => run_skill(&args.name, &args.root),
        Cmd::Prune(args) => prune(&args),
    }
}

fn list(extra_roots: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let skills = relix_runtime::nodes::ai::skills::discover_skills(extra_roots);
    // Also surface any AGENTS.md the loader sees from cwd —
    // useful to confirm the bot will pick up project context.
    if let Ok(cwd) = std::env::current_dir()
        && let Some(agents) = relix_runtime::nodes::ai::skills::discover_agents_md(&cwd)
    {
        println!("AGENTS.md:");
        println!("  {}", agents.path.display());
        println!();
    }
    if skills.is_empty() {
        println!("no SKILL.md / *.md files discovered");
        println!();
        println!("search locations:");
        println!("  ./SKILL.md");
        println!("  ./skills/*.md");
        println!("  ~/.relix/skills/*.md");
        for r in extra_roots {
            println!("  {} (extra)", r.display());
        }
        return Ok(());
    }
    println!("{:<24}  {:<40}  PATH", "NAME", "TITLE");
    for s in skills {
        println!(
            "{:<24}  {:<40}  {}",
            s.name,
            truncate(&s.title, 40),
            s.path.display()
        );
    }
    Ok(())
}

fn run_skill(name: &str, extra_roots: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let skills = relix_runtime::nodes::ai::skills::discover_skills(extra_roots);
    let skill = skills
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no skill named `{name}` discovered"))?;
    // Today: print the skill body. The wired execution path
    // (hand to AI + run the procedure) lands in a follow-up
    // when the AGENTS.md plumbing into ai.chat ships — same
    // file, same loader.
    print!("{}", skill.body);
    if !skill.body.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn prune(args: &PruneArgs) -> Result<(), Box<dyn std::error::Error>> {
    let dir = match args.dir.clone() {
        Some(d) => d,
        None => {
            let cfg = relix_runtime::nodes::ai::skills::SkillsConfig::default();
            match relix_runtime::nodes::ai::skills::resolve_auto_skill_dir(&cfg) {
                Some(d) => d,
                None => {
                    return Err("no HOME / USERPROFILE in env; pass --dir explicitly".into());
                }
            }
        }
    };
    if args.dry_run {
        // For dry run we just enumerate candidates without
        // deleting. Iterate the dir ourselves so the output is
        // deterministic and we don't need a second helper in the
        // runtime crate.
        if !dir.exists() {
            println!("auto-skill dir not present: {}", dir.display());
            return Ok(());
        }
        let cutoff = std::time::SystemTime::now()
            .checked_sub(std::time::Duration::from_secs(
                (args.max_age_days.max(0) as u64) * 86_400,
            ))
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut would_delete = 0usize;
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let p = entry.path();
            if !p.is_file() || p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let meta = entry.metadata()?;
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if mtime < cutoff {
                would_delete += 1;
                println!("would delete: {}", p.display());
            }
        }
        println!(
            "dry-run: {would_delete} file(s) would be deleted from {}",
            dir.display()
        );
        return Ok(());
    }
    let (scanned, deleted) =
        relix_runtime::nodes::ai::skills::prune_auto_skills(&dir, args.max_age_days)?;
    println!(
        "pruned {deleted} of {scanned} auto-skill file(s) in {}",
        dir.display()
    );
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut out = String::with_capacity(max);
    for _ in 0..max.saturating_sub(1) {
        match chars.next() {
            Some(c) => out.push(c),
            None => return out,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_under_cap_returns_input_unchanged() {
        assert_eq!(truncate("short", 40), "short");
    }

    #[test]
    fn truncate_over_cap_appends_ellipsis() {
        let s = truncate("this string is way too long for the cap", 10);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 10);
    }
}
