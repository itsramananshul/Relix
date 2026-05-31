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
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
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
    /// Print the bridge's `/v1/health` summary: uptime,
    /// coordinator status, peer freshness counts, reconnect
    /// telemetry. One-shot snapshot suitable for on-call
    /// triage or status-line scripts.
    Health {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Raw JSON instead of the pretty one-line summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Show {
            bridge,
            json,
            warn_after_secs,
        } => show(&bridge, json, warn_after_secs).await,
        Cmd::Health { bridge, json } => health(&bridge, json).await,
    }
}

async fn health(bridge: &str, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/health", bridge.trim_end_matches('/'));
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
    // One-line summary so this fits in a status bar / tmux
    // pane / shell prompt. Reconnect block only when present.
    let coord = if h.coordinator_configured {
        "yes"
    } else {
        "no"
    };
    let uptime = h.uptime_secs;
    let peers = h.peer_count;
    let fresh = h.peers_fresh;
    let stale = h.peers_stale;
    let expired = h.peers_expired;
    let recon = h
        .reconnect
        .as_ref()
        .map(|r| {
            let a = r.attempts;
            let s = r.successes;
            format!("  reconnect={s}/{a}")
        })
        .unwrap_or_default();
    println!(
        "status={status}  uptime={uptime}s  coord={coord}  peers={peers} (fresh={fresh} stale={stale} expired={expired}){recon}",
        status = h.status,
    );
    Ok(())
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    status: String,
    uptime_secs: i64,
    coordinator_configured: bool,
    peer_count: usize,
    peers_fresh: usize,
    peers_stale: usize,
    peers_expired: usize,
    #[serde(default)]
    reconnect: Option<ReconnectCounters>,
}

#[derive(Debug, Deserialize)]
struct ReconnectCounters {
    attempts: u64,
    successes: u64,
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

    #[test]
    fn health_response_deserializes_full_bridge_body() {
        // Mirrors the bridge's serializer in
        // crates/relix-web-bridge/src/topology.rs::HealthResponse.
        // Guards against silent renames or shape changes.
        let body = r#"{
            "status": "ok",
            "started_at": 1700000000,
            "now": 1700003600,
            "uptime_secs": 3600,
            "coordinator_configured": true,
            "peer_count": 4,
            "peers_fresh": 3,
            "peers_stale": 1,
            "peers_expired": 0,
            "reconnect": {"attempts": 7, "successes": 5}
        }"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert_eq!(h.status, "ok");
        assert_eq!(h.uptime_secs, 3600);
        assert!(h.coordinator_configured);
        assert_eq!(h.peer_count, 4);
        assert_eq!(h.peers_fresh, 3);
        let r = h.reconnect.unwrap();
        assert_eq!(r.attempts, 7);
        assert_eq!(r.successes, 5);
    }

    #[test]
    fn health_response_handles_missing_reconnect_block() {
        // The bridge omits `reconnect` (skip_serializing_if =
        // "Option::is_none") when the MeshClient is absent.
        // The CLI must accept that shape and render gracefully.
        let body = r#"{
            "status": "ok",
            "started_at": 1700000000,
            "now": 1700000010,
            "uptime_secs": 10,
            "coordinator_configured": false,
            "peer_count": 0,
            "peers_fresh": 0,
            "peers_stale": 0,
            "peers_expired": 0
        }"#;
        let h: HealthResponse = serde_json::from_str(body).unwrap();
        assert!(!h.coordinator_configured);
        assert!(h.reconnect.is_none());
    }
}
