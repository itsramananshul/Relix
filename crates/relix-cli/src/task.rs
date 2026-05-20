//! `relix-cli task ...` — operator surface for the Coordinator node.
//!
//! Every subcommand dials a Coordinator peer over libp2p, invokes the
//! relevant `task.*` capability through the real admission pipeline
//! (identity → policy → handler → audit), and prints the response.
//!
//! Calls use the same dial-and-call pattern as `relix-cli ping`. The
//! Coordinator runs the whole admission pipeline on every call, so an
//! operator with no `chat-users` (or whichever group the policy requires)
//! will see `policy_denied` here — by design.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Create a new Task on the Coordinator. Prints the `task_id` on
    /// stdout (32 hex chars).
    Create {
        /// Coordinator peer's libp2p multiaddr.
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Short human-readable title.
        #[arg(long)]
        title: String,
        /// Path/name of the SOL flow this task is associated with.
        #[arg(long)]
        flow_template: String,
        /// Caller-supplied params blob. Free-form (the Coordinator does
        /// not parse it). JSON encouraged.
        #[arg(long, default_value = "")]
        params_json: String,
        /// Override the owner subject id (defaults to the caller's).
        #[arg(long, default_value = "")]
        owner_subject_id: String,
        /// Retry policy hint stored on the Task. Operators reference it
        /// from the chronicle; the runtime does not auto-retry today.
        /// One of `none` / `once` / `bounded`.
        #[arg(long, default_value = "")]
        retry_policy: String,
        /// Max retries permitted under `bounded`. Ignored otherwise.
        #[arg(long, default_value_t = 0i64)]
        max_retries: i64,
        /// Hard ceiling on execution time. The Coordinator's recovery
        /// scan flips `running` rows past `started_at + max_runtime_secs`
        /// to `interrupted`. Omit (0) for no ceiling.
        #[arg(long, default_value_t = 0i64)]
        max_runtime_secs: i64,
    },
    /// Mutate a Task. Any of the optional fields are skipped when
    /// omitted; the Coordinator preserves their previous values.
    Update {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// New status (`pending` / `running` / `retrying` / `interrupted` /
        /// `awaiting_input` / `completed` / `failed` / `cancelled`; the
        /// Coordinator does not enforce a state machine).
        #[arg(long, default_value = "")]
        status: String,
        #[arg(long, default_value = "")]
        result: String,
        #[arg(long, default_value = "")]
        flow_id: String,
        #[arg(long, default_value = "")]
        flow_log_path: String,
        /// Error kind from `relix_core::types::error_kinds`. Omit (0) to
        /// leave unchanged.
        #[arg(long, default_value_t = 0i64)]
        error_kind: i64,
        #[arg(long, default_value = "")]
        error_cause: String,
        /// `FailureClass` written to `last_failure_class`. One of
        /// `transient` / `permanent` / `policy_denied` / `invalid_args` /
        /// `timeout` / `unavailable`. Omit to leave unchanged.
        #[arg(long, default_value = "")]
        failure_class: String,
    },
    /// Run the recovery scan now. Promotes `running` tasks past their
    /// `max_runtime_secs` to `interrupted` and appends a
    /// `task.interrupted` event. Idempotent.
    Recover {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
    },
    /// Append a free-form event to a Task's history.
    Event {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        #[arg(long)]
        event_type: String,
        #[arg(long, default_value = "")]
        payload: String,
    },
    /// Print one Task and its event chronicle.
    Get {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// Reformat the response as a human-readable chronology: header
        /// fields followed by a timeline of events with absolute and
        /// relative timestamps. Default keeps the raw `key=value`
        /// stream, which is grep-friendly for scripts.
        #[arg(long, default_value_t = false)]
        pretty: bool,
    },
    /// List recent Tasks (most-recently-updated first).
    List {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
        /// Client-side filter on `status`. The Coordinator does not
        /// filter today (kept compatible with the existing
        /// `task.list` wire format); we fetch up to `limit` rows then
        /// hide ones that don't match. Empty = no filter.
        #[arg(long, default_value = "")]
        status: String,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Create {
            peer,
            identity,
            client_key,
            title,
            flow_template,
            params_json,
            owner_subject_id,
            retry_policy,
            max_retries,
            max_runtime_secs,
        } => {
            let max_retries_s = if max_retries == 0 {
                String::new()
            } else {
                max_retries.to_string()
            };
            let max_runtime_s = if max_runtime_secs == 0 {
                String::new()
            } else {
                max_runtime_secs.to_string()
            };
            let arg = format!(
                "{title}|{flow_template}|{params_json}|{owner_subject_id}|{retry_policy}|{max_retries_s}|{max_runtime_s}"
            );
            let body = call(&peer, &identity, &client_key, "task.create", arg.as_bytes()).await?;
            print_text("task_id", &body);
        }
        Cmd::Update {
            peer,
            identity,
            client_key,
            task_id,
            status,
            result,
            flow_id,
            flow_log_path,
            error_kind,
            error_cause,
            failure_class,
        } => {
            let ek = if error_kind == 0 {
                String::new()
            } else {
                error_kind.to_string()
            };
            let arg = format!(
                "{task_id}|{status}|{result}|{flow_id}|{flow_log_path}|{ek}|{error_cause}|{failure_class}"
            );
            let body = call(&peer, &identity, &client_key, "task.update", arg.as_bytes()).await?;
            print_text("update", &body);
        }
        Cmd::Recover {
            peer,
            identity,
            client_key,
        } => {
            let body = call(&peer, &identity, &client_key, "task.recover", b"").await?;
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            for line in s.lines() {
                if line.starts_with("recovered=") {
                    println!("{line}");
                } else if !line.is_empty() {
                    println!("interrupted {line}");
                }
            }
        }
        Cmd::Event {
            peer,
            identity,
            client_key,
            task_id,
            event_type,
            payload,
        } => {
            let arg = format!("{task_id}|{event_type}|{payload}");
            let body = call(&peer, &identity, &client_key, "task.event", arg.as_bytes()).await?;
            print_text("event_id", &body);
        }
        Cmd::Get {
            peer,
            identity,
            client_key,
            task_id,
            pretty,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.get",
                task_id.as_bytes(),
            )
            .await?;
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            if pretty {
                print!("{}", render_pretty_task(s));
            } else {
                // Default: raw key=value, grep-friendly.
                print!("{s}");
            }
        }
        Cmd::List {
            peer,
            identity,
            client_key,
            limit,
            status,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.list",
                limit.to_string().as_bytes(),
            )
            .await?;
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            let mut count = 0;
            for line in s.lines() {
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
                    // Client-side filter — the Coordinator's `task.list`
                    // is unsorted-by-status by design, and the data is
                    // already in memory.
                    if !status.is_empty() && parts[1] != status {
                        continue;
                    }
                    println!("{}  {:<14}  {}", parts[0].split_at(8).0, parts[1], parts[2]);
                } else {
                    println!("{line}");
                }
                count += 1;
            }
            if count == 0 {
                if status.is_empty() {
                    println!("(no tasks)");
                } else {
                    println!("(no tasks with status={status})");
                }
            }
        }
    }
    Ok(())
}

