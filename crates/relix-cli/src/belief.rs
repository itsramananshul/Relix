//! `relix belief ...` — RELIX-7.29 PART 3 operator surface.
//!
//! Two subcommands forwarded onto `/v1/belief/<session_id>`:
//!
//! - `belief show --session <id> [--subject <id>]`
//! - `belief reset --session <id> [--subject <id>]`

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the current belief block for a session.
    Show {
        #[arg(long)]
        session: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Clear the belief block for a session.
    Reset {
        #[arg(long)]
        session: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Show {
            session,
            subject,
            bridge,
            raw,
        } => show(&bridge, &session, subject.as_deref(), raw).await,
        Cmd::Reset {
            session,
            subject,
            bridge,
            raw,
        } => reset(&bridge, &session, subject.as_deref(), raw).await,
    }
}

async fn show(
    bridge: &str,
    session: &str,
    subject: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if session.trim().is_empty() {
        return Err("--session is required".into());
    }
    let mut url = format!(
        "{}/v1/belief/{}",
        bridge.trim_end_matches('/'),
        urlencode(session)
    );
    if let Some(s) = subject {
        url.push_str(&format!("?subject_id={}", urlencode(s)));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let view: BeliefView =
        serde_json::from_str(&body).map_err(|e| format!("decode belief: {e} (body={body})"))?;
    println!("subject:  {}", view.subject_id);
    println!("session:  {}", view.session_id);
    println!("enabled:  {}", view.enabled);
    if view.beliefs.is_empty() {
        println!("beliefs:  (none)");
        return Ok(());
    }
    println!("beliefs:");
    for b in view.beliefs {
        println!("  - {} (confidence: {:.2})", b.text, b.confidence);
    }
    Ok(())
}

async fn reset(
    bridge: &str,
    session: &str,
    subject: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if session.trim().is_empty() {
        return Err("--session is required".into());
    }
    let url = format!(
        "{}/v1/belief/{}",
        bridge.trim_end_matches('/'),
        urlencode(session)
    );
    let mut payload = serde_json::Map::new();
    payload.insert("action".into(), Value::from("reset"));
    if let Some(s) = subject {
        payload.insert("subject_id".into(), Value::from(s));
    }
    let body = http_post_json(&url, &Value::Object(payload)).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode reset response: {e} (body={body})"))?;
    let cleared = v.get("cleared").and_then(|x| x.as_bool()).unwrap_or(false);
    if cleared {
        println!("cleared {session}");
    } else {
        println!("no beliefs were stored for {session}");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct BeliefView {
    subject_id: String,
    session_id: String,
    enabled: bool,
    beliefs: Vec<BeliefItem>,
}

#[derive(Debug, Deserialize)]
struct BeliefItem {
    text: String,
    confidence: f32,
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
