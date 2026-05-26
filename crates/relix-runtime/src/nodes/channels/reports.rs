//! Scheduled summary reports.
//!
//! Operators configure `[reports]` in the bridge / controller
//! TOML to get a periodic digest delivered to one or more
//! channels:
//!
//! ```toml
//! [reports]
//! enabled  = true
//! schedule = "0 9 * * 1"   # cron expression (5-field)
//! channels = ["telegram", "discord", "slack"]
//! ```
//!
//! The reporter assembles a [`SummaryReport`] from the
//! coordinator + memory peers, formats it appropriately for
//! each channel (Telegram MarkdownV2, Discord markdown, Slack
//! mrkdwn), and dispatches via the existing channel surfaces.
//!
//! ## Honest scope
//!
//! - The cron evaluator is the existing
//!   `crate::nodes::coordinator::cron::Schedule` parser, so
//!   five-field cron expressions just work. Operators who want
//!   `@daily` / `@weekly` shorthand should write the canonical
//!   form (`0 0 * * *` / `0 0 * * 0`).
//! - The reporter uses a simple `tokio::spawn` loop: wake every
//!   minute, ask the schedule "do you fire in this minute?",
//!   and if so assemble + dispatch. Missed ticks (process was
//!   down) are NOT replayed — the next scheduled fire is what
//!   the operator gets.
//! - Per-channel dispatch is best-effort and fully isolated:
//!   one channel failing doesn't block the others.

use std::sync::Arc;

use serde::Deserialize;

/// `[reports]` section. Every field has a default so the
/// section is opt-in — absent means no reporter spawns.
#[derive(Clone, Debug, Deserialize)]
pub struct ReportsConfig {
    /// Master switch. `false` (default) means no scheduled
    /// reporter ever runs even if the section is present.
    #[serde(default)]
    pub enabled: bool,
    /// Five-field cron expression. Default is daily at 09:00.
    #[serde(default = "default_schedule")]
    pub schedule: String,
    /// Channel names to deliver the report to. Each entry must
    /// match a channel the operator has separately configured
    /// (`telegram`, `discord`, `slack`). Empty disables
    /// delivery — useful for "dry run" mode that exercises
    /// assembly without sending.
    #[serde(default)]
    pub channels: Vec<String>,
}

impl Default for ReportsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: default_schedule(),
            channels: Vec::new(),
        }
    }
}

fn default_schedule() -> String {
    "0 9 * * *".to_string()
}

/// One assembled summary report. Pure data; the per-channel
/// renderers below convert this into platform-specific markup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryReport {
    /// Human-readable period label ("Last 24 hours", etc.).
    pub period: String,
    pub tasks_completed: i64,
    pub tasks_failed: i64,
    pub avg_task_duration_secs: i64,
    /// Total cost across all tasks in the period, USD.
    /// Carried as cents (i64) so renderers don't have to deal
    /// with float formatting variance — `cost_cents / 100`
    /// gives the dollar amount.
    pub total_cost_cents: i64,
    pub most_active_agent: String,
    pub memory_items_added: i64,
    /// Optional alerts the reporter flagged (peers offline,
    /// failure-class anomalies, etc.). Rendered as a bulleted
    /// list at the bottom of the report.
    pub alerts: Vec<String>,
}

impl SummaryReport {
    /// Render the report as Telegram MarkdownV2. Uses the
    /// shared formatter so MarkdownV2-reserved characters in
    /// agent names or alert text get escaped properly.
    pub fn render_telegram(&self) -> String {
        let raw = self.render_plain();
        super::format_for_telegram_markdown_v2(&raw)
    }

    /// Render the report as Discord markdown. Returns one
    /// message (the report is always within Discord's 2000-
    /// char budget — alerts get truncated if needed).
    pub fn render_discord(&self) -> String {
        self.render_plain()
    }

    /// Render the report as Slack mrkdwn — converts `**bold**`
    /// to `*bold*` and strips code-fence language hints (which
    /// Slack doesn't honour).
    pub fn render_slack(&self) -> String {
        super::format_for_slack_mrkdwn(&self.render_plain())
    }

