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
    /// List every attempt of a task, oldest first. One line per
    /// attempt with status, duration, failure class (if any), and
    /// flow_id pointer for cross-referencing into per-flow event
    /// logs on disk.
    Attempts {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
    },
    /// Request an operator-initiated retry. The Coordinator validates
    /// the task is in failed/interrupted and the retry budget is
    /// available, then flips status to `retrying` and emits
    /// `task.retry_requested`. Does NOT re-run the flow — the
    /// operator runs `relix-cli flow-run` (or the bridge picks it up
    /// next time) for that.
    ///
    /// Safety: the CLI refuses by default when the prior failure
    /// class indicates a mutation might have partially succeeded
    /// (currently: `policy_denied`, `invalid_args`, `permanent`).
    /// Pass `--force` to override after operator inspection.
    Retry {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// Override the client-side safety guard. Use only after
        /// you've inspected the flow and confirmed re-execution is
        /// safe under the prior failure class.
        #[arg(long, default_value_t = false)]
        force: bool,
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
        /// In pretty mode, show only the last N events in the
        /// chronology block. 0 = show all (default). Useful for tasks
        /// with thousands of events where the header + summary +
        /// attempts block matter more than the full timeline.
        #[arg(long, default_value_t = 0usize)]
        tail: usize,
    },
    /// List recent Tasks (most-recently-updated first). Server-side
    /// pagination and status filtering since Priority A.
    List {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long, default_value_t = 50usize)]
        limit: usize,
        /// Skip the first N rows (server-side). Pair with --limit
        /// for cursor-style pagination.
        #[arg(long, default_value_t = 0usize)]
        offset: usize,
        /// Server-side status filter. Empty = no filter.
        #[arg(long, default_value = "")]
        status: String,
    },
    /// Follow a task's chronicle live. Polls `task.events`
    /// incrementally and prints each new event as it lands until
    /// Ctrl-C. Operator equivalent of `tail -f` for a task.
    Watch {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// Poll interval in seconds. Default 2.
        #[arg(long, default_value_t = 2u64)]
        interval_secs: u64,
        /// Start from this event_id (exclusive). Default 0
        /// (everything from the beginning).
        #[arg(long, default_value_t = 0i64)]
        since: i64,
    },
    /// Print the total number of tasks, optionally filtered by status.
    Count {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long, default_value = "")]
        status: String,
    },
    /// Chronicle-retention dry-run candidate counter. Calls
    /// `task.compact_events` with `mode=dry-run` and prints
    /// what *would* be deleted under the supplied max-age
    /// policy. No deletion happens — this is the operator
    /// planning surface for the eventual destructive Step 3
    /// pass (see docs/chronicle-retention.md).
    Compact {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Events older than `now - max_age_secs` are
        /// candidates. Must be a positive integer.
        #[arg(long)]
        max_age_secs: i64,
    },
    /// Archival snapshot of one task: header + every attempt +
    /// every chronicle event in a single JSON document. The
    /// operator's "save-before-delete" artifact per
    /// docs/chronicle-retention.md.
    Export {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        task_id: String,
        /// Write the export to this file instead of stdout.
        /// Use `-` (or omit) to stream to stdout for piping
        /// into `jq` or `gzip`.
        #[arg(long, default_value = "-")]
        out: String,
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
            tail,
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
                // Pretty mode also fetches the attempts table so the
                // chronology can show per-attempt boundaries. Fail
                // gracefully if `task.attempts` is unreachable
                // (older Coordinator, policy denial) — render just
                // the task body in that case.
                let attempts_body = match call(
                    &peer,
                    &identity,
                    &client_key,
                    "task.attempts",
                    task_id.as_bytes(),
                )
                .await
                {
                    Ok(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                    Err(_) => None,
                };
                let raw = if tail > 0 {
                    truncate_events_to_tail(s, tail)
                } else {
                    s.to_string()
                };
                print!(
                    "{}",
                    render_pretty_task_with_attempts(&raw, attempts_body.as_deref())
                );
            } else {
                // Default: raw key=value, grep-friendly.
                print!("{s}");
            }
        }
        Cmd::Retry {
            peer,
            identity,
            client_key,
            task_id,
            force,
        } => {
            // C2c.2: client-side safety classification. We GET the
            // task first to inspect last_failure_class. The
            // server-side request_retry handles state/budget
            // validation; we layer one extra check that's
            // operator-judgement-shaped (don't blindly re-run flows
            // whose last failure class suggests a non-retryable
            // condition).
            let get_body = call(
                &peer,
                &identity,
                &client_key,
                "task.get",
                task_id.as_bytes(),
            )
            .await?;
            let s = std::str::from_utf8(&get_body).unwrap_or("");
            let fc = s
                .lines()
                .find_map(|l| l.strip_prefix("last_failure_class="))
                .unwrap_or("-");
            if !force && retry_blocked_by_class(fc) {
                eprintln!("refused: last_failure_class={fc} is not safe to auto-retry");
                eprintln!(
                    "  re-running may produce duplicate side effects or repeat a request that"
                );
                eprintln!(
                    "  should not be repeated. Inspect the flow + chronicle, then pass --force"
                );
                eprintln!("  if the retry is appropriate.");
                std::process::exit(3);
            }
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.retry",
                task_id.as_bytes(),
            )
            .await?;
            print!("{}", std::str::from_utf8(&body).unwrap_or("<binary>"));
        }
        Cmd::Attempts {
            peer,
            identity,
            client_key,
            task_id,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.attempts",
                task_id.as_bytes(),
            )
            .await?;
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            let attempts = parse_attempts(s);
            if attempts.is_empty() {
                println!("(no attempts — task has not transitioned to running)");
            } else {
                println!(
                    "{:>3}  {:<11}  {:<10}  {:<11}  {:<12}  flow_id",
                    "#", "status", "started", "duration", "failure"
                );
                for a in attempts {
                    let dur = match a.finished_at {
                        Some(f) => format!("{}s", f.saturating_sub(a.started_at)),
                        None => "(open)".to_string(),
                    };
                    println!(
                        "{:>3}  {:<11}  {:<10}  {:<11}  {:<12}  {}",
                        a.attempt_num, a.status, a.started_at, dur, a.failure_class, a.flow_id,
                    );
                }
            }
        }
        Cmd::List {
            peer,
            identity,
            client_key,
            limit,
            offset,
            status,
        } => {
            let arg = format!("{limit}|{offset}|{status}");
            let body = call(&peer, &identity, &client_key, "task.list", arg.as_bytes()).await?;
            let s = std::str::from_utf8(&body).unwrap_or("<binary>");
            let mut count = 0;
            for line in s.lines() {
                if line.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = line.splitn(3, '\t').collect();
                if parts.len() == 3 {
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
        Cmd::Watch {
            peer,
            identity,
            client_key,
            task_id,
            interval_secs,
            since,
        } => {
            // Long-poll loop. Each tick calls task.events with the
            // current cursor and advances on any new events. NotFound
            // surfaced by the Coordinator stops the loop (the task
            // disappeared / never existed).
            let mut cursor = since;
            loop {
                let arg = format!("{task_id}|{cursor}|200");
                let body =
                    call(&peer, &identity, &client_key, "task.events", arg.as_bytes()).await?;
                let s = std::str::from_utf8(&body).unwrap_or("");
                let mut last: Option<i64> = None;
                for line in s.lines() {
                    if line.is_empty() {
                        continue;
                    }
                    println!("{line}");
                    if let Some(id) = extract_event_id_from_line(line) {
                        last = Some(id);
                    }
                }
                if let Some(new_cursor) = last {
                    cursor = new_cursor;
                }
                tokio::time::sleep(std::time::Duration::from_secs(interval_secs.max(1))).await;
            }
        }
        Cmd::Count {
            peer,
            identity,
            client_key,
            status,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.count",
                status.as_bytes(),
            )
            .await?;
            print!("{}", std::str::from_utf8(&body).unwrap_or("<binary>"));
        }
        Cmd::Compact {
            peer,
            identity,
            client_key,
            max_age_secs,
        } => {
            if max_age_secs <= 0 {
                return Err("--max-age-secs must be a positive integer".into());
            }
            let arg = format!("{max_age_secs}|dry-run");
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.compact_events",
                arg.as_bytes(),
            )
            .await?;
            // Print the Coordinator's JSON body verbatim — both
            // human-skimmable (single-line) and script-friendly
            // (parseable by `jq` from the CLI).
            print!("{}", std::str::from_utf8(&body).unwrap_or("<binary>"));
            println!();
        }
        Cmd::Export {
            peer,
            identity,
            client_key,
            task_id,
            out,
        } => {
            let body = call(
                &peer,
                &identity,
                &client_key,
                "task.export",
                task_id.as_bytes(),
            )
            .await?;
            if out == "-" {
                std::io::Write::write_all(&mut std::io::stdout().lock(), &body)?;
                println!();
            } else {
                std::fs::write(&out, &body)?;
                eprintln!("wrote {} bytes to {out}", body.len());
            }
        }
    }
    Ok(())
}

