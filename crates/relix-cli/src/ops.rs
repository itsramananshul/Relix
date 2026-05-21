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
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::ProvidersHealth { bridge, json } => providers_health(&bridge, json).await,
        Cmd::Capabilities {
            bridge,
            filter,
            json,
        } => capabilities(&bridge, &filter, json).await,
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
}
