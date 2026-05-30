//! relix-flow-inspect — read flow event logs and audit logs.
//!
//! Operator entry point. Reads:
//! - flow event logs (`--flow <path>`): per-flow append-only signed log.
//! - audit logs (`--audit <path>`): per-responder append-only signed log.
//! - PART 7: audit partition mirror
//!   (`--audit-partition <path>`): SQLite per-tenant index the
//!   bridge writes alongside the signed CBOR log. Required
//!   input mode for the `--tenant` / `--all-tenants` filters
//!   because the signed `AuditRecord` does NOT carry
//!   `tenant_id` (the field lives only on the partition
//!   mirror — changing the signed shape would break the
//!   existing hash chain).
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
//!
//! PART 7 — multi-tenant audit reads (partition mirror only):
//! - `--tenant <id>`: filter rows to the named tenant via the
//!   underlying `tenant_recent(tenant_id)` query (which itself
//!   ships a `WHERE tenant_id = ?1` clause).
//! - `--all-tenants`: print every tenant's rows grouped by
//!   tenant id with a separator line. Requires interactive
//!   confirmation on stdin: the operator must type `yes` to
//!   proceed; anything else exits without reading.
//! - `--multi-tenant-mode`: when set, the no-flag case is
//!   rejected with the documented error message. Operators
//!   running in multi-tenant deployments wire this so a
//!   forgotten `--tenant` flag never reads across tenants.

use clap::Parser;
use ed25519_dalek::SigningKey;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use zeroize::Zeroizing;

use relix_core::eventlog::{self, EventRecord};
use relix_runtime::audit_partition::AuditPartitionStore;

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

    /// PART 7 — path to the audit partition SQLite mirror.
    /// Required for the `--tenant` / `--all-tenants` flags
    /// because the signed CBOR audit log does NOT carry
    /// `tenant_id`. Operators point this at the file written
    /// by `AuditPartitionStore::open` (default
    /// `<data_dir>/audit-partition.db`).
    #[arg(long)]
    audit_partition: Option<PathBuf>,

    /// PART 7 — filter audit-partition rows to a single
    /// tenant id via the underlying `WHERE tenant_id = ?1`
    /// query. Mutually exclusive with `--all-tenants`;
    /// requires `--audit-partition`.
    #[arg(long)]
    tenant: Option<String>,

    /// PART 7 — print every tenant's rows grouped by tenant
    /// id with a separator line between groups. Requires
    /// interactive confirmation on stdin: the operator must
    /// type `yes` to proceed; anything else exits without
    /// reading. Mutually exclusive with `--tenant`; requires
    /// `--audit-partition`.
    #[arg(long, default_value_t = false)]
    all_tenants: bool,

    /// PART 7 — enforces that one of `--tenant <id>` /
    /// `--all-tenants` is supplied when reading the audit
    /// partition. Operators running multi-tenant deployments
    /// wire this so a forgotten flag cannot silently print
    /// every tenant's rows. Equivalent to the bridge's
    /// `[auth] multi_tenant_mode` config flag.
    #[arg(long, default_value_t = false)]
    multi_tenant_mode: bool,

    /// PART 7 — when set, skip the `--all-tenants`
    /// confirmation prompt. Reserved for non-interactive
    /// pipelines (CI / scripted operator tools) that have
    /// already confirmed out-of-band. Has no effect on the
    /// `--multi-tenant-mode` enforcement; the operator still
    /// must pass one of `--tenant` / `--all-tenants`.
    #[arg(long, default_value_t = false)]
    yes: bool,

    /// PART 7 — cap rows returned per tenant by the
    /// partition reader. Defaults to 1000 (the
    /// `tenant_recent` server-side clamp).
    #[arg(long, default_value_t = 1000)]
    limit: usize,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // PART 7: mutual-exclusion on the per-tenant flags. They
    // require --audit-partition AND only one may apply per
    // run.
    if args.tenant.is_some() && args.all_tenants {
        return Err("--tenant and --all-tenants are mutually exclusive".into());
    }
    if (args.tenant.is_some() || args.all_tenants) && args.audit_partition.is_none() {
        return Err(
            "--tenant / --all-tenants require --audit-partition <path>; the signed audit \
             log does not carry tenant_id (the field lives on the partition mirror only)"
                .into(),
        );
    }
    match (
        args.flow.as_ref(),
        args.audit.as_ref(),
        args.audit_partition.as_ref(),
    ) {
        (Some(flow_path), _, _) => handle_flow(flow_path, &args),
        (None, Some(audit_path), _) => handle_audit(audit_path, &args),
        (None, None, Some(partition_path)) => handle_audit_partition(
            partition_path,
            &args,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
        ),
        (None, None, None) => Err("provide --flow <path>, --audit <path>, \
             or --audit-partition <path>"
            .into()),
    }
}

