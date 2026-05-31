//! `relix-cli confidence ...` — RELIX-7.19 operator surface.
//!
//! Three subcommands, each a thin HTTP forwarder onto the
//! `/v1/confidence/*` bridge endpoints:
//!
//! - `confidence policies` — list configured policies.
//! - `confidence history --agent X --method ai.chat` — rolling
//!   window stats for one (agent, method) pair.
//! - `confidence reset --agent X [--method ai.chat]` — clear the
//!   rolling window for one pair OR every method on one agent.
//!
//! Every subcommand accepts `--bridge <url>` (defaults to
//! `http://127.0.0.1:19791`) and the read paths accept
//! `--raw` to dump the JSON body verbatim.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print every configured confidence policy.
    Policies {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Print the rolling-window snapshot for one (agent,
    /// method) pair: call count, error count, error rate,
    /// p50/p95/p99 latency, average confidence.
    History {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        method: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Clear the rolling-window state. Omit `--method` to clear
    /// every method on that agent.
    Reset {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        method: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Policies { bridge, raw } => policies(&bridge, raw).await,
        Cmd::History {
            agent,
            method,
            bridge,
            raw,
        } => history(&bridge, &agent, &method, raw).await,
        Cmd::Reset {
            agent,
            method,
            bridge,
            raw,
        } => reset(&bridge, &agent, method.as_deref(), raw).await,
    }
}

async fn policies(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/confidence/policies", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let policies: Vec<PolicyView> =
        serde_json::from_str(&body).map_err(|e| format!("decode policies: {e} (body={body})"))?;
    if policies.is_empty() {
        println!("(no confidence policies configured)");
        return Ok(());
    }
    for p in policies {
        let low = format_action(p.low_action.as_ref());
        let critical = format_action(p.critical_action.as_ref());
        println!(
            "{cap:<28} low<={low_t:.2} ({low}) critical<={crit_t:.2} ({critical})",
            cap = p.capability,
            low_t = p.low_threshold,
            crit_t = p.critical_threshold,
        );
    }
    Ok(())
}

async fn history(
    bridge: &str,
    agent: &str,
    method: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if agent.trim().is_empty() {
        return Err("--agent is required".into());
    }
    if method.trim().is_empty() {
        return Err("--method is required".into());
    }
    let url = format!(
        "{}/v1/confidence/history/{}?method={}",
        bridge.trim_end_matches('/'),
        urlencode(agent),
        urlencode(method),
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let snap: HistoryView =
        serde_json::from_str(&body).map_err(|e| format!("decode history: {e} (body={body})"))?;
    println!("agent:           {}", snap.agent);
    println!("method:          {}", snap.method);
    println!("call_count:      {}", snap.call_count);
    println!("error_count:     {}", snap.error_count);
    println!("error_rate:      {:.4}", snap.error_rate);
    println!("p50_latency_ms:  {}", snap.p50_latency_ms);
    println!("p95_latency_ms:  {}", snap.p95_latency_ms);
    println!("p99_latency_ms:  {}", snap.p99_latency_ms);
    println!("avg_confidence:  {:.4}", snap.avg_confidence);
    Ok(())
}

async fn reset(
    bridge: &str,
    agent: &str,
    method: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if agent.trim().is_empty() {
        return Err("--agent is required".into());
    }
    let url = format!("{}/v1/confidence/reset", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("agent".into(), Value::from(agent.to_string()));
    if let Some(m) = method
        && !m.trim().is_empty()
    {
        body.insert("method".into(), Value::from(m.to_string()));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp)
        .map_err(|e| format!("decode reset response: {e} (body={resp})"))?;
    if let Some(b) = v.get("cleared_pair").and_then(Value::as_bool) {
        println!(
            "cleared_pair:  {b}\nagent:         {}\nmethod:        {}",
            v.get("agent").and_then(Value::as_str).unwrap_or(""),
            v.get("method").and_then(Value::as_str).unwrap_or(""),
        );
    } else if let Some(n) = v.get("cleared_pairs").and_then(Value::as_u64) {
        println!(
            "cleared_pairs: {n}\nagent:         {}",
            v.get("agent").and_then(Value::as_str).unwrap_or(""),
        );
    } else {
        println!("{resp}");
    }
    Ok(())
}

fn format_action(action: Option<&Value>) -> String {
    let Some(v) = action else {
        return "pass".into();
    };
    if v.is_string() {
        // shouldn't happen with the wire shape but be defensive
        return v.as_str().unwrap_or("?").to_string();
    }
    if let Some(obj) = v.as_object() {
        // wire shape is { "Retry": { ... } } / { "Escalate": {..} } / "Pass"
        if let Some((kind, inner)) = obj.iter().next() {
            match kind.as_str() {
                "Retry" => {
                    let r = inner
                        .get("max_retries")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    return format!("retry x{r}");
                }
                "Escalate" => {
                    let target = inner
                        .get("escalate_to")
                        .and_then(Value::as_str)
                        .unwrap_or("?");
                    return format!("escalate -> {target}");
                }
                "SafeDefault" => return "safe_default".into(),
                "Alert" => return "alert".into(),
                "Abort" => return "abort".into(),
                other => return other.to_string(),
            }
        }
    }
    // Bare string variant Pass.
    if v == "Pass" {
        return "pass".into();
    }
    v.to_string()
}

// ── wire types ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct PolicyView {
    capability: String,
    low_threshold: f32,
    critical_threshold: f32,
    #[serde(default)]
    low_action: Option<Value>,
    #[serde(default)]
    critical_action: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HistoryView {
    agent: String,
    method: String,
    call_count: u64,
    error_count: u64,
    error_rate: f32,
    p50_latency_ms: u64,
    p95_latency_ms: u64,
    p99_latency_ms: u64,
    avg_confidence: f32,
}

// ── http helpers ────────────────────────────────────────

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

async fn http_post_json(url: &str, payload: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .post(url)
        .json(payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_action_handles_every_documented_variant() {
        assert_eq!(format_action(None), "pass");
        assert_eq!(
            format_action(Some(
                &serde_json::json!({"Retry":{"max_retries":3, "retry_delay_ms":500}})
            )),
            "retry x3"
        );
        assert_eq!(
            format_action(Some(
                &serde_json::json!({"Escalate":{"escalate_to":"ai.chat.premium"}})
            )),
            "escalate -> ai.chat.premium"
        );
        assert_eq!(
            format_action(Some(
                &serde_json::json!({"SafeDefault":{"default_value":""}})
            )),
            "safe_default"
        );
        assert_eq!(
            format_action(Some(&serde_json::json!({"Alert":{"alert_message":"x"}}))),
            "alert"
        );
        assert_eq!(
            format_action(Some(&serde_json::json!({"Abort":{"abort_message":"y"}}))),
            "abort"
        );
    }

    #[test]
    fn urlencode_handles_special_characters() {
        assert_eq!(urlencode("ai.chat"), "ai.chat");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("foo bar"), "foo%20bar");
    }
}
