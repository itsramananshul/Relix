//! GAP 16 partial — `relix models` CLI surface.
//!
//! Two subcommands, both calling existing bridge endpoints:
//!
//! - `relix models list` — table of configured providers with
//!   their default model, enabled flag, configured-status, and
//!   last-test outcome (when present). Hits
//!   `GET /v1/config/providers`.
//! - `relix models fetch` — live model catalogue for the active
//!   provider via `GET /v1/models`.
//!
//! `relix models health` was removed in PART 6 because the bridge
//! endpoint it dispatched against (`/v1/providers/health`) was
//! deleted in the prior security session (PART 4 of the bridge
//! security pass — provider key handling was removed from the
//! bridge entirely). The aggregate counters that used to live on
//! that endpoint now flow through `/v1/observability/*` instead.

use clap::{Args, Subcommand};

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every provider configured on the bridge with its
    /// default model + enabled flag. Hits
    /// `GET /v1/config/providers`.
    List(ListArgs),
    /// GAP 16 §7.29 Model Name Resolution — fetch the bridge's
    /// active AI provider's live model catalogue. Hits
    /// `GET /v1/models`. Operators use this to discover real
    /// model IDs before configuring `[reasoning.router.tiers]`.
    Fetch(FetchArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::List(a) => list(a).await,
        Cmd::Fetch(a) => fetch(a).await,
    }
}

async fn fetch(args: FetchArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/models");
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
    let empty: Vec<serde_json::Value> = Vec::new();
    let models = v.get("data").and_then(|x| x.as_array()).unwrap_or(&empty);
    if models.is_empty() {
        println!("(provider returned no models; configure model IDs manually)");
        return Ok(());
    }
    println!("{:<48}  CTX_WINDOW", "MODEL_ID");
    for m in models {
        let id = m.get("id").and_then(|x| x.as_str()).unwrap_or("?");
        let ctx = m
            .get("context_length")
            .and_then(|x| x.as_u64())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".to_string());
        println!("{id:<48}  {ctx}");
    }
    Ok(())
}

async fn list(args: ListArgs) -> Result<(), Box<dyn std::error::Error>> {
    let base = args.bridge.trim_end_matches('/');
    let url = format!("{base}/v1/config/providers");
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
    let empty: Vec<serde_json::Value> = Vec::new();
    let providers = v
        .get("providers")
        .and_then(|x| x.as_array())
        .unwrap_or(&empty);
    print_provider_table(providers);
    Ok(())
}

pub(crate) fn print_provider_table(providers: &[serde_json::Value]) {
    if providers.is_empty() {
        println!("(no providers configured on this bridge)");
        return;
    }
    println!(
        "{:<14}  {:<6}  {:<8}  {:<7}  {:<32}  LAST TEST",
        "PROVIDER", "CONFIG", "DEFAULT", "ENABLED", "MODEL"
    );
    for p in providers {
        let name = p.get("name").and_then(|x| x.as_str()).unwrap_or("?");
        let configured = p
            .get("configured")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let is_default = p
            .get("is_default")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let enabled = p.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false);
        let model = p
            .get("default_model")
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let test_ok = p.get("last_test_ok").and_then(|x| x.as_bool());
        let test_at = p.get("last_test_at").and_then(|x| x.as_i64());
        let last_test = match (test_at, test_ok) {
            (Some(_), Some(true)) => "ok".to_string(),
            (Some(_), Some(false)) => "FAIL".to_string(),
            _ => "—".to_string(),
        };
        println!(
            "{:<14}  {:<6}  {:<8}  {:<7}  {:<32}  {}",
            name,
            yes_no(configured),
            yes_no(is_default),
            yes_no(enabled),
            truncate(model, 32),
            last_test,
        );
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_no_renders_both_branches() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn truncate_passes_short_strings_through_unchanged() {
        assert_eq!(truncate("short", 10), "short");
    }

    #[test]
    fn truncate_caps_long_strings_with_ellipsis() {
        let s = truncate("abcdefghijklmnop", 6);
        assert_eq!(s.chars().count(), 6);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn print_provider_table_handles_empty_input() {
        // Smoke: ensure no panic on the empty path. We cannot
        // assert on stdout from a unit test without capturing
        // it, but exercising the path catches obvious panics
        // (indexing, etc.).
        print_provider_table(&[]);
    }
}