fn handle_flow(flow_path: &PathBuf, args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let recs = eventlog::read_records(flow_path)?;
    if args.replay_verify {
        let key_path = args.signer_key.as_ref().ok_or_else(|| {
            "--replay-verify requires --signer-key <owner-signing-key>".to_string()
        })?;
        // SEC PART 2: the 32-byte ed25519 secret-key bytes
        // live inside `Zeroizing<Vec<u8>>` so they're wiped
        // when this scope ends (the `SigningKey` itself
        // zeroizes its inner buffer on drop too, but the
        // intermediate disk read would otherwise leave the
        // raw bytes on the heap past the verify call).
        let bytes: Zeroizing<Vec<u8>> = Zeroizing::new(std::fs::read(key_path)?);
        if bytes.len() != 32 {
            return Err("signer key must be 32 raw bytes".into());
        }
        let mut arr = Zeroizing::new([0u8; 32]);
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

/// PART 7 — read the audit partition mirror with the
/// per-tenant flags applied. `stdin` / `stdout` are injected
/// so the confirmation prompt is testable without touching
/// real terminal handles. Returns the documented errors
/// verbatim so the operator's tooling can grep them.
fn handle_audit_partition(
    path: &PathBuf,
    args: &Args,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = AuditPartitionStore::open(path)
        .map_err(|e| format!("open audit partition {}: {e}", path.display()))?;

    // Resolve the mode this invocation runs in.
    match (args.tenant.as_deref(), args.all_tenants) {
        (Some(_), true) => unreachable!("main() rejected this combination"),
        (Some(tenant_id), false) => print_tenant_recent(&store, tenant_id, args, stdout),
        (None, true) => {
            // PART 7: --all-tenants requires confirmation
            // unless the operator opted out via --yes.
            if !args.yes && !confirm_all_tenants(stdin, stdout)? {
                writeln!(stdout, "Aborted; no records read.")?;
                return Ok(());
            }
            print_all_tenants(&store, args, stdout)
        }
        (None, false) => {
            if args.multi_tenant_mode {
                Err("In multi-tenant mode you must specify --tenant <id> or --all-tenants.".into())
            } else {
                // Single-tenant mode without a tenant filter:
                // print every row regardless of tenant (the
                // store has only the `default` bucket in
                // single-tenant deployments).
                print_all_tenants(&store, args, stdout)
            }
        }
    }
}

/// PART 7 — prompt the operator on `stdin` and accept ONLY
/// the literal string `yes` (case-insensitive, trimmed).
/// Anything else returns `Ok(false)` so the caller exits
/// cleanly.
fn confirm_all_tenants(
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
) -> Result<bool, Box<dyn std::error::Error>> {
    write!(
        stdout,
        "This will display audit records for ALL tenants. Type 'yes' to confirm: "
    )?;
    stdout.flush()?;
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

/// PART 7 — print one tenant's rows. Calls
/// `AuditPartitionStore::tenant_recent` which itself ships
/// the `WHERE tenant_id = ?1` clause.
fn print_tenant_recent(
    store: &AuditPartitionStore,
    tenant_id: &str,
    args: &Args,
    stdout: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let rows = store
        .tenant_recent(tenant_id, args.limit)
        .map_err(|e| format!("tenant_recent({tenant_id}): {e}"))?;
    writeln!(
        stdout,
        "audit partition rows for tenant={tenant_id}: {}",
        rows.len()
    )?;
    for r in &rows {
        write_partition_row(stdout, r, args.human)?;
    }
    Ok(())
}

/// PART 7 — print every tenant's rows grouped by tenant id.
/// A separator line precedes each tenant's group so the
/// boundaries are visible in operator pipes.
fn print_all_tenants(
    store: &AuditPartitionStore,
    args: &Args,
    stdout: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
    let tenants = store
        .list_tenants()
        .map_err(|e| format!("list_tenants: {e}"))?;
    if tenants.is_empty() {
        writeln!(stdout, "audit partition is empty")?;
        return Ok(());
    }
    let mut total = 0usize;
    for (i, tenant) in tenants.iter().enumerate() {
        if i > 0 {
            // Separator between tenant groups.
            writeln!(stdout, "{}", "-".repeat(72))?;
        }
        let rows = store
            .tenant_recent(tenant, args.limit)
            .map_err(|e| format!("tenant_recent({tenant}): {e}"))?;
        writeln!(stdout, "tenant={tenant} rows={}", rows.len())?;
        for r in &rows {
            write_partition_row(stdout, r, args.human)?;
        }
        total += rows.len();
    }
    writeln!(
        stdout,
        "# total rows across {} tenant(s): {total}",
        tenants.len()
    )?;
    Ok(())
}

/// PART 7 — single-row renderer shared by both paths.
fn write_partition_row(
    stdout: &mut dyn Write,
    r: &relix_runtime::audit_partition::PartitionReadRow,
    human: bool,
) -> std::io::Result<()> {
    if human {
        writeln!(stdout, "  ts={} rid={}", r.ts_secs, r.request_id)?;
        writeln!(
            stdout,
            "    caller={} method={} status={}",
            r.caller_name, r.method, r.status
        )?;
        writeln!(stdout, "    policy={}", r.policy_decision)?;
        if let Some(k) = r.error_kind {
            writeln!(stdout, "    error_kind={k}")?;
        }
        writeln!(
            stdout,
            "    latency_ms={} tenant={}",
            r.latency_ms, r.tenant_id
        )?;
    } else {
        writeln!(
            stdout,
            "ts={} rid={} tenant={} caller={} method={} status={} policy={}",
            r.ts_secs,
            r.request_id,
            r.tenant_id,
            r.caller_name,
            r.method,
            r.status,
            r.policy_decision
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::audit_partition::PartitionRow;
    use tempfile::TempDir;

    /// Helper: build an `Args` skeleton for the audit-
    /// partition tests. Every field defaults to its CLI
    /// default; tests override the bits they care about.
    fn args_for_partition() -> Args {
        Args {
            flow: None,
            audit: None,
            replay_verify: false,
            signer_key: None,
            human: false,
            trace: None,
            rid: None,
            audit_partition: None,
            tenant: None,
            all_tenants: false,
            multi_tenant_mode: false,
            yes: false,
            limit: 1000,
        }
    }

    fn row(tenant: Option<&str>, ts: i64, rid: &str, method: &str) -> PartitionRow {
        PartitionRow {
            ts_secs: ts,
            request_id_hex: rid.to_string(),
            tenant_id: tenant.map(str::to_string),
            caller_name: "alice".into(),
            method: method.into(),
            policy_decision: "allow:r".into(),
            status: "ok",
            error_kind: None,
            latency_ms: 5,
        }
    }

    fn open_store_with_seed() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("tmp");
        let path = tmp.path().join("audit.db");
        let store = AuditPartitionStore::open(&path).expect("open");
        store
            .append(&row(Some("acme"), 100, "a1", "ai.chat"))
            .expect("append a1");
        store
            .append(&row(Some("acme"), 200, "a2", "ai.chat"))
            .expect("append a2");
        store
            .append(&row(Some("globex"), 300, "g1", "tool.web_fetch"))
            .expect("append g1");
        (tmp, path)
    }

    #[test]
    fn fix_part7_tenant_filter_returns_only_matching_rows() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.tenant = Some("acme".into());
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("audit partition rows for tenant=acme: 2"),
            "tenant filter must apply: {text}"
        );
        // The globex row must NOT appear.
        assert!(
            !text.contains("tool.web_fetch"),
            "globex row leaked: {text}"
        );
        assert!(text.contains("rid=a1"));
        assert!(text.contains("rid=a2"));
    }

    #[test]
    fn fix_part7_all_tenants_requires_yes_confirmation() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.all_tenants = true;
        // Type something other than 'yes'.
        let mut stdin = std::io::Cursor::new(b"no\n".to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("This will display audit records for ALL tenants"),
            "prompt missing: {text}"
        );
        assert!(text.contains("Aborted"), "abort line missing: {text}");
        assert!(
            !text.contains("tenant=acme"),
            "rows leaked despite abort: {text}"
        );
    }

    #[test]
    fn fix_part7_all_tenants_with_yes_prints_grouped_rows() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.all_tenants = true;
        let mut stdin = std::io::Cursor::new(b"yes\n".to_vec());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("tenant=acme rows=2"));
        assert!(text.contains("tenant=globex rows=1"));
        // Separator line between the two tenant groups.
        assert!(text.contains("---"));
        assert!(text.contains("total rows across 2 tenant(s): 3"));
    }

    #[test]
    fn fix_part7_all_tenants_with_yes_flag_skips_prompt() {
        // `--yes` lets non-interactive pipelines skip the
        // confirmation prompt while still reading every
        // tenant.
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.all_tenants = true;
        args.yes = true;
        // EMPTY stdin would normally fail the confirmation
        // — `--yes` short-circuits the read.
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(!text.contains("Type 'yes' to confirm"));
        assert!(text.contains("tenant=acme rows=2"));
    }

    #[test]
    fn fix_part7_multi_tenant_mode_requires_a_filter() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.multi_tenant_mode = true;
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut stdout: Vec<u8> = Vec::new();
        let err = handle_audit_partition(&path, &args, &mut stdin, &mut stdout)
            .expect_err("multi-tenant mode must reject no-flag");
        let msg = err.to_string();
        assert!(
            msg.contains("In multi-tenant mode you must specify --tenant"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn fix_part7_single_tenant_mode_without_filter_prints_everything() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        // multi_tenant_mode = false → no filter is OK.
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("tenant=acme rows=2"));
        assert!(text.contains("tenant=globex rows=1"));
    }

    #[test]
    fn fix_part7_human_mode_renders_indented_multiline_per_row() {
        let (_tmp, path) = open_store_with_seed();
        let mut args = args_for_partition();
        args.audit_partition = Some(path.clone());
        args.tenant = Some("acme".into());
        args.human = true;
        let mut stdin = std::io::Cursor::new(Vec::new());
        let mut stdout: Vec<u8> = Vec::new();
        handle_audit_partition(&path, &args, &mut stdin, &mut stdout).expect("ok");
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("  ts=100 rid=a1"));
        assert!(text.contains("    caller=alice method=ai.chat"));
        assert!(text.contains("    policy=allow:r"));
        assert!(text.contains("    latency_ms=5 tenant=acme"));
    }

    #[test]
    fn fix_part7_confirmation_accepts_yes_case_insensitively_and_trimmed() {
        // YES + \n, with leading whitespace, both accepted.
        for input in [b"yes\n".as_slice(), b"YES\n", b"  Yes  \n"] {
            let mut stdin = std::io::Cursor::new(input.to_vec());
            let mut stdout: Vec<u8> = Vec::new();
            assert!(
                confirm_all_tenants(&mut stdin, &mut stdout).unwrap(),
                "must accept: {:?}",
                input
            );
        }
    }

    #[test]
    fn fix_part7_confirmation_rejects_anything_other_than_yes() {
        for input in [b"y\n".as_slice(), b"\n", b"no\n", b"y e s\n"] {
            let mut stdin = std::io::Cursor::new(input.to_vec());
            let mut stdout: Vec<u8> = Vec::new();
            assert!(
                !confirm_all_tenants(&mut stdin, &mut stdout).unwrap(),
                "must reject: {:?}",
                input
            );
        }
    }
}
