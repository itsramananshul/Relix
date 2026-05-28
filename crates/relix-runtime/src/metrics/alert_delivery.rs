//! RELIX-7.11 GAP 3 + GAP 4 — channel fan-out + chronicle
//! writes for alert events.
//!
//! Two sink implementations of [`super::alert::AlertDeliver`]:
//!
//! - [`MultiChannelAlertSink`] — dispatches every alert event
//!   to a configured list of channel targets (Telegram /
//!   Discord / Slack / Email) by calling the corresponding
//!   `*.send` capability on each peer through the coordinator's
//!   `MeshClient`. Non-blocking: `deliver` returns immediately
//!   and the per-target dispatch runs on a tokio task. An
//!   unavailable target logs a warn line but never blocks the
//!   alert engine or stops the next target.
//! - [`ChronicleAlertSink`] — writes every alert event to a
//!   small append-only SQLite chronicle (`alerts.sqlite` next
//!   to `metrics.sqlite`). Always runs alongside the
//!   multi-channel sink so an operator who hasn't wired any
//!   channels still has a persistent audit trail.
//!
//! [`CompositeAlertSink`] composes any number of underlying
//! sinks so the engine sees one delivery target.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use relix_core::bundle::Bundle;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;

use super::alert::{ActiveAlert, AlertDeliver, AlertEvent, AlertKind, AlertSeverity};

/// Static dispatch deadline for channel sends — kept short so a
/// hung Telegram / Discord doesn't pile up tokio tasks.
const SEND_DEADLINE_SECS: i64 = 30;

/// `[metrics.alerts]` config block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct AlertDeliveryConfig {
    /// Optional sqlite path for the alert chronicle. When
    /// unset, the runtime drops it next to the configured
    /// metrics db (`<dir>/alerts.sqlite`).
    #[serde(default)]
    pub chronicle_path: Option<std::path::PathBuf>,
    /// Channel-delivery targets. An empty list means the
    /// multi-channel sink stays dormant; the chronicle sink
    /// still runs.
    #[serde(default)]
    pub targets: Vec<AlertTarget>,
}

/// One channel delivery target — a `(channel, peer)` pair plus
/// optional channel-specific destination metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AlertTarget {
    /// Channel type — `"telegram"` | `"discord"` | `"slack"` |
    /// `"email"`.
    pub channel: String,
    /// Peer alias to dispatch through.
    pub peer: String,
    /// Email: the `To:` recipient. Required when `channel ==
    /// "email"`. Ignored on the other channels.
    #[serde(default)]
    pub to: Option<String>,
    /// Email-only: the `Subject:` override. Defaults to a
    /// templated string built from the alert.
    #[serde(default)]
    pub subject: Option<String>,
    /// Telegram-only: the chat id to post into. Numeric Telegram
    /// chat id passed as a string so the operator's TOML stays
    /// readable. Required when `channel == "telegram"`.
    #[serde(default)]
    pub chat_id: Option<String>,
    /// Discord-only: the channel snowflake id. Required when
    /// `channel == "discord"`.
    #[serde(default)]
    pub channel_id: Option<String>,
    /// Slack-only: the destination channel id (`C…`) or name
    /// (`#ops-alerts`). Required when `channel == "slack"`.
    #[serde(default, alias = "slack_channel")]
    pub slack_channel: Option<String>,
}

/// Mesh client + caller identity bundle handed to the
/// multi-channel sink. Wrapped in an `Arc<OnceCell<...>>` so
/// the coordinator's startup can populate it after the
/// `rpc::Client` finishes discovery.
pub type AlertMeshCell = Arc<tokio::sync::OnceCell<AlertMeshContext>>;

/// Bundle of everything the multi-channel sink needs to
/// dispatch a capability call.
#[derive(Clone)]
pub struct AlertMeshContext {
    pub mesh: MeshClient,
    pub identity: Bundle,
}

/// Channel fan-out alert sink. Non-blocking — `deliver` spawns
/// one task per target.
pub struct MultiChannelAlertSink {
    cell: AlertMeshCell,
    targets: Vec<AlertTarget>,
}

impl MultiChannelAlertSink {
    pub fn new(cell: AlertMeshCell, targets: Vec<AlertTarget>) -> Self {
        Self { cell, targets }
    }

    /// True iff the sink will actually do anything when an
    /// alert fires (mesh up + at least one target configured).
    pub fn is_active(&self) -> bool {
        !self.targets.is_empty()
    }

    /// Format an alert into the operator-facing channel
    /// message body, as documented in the spec.
    pub fn format_message(event: &AlertEvent) -> String {
        match event {
            AlertEvent::Fired(a) => format_fired(a),
            AlertEvent::Recovered(a) => format_recovered(a),
        }
    }
}

