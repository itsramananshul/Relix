//! `relix approval ...` — RELIX-7.30 PART 1 operator surface.
//!
//! - `approval delivery-status <approval_id>` → prints the
//!   delivery + escalation state for one approval id.

use std::time::Duration;

use clap::Subcommand;

const DEFAULT_BRIDGE: &str = "http://127.0.0.1:19791";

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the delivery + escalation state for one approval
    /// id (channel routed to, whether escalation fired, etc.).
    DeliveryStatus {
        approval_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::DeliveryStatus {
            approval_id,
            bridge,
            raw,
        } => delivery_status(&bridge, &approval_id, raw).await,
    }
}

async fn delivery_status(
    bridge: &str,
    approval_id: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if approval_id.trim().is_empty() {
        return Err("approval_id is required".into());
    }
    let url = format!(
        "{}/v1/approval/{}/delivery",
        bridge.trim_end_matches('/'),
        urlencode(approval_id)
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode delivery status: {e} (body={body})"))?;
    let pick_str = |k: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into())
    };
    let pick_i64 = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let pick_bool = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
    println!("approval_id:        {}", pick_str("approval_id"));
    println!("agent:              {}", pick_str("agent_name"));
    println!("capability:         {}", pick_str("capability"));
    println!("status:             {}", pick_str("status"));
    println!("delivery_channel:   {}", pick_str("delivery_channel"));
    println!("delivered_at_ms:    {}", pick_i64("delivered_at_ms"));
    println!("escalated:          {}", pick_bool("escalated"));
    println!(
        "escalation_channel: {}",
        v.get("escalation_channel")
            .and_then(|x| x.as_str())
            .unwrap_or("(none)")
    );
    println!("escalated_at_ms:    {}", pick_i64("escalated_at_ms"));
    if pick_str("status") != "pending" {
        println!("decision:           {}", pick_str("decision"));
        println!(
            "decision_note:      {}",
            v.get("decision_note")
                .and_then(|x| x.as_str())
                .unwrap_or("(none)")
        );
        println!("decided_at_ms:      {}", pick_i64("decided_at_ms"));
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
