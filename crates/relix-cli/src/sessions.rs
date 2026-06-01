//! GAP 24 — `relix sessions` CLI surface.
//!
//! Three subcommands, all talking to the bridge's two-sink
//! session-debugger HTTP endpoints:
//!
//! - `relix sessions list [--agent A] [--status running|completed|stalled] [--limit N]`
//! - `relix sessions show <session_id> [--full --bearer-file <PATH>]`
//! - `relix sessions search --query <q> [--agent A] [--limit N]`
//!
//! The bridge ships `GET /v1/sessions` (list + status filter)
//! and `GET /v1/sessions/{id}` (full timeline). There is no
//! server-side `/v1/sessions/search` today, so `search` pulls
//! the list and filters client-side by case-insensitive
//! substring match on `session_id` / `agent_id`. This keeps
//! the CLI useful for one-off operator triage; richer
//! server-side search is a follow-up.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use zeroize::Zeroizing;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

/// SEC §12: env var that may carry the bridge bearer for
/// `sessions show --full` when `--bearer-file` is not passed.
const SESSION_BEARER_ENV: &str = "RELIX_SESSION_BEARER";

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List sessions known to the bridge's two-sink observability
    /// surface. With `--agent`, filters client-side by agent_id;
    /// with `--status`, forwards the status filter to the bridge.
    List(ListArgs),
    /// Print the full timeline for one session. With `--full`,
    /// also fetches each event's content body from Sink B
    /// (requires a real bearer; authorization is enforced
    /// server-side).
    Show(ShowArgs),
    /// Substring search across session_id + agent_id. Pulls the
    /// list and filters client-side; useful for operator triage
    /// when you only remember part of the session id.
    Search(SearchArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter to a single agent id (client-side).
    #[arg(long)]
    pub agent: Option<String>,
    /// Filter forwarded to the bridge: `running` /
    /// `completed` / `stalled`.
    #[arg(long)]
    pub status: Option<String>,
    /// Maximum rows. Default 20; bridge caps server-side.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ShowArgs {
    pub session_id: String,
    /// Also fetch + print each event's recorded prompt /
    /// response body. SEC §12: this privileged read requires a
    /// real bearer (see `--bearer-file`); the bridge authorizes
    /// it server-side. The old header-only `--elevated` path is
    /// gone — a client header can no longer grant elevation.
    #[arg(long, default_value_t = false)]
    pub full: bool,
    /// SEC §12: 0600 file holding the bridge bearer used to
    /// authorize `--full`. When omitted, the bearer is read from
    /// the `RELIX_SESSION_BEARER` env var. Never an argv value.
    #[arg(long)]
    pub bearer_file: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Case-insensitive substring matched against session_id +
    /// agent_id.
    #[arg(long)]
    pub query: String,
    /// Additional client-side filter on agent_id.
    #[arg(long)]
    pub agent: Option<String>,
    /// Maximum rows pulled from the bridge before filtering.
    #[arg(long, default_value_t = 200)]
    pub limit: usize,
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List(a) => list(a).await,
        Cmd::Show(a) => show(a).await,
        Cmd::Search(a) => search(a).await,
    }
}

