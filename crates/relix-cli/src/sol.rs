//! `relix-cli sol ...` — W2-004 SOLFlow authoring helpers.
//!
//! Two subcommands today:
//!
//! - `sol templates` — list every baked-in workflow template
//!   with a one-line description so operators can scan the
//!   catalog without leaving the terminal.
//! - `sol new --template <name> --out <path>` — write a
//!   template to disk. Refuses to overwrite without `--force`.
//!   This is the "quick add" from the W2-004 spec; operators
//!   get a working `.sol` source in one command instead of
//!   hand-copying from `flows/`.
//!
//! Templates are baked into the binary via `include_str!` so
//! `relix-cli` is genuinely self-contained — no extra
//! resource directory to ship, no path lookups, no "where's
//! the template" surprises.
//!
//! Export-first remains correct (the dashboard doesn't try to
//! synthesize SOL source from a visual editor — it renders
//! the source as the source-of-truth).

use std::path::PathBuf;

use clap::Subcommand;

/// Catalog of baked-in templates. Each entry pairs a short
/// operator-visible name with one-line description + the
/// source body. Names are stable — operators script against
/// them. Add new entries at the END so existing automation
/// keeps working.
const TEMPLATES: &[Template] = &[
    Template {
        name: "ping",
        description: "Simplest distributed flow — single remote_call to node.health on a controller peer.",
        body: include_str!("../../../flows/ping.sol"),
    },
    Template {
        name: "chained_health",
        description: "Multi-peer sequential orchestration — two remote_calls (memory then ai) chained in SOL.",
        body: include_str!("../../../flows/chained_health.sol"),
    },
    Template {
        name: "chat",
        description: "Conversational agent — persist user turn, read history, dispatch ai.chat, persist reply.",
        body: include_str!("../../../flows/chat.sol"),
    },
    Template {
        name: "chat_template",
        description: "Bridge-rendered chat flow with placeholder substitution (M8).",
        body: include_str!("../../../flows/chat_template.sol"),
    },
    Template {
        name: "chat_with_tool",
        description: "Chat flow with an interleaved tool.web_fetch step (M9).",
        body: include_str!("../../../flows/chat_with_tool.sol"),
    },
    Template {
        name: "memory_demo",
        description: "Persist + read a couple of turns against a memory peer (M7).",
        body: include_str!("../../../flows/memory_demo.sol"),
    },
];

/// One row in the template catalog. `body` is the embedded
/// source the operator gets on `sol new`.
struct Template {
    name: &'static str,
    description: &'static str,
    body: &'static str,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List baked-in workflow templates with descriptions.
    /// Read-only.
    Templates,
    /// Write a template to disk as a starting point for a new
    /// flow. Refuses to overwrite an existing file unless
    /// `--force` is passed.
    New {
        /// Template name (one of `sol templates`).
        #[arg(long)]
        template: String,
        /// Destination `.sol` path.
        #[arg(long)]
        out: PathBuf,
        /// Overwrite the destination if it already exists.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Templates => {
            render_templates();
            Ok(())
        }
        Cmd::New {
            template,
            out,
            force,
        } => new_from_template(&template, &out, force),
    }
}

fn render_templates() {
    let name_h = "name";
    let desc_h = "description";
    println!("{name_h:<18}  {desc_h}");
    for t in TEMPLATES {
        println!("{:<18}  {}", t.name, t.description);
    }
    println!("count={}", TEMPLATES.len());
}

fn new_from_template(
    name: &str,
    out: &std::path::Path,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let t = lookup_template(name).ok_or_else(|| {
        format!(
            "unknown template '{name}' (available: {})",
            TEMPLATES
                .iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;
    if out.exists() && !force {
        return Err(format!(
            "refusing to overwrite existing file {}: pass --force to override",
            out.display()
        )
        .into());
    }
    // Create parent directories so `relix-cli sol new --out
    // flows/new/my-flow.sol` works when `flows/new/` doesn't
    // exist yet. We do create directories here (unlike the
    // browser screenshot dir convention) because the
    // operator's intent is explicit — they named the path.
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, t.body)?;
    println!(
        "wrote {} ({} bytes) from template '{}'",
        out.display(),
        t.body.len(),
        name
    );
    Ok(())
}

fn lookup_template(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn catalog_is_non_empty_and_unique_names() {
        assert!(!TEMPLATES.is_empty());
        let mut seen = std::collections::HashSet::new();
        for t in TEMPLATES {
            assert!(seen.insert(t.name), "duplicate template name: {}", t.name);
            assert!(!t.body.is_empty(), "template {} has empty body", t.name);
            assert!(
                !t.description.is_empty(),
                "template {} has empty description",
                t.name
            );
        }
    }

    #[test]
    fn known_templates_are_advertised() {
        // The five flows that ship today should all be in the
        // catalog. If someone removes one without updating the
        // catalog, this test catches it.
        for expected in [
            "ping",
            "chained_health",
            "chat",
            "chat_template",
            "chat_with_tool",
            "memory_demo",
        ] {
            assert!(
                lookup_template(expected).is_some(),
                "missing template: {expected}"
            );
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(lookup_template("does-not-exist").is_none());
    }

    #[test]
    fn new_writes_template_to_disk() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("my-ping.sol");
        new_from_template("ping", &out, false).unwrap();
        let body = std::fs::read_to_string(&out).unwrap();
        // The body should match the embedded ping.sol.
        assert_eq!(body, lookup_template("ping").unwrap().body);
    }

    #[test]
    fn new_refuses_to_overwrite_without_force() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("existing.sol");
        std::fs::write(&out, "// already here").unwrap();
        let err = new_from_template("ping", &out, false).unwrap_err();
        assert!(
            err.to_string().contains("refusing to overwrite"),
            "expected refusal, got: {err}"
        );
        // Existing content must be untouched.
        let body = std::fs::read_to_string(&out).unwrap();
        assert_eq!(body, "// already here");
    }

    #[test]
    fn new_overwrites_with_force() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("existing.sol");
        std::fs::write(&out, "// stale").unwrap();
        new_from_template("ping", &out, true).unwrap();
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(body.contains("// flows/ping.sol"));
    }

    #[test]
    fn new_creates_parent_directories() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("nested/dir/my-flow.sol");
        new_from_template("ping", &out, false).unwrap();
        assert!(out.exists());
    }

    #[test]
    fn new_unknown_template_lists_available_names() {
        let td = TempDir::new().unwrap();
        let out = td.path().join("x.sol");
        let err = new_from_template("does-not-exist", &out, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown template"));
        assert!(msg.contains("ping"));
        assert!(msg.contains("chat"));
    }
}
