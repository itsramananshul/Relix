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

pub fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List(args) => list(&args.root),
        Cmd::Run(args) => run_skill(&args.name, &args.root),
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