/// Render the Coordinator's `task.get` body as a human-readable
/// chronology: header fields on top, blank line, then a timeline of
/// events with absolute UTC timestamps and `+Δs` deltas from the
/// previous event. Falls back to the raw text if the JSON `events=`
/// array can't be parsed.
fn render_pretty_task(raw: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(raw.len() + 256);
    let mut events_line: Option<&str> = None;
    let mut header_lines: Vec<&str> = Vec::new();
    let mut status: Option<&str> = None;
    let mut failure_class: Option<&str> = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("events=") {
            events_line = Some(rest);
        } else {
            if let Some(v) = line.strip_prefix("status=") {
                status = Some(v);
            } else if let Some(v) = line.strip_prefix("last_failure_class=") {
                failure_class = Some(v);
            }
            header_lines.push(line);
        }
    }
    for line in &header_lines {
        let _ = writeln!(out, "{line}");
    }
    let status_callout = status.and_then(|s| status_hint(s).map(|h| format!("[{s}] {h}\n")));
    let class_callout = failure_class
        .and_then(|fc| failure_class_hint(fc).map(|h| format!("[failure: {fc}] {h}\n")));
    if status_callout.is_some() || class_callout.is_some() {
        out.push('\n');
        if let Some(s) = status_callout {
            out.push_str(&s);
        }
        if let Some(s) = class_callout {
            out.push_str(&s);
        }
    }
    let Some(events) = events_line else {
        return out;
    };
    let parsed = parse_events_array(events);
    if parsed.is_empty() {
        return out;
    }
    out.push_str("\nchronology:\n");
    let first_ts = parsed[0].1;
    for (i, (ev_type, ts, payload)) in parsed.iter().enumerate() {
        let delta = ts - first_ts;
        let delta_str = if i == 0 {
            "      ".to_string()
        } else {
            format!("+{delta:>4}s")
        };
        let _ = writeln!(out, "  {delta_str}  {ts}  {ev_type:<22}  {payload}");
    }
    out
}

