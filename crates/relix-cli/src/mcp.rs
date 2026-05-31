//! `relix-cli mcp ...` — operator surface for the MCP registry
//! on a tool node, plus a bridge-side audit mirror.
//!
//! Three subcommands:
//!
//! - PH-MCP-CLI `mcp servers` / `mcp tools` — read-only.
//!   Dial one tool-node peer over libp2p, present the caller's
//!   identity bundle, call `tool.mcp.list_servers` or
//!   `tool.mcp.list_tools` through the full admission pipeline
//!   (identity → policy → handler → audit), parse the tab-delim
//!   response, and pretty-print.
//!
//! - PH-CLI-MCP-AUDIT `mcp audit` — read-only HTTP mirror of
//!   the bridge's `GET /v1/mcp/audit` ring (PH-BRIDGE-MCP-AUDIT).
//!   Different surface from the other two: the audit ring lives
//!   in the bridge process, not on the tool node, so the CLI
//!   reaches it over HTTP (same shape as `relix-cli ops events`
//!   / `ops route-test`).
//!
//! Why a sibling of `relix-cli capability` rather than an
//! `ops mcp` HTTP-against-bridge subcommand for `servers` /
//! `tools`: the bridge proxies the registry now
//! (PH-BRIDGE-MCP) but originally didn't, and direct libp2p
//! dial is still the same shape as `relix-cli ping` /
//! `capability` — clearer when the operator is troubleshooting
//! a peer in isolation. `audit` has no libp2p equivalent — the
//! ring is bridge-only — so it lives here under HTTP as the
//! cleanest place for "mcp anything" to live.

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
    /// List MCP servers the tool node has registered. Tab-delim
    /// rows: id, transport (stdio|http), endpoint,
    /// declared_tool_count, status.
    Servers {
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
    /// List tools that a specific MCP server has declared (per
    /// the operator's config). Today this reads the configured
    /// `declared_tools` list; live discovery via `tools/list`
    /// lands when the stdio runtime is wired (D-009).
    Tools {
        /// Target tool-node libp2p multiaddr.
        #[arg(long)]
        peer: String,
        /// Caller's identity bundle.
        #[arg(long)]
        identity: PathBuf,
        /// 32-byte signing key.
        #[arg(long)]
        client_key: PathBuf,
        /// MCP server id (from `mcp servers`).
        #[arg(long)]
        server_id: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// PH-CLI-MCP-AUDIT: show the bridge's audit ring of
    /// `POST /v1/mcp/invoke` calls. Newest first. Bounded by
    /// the bridge (capacity 256); resets on bridge restart.
    Audit {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Maximum entries to fetch (capped server-side by ring
        /// capacity).
        #[arg(long, default_value_t = 50)]
        max: usize,
        /// Print raw JSON from the bridge instead of the
        /// formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Servers {
            peer,
            identity,
            client_key,
            raw,
        } => {
            let body = call_peer(
                &peer,
                &identity,
                &client_key,
                "tool.mcp.list_servers",
                Vec::new(),
            )
            .await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let rows = parse_servers(&body);
            if rows.is_empty() {
                println!("(no MCP servers configured)");
                return Ok(());
            }
            let id_h = "id";
            let tr_h = "transport";
            let ep_h = "endpoint";
            let tc_h = "tools";
            let st_h = "status";
            println!("{id_h:<16}  {tr_h:<6}  {ep_h:<40}  {tc_h:<6}  {st_h}");
            for r in &rows {
                let ep_trunc = truncate(&r.endpoint, 40);
                println!(
                    "{:<16}  {:<6}  {:<40}  {:<6}  {}",
                    r.id, r.transport, ep_trunc, r.declared_tool_count, r.status,
                );
            }
        }
        Cmd::Tools {
            peer,
            identity,
            client_key,
            server_id,
            raw,
        } => {
            let arg = server_id.into_bytes();
            let body = call_peer(&peer, &identity, &client_key, "tool.mcp.list_tools", arg).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let tools = parse_tools(&body);
            if tools.is_empty() {
                println!("(server declared no tools, or live discovery not yet wired)");
                return Ok(());
            }
            for t in &tools {
                println!("{t}");
            }
            println!("count={}", tools.len());
        }
        Cmd::Audit { bridge, max, raw } => {
            let url = format!("{}/v1/mcp/audit?max={max}", bridge.trim_end_matches('/'),);
            let body = http_get(&url).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let parsed: AuditResp = serde_json::from_str(&body)
                .map_err(|e| format!("decode /v1/mcp/audit body: {e} (body={body})"))?;
            render_audit(&parsed);
        }
    }
    Ok(())
}

/// PH-CLI-MCP-AUDIT: mirror of the bridge's
/// `mcp_audit::McpAuditEntry`. Field-for-field — `error_kind`
/// is optional, matching the bridge's
/// `#[serde(skip_serializing_if = "Option::is_none")]` on the
/// success path.
#[derive(Debug, Deserialize)]
struct AuditEntry {
    #[serde(default)]
    ts_secs: i64,
    #[serde(default)]
    peer_alias: String,
    #[serde(default)]
    server_id: String,
    #[serde(default)]
    tool_name: String,
    #[serde(default)]
    args_len: usize,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    error_kind: Option<String>,
    #[serde(default)]
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct AuditResp {
    #[serde(default)]
    entries: Vec<AuditEntry>,
    #[serde(default)]
    count: usize,
}

fn render_audit(resp: &AuditResp) {
    if resp.entries.is_empty() {
        println!(
            "(no invocations recorded yet — ring count={}; make a \
             POST /v1/mcp/invoke call and retry)",
            resp.count
        );
        return;
    }
    let ts_h = "ts";
    let peer_h = "peer";
    let srv_h = "server";
    let tool_h = "tool";
    let args_h = "args";
    let oc_h = "outcome";
    let err_h = "error_kind";
    let dur_h = "ms";
    println!(
        "{ts_h:<10}  {peer_h:<10}  {srv_h:<12}  {tool_h:<20}  \
         {args_h:>6}  {oc_h:<7}  {err_h:<28}  {dur_h:>6}",
    );
    for e in &resp.entries {
        let srv = truncate(&e.server_id, 12);
        let tool = truncate(&e.tool_name, 20);
        let peer = truncate(&e.peer_alias, 10);
        let err = truncate(e.error_kind.as_deref().unwrap_or(""), 28);
        println!(
            "{ts:<10}  {peer:<10}  {srv:<12}  {tool:<20}  \
             {args:>6}  {oc:<7}  {err:<28}  {dur:>6}",
            ts = e.ts_secs,
            peer = peer,
            srv = srv,
            tool = tool,
            args = e.args_len,
            oc = e.outcome,
            err = err,
            dur = e.duration_ms,
        );
    }
    if resp.entries.len() < resp.count {
        println!(
            "(showing {shown} of {total} total; rerun with --max to see more)",
            shown = resp.entries.len(),
            total = resp.count,
        );
    } else {
        println!("count={}", resp.count);
    }
}

/// PH-CLI-MCP-AUDIT: small GET helper. Mirrors `ops::http_get`
/// (same shape; intentionally duplicated to keep `mcp.rs`
/// standalone — see PH-CLI-DIAL-REFACTOR in the future-cleanup
/// list).
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
struct ServerRow {
    id: String,
    transport: String,
    endpoint: String,
    declared_tool_count: String,
    status: String,
}

/// PH-MCP-CLI: parse the tab-delim body returned by
/// `tool.mcp.list_servers` (see relix-runtime ::tool::mcp).
/// Lines:
///   `id\ttransport\tendpoint\tdeclared_tool_count\tstatus`
/// Final line is `count=<N>` (ignored on the parse side; the
/// table view derives count from the row vec).
fn parse_servers(body: &str) -> Vec<ServerRow> {
    let mut rows = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        rows.push(ServerRow {
            id: parts[0].to_string(),
            transport: parts[1].to_string(),
            endpoint: parts[2].to_string(),
            declared_tool_count: parts[3].to_string(),
            status: parts[4].to_string(),
        });
    }
    rows
}

/// PH-MCP-CLI: parse the tab-delim body returned by
/// `tool.mcp.list_tools` (one tool name per line, then
/// `count=<N>`). Returns just the names.
fn parse_tools(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.starts_with("count=") && !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Dial the peer, present identity, invoke `method` with `args`,
/// and return the body as a UTF-8 string. Mirrors the
/// dial-and-call pattern in `capability::fetch_manifest`.
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
    fn parse_servers_two_rows_with_count_trailer() {
        let body = "alpha\tstdio\tmcp-server\t5\tconfigured\nbeta\thttp\thttps://example.com\t0\tconfigured\ncount=2\n";
        let rows = parse_servers(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "alpha");
        assert_eq!(rows[0].transport, "stdio");
        assert_eq!(rows[0].endpoint, "mcp-server");
        assert_eq!(rows[0].declared_tool_count, "5");
        assert_eq!(rows[0].status, "configured");
        assert_eq!(rows[1].id, "beta");
        assert_eq!(rows[1].endpoint, "https://example.com");
    }

    #[test]
    fn parse_servers_skips_blank_and_count_lines() {
        let body = "\ncount=0\n";
        assert!(parse_servers(body).is_empty());
    }

    #[test]
    fn parse_servers_drops_malformed_rows() {
        // Row missing fields (only 3 cols instead of 5) — drop.
        let body = "broken\tstdio\tonly-three\ncount=0\n";
        assert!(parse_servers(body).is_empty());
    }

    #[test]
    fn parse_tools_returns_names_only() {
        let body = "search\nfetch\nclick\ncount=3\n";
        let tools = parse_tools(body);
        assert_eq!(tools, vec!["search", "fetch", "click"]);
    }

    #[test]
    fn parse_tools_skips_count_trailer_and_blanks() {
        let body = "\nsearch\n\ncount=1\n";
        assert_eq!(parse_tools(body), vec!["search"]);
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

    #[test]
    fn truncate_handles_multibyte_chars() {
        let s = truncate("héllo wörld foo bar", 10);
        // We assert that the byte-count fits in <= max chars by
        // counting chars on the result.
        assert!(s.chars().count() <= 10);
    }

    // ── PH-CLI-MCP-AUDIT: shape parsing + renderer guard rails ───────

    #[test]
    fn audit_resp_round_trips_through_serde() {
        // Mirrors the bridge's `mcp_audit::McpAuditEntry` JSON
        // shape. The bridge omits `error_kind` on ok via
        // `skip_serializing_if = "Option::is_none"` — the CLI
        // must accept both forms.
        let json = r#"{
            "entries": [
                {
                    "ts_secs": 100,
                    "peer_alias": "tool",
                    "server_id": "srv-a",
                    "tool_name": "search",
                    "args_len": 12,
                    "outcome": "ok",
                    "duration_ms": 42
                },
                {
                    "ts_secs": 200,
                    "peer_alias": "tool",
                    "server_id": "srv-b",
                    "tool_name": "fetch",
                    "args_len": 0,
                    "outcome": "err",
                    "error_kind": "responder_runtime_not_connected",
                    "duration_ms": 5
                }
            ],
            "count": 2
        }"#;
        let parsed: AuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].tool_name, "search");
        assert_eq!(parsed.entries[0].error_kind, None);
        assert_eq!(
            parsed.entries[1].error_kind.as_deref(),
            Some("responder_runtime_not_connected")
        );
    }

    #[test]
    fn audit_resp_tolerates_missing_count_and_entries() {
        // Defensive: bridge always sends both, but the CLI
        // should not crash on a partial body (e.g. an older
        // bridge build, or a manually crafted curl response).
        let parsed: AuditResp = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.count, 0);
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn audit_resp_unknown_fields_are_ignored() {
        // Forward-compat: when the bridge grows a new field,
        // an older CLI build should still parse the response.
        let json = r#"{
            "count": 1,
            "entries": [{
                "ts_secs": 1,
                "peer_alias": "x",
                "server_id": "y",
                "tool_name": "z",
                "args_len": 0,
                "outcome": "ok",
                "duration_ms": 0,
                "future_field": "shrug"
            }],
            "future_top_level": 99
        }"#;
        let parsed: AuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.entries[0].tool_name, "z");
    }
}