/// Trim the `events=[...]` array in a `task.get` body to keep
/// only the last `tail` events. Operator-friendly view for tasks
/// with thousands of chronicle entries where the header, summary,
/// and attempt block matter more than every individual step. The
/// non-events lines are preserved verbatim; if we can't find the
/// `events=` line, return the input unchanged.
fn truncate_events_to_tail(raw: &str, tail: usize) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        if let Some(events_array) = line.strip_prefix("events=") {
            let parsed = parse_events_array(events_array);
            if parsed.len() <= tail {
                // Already short enough; pass through.
                out.push_str(line);
                out.push('\n');
                continue;
            }
            let start = parsed.len() - tail;
            // Re-emit the same JSON-array shape the Coordinator uses
            // so the downstream pretty renderer continues to work.
            let mut new_array = String::from("events=[");
            for (i, (ev_type, ts, payload)) in parsed[start..].iter().enumerate() {
                if i > 0 {
                    new_array.push(',');
                }
                new_array.push_str(&format!(
                    r#"{{"id":{},"ts":{},"type":"{}","payload":"{}"}}"#,
                    start as i64 + i as i64 + 1,
                    ts,
                    json_escape_payload(ev_type),
                    json_escape_payload(payload),
                ));
            }
            new_array.push(']');
            out.push_str(&new_array);
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Minimal JSON-string escape — same shape as the Coordinator's
/// `json_escape`. Inlined here so the CLI stays independent of
/// runtime internals.
fn json_escape_payload(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Extract the `id` field from one line of `task.events` output
/// (which is `{"id":N,"ts":N,"type":"...","payload":"..."}`). Used
/// by `task watch` to advance its cursor. Returns `None` when the
/// line doesn't start with the expected prefix or the integer
/// doesn't parse — defensively skips malformed lines rather than
/// stalling the watch loop.
fn extract_event_id_from_line(line: &str) -> Option<i64> {
    let rest = line.strip_prefix("{\"id\":")?;
    let comma = rest.find(',')?;
    rest[..comma].parse().ok()
}

/// Client-side safety guard for `task retry`. The Coordinator does
/// not block based on failure class — it only enforces state +
/// budget. The CLI adds this opinion so an operator can't blindly
/// re-run a flow whose last failure suggests doing so would be
/// harmful (e.g. `policy_denied` — the request was correctly
/// refused; re-running asks the same question and gets the same
/// answer, masking the underlying mis-configuration).
///
/// Classes returned as "blocked" require explicit `--force` to
/// proceed. Returned as `false` for unknown classes so a future
/// FailureClass variant does not silently get blocked.
fn retry_blocked_by_class(class: &str) -> bool {
    matches!(class, "policy_denied" | "invalid_args" | "permanent")
}

/// One row of the `task.attempts` table on the Coordinator. CLI
/// uses owned strings (lifetime erased) so we can move freely.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AttemptRow {
    attempt_num: i64,
    status: String,
    started_at: i64,
    finished_at: Option<i64>,
    failure_class: String,
    flow_id: String,
}

/// Parse the `task.attempts` body — one tab-delimited line per
/// attempt. Skips malformed lines silently (forward-compatible with
/// schema growth).
fn parse_attempts(body: &str) -> Vec<AttemptRow> {
    body.lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 6 {
                return None;
            }
            Some(AttemptRow {
                attempt_num: parts[0].parse().ok()?,
                status: parts[1].to_string(),
                started_at: parts[2].parse().ok()?,
                finished_at: if parts[3] == "-" {
                    None
                } else {
                    parts[3].parse().ok()
                },
                failure_class: parts[4].to_string(),
                flow_id: parts[5].to_string(),
            })
        })
        .collect()
}

