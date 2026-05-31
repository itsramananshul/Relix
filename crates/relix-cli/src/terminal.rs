//! `relix-cli terminal ...` — operator surface for
//! `tool.terminal.*` observability + control on a tool node.
//!
//! Two transports under one parent:
//!
//! - PH-TERM-CLI libp2p path: `sessions` / `audit` / `cancel`
//!   dial one tool-node peer directly over libp2p, present the
//!   caller's identity bundle, call the capability through the
//!   admission pipeline, parse the tab-delim response, and
//!   pretty-print. Read-only by default; `cancel` is the only
//!   mutation.
//!
//! - PH-CLI-AUDIT-MIRRORS HTTP path: `audit-http` hits the
//!   bridge's `GET /v1/terminal/audit` (PH-BRIDGE-TERM-AUDIT),
//!   parses the structured JSON, and renders the same shape as
//!   the libp2p audit. Lives here rather than as a separate
//!   subcommand so operators can `terminal audit-http` without
//!   needing the identity bundle on disk.
//!
//! Sibling of `relix-cli mcp` for the same reasons (PH-MCP-CLI
//! module doc): libp2p dial is the same shape as `relix-cli ping`,
//! and the HTTP variant lets the dashboard-running operator
//! skip identity setup entirely.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List currently-running tool.terminal.run / spawn / shell
    /// sessions. Tab-delim rows: session_id, pid, command,
    /// started_at (unix secs), timeout_secs, caller. Newest
    /// first.
    Sessions {
        /// Target tool-node libp2p multiaddr.
        #[arg(long)]
        peer: String,
        /// Caller's identity bundle (from `relix-cli identity mint`).
        #[arg(long)]
        identity: PathBuf,
        /// 32-byte signing key used as the local libp2p PeerId.
        #[arg(long)]
        client_key: PathBuf,
        /// Raw tab-delim body from the responder instead of the
        /// formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Snapshot the most recent completed tool.terminal runs
    /// from the bounded audit ring. Includes normal exits,
    /// timeouts, and cancels (mutually exclusive per row).
    /// Newest first.
    Audit {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Maximum rows to fetch (server caps at 256).
        #[arg(long, default_value_t = 50usize)]
        max: usize,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Trigger cooperative cancel for a live session by id.
    /// Mirrors `tool.terminal.cancel`; the run task observes
    /// the cancel notify, kills the child, and the next audit
    /// row for that command will carry cancelled=true.
    Cancel {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Session id from `terminal sessions`.
        #[arg(long)]
        session_id: String,
    },
    /// PH-CLI-AUDIT-MIRRORS: same content as `audit` but
    /// fetched via the bridge's HTTP proxy
    /// (`GET /v1/terminal/audit`, PH-BRIDGE-TERM-AUDIT). Useful
    /// when an identity bundle isn't already on disk — the
    /// bridge does the dial-and-call on the operator's behalf.
    AuditHttp {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias (default `tool`).
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Maximum rows to fetch. Server clamps to ring capacity.
        #[arg(long, default_value_t = 50usize)]
        max: usize,
        /// Print raw JSON from the bridge instead of the
        /// formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Sessions {
            peer,
            identity,
            client_key,
            raw,
        } => {
            let body = call_peer(
                &peer,
                &identity,
                &client_key,
                "tool.terminal.sessions",
                Vec::new(),
            )
            .await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let rows = parse_sessions(&body);
            if rows.is_empty() {
                println!("(no live terminal sessions)");
                return Ok(());
            }
            let id_h = "session_id";
            let pid_h = "pid";
            let cmd_h = "command";
            let st_h = "started_at";
            let to_h = "timeout";
            let cl_h = "caller";
            println!("{id_h:<18}  {pid_h:<7}  {cmd_h:<24}  {st_h:<11}  {to_h:<8}  {cl_h}");
            for r in &rows {
                let cmd_trunc = truncate(&r.command, 24);
                let caller_short = truncate(&r.caller_subject_id, 16);
                println!(
                    "{:<18}  {:<7}  {:<24}  {:<11}  {:<8}  {}",
                    r.session_id, r.pid, cmd_trunc, r.started_at, r.timeout_secs, caller_short,
                );
            }
            println!("count={}", rows.len());
        }
        Cmd::Audit {
            peer,
            identity,
            client_key,
            max,
            raw,
        } => {
            let arg = if max == 256 {
                Vec::new()
            } else {
                max.to_string().into_bytes()
            };
            let body = call_peer(
                &peer,
                &identity,
                &client_key,
                "tool.terminal.audit_recent",
                arg,
            )
            .await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let rows = parse_audit(&body);
            if rows.is_empty() {
                println!("(no completed terminal runs in ring)");
                return Ok(());
            }
            let ts_h = "ts";
            let cmd_h = "command";
            let ec_h = "exit";
            let d_h = "duration_ms";
            let st_h = "status";
            let cl_h = "caller";
            println!("{ts_h:<11}  {cmd_h:<24}  {ec_h:<5}  {d_h:<11}  {st_h:<10}  {cl_h}");
            for r in &rows {
                let cmd_trunc = truncate(&r.command, 24);
                let status = if r.cancelled == "true" {
                    "cancelled"
                } else if r.timed_out == "true" {
                    "timed_out"
                } else {
                    "ok"
                };
                let caller_short = truncate(&r.caller_subject_id, 16);
                println!(
                    "{:<11}  {:<24}  {:<5}  {:<11}  {:<10}  {}",
                    r.ts_secs, cmd_trunc, r.exit_code, r.duration_ms, status, caller_short,
                );
            }
            println!("count={}", rows.len());
        }
        Cmd::Cancel {
            peer,
            identity,
            client_key,
            session_id,
        } => {
            let arg = session_id.clone().into_bytes();
            let body =
                call_peer(&peer, &identity, &client_key, "tool.terminal.cancel", arg).await?;
            // tool.terminal.cancel returns `ok session=<id>\n`
            // on hit. Print whatever the responder said.
            print_raw(&body);
        }
        Cmd::AuditHttp {
            bridge,
            peer,
            max,
            raw,
        } => {
            let url = format!(
                "{}/v1/terminal/audit?peer={peer}&max={max}",
                bridge.trim_end_matches('/'),
            );
            let body = http_get(&url).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let parsed: HttpAuditResp = serde_json::from_str(&body)
                .map_err(|e| format!("decode /v1/terminal/audit body: {e} (body={body})"))?;
            render_http_audit(&parsed);
        }
    }
    Ok(())
}