/// Short operator hint for a status value. Only emitted in
/// `--pretty` mode for the few states where the meaning isn't
/// already obvious from the word itself. Returning `None` leaves
/// the status as-is without a callout line.
fn status_hint(status: &str) -> Option<&'static str> {
    match status {
        "interrupted" => Some(
            "executor died or max_runtime_secs was exceeded; recovery scan re-labelled the row. \
             Inspect last_failure_reason and decide whether to re-run.",
        ),
        "awaiting_input" => Some(
            "flow paused on an external dependency (human approval, async webhook). \
             The runtime records this state; the resume primitive is Gate 2.",
        ),
        "retrying" => Some(
            "a previous attempt failed; another attempt has been scheduled. \
             Auto-retry is not wired today, so this status is operator-initiated.",
        ),
        "cancelled" => Some("operator explicitly cancelled this task."),
        _ => None,
    }
}

/// Short operator hint for a failure-class value. Same UX as
/// `status_hint` — only callouts for the classes where the
/// retry-advice isn't obvious from the name.
fn failure_class_hint(class: &str) -> Option<&'static str> {
    match class {
        "transient" => {
            Some("retryable if the flow is idempotent (e.g. same params produce same result).")
        }
        "timeout" => Some(
            "deadline exceeded. Re-run with a higher --max-runtime-secs, or investigate \
             why the flow stalled.",
        ),
        "unavailable" => Some(
            "capability deprecated/removed or manifest stale. Re-check the responder, \
             refresh manifests, then re-run.",
        ),
        "policy_denied" => Some(
            "admission pipeline refused the call. DO NOT re-run blindly; fix the policy \
             or identity first.",
        ),
        "invalid_args" => Some("caller-side input was malformed. Fix the caller, then re-run."),
        "permanent" => {
            Some("logic / contract error inside the flow. Investigate; do not auto-retry.")
        }
        _ => None,
    }
}

/// Minimal parser for the Coordinator's hand-built JSON event array:
/// `[{"id":N,"ts":N,"type":"...","payload":"..."},...]`. We don't want
/// to drag serde_json into the CLI for this; the format is stable and
/// only the Coordinator produces it. Returns
/// `Vec<(type, ts, payload)>`. Returns empty on any parse trouble —
/// callers fall back to the raw text.
fn parse_events_array(s: &str) -> Vec<(String, i64, String)> {
    let s = s.trim();
    let Some(inner) = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')) else {
        return Vec::new();
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut buf = String::new();
    let mut in_str = false;
    let mut esc = false;
    for c in inner.chars() {
        if in_str {
            buf.push(c);
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '{' => {
                depth += 1;
                buf.push(c);
            }
            '}' => {
                depth -= 1;
                buf.push(c);
                if depth == 0 {
                    if let Some(obj) = parse_event_object(buf.trim()) {
                        out.push(obj);
                    }
                    buf.clear();
                }
            }
            ',' if depth == 0 => { /* between objects */ }
            '"' => {
                in_str = true;
                buf.push(c);
            }
            _ => buf.push(c),
        }
    }
    out
}

