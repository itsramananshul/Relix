//! `relix credentials ...` — RELIX-7.30 PART 2 operator surface.

use std::time::Duration;

use clap::Subcommand;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = "http://127.0.0.1:19791";

#[derive(Subcommand, Debug)]
pub enum Cmd {
    Store {
        #[arg(long)]
        name: String,
        #[arg(long)]
        value: String,
        #[arg(long, default_value = "api_key")]
        kind: String,
        #[arg(long)]
        owner: Option<String>,
        /// Expiry as a unix-millisecond timestamp. Operators
        /// pass `--expires-at-ms` rather than an ISO string so
        /// the CLI stays parser-free.
        #[arg(long)]
        expires_at_ms: Option<i64>,
        /// Rotation interval in seconds. The scheduler emits a
        /// notification every interval until the operator
        /// rotates the value.
        #[arg(long)]
        rotate_every: Option<u64>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    List {
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Rotate {
        #[arg(long)]
        name: String,
        #[arg(long)]
        new_value: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Revoke {
        #[arg(long)]
        name: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Audit {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Store {
            name,
            value,
            kind,
            owner,
            expires_at_ms,
            rotate_every,
            bridge,
            raw,
        } => {
            store(
                &bridge,
                &name,
                &value,
                &kind,
                owner.as_deref(),
                expires_at_ms,
                rotate_every,
                raw,
            )
            .await
        }
        Cmd::List { owner, bridge, raw } => list(&bridge, owner.as_deref(), raw).await,
        Cmd::Rotate {
            name,
            new_value,
            bridge,
            raw,
        } => rotate(&bridge, &name, &new_value, raw).await,
        Cmd::Revoke {
            name,
            reason,
            bridge,
            raw,
        } => revoke(&bridge, &name, reason.as_deref(), raw).await,
        Cmd::Audit {
            name,
            limit,
            bridge,
            raw,
        } => audit(&bridge, &name, limit, raw).await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn store(
    bridge: &str,
    name: &str,
    value: &str,
    kind: &str,
    owner: Option<&str>,
    expires_at_ms: Option<i64>,
    rotate_every: Option<u64>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/credentials", bridge.trim_end_matches('/'));
    let mut payload = serde_json::Map::new();
    payload.insert("name".into(), Value::from(name));
    payload.insert("value".into(), Value::from(value));
    payload.insert("kind".into(), Value::from(kind));
    if let Some(o) = owner {
        payload.insert("owner_agent".into(), Value::from(o));
    }
    if let Some(e) = expires_at_ms {
        payload.insert("expires_at_ms".into(), Value::from(e));
    }
    if let Some(r) = rotate_every {
        payload.insert("rotation_interval_secs".into(), Value::from(r));
    }
    let body = http_post_json(&url, &Value::Object(payload)).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn list(
    bridge: &str,
    owner: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!("{}/v1/credentials", bridge.trim_end_matches('/'));
    if let Some(o) = owner {
        url.push_str(&format!("?owner_agent={}", urlencode(o)));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode list: {e} (body={body})"))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no credentials)");
        return Ok(());
    }
    println!("{:<24} {:<10} {:<14} {:<3} status", "name", "kind", "owner", "ver");
    for r in arr {
        let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("?");
        let kind = r.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
        let owner = r.get("owner_agent").and_then(|x| x.as_str()).unwrap_or("-");
        let ver = r.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
        let revoked = r.get("revoked").and_then(|x| x.as_bool()).unwrap_or(false);
        let status = if revoked { "revoked" } else { "active" };
        println!("{name:<24} {kind:<10} {owner:<14} {ver:<3} {status}");
    }
    Ok(())
}

async fn rotate(
    bridge: &str,
    name: &str,
    new_value: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/rotate",
        bridge.trim_end_matches('/'),
        urlencode(name)
    );
    let body = http_post_json(&url, &serde_json::json!({ "new_value": new_value })).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn revoke(
    bridge: &str,
    name: &str,
    reason: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/revoke",
        bridge.trim_end_matches('/'),
        urlencode(name)
    );
    let mut payload = serde_json::Map::new();
    if let Some(r) = reason {
        payload.insert("reason".into(), Value::from(r));
    }
    let body = http_post_json(&url, &Value::Object(payload)).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn audit(
    bridge: &str,
    name: &str,
    limit: usize,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/audit?limit={}",
        bridge.trim_end_matches('/'),
        urlencode(name),
        limit
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode audit: {e} (body={body})"))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no audit rows)");
        return Ok(());
    }
    for row in arr {
        let ts = row
            .get("timestamp_ms")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let ev = row.get("event").and_then(|x| x.as_str()).unwrap_or("?");
        let actor = row.get("actor").and_then(|x| x.as_str()).unwrap_or("-");
        let details = row.get("details").and_then(|x| x.as_str()).unwrap_or("");
        println!("{ts:>13}  {ev:<10}  by {actor:<14}  {details}");
    }
    Ok(())
}

fn print_summary(body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let v: Value = serde_json::from_str(body)?;
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("?");
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
    let ver = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    let revoked = v.get("revoked").and_then(|x| x.as_bool()).unwrap_or(false);
    println!(
        "{name} (kind={kind} version={ver} status={})",
        if revoked { "revoked" } else { "active" }
    );
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
