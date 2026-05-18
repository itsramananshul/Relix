//! relix-flow-inspect — read flow event logs and audit logs.
//!
//! M3 fills in the audit + flow-log reader. M1 ships a minimal CLI skeleton
//! that parses arguments and (when given a flow log) calls
//! `relix_core::eventlog::read_records` to print events.

use clap::Parser;
use ed25519_dalek::SigningKey;
use std::path::PathBuf;

use relix_core::eventlog;

#[derive(Parser, Debug)]
#[command(
    name = "relix-flow-inspect",
    version,
    about = "Read Relix flow and audit logs"
)]
struct Args {
    /// Path to a flow event log file.
    #[arg(long)]
    flow: Option<PathBuf>,

    /// Path to an audit log file.
    #[arg(long)]
    audit: Option<PathBuf>,

    /// Verify hash-chain integrity (requires --signer-key for full signature check).
    #[arg(long)]
    replay_verify: bool,

    /// Path to the owning controller's signing key (32 raw bytes), for signature
    /// verification during `--replay-verify`.
    #[arg(long)]
    signer_key: Option<PathBuf>,

    /// Human-readable execution trace.
    #[arg(long, default_value_t = false)]
    human: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match (args.flow.as_ref(), args.audit.as_ref()) {
        (Some(flow_path), _) => {
            let recs = eventlog::read_records(flow_path)?;
            if args.replay_verify {
                let key_path = args.signer_key.as_ref().ok_or_else(|| {
                    "--replay-verify requires --signer-key <owner-signing-key>".to_string()
                })?;
                let bytes = std::fs::read(key_path)?;
                if bytes.len() != 32 {
                    return Err("signer key must be 32 raw bytes".into());
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                let key = SigningKey::from_bytes(&arr);
                let (next_seq, _last_hash) =
                    eventlog::verify_chain(flow_path, &key.verifying_key())?;
                println!("INTEGRITY OK");
                println!("records: {}", recs.len());
                println!("next_seq: {}", next_seq);
            } else if args.human {
                println!("# Flow events ({} total)", recs.len());
                for r in &recs {
                    println!(
                        "  seq={:<4} kind={:?} ts={} payload_bytes={}",
                        r.event_seq,
                        r.kind,
                        r.ts.0,
                        r.payload.len()
                    );
                }
            } else {
                println!("records: {}", recs.len());
                for r in &recs {
                    println!(
                        "seq={} kind={:?} payload_len={}",
                        r.event_seq,
                        r.kind,
                        r.payload.len()
                    );
                }
            }
            Ok(())
        }
        (None, Some(audit_path)) => {
            let recs = relix_core::audit::read_audit_records(audit_path)?;
            println!("audit records: {}", recs.len());
            for r in &recs {
                println!(
                    "ts={} rid={} caller={} method={} status={} policy={}",
                    r.ts.0, r.request_id, r.caller_name, r.method, r.status, r.policy_decision
                );
            }
            Ok(())
        }
        (None, None) => Err("provide --flow <path> or --audit <path>".into()),
    }
}
