//! `relix observe` — live operator dashboard for the RELIX-7.28 Part 2
//! observability surface.
//!
//! Subcommands:
//!
//! - default — live dashboard that refreshes every 5 seconds; press `q`
//!   to quit. Renders active alerts + per-agent health scores + the
//!   deployment roll-up.
//! - `--once` — print a single snapshot and exit (useful for CI / scripts).
//! - `--alerts` — render only the active-alerts list.
//! - `--health` — render only the health summary.

use std::io::{Write, stdout};
use std::time::Duration;

use clap::Parser;
use crossterm::{
    cursor, execute,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use serde::Deserialize;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Parser, Debug)]
pub struct ObserveArgs {
    /// Print a single snapshot and exit (no terminal raw mode).
    #[arg(long)]
    pub once: bool,
    /// Render only the active-alerts panel.
    #[arg(long)]
    pub alerts: bool,
    /// Render only the health summary panel.
    #[arg(long)]
    pub health: bool,
    /// Bridge URL (default `http://127.0.0.1:19791`).
    #[arg(long, default_value = DEFAULT_BRIDGE)]
    pub bridge: String,
    /// Refresh interval in seconds for the live dashboard. Default 5.
    #[arg(long, default_value_t = 5)]
    pub refresh_secs: u64,
}

pub async fn run(args: ObserveArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.alerts && args.health {
        return Err("--alerts and --health are mutually exclusive".into());
    }
    if args.once || args.alerts || args.health {
        return one_shot(&args).await;
    }
    live_dashboard(&args).await
}

async fn one_shot(args: &ObserveArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.alerts {
        print_alerts_panel(&args.bridge).await?;
    } else if args.health {
        print_health_panel(&args.bridge).await?;
    } else {
        print_full_snapshot(&args.bridge).await?;
    }
    Ok(())
}

async fn live_dashboard(args: &ObserveArgs) -> Result<(), Box<dyn std::error::Error>> {
    terminal::enable_raw_mode()?;
    let result = run_event_loop(args).await;
    terminal::disable_raw_mode()?;
    let mut out = stdout();
    execute!(out, cursor::Show)?;
    result
}

async fn run_event_loop(args: &ObserveArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    execute!(
        out,
        cursor::Hide,
        Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    let interval = Duration::from_secs(args.refresh_secs.max(1));
    let mut ticker = tokio::time::interval(interval);
    // Skip the immediate tick — we render once below and then on
    // every subsequent tick.
    ticker.tick().await;
    render_dashboard(&args.bridge).await?;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                render_dashboard(&args.bridge).await?;
            }
            res = wait_for_quit() => {
                if res.is_ok() {
                    break;
                }
            }
        }
    }
    Ok(())
}

async fn wait_for_quit() -> Result<(), std::io::Error> {
    use crossterm::event::{Event, KeyCode, KeyEvent};
    loop {
        // Yield to the runtime; poll the input non-blockingly.
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !crossterm::event::poll(Duration::from_millis(0))? {
            continue;
        }
        if let Event::Key(KeyEvent {
            code: KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc,
            ..
        }) = crossterm::event::read()?
        {
            return Ok(());
        }
    }
}

async fn render_dashboard(bridge: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = stdout();
    execute!(out, Clear(ClearType::All), cursor::MoveTo(0, 0))?;
    execute!(
        out,
        SetForegroundColor(Color::Cyan),
        Print("Relix Observability — live dashboard (press q to quit)\r\n"),
        ResetColor,
    )?;
    execute!(out, Print("\r\n"))?;
    render_alerts_panel(&mut out, bridge).await?;
    execute!(out, Print("\r\n"))?;
    render_health_panel(&mut out, bridge).await?;
    out.flush()?;
    Ok(())
}

async fn render_alerts_panel(
    out: &mut std::io::Stdout,
    bridge: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/observability/alerts", bridge.trim_end_matches('/'));
    execute!(
        out,
        SetForegroundColor(Color::Yellow),
        Print("Active alerts\r\n"),
        ResetColor,
    )?;
    let body = match http_get(&url).await {
        Ok(b) => b,
        Err(e) => {
            execute!(
                out,
                SetForegroundColor(Color::Red),
                Print(format!("  error: {e}\r\n")),
                ResetColor,
            )?;
            return Ok(());
        }
    };
    let alerts: Vec<ActiveAlert> = serde_json::from_str(&body)?;
    if alerts.is_empty() {
        execute!(out, Print("  (no active alerts)\r\n"))?;
        return Ok(());
    }
    for a in &alerts {
        let (color, badge) = badge_for(&a.severity);
        execute!(
            out,
            SetForegroundColor(color),
            Print(format!("  {badge} ")),
            ResetColor,
        )?;
        let agent = if a.agent.is_empty() {
            "(deployment)"
        } else {
            &a.agent
        };
        let method_label = a
            .method
            .as_deref()
            .map(|m| format!(" [{m}]"))
            .unwrap_or_default();
        execute!(
            out,
            Print(format!(
                "{agent}  {kind}{method_label}  threshold={th:.2}  actual={ac:.2}\r\n",
                kind = a.kind,
                th = a.threshold,
                ac = a.actual,
            ))
        )?;
        execute!(out, Print(format!("        {}\r\n", a.message)))?;
    }
    Ok(())
}

