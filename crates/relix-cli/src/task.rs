//! `relix-cli task ...` — operator surface for the Coordinator node.
//!
//! Every subcommand dials a Coordinator peer over libp2p, invokes the
//! relevant `task.*` capability through the real admission pipeline
//! (identity → policy → handler → audit), and prints the response.
//!
//! Calls use the same dial-and-call pattern as `relix-cli ping`. The
//! Coordinator runs the whole admission pipeline on every call, so an
//! operator with no `chat-users` (or whichever group the policy requires)
//! will see `policy_denied` here — by design.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Create a new Task on the Coordinator. Prints the `task_id` on
    /// stdout (32 hex chars).
    Create {
        /// Coordinator peer's libp2p multiaddr.
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Short human-readable title.
        #[arg(long)]
        title: String,
        /// Path/name of the SOL flow this task is associated with.
        #[arg(long)]
        flow_template: String,
        /// Caller-supplied params blob. Free-form (the Coordinator does
        /// not parse it). JSON encouraged.
        #[arg(long, default_value = "")]
        params_json: String,
        /// Override the owner subject id (defaults to the caller's).
        #[arg(long, default_value = "")]
        owner_subject_id: String,
    },
    /// Mutate a Task. Any of the optional fields are skipped when
    /// omitted; the Coordinator preserves their previous values.
    Update {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// New status (`pending` / `running` / `completed` / `failed` /
        /// `abandoned`; the Coordinator does not enforce a state machine).
        #[arg(long, default_value = "")]
        status: String,
        #[arg(long, default_value = "")]
        result: String,
        #[arg(long, default_value = "")]
        flow_id: String,
        #[arg(long, default_value = "")]
        flow_log_path: String,
        /// Error kind from `relix_core::types::error_kinds`. Omit (0) to
        /// leave unchanged.
        #[arg(long, default_value_t = 0i64)]
        error_kind: i64,
        #[arg(long, default_value = "")]
        error_cause: String,
    },
    /// Append a free-form event to a Task's history.
    Event {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        event_type: String,
        #[arg(long, default_value = "")]
        payload: String,
    },
    /// Print one Task and its event chronicle.
    Get {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
    },
    /// List recent Tasks (most-recently-updated first).
    List {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Create {
            peer,
            identity,
            client_key,
            title,
            flow_template,
            params_json,
            owner_subject_id,
        } => {
            let arg = format!("{title}|{flow_template}|{params_json}|{owner_subject_id}");
            let body = call(&peer, &identity, &client_key, "task.create", arg.as_bytes()).await?;
            print_text("task_id", &body);
        }
        Cmd::Update {
            peer,
            identity,
            client_key,
            task_id,
            status,
            result,
            flow_id,
            flow_log_path,
            error_kind,
            error_cause,
        } => {
            let ek = if error_kind == 0 {
                String::new()
            } else {
                error_kind.to_string()
            };
            let arg =
                format!("{task_id}|{status}|{result}|{flow_id}|{flow_log_path}|{ek}|{error_cause}");
            let body = call(&peer, &identity, &client_key, "task.update", arg.as_bytes()).await?;
            print_text("update", &body);
        }
        Cmd::Event {
            peer,
            identity,
            client_key,
            task_id,
            event_type,
            payload,
        } => {
            let arg = format!("{task_id}|{event_type}|{payload}");
            let body = call(&peer, &identity, &client_key, "task.event", arg.as_bytes()).await?;
            print_text("event_id", &body);
        }
        Cmd::Get {
            peer,
            identity,
            client_key,
            task_id,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.get",
                task_id.as_bytes(),
            )
            .await?;
            // Coordinator returns a multi-line key=value block already
            // suitable for printing verbatim.
            print!("{}", std::str::from_utf8(&body).unwrap_or("<binary>"));
        }
        Cmd::List {
            peer,
            identity,
            client_key,
            limit,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.list",
                limit.to_string().as_bytes(),
            )
            .await?;
            // Format: `task_id\tstatus\ttitle\n`. Pretty up for stdout.
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            let mut count = 0;
            for line in s.lines() {
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    println!("{}  {:<10}  {}", parts[0].split_at(8).0, parts[1], parts[2]);
                } else {
                    println!("{line}");
                }
                count += 1;
            }
            if count == 0 {
                println!("(no tasks)");
            }
        }
    }
    Ok(())
}

/// Dial `peer_addr` once, present `identity_bundle`, invoke `method`
/// with `arg` bytes, return the response body. Mirrors `ping::run` but
/// returns the body instead of pretty-printing it (each subcommand
/// formats its own output).
async fn call(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    let key_bytes = std::fs::read(client_key_path)?;
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

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

fn print_text(label: &str, body: &[u8]) {
    match std::str::from_utf8(body) {
        Ok(s) => println!("{label}: {}", s.trim_end_matches('\n')),
        Err(_) => println!(
            "{label} ({} bytes, binary): {}",
            body.len(),
            hex::encode(body)
        ),
    }
}
