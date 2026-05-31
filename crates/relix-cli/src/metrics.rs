//! `relix-cli metrics ...` — RELIX-7.11 operator surface.
//!
//! Four subcommands, each a thin HTTP forwarder onto the
//! `/v1/metrics/*` bridge endpoints:
//!
//! - `metrics summary [--agent <name>] [--hours 24]` — table.
//! - `metrics alerts` — active alerts.
//! - `metrics cost [--hours 24]` — cost breakdown.
//! - `metrics timeseries --agent <name> [--hours 6] [--bucket 5]`
//!   — ASCII sparkline.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print a table of per-agent summaries. With `--agent X`
    /// renders only the named agent.
    Summary {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 24)]
        hours: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print raw JSON instead of the formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Print the active-alerts list with severity indicators.
    Alerts {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Cost breakdown by (agent, method), descending.
    Cost {
        #[arg(long, default_value_t = 24)]
        hours: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Time-series for one agent, rendered as an ASCII
    /// sparkline of invocation rate over time.
    Timeseries {
        #[arg(long)]
        agent: String,
        #[arg(long, default_value_t = 6)]
        hours: u32,
        #[arg(long, default_value_t = 5)]
        bucket: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// GAP 22 Feature 2: persisted per-provider cost baselines.
    /// `--provider` filters to one model id; `--windows` caps the
    /// returned window count (default 24).
    CostBaselines {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value_t = 24)]
        windows: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// GAP 22 Feature 2: persisted per-agent ask-human rate
    /// baselines.
    AskHumanBaselines {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 24)]
        windows: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// GAP 22 Feature 2: archived cost-spike fire events.
    CostSpikes {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Summary {
            agent,
            hours,
            bridge,
            raw,
        } => summary(&bridge, agent.as_deref(), hours, raw).await,
        Cmd::Alerts { bridge, raw } => alerts(&bridge, raw).await,
        Cmd::Cost { hours, bridge, raw } => cost(&bridge, hours, raw).await,
        Cmd::Timeseries {
            agent,
            hours,
            bucket,
            bridge,
            raw,
        } => timeseries(&bridge, &agent, hours, bucket, raw).await,
        Cmd::CostBaselines {
            provider,
            windows,
            bridge,
            raw,
        } => cost_baselines(&bridge, provider.as_deref(), windows, raw).await,
        Cmd::AskHumanBaselines {
            agent,
            windows,
            bridge,
            raw,
        } => ask_human_baselines(&bridge, agent.as_deref(), windows, raw).await,
        Cmd::CostSpikes { limit, bridge, raw } => cost_spikes(&bridge, limit, raw).await,
    }
}

async fn summary(
    bridge: &str,
    agent: Option<&str>,
    hours: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = if let Some(a) = agent {
        format!(
            "{}/v1/metrics/agents/{a}/summary?hours={hours}",
            bridge.trim_end_matches('/')
        )
    } else {
        format!(
            "{}/v1/metrics/agents?hours={hours}",
            bridge.trim_end_matches('/')
        )
    };
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    if agent.is_some() {
        let row: AgentSummary = serde_json::from_str(&body)
            .map_err(|e| format!("decode summary: {e} (body={body})"))?;
        render_summary_table(std::slice::from_ref(&row));
    } else {
        let rows: Vec<AgentSummary> = serde_json::from_str(&body)
            .map_err(|e| format!("decode agents list: {e} (body={body})"))?;
        if rows.is_empty() {
            println!("(no agents with metrics in the last {hours}h)");
            return Ok(());
        }
        render_summary_table(&rows);
    }
    Ok(())
}

async fn alerts(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/metrics/alerts", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<ActiveAlert> =
        serde_json::from_str(&body).map_err(|e| format!("decode alerts: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no active alerts)");
        return Ok(());
    }
    for a in rows {
        let badge = match a.severity.as_str() {
            "critical" => "[!!]",
            "warning" => "[! ]",
            _ => "[? ]",
        };
        println!(
            "{badge} {agent}  {kind}  threshold={threshold:.2}  actual={actual:.2}\n     {msg}",
            agent = a.agent,
            kind = a.kind,
            threshold = a.threshold,
            actual = a.actual,
            msg = a.message,
        );
    }
    Ok(())
}

