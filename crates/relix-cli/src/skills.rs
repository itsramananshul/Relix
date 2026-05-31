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
    /// List discovered skills. By default walks the file-based
    /// SKILL.md roots (cwd/SKILL.md, cwd/skills/, ~/.relix/skills/,
    /// plus any --root entries). When `--query / --agent /
    /// --min-confidence / --bridge` is passed, hits the bridge's
    /// `GET /v1/skills` and prints the SQLite-backed catalogue
    /// instead.
    List(ListArgs),
    /// Print the body of the named skill to stdout (file-based
    /// path) OR full detail of one stored skill (when --id and
    /// --bridge are supplied).
    Run(RunArgs),
    /// Delete auto-generated SKILL.md files older than
    /// `--max-age-days` (default 30) from
    /// `~/.relix/skills/auto/`. The hand-authored skills under
    /// `~/.relix/skills/` are never touched. Use `--dry-run`
    /// to preview without deleting.
    Prune(PruneArgs),
    /// GAP 4: print full detail (incl. version history) for one
    /// stored skill. Hits GET /v1/skills/:id.
    Show(ShowArgs),
    /// GAP 4: update one stored skill. Hits PATCH /v1/skills/:id.
    Edit(EditArgs),
    /// GAP 4: deprecate one stored skill. Hits POST
    /// /v1/skills/:id/deprecate.
    Delete(DeleteArgs),
    /// GAP 4: export one stored skill as JSON. Without --out the
    /// document is printed to stdout.
    Export(ExportArgs),
    /// GAP 4: import a skill JSON file via POST /v1/skills.
    Import(ImportArgs),
    /// GAP 4: print aggregate statistics. Hits GET /v1/skills/stats.
    Stats(StatsArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Extra root directory to scan. Repeatable.
    #[arg(long)]
    pub root: Vec<PathBuf>,
    /// GAP 4: switch to bridge mode. Always set when any of
    /// --query / --agent / --min-confidence is provided.
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// Filter by source agent (bridge mode only).
    #[arg(long)]
    pub agent: Option<String>,
    /// Minimum confidence filter (bridge mode only).
    #[arg(long = "min-confidence")]
    pub min_confidence: Option<f32>,
    /// Substring search on name + description + tags (bridge mode only).
    #[arg(long)]
    pub query: Option<String>,
    /// Result cap (bridge mode only).
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Print raw JSON instead of the human table.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EditArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    #[arg(long)]
    pub description: Option<String>,
    /// Comma-separated tag list. Replaces the existing set.
    #[arg(long)]
    pub tags: Option<String>,
    /// Path to a JSON file containing a `[{step, tool?, prompt?}, ...]`
    /// array.
    #[arg(long)]
    pub steps_file: Option<PathBuf>,
    /// `active` | `deprecated` | `quarantined`.
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long = "change-reason")]
    pub change_reason: Option<String>,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    #[arg(long)]
    pub reason: Option<String>,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    /// Optional output path. When omitted the document is
    /// printed to stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// GAP 3: output format. `json` (default) emits the JSON
    /// document the bridge returned; `md` renders the skill as
    /// a SKILL.md-style markdown document (works with the
    /// Linux Foundation Agentic AI shared-file convention).
    #[arg(long, default_value = "json")]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// Path to a JSON file with the StoreArgs schema.
    pub file: PathBuf,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct StatsArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
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
        Cmd::List(args) => {
            let bridge_mode =
                args.query.is_some() || args.agent.is_some() || args.min_confidence.is_some();
            if bridge_mode {
                tokio_run(skills_list_remote(args))
            } else {
                list(&args.root)
            }
        }
        Cmd::Run(args) => run_skill(&args.name, &args.root),
        Cmd::Prune(args) => prune(&args),
        Cmd::Show(args) => tokio_run(skills_show(args)),
        Cmd::Edit(args) => tokio_run(skills_edit(args)),
        Cmd::Delete(args) => tokio_run(skills_delete(args)),
        Cmd::Export(args) => tokio_run(skills_export(args)),
        Cmd::Import(args) => tokio_run(skills_import(args)),
        Cmd::Stats(args) => tokio_run(skills_stats(args)),
    }
}

/// Build a small tokio runtime + drive `fut` to completion. The
/// outer CLI is sync; the bridge calls are async — this is the
/// adapter.
fn tokio_run<F>(fut: F) -> Result<(), Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = Result<(), Box<dyn std::error::Error>>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(fut)
}

async fn skills_list_remote(args: ListArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let mut url = format!("{base}/v1/skills?limit={}", args.limit);
    if let Some(q) = args.query.as_ref() {
        url.push_str("&q=");
        url.push_str(&urlencode(q));
    }
    if let Some(a) = args.agent.as_ref() {
        url.push_str("&agent=");
        url.push_str(&urlencode(a));
    }
    if let Some(c) = args.min_confidence {
        url.push_str(&format!("&min_confidence={c}"));
    }
    let client = reqwest::Client::new();
    let r = client.get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no skills matched)");
        return Ok(());
    }
    println!(
        "{:<36}  {:<28}  {:>6}  {:>5}  {:<10}",
        "ID", "NAME", "CONF", "USES", "AGENT"
    );
    for r in rows {
        let id = r.get("id").and_then(|x| x.as_str()).unwrap_or("");
        let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let conf = r.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let uses = r.get("usage_count").and_then(|x| x.as_i64()).unwrap_or(0);
        let agent = r.get("source_agent").and_then(|x| x.as_str()).unwrap_or("");
        println!(
            "{:<36}  {:<28}  {:>6.2}  {:>5}  {:<10}",
            id,
            truncate(name, 28),
            conf,
            uses,
            truncate(agent, 10)
        );
    }
    Ok(())
}

