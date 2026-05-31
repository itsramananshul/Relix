//! `relix routing ...` — RELIX-7.29 PART 1 operator surface.
//!
//! One subcommand: `routing explain --message "<text>"
//! [--session-turns N]` — classifies the message with the
//! §7.29 ComplexityClassifier, asks the coordinator to resolve
//! the tier, and prints the score + decision.
//!
//! Wire shape mirrors `relix confidence ...`:
//! - `--bridge <url>` defaults to `http://127.0.0.1:19791`.
//! - `--raw` prints the JSON body verbatim instead of the
//!   human-formatted summary.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Dry-run the smart router. Classifies the message and
    /// prints which tier + provider + model the coordinator
    /// would dispatch on.
    Explain {
        /// The user message to classify.
        #[arg(long)]
        message: String,
        /// Optional session turn count (raises the tier when
        /// > 5 per the §7.29 signal table). Defaults to 0.
        #[arg(long, default_value_t = 0)]
        session_turns: u32,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Explain {
            message,
            session_turns,
            bridge,
            raw,
        } => explain(&bridge, &message, session_turns, raw).await,
    }
}

async fn explain(
    bridge: &str,
    message: &str,
    session_turns: u32,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if message.trim().is_empty() {
        return Err("--message is required".into());
    }
    let url = format!("{}/v1/routing/explain", bridge.trim_end_matches('/'));
    let payload = serde_json::json!({
        "message": message,
        "session_turns": session_turns,
    });
    let body = http_post_json(&url, &payload).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let view: ExplainView =
        serde_json::from_str(&body).map_err(|e| format!("decode explain: {e} (body={body})"))?;
    println!("tier:           {}", view.score.tier);
    println!("score:          {}", view.score.score);
    if view.score.signals_triggered.is_empty() {
        println!("signals:        (none)");
    } else {
        println!(
            "signals:        {}",
            view.score.signals_triggered.join(", ")
        );
    }
    println!("routing on?:    {}", view.routing_enabled);
    println!("decision tier:  {}", view.decision.tier);
    println!(
        "provider:       {}",
        view.decision.provider.as_deref().unwrap_or("(default)")
    );
    println!(
        "model:          {}",
        view.decision.model.as_deref().unwrap_or("(default)")
    );
    println!("fell back?:     {}", view.decision.fell_back);
    println!("reasoning:      {}", view.decision.reasoning);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ExplainView {
    score: ScoreView,
    decision: DecisionView,
    routing_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct ScoreView {
    tier: String,
    score: u32,
    #[serde(default)]
    signals_triggered: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DecisionView {
    tier: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    fell_back: bool,
    reasoning: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_view_decodes_full_shape() {
        let raw = r#"{
            "score": { "tier": "complex", "score": 5, "signals_triggered": ["length>200w", "explicit_marker"] },
            "decision": {
                "tier": "complex",
                "provider": "ollama-big",
                "model": "llama3.1:70b",
                "fell_back": false,
                "reasoning": "routed complex -> llama3.1:70b on ollama-big"
            },
            "routing_enabled": true
        }"#;
        let v: ExplainView = serde_json::from_str(raw).unwrap();
        assert_eq!(v.score.tier, "complex");
        assert_eq!(v.score.score, 5);
        assert_eq!(v.decision.provider.as_deref(), Some("ollama-big"));
        assert!(v.routing_enabled);
    }

    #[test]
    fn explain_view_decodes_unrouted_shape() {
        let raw = r#"{
            "score": { "tier": "simple", "score": 0, "signals_triggered": [] },
            "decision": {
                "tier": "simple",
                "provider": null,
                "model": null,
                "fell_back": false,
                "reasoning": "ai.routing disabled"
            },
            "routing_enabled": false
        }"#;
        let v: ExplainView = serde_json::from_str(raw).unwrap();
        assert_eq!(v.score.tier, "simple");
        assert_eq!(v.decision.provider, None);
        assert!(!v.routing_enabled);
    }
}
