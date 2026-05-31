//! `relix-cli planning ...` — RELIX-7.24 operator surface.
//!
//! Four subcommands, each a thin HTTP forwarder onto the
//! `/v1/planning/*` bridge endpoints:
//!
//! - `planning agents` — list every agent in the capability
//!   registry visible to the coordinator.
//! - `planning search --task "..."` — score the registry against
//!   a free-text task and print the best matches.
//! - `planning validate --spec "..."` (or `--spec-file <path>`) —
//!   parse a spec into a structured PlanSpec and print it.
//! - `planning plan --spec "..."` (or `--spec-file <path>`) — run
//!   the full spec → workflow generation and either print the
//!   generated workflow (`--dry-run`) or execute it via the
//!   coordinator's WorkflowDispatcher.
//!
//! Every subcommand accepts `--bridge <url>` (default
//! `http://127.0.0.1:19791`) and `--raw` to dump the JSON body
//! verbatim.

use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every agent visible to the planning capability
    /// registry on the coordinator.
    Agents {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Score the registry against a free-text task. Prints
    /// matches ordered by descending score with the
    /// capabilities that contributed.
    Search {
        /// Free-text task description.
        #[arg(long)]
        task: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Parse a natural-language spec into a structured
    /// PlanSpec (goal, constraints, success criteria,
    /// preferred/forbidden agents, max steps, budget hint).
    /// Pass the spec inline via `--spec` or from a file via
    /// `--spec-file`.
    Validate {
        #[arg(long)]
        spec: Option<String>,
        #[arg(long)]
        spec_file: Option<PathBuf>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Generate a workflow from a spec. With `--dry-run` (the
    /// default) only prints the generated workflow YAML +
    /// topology + selected agents. Without `--dry-run`,
    /// schedules the workflow on the coordinator and prints
    /// the execution summary.
    Plan {
        #[arg(long)]
        spec: Option<String>,
        #[arg(long)]
        spec_file: Option<PathBuf>,
        /// Maximum number of agents to include in the
        /// generated workflow.
        #[arg(long)]
        max_agents: Option<usize>,
        /// When true (the default), only print the generated
        /// workflow without executing it.
        #[arg(long, default_value_t = true)]
        dry_run: bool,
        /// Execute the generated workflow on the coordinator.
        /// Short-hand for `--dry-run=false`.
        #[arg(long, default_value_t = false)]
        execute: bool,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-1/3 status: prints the configured
    /// orchestrator + critic settings on the coordinator and
    /// whether the AI dispatcher cell has been wired. Useful
    /// for confirming a fresh boot has the planning pipeline
    /// fully online.
    Status {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-4: approve a pending plan and trigger
    /// execution.
    Approve {
        /// `plan_id` (uuid) of the pending plan.
        plan_id: String,
        /// Operator-facing decision note recorded alongside
        /// the approval.
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-4: reject a pending plan. No
    /// execution happens.
    Reject {
        plan_id: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-4: list approval records. Filter via
    /// `--status pending|approved|rejected|expired`.
    Approvals {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-4: fetch one approval record by id.
    Approval {
        plan_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 Stage-5: full step-level verification log
    /// for one plan.
    Verification {
        plan_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.24 follow-up: export a stored plan as a
    /// portable artifact for external trackers. `--format
    /// markdown` produces a human-readable summary; default
    /// (`json`) produces the full structured PlanSpec +
    /// workflow_yaml + signature, suitable for any tool that
    /// can consume stable JSON. `--output path.{md,json}`
    /// writes to disk instead of stdout.
    Export {
        plan_id: String,
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Agents { bridge, raw } => agents(&bridge, raw).await,
        Cmd::Search { task, bridge, raw } => search(&bridge, &task, raw).await,
        Cmd::Validate {
            spec,
            spec_file,
            bridge,
            raw,
        } => {
            let spec = resolve_spec(spec, spec_file)?;
            validate(&bridge, &spec, raw).await
        }
        Cmd::Plan {
            spec,
            spec_file,
            max_agents,
            dry_run,
            execute,
            bridge,
            raw,
        } => {
            let spec = resolve_spec(spec, spec_file)?;
            let effective_dry_run = !execute && dry_run;
            plan(&bridge, &spec, max_agents, effective_dry_run, raw).await
        }
        Cmd::Status { bridge, raw } => status(&bridge, raw).await,
        Cmd::Approve {
            plan_id,
            note,
            bridge,
            raw,
        } => approve(&bridge, &plan_id, note.as_deref(), raw).await,
        Cmd::Reject {
            plan_id,
            note,
            bridge,
            raw,
        } => reject(&bridge, &plan_id, note.as_deref(), raw).await,
        Cmd::Approvals {
            status,
            bridge,
            raw,
        } => list_approvals(&bridge, status.as_deref(), raw).await,
        Cmd::Approval {
            plan_id,
            bridge,
            raw,
        } => get_approval(&bridge, &plan_id, raw).await,
        Cmd::Verification {
            plan_id,
            bridge,
            raw,
        } => verification(&bridge, &plan_id, raw).await,
        Cmd::Export {
            plan_id,
            format,
            output,
            bridge,
            raw,
        } => export(&bridge, &plan_id, &format, output.as_deref(), raw).await,
    }
}

async fn export(
    bridge: &str,
    plan_id: &str,
    format: &str,
    output: Option<&std::path::Path>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/planning/export/{}?format={}",
        bridge.trim_end_matches('/'),
        urlencode(plan_id),
        urlencode(format)
    );
    let body = http_get(&url).await?;
    if raw {
        if let Some(path) = output {
            std::fs::write(path, body.as_bytes())?;
            println!("(wrote {} bytes to {})", body.len(), path.display());
        } else {
            println!("{body}");
        }
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode export response: {e} (body={body})"))?;
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .ok_or("export response missing `content` field")?;
    if let Some(path) = output {
        std::fs::write(path, content.as_bytes())?;
        println!("(wrote {} bytes to {})", content.len(), path.display());
    } else {
        println!("{content}");
    }
    Ok(())
}

async fn verification(
    bridge: &str,
    plan_id: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/planning/verification/{}",
        bridge.trim_end_matches('/'),
        urlencode(plan_id)
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode verification response: {e} (body={body})"))?;
    let entries = v
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if entries.is_empty() {
        println!("(no verification entries for plan_id={plan_id})");
        return Ok(());
    }
    println!(
        "{:<28} {:<8} {:<20} {:<6}  reason",
        "step_id", "passed", "strategy", "ts"
    );
    for e in entries {
        let step = e.get("step_id").and_then(Value::as_str).unwrap_or("?");
        let strategy = e
            .get("strategy_used")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let passed = e.get("passed").and_then(Value::as_bool).unwrap_or(false);
        let ts = e.get("verified_at_ms").and_then(Value::as_i64).unwrap_or(0);
        let reason = e.get("reason").and_then(Value::as_str).unwrap_or("");
        println!(
            "{step:<28} {pass:<8} {strategy:<20} {ts:<6}  {reason}",
            pass = if passed { "PASS" } else { "FAIL" },
        );
    }
    Ok(())
}

async fn approve(
    bridge: &str,
    plan_id: &str,
    note: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/approve", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("plan_id".into(), Value::from(plan_id.to_string()));
    if let Some(n) = note {
        body.insert("note".into(), Value::from(n.to_string()));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp)
        .map_err(|e| format!("decode approve response: {e} (body={resp})"))?;
    let record = v.get("record").cloned().unwrap_or(v.clone());
    println!(
        "plan_id:       {}",
        record.get("plan_id").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "status:        {}",
        record.get("status").and_then(Value::as_str).unwrap_or("?")
    );
    if let Some(note) = record.get("decision_note").and_then(Value::as_str) {
        println!("decision_note: {note}");
    }
    if let Some(exec) = v.get("execution")
        && !exec.is_null()
    {
        println!("\n--- execution ---");
        let exec_id = exec
            .get("execution_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let status = exec.get("status").and_then(Value::as_str).unwrap_or("");
        let latency = exec
            .get("total_latency_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("execution_id:  {exec_id}");
        println!("status:        {status}");
        println!("total_latency: {latency}ms");
        if let Some(result) = exec.get("result")
            && !result.is_null()
        {
            println!("result:        {result}");
        }
    } else {
        println!("(execution deferred — coordinator mesh dispatcher not yet wired)");
    }
    Ok(())
}

async fn reject(
    bridge: &str,
    plan_id: &str,
    note: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/reject", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("plan_id".into(), Value::from(plan_id.to_string()));
    if let Some(n) = note {
        body.insert("note".into(), Value::from(n.to_string()));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp)
        .map_err(|e| format!("decode reject response: {e} (body={resp})"))?;
    println!(
        "plan_id:       {}",
        v.get("plan_id").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "status:        {}",
        v.get("status").and_then(Value::as_str).unwrap_or("?")
    );
    if let Some(note) = v.get("decision_note").and_then(Value::as_str) {
        println!("decision_note: {note}");
    }
    Ok(())
}

async fn list_approvals(
    bridge: &str,
    status: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!("{}/v1/planning/approvals", bridge.trim_end_matches('/'));
    if let Some(s) = status
        && !s.is_empty()
    {
        url.push_str(&format!("?status={}", urlencode(s)));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode approvals response: {e} (body={body})"))?;
    let list = v
        .get("approvals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if list.is_empty() {
        println!("(no approval records)");
        return Ok(());
    }
    for r in list {
        let plan_id = r.get("plan_id").and_then(Value::as_str).unwrap_or("?");
        let status = r.get("status").and_then(Value::as_str).unwrap_or("?");
        let created = r.get("created_at_ms").and_then(Value::as_i64).unwrap_or(0);
        let goal = r
            .get("spec")
            .and_then(|s| s.get("goal"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let goal_preview: String = goal.chars().take(72).collect();
        println!("{plan_id:<36}  {status:<10} created={created}  goal=\"{goal_preview}\"");
    }
    Ok(())
}

async fn get_approval(
    bridge: &str,
    plan_id: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/planning/approvals/{}",
        bridge.trim_end_matches('/'),
        urlencode(plan_id)
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode approval response: {e} (body={body})"))?;
    println!(
        "plan_id:       {}",
        v.get("plan_id").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "status:        {}",
        v.get("status").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "created_at_ms: {}",
        v.get("created_at_ms").and_then(Value::as_i64).unwrap_or(0)
    );
    if let Some(d) = v.get("decided_at_ms").and_then(Value::as_i64) {
        println!("decided_at_ms: {d}");
    }
    if let Some(note) = v.get("decision_note").and_then(Value::as_str) {
        println!("decision_note: {note}");
    }
    if let Some(spec) = v.get("spec") {
        let goal = spec.get("goal").and_then(Value::as_str).unwrap_or("");
        println!("goal:          {goal}");
        if let Some(sid) = spec.get("spec_id").and_then(Value::as_str) {
            println!("spec_id:       {sid}");
        }
        if let Some(score) = spec.get("complexity_score").and_then(Value::as_f64) {
            println!("complexity:    {score:.2}");
        }
    }
    if let Some(yaml) = v.get("workflow_yaml").and_then(Value::as_str) {
        println!("\n--- workflow_yaml ---\n{yaml}");
    }
    Ok(())
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

async fn agents(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/agents", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode agents response: {e} (body={body})"))?;
    let list = v
        .get("agents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if list.is_empty() {
        println!("(no agents registered)");
        return Ok(());
    }
    for a in list {
        let name = a.get("name").and_then(Value::as_str).unwrap_or("?");
        let peer = a.get("peer").and_then(Value::as_str).unwrap_or("(local)");
        let caps = a
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|c| c.len())
            .unwrap_or(0);
        let desc = a.get("description").and_then(Value::as_str).unwrap_or("");
        if desc.is_empty() {
            println!("{name:<28} peer={peer:<18} caps={caps}");
        } else {
            println!("{name:<28} peer={peer:<18} caps={caps}  {desc}");
        }
    }
    Ok(())
}

async fn search(bridge: &str, task: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    if task.trim().is_empty() {
        return Err("--task is required".into());
    }
    let url = format!("{}/v1/planning/agents/search", bridge.trim_end_matches('/'));
    let body = serde_json::json!({ "task": task });
    let resp = http_post_json(&url, &body).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&resp).map_err(|e| format!("decode search: {e} (body={resp})"))?;
    let matches = v
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if matches.is_empty() {
        println!("(no matching agents for task)");
        return Ok(());
    }
    for m in matches {
        let agent = m.get("agent").and_then(Value::as_str).unwrap_or("?");
        let score = m.get("score").and_then(Value::as_u64).unwrap_or(0);
        let caps = m
            .get("matched_capabilities")
            .and_then(Value::as_array)
            .map(|c| {
                c.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        println!("{agent:<28} score={score:<4} caps=[{caps}]");
    }
    Ok(())
}

async fn validate(bridge: &str, spec: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/validate", bridge.trim_end_matches('/'));
    let body = serde_json::json!({ "spec": spec });
    let resp = http_post_json(&url, &body).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&resp).map_err(|e| format!("decode validate: {e} (body={resp})"))?;
    let goal = v.get("goal").and_then(Value::as_str).unwrap_or("");
    println!("goal:               {goal}");
    print_str_list("constraints:       ", v.get("constraints"));
    print_str_list("success_criteria:  ", v.get("success_criteria"));
    print_str_list("preferred_agents:  ", v.get("preferred_agents"));
    print_str_list("forbidden_agents:  ", v.get("forbidden_agents"));
    match v.get("max_steps").and_then(Value::as_u64) {
        Some(n) => println!("max_steps:          {n}"),
        None => println!("max_steps:          (none)"),
    }
    match v.get("budget_hint").and_then(Value::as_str) {
        Some(b) if !b.is_empty() => println!("budget_hint:        {b}"),
        _ => println!("budget_hint:        (none)"),
    }
    Ok(())
}

async fn plan(
    bridge: &str,
    spec: &str,
    max_agents: Option<usize>,
    dry_run: bool,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/plan", bridge.trim_end_matches('/'));
    let mut body = serde_json::Map::new();
    body.insert("spec".into(), Value::from(spec.to_string()));
    body.insert("dry_run".into(), Value::from(dry_run));
    if let Some(n) = max_agents {
        body.insert("max_agents".into(), Value::from(n));
    }
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&resp).map_err(|e| format!("decode plan: {e} (body={resp})"))?;
    let topology = v.get("topology").and_then(Value::as_str).unwrap_or("?");
    let name = v
        .get("workflow_name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    println!("workflow_name:  {name}");
    println!("topology:       {topology}");
    print_str_list("agents_selected:", v.get("agents_selected"));

    // RELIX-7.24 Stage-1/3 fields.
    let orch_activated = v
        .get("orchestrator_activated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let specialist_count = v
        .get("specialist_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!(
        "orchestrator:   {} (specialist_count={specialist_count})",
        if orch_activated { "ACTIVE" } else { "skipped" }
    );
    if orch_activated && let Some(o) = v.get("orchestrator") {
        if let Some(s) = o.get("complexity_score").and_then(Value::as_f64) {
            let t = o
                .get("complexity_threshold")
                .and_then(Value::as_f64)
                .unwrap_or(0.6);
            println!("                complexity {s:.2} >= threshold {t:.2}");
        }
        if let Some(decomposed_by_heuristic) =
            o.get("decomposed_by_heuristic").and_then(Value::as_bool)
            && decomposed_by_heuristic
        {
            println!("                (decomposed via heuristic — AI decomposer unreachable)");
        }
        print_str_list("                sub_goals:", o.get("sub_goals"));
    }

    let critic_rounds = v.get("critic_rounds").and_then(Value::as_u64).unwrap_or(0);
    let critic_approved = v
        .get("critic_approved")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!(
        "critic:         {} (rounds={critic_rounds})",
        if critic_approved {
            "APPROVED"
        } else {
            "NOT APPROVED"
        }
    );
    if let Some(c) = v.get("critic")
        && let Some(w) = c.get("warning").and_then(Value::as_str)
        && !w.is_empty()
    {
        println!("                warning: {w}");
    }

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
        println!("conflicts:      detected={detected} resolved={resolved} strategy={strategy}");
        if let Some(esc) = report.get("escalated").and_then(Value::as_str) {
            println!("                ESCALATED: {esc}");
        }
    }

    if let Some(yaml) = v.get("workflow_yaml").and_then(Value::as_str) {
        println!("\n--- workflow_yaml ---\n{yaml}");
    }
    if let Some(exec) = v.get("execution")
        && !exec.is_null()
    {
        println!("\n--- execution ---");
        let exec_id = exec
            .get("execution_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let status = exec.get("status").and_then(Value::as_str).unwrap_or("");
        let latency = exec
            .get("total_latency_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        println!("execution_id:   {exec_id}");
        println!("status:         {status}");
        println!("total_latency:  {latency}ms");
        if let Some(result) = exec.get("result")
            && !result.is_null()
        {
            println!("result:         {result}");
        }
    }
    Ok(())
}

async fn status(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/planning/status", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode status: {e} (body={body})"))?;
    let orch = v.get("orchestrator").cloned().unwrap_or(Value::Null);
    let critic = v.get("critic").cloned().unwrap_or(Value::Null);
    let live = v
        .get("dispatcher_live")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    println!("orchestrator:");
    println!(
        "  enabled:                 {}",
        orch.get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    println!(
        "  agent:                   {}",
        orch.get("agent").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "  peer:                    {}",
        orch.get("peer").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "  complexity_threshold:    {:.2}",
        orch.get("complexity_threshold")
            .and_then(Value::as_f64)
            .unwrap_or(0.6)
    );
    println!(
        "  max_parallel_specialists:{}",
        orch.get("max_parallel_specialists")
            .and_then(Value::as_u64)
            .unwrap_or(4)
    );
    println!("critic:");
    println!(
        "  enabled:                 {}",
        critic
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    println!(
        "  agent:                   {}",
        critic.get("agent").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "  peer:                    {}",
        critic.get("peer").and_then(Value::as_str).unwrap_or("?")
    );
    println!(
        "  max_rounds:              {}",
        critic
            .get("max_rounds")
            .and_then(Value::as_u64)
            .unwrap_or(3)
    );
    println!(
        "dispatcher_live:             {} ({})",
        live,
        if live {
            "AI peers reachable"
        } else {
            "heuristic fallback only"
        }
    );
    Ok(())
}

fn resolve_spec(
    inline: Option<String>,
    file: Option<PathBuf>,
) -> Result<String, Box<dyn std::error::Error>> {
    match (inline, file) {
        (Some(s), None) => {
            if s.trim().is_empty() {
                Err("--spec must not be empty".into())
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
        (Some(_), Some(_)) => Err("pass either --spec or --spec-file, not both".into()),
        (None, None) => Err("one of --spec or --spec-file is required".into()),
    }
}

fn print_str_list(label: &str, v: Option<&Value>) {
    let items: Vec<String> = v
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        println!("{label} (none)");
    } else {
        println!("{label} {}", items.join(", "));
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
        .timeout(Duration::from_secs(120))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_spec_requires_one_of_inline_or_file() {
        let err = resolve_spec(None, None).unwrap_err().to_string();
        assert!(err.contains("--spec"));
    }

    #[test]
    fn resolve_spec_rejects_both_inline_and_file() {
        let err = resolve_spec(Some("x".into()), Some("/tmp/x.txt".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"));
    }

    #[test]
    fn resolve_spec_rejects_empty_inline() {
        let err = resolve_spec(Some("   ".into()), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn resolve_spec_returns_inline_when_present() {
        assert_eq!(resolve_spec(Some("hi".into()), None).unwrap(), "hi");
    }

    #[test]
    fn resolve_spec_reads_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("spec.txt");
        std::fs::write(&p, "from-file").unwrap();
        assert_eq!(resolve_spec(None, Some(p)).unwrap(), "from-file");
    }

    #[test]
    fn resolve_spec_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        std::fs::write(&p, "   \n").unwrap();
        let err = resolve_spec(None, Some(p)).unwrap_err().to_string();
        assert!(err.contains("empty"));
    }
}
