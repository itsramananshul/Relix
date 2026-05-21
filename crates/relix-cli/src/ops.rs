//! PH-WAVE2L — `relix-cli ops` operator-facing CLI snapshots.
//!
//! Today: a single subcommand `providers-health` that hits the
//! bridge's `GET /v1/providers/health` (PH-WAVE2K) and pretty-
//! prints a one-screen ops view. Useful for status-line scripts,
//! on-call triage, and tmux dashboards.
//!
//! Future: this module is the right home for additional ops
//! views — `relix-cli ops stuck`, `relix-cli ops thrash`, etc. —
//! all of which mirror the per-provider snapshot pattern.

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
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::ProvidersHealth { bridge, json } => providers_health(&bridge, json).await,
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
}