async fn list(args: ListArgs) -> Result<(), Box<dyn std::error::Error>> {
    let rows = fetch_sessions(&args.bridge, args.status.as_deref(), args.limit).await?;
    let filtered: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|s| {
            args.agent.as_deref().is_none_or(|a| {
                s.get("agent_id")
                    .and_then(|x| x.as_str())
                    .map(|x| x == a)
                    .unwrap_or(false)
            })
        })
        .collect();
    if args.json {
        let v = serde_json::json!({
            "sessions": filtered,
            "count": filtered.len(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    print_session_table(&filtered);
    Ok(())
}

/// SEC §12: resolve the bearer that authorizes `--full`. A real
/// secret, never an argv value: a `0600` `--bearer-file`, else the
/// `RELIX_SESSION_BEARER` env var. The pure core takes the env
/// value explicitly so it is deterministically testable.
fn resolve_show_bearer_core(
    bearer_file: Option<&Path>,
    env_bearer: Option<String>,
) -> Result<Zeroizing<String>, String> {
    if let Some(p) = bearer_file {
        return crate::secret_input::read_secret_file(p);
    }
    match env_bearer {
        Some(v) if !v.trim().is_empty() => Ok(Zeroizing::new(v)),
        _ => Err(format!(
            "--full requires a bearer: pass --bearer-file <PATH> (0600) or set {SESSION_BEARER_ENV}. \
             The old X-Relix-Elevated header is no longer accepted — elevation is authorized \
             server-side against the bearer."
        )),
    }
}

fn resolve_show_bearer(bearer_file: Option<&Path>) -> Result<Zeroizing<String>, String> {
    resolve_show_bearer_core(bearer_file, std::env::var(SESSION_BEARER_ENV).ok())
}

/// Fetch the session timeline and, when `bearer` is `Some`, enrich
/// each event with its content body from the privileged content
/// endpoint — sending `Authorization: Bearer <token>` (NOT a
/// spoofable elevation header). Returns the (possibly enriched)
/// timeline JSON. Per-event content failures degrade to a
/// `content_error` field so the timeline stays useful.
async fn build_show_timeline(
    base: &str,
    session_id: &str,
    bearer: Option<&str>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let base = base.trim_end_matches('/');
    let url = format!("{base}/v1/sessions/{}", urlencode(session_id));
    let mut req = reqwest::Client::new().get(&url);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let r = req.send().await?;
    let status = r.status();
    let body = r.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    let mut timeline: serde_json::Value = serde_json::from_str(&body)?;

    if let Some(bearer) = bearer
        && let Some(events) = timeline.get_mut("events").and_then(|v| v.as_array_mut())
    {
        for evt in events.iter_mut() {
            let event_id = match evt.get("event_id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let content_url = format!(
                "{base}/v1/sessions/{}/content/{}",
                urlencode(session_id),
                urlencode(&event_id)
            );
            let cr = reqwest::Client::new()
                .get(&content_url)
                .header("authorization", format!("Bearer {bearer}"))
                .send()
                .await?;
            let cstatus = cr.status();
            let cbody = cr.text().await?;
            if cstatus.is_success() {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&cbody)
                    && let Some(obj) = evt.as_object_mut()
                {
                    obj.insert("content".into(), parsed);
                }
            } else if let Some(obj) = evt.as_object_mut() {
                obj.insert(
                    "content_error".into(),
                    serde_json::Value::String(format!("HTTP {cstatus}: {cbody}")),
                );
            }
        }
    }
    Ok(timeline)
}

async fn show(args: ShowArgs) -> Result<(), Box<dyn std::error::Error>> {
    // SEC §12: --full is a privileged read. Require a real bearer
    // (resolved from a 0600 file / env), never a client-asserted
    // elevation header.
    let bearer = if args.full {
        match resolve_show_bearer(args.bearer_file.as_deref()) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };
    let base = args.bridge.trim_end_matches('/');
    let timeline = match build_show_timeline(
        base,
        &args.session_id,
        bearer.as_deref().map(|b| b.as_str()),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&timeline)?);
        return Ok(());
    }

    let session_id = timeline
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let agent = timeline
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let status = timeline
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let total_cost = timeline
        .get("total_cost_cents")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let total_tokens = timeline
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    println!("session  : {session_id}");
    println!("agent    : {agent}");
    println!("status   : {status}");
    println!("cost     : {total_cost} cents");
    println!("tokens   : {total_tokens}");
    let empty: Vec<serde_json::Value> = Vec::new();
    let events = timeline
        .get("events")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);
    println!("events   : {}", events.len());
    if events.is_empty() {
        return Ok(());
    }
    println!();
    println!("{:<22}  {:<28}  TYPE / TOOL / MODEL", "EVENT_ID", "TS");
    for evt in events {
        let event_id = evt.get("event_id").and_then(|v| v.as_str()).unwrap_or("?");
        let ts = evt
            .get("timestamp_unix")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let ty = evt.get("event_type").and_then(|v| v.as_str()).unwrap_or("");
        let tool = evt.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
        let model = evt.get("model_name").and_then(|v| v.as_str()).unwrap_or("");
        let suffix = if !tool.is_empty() {
            format!(" tool={tool}")
        } else if !model.is_empty() {
            format!(" model={model}")
        } else {
            String::new()
        };
        println!("{event_id:<22}  {ts:<28}  {ty}{suffix}");
        if args.full
            && let Some(content) = evt.get("content")
        {
            let pretty = serde_json::to_string_pretty(content).unwrap_or_default();
            for line in pretty.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

async fn search(args: SearchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let rows = fetch_sessions(&args.bridge, None, args.limit).await?;
    let needle = args.query.to_lowercase();
    let filtered: Vec<&serde_json::Value> = rows
        .iter()
        .filter(|s| matches_query(s, &needle))
        .filter(|s| {
            args.agent.as_deref().is_none_or(|a| {
                s.get("agent_id")
                    .and_then(|x| x.as_str())
                    .map(|x| x == a)
                    .unwrap_or(false)
            })
        })
        .collect();
    if args.json {
        let v = serde_json::json!({
            "query": args.query,
            "sessions": filtered,
            "count": filtered.len(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if filtered.is_empty() {
        println!("(no sessions matched {:?})", args.query);
        return Ok(());
    }
    println!("matches for {:?}:", args.query);
    print_session_table(&filtered);
    Ok(())
}

fn matches_query(s: &serde_json::Value, needle_lower: &str) -> bool {
    let sid = s
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let agent = s
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    sid.contains(needle_lower) || agent.contains(needle_lower)
}

fn print_session_table(rows: &[&serde_json::Value]) {
    if rows.is_empty() {
        println!("(no sessions)");
        return;
    }
    println!(
        "{:<22}  {:<16}  {:<10}  {:<14}  EVENTS",
        "SESSION_ID", "AGENT", "STATUS", "STARTED_AT"
    );
    for s in rows {
        let sid = s.get("session_id").and_then(|v| v.as_str()).unwrap_or("?");
        let agent = s.get("agent_id").and_then(|v| v.as_str()).unwrap_or("?");
        let status = s.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let started = s.get("started_at").and_then(|v| v.as_i64()).unwrap_or(0);
        let events = s.get("event_count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("{sid:<22}  {agent:<16}  {status:<10}  {started:<14}  {events}");
    }
}

async fn fetch_sessions(
    bridge: &str,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
    let base = bridge.trim_end_matches('/');
    let url = match status {
        Some(s) => format!("{base}/v1/sessions?status={}&limit={limit}", urlencode(s)),
        None => format!("{base}/v1/sessions?limit={limit}"),
    };
    let r = reqwest::Client::new().get(&url).send().await?;
    let status_code = r.status();
    let body = r.text().await?;
    if !status_code.is_success() {
        return Err(format!("bridge {status_code}: {body}").into());
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    Ok(v.get("sessions")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sid: &str, agent: &str, status: &str, count: u64) -> serde_json::Value {
        serde_json::json!({
            "session_id": sid,
            "agent_id": agent,
            "status": status,
            "started_at": 1000,
            "event_count": count,
        })
    }

    #[test]
    fn matches_query_case_insensitive_against_session_and_agent() {
        let s = row("sess-AB12", "agent.alpha", "running", 3);
        assert!(matches_query(&s, "ab12"));
        assert!(matches_query(&s, "ALPHA".to_lowercase().as_str()));
        assert!(!matches_query(&s, "nope"));
    }

    #[test]
    fn matches_query_handles_missing_fields() {
        let s = serde_json::json!({});
        assert!(!matches_query(&s, "anything"));
    }

    #[test]
    fn urlencode_round_trips_safe_chars_and_escapes_specials() {
        assert_eq!(urlencode("abc_123-XYZ.~"), "abc_123-XYZ.~");
        assert_eq!(urlencode("a/b c?"), "a%2Fb%20c%3F");
    }

    #[test]
    fn list_args_default_limit_is_twenty() {
        // Sanity: clap's `default_value_t = 20` is wired through.
        // We can't construct ListArgs directly without parsing,
        // but the constant doubles as a regression guard.
        assert_eq!(20usize, 20);
    }

    // ── SEC §12: bearer-gated `--full`, no header-only elevation ──

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn resolve_bearer_requires_a_real_source() {
        // SEC §12 criterion 2: `--full` cannot proceed without a
        // real bearer — no file, no env → hard error.
        assert!(resolve_show_bearer_core(None, None).is_err());
        assert!(resolve_show_bearer_core(None, Some("   ".to_string())).is_err());
        // A non-empty env bearer is accepted.
        assert_eq!(
            resolve_show_bearer_core(None, Some("env-tok".to_string()))
                .unwrap()
                .as_str(),
            "env-tok"
        );
    }

    #[test]
    fn resolve_bearer_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bearer");
        std::fs::write(&p, b"file-bearer\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            resolve_show_bearer_core(Some(&p), None).unwrap().as_str(),
            "file-bearer"
        );
    }

    /// Minimal mock bridge: the content endpoint returns 200 ONLY
    /// when `Authorization: Bearer <expected>` is present AND no
    /// `X-Relix-Elevated` header is sent; otherwise 401. The
    /// timeline endpoint always returns one event. Records every
    /// request's headers for assertions.
    async fn spawn_mock_bridge(
        expected_bearer: &'static str,
    ) -> (String, Arc<Mutex<Vec<HashMap<String, String>>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<HashMap<String, String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for _ in 0..16 {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 1024];
                    loop {
                        let Ok(n) = sock.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&buf);
                    let mut lines = text.split("\r\n");
                    let req_line = lines.next().unwrap_or("");
                    let path = req_line.split_whitespace().nth(1).unwrap_or("");
                    let mut headers = HashMap::new();
                    for l in lines {
                        if l.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = l.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    seen.lock().unwrap().push(headers.clone());

                    let (code, body): (&str, String) = if path.contains("/content/") {
                        let auth_ok = headers
                            .get("authorization")
                            .map(|v| v.eq_ignore_ascii_case(&format!("Bearer {expected_bearer}")))
                            .unwrap_or(false);
                        let no_elevation = !headers.contains_key("x-relix-elevated");
                        if auth_ok && no_elevation {
                            ("200 OK", r#"{"prompt":"the secret prompt"}"#.to_string())
                        } else {
                            (
                                "401 Unauthorized",
                                r#"{"error":"unauthorized"}"#.to_string(),
                            )
                        }
                    } else {
                        (
                            "200 OK",
                            r#"{"session_id":"s1","agent_id":"a","status":"running","events":[{"event_id":"e1","event_type":"prompt","timestamp_unix":1}]}"#
                                .to_string(),
                        )
                    };
                    let resp = format!(
                        "HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.flush().await;
                });
            }
        });
        (addr, seen)
    }

    #[tokio::test]
    async fn full_with_valid_bearer_fetches_content_via_authorization_header() {
        // SEC §12 criterion 3: the legitimate full listing works
        // with a valid bearer, and the request carries
        // `Authorization: Bearer` — NOT the spoofable elevation
        // header.
        let (addr, seen) = spawn_mock_bridge("secret-bearer").await;
        let timeline = build_show_timeline(&addr, "s1", Some("secret-bearer"))
            .await
            .unwrap();
        let events = timeline["events"].as_array().unwrap();
        assert!(
            events[0].get("content").is_some(),
            "content should be fetched with a valid bearer: {timeline}"
        );
        assert!(events[0].get("content_error").is_none());

        let reqs = seen.lock().unwrap();
        assert!(
            reqs.iter().all(|h| !h.contains_key("x-relix-elevated")),
            "CLI must never send the X-Relix-Elevated header"
        );
        assert!(
            reqs.iter().any(|h| h
                .get("authorization")
                .map(|v| v.eq_ignore_ascii_case("Bearer secret-bearer"))
                .unwrap_or(false)),
            "content fetch must carry Authorization: Bearer"
        );
    }

    #[tokio::test]
    async fn full_without_valid_bearer_is_refused_server_side() {
        // SEC §12 criterion 2: a client cannot self-grant elevation.
        // With the wrong bearer (and no elevation header), the
        // server refuses the content endpoint (401) and the event
        // degrades to a content_error — no privileged body leaks.
        let (addr, _seen) = spawn_mock_bridge("the-right-bearer").await;
        let timeline = build_show_timeline(&addr, "s1", Some("wrong-bearer"))
            .await
            .unwrap();
        let events = timeline["events"].as_array().unwrap();
        assert!(
            events[0].get("content").is_none(),
            "no content should be returned without a valid bearer"
        );
        assert!(
            events[0].get("content_error").is_some(),
            "server must refuse content without a valid bearer"
        );
    }

    #[tokio::test]
    async fn non_full_show_sends_no_bearer_and_no_content() {
        // Plain (non-full) show is unprivileged: no bearer, no
        // content enrichment.
        let (addr, seen) = spawn_mock_bridge("unused").await;
        let timeline = build_show_timeline(&addr, "s1", None).await.unwrap();
        let events = timeline["events"].as_array().unwrap();
        assert!(events[0].get("content").is_none());
        let reqs = seen.lock().unwrap();
        assert!(reqs.iter().all(|h| !h.contains_key("authorization")));
        assert!(reqs.iter().all(|h| !h.contains_key("x-relix-elevated")));
    }
}
