//! `relix-cli topology` — mesh topology inspection.
//!
//! Hits the bridge's `GET /v1/topology` endpoint and prints a
//! one-line-per-peer summary. Distinct from `relix-cli capability
//! ls`, which talks libp2p directly to ONE peer. Topology
//! aggregates across every peer the bridge has discovered, and
//! surfaces per-peer freshness (when did we last successfully
//! refresh this peer's manifest?) so operators can spot
//! degraded / unreachable peers without log-grepping.
//!
//! The CLI talks plain HTTP to the bridge — operators already
//! have the bridge URL from their `--bridge` flag everywhere
//! else. No libp2p dial-out from this command.

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Show every peer the bridge knows about, with freshness +
    /// capability count. Optional `--json` for machine-readable
    /// output (the bridge's raw response, piped through verbatim).
    Show {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = "http://127.0.0.1:19791")]
        bridge: String,
        /// Skip pretty-printing; emit the bridge's raw JSON
        /// body for piping into `jq` or scripts.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Override the warning threshold (seconds since last
        /// refresh) at which a peer is flagged in the table.
        /// Default 120 matches the bridge's `stale` bucket.
        #[arg(long, default_value_t = 120i64)]
        warn_after_secs: i64,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Show {
            bridge,
            json,
            warn_after_secs,
        } => show(&bridge, json, warn_after_secs).await,
    }
}

async fn show(
    bridge: &str,
    json: bool,
    warn_after_secs: i64,
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
    let parsed: TopologyResponse = serde_json::from_str(&body)
        .map_err(|e| format!("bridge returned non-JSON body: {e}\nraw:\n{body}"))?;
    render_table(&parsed, warn_after_secs);
    Ok(())
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    // Build the client per-call: this command is one-shot and
    // doesn't justify a pool. Short timeout because the bridge
    // is local; if it's down, fail fast.
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

fn render_table(t: &TopologyResponse, warn_after_secs: i64) {
    if t.peers.is_empty() {
        println!("(no peers discovered)");
        return;
    }
    let alias_h = "alias";
    let type_h = "type";
    let name_h = "node_name";
    let caps_h = "caps";
    let refr_h = "last_refr";
    let id_h = "node_id";
    println!("{alias_h:<14}  {type_h:<10}  {name_h:<14}  {caps_h:>5}  {refr_h:>10}  {id_h}");
    for p in &t.peers {
        let alias = p.alias.as_deref().unwrap_or("(none)");
        let stale_marker = if p.last_refreshed_secs_ago >= warn_after_secs {
            "!"
        } else {
            " "
        };
        let node_type = &p.node_type;
        let node_name = &p.node_name;
        let caps = p.capability_count;
        let secs = p.last_refreshed_secs_ago;
        let short = shorten_id(&p.node_id);
        let fresh = &p.freshness;
        println!(
            "{alias:<14}  {node_type:<10}  {node_name:<14}  {caps:>5}  {secs:>8}s{stale_marker}  {short}  [{fresh}]"
        );
    }
    println!();
    let gen_at = t.generated_at;
    let n = t.peers.len();
    println!("generated_at={gen_at}  peers={n}  warn_after_secs={warn_after_secs}");
}

fn shorten_id(id: &str) -> String {
    if id.len() > 16 {
        format!("{}…{}", &id[..8], &id[id.len() - 4..])
    } else {
        id.to_string()
    }
}

#[derive(Debug, Deserialize)]
struct TopologyResponse {
    peers: Vec<PeerView>,
    generated_at: i64,
}

#[derive(Debug, Deserialize)]
struct PeerView {
    #[serde(default)]
    alias: Option<String>,
    node_id: String,
    node_type: String,
    node_name: String,
    capability_count: usize,
    last_refreshed_secs_ago: i64,
    freshness: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_id_handles_short_inputs_without_modification() {
        assert_eq!(shorten_id("abc"), "abc");
        assert_eq!(shorten_id("0123456789abcdef"), "0123456789abcdef");
    }

    #[test]
    fn shorten_id_truncates_long_inputs_with_ellipsis() {
        let id = "0123456789abcdef0123456789abcdef";
        let s = shorten_id(id);
        assert!(s.contains('…'), "expected ellipsis in {s}");
        assert!(s.starts_with("01234567"));
        assert!(s.ends_with("cdef"));
    }

    #[test]
    fn topology_response_deserializes_typical_bridge_body() {
        let body = r#"{
            "peers": [
                {
                    "alias": "memory",
                    "node_id": "deadbeef",
                    "node_type": "memory",
                    "node_name": "local-memory",
                    "manifest_version": 1,
                    "capability_count": 3,
                    "methods": ["memory.write_turn"],
                    "last_refreshed_at": 1700000000,
                    "last_refreshed_secs_ago": 42,
                    "freshness": "fresh"
                }
            ],
            "generated_at": 1700000042
        }"#;
        let t: TopologyResponse = serde_json::from_str(body).unwrap();
        assert_eq!(t.peers.len(), 1);
        assert_eq!(t.peers[0].alias.as_deref(), Some("memory"));
        assert_eq!(t.peers[0].freshness, "fresh");
        assert_eq!(t.generated_at, 1_700_000_042);
    }

    #[test]
    fn topology_response_handles_peer_without_alias() {
        let body = r#"{
            "peers": [{"node_id":"x","node_type":"t","node_name":"n",
                       "capability_count":0,
                       "last_refreshed_secs_ago":5,"freshness":"fresh"}],
            "generated_at": 1
        }"#;
        let t: TopologyResponse = serde_json::from_str(body).unwrap();
        assert!(t.peers[0].alias.is_none());
    }
}