/// Build a one-line operator synopsis from the parsed task header
/// fields. The synopsis is the first thing rendered in pretty mode
/// so operators triaging in bulk can see "what state is this in" at
/// a glance without scrolling.
///
/// Returns `None` when the inputs are too sparse to produce a
/// meaningful summary (e.g. malformed task body).
fn build_summary(header: &TaskHeader<'_>) -> Option<String> {
    let status = header.status?;
    let mut parts: Vec<String> = vec![format!("status={status}")];
    if let Some(n) = header.attempt_count {
        parts.push(format!("attempts={n}"));
    }
    if let (Some(start), Some(end)) = (header.started_at, header.updated_at)
        && matches!(status, "completed" | "failed" | "cancelled" | "interrupted")
        && end >= start
    {
        parts.push(format!("duration={}s", end - start));
    } else if let Some(start) = header.started_at
        && status == "running"
    {
        parts.push(format!("started={start}"));
    }
    if let Some(fc) = header.last_failure_class {
        parts.push(format!("failure={fc}"));
    }
    if let Some(rp) = header.retry_policy
        && rp != "none"
    {
        if let Some(max) = header.max_retries {
            let count = header.retry_count.unwrap_or(0);
            parts.push(format!("retries={count}/{max}({rp})"));
        } else {
            parts.push(format!("retry_policy={rp}"));
        }
    }
    Some(format!("summary: {}", parts.join("  ")))
}