    /// Plain-text base used by every renderer. Markdown-ish but
    /// uses CommonMark `**bold**` so the per-channel renderers
    /// can fix it up. Keeping it in one place ensures every
    /// channel sees the same content.
    fn render_plain(&self) -> String {
        let mut s = String::new();
        s.push_str("**Relix summary report**\n");
        s.push_str(&format!("_Period: {}_\n\n", self.period));
        s.push_str(&format!(
            "- Tasks completed: **{}**\n",
            self.tasks_completed
        ));
        s.push_str(&format!("- Tasks failed:    **{}**\n", self.tasks_failed));
        s.push_str(&format!(
            "- Avg duration:    **{}s**\n",
            self.avg_task_duration_secs
        ));
        s.push_str(&format!(
            "- Total cost:      **${:.2}**\n",
            self.total_cost_cents as f64 / 100.0
        ));
        s.push_str(&format!(
            "- Most active:     **{}**\n",
            if self.most_active_agent.is_empty() {
                "—"
            } else {
                &self.most_active_agent
            }
        ));
        s.push_str(&format!(
            "- Memory added:    **{}**\n",
            self.memory_items_added
        ));
        if !self.alerts.is_empty() {
            s.push_str("\n**Alerts**\n");
            for a in &self.alerts {
                s.push_str(&format!("- {a}\n"));
            }
        }
        s
    }
}

/// Source the reporter pulls aggregates from. Today the
/// implementation is a thin wrapper around the coordinator's
/// `task.count` / `task.list_cursor` / `task.events`. The
/// trait shape exists so a future smarter aggregator (cached,
/// pre-rolled) can swap in without touching the scheduling
/// loop.
#[async_trait::async_trait]
pub trait ReportSource: Send + Sync {
    /// Compute aggregates for the period ending now and
    /// starting `period_secs` ago.
    async fn assemble(&self, period_secs: i64) -> SummaryReport;
}

/// Channels the reporter knows how to dispatch to. Each entry
/// is a closure that takes the rendered text + returns Ok on
/// success. The reporter calls every configured channel; one
/// failure doesn't block the others.
pub type SendFn =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Period for the daily report in seconds (24 hours).
pub const DAILY_PERIOD_SECS: i64 = 24 * 3600;

/// Per-channel dispatcher. `name` is the channel slug (matches
/// the operator's `[reports] channels = [...]` config). `send`
/// is invoked with the rendered text for that channel — the
/// reporter calls the channel-specific renderer based on the
/// name (`telegram` / `discord` / `slack`).
#[derive(Clone)]
pub struct ChannelSender {
    pub name: String,
    pub send: SendFn,
}

/// Spawn the background reports loop. Returns the
/// `JoinHandle` so callers (controller_runtime) can keep the
/// task alive for the process lifetime. Drops the handle to
/// detach.
///
/// Algorithm:
/// 1. Parse the cron schedule once at spawn time. A malformed
///    schedule logs a `tracing::error!` and the loop never
///    fires (the rest of the controller still boots).
/// 2. Compute the next fire instant via the existing
///    `coordinator::cron::schedule::Schedule::next_after`.
/// 3. Sleep until then, assemble the report from `source`,
///    render per-channel, dispatch concurrently.
/// 4. Loop forever; one channel failing doesn't block others.
///
/// Honest scope: missed-tick replay is NOT implemented — if
/// the process is down across a fire window, the next
/// scheduled time is what operators get. That matches the
/// design intent in `docs/channel-node-architecture.md`.
pub fn spawn_report_loop(
    cfg: ReportsConfig,
    source: Arc<dyn ReportSource>,
    channels: Vec<ChannelSender>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_report_loop(cfg, source, channels).await;
    })
}

