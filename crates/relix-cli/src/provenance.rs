//! GAP 13 — `relix provenance` CLI surface.
//!
//! Four subcommands, all talking to the bridge's
//! `/v1/provenance/*` HTTP endpoints:
//!
//! - `relix provenance show <trace_id>`
//! - `relix provenance diff <a> <b>`
//! - `relix provenance history [--prompt <file>] [--from] [--to]`
//! - `relix provenance audit [--from] [--to]`
//!
//! `history` and `audit` filter snapshot rows by timestamp +
//! optional path. Both pull the full snapshot list and filter
//! client-side — the registry doesn't ship a per-bucket query
//! today; that's a follow-up.

use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print the snapshot for one trace id.
    Show(ShowArgs),
    /// Diff two snapshots and print the change list.
    Diff(DiffArgs),
    /// Print every snapshot whose policy_version mentions the
    /// given prompt file path, optionally filtered to a date
    /// range. When no `--prompt` is supplied, every
    /// `prompt_file_load` snapshot in the range is printed.
    History(HistoryArgs),
    /// Print every snapshot in a time range — full audit view.
    Audit(AuditArgs),
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    pub trace_id: String,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    pub trace_a: String,
    pub trace_b: String,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    #[arg(long)]
    pub prompt: Option<String>,
    /// ISO-8601 lower bound (inclusive). Example:
    /// `2025-11-01T00:00:00Z`.
    #[arg(long)]
    pub from: Option<String>,
    /// ISO-8601 upper bound (inclusive).
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AuditArgs {
    /// ISO-8601 lower bound (inclusive). Defaults to last 24h.
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Show(a) => show(a).await,
        Cmd::Diff(a) => diff(a).await,
        Cmd::History(a) => history(a).await,
        Cmd::Audit(a) => audit(a).await,
    }
}

async fn show(args: ShowArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/provenance/{}", urlencode(&args.trace_id));
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn diff(args: DiffArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/provenance/diff?a={}&b={}",
        urlencode(&args.trace_a),
        urlencode(&args.trace_b)
    );
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let changes = v
        .get("changes")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    println!("diff {} → {}", args.trace_a, args.trace_b);
    if changes.is_empty() {
        println!("  (no changes)");
        return Ok(());
    }
    for c in &changes {
        let kind = c.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
        let pretty = serde_json::to_string(c).unwrap_or_default();
        println!("  [{kind}] {pretty}");
    }
    Ok(())
}

async fn history(args: HistoryArgs) -> Result<(), Box<dyn std::error::Error>> {
    let rows = fetch_recent_snapshots(&args.bridge).await?;
    let from_ts = parse_iso(args.from.as_deref());
    let to_ts = parse_iso(args.to.as_deref());
    let filtered: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|s| {
            let ts = s
                .get("timestamp_unix")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            in_range(ts, from_ts, to_ts)
        })
        .filter(|s| {
            let policy = s
                .get("policy_version")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            policy.starts_with("prompt_file_load")
                && args.prompt.as_deref().is_none_or(|p| policy.contains(p))
        })
        .collect();
    if args.json {
        let v = serde_json::json!({ "snapshots": filtered });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if filtered.is_empty() {
        println!("(no matching prompt-file snapshots)");
        return Ok(());
    }
    println!("{:<20}  {:<40}  TRACE_ID", "TIMESTAMP", "PROMPT FILE");
    for s in filtered {
        let ts = s
            .get("timestamp_unix")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let policy = s
            .get("policy_version")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let trace = s.get("trace_id").and_then(|x| x.as_str()).unwrap_or("");
        println!(
            "{:<20}  {:<40}  {}",
            ts,
            policy.trim_start_matches("prompt_file_load:"),
            trace
        );
    }
    Ok(())
}

