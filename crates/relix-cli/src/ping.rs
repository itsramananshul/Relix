//! `relix-cli ping <addr> --identity-bundle <path> --client-key <path>`
//!
//! Spins up an ephemeral libp2p peer, dials the given multiaddr, and invokes
//! `node.health` on the remote node using the supplied identity bundle.

use std::path::Path;
use std::time::Duration;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

pub async fn run(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    let key_bytes = std::fs::read(client_key_path)?;
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

    // Bind to a random high port to avoid the libp2p multiaddr-validation
    // edge case around `tcp/0` in some 0.54 builds.
    let port = 20_000 + (rand::random::<u16>() % 10_000);
    let (client, mut events, event_loop) = rpc::new(key, port).await?;
    tokio::spawn(event_loop.run());

    let addr: Multiaddr = peer_addr
        .parse()
        .map_err(|e| format!("parse multiaddr '{peer_addr}': {e:?}"))?;
    client
        .dial(addr.clone())
        .await
        .map_err(|e| format!("dial: {e}"))?;

    // Wait for connection or timeout.
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

    // Build envelope; deadline 10s.
    let envelope = build_request("node.health", b"ping".to_vec(), bundle, 10);

    // Issue RPC.
    let resp_bytes = client
        .call(connected, envelope)
        .await
        .map_err(|e| format!("rpc: {e}"))?;
    let resp = decode_response(&resp_bytes)?;
    match resp.res {
        ResponseResult::Ok(body) => {
            println!(
                "OK from {}: {}",
                resp.responder,
                String::from_utf8_lossy(body.as_ref())
            );
            println!("aid: {}", hex::encode(resp.aid.as_ref()));
        }
        ResponseResult::Err(e) => {
            println!("ERR kind={} cause={}", e.kind, e.cause);
            std::process::exit(2);
        }
        ResponseResult::StreamHandle(_) => {
            println!("unexpected stream-handle response from node.health");
            std::process::exit(2);
        }
    }
    Ok(())
}
