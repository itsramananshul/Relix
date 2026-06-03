//! `relix-cli flow-run` — execute a SOL flow against a real Relix mesh.
//!
//! Usage:
//! ```text
//! relix-cli flow-run \
//!     --flow flows/ping.sol \
//!     --identity dev-keys/alice.aic \
//!     --client-key dev-keys/org.key \
//!     --peers configs/peers.toml
//! ```
//!
//! Spins up an ephemeral libp2p peer, dials every peer named in `--peers`,
//! compiles the SOL flow, attaches the real `RemoteCallDispatcher` from
//! `relix_runtime::flow_runner`, and executes the VM. On exit the runner
//! prints the flow id, the flow-log path, and either the final return value
//! or the structured `RemoteCallError`.

use std::path::Path;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::flow_runner::{FlowRunOptions, FlowRunner, PeersFile};

pub async fn run(
    flow_path: &Path,
    identity_path: &Path,
    client_key_path: &Path,
    peers_path: &Path,
    deadline_secs: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    // Identity bundle.
    let bundle_bytes = std::fs::read(identity_path)?;
    let identity: Bundle = codec::decode(&bundle_bytes)?;

    // Local libp2p PeerId key.
    //
    // SEC PART 2: wrap both the disk read AND the parsed
    // 32-byte array in `Zeroizing` so the secret-key bytes
    // never linger past this scope. `FlowRunOptions.client_key`
    // accepts the zeroizing wrapper.
    let key_bytes: zeroize::Zeroizing<Vec<u8>> =
        zeroize::Zeroizing::new(std::fs::read(client_key_path)?);
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&key_bytes);

    // Peer alias map.
    let peers = PeersFile::from_path(peers_path)?;

    let opts = FlowRunOptions {
        flow_path: flow_path.to_path_buf(),
        identity_bundle: identity,
        client_key: key,
        peers,
        data_dir: None,
        deadline_secs,
        capability_cache: None,
        mesh_client: None,
        trace_id: None,
        task_id: None,
        session_id: None,
        workspace_path: None,
        chunk_observer: None,
        cancel_signal: None,
        last_confidence_cell: None,
    };
    let result = FlowRunner::new(opts).run().await?;

    println!("# Relix flow run");
    println!("flow_id:       {}", result.flow_id);
    println!("trace_id:      {}", result.trace_id);
    println!("flow_log:      {}", result.flow_log_path.display());
    if let Some(s) = &result.final_string {
        println!("status:        ok");
        println!("return:        {}", s);
    } else if let Some(e) = &result.last_error {
        println!("status:        failed");
        println!("error:         {}", e);
        std::process::exit(2);
    } else {
        println!("status:        ok (non-string exit={})", result.vm_exit);
    }
    Ok(())
}