async fn cost(bridge: &str, hours: u32, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/metrics/cost?hours={hours}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<CostRow> =
        serde_json::from_str(&body).map_err(|e| format!("decode cost: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no recorded cost in the last {hours}h)");
        return Ok(());
    }
    let agent_w = rows.iter().map(|r| r.agent.len()).max().unwrap_or(5).max(5);
    let method_w = rows
        .iter()
        .map(|r| r.method.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{agent:<aw$}  {method:<mw$}  {cost:>12}  {tokens:>10}  {invs:>7}",
        agent = "agent",
        method = "method",
        cost = "cost (USD)",
        tokens = "tokens",
        invs = "calls",
        aw = agent_w,
        mw = method_w,
    );
    for r in rows {
        println!(
            "{agent:<aw$}  {method:<mw$}  {cost:>12.4}  {tokens:>10}  {invs:>7}",
            agent = r.agent,
            method = r.method,
            cost = (r.total_cost_micros as f64) / 1_000_000.0,
            tokens = r.total_tokens,
            invs = r.invocations,
            aw = agent_w,
            mw = method_w,
        );
    }
    Ok(())
}

async fn timeseries(
    bridge: &str,
    agent: &str,
    hours: u32,
    bucket: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/metrics/agents/{agent}/timeseries?hours={hours}&bucket_minutes={bucket}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<TimeseriesBucket> =
        serde_json::from_str(&body).map_err(|e| format!("decode timeseries: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no metrics for {agent} in the last {hours}h)");
        return Ok(());
    }
    let counts: Vec<u64> = rows.iter().map(|b| b.invocations).collect();
    let line = sparkline(&counts);
    let total: u64 = counts.iter().sum();
    let max = counts.iter().copied().max().unwrap_or(0);
    println!(
        "{agent} — {n} buckets, total={total} invocations, max-per-bucket={max}",
        n = rows.len()
    );
    println!("{line}");
    Ok(())
}

// ── rendering helpers ────────────────────────────────────

const SPARK_CHARS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render a sparkline of `counts` using Unicode block
/// characters. Empty input returns an empty string; a single
/// non-zero element renders as `█`.
pub fn sparkline(counts: &[u64]) -> String {
    if counts.is_empty() {
        return String::new();
    }
    let max = counts.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return "▁".repeat(counts.len());
    }
    let n = SPARK_CHARS.len() - 1;
    let mut out = String::with_capacity(counts.len());
    for c in counts {
        let scaled = (*c as f64 / max as f64) * n as f64;
        // Use ceil so any non-zero count is at least the
        // lowest-non-empty block (▂), giving the operator a
        // visual signal that the bucket wasn't dead.
        let idx = if *c == 0 {
            0
        } else {
            (scaled.ceil() as usize).clamp(1, n)
        };
        out.push(SPARK_CHARS[idx]);
    }
    out
}

fn render_summary_table(rows: &[AgentSummary]) {
    let agent_w = rows.iter().map(|r| r.agent.len()).max().unwrap_or(5).max(5);
    println!(
        "{agent:<aw$}  {invs:>6}  {success:>7}  {err:>6}  {p95:>6}  {tokens:>7}  {cost:>10}  top_error",
        agent = "agent",
        invs = "calls",
        success = "succ%",
        err = "err%",
        p95 = "p95ms",
        tokens = "tokens",
        cost = "cost (USD)",
        aw = agent_w,
    );
    for r in rows {
        let pct = |x: f64| x * 100.0;
        println!(
            "{agent:<aw$}  {invs:>6}  {success:>6.2}%  {err:>5.2}%  {p95:>6}  {tokens:>7}  {cost:>10.4}  {top}",
            agent = r.agent,
            invs = r.invocations,
            success = pct(r.success_rate),
            err = pct(r.error_rate),
            p95 = r.p95_latency_ms,
            tokens = r.total_tokens,
            cost = (r.total_cost_micros as f64) / 1_000_000.0,
            top = r.most_common_error_kind.as_deref().unwrap_or("-"),
            aw = agent_w,
        );
    }
}