fn parse_event_object(obj: &str) -> Option<(String, i64, String)> {
    // Strip outer braces.
    let body = obj.strip_prefix('{')?.strip_suffix('}')?;
    let mut ts: Option<i64> = None;
    let mut ev_type: Option<String> = None;
    let mut payload: Option<String> = None;
    // Walk top-level "key":value pairs.
    let mut chars = body.chars().peekable();
    while chars.peek().is_some() {
        // Skip whitespace and commas.
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        // Read "key".
        if chars.next() != Some('"') {
            return None;
        }
        let mut key = String::new();
        for c in chars.by_ref() {
            if c == '"' {
                break;
            }
            key.push(c);
        }
        // Skip ':'.
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ':') {
            chars.next();
        }
        // Read value (string or integer).
        match chars.peek() {
            Some('"') => {
                chars.next();
                let mut v = String::new();
                let mut esc = false;
                for c in chars.by_ref() {
                    if esc {
                        match c {
                            'n' => v.push('\n'),
                            'r' => v.push('\r'),
                            't' => v.push('\t'),
                            '"' => v.push('"'),
                            '\\' => v.push('\\'),
                            other => v.push(other),
                        }
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        break;
                    } else {
                        v.push(c);
                    }
                }
                match key.as_str() {
                    "type" => ev_type = Some(v),
                    "payload" => payload = Some(v),
                    _ => {}
                }
            }
            Some(_) => {
                let mut v = String::new();
                while let Some(c) = chars.peek() {
                    if c.is_ascii_digit() || *c == '-' {
                        v.push(*c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if key == "ts" {
                    ts = v.parse().ok();
                }
            }
            None => break,
        }
    }
    Some((ev_type?, ts?, payload.unwrap_or_default()))
}

/// Dial `peer_addr` once, present `identity_bundle`, invoke `method`
/// with `arg` bytes, return the response body. Mirrors `ping::run` but
/// returns the body instead of pretty-printing it (each subcommand
/// formats its own output).
async fn call(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    let key_bytes = std::fs::read(client_key_path)?;
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

    let port = 20_000 + (rand::random::<u16>() % 10_000);
    let (client, mut events, event_loop) = rpc::new(key, port).await?;
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

    let envelope = build_request(method, arg.to_vec(), bundle, 10);
    let resp_bytes = client
        .call(connected, envelope)
        .await
        .map_err(|e| format!("rpc: {e}"))?;
    let resp = decode_response(&resp_bytes)?;
    match resp.res {
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(e) => {
            eprintln!("ERR kind={} cause={}", e.kind, e.cause);
            std::process::exit(2);
        }
        ResponseResult::StreamHandle(_) => {
            eprintln!("unexpected stream-handle response from method '{method}'");
            std::process::exit(2);
        }
    }
}

fn print_text(label: &str, body: &[u8]) {
    match std::str::from_utf8(body) {
        Ok(s) => println!("{label}: {}", s.trim_end_matches('\n')),
        Err(_) => println!(
            "{label} ({} bytes, binary): {}",
            body.len(),
            hex::encode(body)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_events_array_empty() {
        assert!(parse_events_array("[]").is_empty());
        assert!(parse_events_array("").is_empty());
    }

    #[test]
    fn parse_events_array_one_event() {
        let s = r#"[{"id":1,"ts":1700000000,"type":"flow_selected","payload":"chat"}]"#;
        let out = parse_events_array(s);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "flow_selected");
        assert_eq!(out[0].1, 1700000000);
        assert_eq!(out[0].2, "chat");
    }

    #[test]
    fn parse_events_array_multiple_events_and_escapes() {
        let s = r#"[{"id":1,"ts":1700000000,"type":"a","payload":"x"},{"id":2,"ts":1700000005,"type":"b","payload":"with \"quote\" and \\backslash"}]"#;
        let out = parse_events_array(s);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, "a");
        assert_eq!(out[1].2, "with \"quote\" and \\backslash");
    }

    #[test]
    fn render_pretty_task_includes_chronology_block() {
        let raw = "task_id=abcd1234\nstatus=completed\nevents=[{\"id\":1,\"ts\":1700000000,\"type\":\"flow.started\",\"payload\":\"chat\"},{\"id\":2,\"ts\":1700000007,\"type\":\"task.completed\",\"payload\":\"hi\"}]\n";
        let pretty = render_pretty_task(raw);
        assert!(pretty.contains("task_id=abcd1234"));
        assert!(pretty.contains("status=completed"));
        assert!(pretty.contains("chronology:"));
        assert!(pretty.contains("flow.started"));
        assert!(pretty.contains("task.completed"));
        assert!(pretty.contains("+   7s"));
    }

    #[test]
    fn render_pretty_task_falls_back_when_events_unparseable() {
        let raw = "task_id=x\nevents=not-json\n";
        let pretty = render_pretty_task(raw);
        // Header preserved; no chronology synthesized.
        assert!(pretty.contains("task_id=x"));
        assert!(!pretty.contains("chronology"));
    }

    #[test]
    fn render_pretty_task_surfaces_interrupted_status_with_hint() {
        let raw = "task_id=x\nstatus=interrupted\nlast_failure_class=timeout\nevents=[]\n";
        let pretty = render_pretty_task(raw);
        // Status hint AND failure-class hint both appear, since both
        // are operator-relevant for this row.
        assert!(pretty.contains("[interrupted]"));
        assert!(pretty.contains("recovery scan"));
        assert!(pretty.contains("[failure: timeout]"));
        assert!(pretty.contains("deadline exceeded"));
    }

    #[test]
    fn render_pretty_task_surfaces_awaiting_input_with_gate_2_note() {
        let raw = "task_id=x\nstatus=awaiting_input\nevents=[]\n";
        let pretty = render_pretty_task(raw);
        assert!(pretty.contains("[awaiting_input]"));
        // The note about Gate 2 is load-bearing — operators must not
        // mistake "we recorded the state" for "the runtime resumes
        // automatically".
        assert!(pretty.contains("Gate 2"));
    }

    #[test]
    fn render_pretty_task_no_callout_for_terminal_completed() {
        let raw = "task_id=x\nstatus=completed\nevents=[]\n";
        let pretty = render_pretty_task(raw);
        // `completed` is self-explanatory; no callout, no clutter.
        assert!(!pretty.contains("[completed]"));
        assert!(!pretty.contains("[failure"));
    }

    #[test]
    fn render_pretty_task_warns_on_policy_denied_class() {
        let raw = "task_id=x\nstatus=failed\nlast_failure_class=policy_denied\nevents=[]\n";
        let pretty = render_pretty_task(raw);
        assert!(pretty.contains("[failure: policy_denied]"));
        assert!(pretty.contains("DO NOT re-run"));
    }
}
