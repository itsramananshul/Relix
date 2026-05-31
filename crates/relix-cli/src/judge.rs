//! `relix judge ...` — RELIX-7.29 PART 4 operator surface.
//!
//! - `judge verdicts [--limit N]` — print the recent
//!   verdicts ring.
//! - `judge stats`                 — print the counters
//!   surfaced by `/v1/judge/stats`.

use std::time::Duration;

use clap::Subcommand;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the most recent judge verdicts.
    Verdicts {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Print the judge counters (proceed/modify/block/timeout +
    /// per-agent breakdown).
    Stats {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Verdicts { limit, bridge, raw } => verdicts(&bridge, limit, raw).await,
        Cmd::Stats { bridge, raw } => stats(&bridge, raw).await,
    }
}

async fn verdicts(bridge: &str, limit: usize, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/judge/verdicts?limit={}",
        bridge.trim_end_matches('/'),
        limit
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode verdicts: {e} (body={body})"))?;
    let items = v
        .get("verdicts")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        println!("(no judge verdicts recorded)");
        return Ok(());
    }
    for item in items {
        let verdict = item
            .pointer("/verdict/verdict")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let agent = item.get("agent").and_then(|x| x.as_str()).unwrap_or("?");
        let session = item
            .get("session_id")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let conf = item
            .get("final_confidence")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let timed_out = item
            .get("timed_out")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let ts = item
            .get("timestamp_ms")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        println!(
            "{ts:>13}  {agent:<16} {session:<20} conf={conf:.2}  {verdict}{}",
            if timed_out { " [timeout]" } else { "" }
        );
    }
    Ok(())
}

async fn stats(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/judge/stats", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode stats: {e} (body={body})"))?;
    let proceed = v.get("proceed_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let modify = v.get("modify_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let block = v.get("block_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let timeout = v.get("timeout_count").and_then(|x| x.as_u64()).unwrap_or(0);
    let buffered = v
        .get("recent_buffered")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cap = v.get("capacity").and_then(|x| x.as_u64()).unwrap_or(0);
    println!("proceed:   {proceed}");
    println!("modify:    {modify}");
    println!("block:     {block}");
    println!("timeout:   {timeout}");
    println!("buffered:  {buffered}/{cap}");
    if let Some(per) = v.get("per_agent").and_then(|x| x.as_object())
        && !per.is_empty()
    {
        println!("per_agent:");
        for (k, v) in per {
            let p = v.get("proceed").and_then(|x| x.as_u64()).unwrap_or(0);
            let m = v.get("modify").and_then(|x| x.as_u64()).unwrap_or(0);
            let b = v.get("block").and_then(|x| x.as_u64()).unwrap_or(0);
            let t = v.get("timeout").and_then(|x| x.as_u64()).unwrap_or(0);
            println!("  {k:<20}  proceed={p}  modify={m}  block={b}  timeout={t}");
        }
    }
    Ok(())
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}
