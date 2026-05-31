//! `relix-cli training ...` — RELIX-7.15 operator surface.
//!
//! Five subcommands, each an HTTP forwarder onto the
//! `/v1/training/*` bridge endpoints:
//!
//! - `training stats` — aggregate counters + score histogram.
//! - `training list [--agent X] [--min-quality 0.7] [--limit 20]`
//!   — recent interactions with quality scores.
//! - `training show <interaction-id>` — full record.
//! - `training export --format openai --output ./exports --set-name <name>
//!   [--min-quality 0.7] [--agent X]` — runs an export.
//! - `training delete <interaction-id>` — hard delete.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print aggregate stats: total / exported / avg quality /
    /// score distribution / top agents / top models.
    Stats {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// List recent interactions with quality scores. Sorted
    /// newest-first.
    List {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "min-quality", default_value_t = 0.0)]
        min_quality: f32,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Print one interaction's full record (system prompt /
    /// user message / response / tool calls).
    Show {
        interaction_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Run an export. `--format` is one of
    /// `openai / anthropic / generic / raw_json`.
    Export {
        #[arg(long)]
        format: String,
        #[arg(long = "set-name")]
        set_name: String,
        #[arg(long)]
        output: Option<String>,
        #[arg(long = "min-quality", default_value_t = 0.7)]
        min_quality: f32,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        max: Option<u32>,
        #[arg(long = "no-tool-calls", default_value_t = false)]
        no_tool_calls: bool,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Hard-delete one interaction.
    Delete {
        interaction_id: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Stats { bridge, raw } => stats(&bridge, raw).await,
        Cmd::List {
            agent,
            min_quality,
            limit,
            session_id,
            bridge,
            raw,
        } => {
            list(
                &bridge,
                agent.as_deref(),
                min_quality,
                limit,
                session_id.as_deref(),
                raw,
            )
            .await
        }
        Cmd::Show {
            interaction_id,
            bridge,
            raw,
        } => show(&bridge, &interaction_id, raw).await,
        Cmd::Export {
            format,
            set_name,
            output,
            min_quality,
            agent,
            session_id,
            max,
            no_tool_calls,
            bridge,
            raw,
        } => {
            export(
                &bridge,
                &format,
                &set_name,
                output.as_deref(),
                min_quality,
                agent.as_deref(),
                session_id.as_deref(),
                max,
                !no_tool_calls,
                raw,
            )
            .await
        }
        Cmd::Delete {
            interaction_id,
            bridge,
        } => delete(&bridge, &interaction_id).await,
    }
}

async fn stats(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/training/stats", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let s: Stats =
        serde_json::from_str(&body).map_err(|e| format!("decode stats: {e} (body={body})"))?;
    println!("total:                {total}", total = s.total);
    println!(
        "exported:             {exp} ({pct:.1}%)",
        exp = s.exported,
        pct = if s.total > 0 {
            (s.exported as f64) * 100.0 / s.total as f64
        } else {
            0.0
        }
    );
    if let Some(avg) = s.average_quality_score {
        println!("average quality:      {avg:.3}");
    } else {
        println!("average quality:      (no scored rows yet)");
    }
    if let Some(dist) = s.score_distribution.as_ref() {
        let max = dist.buckets.iter().copied().max().unwrap_or(0).max(1);
        println!("score distribution:");
        for (i, count) in dist.buckets.iter().enumerate() {
            let lo = (i as f64) / 10.0;
            let hi = lo + 0.1;
            let bar_w = ((*count as f64 / max as f64) * 30.0).round() as usize;
            let bar = "█".repeat(bar_w);
            println!("  {lo:.1}–{hi:.1}  {count:>5}  {bar}");
        }
        println!("  unscored {n:>9}", n = dist.unscored);
    }
    if !s.by_agent.is_empty() {
        println!("top agents:");
        for g in s.by_agent.iter().take(10) {
            println!("  {label:<24}  {count}", label = g.label, count = g.count);
        }
    }
    if !s.by_model.is_empty() {
        println!("top models:");
        for g in s.by_model.iter().take(10) {
            println!("  {label:<24}  {count}", label = g.label, count = g.count);
        }
    }
    Ok(())
}

async fn list(
    bridge: &str,
    agent: Option<&str>,
    min_quality: f32,
    limit: u32,
    session_id: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut query = format!("page=1&page_size={limit}&min_quality_score={min_quality}");
    if let Some(a) = agent {
        query.push_str("&agent=");
        query.push_str(&urlencode(a));
    }
    if let Some(s) = session_id {
        query.push_str("&session_id=");
        query.push_str(&urlencode(s));
    }
    let url = format!(
        "{}/v1/training/interactions?{query}",
        bridge.trim_end_matches('/')
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<Summary> =
        serde_json::from_str(&body).map_err(|e| format!("decode list: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no interactions match these filters)");
        return Ok(());
    }
    let agent_w = rows.iter().map(|r| r.agent.len()).max().unwrap_or(5).max(5);
    let model_w = rows.iter().map(|r| r.model.len()).max().unwrap_or(5).max(5);
    println!(
        "{id:<14}  {agent:<aw$}  {model:<mw$}  {q:>5}  {tok:>6}  {lat:>6}  {ok:>3}  {preview}",
        id = "id",
        agent = "agent",
        model = "model",
        q = "qual",
        tok = "tokens",
        lat = "lat-ms",
        ok = "ok",
        preview = String::from("user"),
        aw = agent_w,
        mw = model_w,
    );
    for r in rows {
        let q = match r.quality_score {
            Some(v) => format!("{v:.2}"),
            None => "  -".into(),
        };
        let id_short: String = r.interaction_id.chars().take(12).collect();
        let ok = if r.success { "OK" } else { "ERR" };
        let tokens = r
            .token_count
            .map(|t| t.to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "{id:<14}  {agent:<aw$}  {model:<mw$}  {q:>5}  {tok:>6}  {lat:>6}  {ok:>3}  {preview}",
            id = id_short,
            agent = r.agent,
            model = r.model,
            q = q,
            tok = tokens,
            lat = r.latency_ms,
            ok = ok,
            preview = r.user_preview,
            aw = agent_w,
            mw = model_w,
        );
    }
    Ok(())
}

async fn show(
    bridge: &str,
    interaction_id: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/training/interactions/{id}",
        bridge.trim_end_matches('/'),
        id = urlencode(interaction_id),
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("decode interaction: {e} (body={body})"))?;
    let id = v
        .get("interaction_id")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let agent = v.get("agent").and_then(Value::as_str).unwrap_or("?");
    let session = v.get("session_id").and_then(Value::as_str).unwrap_or("?");
    let model = v.get("model").and_then(Value::as_str).unwrap_or("?");
    let provider = v.get("provider").and_then(Value::as_str).unwrap_or("?");
    let qual = v.get("quality_score").and_then(Value::as_f64);
    let tokens = v.get("token_count").and_then(Value::as_u64);
    let latency = v.get("latency_ms").and_then(Value::as_u64).unwrap_or(0);
    let success = v.get("success").and_then(Value::as_bool).unwrap_or(false);
    println!("interaction_id:  {id}");
    println!("agent:           {agent}");
    println!("session_id:      {session}");
    println!("model:           {model}");
    println!("provider:        {provider}");
    println!("success:         {success}");
    println!("latency_ms:      {latency}");
    if let Some(t) = tokens {
        println!("token_count:     {t}");
    }
    if let Some(q) = qual {
        println!("quality_score:   {q:.3}");
    } else {
        println!("quality_score:   (unscored)");
    }
    if let Some(s) = v.get("system_prompt").and_then(Value::as_str)
        && !s.is_empty()
    {
        println!("\n── system prompt ──");
        println!("{s}");
    }
    if let Some(u) = v.get("user_message").and_then(Value::as_str) {
        println!("\n── user message ──");
        println!("{u}");
    }
    if let Some(r) = v.get("response").and_then(Value::as_str) {
        println!("\n── assistant response ──");
        println!("{r}");
    }
    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array)
        && !calls.is_empty()
    {
        println!("\n── tool calls ──");
        for c in calls {
            let tool = c.get("tool").and_then(Value::as_str).unwrap_or("?");
            let ok = c.get("success").and_then(Value::as_bool).unwrap_or(false);
            let lat = c.get("latency_ms").and_then(Value::as_u64).unwrap_or(0);
            let ok_label = if ok { "ok" } else { "ERR" };
            println!("  - {tool}  {ok_label}  {lat}ms");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn export(
    bridge: &str,
    format: &str,
    set_name: &str,
    output: Option<&str>,
    min_quality: f32,
    agent: Option<&str>,
    session_id: Option<&str>,
    max: Option<u32>,
    include_tool_calls: bool,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut req = serde_json::Map::new();
    req.insert("format".into(), Value::from(format));
    req.insert("export_set".into(), Value::from(set_name));
    if let Some(o) = output {
        req.insert("output_dir".into(), Value::from(o));
    }
    req.insert("min_quality_score".into(), Value::from(min_quality));
    if let Some(a) = agent {
        req.insert("agent".into(), Value::from(a));
    }
    if let Some(s) = session_id {
        req.insert("session_id".into(), Value::from(s));
    }
    if let Some(m) = max {
        req.insert("max_interactions".into(), Value::from(m));
    }
    req.insert("include_tool_calls".into(), Value::from(include_tool_calls));
    let url = format!("{}/v1/training/export", bridge.trim_end_matches('/'));
    let body = http_post_json(&url, &Value::Object(req)).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode export: {e} (body={body})"))?;
    let matched = v.get("matched_count").and_then(Value::as_u64).unwrap_or(0);
    let exported = v.get("exported_count").and_then(Value::as_u64).unwrap_or(0);
    let path = v.get("output_path").and_then(Value::as_str);
    let tokens = v.get("total_tokens").and_then(Value::as_u64).unwrap_or(0);
    println!("matched:   {matched}");
    println!("exported:  {exported}");
    println!("tokens:    {tokens}");
    match path {
        Some(p) => println!("output:    {p}"),
        None => println!("output:    (no matching interactions — no file created)"),
    }
    Ok(())
}

async fn delete(bridge: &str, interaction_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/training/interactions/{id}",
        bridge.trim_end_matches('/'),
        id = urlencode(interaction_id),
    );
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?
        .delete(&url)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    println!("deleted: {interaction_id}");
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Stats {
    #[serde(default)]
    total: u64,
    #[serde(default)]
    exported: u64,
    #[serde(default)]
    average_quality_score: Option<f64>,
    #[serde(default)]
    score_distribution: Option<ScoreDistribution>,
    #[serde(default)]
    by_agent: Vec<GroupedCount>,
    #[serde(default)]
    by_model: Vec<GroupedCount>,
}

#[derive(Debug, Deserialize)]
struct ScoreDistribution {
    #[serde(default)]
    buckets: [u64; 10],
    #[serde(default)]
    unscored: u64,
}

#[derive(Debug, Deserialize)]
struct GroupedCount {
    #[serde(default)]
    label: String,
    #[serde(default)]
    count: u64,
}

#[derive(Debug, Deserialize)]
struct Summary {
    #[serde(default)]
    interaction_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    latency_ms: u64,
    #[serde(default)]
    success: bool,
    #[serde(default)]
    token_count: Option<u64>,
    #[serde(default)]
    quality_score: Option<f64>,
    #[serde(default)]
    user_preview: String,
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
        .timeout(Duration::from_secs(60))
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

/// Minimal percent-encoder for path components. We only need
/// to handle the small set of ASCII characters that can show up
/// in interaction ids or filter values (alphanumerics + hex
/// digits + a few common punctuation marks). Anything outside
/// that set gets a `%XX` escape. This avoids pulling
/// `percent-encoding` into the CLI's dependency tree for one
/// helper.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{:02X}", b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_through_alphanumerics() {
        assert_eq!(urlencode("abc123"), "abc123");
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencode_escapes_spaces_and_slashes() {
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
        assert_eq!(urlencode("alice@example.com"), "alice%40example.com");
    }
}
