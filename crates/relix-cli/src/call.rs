//! `relix call` — generic raw capability invocation.
//!
//! Dials a peer over libp2p and invokes ANY capability by wire name
//! with a verbatim pipe-delimited argument string, then prints the
//! raw response body. This is the operator escape hatch for the long
//! tail of capabilities that don't have a bespoke subcommand (the
//! product-spine `brief.*` / `mandate.*` / `campaign.*` reads, the
//! `rig.*` and `agent.*` org reads, …). Same dial-and-call path as
//! `relix task` / `relix ping`, so it goes through the full
//! admission pipeline (identity → policy → handler → audit).
//!
//! Examples:
//!   relix call --peer <addr> --identity id.bundle --client-key k \
//!       --method brief.detail --arg <task_id>
//!   relix call ... --method mandate.search --arg "auth|20"
//!   relix call ... --method agent.by_role --arg engineer

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Args;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Args, Debug)]
pub struct CallArgs {
    /// Target peer's libp2p multiaddr.
    #[arg(long)]
    peer: String,
    /// Caller identity bundle.
    #[arg(long)]
    identity: PathBuf,
    /// Caller's 32-byte client key.
    #[arg(long)]
    client_key: PathBuf,
    /// The capability wire name, e.g. `brief.detail`.
    #[arg(long)]
    method: String,
    /// Verbatim pipe-delimited argument string (handler-specific).
    /// Empty when the capability takes no args.
    #[arg(long, default_value = "")]
    arg: String,
}

pub async fn run(args: CallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let body = call(
        &args.peer,
        &args.identity,
        &args.client_key,
        &args.method,
        args.arg.as_bytes(),
    )
    .await?;
    // Raw stdout so the result pipes cleanly into jq / grep / a file.
    use std::io::Write;
    let mut out = std::io::stdout();
    out.write_all(&body).ok();
    // Add a trailing newline only when the body doesn't end in one,
    // so interactive use reads naturally without corrupting piping of
    // already-newline-terminated payloads.
    if body.last() != Some(&b'\n') {
        out.write_all(b"\n").ok();
    }
    Ok(())
}

async fn call(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    // Zeroize the raw key bytes on scope exit.
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
