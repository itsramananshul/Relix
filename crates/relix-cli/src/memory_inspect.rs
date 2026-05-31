//! `relix memory list / show / search / invalidate / stats`.
//!
//! Thin HTTP client over the bridge's
//! `/v1/memory/records/*` and `/v1/memory/stats` endpoints
//! (see `crates/relix-web-bridge/src/memory_inspect.rs`).
//! Output is operator-friendly by default; `--json` returns
//! the raw response body so dashboards / scripts can consume
//! it without re-parsing the human format.

use std::io::{self, Write};

use clap::{Args, Subcommand};
use serde::Deserialize;
use serde_json::Value;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List records (filtered by layer / source).
    List(ListArgs),
    /// Show one record by id.
    Show(ShowArgs),
    /// Substring search over `text`.
    Search(SearchArgs),
    /// Mark a record as no longer valid (sets `valid_to`).
    Invalidate(InvalidateArgs),
    /// Counts per layer + most-recent record per layer.
    Stats(StatsArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// Filter by layer: raw, semantic, observation, model.
    #[arg(long)]
    pub layer: Option<String>,
    /// Filter by source (typically a session id).
    #[arg(long)]
    pub source: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub query: String,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct InvalidateArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    pub id: String,
    /// Skip the confirmation prompt. Required for scripted use.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct StatsArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long)]
    pub json: bool,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List(a) => list(&a).await,
        Cmd::Show(a) => show(&a).await,
        Cmd::Search(a) => search(&a).await,
        Cmd::Invalidate(a) => invalidate(&a).await,
        Cmd::Stats(a) => stats(&a).await,
    }
}

#[derive(Debug, Deserialize)]
struct RecordJson {
    id: String,
    layer: String,
    text: String,
    source: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    valid_from: i64,
    #[serde(default)]
    valid_to: Option<i64>,
    #[serde(default)]
    observed_at: i64,
    #[serde(default)]
    has_embedding: bool,
    #[serde(default)]
    embedding_dim: usize,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    records: Vec<RecordJson>,
    count: usize,
}

async fn list(args: &ListArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let mut url = format!("{base}/v1/memory/records?limit={}", args.limit);
    if let Some(l) = &args.layer {
        url.push_str(&format!("&layer={l}"));
    }
    if let Some(s) = &args.source {
        url.push_str(&format!("&source={s}"));
    }
    let resp = reqwest::Client::new().get(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}: {body}").into());
    }
    if args.json {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        writeln!(out, "{body}")?;
        return Ok(());
    }
    let parsed: ListResponse = serde_json::from_str(&body)?;
    println!("{} record(s)", parsed.count);
    println!(
        "{:<18}  {:<12}  {:<20}  {:<10}  TEXT",
        "ID", "LAYER", "SOURCE", "VALID_TO"
    );
    for r in &parsed.records {
        let preview = truncate(&r.text, 80);
        let valid_to = r
            .valid_to
            .map(|v| v.to_string())
            .unwrap_or_else(|| "—".to_string());
        println!(
            "{:<18}  {:<12}  {:<20}  {:<10}  {preview}",
            truncate(&r.id, 18),
            r.layer,
            truncate(&r.source, 20),
            valid_to
        );
    }
    Ok(())
}

async fn show(args: &ShowArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/records/{}", args.id);
    let resp = reqwest::Client::new().get(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}: {body}").into());
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let r: RecordJson = serde_json::from_str(&body)?;
    println!("id           {}", r.id);
    println!("layer        {}", r.layer);
    println!("source       {}", r.source);
    println!("tags         {}", r.tags.join(", "));
    println!("created_at   {}", r.created_at);
    println!("valid_from   {}", r.valid_from);
    println!(
        "valid_to     {}",
        r.valid_to
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(still valid)".to_string())
    );
    println!("observed_at  {}", r.observed_at);
    println!(
        "embedding    {}",
        if r.has_embedding {
            format!("yes ({} dims)", r.embedding_dim)
        } else {
            "no".to_string()
        }
    );
    println!("text         ----");
    println!("{}", r.text);
    Ok(())
}

async fn search(args: &SearchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/records/search");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "query": args.query, "limit": args.limit }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("POST {url}: HTTP {status}: {body}").into());
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let parsed: ListResponse = serde_json::from_str(&body)?;
    println!("{} hit(s) for `{}`", parsed.count, args.query);
    for r in &parsed.records {
        println!(
            "  [{}/{}] {}",
            r.layer,
            truncate(&r.id, 16),
            truncate(&r.text, 100)
        );
    }
    Ok(())
}

async fn invalidate(args: &InvalidateArgs) -> Result<(), Box<dyn std::error::Error>> {
    if !args.yes {
        // The CLI asks for an interactive confirm by default.
        // The `--yes` flag is documented + required for
        // scripts.
        eprint!(
            "Are you sure you want to invalidate record `{}`? [y/N] ",
            args.id
        );
        io::stderr().flush().ok();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        let answer = buf.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            eprintln!("aborted");
            return Ok(());
        }
    }
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/records/{}/invalidate", args.id);
    let resp = reqwest::Client::new().post(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("POST {url}: HTTP {status}: {body}").into());
    }
    println!("invalidated: {body}");
    Ok(())
}

async fn stats(args: &StatsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/memory/stats");
    let resp = reqwest::Client::new().get(&url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("GET {url}: HTTP {status}: {body}").into());
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let parsed: Value = serde_json::from_str(&body)?;
    println!("Counts per layer:");
    if let Some(c) = parsed.get("counts_per_layer").and_then(|v| v.as_object()) {
        for (k, v) in c {
            println!("  {k:<12}  {}", v);
        }
    }
    println!("\nPending embeddings:");
    if let Some(p) = parsed
        .get("pending_embeddings_per_layer")
        .and_then(|v| v.as_object())
    {
        for (k, v) in p {
            println!("  {k:<12}  {}", v);
        }
    }
    println!("\nMost recent per layer:");
    if let Some(mr) = parsed
        .get("most_recent_per_layer")
        .and_then(|v| v.as_object())
    {
        for (k, v) in mr {
            let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("—");
            let preview = v
                .get("text")
                .and_then(|x| x.as_str())
                .map(|t| truncate(t, 80))
                .unwrap_or_else(|| "—".to_string());
            println!("  {k:<12}  {id}  {preview}");
        }
    }
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    let mut chars = s.chars();
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
    fn truncate_under_cap_is_unchanged() {
        assert_eq!(truncate("hello", 20), "hello");
    }

    #[test]
    fn truncate_over_cap_ends_with_ellipsis() {
        let s = truncate("the quick brown fox jumps", 10);
        assert!(s.ends_with('…'));
        assert!(s.chars().count() <= 10);
    }
}