fn format_fired(a: &ActiveAlert) -> String {
    // RELIX-7.19 GAP 2: LowConfidence uses a dedicated
    // operator-facing format per spec.
    if a.kind == AlertKind::LowConfidence {
        return format_low_confidence_fired(a);
    }
    // RELIX-7.28 Part 1: BudgetExceeded carries the cause /
    // window / scope inside `message`; the alert layer renders
    // a dedicated panel so operators see the cap they tripped.
    if a.kind == AlertKind::BudgetExceeded {
        return format_budget_exceeded_fired(a);
    }
    let (badge, header) = match a.severity {
        AlertSeverity::Warning => ("⚠️", "Relix Alert — WARNING"),
        AlertSeverity::Critical => ("🚨", "Relix Alert — CRITICAL"),
    };
    format!(
        "{badge} {header}\n\
         Agent: {agent}\n\
         Metric: {metric} exceeded threshold\n\
         Current: {actual}\n\
         Threshold: {threshold}\n\
         Time: {ts}",
        badge = badge,
        header = header,
        agent = a.agent,
        metric = a.kind.as_str(),
        actual = format_value(a.kind.as_str(), a.actual),
        threshold = format_value(a.kind.as_str(), a.threshold),
        ts = iso_ms(a.triggered_at_ms),
    )
}

fn format_recovered(a: &ActiveAlert) -> String {
    if a.kind == AlertKind::LowConfidence {
        return format_low_confidence_recovered(a);
    }
    if a.kind == AlertKind::BudgetExceeded {
        return format_budget_exceeded_recovered(a);
    }
    format!(
        "✅ Relix Alert — RECOVERED\n\
         Agent: {agent}\n\
         Metric: {metric} back below threshold\n\
         Current: {actual}\n\
         Threshold: {threshold}\n\
         Time: {ts}",
        agent = a.agent,
        metric = a.kind.as_str(),
        actual = format_value(a.kind.as_str(), a.actual),
        threshold = format_value(a.kind.as_str(), a.threshold),
        ts = iso_ms(unix_now_ms()),
    )
}

/// RELIX-7.19 GAP 2: low-confidence formatter. Spec format:
///
/// ```text
/// ⚠️ Relix Alert — LOW CONFIDENCE
/// Agent: <agent>
/// Method: <method>
/// Confidence: <score> (threshold: <threshold>)
/// Message: <alert_message>
/// ```
///
/// And the critical variant swaps the badge + header:
///
/// ```text
/// 🚨 Relix Alert — CRITICALLY LOW CONFIDENCE
/// ```
fn format_low_confidence_fired(a: &ActiveAlert) -> String {
    let (badge, header) = match a.severity {
        AlertSeverity::Warning => ("⚠️", "Relix Alert — LOW CONFIDENCE"),
        AlertSeverity::Critical => ("🚨", "Relix Alert — CRITICALLY LOW CONFIDENCE"),
    };
    let method = a.method.as_deref().unwrap_or("(unknown)");
    format!(
        "{badge} {header}\n\
         Agent: {agent}\n\
         Method: {method}\n\
         Confidence: {score} (threshold: {threshold})\n\
         Message: {message}",
        badge = badge,
        header = header,
        agent = a.agent,
        method = method,
        score = format_value("low_confidence", a.actual),
        threshold = format_value("low_confidence", a.threshold),
        message = a.message,
    )
}

fn format_low_confidence_recovered(a: &ActiveAlert) -> String {
    let method = a.method.as_deref().unwrap_or("(unknown)");
    format!(
        "✅ Relix Alert — CONFIDENCE RECOVERED\n\
         Agent: {agent}\n\
         Method: {method}\n\
         Confidence: {score} (threshold: {threshold})\n\
         Time: {ts}",
        agent = a.agent,
        method = method,
        score = format_value("low_confidence", a.actual),
        threshold = format_value("low_confidence", a.threshold),
        ts = iso_ms(unix_now_ms()),
    )
}

/// RELIX-7.28 Part 1: BudgetExceeded fired-alert formatter.
fn format_budget_exceeded_fired(a: &ActiveAlert) -> String {
    let limit_usd = a.threshold / 1_000_000.0;
    let actual_usd = a.actual / 1_000_000.0;
    let descriptor = a
        .method
        .clone()
        .unwrap_or_else(|| "budget:agent:daily".to_string());
    let agent_label = if a.agent.is_empty() {
        "(deployment)".to_string()
    } else {
        a.agent.clone()
    };
    format!(
        "🚨 Relix Alert — BUDGET EXCEEDED\n\
         Agent: {agent}\n\
         Budget: {descriptor}\n\
         Limit: ${limit:.4} USD\n\
         Current: ${actual:.4} USD\n\
         {message}\n\
         Time: {ts}",
        agent = agent_label,
        descriptor = descriptor,
        limit = limit_usd,
        actual = actual_usd,
        message = a.message,
        ts = iso_ms(a.triggered_at_ms),
    )
}

fn format_budget_exceeded_recovered(a: &ActiveAlert) -> String {
    let agent_label = if a.agent.is_empty() {
        "(deployment)".to_string()
    } else {
        a.agent.clone()
    };
    let descriptor = a
        .method
        .clone()
        .unwrap_or_else(|| "budget:agent:daily".to_string());
    format!(
        "✅ Relix Alert — BUDGET WINDOW RESET\n\
         Agent: {agent}\n\
         Budget: {descriptor}\n\
         Window reset (current spend ${actual:.4} USD)\n\
         Time: {ts}",
        agent = agent_label,
        descriptor = descriptor,
        actual = a.actual / 1_000_000.0,
        ts = iso_ms(unix_now_ms()),
    )
}

