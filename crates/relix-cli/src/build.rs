//! `relix build "<spec>"` — RELIX-7.24 Build Mode entry point.
//!
//! Wraps the full spec-driven planning pipeline under one
//! operator-facing command:
//!
//! 1. Call `planning.create_plan` on the bridge with the
//!    operator's spec. `--max-agents` controls the
//!    specialist count; `--no-approval` forces the
//!    legacy execute-now path; `--dry-run` shows the
//!    generated plan without executing or approval.
//! 2. Pretty-print the plan summary: orchestrator state,
//!    critic verdict, conflict report, generated workflow
//!    YAML.
//! 3. When the response carries an `approval.status =
//!    pending`: display the plan, prompt the operator on
//!    stdin for `approve` / `reject`, call the matching cap.
//!    On non-TTY stdin (CI / piped input) the prompt is
//!    SKIPPED and a tracing line tells the operator to
//!    decide via `relix planning approve/reject` instead.
//! 4. When verification ran (Stage-5), pretty-print the
//!    log via `planning.verification_log`.
//!
//! `--output json` short-circuits steps 2-4 and dumps the
//! raw `planning.create_plan` body (plus approval / decision
//! result when applicable). Useful for scripting.

use std::io::Write;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Args, Debug)]
pub struct BuildArgs {
    /// Operator's spec text. Mutually exclusive with
    /// `--spec-file`.
    pub spec: Option<String>,
    /// Read the spec from disk. Mutually exclusive with the
    /// positional `spec`.
    #[arg(long)]
    pub spec_file: Option<PathBuf>,
    /// Maximum specialist count. Defaults to 3 — the same
    /// default as `planning.create_plan`.
    #[arg(long)]
    pub max_agents: Option<usize>,
    /// Generate the plan without executing or asking for
    /// approval. Mutually exclusive with `--execute` (the
    /// default).
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
    /// Skip the approval gate even when the coordinator is
    /// configured to require approval. Useful when running
    /// locally without notification channels wired.
    #[arg(long, default_value_t = false)]
    pub no_approval: bool,
    /// `"pretty"` (default) prints a human-readable summary
    /// plus an interactive approve/reject prompt when
    /// applicable. `"json"` dumps the raw responses for
    /// scripting and never prompts.
    #[arg(long, default_value = "pretty")]
    pub output: String,
    /// Bridge URL. Defaults to `http://127.0.0.1:19791`.
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
}