/// Lightweight view of the header fields the pretty renderer
/// references. Lives as a borrowed projection of the raw `task.get`
/// body so we don't pull `serde` in for one-shot parsing.
#[derive(Default)]
struct TaskHeader<'a> {
    status: Option<&'a str>,
    attempt_count: Option<i64>,
    started_at: Option<i64>,
    updated_at: Option<i64>,
    last_failure_class: Option<&'a str>,
    retry_policy: Option<&'a str>,
    retry_count: Option<i64>,
    max_retries: Option<i64>,
}

fn parse_header(raw: &str) -> TaskHeader<'_> {
    let mut h = TaskHeader::default();
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("status=") {
            h.status = Some(v);
        } else if let Some(v) = line.strip_prefix("attempt_count=") {
            h.attempt_count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("started_at=") {
            h.started_at = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("updated_at=") {
            h.updated_at = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("last_failure_class=") {
            h.last_failure_class = Some(v);
        } else if let Some(v) = line.strip_prefix("retry_policy=") {
            h.retry_policy = Some(v);
        } else if let Some(v) = line.strip_prefix("retry_count=") {
            h.retry_count = v.parse().ok();
        } else if let Some(v) = line.strip_prefix("max_retries=") {
            h.max_retries = v.parse().ok();
        }
    }
    h
}

/// Render the Coordinator's `task.get` body together with an
/// optional `task.attempts` response. Adds an "attempts:" block
/// between the header callouts and the chronology when at least one
/// attempt exists.
fn render_pretty_task_with_attempts(raw: &str, attempts_body: Option<&str>) -> String {
    let attempts = attempts_body.map(parse_attempts).unwrap_or_default();
    let mut base = render_pretty_task(raw);
    // C2d.1: prepend a one-line operator synopsis so the first thing
    // an operator sees is the answer to "what state is this in?".
    if let Some(summary) = build_summary(&parse_header(raw)) {
        base = format!("{summary}\n\n{base}");
    }
    if attempts.is_empty() {
        return base;
    }
    // Insert the attempts block immediately before the chronology
    // section so the timeline can reference attempt numbers the
    // operator already saw above.
    let marker = "\nchronology:";
    let block = {
        let mut s = String::from("\nattempts:\n");
        for a in &attempts {
            let dur = match a.finished_at {
                Some(f) => format!("{}s", f.saturating_sub(a.started_at)),
                None => "(open)".to_string(),
            };
            let suffix = if a.failure_class != "-" {
                format!(" failure={}", a.failure_class)
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  #{:<2}  {:<11}  started={}  duration={}{}\n",
                a.attempt_num, a.status, a.started_at, dur, suffix
            ));
        }
        s
    };
    if let Some(pos) = base.find(marker) {
        let mut out = String::with_capacity(base.len() + block.len());
        out.push_str(&base[..pos]);
        out.push_str(&block);
        out.push_str(&base[pos..]);
        out
    } else {
        // No chronology block (events were empty). Append attempts
        // at the end.
        let mut out = base;
        out.push_str(&block);
        out
    }
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
    let mut current_attempt: Option<i64> = None;
    for (i, (ev_type, ts, payload)) in parsed.iter().enumerate() {
        // C2d.2: when we cross an attempt boundary, emit a visual
        // separator so operators can scan the timeline by attempt
        // group at a glance. We parse the attempt_id from the event
        // payload (format: `attempt_id=N attempt_num=M ...`) so
        // attempt_started rows label the group with the attempt
        // number; attempt_finished rows close the group.
        if ev_type == "task.attempt_started" {
            let num = extract_kv_int(payload, "attempt_num");
            current_attempt = num;
            if let Some(n) = num {
                let _ = writeln!(out, "  ---- attempt #{n} ----");
            }
        }
        let delta = ts - first_ts;
        let delta_str = if i == 0 {
            "      ".to_string()
        } else {
            format!("+{delta:>4}s")
        };
        let _ = writeln!(out, "  {delta_str}  {ts}  {ev_type:<22}  {payload}");
        if ev_type == "task.attempt_finished"
            && let Some(n) = current_attempt
        {
            let _ = writeln!(out, "  ---- end attempt #{n} ----");
            current_attempt = None;
        }
    }
    out
}

