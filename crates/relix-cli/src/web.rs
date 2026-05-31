//! `relix-cli web ...` — PH-CLI-WEB-BLOCKLIST operator surface
//! for web-tool config introspection on a tool node, fetched
//! through the bridge.
//!
//! HTTP mirror of the bridge's `GET /v1/tool/blocklist`
//! (PH-DASH-BLOCKLIST). Same shape as `relix-cli mcp audit` /
//! `fs audit`: one read-only subcommand, padded table render,
//! `--raw` escape hatch.
//!
//! Future PH-CLI-WEB siblings could surface other web-tool
//! observability (e.g. fetch-recent ring once we add one);
//! today the module ships only the blocklist projection.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// PH-CLI-WEB-BLOCKLIST: show the operator-curated host
    /// blocklist as fetched from the bridge proxy. Sorted
    /// lexicographically; first 50 by default. Read-only.
    Blocklist {
        /// Bridge HTTP base URL.
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias.
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Maximum rows to render in the table view. Hosts past
        /// the cap are omitted from the table but the trailing
        /// summary still shows the full count. Default 50.
        #[arg(long, default_value_t = 50usize)]
        max: usize,
        /// Print raw JSON from the bridge instead of the
        /// formatted table. Bypasses `--max` (always shows
        /// every host the bridge returned).
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Blocklist {
            bridge,
            peer,
            max,
            raw,
        } => {
            let url = format!(
                "{}/v1/tool/blocklist?peer={peer}",
                bridge.trim_end_matches('/'),
            );
            let body = http_get(&url).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let parsed: BlocklistResp = serde_json::from_str(&body)
                .map_err(|e| format!("decode /v1/tool/blocklist body: {e} (body={body})"))?;
            render_blocklist(&parsed, max);
        }
    }
    Ok(())
}

/// PH-CLI-WEB-BLOCKLIST: mirror of the bridge's
/// `blocklist::BlocklistResponse`.
#[derive(Debug, Deserialize)]
struct BlocklistResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    count: usize,
}

fn render_blocklist(resp: &BlocklistResp, max: usize) {
    if resp.hosts.is_empty() {
        println!(
            "(no hosts on the blocklist for peer '{p}' — ring count={c})",
            p = resp.peer,
            c = resp.count
        );
        return;
    }
    let host_h = "host";
    println!("{host_h}");
    let take = max.max(1).min(resp.hosts.len());
    for h in resp.hosts.iter().take(take) {
        println!("{h}");
    }
    if resp.hosts.len() > take {
        println!(
            "(showing {shown} of {total}; rerun with --max to see more, or --raw for full JSON)",
            shown = take,
            total = resp.hosts.len(),
        );
    } else {
        println!("count={}", resp.count);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_resp_round_trips_through_serde() {
        let json = r#"{
            "peer": "tool",
            "hosts": ["alpha.example.com", "beta.example.com"],
            "count": 2
        }"#;
        let parsed: BlocklistResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.peer, "tool");
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.hosts.len(), 2);
        assert_eq!(parsed.hosts[1], "beta.example.com");
    }

    #[test]
    fn blocklist_resp_tolerates_missing_fields() {
        let parsed: BlocklistResp = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.peer, "");
        assert_eq!(parsed.count, 0);
        assert!(parsed.hosts.is_empty());
    }

    #[test]
    fn blocklist_resp_ignores_unknown_fields() {
        // Forward-compat: a new bridge field shouldn't break an
        // older CLI build.
        let json = r#"{
            "peer": "tool",
            "count": 1,
            "future_field": 99,
            "hosts": ["x.example.com"]
        }"#;
        let parsed: BlocklistResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.hosts[0], "x.example.com");
    }
}
