//! `relix reasoning ...` — RELIX-7.29 PART 5 operator surface.
//!
//! One subcommand: `reasoning status` prints a four-line
//! summary of every §7.29 component's configured-or-not state
//! plus the live counters (or `--raw` for the full JSON body).

use std::time::Duration;

use clap::Subcommand;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the §7.29 reasoning-engine status.
    Status {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Status { bridge, raw } => status(&bridge, raw).await,
    }
}

async fn status(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/reasoning/status", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode status: {e} (body={body})"))?;

    // Routing
    let r_enabled = bool_at(&v, "/routing/enabled");
    println!(
        "routing:           {}",
        if r_enabled {
            describe_routing(&v)
        } else {
            "disabled".into()
        }
    );

    // Self-consistency
    let sc_enabled = bool_at(&v, "/self_consistency/enabled");
    let sc_sample_count = u64_at(&v, "/self_consistency/config/sample_count");
    let sc_min = f64_at(&v, "/self_consistency/config/min_score_to_enable");
    let sc_triggers = u64_at(&v, "/self_consistency/stats/trigger_count");
    let sc_avg = f64_at(&v, "/self_consistency/stats/average_score");
    if sc_enabled {
        println!(
            "self_consistency:  enabled (samples={sc_sample_count}, trigger<={sc_min:.2}, fired {sc_triggers}x, avg={sc_avg:.2})"
        );
    } else {
        println!("self_consistency:  disabled");
    }

    // Belief state
    let b_enabled = bool_at(&v, "/belief_state/enabled");
    let b_sessions = u64_at(&v, "/belief_state/tracked_sessions");
    let b_max = u64_at(&v, "/belief_state/config/max_beliefs");
    let b_inject = bool_at(&v, "/belief_state/config/inject_into_prompt");
    if b_enabled {
        println!(
            "belief_state:      enabled (tracked_sessions={b_sessions}, max_beliefs={b_max}, inject={b_inject})"
        );
    } else {
        println!("belief_state:      disabled");
    }

    // Judge
    let j_enabled = bool_at(&v, "/judge/enabled");
    let j_threshold = f64_at(&v, "/judge/config/judge_threshold");
    let j_timeout = u64_at(&v, "/judge/config/max_judge_latency_ms");
    let j_proceed = u64_at(&v, "/judge/stats/proceed_count");
    let j_modify = u64_at(&v, "/judge/stats/modify_count");
    let j_block = u64_at(&v, "/judge/stats/block_count");
    let j_timeout_count = u64_at(&v, "/judge/stats/timeout_count");
    if j_enabled {
        println!(
            "judge:             enabled (threshold<{j_threshold:.2}, timeout={j_timeout}ms, proceed={j_proceed}, modify={j_modify}, block={j_block}, timed_out={j_timeout_count})"
        );
    } else {
        println!("judge:             disabled");
    }

    Ok(())
}

fn describe_routing(v: &serde_json::Value) -> String {
    let tiers = v.pointer("/routing/config/tiers");
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = tiers {
        for k in ["simple", "medium", "complex"] {
            if let Some(target) = t.pointer(&format!("/{k}"))
                && !target.is_null()
            {
                let provider = target
                    .get("provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("?");
                let model = target.get("model").and_then(|x| x.as_str()).unwrap_or("?");
                parts.push(format!("{k}={provider}:{model}"));
            }
        }
    }
    if parts.is_empty() {
        "enabled (no tiers configured)".into()
    } else {
        format!("enabled ({})", parts.join(", "))
    }
}

fn bool_at(v: &serde_json::Value, ptr: &str) -> bool {
    v.pointer(ptr).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn u64_at(v: &serde_json::Value, ptr: &str) -> u64 {
    v.pointer(ptr).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn f64_at(v: &serde_json::Value, ptr: &str) -> f64 {
    v.pointer(ptr).and_then(|x| x.as_f64()).unwrap_or(0.0)
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
