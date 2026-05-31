//! `relix-cli fs ...` — PH-CLI-AUDIT-MIRRORS operator surface
//! for the filesystem audit ring on a tool node.
//!
//! HTTP mirror of the bridge's `GET /v1/fs/audit` endpoint
//! (PH-BRIDGE-FS-AUDIT). Same shape as `relix-cli mcp audit`:
//! one read-only subcommand, padded table render, `--raw`
//! escape hatch for verbatim JSON.
//!
//! No libp2p variant lives under `fs` today — `tool.fs.audit_recent`
//! can be reached directly via `relix-cli capability invoke …`
//! but the dedicated CLI surface is the bridge HTTP path because
//! it's what an operator running the dashboard is most likely to
//! have running anyway. A future PH-CLI-FS would add the libp2p
//! sibling commands (write / read / patch / search) under this
//! same module.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// PH-CLI-AUDIT-MIRRORS: show the per-jail mutation ring on
    /// the tool node, fetched through the bridge proxy. Newest
    /// first. Bounded server-side; resets on tool-node restart.
    Audit {
        /// Bridge HTTP base URL (e.g. `http://127.0.0.1:19791`).
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        /// Target peer alias (default `tool`).
        #[arg(long, default_value = "tool")]
        peer: String,
        /// Maximum rows to fetch. Server clamps to ring capacity.
        #[arg(long, default_value_t = 50usize)]
        max: usize,
        /// Optional op filter — one of `write`, `append`, `patch`,
        /// `fuzzy_replace`. Unknown ops are rejected by the
        /// responder with HTTP 400.
        #[arg(long)]
        op: Option<String>,
        /// Print raw JSON from the bridge instead of the
        /// formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Audit {
            bridge,
            peer,
            max,
            op,
            raw,
        } => {
            let mut url = format!(
                "{}/v1/fs/audit?peer={peer}&max={max}",
                bridge.trim_end_matches('/'),
            );
            if let Some(o) = op.as_deref()
                && !o.is_empty()
            {
                url.push_str("&op=");
                url.push_str(&urlencode_token(o));
            }
            let body = http_get(&url).await?;
            if raw {
                print_raw(&body);
                return Ok(());
            }
            let parsed: AuditResp = serde_json::from_str(&body)
                .map_err(|e| format!("decode /v1/fs/audit body: {e} (body={body})"))?;
            render_audit(&parsed);
        }
    }
    Ok(())
}

/// PH-CLI-AUDIT-MIRRORS: mirror of the bridge's
/// `fs_audit::FsAuditRow`. Field-for-field.
#[derive(Debug, Deserialize)]
struct AuditRow {
    #[serde(default)]
    ts_secs: i64,
    #[serde(default)]
    op: String,
    #[serde(default)]
    rel_path: String,
    #[serde(default)]
    bytes: usize,
    #[serde(default)]
    caller_subject_id: String,
}

#[derive(Debug, Deserialize)]
struct AuditResp {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    entries: Vec<AuditRow>,
    #[serde(default)]
    count: usize,
}

fn render_audit(resp: &AuditResp) {
    if resp.entries.is_empty() {
        println!(
            "(no fs mutations recorded yet on peer '{p}' — ring count={c})",
            p = resp.peer,
            c = resp.count
        );
        return;
    }
    let ts_h = "ts";
    let op_h = "op";
    let path_h = "path";
    let bytes_h = "bytes";
    let caller_h = "caller";
    println!("{ts_h:<10}  {op_h:<14}  {path_h:<40}  {bytes_h:>8}  {caller_h}");
    for e in &resp.entries {
        let path = truncate(&e.rel_path, 40);
        let caller = truncate(&e.caller_subject_id, 16);
        println!(
            "{ts:<10}  {op:<14}  {path:<40}  {bytes:>8}  {caller}",
            ts = e.ts_secs,
            op = e.op,
            path = path,
            bytes = e.bytes,
            caller = caller,
        );
    }
    println!("count={}", resp.count);
}

/// PH-CLI-AUDIT-MIRRORS: small GET helper. Duplicated from
/// `mcp::http_get` — see PH-CLI-DIAL-REFACTOR in the future-
/// cleanup list.
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

/// Minimal URL-encode for op-filter values. The responder only
/// accepts a closed set (`write`, `append`, `patch`,
/// `fuzzy_replace`) so encoding is trivial — but defensive
/// against an operator passing something unexpected through.
fn urlencode_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' | b'-' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_resp_round_trips_through_serde() {
        let json = r#"{
            "peer": "tool",
            "entries": [
                {"ts_secs": 100, "op": "write", "rel_path": "a.md",
                 "bytes": 42, "caller_subject_id": "f00b"},
                {"ts_secs": 200, "op": "patch", "rel_path": "src/main.rs",
                 "bytes": 9001, "caller_subject_id": "beef"}
            ],
            "count": 2
        }"#;
        let parsed: AuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.peer, "tool");
        assert_eq!(parsed.count, 2);
        assert_eq!(parsed.entries[0].op, "write");
        assert_eq!(parsed.entries[1].bytes, 9001);
    }

    #[test]
    fn audit_resp_tolerates_missing_top_level_fields() {
        let parsed: AuditResp = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.peer, "");
        assert_eq!(parsed.count, 0);
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn audit_resp_ignores_unknown_fields() {
        // Forward-compat: a new bridge field shouldn't break an
        // older CLI build.
        let json = r#"{
            "peer": "tool",
            "count": 1,
            "future_field": 99,
            "entries": [{
                "ts_secs": 1, "op": "write", "rel_path": "x",
                "bytes": 1, "caller_subject_id": "c",
                "future_field": "shrug"
            }]
        }"#;
        let parsed: AuditResp = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.count, 1);
        assert_eq!(parsed.entries[0].rel_path, "x");
    }

    #[test]
    fn urlencode_token_passes_through_safe_chars() {
        assert_eq!(urlencode_token("write"), "write");
        assert_eq!(urlencode_token("fuzzy_replace"), "fuzzy_replace");
        assert_eq!(urlencode_token("a-b.c~d"), "a-b.c~d");
    }

    #[test]
    fn urlencode_token_escapes_unsafe_chars() {
        assert_eq!(urlencode_token("a b"), "a%20b");
        assert_eq!(urlencode_token("x|y"), "x%7Cy");
        // Quote, semicolon — the kinds of chars an attacker might
        // pass; even if encoded the responder still rejects them
        // as unknown ops, but we never want them traveling
        // verbatim through the URL.
        assert_eq!(urlencode_token("w\""), "w%22");
        assert_eq!(urlencode_token("a;b"), "a%3Bb");
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
}