// ── shared types ─────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
struct AgentSummary {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    invocations: u64,
    #[serde(default)]
    success_rate: f64,
    #[serde(default)]
    error_rate: f64,
    #[serde(default)]
    p95_latency_ms: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    total_cost_micros: u64,
    #[serde(default)]
    most_common_error_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActiveAlert {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    threshold: f64,
    #[serde(default)]
    actual: f64,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct CostRow {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    total_cost_micros: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    invocations: u64,
}

#[derive(Debug, Deserialize)]
struct TimeseriesBucket {
    #[serde(default)]
    invocations: u64,
}

// ── GAP 22 Feature 2 baseline + spike-history subcommands ──

async fn cost_baselines(
    bridge: &str,
    provider: Option<&str>,
    windows: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!(
        "{}/v1/metrics/cost-baselines?windows={windows}",
        bridge.trim_end_matches('/')
    );
    if let Some(p) = provider {
        url.push_str(&format!("&provider={p}"));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("windows")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no cost baselines)");
        return Ok(());
    }
    println!(
        "{:<20} {:>14} {:>14} {:>10} {:>20}",
        "provider", "avg_micros", "p95_micros", "calls", "created_at_ms"
    );
    for r in rows {
        let provider = r.get("provider").and_then(|x| x.as_str()).unwrap_or("?");
        let avg = r
            .get("avg_cost_micros_per_call")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let p95 = r
            .get("p95_cost_micros")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let calls = r
            .get("invocation_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let created = r.get("created_at_ms").and_then(|x| x.as_i64()).unwrap_or(0);
        println!(
            "{:<20} {:>14} {:>14} {:>10} {:>20}",
            provider, avg, p95, calls, created
        );
    }
    Ok(())
}

async fn ask_human_baselines(
    bridge: &str,
    agent: Option<&str>,
    windows: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!(
        "{}/v1/metrics/ask-human-baselines?windows={windows}",
        bridge.trim_end_matches('/')
    );
    if let Some(a) = agent {
        url.push_str(&format!("&agent={a}"));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("windows")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no ask-human baselines)");
        return Ok(());
    }
    println!(
        "{:<20} {:>10} {:>10} {:>8} {:>20}",
        "agent", "calls", "asked", "rate%", "created_at_ms"
    );
    for r in rows {
        let agent = r.get("agent").and_then(|x| x.as_str()).unwrap_or("?");
        let total = r
            .get("total_invocations")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let asked = r
            .get("ask_human_count")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let rate = r
            .get("ask_human_rate")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0);
        let created = r.get("created_at_ms").and_then(|x| x.as_i64()).unwrap_or(0);
        println!(
            "{:<20} {:>10} {:>10} {:>7.2}% {:>20}",
            agent,
            total,
            asked,
            rate * 100.0,
            created
        );
    }
    Ok(())
}

async fn cost_spikes(
    bridge: &str,
    limit: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/metrics/cost-spikes?limit={limit}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let rows = v
        .get("spikes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        println!("(no cost spikes archived)");
        return Ok(());
    }
    println!(
        "{:<20} {:>14} {:>14} {:>8} {:>20}",
        "provider", "current", "baseline", "ratio", "created_at_ms"
    );
    for r in rows {
        let provider = r.get("provider").and_then(|x| x.as_str()).unwrap_or("?");
        let cur = r
            .get("current_avg_micros")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let base = r
            .get("baseline_avg_micros")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let ratio = r.get("spike_ratio").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let created = r.get("created_at_ms").and_then(|x| x.as_i64()).unwrap_or(0);
        println!(
            "{:<20} {:>14} {:>14} {:>7.2}x {:>20}",
            provider, cur, base, ratio, created
        );
    }
    Ok(())
}

// ── http ─────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_empty_input_returns_empty_string() {
        assert_eq!(sparkline(&[]), "");
    }

    #[test]
    fn sparkline_all_zero_returns_low_blocks() {
        assert_eq!(sparkline(&[0, 0, 0]), "▁▁▁");
    }

    #[test]
    fn sparkline_uniform_renders_full_blocks_at_top() {
        let s = sparkline(&[7, 7, 7]);
        assert_eq!(s.chars().count(), 3);
        // Every char is the full block.
        for c in s.chars() {
            assert_eq!(c, '█');
        }
    }

    #[test]
    fn sparkline_max_maps_to_full_block_and_zero_to_lowest() {
        let s = sparkline(&[0, 10]);
        let mut it = s.chars();
        assert_eq!(it.next(), Some('▁'));
        assert_eq!(it.next(), Some('█'));
    }

    #[test]
    fn sparkline_uses_all_levels_across_distribution() {
        // Eight evenly-spaced counts should engage every level.
        let s = sparkline(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(s.chars().count(), 8);
        // Last char must be the full block.
        assert_eq!(s.chars().last(), Some('█'));
    }
}
