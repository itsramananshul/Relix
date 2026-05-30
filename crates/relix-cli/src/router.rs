//! `relix-cli router ...` — operator surface for the Router Node.
//!
//! Three subcommands cover the operator workflow:
//! - `status`   — `router.network_summary` + a one-screen overview.
//! - `peers`    — peer table (peer_id, name, caps, healthy, last heartbeat).
//! - `sessions` — `router.session_list` with `--status / --limit / --offset`.
//!
//! Each call dials the router peer once, presents an identity
//! bundle, invokes the capability through the real admission
//! pipeline (identity → policy → handler → audit), and prints
//! the response. Mirrors the `relix-cli task` shape exactly so
//! operators have one mental model across all CLI ops.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::router::{
    HeartbeatResponse, NetworkSummaryRequest, NetworkSummaryResponse, SessionListRequest,
    SessionListResponse,
};
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// One-screen mesh overview: router id, uptime, peer count,
    /// active sessions, total sessions since start.
    Status {
        /// Router peer's libp2p multiaddr.
        #[arg(long)]
        peer: String,
        /// Caller's identity bundle.
        #[arg(long)]
        identity: PathBuf,
        /// 32-byte signing key used as the local libp2p PeerId.
        #[arg(long)]
        client_key: PathBuf,
        /// Optional org filter (substring match against peer
        /// `groups`). Empty = all visible peers.
        #[arg(long, default_value = "")]
        org_filter: String,
    },
    /// Peer table — one row per known peer.
    Peers {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Optional org filter (substring match against peer
        /// `groups`). Empty = all visible peers.
        #[arg(long, default_value = "")]
        org_filter: String,
    },
    /// Session table — paginated, filterable by status.
    Sessions {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// `running` | `completed` | `failed`. Empty = all.
        #[arg(long, default_value = "")]
        status: String,
        /// Page size (default 100).
        #[arg(long, default_value_t = 100usize)]
        limit: usize,
        /// Page offset (default 0).
        #[arg(long, default_value_t = 0usize)]
        offset: usize,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Status {
            peer,
            identity,
            client_key,
            org_filter,
        } => status(&peer, &identity, &client_key, &org_filter).await,
        Cmd::Peers {
            peer,
            identity,
            client_key,
            org_filter,
        } => peers(&peer, &identity, &client_key, &org_filter).await,
        Cmd::Sessions {
            peer,
            identity,
            client_key,
            status,
            limit,
            offset,
        } => sessions(&peer, &identity, &client_key, &status, limit, offset).await,
    }
}

async fn status(
    peer: &str,
    identity: &Path,
    client_key: &Path,
    org_filter: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = NetworkSummaryRequest {
        org_filter: opt_str(org_filter),
    };
    let body = call(
        peer,
        identity,
        client_key,
        "router.network_summary",
        &codec::encode(&req)?,
    )
    .await?;
    let r: NetworkSummaryResponse = codec::decode(&body)?;
    println!("router_peer_id={pid}", pid = r.router_peer_id);
    println!("router_name={name}", name = r.router_name);
    println!("uptime_secs={u}", u = r.uptime_secs);
    println!("peer_count={pc}", pc = r.peer_count);
    println!("active_sessions={a}", a = r.active_sessions);
    println!(
        "total_sessions_since_start={t}",
        t = r.total_sessions_since_start,
    );
    println!("timestamp={ts}", ts = r.timestamp);
    Ok(())
}

