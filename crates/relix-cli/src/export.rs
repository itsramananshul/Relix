//! `relix export` — conversation export.
//!
//! Pulls one or more session chronicles from the bridge's
//! task surface and renders them as JSON, Markdown, or CSV.
//!
//! The bridge endpoint is `GET /v1/sessions/export` which
//! aggregates the coordinator's `task.list_cursor` +
//! `task.events` data. The CLI hits that endpoint and renders
//! locally so the bridge stays the single source of truth.

use std::path::PathBuf;

use clap::{Args, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum ExportFormat {
    Json,
    Markdown,
    Csv,
}

impl ExportFormat {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::Csv => "csv",
        }
    }
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Export exactly one session by id. Mutually exclusive
    /// with `--agent` and `--all`.
    #[arg(long, group = "scope")]
    pub session: Option<String>,
    /// Export every session for an agent (matched by name /
    /// subject_id).
    #[arg(long, group = "scope")]
    pub agent: Option<String>,
    /// Export everything the coordinator has. Use with care on
    /// large deployments.
    #[arg(long, group = "scope")]
    pub all: bool,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = ExportFormat::Json)]
    pub format: ExportFormat,
    /// Output destination. `-` (default) streams to stdout;
    /// any other value writes to the named file.
    #[arg(long, default_value = "-")]
    pub out: String,
    /// Bridge HTTP base URL (matches `relix doctor`).
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    /// SEC §12: bridge bearer token, read from this 0600 file.
    /// When absent, the command reads `~/.relix/bridge-token`. The
    /// token is NEVER taken as an argv value (visible in `ps` /
    /// shell history / journald).
    #[arg(long)]
    pub token_file: Option<PathBuf>,
}

pub async fn run(args: ExportArgs) -> Result<(), Box<dyn std::error::Error>> {
    let scope = match (&args.session, &args.agent, args.all) {
        (Some(s), None, false) => format!("session={}", urlencoding(s)),
        (None, Some(a), false) => format!("agent={}", urlencoding(a)),
        (None, None, true) => "all=1".to_string(),
        _ => {
            return Err("specify exactly one of --session <id>, --agent <name>, --all".into());
        }
    };
    let token = match args.token_file.as_deref() {
        Some(path) => crate::secret_input::read_secret_file(path)
            .map_err(|e| format!("export: {e}"))?
            .as_str()
            .to_string(),
        None => read_bridge_token()?,
    };
    let url = format!(
        "{}/v1/sessions/export?{}&format={}",
        args.bridge.trim_end_matches('/'),
        scope,
        args.format.as_wire()
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.bytes().await?;
    if !status.is_success() {
        return Err(format!(
            "bridge returned HTTP {status}: {}",
            String::from_utf8_lossy(&body)
        )
        .into());
    }
    if args.out == "-" {
        use std::io::Write;
        std::io::stdout().write_all(&body)?;
        if !body.ends_with(b"\n") {
            println!();
        }
    } else {
        std::fs::write(&args.out, &body)?;
        eprintln!("wrote {} bytes to {}", body.len(), args.out);
    }
    Ok(())
}

fn read_bridge_token() -> Result<String, Box<dyn std::error::Error>> {
    let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_var)
        .ok_or("no HOME / USERPROFILE — pass --token-file to override")?;
    let path = std::path::PathBuf::from(home)
        .join(".relix")
        .join("bridge-token");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("read bridge token at {}: {e}", path.display()))?;
    let v = raw.trim().to_string();
    if v.is_empty() {
        return Err("bridge token file is empty".into());
    }
    Ok(v)
}

/// Minimal URL encoder. Only escapes the small set that breaks
/// query strings (`&`, `=`, `#`, `+`, ` `). Avoids pulling in a
/// percent-encoding crate for this single use.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("%26"),
            '=' => out.push_str("%3D"),
            '#' => out.push_str("%23"),
            '+' => out.push_str("%2B"),
            ' ' => out.push_str("%20"),
            _ => out.push(ch),
        }
    }
    out
}

// ─────────────────────── Render helpers ─────────────────────────────

