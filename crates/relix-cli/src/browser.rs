//! `relix-cli browser ...` — PH-CLI-BROWSER operator surface
//! for browser-session observability on a tool node, fetched
//! through the bridge.
//!
//! HTTP mirror of the bridge's `GET /v1/browser/sessions`
//! (PH-DASH-BROWSER). Same shape as `relix-cli mcp audit` /
//! `fs audit` / `web blocklist`: one read-only subcommand,
//! padded table render, `--raw` escape hatch.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// PH-CLI-BROWSER: list open `tool.browser.*` sessions on
    /// the named peer via the bridge proxy. Read-only — open /
    /// close go through the existing libp2p dispatch.
    Sessions {
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Print raw JSON from the bridge instead of the
        /// formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Sessions { bridge, peer, raw } => {
            let url = format!(
                "{}/v1/browser/sessions?peer={peer}",
                bridge.trim_end_matches('/'),
            );
            let body = http_get(&url).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let parsed: SessionsResp = serde_json::from_str(&body)
                .map_err(|e| format!("decode /v1/browser/sessions body: {e} (body={body})"))?;
            render_sessions(&parsed);
        }
    }
    Ok(())
}

/// PH-CLI-BROWSER: mirror of the bridge's
/// `browser_sessions::BrowserSessionRow`.
#[derive(Debug, Deserialize)]
struct SessionRow {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    opened_at: i64,
    #[serde(default)]
    current_url: Option<String>,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Deserialize)]
struct SessionsResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    sessions: Vec<SessionRow>,
    #[serde(default)]
    count: usize,
}

fn render_sessions(resp: &SessionsResp) {
    if resp.sessions.is_empty() {
        println!(
            "(no open browser sessions on peer '{p}' — count={c})",
            p = resp.peer,
            c = resp.count
        );
        return;
    }
    let sid_h = "session";
    let ts_h = "opened";
    let url_h = "url";
    let st_h = "status";
    println!("{sid_h:<16}  {ts_h:<10}  {url_h:<40}  {st_h}");
    for s in &resp.sessions {
        let sid = truncate(&s.session_id, 16);
        let url = truncate(s.current_url.as_deref().unwrap_or("-"), 40);
        println!(
            "{sid:<16}  {ts:<10}  {url:<40}  {st}",
            sid = sid,
            ts = s.opened_at,
            url = url,
            st = s.status,
        );
    }
    println!("count={}", resp.count);
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

fn print_raw(body: &str) {
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_resp_round_trips_through_serde() {
        let json = r#"{
            "peer": "tool",
            "sessions": [
                {"session_id": "abc123", "opened_at": 100,
                 "current_url": "https://example.com/",
                 "status": "connected"},
                {"session_id": "def456", "opened_at": 200,
                 "status": "unconnected"}
            ],
            "count": 2
        }"#;
        let parsed: SessionsResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.peer, "tool");
        assert_eq!(parsed.count, 2);
        assert_eq!(
            parsed.sessions[0].current_url.as_deref(),
            Some("https://example.com/")
        );
        // The second entry omits current_url; the Option default
        // is None.
        assert_eq!(parsed.sessions[1].current_url, None);
    }

    #[test]
    fn sessions_resp_tolerates_missing_top_level_fields() {
        let parsed: SessionsResp = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.peer, "");
        assert_eq!(parsed.count, 0);
        assert!(parsed.sessions.is_empty());
    }

    #[test]
    fn sessions_resp_ignores_unknown_fields() {
        let json = r#"{
            "peer": "tool",
            "count": 1,
            "future_top_field": 99,
            "sessions": [{
                "session_id": "x",
                "opened_at": 1,
                "status": "ok",
                "future_row_field": "shrug"
            }]
        }"#;
        let parsed: SessionsResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.sessions[0].session_id, "x");
    }
}