async fn render_health_panel(
    out: &mut std::io::Stdout,
    bridge: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/observability/health", bridge.trim_end_matches('/'));
    execute!(
        out,
        SetForegroundColor(Color::Yellow),
        Print("Per-agent health\r\n"),
        ResetColor,
    )?;
    let body = match http_get(&url).await {
        Ok(b) => b,
        Err(e) => {
            execute!(
                out,
                SetForegroundColor(Color::Red),
                Print(format!("  error: {e}\r\n")),
                ResetColor,
            )?;
            return Ok(());
        }
    };
    let summary: HealthSummary = serde_json::from_str(&body)?;
    if summary.agents.is_empty() {
        execute!(out, Print("  (no agents reporting in the window)\r\n"))?;
    } else {
        let agent_w = summary
            .agents
            .iter()
            .map(|r| r.agent.len())
            .max()
            .unwrap_or(5)
            .max(5);
        execute!(
            out,
            Print(format!(
                "  {agent:<aw$}  {sc:>5}  {st:<7}  {er:>6}  {p95:>6}  {al:>6}\r\n",
                agent = "agent",
                sc = "score",
                st = "status",
                er = "err%",
                p95 = "p95ms",
                al = "alerts",
                aw = agent_w,
            ))
        )?;
        for r in &summary.agents {
            let color = match r.status.as_str() {
                "green" => Color::Green,
                "yellow" => Color::Yellow,
                _ => Color::Red,
            };
            execute!(out, SetForegroundColor(color))?;
            execute!(
                out,
                Print(format!(
                    "  {agent:<aw$}  {sc:>5}  {st:<7}  {er:>5.2}%  {p95:>6}  {al:>6}\r\n",
                    agent = r.agent,
                    sc = r.score,
                    st = r.status,
                    er = r.error_rate_pct,
                    p95 = r.p95_latency_ms,
                    al = r.active_alerts,
                    aw = agent_w,
                ))
            )?;
            execute!(out, ResetColor)?;
        }
    }
    execute!(
        out,
        Print(format!(
            "\r\nDeployment: total_cost=${cost:.4} invocations={inv} \
             error_rate={err:.2}% active_alerts={al} avg_score={sc}\r\n",
            cost = summary.deployment.total_cost_usd,
            inv = summary.deployment.total_invocations,
            err = summary.deployment.overall_error_rate_pct,
            al = summary.deployment.active_alert_count,
            sc = summary.deployment.avg_health_score,
        ))
    )?;
    Ok(())
}

async fn print_alerts_panel(bridge: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/observability/alerts", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    let alerts: Vec<ActiveAlert> = serde_json::from_str(&body)?;
    if alerts.is_empty() {
        println!("(no active alerts)");
        return Ok(());
    }
    for a in alerts {
        let badge = match a.severity.as_str() {
            "critical" => "[!!]",
            "warning" => "[! ]",
            _ => "[? ]",
        };
        let agent = if a.agent.is_empty() {
            "(deployment)"
        } else {
            &a.agent
        };
        let method = a
            .method
            .as_deref()
            .map(|m| format!(" [{m}]"))
            .unwrap_or_default();
        println!(
            "{badge} {agent}  {kind}{method}  threshold={th:.2}  actual={ac:.2}",
            kind = a.kind,
            th = a.threshold,
            ac = a.actual
        );
        println!("       {}", a.message);
    }
    Ok(())
}