async fn peers(
    peer: &str,
    identity: &Path,
    client_key: &Path,
    org_filter: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = NetworkSummaryRequest {
        org_filter: opt_str(org_filter),
    };
    let body = call(
        peer,
        identity,
        client_key,
        "router.network_summary",
        &codec::encode(&req)?,
    )
    .await?;
    let r: NetworkSummaryResponse = codec::decode(&body)?;
    if r.peers.is_empty() {
        println!("(no peers)");
        return Ok(());
    }
    let id_h = "peer_id";
    let name_h = "name";
    let caps_h = "caps";
    let hlt_h = "healthy";
    let hb_h = "last_hb";
    println!("{id_h:<24}  {name_h:<14}  {caps_h:>4}  {hlt_h:<7}  {hb_h}");
    for p in &r.peers {
        let short = if p.peer_id.len() > 22 {
            &p.peer_id[..22]
        } else {
            &p.peer_id
        };
        println!(
            "{id:<24}  {name:<14}  {caps:>4}  {hlt:<7}  {hb}",
            id = short,
            name = p.name,
            caps = p.capabilities.len(),
            hlt = if p.healthy { "yes" } else { "no" },
            hb = p.last_heartbeat_secs,
        );
    }
    Ok(())
}

async fn sessions(
    peer: &str,
    identity: &Path,
    client_key: &Path,
    status: &str,
    limit: usize,
    offset: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let req = SessionListRequest {
        status_filter: opt_str(status),
        limit: Some(limit),
        offset: Some(offset),
    };
    let body = call(
        peer,
        identity,
        client_key,
        "router.session_list",
        &codec::encode(&req)?,
    )
    .await?;
    let r: SessionListResponse = codec::decode(&body)?;
    println!(
        "sessions  shown={shown}  total={total}  limit={limit}  offset={offset}",
        shown = r.sessions.len(),
        total = r.total,
        limit = limit,
        offset = offset,
    );
    if r.sessions.is_empty() {
        println!("(no sessions match)");
        return Ok(());
    }
    let id_h = "session_id";
    let wf_h = "workflow";
    let st_h = "status";
    let route_h = "route";
    let age_h = "started_at";
    println!("{id_h:<14}  {wf_h:<18}  {st_h:<10}  {age_h:>10}  {route_h}");
    for s in &r.sessions {
        let short = if s.session_id.len() > 12 {
            &s.session_id[..12]
        } else {
            &s.session_id
        };
        println!(
            "{id:<14}  {wf:<18}  {st:<10}  {ts:>10}  {route}",
            id = short,
            wf = s.workflow_name,
            st = s.status,
            ts = s.started_at,
            route = s.route.join(" → "),
        );
    }
    Ok(())
}

fn opt_str(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Suppress dead_code on a re-export used by future extensions
/// (`relix-cli router heartbeat-test` could simulate a controller
/// pushing a heartbeat for debugging — landing in a follow-up).
#[allow(dead_code)]
fn _heartbeat_response_reference() {
    let _ = std::any::TypeId::of::<HeartbeatResponse>();
}

/// Dial `peer`, present `identity`, invoke `method` with CBOR
/// `arg` bytes, return the response body. Mirrors task.rs::call.
async fn call(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    // SEC PART 2: zeroize the raw key bytes on scope exit.
    let key_bytes: zeroize::Zeroizing<Vec<u8>> =
        zeroize::Zeroizing::new(std::fs::read(client_key_path)?);
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&key_bytes);

    let port = 20_000 + (rand::random::<u16>() % 10_000);
    let (client, mut events, event_loop) = rpc::new(*key, port).await?;
    tokio::spawn(event_loop.run());

    let addr: Multiaddr = peer_addr
        .parse()
        .map_err(|e| format!("parse multiaddr '{peer_addr}': {e:?}"))?;
    client
        .dial(addr.clone())
        .await
        .map_err(|e| format!("dial: {e}"))?;

    let connected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::PeerConnected { peer_id, .. }) = events.recv().await {
                return Some(peer_id);
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or("timeout waiting for peer connection")?;

    let envelope = build_request(method, arg.to_vec(), bundle, 10);
    let resp_bytes = client
        .call(connected, envelope)
        .await
        .map_err(|e| format!("rpc: {e}"))?;
    let resp = decode_response(&resp_bytes)?;
    match resp.res {
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(e) => {
            eprintln!("ERR kind={} cause={}", e.kind, e.cause);
            std::process::exit(2);
        }
        ResponseResult::StreamHandle(_) => {
            eprintln!("unexpected stream-handle response from method '{method}'");
            std::process::exit(2);
        }
    }
}