/// PH-CLI-AUDIT-MIRRORS: bridge wire-shape mirror of
/// `term_audit::TermAuditRow`. `exit_code` is `Option<i32>` —
/// the bridge emits `None` for killed children (responder sent
/// `"?"`) and the JSON omits the field via
/// `skip_serializing_if`. Both shapes deserialize cleanly.
#[derive(Debug, Deserialize)]
struct HttpAuditRow {
    #[serde(default)]
    ts_secs: i64,
    #[serde(default)]
    command: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    caller_subject_id: String,
}

#[derive(Debug, Deserialize)]
struct HttpAuditResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    entries: Vec<HttpAuditRow>,
    #[serde(default)]
    count: usize,
}

fn render_http_audit(resp: &HttpAuditResp) {
    if resp.entries.is_empty() {
        println!(
            "(no terminal runs recorded yet on peer '{p}' — ring count={c})",
            p = resp.peer,
            c = resp.count
        );
        return;
    }
    let ts_h = "ts";
    let cmd_h = "command";
    let exit_h = "exit";
    let dur_h = "ms";
    let status_h = "status";
    let caller_h = "caller";
    println!(
        "{ts_h:<10}  {cmd_h:<28}  {exit_h:>5}  {dur_h:>8}  \
         {status_h:<10}  {caller_h}",
    );
    for e in &resp.entries {
        let exit_str = match e.exit_code {
            Some(c) => c.to_string(),
            None => "—".to_string(),
        };
        let status = if e.timed_out {
            "timed_out"
        } else if e.cancelled {
            "cancelled"
        } else if e.exit_code.is_none() {
            "killed"
        } else if e.exit_code == Some(0) {
            "ok"
        } else {
            "nonzero"
        };
        let cmd = truncate(&e.command, 28);
        let caller = truncate(&e.caller_subject_id, 16);
        println!(
            "{ts:<10}  {cmd:<28}  {exit:>5}  {dur:>8}  {status:<10}  {caller}",
            ts = e.ts_secs,
            cmd = cmd,
            exit = exit_str,
            dur = e.duration_ms,
            status = status,
            caller = caller,
        );
    }
    println!("count={}", resp.count);
}

