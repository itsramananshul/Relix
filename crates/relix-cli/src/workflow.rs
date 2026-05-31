//! `relix-cli workflow ...` — operator surface for the
//! workflow engine, talking to the local bridge over HTTP.
//!
//! Four subcommands, each a thin HTTP forwarder onto the
//! `/v1/workflows*` bridge endpoints (which in turn forward
//! to the coordinator's `workflow.*` capabilities):
//!
//! - `workflow list`                       → catalog table.
//! - `workflow run <name> --input <text>`  → execute + render
//!   the final result + per-step trace.
//! - `workflow validate <file>`            → type-check a
//!   `.workflow` file before committing.
//! - `workflow trace <execution-id>`       → look up a past
//!   execution by id and render its trace.
//!
//! Every subcommand accepts `--bridge <url>` (defaults to
//! `http://127.0.0.1:19791`) and `--raw` to dump the bridge
//! JSON verbatim instead of the formatted view — useful for
//! scripting + debugging.

use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every `.workflow` file the coordinator's
    /// workflow store can see.
    List {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print raw JSON instead of the formatted table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Execute a workflow by name. Body is JSON over the
    /// bridge — operators don't need to think about the
    /// envelope shape.
    Run {
        /// Workflow name (matches `<name>.workflow` in the
        /// workflows directory).
        name: String,
        /// Workflow input. Bound to `{{workflow.input}}` in
        /// the first agent step.
        #[arg(long, default_value = "")]
        input: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print the raw bridge JSON response instead of the
        /// formatted result + step table.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Type-check a `.workflow` source file. Catches parse
    /// errors, undefined variable references, cycles, and
    /// missing peers (when peers are configured on the
    /// coordinator).
    Validate {
        /// Path to the `.workflow` file to check.
        file: PathBuf,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print raw bridge JSON instead of the formatted
        /// summary.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Look up a past execution by id and render its trace.
    /// Same payload `workflow run` prints on completion;
    /// useful when you missed the output the first time
    /// (`run` printed only the result preview, or you ran a
    /// workflow from the dashboard / API).
    Trace {
        /// Execution id (32 hex chars, printed by
        /// `workflow run`).
        execution_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print raw bridge JSON instead of the formatted
        /// trace view.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Drop the workflow file cache on the coordinator so the
    /// next list / run picks up any in-place edits without
    /// requiring a coordinator restart.
    Reload {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print raw bridge JSON instead of `ok`.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List { bridge, raw } => list(&bridge, raw).await,
        Cmd::Run {
            name,
            input,
            bridge,
            raw,
        } => run_workflow(&bridge, &name, &input, raw).await,
        Cmd::Validate { file, bridge, raw } => validate(&bridge, &file, raw).await,
        Cmd::Trace {
            execution_id,
            bridge,
            raw,
        } => trace(&bridge, &execution_id, raw).await,
        Cmd::Reload { bridge, raw } => reload(&bridge, raw).await,
    }
}

// ── command bodies ───────────────────────────────────────

async fn list(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/workflows", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        print_raw(&body);
        return Ok(());
    }
    let entries: Vec<ListEntry> =
        serde_json::from_str(&body).map_err(|e| format!("decode list body: {e} (body={body})"))?;
    if entries.is_empty() {
        println!("(no workflows found; check the workflows directory on the coordinator)");
        return Ok(());
    }
    render_list(&entries);
    Ok(())
}

async fn run_workflow(
    bridge: &str,
    name: &str,
    input: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/workflows/run", bridge.trim_end_matches('/'));
    let req = serde_json::json!({
        "name": name,
        "input": input,
    });
    let body = http_post(&url, &req).await?;
    if raw {
        print_raw(&body);
        return Ok(());
    }
    let rec: ExecutionRecord =
        serde_json::from_str(&body).map_err(|e| format!("decode run body: {e} (body={body})"))?;
    render_execution(&rec);
    Ok(())
}

async fn validate(
    bridge: &str,
    file: &PathBuf,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let source =
        std::fs::read_to_string(file).map_err(|e| format!("read {}: {e}", file.display()))?;
    let url = format!("{}/v1/workflows/validate", bridge.trim_end_matches('/'));
    let req = serde_json::json!({ "source": source });
    let (status, body) = http_post_with_status(&url, &req).await?;
    if raw {
        print_raw(&body);
        if !status.is_success() {
            std::process::exit(2);
        }
        return Ok(());
    }
    if status.is_success() {
        let ok: ValidateOk = serde_json::from_str(&body)
            .map_err(|e| format!("decode validate body: {e} (body={body})"))?;
        println!(
            "ok\n  name        : {}\n  description : {}\n  version     : {}",
            ok.name, ok.description, ok.version,
        );
        Ok(())
    } else {
        // Bridge returns the {ok:false, error:...} JSON on
        // 400; render the error verbatim so operators see
        // the exact validation message.
        let err: ValidateErr = serde_json::from_str(&body)
            .map_err(|e| format!("decode validate err body: {e} (body={body})"))?;
        eprintln!("validation failed: {}", err.error);
        std::process::exit(2);
    }
}

async fn trace(
    bridge: &str,
    execution_id: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/workflows/status/{}",
        bridge.trim_end_matches('/'),
        execution_id,
    );
    let body = http_get(&url).await?;
    if raw {
        print_raw(&body);
        return Ok(());
    }
    let rec: ExecutionRecord =
        serde_json::from_str(&body).map_err(|e| format!("decode trace body: {e} (body={body})"))?;
    render_execution(&rec);
    Ok(())
}

async fn reload(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/workflows/reload", bridge.trim_end_matches('/'));
    let body = http_post(&url, &serde_json::json!({})).await?;
    if raw {
        print_raw(&body);
    } else {
        println!("ok (workflow file cache cleared)");
    }
    Ok(())
}

// ── rendering ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ListEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
struct ValidateOk {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    version: u32,
}

#[derive(Debug, Deserialize)]
struct ValidateErr {
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize)]
struct ExecutionRecord {
    #[serde(default)]
    execution_id: String,
    #[serde(default)]
    workflow_name: String,
    #[serde(default)]
    input: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    result: String,
    #[serde(default)]
    total_latency_ms: u64,
    #[serde(default)]
    steps: Vec<TraceStep>,
}

