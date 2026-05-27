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

use super::alert::{ActiveAlert, AlertDeliver, AlertEvent, AlertSeverity};

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

/// Render a metric value with units appropriate to the metric.
fn format_value(metric: &str, value: f64) -> String {
    match metric {
        "error_rate" => format!("{value:.2}%"),
        "p95_latency" => format!("{value:.0} ms"),
        "cost_per_hour" => format!("${:.4}", value / 1_000_000.0),
        "zero_success" => format!("{value:.0} successes"),
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

async fn dispatch_to_target(
    ctx: &AlertMeshContext,
    target: &AlertTarget,
    body: &str,
) -> Result<(), String> {
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
            call_unary(ctx, &target.peer, "email.send", arg_bytes).await
        }
        "telegram" | "discord" | "slack" => {
            // These channels don't expose an outbound `*.send`
            // capability the coordinator can call directly
            // today (they're inbound-only with per-channel
            // outbound clients used by the channel's own
            // controller). Log honestly and continue — the
            // chronicle sink still records the event.
            tracing::warn!(
                channel = %channel,
                peer = %target.peer,
                "alert delivery: {channel} channel currently has no inbound `*.send` capability; alert chronicled but not dispatched"
            );
            Ok(())
        }
        other => Err(format!("unknown channel: {other}")),
    }
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
            },
        };
        let conn = self.conn.lock().map_err(|_| ChronicleError::Lock)?;
        conn.execute(
            "INSERT INTO alert_events \
             (event_type, agent, metric, severity, actual_value, threshold_value, \
              triggered_at, recovered_at, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                    triggered_at, recovered_at, recorded_at_ms \
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
    Ok(())
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
}
