//! `relix pii` — RELIX-7.28 Part 3 operator surface.
//!
//! Two subcommands, each a thin HTTP forwarder onto the bridge:
//!
//! - `pii stats [--hours N]`         — totals by action + top methods.
//! - `pii events [--method M] [--limit N]` — recent events.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Aggregate counts over a window (default 24 hours).
    Stats {
        #[arg(long, default_value_t = 24)]
        hours: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Newest events, optionally filtered by capability method.
    Events {
        #[arg(long)]
        method: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Stats { hours, bridge, raw } => stats(&bridge, hours, raw).await,
        Cmd::Events {
            method,
            limit,
            bridge,
            raw,
        } => events(&bridge, method.as_deref(), limit, raw).await,
    }
}

async fn stats(bridge: &str, hours: u32, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/pii/stats?hours={hours}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let stats: ScanStats =
        serde_json::from_str(&body).map_err(|e| format!("decode stats: {e} (body={body})"))?;
    println!(
        "window = {h}h  total = {t}  blocked = {b}  redacted = {r}  logged = {l}",
        h = stats.window_hours,
        t = stats.total_events,
        b = stats.blocked,
        r = stats.redacted,
        l = stats.logged,
    );
    if stats.top_methods.is_empty() {
        println!("(no events in window)");
        return Ok(());
    }
    let method_w = stats
        .top_methods
        .iter()
        .map(|m| m.method.len())
        .max()
        .unwrap_or(8)
        .max(8);
    println!(
        "\n{method:<mw$}  {count:>8}",
        method = "method",
        count = "count",
        mw = method_w,
    );
    for row in &stats.top_methods {
        println!(
            "{method:<mw$}  {count:>8}",
            method = row.method,
            count = row.count,
            mw = method_w,
        );
    }
    Ok(())
}

async fn events(
    bridge: &str,
    method: Option<&str>,
    limit: usize,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!(
        "{}/v1/pii/events?limit={limit}",
        bridge.trim_end_matches('/')
    );
    if let Some(m) = method {
        url.push_str(&format!("&method={m}"));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<EventRow> =
        serde_json::from_str(&body).map_err(|e| format!("decode events: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no PII events recorded)");
        return Ok(());
    }
    println!(
        "{at:<13}  {action:<8}  {agent:<16}  {method:<24}  {dir:<8}  {spans:>5}  types",
        at = "ts(ms)",
        action = "action",
        agent = "agent",
        method = "method",
        dir = "direction",
        spans = "spans",
    );
    for r in &rows {
        println!(
            "{at:<13}  {action:<8}  {agent:<16}  {method:<24}  {dir:<8}  {spans:>5}  {types}",
            at = r.recorded_at_ms,
            action = r.action_taken,
            agent = r.agent,
            method = r.method,
            dir = r.direction,
            spans = r.span_count,
            types = r.types,
        );
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

#[derive(Debug, Deserialize)]
struct ScanStats {
    #[serde(default)]
    window_hours: u32,
    #[serde(default)]
    total_events: u64,
    #[serde(default)]
    blocked: u64,
    #[serde(default)]
    redacted: u64,
    #[serde(default)]
    logged: u64,
    #[serde(default)]
    top_methods: Vec<MethodFrequency>,
}

#[derive(Debug, Deserialize)]
struct MethodFrequency {
    method: String,
    count: u64,
}

#[derive(Debug, Deserialize)]
struct EventRow {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    direction: String,
    #[serde(default)]
    action_taken: String,
    #[serde(default)]
    span_count: u32,
    #[serde(default)]
    recorded_at_ms: i64,
    #[serde(default)]
    types: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_stats_round_trips_through_serde() {
        let body = r#"{
            "window_hours":24,
            "total_events":7,
            "blocked":1,
            "redacted":4,
            "logged":2,
            "top_methods":[{"method":"ai.chat","count":5}]
        }"#;
        let stats: ScanStats = serde_json::from_str(body).unwrap();
        assert_eq!(stats.window_hours, 24);
        assert_eq!(stats.total_events, 7);
        assert_eq!(stats.top_methods.len(), 1);
        assert_eq!(stats.top_methods[0].method, "ai.chat");
        assert_eq!(stats.top_methods[0].count, 5);
    }

    #[test]
    fn event_row_round_trips_through_serde() {
        let body = r#"[{
            "request_id":"r1",
            "agent":"alice",
            "method":"ai.chat",
            "direction":"inbound",
            "action_taken":"redacted",
            "span_count":2,
            "recorded_at_ms":1700000000000,
            "types":"EMAIL,PHONE"
        }]"#;
        let rows: Vec<EventRow> = serde_json::from_str(body).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_taken, "redacted");
        assert_eq!(rows[0].span_count, 2);
        assert!(rows[0].types.contains("EMAIL"));
    }
}