async fn skills_show(args: ShowArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills/{}", urlencode(&args.id));
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn skills_edit(args: EditArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut body = serde_json::Map::new();
    if let Some(d) = args.description {
        body.insert("description".into(), serde_json::Value::from(d));
    }
    if let Some(tags) = args.tags {
        let parsed: Vec<String> = tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        body.insert("tags".into(), serde_json::Value::from(parsed));
    }
    if let Some(p) = args.steps_file {
        let text = std::fs::read_to_string(&p)?;
        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        body.insert("steps".into(), parsed);
    }
    if let Some(s) = args.status {
        body.insert("status".into(), serde_json::Value::from(s));
    }
    if let Some(r) = args.change_reason {
        body.insert("change_reason".into(), serde_json::Value::from(r));
    }
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills/{}", urlencode(&args.id));
    let r = reqwest::Client::new()
        .patch(&url)
        .json(&serde_json::Value::Object(body))
        .send()
        .await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn skills_delete(args: DeleteArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut body = serde_json::Map::new();
    if let Some(r) = args.reason {
        body.insert("reason".into(), serde_json::Value::from(r));
    }
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills/{}/deprecate", urlencode(&args.id));
    let r = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::Value::Object(body))
        .send()
        .await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn skills_export(args: ExportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills/{}", urlencode(&args.id));
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }

    let format = args.format.trim().to_ascii_lowercase();
    let output = match format.as_str() {
        "json" | "" => {
            let v: serde_json::Value = serde_json::from_str(&body)?;
            serde_json::to_string_pretty(&v)?
        }
        "md" | "markdown" => {
            // GAP 3: render the bridge's JSON body as a
            // SKILL.md-style markdown document. The runtime's
            // render_stored_skill_md helper takes a typed
            // StoredSkill; we deserialise the bridge body into
            // it via serde so any forward-compat additions on
            // the JSON side don't break the CLI.
            let stored: relix_runtime::nodes::ai::skill_store::StoredSkill =
                serde_json::from_str(&body)?;
            relix_runtime::nodes::ai::skill_store::render_stored_skill_md(&stored)
        }
        other => {
            eprintln!("error: unknown --format {other:?} (expected json or md)");
            std::process::exit(2);
        }
    };

    match args.out.as_ref() {
        Some(p) => {
            std::fs::write(p, &output)?;
            eprintln!("wrote {} bytes to {}", output.len(), p.display());
        }
        None => {
            println!("{output}");
        }
    }
    Ok(())
}

async fn skills_import(args: ImportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(&args.file)?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)?;
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills");
    let r = reqwest::Client::new()
        .post(&url)
        .json(&parsed)
        .send()
        .await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn skills_stats(args: StatsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/skills/stats");
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let total = v.get("total_skills").and_then(|x| x.as_i64()).unwrap_or(0);
    let active = v.get("active_skills").and_then(|x| x.as_i64()).unwrap_or(0);
    let avg = v
        .get("avg_confidence")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    println!("total_skills:    {total}");
    println!("active_skills:   {active}");
    println!("avg_confidence:  {avg:.2}");
    if let Some(arr) = v.get("top_5_by_usage").and_then(|x| x.as_array())
        && !arr.is_empty()
    {
        println!();
        println!("top by usage:");
        for s in arr {
            let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let uses = s.get("usage_count").and_then(|x| x.as_i64()).unwrap_or(0);
            let conf = s.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.0);
            println!("  {name:<32}  uses={uses:>4}  conf={conf:>4.2}");
        }
    }
    Ok(())
}

/// Percent-encode the operator-supplied URL segment. Bare
/// reqwest doesn't encode IDs in path segments; we do it
/// ourselves so a hyphen-bearing UUID doesn't break the route
/// match.
fn urlencode(s: &str) -> String {
    // Conservative: encode every byte that isn't an unreserved
    // URL char. Matches RFC 3986 §2.3.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~');
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn list(extra_roots: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let skills = relix_runtime::nodes::ai::skills::discover_skills(extra_roots);
    // GAP 3: surface every agent-context file the AI controller
    // would pick up at startup — AGENTS.md, CLAUDE.md, and
    // .cursorrules. The bot's system prompt merges these in
    // canonical order so operators can confirm the wiring from
    // the CLI without spinning up the controller.
    if let Ok(cwd) = std::env::current_dir() {
        let ctx = relix_runtime::nodes::ai::skills::discover_agent_context(&cwd);
        if !ctx.is_empty() {
            println!("Agent context files:");
            for entry in &ctx {
                println!("  {}", entry.path.display());
            }
            println!();
        }
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