/// One session in the canonical export shape. The bridge emits
/// an array of these for JSON; the same data drives the
/// markdown + CSV formatters too.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)] // Mirrored from bridge for SDK consumers + tests.
pub struct SessionExport {
    pub session_id: String,
    #[serde(default)]
    pub agent: String,
    pub start_time: i64,
    pub end_time: i64,
    pub messages: Vec<SessionMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct SessionMessage {
    pub timestamp: i64,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub token_count: u32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct ToolCall {
    pub timestamp: i64,
    pub tool_name: String,
    #[serde(default)]
    pub args: String,
}

/// Render a slice of sessions as Markdown. Pure function; the
/// CLI uses the bridge's pre-formatted markdown output by
/// default but this lets a SDK consumer render a JSON export
/// locally. Exported for tests.
#[allow(dead_code)]
pub fn render_markdown(sessions: &[SessionExport]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Relix conversation export");
    let _ = writeln!(out, "\n_{} session(s)_\n", sessions.len());
    for s in sessions {
        let _ = writeln!(out, "---\n");
        let _ = writeln!(out, "## Session `{}`", s.session_id);
        if !s.agent.is_empty() {
            let _ = writeln!(out, "- **Agent:** {}", s.agent);
        }
        let _ = writeln!(
            out,
            "- **Started:** {} (unix={})\n- **Ended:**   {} (unix={})\n- **Cost:**    ${:.4}\n",
            fmt_ts(s.start_time),
            s.start_time,
            fmt_ts(s.end_time),
            s.end_time,
            s.cost_usd,
        );
        for m in &s.messages {
            let _ = writeln!(
                out,
                "### [{} · {}] {}\n\n{}\n",
                fmt_ts(m.timestamp),
                m.role,
                if m.token_count > 0 {
                    format!("{} tok", m.token_count)
                } else {
                    String::new()
                },
                m.content,
            );
        }
        if !s.tool_calls.is_empty() {
            let _ = writeln!(out, "### Tool calls\n");
            for t in &s.tool_calls {
                let _ = writeln!(
                    out,
                    "- `{}` at {} — {}",
                    t.tool_name,
                    fmt_ts(t.timestamp),
                    t.args
                );
            }
        }
    }
    out
}

/// Render sessions as a CSV. One row per message. Tool calls
/// land as additional rows with `role = "tool"` and
/// `tool_name` populated.
#[allow(dead_code)]
pub fn render_csv(sessions: &[SessionExport]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("session_id,timestamp,role,content,tool_name,token_count,cost_usd\n");
    for s in sessions {
        for m in &s.messages {
            let _ = writeln!(
                out,
                "{},{},{},{},,{},{:.6}",
                csv_escape(&s.session_id),
                m.timestamp,
                csv_escape(&m.role),
                csv_escape(&m.content),
                m.token_count,
                s.cost_usd,
            );
        }
        for t in &s.tool_calls {
            let _ = writeln!(
                out,
                "{},{},tool,{},{},0,{:.6}",
                csv_escape(&s.session_id),
                t.timestamp,
                csv_escape(&t.args),
                csv_escape(&t.tool_name),
                s.cost_usd,
            );
        }
    }
    out
}

#[allow(dead_code)]
fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let body = s.replace('"', "\"\"");
        format!("\"{body}\"")
    } else {
        s.to_string()
    }
}

#[allow(dead_code)]
fn fmt_ts(unix_secs: i64) -> String {
    // Reuse the home-rolled epoch-to-ISO converter pattern from
    // `relix_runtime::db`. Inline here so the CLI stays
    // chrono-free.
    let days = unix_secs / 86_400;
    let rem = unix_secs.rem_euclid(86_400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[allow(dead_code)]
fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_session() -> SessionExport {
        SessionExport {
            session_id: "sess-1".into(),
            agent: "alice".into(),
            start_time: 1_700_000_000,
            end_time: 1_700_000_120,
            messages: vec![
                SessionMessage {
                    timestamp: 1_700_000_001,
                    role: "user".into(),
                    content: "hi there".into(),
                    token_count: 2,
                },
                SessionMessage {
                    timestamp: 1_700_000_010,
                    role: "assistant".into(),
                    content: "hello, alice!".into(),
                    token_count: 3,
                },
            ],
            tool_calls: vec![ToolCall {
                timestamp: 1_700_000_005,
                tool_name: "memory.search".into(),
                args: "hi".into(),
            }],
            cost_usd: 0.0001,
        }
    }

    #[test]
    fn json_round_trips_via_serde() {
        let s = sample_session();
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionExport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "sess-1");
        assert_eq!(back.messages.len(), 2);
        assert_eq!(back.tool_calls.len(), 1);
    }

    #[test]
    fn markdown_render_includes_header_and_messages() {
        let s = sample_session();
        let md = render_markdown(&[s]);
        assert!(md.contains("# Relix conversation export"));
        assert!(md.contains("## Session `sess-1`"));
        assert!(md.contains("hi there"));
        assert!(md.contains("hello, alice!"));
        assert!(md.contains("### Tool calls"));
        assert!(md.contains("memory.search"));
    }

    #[test]
    fn csv_render_emits_header_and_rows() {
        let s = sample_session();
        let csv = render_csv(&[s]);
        let lines: Vec<&str> = csv.lines().collect();
        // 1 header + 2 messages + 1 tool call = 4 lines
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("session_id,timestamp,role,"));
        assert!(lines[1].contains("user"));
        assert!(lines[2].contains("assistant"));
        assert!(lines[3].contains("tool"));
        assert!(lines[3].contains("memory.search"));
    }

    #[test]
    fn csv_escape_quotes_strings_with_commas() {
        let e = csv_escape("a,b,c");
        assert_eq!(e, "\"a,b,c\"");
        let e = csv_escape("plain");
        assert_eq!(e, "plain");
        let e = csv_escape("with \"quote\"");
        assert_eq!(e, "\"with \"\"quote\"\"\"");
    }

    #[test]
    fn fmt_ts_produces_iso_string_with_z_suffix() {
        let s = fmt_ts(1_700_000_000);
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z'));
        // The actual date for 1_700_000_000 is 2023-11-14.
        assert!(s.starts_with("2023-11-14"), "got {s}");
    }
}
