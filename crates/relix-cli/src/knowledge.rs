//! `relix-cli knowledge ...` — RELIX-7.16 operator surface.
//!
//! Five subcommands, each a thin HTTP forwarder onto the
//! `/v1/knowledge/*` bridge endpoints:
//!
//! - `knowledge groups` — print configured groups.
//! - `knowledge share --from X --to A,B --ids id1,id2 [--message M]`
//! - `knowledge broadcast --group G --caller X --ids id1,id2 [--message M]`
//! - `knowledge shared --agent X [--shared-by Y]`
//! - `knowledge revoke --ids id1,id2`
//!
//! Every subcommand accepts `--bridge <url>` (defaults to
//! `http://127.0.0.1:19791`) and the read-paths accept
//! `--raw` to dump the JSON body verbatim.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List configured sharing groups + members.
    Groups {
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Share specific observations from one agent to one or
    /// more targets.
    Share {
        #[arg(long)]
        from: String,
        /// Comma-separated list of target agent names.
        #[arg(long)]
        to: String,
        /// Comma-separated list of observation ids.
        #[arg(long)]
        ids: String,
        /// Optional human-readable note attached to the share.
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Broadcast specific observations to every OTHER member
    /// of the named group simultaneously.
    Broadcast {
        #[arg(long)]
        group: String,
        /// The calling agent — must be a member of the group.
        #[arg(long)]
        caller: String,
        /// Comma-separated list of observation ids.
        #[arg(long)]
        ids: String,
        #[arg(long)]
        message: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// List observations an agent has RECEIVED via knowledge
    /// sharing.
    Shared {
        #[arg(long)]
        agent: String,
        /// Optional filter — only rows shared by this agent.
        #[arg(long = "shared-by")]
        shared_by: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Revoke specific received observations (soft-delete on
    /// the receiver only).
    Revoke {
        /// Comma-separated list of observation ids.
        #[arg(long)]
        ids: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.16 GAP 2: recall SOURCE observations across
    /// every receiver they were shared with. The source
    /// observation itself is not deleted — only the copies on
    /// the receivers.
    Recall {
        /// The source agent — must match each observation's
        /// `source` column.
        #[arg(long)]
        from: String,
        /// Comma-separated list of SOURCE observation ids.
        #[arg(long)]
        ids: String,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Groups { bridge, raw } => groups(&bridge, raw).await,
        Cmd::Share {
            from,
            to,
            ids,
            message,
            bridge,
            raw,
        } => {
            let targets = split_csv(&to);
            let observation_ids = split_csv(&ids);
            share(
                &bridge,
                &from,
                &targets,
                &observation_ids,
                message.as_deref(),
                raw,
            )
            .await
        }
        Cmd::Broadcast {
            group,
            caller,
            ids,
            message,
            bridge,
            raw,
        } => {
            let observation_ids = split_csv(&ids);
            broadcast(
                &bridge,
                &caller,
                &group,
                &observation_ids,
                message.as_deref(),
                raw,
            )
            .await
        }
        Cmd::Shared {
            agent,
            shared_by,
            bridge,
            raw,
        } => shared(&bridge, &agent, shared_by.as_deref(), raw).await,
        Cmd::Revoke { ids, bridge, raw } => {
            let observation_ids = split_csv(&ids);
            revoke(&bridge, &observation_ids, raw).await
        }
        Cmd::Recall {
            from,
            ids,
            bridge,
            raw,
        } => {
            let observation_ids = split_csv(&ids);
            recall(&bridge, &from, &observation_ids, raw).await
        }
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

async fn groups(bridge: &str, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/knowledge/groups", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let groups: Vec<GroupView> =
        serde_json::from_str(&body).map_err(|e| format!("decode groups: {e} (body={body})"))?;
    if groups.is_empty() {
        println!("(no sharing groups configured)");
        return Ok(());
    }
    for g in groups {
        let layers = if g.auto_share_layers.is_empty() {
            "(none)".to_string()
        } else {
            g.auto_share_layers.join(", ")
        };
        let floor = g
            .min_quality_score
            .map(|f| format!("{f:.2}"))
            .unwrap_or_else(|| "(none)".into());
        println!(
            "{name}\n  members:           {members}\n  auto_share_layers: {layers}\n  min_quality:       {floor}",
            name = g.name,
            members = g.members.join(", "),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn share(
    bridge: &str,
    from: &str,
    targets: &[String],
    ids: &[String],
    message: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if targets.is_empty() {
        return Err("--to must list at least one agent".into());
    }
    if ids.is_empty() {
        return Err("--ids must list at least one observation id".into());
    }
    let mut body = serde_json::Map::new();
    body.insert("source_agent".into(), Value::from(from));
    body.insert("target_agents".into(), Value::from(targets.to_vec()));
    body.insert("observation_ids".into(), Value::from(ids.to_vec()));
    if let Some(m) = message {
        body.insert("message".into(), Value::from(m));
    }
    let url = format!("{}/v1/knowledge/share", bridge.trim_end_matches('/'));
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("decode: {e}"))?;
    let shared = v.get("shared_count").and_then(Value::as_u64).unwrap_or(0);
    let rejected = v
        .get("rejection_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("shared:   {shared}");
    println!("rejected: {rejected}");
    if rejected > 0
        && let Some(rejs) = v.get("rejections").and_then(Value::as_array)
    {
        for r in rejs {
            let id = r
                .get("observation_id")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let target = r.get("target_agent").and_then(Value::as_str).unwrap_or("?");
            let reason = r
                .get("reason")
                .and_then(|x| x.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("?");
            println!("  ✗ {id} → {target} ({reason})");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn broadcast(
    bridge: &str,
    caller: &str,
    group: &str,
    ids: &[String],
    message: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if ids.is_empty() {
        return Err("--ids must list at least one observation id".into());
    }
    let mut body = serde_json::Map::new();
    body.insert("caller_agent".into(), Value::from(caller));
    body.insert("group".into(), Value::from(group));
    body.insert("observation_ids".into(), Value::from(ids.to_vec()));
    if let Some(m) = message {
        body.insert("message".into(), Value::from(m));
    }
    let url = format!("{}/v1/knowledge/broadcast", bridge.trim_end_matches('/'));
    let resp = http_post_json(&url, &Value::Object(body)).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("decode: {e}"))?;
    let group_name = v.get("group").and_then(Value::as_str).unwrap_or("?");
    println!("group: {group_name}");
    if let Some(rows) = v.get("per_target").and_then(Value::as_array) {
        for row in rows {
            if let Some(arr) = row.as_array()
                && arr.len() == 2
            {
                let target = arr[0].as_str().unwrap_or("?");
                let shared = arr[1]
                    .get("shared_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let rejected = arr[1]
                    .get("rejection_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                println!("  {target}: shared={shared} rejected={rejected}");
            }
        }
    }
    Ok(())
}

async fn shared(
    bridge: &str,
    agent: &str,
    shared_by: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut query = String::new();
    if let Some(s) = shared_by {
        query.push_str("?shared_by=");
        query.push_str(&urlencode(s));
    }
    let url = format!(
        "{}/v1/knowledge/shared/{agent}{query}",
        bridge.trim_end_matches('/'),
        agent = urlencode(agent),
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let rows: Vec<SharedRow> = serde_json::from_str(&body)
        .map_err(|e| format!("decode shared rows: {e} (body={body})"))?;
    if rows.is_empty() {
        println!("(no received observations for agent {agent:?})");
        return Ok(());
    }
    let id_w = rows.iter().map(|r| r.id.len()).max().unwrap_or(8).max(8);
    let from_w = rows
        .iter()
        .map(|r| r.shared_by.len())
        .max()
        .unwrap_or(6)
        .max(6);
    let id_header = "id".to_string();
    let from_header = "from".to_string();
    let revoked_header = "rvkd".to_string();
    let text_header = "text".to_string();
    println!(
        "{id_header:<idw$}  {from_header:<fw$}  {revoked_header}  {text_header}",
        idw = id_w,
        fw = from_w,
    );
    for r in rows {
        let preview: String = r.text.chars().take(64).collect();
        println!(
            "{id:<idw$}  {from:<fw$}  {revoked:<4}  {preview}",
            id = r.id,
            from = r.shared_by,
            revoked = if r.revoked { "yes" } else { "" },
            idw = id_w,
            fw = from_w,
        );
    }
    Ok(())
}

async fn revoke(bridge: &str, ids: &[String], raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    if ids.is_empty() {
        return Err("--ids must list at least one observation id".into());
    }
    let url = format!("{}/v1/knowledge/revoke", bridge.trim_end_matches('/'));
    let body = serde_json::json!({ "observation_ids": ids });
    let resp = http_post_json(&url, &body).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("decode: {e}"))?;
    let revoked = v.get("revoked_count").and_then(Value::as_u64).unwrap_or(0);
    let missing = v
        .get("missing_ids")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    println!("revoked: {revoked}");
    if missing > 0 {
        println!("missing: {missing}");
        if let Some(arr) = v.get("missing_ids").and_then(Value::as_array) {
            for m in arr {
                if let Some(s) = m.as_str() {
                    println!("  ? {s}");
                }
            }
        }
    }
    Ok(())
}

async fn recall(
    bridge: &str,
    from: &str,
    ids: &[String],
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if from.trim().is_empty() {
        return Err("--from is required".into());
    }
    if ids.is_empty() {
        return Err("--ids must list at least one source observation id".into());
    }
    let url = format!("{}/v1/knowledge/recall", bridge.trim_end_matches('/'));
    let body = serde_json::json!({
        "source_agent": from,
        "source_observation_ids": ids,
    });
    let resp = http_post_json(&url, &body).await?;
    if raw {
        println!("{resp}");
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp).map_err(|e| format!("decode: {e}"))?;
    let processed = v
        .get("source_ids_processed")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = v
        .get("total_copies_revoked")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    println!("source_ids_processed:  {processed}");
    println!("total_copies_revoked:  {total}");
    if let Some(targets) = v.get("per_target").and_then(Value::as_array)
        && !targets.is_empty()
    {
        println!("per_target:");
        for t in targets {
            let agent = t.get("target_agent").and_then(Value::as_str).unwrap_or("?");
            let revoked = t.get("copies_revoked").and_then(Value::as_u64).unwrap_or(0);
            println!("  {agent}: {revoked}");
        }
    }
    if let Some(missing) = v.get("missing_source_ids").and_then(Value::as_array)
        && !missing.is_empty()
    {
        println!("missing_source_ids:");
        for id in missing {
            if let Some(s) = id.as_str() {
                println!("  ? {s}");
            }
        }
    }
    if let Some(unauth) = v.get("unauthorised_source_ids").and_then(Value::as_array)
        && !unauth.is_empty()
    {
        println!("unauthorised_source_ids (caller is not the source agent):");
        for id in unauth {
            if let Some(s) = id.as_str() {
                println!("  ✗ {s}");
            }
        }
    }
    Ok(())
}

// ── shared types ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GroupView {
    name: String,
    #[serde(default)]
    members: Vec<String>,
    #[serde(default)]
    auto_share_layers: Vec<String>,
    #[serde(default)]
    min_quality_score: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct SharedRow {
    id: String,
    text: String,
    shared_by: String,
    #[serde(default)]
    revoked: bool,
}

// ── http ────────────────────────────────────────────────

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

/// Minimal percent-encoder for path components.
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
    fn split_csv_drops_empty_and_trims_whitespace() {
        assert_eq!(split_csv("a, b ,c"), vec!["a", "b", "c"]);
        assert_eq!(split_csv("  ,,"), Vec::<String>::new());
        assert!(split_csv("").is_empty());
    }

    #[test]
    fn urlencode_round_trips_simple_strings() {
        assert_eq!(urlencode("alice"), "alice");
        assert_eq!(urlencode("alice@example.com"), "alice%40example.com");
        assert_eq!(urlencode("a b/c"), "a%20b%2Fc");
    }
}