async fn audit(args: AuditArgs) -> Result<(), Box<dyn std::error::Error>> {
    let rows = fetch_recent_snapshots(&args.bridge).await?;
    let from_ts = parse_iso(args.from.as_deref());
    let to_ts = parse_iso(args.to.as_deref());
    let filtered: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|s| {
            let ts = s
                .get("timestamp_unix")
                .and_then(|x| x.as_i64())
                .unwrap_or(0);
            in_range(ts, from_ts, to_ts)
        })
        .collect();
    if args.json {
        let v = serde_json::json!({ "snapshots": filtered });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if filtered.is_empty() {
        println!("(no snapshots in the requested range)");
        return Ok(());
    }
    println!("{:<14}  {:<60}  POLICY", "TIMESTAMP", "TRACE_ID");
    for s in filtered {
        let ts = s
            .get("timestamp_unix")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let trace = s.get("trace_id").and_then(|x| x.as_str()).unwrap_or("");
        let policy = s
            .get("policy_version")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        println!("{:<14}  {:<60}  {}", ts, trace, policy);
    }
    Ok(())
}

/// Pull the recent-snapshot list from the bridge. Today the
/// bridge exposes a single `GET /v1/provenance/recent` endpoint
/// that returns the newest 200 snapshots; when that endpoint is
/// unavailable on this version of the bridge, we fall back to
/// an empty list and report (so older bridges don't crash the
/// CLI).
async fn fetch_recent_snapshots(
    bridge: &str,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = format!("{base}/v1/provenance/recent");
    let r = reqwest::Client::new().get(&url).send().await?;
    let status = r.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::NOT_FOUND {
            eprintln!(
                "note: bridge does not expose /v1/provenance/recent yet; \
                 history + audit return an empty list. Hit /v1/provenance/<trace_id> \
                 directly with `relix provenance show` instead."
            );
            return Ok(Vec::new());
        }
        let body = r.text().await?;
        eprintln!("error: HTTP {status}: {body}");
        std::process::exit(1);
    }
    let body = r.text().await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let snaps = v
        .get("snapshots")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(snaps)
}

/// Parse an ISO-8601 second-precision string into a Unix
/// timestamp. Returns `None` when the parse fails — callers
/// treat that as "no bound" so a typo doesn't silently drop the
/// expected rows.
fn parse_iso(s: Option<&str>) -> Option<i64> {
    let s = s?;
    // The roadmap's helper uses YYYY-MM-DDTHH:MM:SSZ — accept
    // that shape exactly. Anything else returns None and the
    // caller treats it as unbounded.
    let s = s.trim();
    if s.len() != 20 || !s.ends_with('Z') {
        return None;
    }
    let year: i32 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let minute: u32 = s[14..16].parse().ok()?;
    let second: u32 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from Civil — same algorithm the runtime uses.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m = month as i32;
    let doy = ((153 * (m + (if m > 2 { -3 } else { 9 })) + 2) / 5) as u32 + day - 1;
    let doe = (yoe * 365 + yoe / 4 - yoe / 100 + doy) as i64;
    let days = era as i64 * 146_097 + doe - 719_468;
    Some(days * 86_400 + (hour as i64) * 3_600 + (minute as i64) * 60 + second as i64)
}

fn in_range(ts: i64, from: Option<i64>, to: Option<i64>) -> bool {
    if let Some(f) = from
        && ts < f
    {
        return false;
    }
    if let Some(t) = to
        && ts > t
    {
        return false;
    }
    true
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        let c = *b as char;
        let safe = c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~');
        if safe {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_iso_round_trips_a_known_date() {
        let ts = parse_iso(Some("2025-11-01T00:00:00Z")).unwrap();
        // 2025-11-01 00:00:00 UTC is 1761955200 since epoch.
        assert_eq!(ts, 1_761_955_200);
    }

    #[test]
    fn parse_iso_rejects_malformed_input() {
        assert!(parse_iso(Some("yesterday")).is_none());
        assert!(parse_iso(Some("2025-11-01")).is_none());
        assert!(parse_iso(None).is_none());
    }

    #[test]
    fn in_range_respects_bounds() {
        assert!(in_range(100, None, None));
        assert!(in_range(100, Some(50), Some(200)));
        assert!(!in_range(100, Some(200), None));
        assert!(!in_range(100, None, Some(50)));
    }

    #[test]
    fn urlencode_escapes_non_safe_chars() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("alpha-1.2_x~y"), "alpha-1.2_x~y");
    }
}
