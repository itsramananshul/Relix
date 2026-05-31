//! GAP 11 + 12 — `relix execution` CLI subcommands.
//!
//! Talks to the bridge's `/v1/execution/*` HTTP surface.

use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// GAP 11: roll back every action recorded under one
    /// transaction id. Tier A actions run their declared
    /// compensating call; Tier B actions surface their plan
    /// for operator review.
    Rollback(RollbackArgs),
    /// GAP 11: print the full transaction record (every action
    /// with tier classification + idempotency key + dry-run
    /// flag).
    Transaction(TransactionArgs),
    /// GAP 12: print evidence records. Optionally filtered by
    /// action_id or actor.
    Evidence(EvidenceArgs),
}

#[derive(Args, Debug)]
pub struct RollbackArgs {
    /// Transaction id to roll back.
    pub transaction_id: String,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TransactionArgs {
    /// Transaction id to inspect.
    pub transaction_id: String,
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct EvidenceArgs {
    #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
    pub bridge: String,
    #[arg(long = "action")]
    pub action_id: Option<String>,
    #[arg(long = "actor")]
    pub actor_id: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Rollback(a) => rollback(a).await,
        Cmd::Transaction(a) => transaction(a).await,
        Cmd::Evidence(a) => evidence(a).await,
    }
}

async fn rollback(args: RollbackArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/execution/rollback");
    let body = serde_json::json!({ "transaction_id": args.transaction_id });
    let r = reqwest::Client::new().post(&url).json(&body).send().await?;
    let status = r.status();
    let resp_body = r.text().await?;
    if !status.is_success() {
        eprintln!("error: HTTP {status}: {resp_body}");
        std::process::exit(1);
    }
    if args.json {
        println!("{resp_body}");
        return Ok(());
    }
    let v: serde_json::Value = serde_json::from_str(&resp_body)?;
    let auto = v
        .get("auto_rolled_back")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let human = v
        .get("human_review_required")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let errors = v
        .get("errors")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    println!("transaction_id  {}", args.transaction_id);
    println!();
    if auto.is_empty() && human.is_empty() && errors.is_empty() {
        println!("(nothing to roll back)");
        return Ok(());
    }
    if !auto.is_empty() {
        println!("auto-rolled-back ({}):", auto.len());
        for a in &auto {
            let ok = a.get("success").and_then(|x| x.as_bool()).unwrap_or(false);
            let tool = a
                .get("original_tool")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let comp = a
                .get("compensating_tool")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let marker = if ok { "OK  " } else { "FAIL" };
            println!("  [{marker}] {tool} via {comp}");
            if !ok && let Some(err) = a.get("error").and_then(|x| x.as_str()) {
                println!("        error: {err}");
            }
        }
        println!();
    }
    if !human.is_empty() {
        println!("human-review-required ({}):", human.len());
        for h in &human {
            let tool = h.get("tool").and_then(|x| x.as_str()).unwrap_or("");
            let plan = h
                .get("rollback_plan")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            println!("  {tool}");
            println!("      plan: {plan}");
        }
        println!();
    }
    if !errors.is_empty() {
        println!("errors ({}):", errors.len());
        for e in &errors {
            if let Some(s) = e.as_str() {
                println!("  {s}");
            }
        }
    }
    Ok(())
}

async fn transaction(args: TransactionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!(
        "{base}/v1/execution/transactions/{}",
        urlencode(&args.transaction_id)
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
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}

async fn evidence(args: EvidenceArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let mut url = format!("{base}/v1/execution/evidence?limit={}", args.limit);
    if let Some(a) = args.action_id.as_ref() {
        url.push_str("&action_id=");
        url.push_str(&urlencode(a));
    }
    if let Some(a) = args.actor_id.as_ref() {
        url.push_str("&actor_id=");
        url.push_str(&urlencode(a));
    }
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