/// PH-CLI-AUDIT-MIRRORS: small GET helper. Duplicated from
/// `mcp::http_get` / `fs::http_get` — see PH-CLI-DIAL-REFACTOR
/// for the future hoist.
async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

fn print_raw(body: &str) {
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[derive(Debug, Clone)]
struct SessionRow {
    session_id: String,
    pid: String,
    command: String,
    started_at: String,
    timeout_secs: String,
    caller_subject_id: String,
}

#[derive(Debug, Clone)]
struct AuditRow {
    ts_secs: String,
    command: String,
    exit_code: String,
    duration_ms: String,
    timed_out: String,
    cancelled: String,
    caller_subject_id: String,
}

/// PH-TERM-CLI: parse the tab-delim body returned by
/// `tool.terminal.sessions` (see relix-runtime ::tool::terminal).
/// Format:
///   `session_id\tpid\tcommand\tstarted_at\ttimeout_secs\tcaller`
/// Trailing `count=N` line. Order: newest first.
fn parse_sessions(body: &str) -> Vec<SessionRow> {
    let mut rows = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            continue;
        }
        rows.push(SessionRow {
            session_id: parts[0].to_string(),
            pid: parts[1].to_string(),
            command: parts[2].to_string(),
            started_at: parts[3].to_string(),
            timeout_secs: parts[4].to_string(),
            caller_subject_id: parts[5].to_string(),
        });
    }
    rows
}

/// PH-TERM-CLI: parse the tab-delim body returned by
/// `tool.terminal.audit_recent`. Format:
///   `ts_secs\tcommand\texit_code\tduration_ms\ttimed_out\tcancelled\tcaller`
/// Trailing `count=N` line. Order: newest first.
fn parse_audit(body: &str) -> Vec<AuditRow> {
    let mut rows = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            continue;
        }
        rows.push(AuditRow {
            ts_secs: parts[0].to_string(),
            command: parts[1].to_string(),
            exit_code: parts[2].to_string(),
            duration_ms: parts[3].to_string(),
            timed_out: parts[4].to_string(),
            cancelled: parts[5].to_string(),
            caller_subject_id: parts[6].to_string(),
        });
    }
    rows
}