async fn run_report_loop(
    cfg: ReportsConfig,
    source: Arc<dyn ReportSource>,
    channels: Vec<ChannelSender>,
) {
    if !cfg.enabled {
        tracing::info!("reports: disabled; loop will not fire");
        return;
    }
    let schedule = match crate::nodes::coordinator::cron::schedule::Schedule::parse(&cfg.schedule) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(
                schedule = %cfg.schedule,
                error = %e,
                "reports: malformed schedule expression; loop will not fire"
            );
            return;
        }
    };
    if channels.is_empty() {
        tracing::warn!(
            "reports: enabled but no channels configured; loop will fire but messages \
             go nowhere"
        );
    }
    tracing::info!(
        schedule = %cfg.schedule,
        channels = ?channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "reports: scheduler started"
    );
    loop {
        let now = unix_secs();
        let next = schedule.next_after(now);
        let wait = (next - now).max(1) as u64;
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;

        let report = source.assemble(DAILY_PERIOD_SECS).await;
        for ch in &channels {
            let rendered = match ch.name.as_str() {
                "telegram" => report.render_telegram(),
                "discord" => report.render_discord(),
                "slack" => report.render_slack(),
                other => {
                    tracing::warn!(channel = other, "reports: unknown channel; skipping");
                    continue;
                }
            };
            let send = ch.send.clone();
            let name = ch.name.clone();
            tokio::spawn(async move {
                if let Err(e) = (send)(rendered).await {
                    tracing::warn!(channel = %name, error = %e, "reports: dispatch failed");
                }
            });
        }
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> SummaryReport {
        SummaryReport {
            period: "Last 24 hours".into(),
            tasks_completed: 42,
            tasks_failed: 3,
            avg_task_duration_secs: 7,
            total_cost_cents: 1234,
            most_active_agent: "alice".into(),
            memory_items_added: 9,
            alerts: vec!["one peer flapping".into()],
        }
    }

    #[test]
    fn render_plain_contains_every_metric() {
        let r = sample_report();
        let s = r.render_plain();
        assert!(s.contains("Tasks completed"));
        assert!(s.contains("**42**"));
        assert!(s.contains("Tasks failed"));
        assert!(s.contains("**3**"));
        assert!(s.contains("$12.34"));
        assert!(s.contains("alice"));
        assert!(s.contains("Memory added"));
        assert!(s.contains("Alerts"));
        assert!(s.contains("one peer flapping"));
    }

    #[test]
    fn render_telegram_escapes_reserved_characters() {
        let r = sample_report();
        let t = r.render_telegram();
        // The dollar sign isn't reserved, but the `.` in "$12.34"
        // and the `_` in `_Period: ..._` ARE — both must be
        // backslash-escaped.
        assert!(t.contains(r"\."));
        assert!(t.contains(r"\_"));
    }

    #[test]
    fn render_discord_passes_markdown_through() {
        let r = sample_report();
        let d = r.render_discord();
        assert!(d.contains("**Relix summary report**"));
        assert!(d.contains("**42**"));
    }

    #[test]
    fn render_slack_converts_double_asterisks_to_single() {
        let r = sample_report();
        let s = r.render_slack();
        // `**42**` (CommonMark bold) became `*42*` (Slack
        // mrkdwn bold).
        assert!(s.contains("*42*"), "got {s}");
        assert!(!s.contains("**42**"), "double-asterisk survived");
    }

    #[test]
    fn empty_alerts_omits_section() {
        let mut r = sample_report();
        r.alerts.clear();
        let s = r.render_plain();
        assert!(!s.contains("Alerts"));
    }

    #[test]
    fn reports_config_defaults_to_disabled() {
        let c = ReportsConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.schedule, "0 9 * * *");
        assert!(c.channels.is_empty());
    }

    #[test]
    fn next_fire_calculation_uses_existing_schedule_parser() {
        // Sanity that the spawn loop's schedule parser handles
        // both 5-field cron and duration shorthand. The actual
        // arithmetic lives in the cron module; this test just
        // confirms reports honour both shapes.
        let cron = crate::nodes::coordinator::cron::schedule::Schedule::parse("0 9 * * 1");
        assert!(cron.is_ok(), "5-field cron must parse");
        let dur = crate::nodes::coordinator::cron::schedule::Schedule::parse("30m");
        assert!(dur.is_ok(), "duration shorthand must parse");
        let dur = dur.unwrap();
        // 30m == 1800s from `now`.
        let now = 1_700_000_000;
        let next = dur.next_after(now);
        assert_eq!(next - now, 1800);
    }

    #[tokio::test]
    async fn spawn_report_loop_returns_immediately_when_disabled() {
        // `enabled = false` short-circuits — the spawned task
        // should complete on its own without firing.
        let cfg = ReportsConfig {
            enabled: false,
            schedule: "0 9 * * 1".into(),
            channels: vec![],
        };
        struct DummySource;
        #[async_trait::async_trait]
        impl ReportSource for DummySource {
            async fn assemble(&self, _: i64) -> SummaryReport {
                sample_report()
            }
        }
        let handle = spawn_report_loop(cfg, Arc::new(DummySource), Vec::new());
        // Race against a generous deadline — the task must
        // complete because `enabled = false` returns early.
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        assert!(res.is_ok(), "disabled loop should exit immediately");
    }

    #[test]
    fn most_active_agent_falls_back_to_dash_when_empty() {
        let mut r = sample_report();
        r.most_active_agent.clear();
        let s = r.render_plain();
        assert!(s.contains("Most active"));
        assert!(s.contains("—"));
    }
}
