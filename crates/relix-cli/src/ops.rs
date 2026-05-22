//! `relix-cli ops` — operator-facing CLI snapshots.
//!
//! Subcommands:
//! - `providers-health` (PH-WAVE2L) — hits the bridge's
//!   `/v1/providers/health` and pretty-prints aggregate +
//!   per-provider state.
//! - `capabilities` (PH-DASH3-CLI) — hits `/v1/topology` and
//!   pretty-prints every capability the bridge has discovered,
//!   mirroring the dashboard's PH-DASH3 explorer for terminal
//!   operators.
//!
//! All subcommands are one-shot HTTP-against-bridge — useful
//! for status-line scripts, on-call triage, and tmux dashboards.

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the consolidated provider-health snapshot from the
    /// bridge. Shows aggregate counters across the AI stack
    /// (cooldowns active, rate-limit hits in 5min / 1h, lifetime
    /// success / fail counts) plus a per-provider one-liner.
    ProvidersHealth {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Raw JSON instead of the pretty one-screen summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List every capability the bridge has discovered across
    /// every peer in the cached topology. Mirrors the dashboard's
    /// PH-DASH3 capability explorer for terminal operators.
    /// Source is `/v1/topology` — each peer's methods[]
    /// aggregated.
    Capabilities {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Filter by capability prefix (e.g. `tool.web`).
        /// Substring match, case-insensitive. Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// H6 stuck-running task projection from /v1/tasks/stuck.
    /// Shows tasks that have been `running` longer than
    /// `--threshold-secs` (default 300) without a deadline.
    Stuck {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Stuck threshold in seconds (passed to the bridge).
        #[arg(long, default_value_t = 300i64)]
        threshold_secs: i64,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Preview the HealthAwareRouter's pick for a candidate
    /// list (PH-ROUTER-PREVIEW). Hits POST
    /// /v1/providers/route_test against current cached health.
    /// Does NOT send any chat call.
    RouteTest {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Comma-separated candidate list (e.g.
        /// `openai,anthropic`). Order matters — the router uses
        /// it for stable tie-breaking.
        #[arg(long)]
        candidates: String,
        /// Raw JSON instead of the pretty summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// W2-006c mirror: per-capability invocation + latency
    /// counters from a peer's DispatchBridge, fetched via
    /// `GET /v1/dispatch/stats`. Sorted by mean latency desc —
    /// the slowest capability shows first. Lifetime counters,
    /// reset on peer restart.
    DispatchStats {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Recent cross-task events from /v1/tasks/events/recent.
    /// Mirrors the dashboard firehose for terminal operators
    /// — shows the H2 one-line summary projection per row.
    Events {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Page limit (server caps at 500).
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
        /// Filter by event_type substring (e.g.
        /// `task.retry`). Empty = all.
        #[arg(long, default_value = "")]
        filter: String,
        /// Raw JSON instead of the table view.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::ProvidersHealth { bridge, json } => providers_health(&bridge, json).await,
        Cmd::Capabilities {
            bridge,
            filter,
            json,
        } => capabilities(&bridge, &filter, json).await,
        Cmd::Stuck {
            bridge,
            threshold_secs,
            json,
        } => stuck(&bridge, threshold_secs, json).await,
        Cmd::Events {
            bridge,
            limit,
            filter,
            json,
        } => events(&bridge, limit, &filter, json).await,
        Cmd::RouteTest {
            bridge,
            candidates,
            json,
        } => route_test(&bridge, &candidates, json).await,
        Cmd::DispatchStats { bridge, peer, json } => dispatch_stats(&bridge, &peer, json).await,
    }
}

async fn providers_health(bridge: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/providers/health", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let h: HealthResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    print_pretty(&h);
    Ok(())
}

fn print_pretty(h: &HealthResponse) {
    // Aggregate header.
    let a = &h.aggregate;
    let total_ok = a.success_request_count;
    let total_fail = a.failed_request_count;
    let reliability = (total_ok * 100)
        .checked_div(total_ok + total_fail)
        .unwrap_or(0);
    println!(
        "providers={count}  cooldowns_active={cd}  quarantined={q}",
        count = h.providers.len(),
        cd = a.cooldowns_active,
        q = a.quarantined,
    );
    println!(
        "rate_limit_hits  5min={rl5}  1h={rl1h}",
        rl5 = a.rate_limit_hits_5min,
        rl1h = a.rate_limit_hits_1h,
    );
    println!(
        "lifetime  ok={ok}  fail={fail}  reliability={r}%",
        ok = total_ok,
        fail = total_fail,
        r = reliability,
    );
    println!();
    // Per-provider table.
    let name_h = "provider";
    let cfg_h = "cfg";
    let cd_h = "cooldown";
    let last_h = "last_fail";
    let rl_h = "rl 5m/1h";
    println!("{name_h:<14}  {cfg_h:<5}  {cd_h:<12}  {last_h:<24}  {rl_h}");
    for p in &h.providers {
        let name = &p.name;
        let cfg = if p.configured { "yes" } else { "no" };
        let cd = match p.cooldown_until {
            Some(c) => {
                let rem = c - now_secs();
                if rem > 0 {
                    let auto = if p.quarantined_at.is_none() {
                        "auto"
                    } else {
                        "op"
                    };
                    format!("{rem}s ({auto})")
                } else {
                    "-".to_string()
                }
            }
            None => "-".to_string(),
        };
        let last = match (p.last_failure_reason.as_deref(), p.last_failure_at) {
            (Some(r), Some(t)) => format!("{r} @ {t}"),
            _ => "-".to_string(),
        };
        let rl = format!("{}/{}", p.rate_limit_hits_5min, p.rate_limit_hits_1h);
        println!("{name:<14}  {cfg:<5}  {cd:<12}  {last:<24}  {rl}",);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn capabilities(
    bridge: &str,
    filter: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/topology", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let topo: TopologyResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for p in &topo.peers {
        let alias = p.alias.clone().unwrap_or_else(|| "(none)".to_string());
        for m in &p.methods {
            if !needle.is_empty() && !m.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            rows.push((
                m.clone(),
                alias.clone(),
                p.node_type.clone(),
                p.freshness.clone(),
            ));
        }
    }
    rows.sort();
    let total_methods: usize = topo.peers.iter().map(|p| p.methods.len()).sum();
    println!(
        "capabilities  shown={shown}  total={total}  peers={peers}",
        shown = rows.len(),
        total = total_methods,
        peers = topo.peers.len(),
    );
    if rows.is_empty() {
        if needle.is_empty() {
            println!("(no capabilities discovered yet)");
        } else {
            println!("(no capabilities match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let m_h = "capability";
    let a_h = "alias";
    let t_h = "node_type";
    let f_h = "freshness";
    println!("{m_h:<36}  {a_h:<14}  {t_h:<14}  {f_h}");
    for (method, alias, node_type, fresh) in &rows {
        println!("{method:<36}  {alias:<14}  {node_type:<14}  {fresh}",);
    }
    Ok(())
}

async fn stuck(
    bridge: &str,
    threshold_secs: i64,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/tasks/stuck?threshold_secs={}",
        bridge.trim_end_matches('/'),
        threshold_secs.max(0),
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let s: StuckResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    println!(
        "stuck={count}  threshold_secs={threshold}",
        count = s.count,
        threshold = s.threshold_secs.unwrap_or(threshold_secs),
    );
    if s.items.is_empty() {
        println!("(no stuck tasks)");
        return Ok(());
    }
    println!();
    let id_h = "task_id";
    let title_h = "title";
    let age_h = "age";
    println!("{id_h:<36}  {title_h:<32}  {age_h}");
    for it in &s.items {
        println!(
            "{id:<36}  {title:<32}  {age}s",
            id = it.task_id,
            title = it.title,
            age = it.age_secs,
        );
    }
    Ok(())
}

async fn events(
    bridge: &str,
    limit: usize,
    filter: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cap = limit.clamp(1, 500);
    let url = format!(
        "{}/v1/tasks/events/recent?limit={}",
        bridge.trim_end_matches('/'),
        cap,
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: EventsResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    let needle = filter.trim().to_ascii_lowercase();
    let filtered: Vec<&EventRow> = resp
        .items
        .iter()
        .filter(|r| needle.is_empty() || r.event_type.to_ascii_lowercase().contains(&needle))
        .collect();
    println!(
        "events  shown={shown}  fetched={fetched}  next_cursor={cursor}",
        shown = filtered.len(),
        fetched = resp.items.len(),
        cursor = resp.next_cursor,
    );
    if filtered.is_empty() {
        if needle.is_empty() {
            println!("(no events)");
        } else {
            println!("(no events match filter \"{needle}\")");
        }
        return Ok(());
    }
    println!();
    let ev_h = "event_type";
    let tid_h = "task_id";
    let id_h = "id";
    let sum_h = "summary";
    println!("{ev_h:<28}  {tid_h:<10}  {id_h:>6}  {sum_h}");
    for r in &filtered {
        let short = if r.task_id.len() > 8 {
            &r.task_id[..8]
        } else {
            &r.task_id
        };
        let sum = if r.summary.is_empty() {
            r.payload.as_str()
        } else {
            r.summary.as_str()
        };
        println!(
            "{et:<28}  {tid:<10}  {id:>6}  {sum}",
            et = r.event_type,
            tid = short,
            id = r.event_id,
            sum = sum,
        );
    }
    Ok(())
}

async fn route_test(
    bridge: &str,
    candidates_csv: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let candidates: Vec<String> = candidates_csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if candidates.is_empty() {
        return Err("--candidates required (comma-separated, non-empty)".into());
    }
    let url = format!("{}/v1/providers/route_test", bridge.trim_end_matches('/'));
    let body_in = serde_json::json!({ "candidates": candidates }).to_string();
    let body = http_post_json(&url, &body_in).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let resp: RouteTestResp = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    println!("router={r}  chosen={c}", r = resp.router, c = resp.chosen,);
    println!("reasoning: {}", resp.reasoning);
    println!();
    let name_h = "candidate";
    let score_h = "score";
    let elig_h = "eligibility";
    let why_h = "why";
    println!("{name_h:<14}  {score_h:>5}  {elig_h:<12}  {why_h}");
    for c in &resp.candidates {
        println!(
            "{n:<14}  {s:>5.2}  {e:<12}  {w}",
            n = c.name,
            s = c.score,
            e = c.eligibility,
            w = c.why,
        );
    }
    Ok(())
}

async fn http_post_json(url: &str, body: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct RouteTestResp {
    #[serde(default)]
    router: String,
    #[serde(default)]
    chosen: String,
    #[serde(default)]
    reasoning: String,
    #[serde(default)]
    candidates: Vec<RouteTestCandidate>,
}

#[derive(Debug, Deserialize)]
struct RouteTestCandidate {
    #[serde(default)]
    name: String,
    #[serde(default)]
    score: f32,
    #[serde(default)]
    eligibility: String,
    #[serde(default)]
    why: String,
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    #[serde(default)]
    items: Vec<EventRow>,
    #[serde(default)]
    next_cursor: i64,
}

#[derive(Debug, Deserialize)]
struct EventRow {
    #[serde(default)]
    task_id: String,
    #[serde(default)]
    event_id: i64,
    #[serde(default)]
    event_type: String,
    #[serde(default)]
    payload: String,
    #[serde(default)]
    summary: String,
}

#[derive(Debug, Deserialize)]
struct StuckResponse {
    #[serde(default)]
    items: Vec<StuckItem>,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    threshold_secs: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StuckItem {
    task_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    age_secs: i64,
}

#[derive(Debug, Deserialize)]
struct TopologyResponse {
    #[serde(default)]
    peers: Vec<TopologyPeer>,
}

#[derive(Debug, Deserialize)]
struct TopologyPeer {
    #[serde(default)]
    alias: Option<String>,
    node_type: String,
    #[serde(default)]
    methods: Vec<String>,
    #[serde(default)]
    freshness: String,
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

// W2-006c CLI mirror: GET /v1/dispatch/stats?peer=...

#[derive(Debug, Deserialize)]
struct DispatchStatsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    rows: Vec<DispatchStatsRow>,
    #[serde(default)]
    count: usize,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)] // last_invoked_at / last_error_at preserved for future "stale" detection
struct DispatchStatsRow {
    #[serde(default)]
    method: String,
    #[serde(default)]
    invocations: u64,
    #[serde(default)]
    errors: u64,
    #[serde(default)]
    denied: u64,
    #[serde(default)]
    unknown_method: u64,
    #[serde(default)]
    last_invoked_at: i64,
    #[serde(default)]
    last_error_at: Option<i64>,
    #[serde(default)]
    latency_samples: u64,
    #[serde(default)]
    last_elapsed_ms: u64,
    #[serde(default)]
    max_elapsed_ms: u64,
    #[serde(default)]
    mean_elapsed_ms: u64,
    /// W2-006d: recent per-call latencies ring (oldest-first,
    /// capped at 32 by the runtime). Empty when the responder
    /// is an older peer that doesn't ship the column.
    #[serde(default)]
    recent_latencies: Vec<u32>,
}

/// W2-006d: render a ring of latency samples as a Unicode
/// block-character sparkline. Heights normalize to the ring's
/// own max so a 5ms-mean method and a 2000ms-mean method both
/// render legibly side-by-side.
fn ascii_sparkline(samples: &[u32]) -> String {
    if samples.is_empty() {
        return "-".to_string();
    }
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = (*samples.iter().max().unwrap_or(&1)).max(1);
    samples
        .iter()
        .map(|&v| {
            // Map [0..=max] → BARS index. f64 keeps the
            // mapping stable across the full u32 range
            // without integer overflow.
            let idx = ((v as f64 / max as f64) * (BARS.len() - 1) as f64).round() as usize;
            BARS[idx.min(BARS.len() - 1)]
        })
        .collect()
}

async fn dispatch_stats(
    bridge: &str,
    peer: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/dispatch/stats?peer={peer}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if json {
        print!("{body}");
        if !body.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    let parsed: DispatchStatsResp = serde_json::from_str(&body)
        .map_err(|e| format!("decode /v1/dispatch/stats body: {e} (body={body})"))?;
    if parsed.rows.is_empty() {
        println!(
            "(no dispatch activity on peer '{p}' — count={c})",
            p = parsed.peer,
            c = parsed.count
        );
        return Ok(());
    }
    // Sort by mean elapsed desc (tied: invocations desc).
    let mut rows = parsed.rows;
    rows.sort_by(|a, b| {
        b.mean_elapsed_ms
            .cmp(&a.mean_elapsed_ms)
            .then_with(|| b.invocations.cmp(&a.invocations))
    });
    let m_h = "method";
    let i_h = "invocs";
    let e_h = "errs";
    let mean_h = "mean";
    let max_h = "max";
    let last_h = "last";
    let samples_h = "samples";
    let trend_h = "trend";
    println!(
        "{m_h:<36}  {i_h:>7}  {e_h:>5}  {mean_h:>6}  {max_h:>6}  {last_h:>6}  {samples_h:>7}  {trend_h}",
    );
    for r in &rows {
        let method = truncate(&r.method, 36);
        let errs = r.errors + r.denied + r.unknown_method;
        let trend = ascii_sparkline(&r.recent_latencies);
        println!(
            "{method:<36}  {invocs:>7}  {errs:>5}  {mean:>5}ms  {max:>5}ms  {last:>5}ms  {samples:>7}  {trend}",
            method = method,
            invocs = r.invocations,
            errs = errs,
            mean = r.mean_elapsed_ms,
            max = r.max_elapsed_ms,
            last = r.last_elapsed_ms,
            samples = r.latency_samples,
            trend = trend,
        );
    }
    println!("count={}", parsed.count);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// Loose deserializers — we accept whatever the bridge sends and
// don't fail on additional fields. Same posture as topology.rs's
// HealthResponse.

#[derive(Debug, Deserialize)]
struct HealthResponse {
    providers: Vec<ProviderStatus>,
    aggregate: Aggregate,
}

#[derive(Debug, Deserialize, Default)]
struct Aggregate {
    #[serde(default)]
    cooldowns_active: u64,
    #[serde(default)]
    quarantined: u64,
    #[serde(default)]
    rate_limit_hits_5min: u64,
    #[serde(default)]
    rate_limit_hits_1h: u64,
    #[serde(default)]
    failed_request_count: u64,
    #[serde(default)]
    success_request_count: u64,
}

#[derive(Debug, Deserialize)]
struct ProviderStatus {
    name: String,
    #[serde(default)]
    configured: bool,
    #[serde(default)]
    cooldown_until: Option<i64>,
    #[serde(default)]
    quarantined_at: Option<i64>,
    #[serde(default)]
    last_failure_reason: Option<String>,
    #[serde(default)]
    last_failure_at: Option<i64>,
    #[serde(default)]
    rate_limit_hits_5min: u64,
    #[serde(default)]
    rate_limit_hits_1h: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_health_body() {
        let body = r#"{
            "providers": [
                {
                    "name": "openai",
                    "configured": true,
                    "rate_limit_hits_5min": 2,
                    "rate_limit_hits_1h": 5,
                    "last_failure_reason": "rate-limit",
                    "last_failure_at": 1700000000
                },
                {"name": "anthropic", "configured": false}
            ],
            "aggregate": {
                "cooldowns_active": 1,
                "quarantined": 0,
                "rate_limit_hits_5min": 2,
                "rate_limit_hits_1h": 5,
                "failed_request_count": 9,
                "success_request_count": 41
            }
        }"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert_eq!(h.providers.len(), 2);
        assert_eq!(h.providers[0].rate_limit_hits_5min, 2);
        assert_eq!(h.aggregate.cooldowns_active, 1);
        assert_eq!(h.aggregate.failed_request_count, 9);
    }

    #[test]
    fn parse_empty_providers() {
        let body = r#"{"providers":[], "aggregate":{}}"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert!(h.providers.is_empty());
        assert_eq!(h.aggregate.cooldowns_active, 0);
    }

    #[test]
    fn parse_events_response() {
        let body = r#"{
            "items": [
                {"task_id": "abc123",
                 "event_id": 5,
                 "event_type": "task.retry_requested",
                 "payload": "raw payload",
                 "summary": "[retry] requested (#2/5)"}
            ],
            "next_cursor": 5
        }"#;
        let r: EventsResponse = serde_json::from_str(body).unwrap();
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.next_cursor, 5);
        assert_eq!(r.items[0].summary, "[retry] requested (#2/5)");
    }

    #[test]
    fn parse_route_test_response() {
        let body = r#"{
            "router": "health-aware",
            "chosen": "openai",
            "reasoning": "health-aware: chose openai (success_ratio=0.95) from 2 eligible of 2 total",
            "chosen_at": 1700000000,
            "candidates": [
                {"name": "openai", "score": 0.95, "eligibility": "eligible", "why": "chosen (healthy success_ratio=0.95)"},
                {"name": "anthropic", "score": 0.42, "eligibility": "eligible", "why": "considered (healthy success_ratio=0.84)"}
            ]
        }"#;
        let r: RouteTestResp = serde_json::from_str(body).unwrap();
        assert_eq!(r.router, "health-aware");
        assert_eq!(r.chosen, "openai");
        assert_eq!(r.candidates.len(), 2);
        assert!((r.candidates[0].score - 0.95).abs() < 1e-3);
        assert_eq!(r.candidates[1].eligibility, "eligible");
    }

    #[test]
    fn parse_stuck_response() {
        let body = r#"{
            "items": [
                {"task_id": "abcd1234abcd1234abcd1234abcd1234",
                 "title": "long-running task",
                 "started_at": 1700000000,
                 "age_secs": 1234}
            ],
            "count": 1,
            "threshold_secs": 300
        }"#;
        let s: StuckResponse = serde_json::from_str(body).unwrap();
        assert_eq!(s.count, 1);
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.items[0].age_secs, 1234);
    }

    #[test]
    fn parse_topology_for_capabilities() {
        // PH-DASH3-CLI: minimal topology body the capabilities
        // subcommand needs. Aliases optional; methods required;
        // freshness propagated.
        let body = r#"{
            "peers": [
                {
                    "alias": "tool",
                    "node_id": "abc",
                    "node_type": "tool",
                    "node_name": "t",
                    "manifest_version": 1,
                    "capability_count": 2,
                    "methods": ["tool.web_fetch", "tool.web_search"],
                    "last_refreshed_at": 1,
                    "last_refreshed_secs_ago": 5,
                    "freshness": "fresh"
                }
            ],
            "generated_at": 0
        }"#;
        let t: TopologyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(t.peers.len(), 1);
        assert_eq!(t.peers[0].methods.len(), 2);
        assert_eq!(t.peers[0].freshness, "fresh");
    }

    // ── W2-006d: dispatch_stats CLI sparkline ──────────────────

    #[test]
    fn ascii_sparkline_empty_returns_dash() {
        assert_eq!(ascii_sparkline(&[]), "-");
    }

    #[test]
    fn ascii_sparkline_flat_renders_low_bars() {
        // All-equal samples normalize to the max bar.
        let s = ascii_sparkline(&[5, 5, 5, 5]);
        assert_eq!(s.chars().count(), 4);
        // Every bar at full height since v == max for all.
        assert!(s.chars().all(|c| c == '█'));
    }

    #[test]
    fn ascii_sparkline_renders_one_char_per_sample() {
        let s = ascii_sparkline(&[1, 10, 5, 20, 8]);
        assert_eq!(s.chars().count(), 5);
        // The peak (20) should map to the tallest bar.
        assert!(s.contains('█'));
    }

    #[test]
    fn dispatch_stats_row_parses_recent_latencies() {
        // Forward-compat: the JSON may or may not include
        // recent_latencies. Both shapes parse.
        let with_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12,
            "recent_latencies": [10, 15, 12, 25, 8]
        }"#;
        let r: DispatchStatsRow = serde_json::from_str(with_field).unwrap();
        assert_eq!(r.recent_latencies, vec![10, 15, 12, 25, 8]);

        let without_field = r#"{
            "method": "tool.web_fetch",
            "invocations": 5, "errors": 0, "denied": 0, "unknown_method": 0,
            "last_invoked_at": 100, "latency_samples": 5,
            "last_elapsed_ms": 10, "max_elapsed_ms": 25, "mean_elapsed_ms": 12
        }"#;
        let r2: DispatchStatsRow = serde_json::from_str(without_field).unwrap();
        assert!(r2.recent_latencies.is_empty());
    }
}
