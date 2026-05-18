//! relix-flow-inspect — read flow event logs and audit logs.
//!
//! Operator entry point. Reads:
//! - flow event logs (`--flow <path>`): per-flow append-only signed log.
//! - audit logs (`--audit <path>`): per-responder append-only signed log.
//!
//! Output modes:
//! - default: one summary line per record.
//! - `--human`: indented, multi-line; payload key=value lines surfaced;
//!   latency_ms extracted from `RemoteCallCompleted` / `RemoteCallFailed`.
//! - `--replay-verify` (flow only): walks the hash chain + verifies every
//!   record's signature against the supplied owner signing key. Prints
//!   `INTEGRITY OK` and the record/seq counts on success.
//!
//! Filters (audit only):
//! - `--trace <hex>`: keep only records whose `trace_id` matches.
//! - `--rid   <hex>`: keep only records whose `request_id` matches.

use clap::Parser;
use ed25519_dalek::SigningKey;
use std::path::PathBuf;

use relix_core::eventlog::{self, EventRecord};

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

    /// (audit only) Filter records by trace_id (hex).
    #[arg(long)]
    trace: Option<String>,

    /// (audit only) Filter records by request_id (hex).
    #[arg(long)]
    rid: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    match (args.flow.as_ref(), args.audit.as_ref()) {
        (Some(flow_path), _) => handle_flow(flow_path, &args),
        (None, Some(audit_path)) => handle_audit(audit_path, &args),
        (None, None) => Err("provide --flow <path> or --audit <path>".into()),
    }
}

fn handle_flow(flow_path: &PathBuf, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
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
        let (next_seq, _last_hash) = eventlog::verify_chain(flow_path, &key.verifying_key())?;
        println!("INTEGRITY OK");
        println!("records: {}", recs.len());
        println!("next_seq: {}", next_seq);
        return Ok(());
    }
    if args.human {
        print_flow_human(&recs);
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

fn handle_audit(audit_path: &PathBuf, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let recs = relix_core::audit::read_audit_records(audit_path)?;
    // Apply optional filters.
    let trace_filter = args.trace.as_ref().map(|s| s.to_lowercase());
    let rid_filter = args.rid.as_ref().map(|s| s.to_lowercase());
    let filtered: Vec<_> = recs
        .iter()
        .filter(|r| {
            if let Some(t) = &trace_filter
                && hex::encode(r.trace_id.0) != *t
            {
                return false;
            }
            if let Some(r2) = &rid_filter
                && hex::encode(r.request_id.0) != *r2
            {
                return false;
            }
            true
        })
        .collect();
    println!(
        "audit records: {}{}",
        filtered.len(),
        if filtered.len() != recs.len() {
            format!(" (filtered from {})", recs.len())
        } else {
            String::new()
        }
    );
    for r in &filtered {
        if args.human {
            println!("  ts={} rid={} trace={}", r.ts.0, r.request_id, r.trace_id);
            println!("    caller={} groups={:?}", r.caller_name, r.caller_groups);
            println!("    method={} status={}", r.method, r.status);
            println!("    policy={}", r.policy_decision);
            if let Some(k) = r.error_kind {
                println!("    error_kind={k}");
            }
            println!("    latency_ms={}", r.latency_ms);
        } else {
            println!(
                "ts={} rid={} caller={} method={} status={} policy={}",
                r.ts.0, r.request_id, r.caller_name, r.method, r.status, r.policy_decision
            );
        }
    }
    Ok(())
}

/// Pretty-print a flow log with indented payloads + extracted `latency_ms`.
///
/// Payloads are written by `relix_runtime::flow_runner` as multi-line
/// `key=value\n` UTF-8 text. We decode best-effort; non-UTF-8 falls back to
/// the byte-count summary so the inspector never panics on novel payloads.
fn print_flow_human(recs: &[EventRecord]) {
    println!("# Flow events ({} total)", recs.len());
    for r in recs {
        let latency = extract_latency_ms(r.payload.as_ref());
        let kind_str = format!("{:?}", r.kind);
        let lat_str = match latency {
            Some(ms) => format!("  ({ms} ms)"),
            None => String::new(),
        };
        println!(
            "  seq={:<3} ts={} kind={}{lat_str}",
            r.event_seq, r.ts.0, kind_str
        );
        match std::str::from_utf8(r.payload.as_ref()) {
            Ok(text) if !text.trim().is_empty() => {
                for line in text.lines() {
                    if !line.is_empty() {
                        println!("      {line}");
                    }
                }
            }
            Ok(_) => {} // empty
            Err(_) => println!("      <binary payload: {} bytes>", r.payload.len()),
        }
    }
}

/// Pull `latency_ms=<u64>` out of a key=value payload, if present.
fn extract_latency_ms(payload: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(payload).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("latency_ms=") {
            return rest.trim().parse().ok();
        }
    }
    None
}