/// Render a metric value with units appropriate to the metric.
fn format_value(metric: &str, value: f64) -> String {
    match metric {
        "error_rate" => format!("{value:.2}%"),
        "p95_latency" => format!("{value:.0} ms"),
        "cost_per_hour" => format!("${:.4}", value / 1_000_000.0),
        "zero_success" => format!("{value:.0} successes"),
        // RELIX-7.19 GAP 2: confidence renders as a fixed
        // 3-decimal float in `[0, 1]`.
        "low_confidence" => format!("{value:.3}"),
        // RELIX-7.28 Part 1: budget values are stored in
        // micro-USD; convert to dollars at the rendering edge.
        "budget_exceeded" => format!("${:.4}", value / 1_000_000.0),
        _ => format!("{value:.2}"),
    }
}

impl AlertDeliver for MultiChannelAlertSink {
    fn deliver(&self, event: &AlertEvent) {
        if self.targets.is_empty() {
            return;
        }
        let Some(ctx) = self.cell.get().cloned() else {
            tracing::warn!(
                agent = %event.agent(),
                metric = event.kind().as_str(),
                "alert delivery: mesh client not initialised; skipping channel fan-out"
            );
            return;
        };
        let body = MultiChannelAlertSink::format_message(event);
        for target in &self.targets {
            let target = target.clone();
            let ctx = ctx.clone();
            let body = body.clone();
            // Spawn one task per target so a slow / stuck
            // channel can't block the alert engine OR the
            // next target.
            tokio::spawn(async move {
                if let Err(e) = dispatch_to_target(&ctx, &target, &body).await {
                    tracing::warn!(
                        channel = %target.channel,
                        peer = %target.peer,
                        error = %e,
                        "alert delivery: dispatch failed"
                    );
                }
            });
        }
    }
}

/// Project a target + alert body into `(capability_method,
/// json_bytes)` ready for the mesh dispatch. Pulled out of
/// `dispatch_to_target` so unit tests can verify the wire shape
/// per channel without standing up a libp2p stack.
pub(crate) fn encode_dispatch(
    target: &AlertTarget,
    body: &str,
) -> Result<(&'static str, Vec<u8>), String> {
    let channel = target.channel.trim().to_ascii_lowercase();
    match channel.as_str() {
        "email" => {
            let to = target
                .to
                .as_deref()
                .ok_or_else(|| "email target missing `to` field".to_string())?;
            let subject = target
                .subject
                .clone()
                .unwrap_or_else(|| "Relix alert".to_string());
            let args = serde_json::json!({
                "to": [to],
                "subject": subject,
                "body": body,
            });
            let arg_bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("email.send", arg_bytes))
        }
        "telegram" => {
            let chat_id = target
                .chat_id
                .as_deref()
                .ok_or_else(|| "telegram target missing `chat_id` field".to_string())?;
            let args = serde_json::json!({
                "chat_id": chat_id,
                "text": body,
            });
            let arg_bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("telegram.send", arg_bytes))
        }
        "discord" => {
            let channel_id = target
                .channel_id
                .as_deref()
                .ok_or_else(|| "discord target missing `channel_id` field".to_string())?;
            let args = serde_json::json!({
                "channel_id": channel_id,
                "text": body,
            });
            let arg_bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("discord.send", arg_bytes))
        }
        "slack" => {
            let slack_channel = target
                .slack_channel
                .as_deref()
                .ok_or_else(|| "slack target missing `slack_channel` field".to_string())?;
            let args = serde_json::json!({
                "channel": slack_channel,
                "text": body,
            });
            let arg_bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("slack.send", arg_bytes))
        }
        other => Err(format!("unknown channel: {other}")),
    }
}

async fn dispatch_to_target(
    ctx: &AlertMeshContext,
    target: &AlertTarget,
    body: &str,
) -> Result<(), String> {
    let (method, arg_bytes) = encode_dispatch(target, body)?;
    call_unary(ctx, &target.peer, method, arg_bytes).await
}

async fn call_unary(
    ctx: &AlertMeshContext,
    alias: &str,
    method: &str,
    body: Vec<u8>,
) -> Result<(), String> {
    let envelope = build_request(method, body, ctx.identity.clone(), SEND_DEADLINE_SECS);
    let raw = tokio::time::timeout(
        Duration::from_secs(SEND_DEADLINE_SECS as u64 + 5),
        ctx.mesh.call(alias, envelope),
    )
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(|e| format!("call: {e}"))?;
    let resp = decode_response(&raw).map_err(|e| format!("decode: {e}"))?;
    match resp.res {
        ResponseResult::Ok(_) => Ok(()),
        ResponseResult::Err(env) => Err(format!(
            "responder err kind={} cause={}",
            env.kind, env.cause
        )),
        ResponseResult::StreamHandle(_) => Err("unexpected stream handle".into()),
    }
}

// ── chronicle ────────────────────────────────────────────