/// Pull `key=value` from a space-delimited payload string and parse
/// `value` as `i64`. Returns `None` if the key isn't present or the
/// value doesn't parse.
fn extract_kv_int(payload: &str, key: &str) -> Option<i64> {
    for tok in payload.split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix(key)
            && let Some(v) = v.strip_prefix('=')
        {
            return v.parse().ok();
        }
    }
    None
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

    #[test]
    fn parse_attempts_empty_or_malformed_yields_nothing() {
        assert!(parse_attempts("").is_empty());
        assert!(parse_attempts("not\tenough\tcolumns").is_empty());
        // Comment-like lines aren't expected but should skip cleanly.
        assert!(parse_attempts("# header\nshort").is_empty());
    }

    #[test]
    fn parse_attempts_one_row_with_open_finish() {
        let body = "1\trunning\t1700000000\t-\t-\t-\n";
        let out = parse_attempts(body);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].attempt_num, 1);
        assert_eq!(out[0].status, "running");
        assert_eq!(out[0].started_at, 1_700_000_000);
        assert!(out[0].finished_at.is_none());
    }

    #[test]
    fn parse_attempts_multiple_rows() {
        let body = "1\tfailed\t1700000000\t1700000005\ttransient\tflowA\n2\tcompleted\t1700000010\t1700000020\t-\tflowB\n";
        let out = parse_attempts(body);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].status, "failed");
        assert_eq!(out[0].finished_at, Some(1_700_000_005));
        assert_eq!(out[0].failure_class, "transient");
        assert_eq!(out[1].status, "completed");
        assert_eq!(out[1].flow_id, "flowB");
    }

    #[test]
    fn pretty_with_attempts_inserts_block_before_chronology() {
        let raw = "task_id=x\nstatus=completed\nevents=[{\"id\":1,\"ts\":1700000000,\"type\":\"flow.started\",\"payload\":\"chat\"}]\n";
        let attempts = "1\tcompleted\t1700000000\t1700000007\t-\tflowabc\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(attempts));
        assert!(pretty.contains("attempts:"));
        assert!(pretty.contains("#1"));
        assert!(pretty.contains("duration=7s"));
        // Attempt block precedes chronology section.
        let attempts_pos = pretty.find("attempts:").unwrap();
        let chronology_pos = pretty.find("chronology:").unwrap();
        assert!(attempts_pos < chronology_pos);
    }

    #[test]
    fn pretty_with_no_attempts_is_identity() {
        let raw = "task_id=x\nstatus=pending\nevents=[]\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(""));
        assert!(!pretty.contains("attempts:"));
    }

    #[test]
    fn truncate_events_to_tail_keeps_last_n() {
        let raw = "task_id=x\nstatus=running\nevents=[{\"id\":1,\"ts\":100,\"type\":\"a\",\"payload\":\"p1\"},{\"id\":2,\"ts\":200,\"type\":\"b\",\"payload\":\"p2\"},{\"id\":3,\"ts\":300,\"type\":\"c\",\"payload\":\"p3\"}]\n";
        let trimmed = truncate_events_to_tail(raw, 2);
        // Header preserved.
        assert!(trimmed.contains("task_id=x"));
        assert!(trimmed.contains("status=running"));
        // Only the last 2 events appear.
        assert!(!trimmed.contains("\"type\":\"a\""));
        assert!(trimmed.contains("\"type\":\"b\""));
        assert!(trimmed.contains("\"type\":\"c\""));
    }

    #[test]
    fn truncate_events_to_tail_short_chronology_unchanged() {
        let raw = "task_id=x\nstatus=running\nevents=[{\"id\":1,\"ts\":100,\"type\":\"a\",\"payload\":\"p\"}]\n";
        let trimmed = truncate_events_to_tail(raw, 5);
        // tail > event_count: no change to the array.
        assert!(trimmed.contains("\"type\":\"a\""));
    }

    #[test]
    fn truncate_events_to_tail_missing_events_line_passthrough() {
        let raw = "task_id=x\nstatus=pending\n";
        let trimmed = truncate_events_to_tail(raw, 5);
        // No events= line: input preserved (trailing newline added
        // per line by our writer).
        assert!(trimmed.contains("task_id=x"));
        assert!(trimmed.contains("status=pending"));
    }

    #[test]
    fn truncate_events_to_tail_then_pretty_render_works() {
        // Belt-and-suspenders: feed the truncated output to the
        // pretty renderer and confirm chronology block is built
        // from the trimmed events.
        let raw = "task_id=x\nstatus=running\nevents=[{\"id\":1,\"ts\":100,\"type\":\"a\",\"payload\":\"p1\"},{\"id\":2,\"ts\":200,\"type\":\"b\",\"payload\":\"p2\"},{\"id\":3,\"ts\":300,\"type\":\"c\",\"payload\":\"p3\"}]\n";
        let trimmed = truncate_events_to_tail(raw, 2);
        let pretty = render_pretty_task(&trimmed);
        assert!(pretty.contains("chronology:"));
        assert!(!pretty.contains(" a "));
        assert!(pretty.contains("b"));
        assert!(pretty.contains("c"));
    }

    #[test]
    fn extract_event_id_finds_id_from_typical_line() {
        let line = r#"{"id":42,"ts":1700000000,"type":"task.created","payload":"x"}"#;
        assert_eq!(extract_event_id_from_line(line), Some(42));
    }

    #[test]
    fn extract_event_id_handles_malformed_lines() {
        assert_eq!(extract_event_id_from_line(""), None);
        assert_eq!(extract_event_id_from_line("not json"), None);
        assert_eq!(extract_event_id_from_line(r#"{"id":notanum,"ts":1}"#), None);
        // Different field first — defensively unsupported.
        assert_eq!(
            extract_event_id_from_line(r#"{"ts":1,"id":42,"type":"x","payload":""}"#),
            None
        );
    }

    #[test]
    fn extract_event_id_handles_large_id() {
        let line = r#"{"id":9223372036854775000,"ts":1,"type":"x","payload":""}"#;
        assert_eq!(
            extract_event_id_from_line(line),
            Some(9_223_372_036_854_775_000)
        );
    }

    #[test]
    fn retry_blocked_for_non_retryable_classes() {
        for c in ["policy_denied", "invalid_args", "permanent"] {
            assert!(
                retry_blocked_by_class(c),
                "class {c} should be blocked without --force"
            );
        }
    }

    #[test]
    fn retry_allowed_for_retryable_classes() {
        for c in ["transient", "timeout", "unavailable", "-"] {
            assert!(
                !retry_blocked_by_class(c),
                "class {c} should be allowed without --force"
            );
        }
    }

    #[test]
    fn retry_unknown_class_not_blocked() {
        // Forward compatibility: a future FailureClass variant must
        // not be silently treated as blocking. The Coordinator will
        // still gate on state/budget; the CLI only adds opinion to
        // KNOWN-bad classes.
        assert!(!retry_blocked_by_class("brand_new_class"));
    }

    #[test]
    fn summary_line_for_completed_task_shows_duration() {
        let raw = "task_id=x\nstatus=completed\nstarted_at=1700000000\nupdated_at=1700000012\nattempt_count=1\nevents=[]\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(""));
        let first = pretty.lines().next().unwrap();
        assert!(first.starts_with("summary:"), "got: {first}");
        assert!(first.contains("status=completed"));
        assert!(first.contains("attempts=1"));
        assert!(first.contains("duration=12s"));
    }

    #[test]
    fn summary_line_for_running_task_shows_started_not_duration() {
        let raw = "task_id=x\nstatus=running\nstarted_at=1700000000\nupdated_at=1700000050\nattempt_count=1\nevents=[]\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(""));
        let first = pretty.lines().next().unwrap();
        assert!(first.contains("status=running"));
        assert!(first.contains("started=1700000000"));
        assert!(!first.contains("duration="));
    }

    #[test]
    fn summary_line_includes_failure_and_retry_budget() {
        let raw = "task_id=x\nstatus=failed\nstarted_at=1700000000\nupdated_at=1700000005\nattempt_count=2\nretry_policy=bounded\nretry_count=1\nmax_retries=3\nlast_failure_class=transient\nevents=[]\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(""));
        let first = pretty.lines().next().unwrap();
        assert!(first.contains("failure=transient"));
        assert!(first.contains("retries=1/3(bounded)"));
    }

    #[test]
    fn extract_kv_int_finds_value() {
        assert_eq!(
            extract_kv_int("attempt_id=42 attempt_num=3 trace=abc", "attempt_num"),
            Some(3)
        );
        assert_eq!(extract_kv_int("", "x"), None);
        assert_eq!(extract_kv_int("attempt_num=NaN", "attempt_num"), None);
    }

    #[test]
    fn chronology_groups_by_attempt_boundary() {
        let raw = concat!(
            "task_id=x\nstatus=completed\n",
            "events=[",
            r#"{"id":1,"ts":1700000000,"type":"task.created","payload":"chat"},"#,
            r#"{"id":2,"ts":1700000000,"type":"task.attempt_started","payload":"attempt_id=1 attempt_num=1"},"#,
            r#"{"id":3,"ts":1700000010,"type":"task.attempt_finished","payload":"attempt_id=1 status=failed failure_class=transient"},"#,
            r#"{"id":4,"ts":1700000020,"type":"task.attempt_started","payload":"attempt_id=2 attempt_num=2"},"#,
            r#"{"id":5,"ts":1700000030,"type":"task.attempt_finished","payload":"attempt_id=2 status=completed"}"#,
            "]\n"
        );
        let pretty = render_pretty_task(raw);
        assert!(pretty.contains("---- attempt #1 ----"));
        assert!(pretty.contains("---- end attempt #1 ----"));
        assert!(pretty.contains("---- attempt #2 ----"));
        assert!(pretty.contains("---- end attempt #2 ----"));
        // Ordering: attempt #1 markers precede attempt #2 markers.
        let pos1 = pretty.find("---- attempt #1 ----").unwrap();
        let pos2 = pretty.find("---- attempt #2 ----").unwrap();
        assert!(pos1 < pos2);
    }

    #[test]
    fn chronology_skips_grouping_when_attempt_events_absent() {
        let raw = concat!(
            "task_id=x\nstatus=completed\n",
            "events=[",
            r#"{"id":1,"ts":1700000000,"type":"flow.started","payload":"x"},"#,
            r#"{"id":2,"ts":1700000005,"type":"task.completed","payload":"ok"}"#,
            "]\n"
        );
        let pretty = render_pretty_task(raw);
        assert!(!pretty.contains("attempt #"));
    }

    #[test]
    fn summary_line_omitted_when_status_missing() {
        let raw = "task_id=x\nevents=[]\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(""));
        assert!(!pretty.starts_with("summary:"));
    }

    #[test]
    fn pretty_with_attempts_appends_when_no_chronology() {
        let raw = "task_id=x\nstatus=running\nevents=[]\n";
        let attempts = "1\trunning\t1700000000\t-\t-\t-\n";
        let pretty = render_pretty_task_with_attempts(raw, Some(attempts));
        assert!(pretty.contains("attempts:"));
        assert!(pretty.contains("(open)"));
    }
}