#[derive(Debug, Deserialize)]
struct TraceStep {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    peer: String,
    #[serde(default)]
    capability: String,
    #[serde(default)]
    latency_ms: u64,
    #[serde(default)]
    error: Option<String>,
}

fn render_list(entries: &[ListEntry]) {
    let name_w = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let desc_w = entries
        .iter()
        .map(|e| e.description.len())
        .max()
        .unwrap_or(11)
        .max(11);
    println!(
        "{name:<name_w$}  ver  {desc:<desc_w$}  path",
        name = "name",
        desc = "description",
    );
    for e in entries {
        println!(
            "{name:<name_w$}  {ver:>3}  {desc:<desc_w$}  {path}",
            name = e.name,
            ver = e.version,
            desc = e.description,
            path = e.path,
        );
    }
}

fn render_execution(rec: &ExecutionRecord) {
    println!("execution    : {}", rec.execution_id);
    println!("workflow     : {}", rec.workflow_name);
    println!("status       : {}", rec.status);
    println!("input        : {}", rec.input);
    println!("latency (ms) : {}", rec.total_latency_ms);
    println!("result       : {}", rec.result);
    if rec.steps.is_empty() {
        println!("(no steps recorded)");
        return;
    }
    println!();
    let agent_w = rec
        .steps
        .iter()
        .map(|s| s.agent.len())
        .max()
        .unwrap_or(5)
        .max(5);
    let peer_w = rec
        .steps
        .iter()
        .map(|s| s.peer.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let cap_w = rec
        .steps
        .iter()
        .map(|s| s.capability.len())
        .max()
        .unwrap_or(10)
        .max(10);
    println!(
        "{agent:<agent_w$}  {peer:<peer_w$}  {cap:<cap_w$}  {lat:>8}  status",
        agent = "agent",
        peer = "peer",
        cap = "capability",
        lat = "lat (ms)",
    );
    for s in &rec.steps {
        let status = match &s.error {
            None => "ok".to_string(),
            Some(e) => format!("err: {e}"),
        };
        println!(
            "{agent:<agent_w$}  {peer:<peer_w$}  {cap:<cap_w$}  {lat:>8}  {status}",
            agent = s.agent,
            peer = s.peer,
            cap = s.capability,
            lat = s.latency_ms,
        );
    }
}

// ── HTTP plumbing ─────────────────────────────────────────

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {body}").into());
    }
    Ok(body)
}

async fn http_post(
    url: &str,
    body: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let (status, text) = http_post_with_status(url, body).await?;
    if !status.is_success() {
        return Err(format!("bridge returned HTTP {status}: {text}").into());
    }
    Ok(text)
}

async fn http_post_with_status(
    url: &str,
    body: &serde_json::Value,
) -> Result<(reqwest::StatusCode, String), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    let resp = client
        .post(url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(body)?)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    Ok((status, text))
}

fn print_raw(body: &str) {
    print!("{body}");
    if !body.ends_with('\n') {
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_entry_deserializes_with_defaults() {
        let parsed: ListEntry = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.name, "");
        assert_eq!(parsed.version, 0);
    }

    #[test]
    fn execution_record_round_trips() {
        let json = r#"{
            "execution_id": "abc",
            "workflow_name": "demo",
            "input": "hi",
            "status": "success",
            "result": "out",
            "total_latency_ms": 42,
            "steps": [
                { "agent": "a", "peer": "p", "capability": "c", "latency_ms": 10, "error": null }
            ]
        }"#;
        let parsed: ExecutionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.execution_id, "abc");
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].agent, "a");
        assert!(parsed.steps[0].error.is_none());
    }

    #[test]
    fn execution_record_with_error_step() {
        let json = r#"{
            "execution_id": "abc",
            "workflow_name": "demo",
            "input": "x",
            "status": "failed",
            "result": "boom",
            "total_latency_ms": 1,
            "steps": [
                { "agent": "a", "peer": "p", "capability": "c", "latency_ms": 1, "error": "boom" }
            ]
        }"#;
        let parsed: ExecutionRecord = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.status, "failed");
        assert_eq!(parsed.steps[0].error.as_deref(), Some("boom"));
    }
}
