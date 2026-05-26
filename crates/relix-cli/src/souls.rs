//! `relix souls list` and `relix souls edit <agent>`.
//!
//! Thin CLI front-end over
//! `relix_runtime::nodes::ai::soul`. The runtime owns the
//! discovery / candidate-path logic so the bridge, the CLI,
//! and any future SDK all agree on where soul files live.

use std::path::PathBuf;
use std::process::Command;

use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List discovered SOUL.md files + the agents they apply
    /// to. Walks every documented search location and
    /// de-duplicates by agent name.
    List,
    /// Open the soul file for `agent` in `$EDITOR`. Creates
    /// the file (under `~/.relix/souls/<agent>.md`) if it
    /// doesn't exist so first-time operators can author one
    /// from the template the command writes.
    Edit(EditArgs),
}

#[derive(Args, Debug)]
pub struct EditArgs {
    /// Agent slug. Becomes the file stem under
    /// `~/.relix/souls/`.
    pub agent: String,
}

pub fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List => list(),
        Cmd::Edit(args) => edit(&args.agent),
    }
}

fn list() -> Result<(), Box<dyn std::error::Error>> {
    let souls = relix_runtime::nodes::ai::soul::list_all();
    if souls.is_empty() {
        println!("no SOUL.md files discovered");
        println!();
        println!("search locations:");
        for p in relix_runtime::nodes::ai::soul::candidate_paths("<agent>", None) {
            println!("  {}", p.display());
        }
        println!();
        println!("create one with `relix souls edit <agent>`");
        return Ok(());
    }
    let agent_col = "AGENT";
    let path_col = "PATH";
    println!("{agent_col:<20}  {path_col}");
    for s in souls {
        println!("{:<20}  {}", s.name, s.path.display());
    }
    Ok(())
}

fn edit(agent: &str) -> Result<(), Box<dyn std::error::Error>> {
    if agent.is_empty() {
        return Err("agent name is required".into());
    }
    let path = soul_path_for_edit(agent)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        std::fs::write(&path, default_soul_template(agent))?;
        eprintln!("created {}", path.display());
    }
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| {
            if cfg!(windows) {
                "notepad".to_string()
            } else {
                "vi".to_string()
            }
        });
    let status = Command::new(&editor).arg(&path).status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("editor `{editor}` exited {s}").into()),
        Err(e) => Err(format!("failed to spawn editor `{editor}`: {e}").into()),
    }
}

fn soul_path_for_edit(agent: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var).ok_or("no HOME / USERPROFILE in env")?;
    Ok(PathBuf::from(home)
        .join(".relix")
        .join("souls")
        .join(format!("{agent}.md")))
}

/// First-time-use template. Operators get a sketch to fill in
/// rather than a blank file — friction-reducer.
fn default_soul_template(agent: &str) -> String {
    format!(
        "# {agent}\n\
        \n\
        ## Personality\n\
        \n\
        ## Communication Style\n\
        \n\
        ## Knowledge Domain\n\
        \n\
        ## Constraints\n\
        \n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_includes_all_four_sections() {
        let body = default_soul_template("alice");
        assert!(body.contains("# alice"));
        assert!(body.contains("## Personality"));
        assert!(body.contains("## Communication Style"));
        assert!(body.contains("## Knowledge Domain"));
        assert!(body.contains("## Constraints"));
    }

    #[test]
    fn soul_path_for_edit_lands_under_home_relix_souls() {
        // Only verifies the construction shape, not actual env
        // (the path uses whatever env the test runner exposes).
        if let Ok(p) = soul_path_for_edit("bob") {
            let s = p.to_string_lossy();
            assert!(s.contains(".relix"));
            assert!(s.contains("souls"));
            assert!(s.ends_with("bob.md"));
        }
    }
}