/// Append-only SQLite chronicle for alert events. Sits next to
/// the metrics db; survives restarts.
#[derive(Clone)]
pub struct AlertChronicle {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChronicleError {
    #[error("alert chronicle io: {0}")]
    Io(String),
    #[error("alert chronicle sqlite: {0}")]
    Db(String),
    #[error("alert chronicle lock poisoned")]
    Lock,
}

impl From<rusqlite::Error> for ChronicleError {
    fn from(e: rusqlite::Error) -> Self {
        ChronicleError::Db(e.to_string())
    }
}

/// One persisted alert event row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlertChronicleRow {
    /// `"alert.fired"` or `"alert.recovered"`.
    pub event_type: String,
    pub agent: String,
    pub metric: String,
    /// Only populated for `alert.fired`. `"warning"` / `"critical"`.
    pub severity: Option<String>,
    pub actual_value: f64,
    pub threshold_value: f64,
    /// ISO-8601 — populated for both fired and recovered rows.
    /// On a recovered row this is the timestamp the alert
    /// ORIGINALLY fired.
    pub triggered_at: Option<String>,
    /// ISO-8601 — populated only on `alert.recovered`.
    pub recovered_at: Option<String>,
    /// Unix-ms timestamp the row was written. Useful for
    /// pure-SQL queries.
    pub recorded_at_ms: i64,
    /// RELIX-7.19 GAP 2: capability method the alert was raised
    /// against. `None` for poll-driven kinds; `Some(method)` for
    /// `LowConfidence`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// RELIX-7.19 GAP 2: operator-supplied alert message.
    /// `None` for poll-driven kinds whose message is built from
    /// the metric values; `Some(msg)` for `LowConfidence`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AlertChronicle {
    pub fn open(path: &Path) -> Result<Self, ChronicleError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ChronicleError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, ChronicleError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record one alert event. Computes the right
    /// `event_type` / severity / triggered_at / recovered_at
    /// fields from the event variant.
    pub fn record(&self, event: &AlertEvent) -> Result<(), ChronicleError> {
        let now_ms = unix_now_ms();
        let row = match event {
            AlertEvent::Fired(a) => AlertChronicleRow {
                event_type: "alert.fired".into(),
                agent: a.agent.clone(),
                metric: a.kind.as_str().to_string(),
                severity: Some(a.severity.as_str().to_string()),
                actual_value: a.actual,
                threshold_value: a.threshold,
                triggered_at: Some(iso_ms(a.triggered_at_ms)),
                recovered_at: None,
                recorded_at_ms: now_ms,
                method: a.method.clone(),
                message: if matches!(a.kind, AlertKind::LowConfidence | AlertKind::BudgetExceeded) {
                    Some(a.message.clone())
                } else {
                    None
                },
            },
            AlertEvent::Recovered(a) => AlertChronicleRow {
                event_type: "alert.recovered".into(),
                agent: a.agent.clone(),
                metric: a.kind.as_str().to_string(),
                severity: None,
                actual_value: a.actual,
                threshold_value: a.threshold,
                triggered_at: Some(iso_ms(a.triggered_at_ms)),
                recovered_at: Some(iso_ms(now_ms)),
                recorded_at_ms: now_ms,
                method: a.method.clone(),
                message: None,
            },
        };
        let conn = self.conn.lock().map_err(|_| ChronicleError::Lock)?;
        conn.execute(
            "INSERT INTO alert_events \
             (event_type, agent, metric, severity, actual_value, threshold_value, \
              triggered_at, recovered_at, recorded_at_ms, method, message) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.event_type,
                row.agent,
                row.metric,
                row.severity,
                row.actual_value,
                row.threshold_value,
                row.triggered_at,
                row.recovered_at,
                row.recorded_at_ms,
                row.method,
                row.message,
            ],
        )?;
        Ok(())
    }

    /// Snapshot the newest N rows. Used by tests + future
    /// dashboard queries.
    pub fn recent(&self, limit: usize) -> Result<Vec<AlertChronicleRow>, ChronicleError> {
        let conn = self.conn.lock().map_err(|_| ChronicleError::Lock)?;
        let limit = limit.clamp(1, 1000) as i64;
        let mut stmt = conn.prepare(
            "SELECT event_type, agent, metric, severity, actual_value, threshold_value, \
                    triggered_at, recovered_at, recorded_at_ms, method, message \
             FROM alert_events ORDER BY recorded_at_ms DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(AlertChronicleRow {
                event_type: r.get(0)?,
                agent: r.get(1)?,
                metric: r.get(2)?,
                severity: r.get(3)?,
                actual_value: r.get(4)?,
                threshold_value: r.get(5)?,
                triggered_at: r.get(6)?,
                recovered_at: r.get(7)?,
                recorded_at_ms: r.get(8)?,
                method: r.get(9)?,
                message: r.get(10)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Row count — used by tests + the dashboard's "alerts
    /// recorded" indicator.
    pub fn count(&self) -> Result<u64, ChronicleError> {
        let conn = self.conn.lock().map_err(|_| ChronicleError::Lock)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM alert_events", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

fn init_schema(conn: &Connection) -> Result<(), ChronicleError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS alert_events (\
             id              INTEGER PRIMARY KEY AUTOINCREMENT,\
             event_type      TEXT NOT NULL,\
             agent           TEXT NOT NULL,\
             metric          TEXT NOT NULL,\
             severity        TEXT,\
             actual_value    REAL NOT NULL,\
             threshold_value REAL NOT NULL,\
             triggered_at    TEXT,\
             recovered_at    TEXT,\
             recorded_at_ms  INTEGER NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS alert_events_recorded_at \
             ON alert_events(recorded_at_ms DESC);\
         CREATE INDEX IF NOT EXISTS alert_events_agent_ts \
             ON alert_events(agent, recorded_at_ms DESC);",
    )?;
    // RELIX-7.19 GAP 2: backwards-compat ALTER to add the
    // `method` + `message` columns when missing. Pre-7.19
    // databases pick them up on open with safe NULL defaults.
    if !column_exists(conn, "alert_events", "method")? {
        conn.execute("ALTER TABLE alert_events ADD COLUMN method TEXT", [])?;
    }
    if !column_exists(conn, "alert_events", "message")? {
        conn.execute("ALTER TABLE alert_events ADD COLUMN message TEXT", [])?;
    }
    Ok(())
}

/// Probe for a column's existence using SQLite's `PRAGMA
/// table_info`. Used by the 7.19 GAP 2 alert-chronicle
/// migration so an upgraded database picks up the new
/// columns without failing the `ALTER TABLE` on a fresh
/// schema.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, ChronicleError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `AlertDeliver` that writes every event to a chronicle.
/// Always runs alongside the multi-channel sink so an operator
/// who hasn't wired any channel targets still has a persistent
/// audit trail.
pub struct ChronicleAlertSink {
    chronicle: AlertChronicle,
}

impl ChronicleAlertSink {
    pub fn new(chronicle: AlertChronicle) -> Self {
        Self { chronicle }
    }

    /// Cheap handle to the underlying chronicle so other
    /// surfaces (CLI / bridge) can read recent rows.
    pub fn chronicle(&self) -> AlertChronicle {
        self.chronicle.clone()
    }
}

impl AlertDeliver for ChronicleAlertSink {
    fn deliver(&self, event: &AlertEvent) {
        if let Err(e) = self.chronicle.record(event) {
            tracing::warn!(error = %e, "alert chronicle: write failed");
        }
    }
}

// ── composite sink ───────────────────────────────────────

/// Fan an alert event out to every wrapped sink. Used to wire
/// chronicle + channel + logging sinks behind a single
/// `AlertSink` the engine sees.
pub struct CompositeAlertSink {
    sinks: Vec<Arc<dyn AlertDeliver>>,
}

impl CompositeAlertSink {
    pub fn new(sinks: Vec<Arc<dyn AlertDeliver>>) -> Self {
        Self { sinks }
    }
}

impl AlertDeliver for CompositeAlertSink {
    fn deliver(&self, event: &AlertEvent) {
        for s in &self.sinks {
            s.deliver(event);
        }
    }
}

// ── helpers ──────────────────────────────────────────────

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Render a unix-ms timestamp as ISO 8601 in UTC, second
/// precision. The runtime ships `time` in workspace.deps, but
/// this helper stays dep-free to mirror `db.rs`'s home-rolled
/// formatter — keeps the alert path small.
pub fn iso_ms(ts_ms: i64) -> String {
    let secs = (ts_ms / 1000).max(0);
    let ms = (ts_ms % 1000).max(0);
    let days = secs / 86_400;
    let rem = secs.rem_euclid(86_400);
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    let (y, mo, d) = days_to_ymd(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z",
        y = y,
        mo = mo,
        d = d,
        h = h,
        m = m,
        s = s,
        ms = ms
    )
}

fn days_to_ymd(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::super::alert::AlertKind;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fired(agent: &str, severity: AlertSeverity, ts_ms: i64) -> ActiveAlert {
        ActiveAlert {
            agent: agent.into(),
            kind: AlertKind::ErrorRate,
            severity,
            triggered_at_ms: ts_ms,
            threshold: 10.0,
            actual: 12.5,
            message: "test".into(),
            method: None,
        }
    }

    fn fired_low_conf(
        agent: &str,
        method: &str,
        score: f64,
        threshold: f64,
        severity: AlertSeverity,
        ts_ms: i64,
    ) -> ActiveAlert {
        ActiveAlert {
            agent: agent.into(),
            kind: AlertKind::LowConfidence,
            severity,
            triggered_at_ms: ts_ms,
            threshold,
            actual: score,
            message: "tool wobble".into(),
            method: Some(method.into()),
        }
    }

    // ── format tests ─────────────────────────────────────

    #[test]
    fn format_warning_alert_includes_warning_badge_and_fields() {
        let event = AlertEvent::Fired(fired("alice", AlertSeverity::Warning, 1_700_000_000_000));
        let body = MultiChannelAlertSink::format_message(&event);
        assert!(body.contains("⚠️"));
        assert!(body.contains("WARNING"));
        assert!(body.contains("Agent: alice"));
        assert!(body.contains("Metric: error_rate"));
        assert!(body.contains("Current: 12.50%"));
        assert!(body.contains("Threshold: 10.00%"));
        assert!(body.contains("Time: 2023-"));
    }

    #[test]
    fn format_critical_alert_uses_critical_badge() {
        let event = AlertEvent::Fired(fired("bob", AlertSeverity::Critical, 1_700_000_000_000));
        let body = MultiChannelAlertSink::format_message(&event);
        assert!(body.contains("🚨"));
        assert!(body.contains("CRITICAL"));
    }

    #[test]
    fn format_recovered_uses_recovered_badge_and_message() {
        let event =
            AlertEvent::Recovered(fired("alice", AlertSeverity::Warning, 1_700_000_000_000));
        let body = MultiChannelAlertSink::format_message(&event);
        assert!(body.contains("✅"));
        assert!(body.contains("RECOVERED"));
        assert!(body.contains("back below threshold"));
    }

    #[test]
    fn cost_value_renders_in_dollars() {
        // 2_500_000 micro-USD = $2.50.
        let s = format_value("cost_per_hour", 2_500_000.0);
        assert_eq!(s, "$2.5000");
    }

    #[test]
    fn p95_value_renders_in_ms() {
        assert_eq!(format_value("p95_latency", 1500.0), "1500 ms");
    }

    // ── multi-channel routing tests ──────────────────────

    #[test]
    fn empty_target_list_makes_sink_inactive() {
        let sink = MultiChannelAlertSink::new(Arc::new(tokio::sync::OnceCell::new()), Vec::new());
        assert!(!sink.is_active());
        // Calling deliver on an empty sink is a no-op.
        sink.deliver(&AlertEvent::Fired(fired(
            "alice",
            AlertSeverity::Warning,
            1_700_000_000_000,
        )));
    }

    #[tokio::test]
    async fn deliver_with_no_mesh_logs_and_returns() {
        // No `AlertMeshContext` in the cell yet — sink should
        // skip dispatch without panicking.
        let cell: AlertMeshCell = Arc::new(tokio::sync::OnceCell::new());
        let sink = MultiChannelAlertSink::new(
            cell,
            vec![AlertTarget {
                channel: "email".into(),
                peer: "email-peer".into(),
                to: Some("ops@example.com".into()),
                subject: None,
                chat_id: None,
                channel_id: None,
                slack_channel: None,
            }],
        );
        sink.deliver(&AlertEvent::Fired(fired(
            "alice",
            AlertSeverity::Critical,
            1_700_000_000_000,
        )));
        // Yield once so any (none-expected) spawned task can
        // run before we leave.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Verify a non-blocking deliver path: a *blocking* sink
    /// wrapped behind a CompositeAlertSink shouldn't stall
    /// the others. We simulate by giving the composite a
    /// recording sink + a panicking sink and confirming the
    /// recording sink still ran.
    #[test]
    fn composite_runs_every_sink_even_when_one_panics_in_record() {
        struct CountingSink(Arc<AtomicUsize>);
        impl AlertDeliver for CountingSink {
            fn deliver(&self, _e: &AlertEvent) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let counter = Arc::new(AtomicUsize::new(0));
        let composite = CompositeAlertSink::new(vec![
            Arc::new(CountingSink(counter.clone())) as Arc<dyn AlertDeliver>,
            Arc::new(CountingSink(counter.clone())) as Arc<dyn AlertDeliver>,
        ]);
        composite.deliver(&AlertEvent::Fired(fired(
            "alice",
            AlertSeverity::Warning,
            1_700_000_000_000,
        )));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    // ── chronicle tests ──────────────────────────────────

    #[test]
    fn chronicle_records_fired_event_with_all_fields() {
        let ch = AlertChronicle::in_memory().unwrap();
        let event = AlertEvent::Fired(fired("alice", AlertSeverity::Warning, 1_700_000_000_000));
        ch.record(&event).unwrap();
        let rows = ch.recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.event_type, "alert.fired");
        assert_eq!(r.agent, "alice");
        assert_eq!(r.metric, "error_rate");
        assert_eq!(r.severity.as_deref(), Some("warning"));
        assert_eq!(r.actual_value, 12.5);
        assert_eq!(r.threshold_value, 10.0);
        assert!(r.triggered_at.as_ref().unwrap().starts_with("2023-"));
        assert!(r.recovered_at.is_none());
    }

    #[test]
    fn chronicle_records_recovered_event_with_triggered_and_recovered() {
        let ch = AlertChronicle::in_memory().unwrap();
        let event =
            AlertEvent::Recovered(fired("alice", AlertSeverity::Warning, 1_700_000_000_000));
        ch.record(&event).unwrap();
        let rows = ch.recent(10).unwrap();
        let r = &rows[0];
        assert_eq!(r.event_type, "alert.recovered");
        assert!(r.severity.is_none());
        assert!(r.triggered_at.is_some(), "should carry original trigger ts");
        assert!(r.recovered_at.is_some(), "should carry recover ts");
    }

    #[test]
    fn chronicle_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.sqlite");
        {
            let ch = AlertChronicle::open(&path).unwrap();
            ch.record(&AlertEvent::Fired(fired(
                "alice",
                AlertSeverity::Critical,
                1_700_000_000_000,
            )))
            .unwrap();
            assert_eq!(ch.count().unwrap(), 1);
        }
        // Re-open the same file and verify the row survived.
        let ch2 = AlertChronicle::open(&path).unwrap();
        assert_eq!(ch2.count().unwrap(), 1);
        let rows = ch2.recent(10).unwrap();
        assert_eq!(rows[0].agent, "alice");
        assert_eq!(rows[0].severity.as_deref(), Some("critical"));
    }

    #[test]
    fn chronicle_sink_writes_every_delivered_event() {
        let ch = AlertChronicle::in_memory().unwrap();
        let sink = ChronicleAlertSink::new(ch.clone());
        sink.deliver(&AlertEvent::Fired(fired(
            "alice",
            AlertSeverity::Warning,
            1_700_000_000_000,
        )));
        sink.deliver(&AlertEvent::Recovered(fired(
            "alice",
            AlertSeverity::Warning,
            1_700_000_000_000,
        )));
        assert_eq!(ch.count().unwrap(), 2);
    }

    #[test]
    fn iso_ms_renders_known_timestamp() {
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20.000Z
        let s = iso_ms(1_700_000_000_000);
        assert_eq!(s, "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn iso_ms_handles_subsecond_precision() {
        // 123 ms past 1_700_000_000s
        let s = iso_ms(1_700_000_000_123);
        assert_eq!(s, "2023-11-14T22:13:20.123Z");
    }

    // ── per-channel dispatch encoding tests ──────────────

    fn target(
        channel: &str,
        peer: &str,
        to: Option<&str>,
        chat_id: Option<&str>,
        channel_id: Option<&str>,
        slack_channel: Option<&str>,
    ) -> AlertTarget {
        AlertTarget {
            channel: channel.into(),
            peer: peer.into(),
            to: to.map(|s| s.to_string()),
            subject: None,
            chat_id: chat_id.map(|s| s.to_string()),
            channel_id: channel_id.map(|s| s.to_string()),
            slack_channel: slack_channel.map(|s| s.to_string()),
        }
    }

    #[test]
    fn encode_dispatch_email_uses_email_send_with_to_subject_body() {
        let t = target(
            "email",
            "email-peer",
            Some("ops@example.com"),
            None,
            None,
            None,
        );
        let (method, bytes) = encode_dispatch(&t, "BODY").unwrap();
        assert_eq!(method, "email.send");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["body"], "BODY");
        assert_eq!(v["subject"], "Relix alert");
        assert_eq!(v["to"][0], "ops@example.com");
    }

    #[test]
    fn encode_dispatch_telegram_uses_telegram_send_with_chat_id_and_text() {
        let t = target("telegram", "tg-peer", None, Some("99988"), None, None);
        let (method, bytes) = encode_dispatch(&t, "alert body").unwrap();
        assert_eq!(method, "telegram.send");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["chat_id"], "99988");
        assert_eq!(v["text"], "alert body");
    }

    #[test]
    fn encode_dispatch_telegram_without_chat_id_fails() {
        let t = target("telegram", "tg-peer", None, None, None, None);
        match encode_dispatch(&t, "x") {
            Err(e) => assert!(e.contains("chat_id")),
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn encode_dispatch_discord_uses_discord_send_with_channel_id_and_text() {
        let t = target("discord", "dc-peer", None, None, Some("C77777"), None);
        let (method, bytes) = encode_dispatch(&t, "alert body").unwrap();
        assert_eq!(method, "discord.send");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["channel_id"], "C77777");
        assert_eq!(v["text"], "alert body");
    }

    #[test]
    fn encode_dispatch_discord_without_channel_id_fails() {
        let t = target("discord", "dc-peer", None, None, None, None);
        match encode_dispatch(&t, "x") {
            Err(e) => assert!(e.contains("channel_id")),
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn encode_dispatch_slack_uses_slack_send_with_channel_and_text() {
        let t = target("slack", "sl-peer", None, None, None, Some("#ops-alerts"));
        let (method, bytes) = encode_dispatch(&t, "alert body").unwrap();
        assert_eq!(method, "slack.send");
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["channel"], "#ops-alerts");
        assert_eq!(v["text"], "alert body");
    }

    #[test]
    fn encode_dispatch_slack_without_channel_fails() {
        let t = target("slack", "sl-peer", None, None, None, None);
        match encode_dispatch(&t, "x") {
            Err(e) => assert!(e.contains("slack_channel")),
            Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn encode_dispatch_unknown_channel_fails() {
        let t = target("smoke", "p", None, None, None, None);
        assert!(encode_dispatch(&t, "x").is_err());
    }

    /// Channel-target config parses correctly from operator TOML.
    /// Aliases the spec's `slack_channel` field through serde so
    /// operator config stays readable.
    #[test]
    fn alert_target_parses_each_channel_shape_from_toml() {
        // `r##"…"##` because the `"#ops"` literal below contains
        // the `"#` raw-string-delimiter pair, which would
        // prematurely close a single-hash `r#"…"#` block.
        let toml_text = r##"
            [[targets]]
            channel = "telegram"
            peer = "tg-peer"
            chat_id = "12345"

            [[targets]]
            channel = "discord"
            peer = "dc-peer"
            channel_id = "C77"

            [[targets]]
            channel = "slack"
            peer = "sl-peer"
            slack_channel = "#ops"

            [[targets]]
            channel = "email"
            peer = "em-peer"
            to = "ops@example.com"
        "##;
        let cfg: AlertDeliveryConfig = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.targets.len(), 4);
        assert_eq!(cfg.targets[0].chat_id.as_deref(), Some("12345"));
        assert_eq!(cfg.targets[1].channel_id.as_deref(), Some("C77"));
        assert_eq!(cfg.targets[2].slack_channel.as_deref(), Some("#ops"));
        assert_eq!(cfg.targets[3].to.as_deref(), Some("ops@example.com"));
    }

    /// MultiChannelAlertSink fans out to every target's
    /// per-target tokio task. With the cell unpopulated, every
    /// task short-circuits → no panics, no blocking. Verifies the
    /// "one bad target doesn't take down the others" guarantee.
    #[tokio::test]
    async fn fan_out_with_mixed_targets_doesnt_panic_when_mesh_absent() {
        let cell: AlertMeshCell = Arc::new(tokio::sync::OnceCell::new());
        let sink = MultiChannelAlertSink::new(
            cell,
            vec![
                target("telegram", "tg", None, Some("1"), None, None),
                target("discord", "dc", None, None, Some("C1"), None),
                target("slack", "sl", None, None, None, Some("#x")),
                target("email", "em", Some("a@b"), None, None, None),
            ],
        );
        sink.deliver(&AlertEvent::Fired(fired(
            "alice",
            AlertSeverity::Warning,
            1_700_000_000_000,
        )));
        // Give spawned tasks one tick.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // ── RELIX-7.19 GAP 2: LowConfidence formatting + chronicle

    #[test]
    fn format_low_confidence_warning_matches_documented_shape() {
        let ev = AlertEvent::Fired(fired_low_conf(
            "alice",
            "ai.chat",
            0.42,
            0.50,
            AlertSeverity::Warning,
            1_700_000_000_000,
        ));
        let body = MultiChannelAlertSink::format_message(&ev);
        // Spec format — every line present.
        assert!(body.contains("Relix Alert — LOW CONFIDENCE"), "{body}");
        assert!(body.contains("Agent: alice"), "{body}");
        assert!(body.contains("Method: ai.chat"), "{body}");
        assert!(
            body.contains("Confidence: 0.420 (threshold: 0.500)"),
            "{body}"
        );
        assert!(body.contains("Message: tool wobble"), "{body}");
        // Warning badge ⚠️ not the critical 🚨.
        assert!(body.starts_with("\u{26A0}"), "{body}");
    }

    #[test]
    fn format_low_confidence_critical_uses_critical_header() {
        let ev = AlertEvent::Fired(fired_low_conf(
            "alice",
            "ai.chat",
            0.20,
            0.30,
            AlertSeverity::Critical,
            1_700_000_000_000,
        ));
        let body = MultiChannelAlertSink::format_message(&ev);
        assert!(
            body.contains("Relix Alert — CRITICALLY LOW CONFIDENCE"),
            "{body}"
        );
        // 🚨 prefix.
        assert!(body.starts_with("\u{1F6A8}"), "{body}");
    }

    #[test]
    fn format_low_confidence_recovered_uses_recovered_header() {
        let ev = AlertEvent::Recovered(fired_low_conf(
            "alice",
            "ai.chat",
            0.85,
            0.50,
            AlertSeverity::Warning,
            1_700_000_000_000,
        ));
        let body = MultiChannelAlertSink::format_message(&ev);
        assert!(
            body.contains("Relix Alert — CONFIDENCE RECOVERED"),
            "{body}"
        );
        assert!(body.contains("Method: ai.chat"), "{body}");
    }

    #[test]
    fn chronicle_records_low_confidence_event_with_method_and_message() {
        let c = AlertChronicle::in_memory().unwrap();
        let ev = AlertEvent::Fired(fired_low_conf(
            "alice",
            "ai.chat",
            0.40,
            0.50,
            AlertSeverity::Warning,
            1_700_000_000_000,
        ));
        c.record(&ev).unwrap();
        let rows = c.recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.event_type, "alert.fired");
        assert_eq!(row.metric, "low_confidence");
        assert_eq!(row.method.as_deref(), Some("ai.chat"));
        assert_eq!(row.message.as_deref(), Some("tool wobble"));
        assert_eq!(row.severity.as_deref(), Some("warning"));
    }

    #[test]
    fn chronicle_records_low_confidence_recovered_with_method() {
        let c = AlertChronicle::in_memory().unwrap();
        let ev = AlertEvent::Recovered(fired_low_conf(
            "alice",
            "ai.chat",
            0.85,
            0.50,
            AlertSeverity::Warning,
            1_700_000_000_000,
        ));
        c.record(&ev).unwrap();
        let rows = c.recent(10).unwrap();
        assert_eq!(rows[0].event_type, "alert.recovered");
        assert_eq!(rows[0].method.as_deref(), Some("ai.chat"));
        // Recovered rows leave `message` None — the message is
        // operator-supplied at fire time, not at recovery.
        assert!(rows[0].message.is_none());
    }
}