pub async fn run(args: BuildArgs) -> Result<(), Box<dyn std::error::Error>> {
    let spec = resolve_spec(args.spec, args.spec_file)?;
    let pretty = args.output != "json";

    let require_approval = !args.no_approval && !args.dry_run;

    // 1. Create the plan.
    let plan_url = format!("{}/v1/planning/plan", args.bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("spec".into(), Value::from(spec));
    body.insert("dry_run".into(), Value::from(args.dry_run));
    if let Some(n) = args.max_agents {
        body.insert("max_agents".into(), Value::from(n));
    }
    if require_approval {
        body.insert("require_approval".into(), Value::from(true));
    }
    let plan_resp_text = http_post_json(&plan_url, &Value::Object(body)).await?;
    let plan: Value = serde_json::from_str(&plan_resp_text)
        .map_err(|e| format!("decode plan response: {e} (body={plan_resp_text})"))?;

    if !pretty {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        // In JSON mode we also fetch + print the
        // verification log when present so scripts get one
        // combined view.
        if let Some(plan_id) = plan
            .get("plan_spec")
            .and_then(|s| s.get("spec_id"))
            .and_then(Value::as_str)
            && plan.get("verification").is_some()
        {
            let log = fetch_verification_log(&args.bridge, plan_id).await.ok();
            if let Some(log) = log {
                println!("{}", serde_json::to_string_pretty(&log)?);
            }
        }
        return Ok(());
    }

    pretty_print_plan(&plan);

    // 2. dry_run → done.
    if args.dry_run {
        return Ok(());
    }

    // 3. Approval prompt when the plan is pending.
    if let Some(approval) = plan.get("approval")
        && approval
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            == "pending"
    {
        let plan_id = approval
            .get("plan_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if plan_id.is_empty() {
            return Err("approval response missing plan_id".into());
        }
        if !std::io::stdin().is_terminal() {
            // Non-TTY: don't block on stdin. Tell the operator
            // explicitly that a decision is pending.
            println!();
            println!(
                "(non-interactive stdin: plan {plan_id} stays pending — decide via \
                 `relix planning approve {plan_id}` or `relix planning reject {plan_id}`)"
            );
            return Ok(());
        }
        match prompt_decision()? {
            Decision::Approve(note) => {
                // RELIX-7.24 streaming verification: spawn a
                // task that consumes the SSE stream and
                // prints each new entry live AS the approve
                // call executes the workflow. Tied to a
                // cancel sender so the stream consumer exits
                // cleanly once approve_plan returns.
                let bridge_for_stream = args.bridge.clone();
                let plan_id_for_stream = plan_id.clone();
                let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
                let stream_handle = tokio::spawn(async move {
                    stream_verification_live(&bridge_for_stream, &plan_id_for_stream, stop_rx)
                        .await;
                });
                let approval_response =
                    call_approve(&args.bridge, &plan_id, note.as_deref()).await?;
                // Approve returned — execution is done. Tell
                // the stream task to wrap up.
                let _ = stop_tx.send(());
                let _ = stream_handle.await;
                pretty_print_approval(&approval_response);
                // Final full log (catches any entry the
                // stream's last poll missed in the race).
                if let Ok(log) = fetch_verification_log(&args.bridge, &plan_id).await {
                    pretty_print_verification_log(&log);
                }
            }
            Decision::Reject(note) => {
                let reject_response = call_reject(&args.bridge, &plan_id, note.as_deref()).await?;
                pretty_print_reject(&reject_response);
            }
        }
    } else {
        // 4. Execution ran inline. If verification fired,
        // fetch + display the log.
        let plan_id = plan
            .get("plan_spec")
            .and_then(|s| s.get("spec_id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !plan_id.is_empty()
            && plan.get("verification").is_some()
            && let Ok(log) = fetch_verification_log(&args.bridge, &plan_id).await
        {
            pretty_print_verification_log(&log);
        }
    }
    Ok(())
}

// ── interactive prompt ──────────────────────────────

enum Decision {
    Approve(Option<String>),
    Reject(Option<String>),
}

fn prompt_decision() -> Result<Decision, Box<dyn std::error::Error>> {
    println!();
    print!("approve? [y/N] ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let trimmed = line.trim().to_ascii_lowercase();
    let approve = matches!(trimmed.as_str(), "y" | "yes");
    print!("note (optional, press enter to skip): ");
    std::io::stdout().flush().ok();
    let mut note = String::new();
    std::io::stdin().lock().read_line(&mut note)?;
    let note_trimmed = note.trim().to_string();
    let note = if note_trimmed.is_empty() {
        None
    } else {
        Some(note_trimmed)
    };
    if approve {
        Ok(Decision::Approve(note))
    } else {
        Ok(Decision::Reject(note))
    }
}

// ── pretty printers ──────────────────────────────────

fn pretty_print_plan(v: &Value) {
    let goal = v
        .get("plan_spec")
        .and_then(|s| s.get("goal"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let workflow_name = v
        .get("workflow_name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let topology = v.get("topology").and_then(Value::as_str).unwrap_or("?");
    println!("relix build — plan generated");
    println!("  goal:           {goal}");
    println!("  workflow_name:  {workflow_name}");
    println!("  topology:       {topology}");

    let orch_activated = v
        .get("orchestrator_activated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let specialist_count = v
        .get("specialist_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "  orchestrator:   {} (specialists={specialist_count})",
        if orch_activated { "ACTIVE" } else { "skipped" }
    );
    let critic_rounds = v.get("critic_rounds").and_then(Value::as_u64).unwrap_or(0);
    let critic_approved = v
        .get("critic_approved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!(
        "  critic:         {} (rounds={critic_rounds})",
        if critic_approved {
            "APPROVED"
        } else {
            "NOT APPROVED"
        }
    );
    if let Some(report) = v.get("conflict_resolution_report")
        && !report.is_null()
    {
        let detected = report
            .get("conflicts_detected")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let resolved = report
            .get("conflicts_resolved")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let strategy = report
            .get("strategy_used")
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!("  conflicts:      detected={detected} resolved={resolved} strategy={strategy}");
    }
    if let Some(approval) = v.get("approval")
        && !approval.is_null()
    {
        let plan_id = approval
            .get("plan_id")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let status = approval
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let notified = approval
            .get("notified_targets")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("  approval:       {status} plan_id={plan_id} notified_targets={notified}");
    }
    if let Some(exec) = v.get("execution")
        && !exec.is_null()
    {
        let status = exec.get("status").and_then(Value::as_str).unwrap_or("?");
        let latency = exec
            .get("total_latency_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("  execution:      {status} latency={latency}ms");
    }
    if let Some(verify) = v.get("verification")
        && !verify.is_null()
    {
        let passed = verify
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let total = verify
            .get("total_entries")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let critical = verify
            .get("critical_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let advisory = verify
            .get("advisory_failures")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!(
            "  verification:   {} (entries={total} critical={critical} advisory={advisory})",
            if passed { "PASS" } else { "FAIL" }
        );
    }
    if let Some(yaml) = v.get("workflow_yaml").and_then(Value::as_str) {
        println!("\n--- workflow_yaml ---\n{yaml}");
    }
}

fn pretty_print_approval(v: &Value) {
    println!();
    println!("APPROVED");
    let record = v.get("record").cloned().unwrap_or(v.clone());
    println!(
        "  plan_id:       {}",
        record.get("plan_id").and_then(Value::as_str).unwrap_or("?")
    );
    if let Some(note) = record.get("decision_note").and_then(Value::as_str) {
        println!("  note:          {note}");
    }
    if let Some(exec) = v.get("execution")
        && !exec.is_null()
    {
        let status = exec.get("status").and_then(Value::as_str).unwrap_or("?");
        let latency = exec
            .get("total_latency_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("  execution:     {status} latency={latency}ms");
        if let Some(result) = exec.get("result").and_then(Value::as_str) {
            let preview: String = result.chars().take(400).collect();
            println!("  result:        {preview}");
        }
    } else {
        println!("  (execution deferred — coordinator mesh dispatcher not yet wired)");
    }
}

fn pretty_print_reject(v: &Value) {
    println!();
    println!("REJECTED");
    println!(
        "  plan_id:       {}",
        v.get("plan_id").and_then(Value::as_str).unwrap_or("?")
    );
    if let Some(note) = v.get("decision_note").and_then(Value::as_str) {
        println!("  note:          {note}");
    }
}

fn pretty_print_verification_log(v: &Value) {
    let entries = v
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        return;
    }
    println!();
    println!(
        "--- verification log ({} entr{}) ---",
        entries.len(),
        if entries.len() == 1 { "y" } else { "ies" }
    );
    for e in entries {
        let step = e.get("step_id").and_then(Value::as_str).unwrap_or("?");
        let strategy = e
            .get("strategy_used")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let passed = e.get("passed").and_then(Value::as_bool).unwrap_or(false);
        let reason = e.get("reason").and_then(Value::as_str).unwrap_or("");
        println!(
            "  [{}] {step:<24} {strategy:<18}  {reason}",
            if passed { "PASS" } else { "FAIL" }
        );
    }
}

// ── HTTP / file helpers ──────────────────────────────

fn resolve_spec(
    inline: Option<String>,
    file: Option<PathBuf>,
) -> Result<String, Box<dyn std::error::Error>> {
    match (inline, file) {
        (Some(s), None) => {
            if s.trim().is_empty() {
                Err("spec must not be empty".into())
            } else {
                Ok(s)
            }
        }
        (None, Some(p)) => {
            let txt = std::fs::read_to_string(&p)
                .map_err(|e| format!("read --spec-file {}: {e}", p.display()))?;
            if txt.trim().is_empty() {
                Err(format!("--spec-file {} is empty", p.display()).into())
            } else {
                Ok(txt)
            }
        }
        (Some(_), Some(_)) => Err("pass either spec or --spec-file, not both".into()),
        (None, None) => Err("a spec is required (positional or --spec-file)".into()),
    }
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

async fn http_post_json(url: &str, payload: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?
        .post(url)
        .json(payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

async fn call_approve(
    bridge: &str,
    plan_id: &str,
    note: Option<&str>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/approve", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("plan_id".into(), Value::from(plan_id.to_string()));
    if let Some(n) = note {
        body.insert("note".into(), Value::from(n.to_string()));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    serde_json::from_str(&resp)
        .map_err(|e| format!("decode approve response: {e} (body={resp})").into())
}

async fn call_reject(
    bridge: &str,
    plan_id: &str,
    note: Option<&str>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/reject", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("plan_id".into(), Value::from(plan_id.to_string()));
    if let Some(n) = note {
        body.insert("note".into(), Value::from(n.to_string()));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    serde_json::from_str(&resp)
        .map_err(|e| format!("decode reject response: {e} (body={resp})").into())
}

async fn fetch_verification_log(
    bridge: &str,
    plan_id: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/planning/verification/{}",
        bridge.trim_end_matches('/'),
        urlencode(plan_id)
    );
    let body = http_get(&url).await?;
    serde_json::from_str(&body)
        .map_err(|e| format!("decode verification log: {e} (body={body})").into())
}

/// Open the SSE stream and print each `entry` event as it
/// arrives. Exits cleanly when `stop` fires (the parent task
/// signals end-of-execution) OR when the server closes the
/// stream OR when the connection errors. Best-effort: any
/// transport error is silently swallowed so the parent's
/// approve_plan flow stays the source of truth.
async fn stream_verification_live(
    bridge: &str,
    plan_id: &str,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let url = format!(
        "{}/v1/planning/verification/{}/stream",
        bridge.trim_end_matches('/'),
        urlencode(plan_id)
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return,
    };
    println!();
    println!("--- live verification stream ---");
    let mut buf = String::new();
    let mut stream = resp;
    loop {
        tokio::select! {
            _ = &mut stop => return,
            chunk = stream.chunk() => {
                let Ok(Some(bytes)) = chunk else { return };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                // Process complete SSE messages (delimited
                // by a blank line). Each message has the
                // form `event: <name>\ndata: <payload>\n\n`.
                while let Some(idx) = buf.find("\n\n") {
                    let msg: String = buf[..idx].to_string();
                    buf.replace_range(..idx + 2, "");
                    let mut event_name: Option<String> = None;
                    let mut data: Option<String> = None;
                    for line in msg.lines() {
                        if let Some(rest) = line.strip_prefix("event:") {
                            event_name = Some(rest.trim().to_string());
                        } else if let Some(rest) = line.strip_prefix("data:") {
                            data = Some(rest.trim().to_string());
                        }
                    }
                    if let (Some(name), Some(payload)) = (event_name, data) {
                        match name.as_str() {
                            "entry" => print_stream_entry(&payload),
                            "done" => return,
                            _ => {} // ignore heartbeat / keep-alive
                        }
                    }
                }
            }
        }
    }
}

fn print_stream_entry(payload: &str) {
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    let step = v.get("step_id").and_then(Value::as_str).unwrap_or("?");
    let strategy = v
        .get("strategy_used")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let passed = v.get("passed").and_then(Value::as_bool).unwrap_or(false);
    let reason = v.get("reason").and_then(Value::as_str).unwrap_or("");
    println!(
        "  [{}] {step:<24} {strategy:<18}  {reason}",
        if passed { "PASS" } else { "FAIL" }
    );
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_spec_accepts_positional() {
        assert_eq!(resolve_spec(Some("hi".into()), None).unwrap(), "hi");
    }

    #[test]
    fn resolve_spec_rejects_both() {
        assert!(resolve_spec(Some("a".into()), Some(PathBuf::from("b"))).is_err());
    }

    #[test]
    fn resolve_spec_rejects_neither() {
        assert!(resolve_spec(None, None).is_err());
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("plan-123"), "plan-123");
        assert_eq!(urlencode("plan id"), "plan%20id");
    }
}