async fn print_health_panel(bridge: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/observability/health", bridge.trim_end_matches('/'));
    let body = http_get(&url).await?;
    let summary: HealthSummary = serde_json::from_str(&body)?;
    if summary.agents.is_empty() {
        println!("(no agents reporting in the window)");
        return Ok(());
    }
    let agent_w = summary
        .agents
        .iter()
        .map(|r| r.agent.len())
        .max()
        .unwrap_or(5)
        .max(5);
    println!(
        "{agent:<aw$}  {sc:>5}  {st:<7}  {er:>6}  {p95:>6}  {al:>6}",
        agent = "agent",
        sc = "score",
        st = "status",
        er = "err%",
        p95 = "p95ms",
        al = "alerts",
        aw = agent_w,
    );
    for r in &summary.agents {
        println!(
            "{agent:<aw$}  {sc:>5}  {st:<7}  {er:>5.2}%  {p95:>6}  {al:>6}",
            agent = r.agent,
            sc = r.score,
            st = r.status,
            er = r.error_rate_pct,
            p95 = r.p95_latency_ms,
            al = r.active_alerts,
            aw = agent_w,
        );
    }
    println!(
        "\nDeployment: total_cost=${cost:.4} invocations={inv} \
         error_rate={err:.2}% active_alerts={al} avg_score={sc}",
        cost = summary.deployment.total_cost_usd,
        inv = summary.deployment.total_invocations,
        err = summary.deployment.overall_error_rate_pct,
        al = summary.deployment.active_alert_count,
        sc = summary.deployment.avg_health_score,
    );
    Ok(())
}

async fn print_full_snapshot(bridge: &str) -> Result<(), Box<dyn std::error::Error>> {
    print_alerts_panel(bridge).await?;
    println!();
    print_health_panel(bridge).await
}

fn badge_for(severity: &str) -> (Color, &'static str) {
    match severity {
        "critical" => (Color::Red, "[!!]"),
        "warning" => (Color::Yellow, "[! ]"),
        _ => (Color::Grey, "[? ]"),
    }
}

#[derive(Debug, Deserialize)]
struct ActiveAlert {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    threshold: f64,
    #[serde(default)]
    actual: f64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    method: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthSummary {
    #[serde(default)]
    agents: Vec<AgentHealthRow>,
    #[serde(default)]
    deployment: DeploymentSummary,
}

#[derive(Debug, Default, Deserialize)]
struct AgentHealthRow {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    score: u32,
    #[serde(default)]
    status: String,
    #[serde(default)]
    error_rate_pct: f64,
    #[serde(default)]
    p95_latency_ms: u64,
    #[serde(default)]
    active_alerts: u64,
}

#[derive(Debug, Default, Deserialize)]
struct DeploymentSummary {
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    total_invocations: u64,
    #[serde(default)]
    overall_error_rate_pct: f64,
    #[serde(default)]
    active_alert_count: u64,
    #[serde(default)]
    avg_health_score: u32,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_match_documented_severities() {
        let (color, badge) = badge_for("critical");
        assert_eq!(badge, "[!!]");
        assert!(matches!(color, Color::Red));
        let (_, badge) = badge_for("warning");
        assert_eq!(badge, "[! ]");
        let (_, badge) = badge_for("something-else");
        assert_eq!(badge, "[? ]");
    }

    #[test]
    fn health_summary_deserialises_from_minimal_json() {
        let body = r#"{"agents":[],"deployment":{},"window_hours":24}"#;
        let summary: HealthSummary = serde_json::from_str(body).unwrap();
        assert!(summary.agents.is_empty());
        assert_eq!(summary.deployment.total_invocations, 0);
    }

    #[test]
    fn health_summary_deserialises_with_full_row() {
        let body = r#"{
            "agents":[{
                "agent":"alice",
                "score":85,
                "status":"green",
                "error_rate_pct":2.5,
                "p95_latency_ms":120,
                "active_alerts":0
            }],
            "deployment":{
                "total_cost_usd":4.2,
                "total_invocations":900,
                "overall_error_rate_pct":1.1,
                "active_alert_count":0,
                "avg_health_score":85
            },
            "window_hours":24
        }"#;
        let summary: HealthSummary = serde_json::from_str(body).unwrap();
        assert_eq!(summary.agents.len(), 1);
        assert_eq!(summary.agents[0].agent, "alice");
        assert_eq!(summary.agents[0].score, 85);
        assert_eq!(summary.agents[0].status, "green");
    }

    #[test]
    fn active_alert_deserialises_with_optional_method() {
        let body = r#"[{
            "agent":"alice",
            "kind":"budget_exceeded",
            "severity":"critical",
            "threshold":1000000.0,
            "actual":2000000.0,
            "message":"budget tripped",
            "method":"budget:agent:daily"
        }]"#;
        let alerts: Vec<ActiveAlert> = serde_json::from_str(body).unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "budget_exceeded");
        assert_eq!(alerts[0].method.as_deref(), Some("budget:agent:daily"));
    }
}
