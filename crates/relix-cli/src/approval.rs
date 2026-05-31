//! `relix approval ...` — RELIX-7.30 PART 1 operator surface.
//!
//! - `approval delivery-status <approval_id>` → prints the
//!   delivery + escalation state for one approval id.
//! - `approval get <approval_id>` → DEFERRED C: prints the
//!   full agent-store approval row (status, decision_note,
//!   authorized_approvers, lifecycle timestamps). Pass
//!   `--json` for a raw JSON dump instead of the
//!   human-readable layout.

use std::time::Duration;

use clap::Subcommand;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

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
    /// DEFERRED C: print the full agent-store approval row for
    /// one approval id. Status is printed on its own line at
    /// the top so an operator can `relix approval get <id> |
    /// head -1` to script around it. Pass `--json` for the raw
    /// JSON dump.
    Get {
        approval_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::DeliveryStatus {
            approval_id,
            bridge,
            raw,
        } => delivery_status(&bridge, &approval_id, raw).await,
        Cmd::Get {
            approval_id,
            bridge,
            json,
        } => get_approval(&bridge, &approval_id, json).await,
    }
}

async fn get_approval(
    bridge: &str,
    approval_id: &str,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if approval_id.trim().is_empty() {
        return Err("approval_id is required".into());
    }
    let url = format!(
        "{}/v1/approval/{}",
        bridge.trim_end_matches('/'),
        urlencode(approval_id)
    );
    let body = http_get(&url).await?;
    if json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode approval row: {e} (body={body})"))?;
    print!("{}", format_approval(&v));
    Ok(())
}

/// DEFERRED C: pure formatter for the `relix approval get`
/// command. Pulled out for testability — the test asserts the
/// status line lands first AND the body carries every
/// documented field. `status: <wire>` always appears on the
/// first line (operators pipe `| head -1` to script around it).
pub fn format_approval(v: &serde_json::Value) -> String {
    let pick_str = |k: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "?".into())
    };
    let pick_str_or = |k: &str, default: &str| -> String {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| default.into())
    };
    let pick_i64 = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let approvers = v
        .get("authorized_approvers")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "(none)".into());
    let mut out = String::new();
    out.push_str(&format!("status: {}\n", pick_str("status")));
    out.push_str(&format!(
        "approval_id:           {}\n",
        pick_str("approval_id")
    ));
    out.push_str(&format!(
        "agent_id:              {}\n",
        pick_str("agent_id")
    ));
    out.push_str(&format!(
        "subject_id:            {}\n",
        pick_str("subject_id")
    ));
    out.push_str(&format!("method:                {}\n", pick_str("method")));
    out.push_str(&format!(
        "capability_category:   {}\n",
        pick_str("capability_category")
    ));
    out.push_str(&format!(
        "reason:                {}\n",
        pick_str_or("reason", "(none)")
    ));
    out.push_str(&format!(
        "requested_at:          {}\n",
        pick_i64("requested_at")
    ));
    out.push_str(&format!(
        "expires_at:            {}\n",
        pick_i64("expires_at")
    ));
    out.push_str(&format!(
        "task_id:               {}\n",
        pick_str_or("task_id", "(none)")
    ));
    out.push_str(&format!("authorized_approvers:  {approvers}\n"));
    let decided_at = v
        .get("decided_at")
        .and_then(|x| x.as_i64())
        .filter(|n| *n != 0);
    if let Some(t) = decided_at {
        out.push_str(&format!("decided_at:            {t}\n"));
        out.push_str(&format!(
            "decided_by:            {}\n",
            pick_str_or("decided_by", "(unknown)")
        ));
        out.push_str(&format!(
            "decision_note:         {}\n",
            pick_str_or("decision_note", "(none)")
        ));
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(status: &str, note: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "status": status,
            "approval_id": "apr_test",
            "agent_id": "agt-1",
            "subject_id": "subj-1",
            "method": "tool.web_read",
            "capability_category": "external_api:read",
            "reason": "test",
            "requested_at": 1_700_000_000_i64,
            "expires_at": 1_700_060_000_i64,
            "decided_at": null,
            "decided_by": null,
            "decision_note": note,
            "task_id": null,
            "authorized_approvers": vec!["subj-op"],
        })
    }

    #[test]
    fn format_approval_prints_status_on_first_line_prominently() {
        let v = fixture("pending", None);
        let out = format_approval(&v);
        let first = out.lines().next().expect("at least one line");
        assert_eq!(first, "status: pending");
    }

    #[test]
    fn format_approval_carries_every_documented_field() {
        let v = fixture("pending", None);
        let out = format_approval(&v);
        for key in [
            "approval_id:",
            "agent_id:",
            "subject_id:",
            "method:",
            "capability_category:",
            "reason:",
            "requested_at:",
            "expires_at:",
            "task_id:",
            "authorized_approvers:",
        ] {
            assert!(out.contains(key), "missing field `{key}` in output:\n{out}");
        }
    }

    #[test]
    fn format_approval_emits_decision_block_only_when_decided() {
        let pending = format_approval(&fixture("pending", None));
        assert!(!pending.contains("decided_at:"));
        let mut decided = fixture("approved", Some("looks fine"));
        decided["decided_at"] = serde_json::json!(1_700_000_500_i64);
        decided["decided_by"] = serde_json::json!("operator-bob");
        let out = format_approval(&decided);
        assert!(out.contains("decided_at:            1700000500"));
        assert!(out.contains("decided_by:            operator-bob"));
        assert!(out.contains("decision_note:         looks fine"));
    }

    #[test]
    fn format_approval_shows_legacy_token_expired_decision_note() {
        let v = fixture(
            "legacy_token_expired",
            Some(
                "legacy_token_expired: opaque approval_token from a pre-SEC-PART-A deployment \
                 cannot be verified by the new HMAC-signed token gate. Retry to mint a fresh \
                 structured token.",
            ),
        );
        // decided_at must be non-zero so the decision block renders.
        let mut v = v;
        v["decided_at"] = serde_json::json!(1_700_000_500_i64);
        let out = format_approval(&v);
        assert!(out.starts_with("status: legacy_token_expired\n"));
        assert!(out.contains("legacy_token_expired:"));
    }
}