/// Dial the peer, present identity, invoke `method` with `args`,
/// and return the body as a UTF-8 string. Mirrors the
/// dial-and-call pattern in `mcp::call_peer` /
/// `capability::fetch_manifest`. Future PH-CLI-DIAL-REFACTOR
/// could extract this into a shared module.
async fn call_peer(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
    method: &str,
    args: Vec<u8>,
) -> Result<String, Box<dyn std::error::Error>> {
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

    let envelope = build_request(method, args, bundle, 10);
    let resp_bytes = client
        .call(connected, envelope)
        .await
        .map_err(|e| format!("rpc: {e}"))?;
    let resp = decode_response(&resp_bytes)?;
    let body = match resp.res {
        ResponseResult::Ok(b) => b.to_vec(),
        ResponseResult::Err(e) => {
            eprintln!("ERR kind={} cause={}", e.kind, e.cause);
            std::process::exit(2);
        }
        ResponseResult::StreamHandle(_) => {
            return Err(format!("unexpected stream response from {method}").into());
        }
    };
    Ok(String::from_utf8(body).map_err(|e| format!("body utf8: {e}"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sessions_two_rows_with_count_trailer() {
        let body = "abc123\t4242\techo\t1700000000\t30\tdeadbeef\n\
                    def456\t9001\tls\t1700000100\t60\tcafebabe\n\
                    count=2\n";
        let rows = parse_sessions(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, "abc123");
        assert_eq!(rows[0].pid, "4242");
        assert_eq!(rows[0].command, "echo");
        assert_eq!(rows[1].session_id, "def456");
        assert_eq!(rows[1].command, "ls");
    }

    #[test]
    fn parse_sessions_skips_blank_and_count_lines() {
        let body = "\ncount=0\n";
        assert!(parse_sessions(body).is_empty());
    }

    #[test]
    fn parse_sessions_drops_malformed_rows() {
        // 4 fields instead of 6 — drop.
        let body = "broken\t4242\techo\t1700000000\ncount=0\n";
        assert!(parse_sessions(body).is_empty());
    }

    #[test]
    fn parse_audit_full_row_with_cancelled_distinct_from_timed_out() {
        let body = "1700000000\techo\t0\t5\tfalse\tfalse\taaaa\n\
                    1700000050\tsleep\t?\t30000\ttrue\tfalse\tbbbb\n\
                    1700000099\tls\t?\t200\tfalse\ttrue\tcccc\n\
                    count=3\n";
        let rows = parse_audit(body);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].timed_out, "false");
        assert_eq!(rows[0].cancelled, "false");
        assert_eq!(rows[1].timed_out, "true");
        assert_eq!(rows[1].cancelled, "false");
        assert_eq!(rows[2].timed_out, "false");
        assert_eq!(rows[2].cancelled, "true");
    }

    #[test]
    fn parse_audit_skips_count_and_blanks() {
        let body = "\ncount=0\n";
        assert!(parse_audit(body).is_empty());
    }

    #[test]
    fn parse_audit_drops_malformed() {
        // 5 fields instead of 7 — drop.
        let body = "1700000000\techo\t0\t5\tfalse\ncount=0\n";
        assert!(parse_audit(body).is_empty());
    }

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        let s = truncate("abcdefghijklmnop", 8);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 8);
    }

    // ── PH-CLI-AUDIT-MIRRORS: bridge HTTP wire-shape ─────────────────

    #[test]
    fn http_audit_resp_round_trips_with_both_exit_shapes() {
        // Bridge omits exit_code on killed children via
        // skip_serializing_if=Option::is_none. The CLI must
        // accept both forms cleanly.
        let json = r#"{
            "peer": "tool",
            "entries": [
                {"ts_secs": 100, "command": "ls", "exit_code": 0,
                 "duration_ms": 12, "timed_out": false,
                 "cancelled": false, "caller_subject_id": "aaa"},
                {"ts_secs": 200, "command": "sleep",
                 "duration_ms": 5000, "timed_out": true,
                 "cancelled": false, "caller_subject_id": "bbb"}
            ],
            "count": 2
        }"#;
        let parsed: HttpAuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.peer, "tool");
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.entries[0].exit_code, Some(0));
        assert_eq!(parsed.entries[1].exit_code, None);
        assert!(parsed.entries[1].timed_out);
    }

    #[test]
    fn http_audit_resp_tolerates_missing_fields() {
        // Defensive: a partial body shouldn't crash the CLI.
        let parsed: HttpAuditResp = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.peer, "");
        assert_eq!(parsed.count, 0);
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn http_audit_resp_ignores_unknown_fields() {
        // Forward-compat: a new bridge field (e.g. exit_signal)
        // shouldn't break an older CLI build.
        let json = r#"{
            "peer": "tool",
            "count": 1,
            "future_field": 99,
            "entries": [{
                "ts_secs": 1, "command": "x", "exit_code": 0,
                "duration_ms": 0, "timed_out": false,
                "cancelled": false, "caller_subject_id": "c",
                "exit_signal": "SIGTERM"
            }]
        }"#;
        let parsed: HttpAuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.entries[0].command, "x");
    }
}
