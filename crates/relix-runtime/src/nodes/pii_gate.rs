//! RELIX-7.28 Part 3 — mesh-level PII detection gate.
//!
//! The gate wraps the dispatch bridge's inbound path. Every
//! `RequestEnvelope.args` byte string is scanned (UTF-8 lift) before the
//! bridge routes to a handler. The detector + anonymizer come from
//! [`crate::training::pii`] — we do NOT reimplement them.
//!
//! ## Actions
//!
//! - `block` — short-circuits the dispatch with `POLICY_DENIED`. The
//!   handler never sees the request.
//! - `redact` — rewrites `args` in place with redacted PII before the
//!   handler runs. The handler sees the redacted form.
//! - `log_only` — passes the request through unchanged but writes a
//!   `pii_event` row to the chronicle.
//!
//! All actions write a `pii_event` row when at least one PII span is
//! detected, so operators always have an audit trail.
//!
//! ## Storage
//!
//! Events are persisted in a `pii_events` table that lives next to the
//! metrics SQLite store (same file). The schema is append-only; rows are
//! never updated or deleted by the runtime.
//!
//! ## Cost
//!
//! When `MeshPiiConfig::enabled = false`, the bridge wires no gate and
//! the dispatch path stays byte-for-byte pre-7.28. When enabled, the gate
//! pays a single UTF-8 lift + regex scan on the inbound args.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::training::pii::{PiiAnonymizer, PiiConfig, PiiDetector, PiiSpan, PiiStrategy, PiiType};

/// Action the gate takes when PII is detected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeshPiiAction {
    /// Short-circuit with `POLICY_DENIED`. Hard refusal — the handler
    /// never sees the request.
    Block,
    /// Rewrite the args in place with PII replaced before invoking the
    /// handler.
    #[default]
    Redact,
    /// Pass the request through unchanged but record a `pii_event`.
    LogOnly,
}

impl MeshPiiAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Redact => "redact",
            Self::LogOnly => "log_only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "block" => Some(Self::Block),
            "redact" => Some(Self::Redact),
            "log_only" | "log-only" | "logonly" => Some(Self::LogOnly),
            _ => None,
        }
    }
}

/// `[mesh_pii]` configuration block. Absent / `enabled = false` keeps
/// the bridge in pre-7.28 mode (zero scanning overhead).
#[derive(Clone, Debug, Deserialize)]
pub struct MeshPiiConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub action: MeshPiiAction,
    /// Scan inbound request args. Default true.
    #[serde(default = "default_scan_args")]
    pub scan_args: bool,
    /// Scan outbound response bodies. Off by default — every outbound
    /// frame would pay UTF-8 lift + regex cost, which is hot for SSE /
    /// streaming endpoints. Operators with compliance requirements flip
    /// this on explicitly.
    #[serde(default)]
    pub scan_responses: bool,
    /// Methods exempt from scanning (e.g. memory writes that store
    /// intentionally-personal data).
    #[serde(default)]
    pub exempt_methods: Vec<String>,
    /// Optional sqlite path for the PII chronicle. When unset the
    /// runtime drops the table inside the metrics SQLite file.
    #[serde(default)]
    pub chronicle_path: Option<PathBuf>,
}

fn default_scan_args() -> bool {
    true
}

impl Default for MeshPiiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            action: MeshPiiAction::Redact,
            scan_args: true,
            scan_responses: false,
            exempt_methods: vec![],
            chronicle_path: None,
        }
    }
}

/// The gate's verdict on one inbound (or outbound) call. The bridge
/// inspects this to decide whether to short-circuit / mutate the args /
/// pass through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateOutcome {
    /// Hard refusal — `action = block`. `cause` is the human-readable
    /// reason recorded in the audit log.
    Blocked { cause: String },
    /// `args` (or response) was rewritten in place with PII replaced.
    Redacted,
    /// PII was detected but the configured action is `log_only` (or
    /// per-method exempt → fall through with no scan). The event was
    /// written.
    Logged,
}

#[derive(Debug, thiserror::Error)]
pub enum PiiGateError {
    #[error("pii gate io: {0}")]
    Io(String),
    #[error("pii gate sqlite: {0}")]
    Db(String),
    #[error("pii gate lock poisoned")]
    Lock,
}

impl From<rusqlite::Error> for PiiGateError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e.to_string())
    }
}

/// One persisted PII event row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PiiEventRow {
    pub request_id: String,
    pub agent: String,
    pub method: String,
    pub direction: String,
    pub action_taken: String,
    pub span_count: u32,
    pub recorded_at_ms: i64,
    /// Distinct PII types detected, joined by `,`. Stored so operators
    /// can aggregate "how often is EMAIL the trigger" without
    /// reconstructing the spans.
    pub types: String,
}

/// Aggregate counts from `pii.scan_stats`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PiiScanStats {
    pub window_hours: u32,
    pub total_events: u64,
    pub blocked: u64,
    pub redacted: u64,
    pub logged: u64,
    /// Top methods by frequency, descending. Bounded to 10 rows.
    pub top_methods: Vec<MethodFrequency>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MethodFrequency {
    pub method: String,
    pub count: u64,
}

/// The mesh PII gate. Cheap to clone — wraps the SQLite chronicle in
/// `Arc<Mutex<...>>` and the resolved config in immutable fields.
#[derive(Clone)]
pub struct MeshPiiGate {
    inner: Arc<GateInner>,
}

struct GateInner {
    config: MeshPiiConfig,
    detector: PiiDetector,
    anonymizer: PiiAnonymizer,
    chronicle: Arc<Mutex<Connection>>,
}

impl MeshPiiGate {
    /// Open the chronicle at the given path + build the gate. Returns
    /// `Ok(None)` when the config has `enabled = false`.
    pub fn from_config(cfg: MeshPiiConfig, path: &Path) -> Result<Option<Self>, PiiGateError> {
        if !cfg.enabled {
            return Ok(None);
        }
        let conn = Self::open_chronicle(path)?;
        // Build a strict PiiConfig that always redacts every type the
        // detector knows about — operators control the action knob
        // through `MeshPiiConfig::action`, not the per-type overrides.
        let pii_cfg = PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Redact,
            overrides: Default::default(),
        };
        let anonymizer = PiiAnonymizer::from_config(&pii_cfg);
        Ok(Some(Self {
            inner: Arc::new(GateInner {
                config: cfg,
                detector: PiiDetector,
                anonymizer,
                chronicle: Arc::new(Mutex::new(conn)),
            }),
        }))
    }

    /// Build an in-memory gate — used by unit tests.
    pub fn in_memory(cfg: MeshPiiConfig) -> Result<Self, PiiGateError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        let pii_cfg = PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Redact,
            overrides: Default::default(),
        };
        let anonymizer = PiiAnonymizer::from_config(&pii_cfg);
        Ok(Self {
            inner: Arc::new(GateInner {
                config: cfg,
                detector: PiiDetector,
                anonymizer,
                chronicle: Arc::new(Mutex::new(conn)),
            }),
        })
    }

    fn open_chronicle(path: &Path) -> Result<Connection, PiiGateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PiiGateError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(conn)
    }

    /// True iff the gate is active (config.enabled = true).
    pub fn is_enabled(&self) -> bool {
        self.inner.config.enabled
    }

    /// Configured action.
    pub fn action(&self) -> MeshPiiAction {
        self.inner.config.action
    }

    /// Scan inbound args. When `enabled = false` OR `scan_args = false`
    /// OR the method is exempt, the call returns `None` and the bridge
    /// keeps the original args unchanged.
    ///
    /// Mutating contract: on `redact`, `args_buf` is rewritten in place
    /// with the redacted text. On `block` or `log_only`, the buffer is
    /// left untouched.
    pub fn scan_inbound(
        &self,
        request_id: &str,
        agent: &str,
        method: &str,
        args_buf: &mut Vec<u8>,
    ) -> Option<GateOutcome> {
        if !self.inner.config.enabled || !self.inner.config.scan_args {
            return None;
        }
        if self.inner.config.exempt_methods.iter().any(|m| m == method) {
            return None;
        }
        self.scan_buf(request_id, agent, method, "inbound", args_buf)
    }

    /// Scan an outbound response body. Same semantics as `scan_inbound`
    /// except gated on `scan_responses`. Returns `None` when scanning is
    /// disabled.
    pub fn scan_outbound(
        &self,
        request_id: &str,
        agent: &str,
        method: &str,
        body_buf: &mut Vec<u8>,
    ) -> Option<GateOutcome> {
        if !self.inner.config.enabled || !self.inner.config.scan_responses {
            return None;
        }
        if self.inner.config.exempt_methods.iter().any(|m| m == method) {
            return None;
        }
        self.scan_buf(request_id, agent, method, "outbound", body_buf)
    }

    fn scan_buf(
        &self,
        request_id: &str,
        agent: &str,
        method: &str,
        direction: &str,
        buf: &mut Vec<u8>,
    ) -> Option<GateOutcome> {
        let text = match std::str::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => {
                // Non-UTF-8 args (e.g. CBOR-encoded structured payloads
                // that just happen to contain PII inside string fields).
                // Best-effort: detect on a lossy decode, but never mutate
                // a non-UTF-8 buffer — the handler must be able to
                // decode what it received.
                let lossy = String::from_utf8_lossy(buf);
                let spans = self.inner.detector.scan(&lossy);
                if spans.is_empty() {
                    return None;
                }
                let outcome = match self.inner.config.action {
                    MeshPiiAction::Block => GateOutcome::Blocked {
                        cause: build_cause(&spans),
                    },
                    // Lossy buffers can't be cleanly rewritten in place;
                    // we fall back to logging the detection.
                    _ => GateOutcome::Logged,
                };
                self.record_event(request_id, agent, method, direction, &outcome, &spans);
                return Some(outcome);
            }
        };
        let spans = self.inner.detector.scan(text);
        if spans.is_empty() {
            return None;
        }
        let outcome = match self.inner.config.action {
            MeshPiiAction::Block => GateOutcome::Blocked {
                cause: build_cause(&spans),
            },
            MeshPiiAction::Redact => {
                let redacted = self.inner.anonymizer.apply(text, &spans);
                *buf = redacted.into_bytes();
                GateOutcome::Redacted
            }
            MeshPiiAction::LogOnly => GateOutcome::Logged,
        };
        self.record_event(request_id, agent, method, direction, &outcome, &spans);
        Some(outcome)
    }

    fn record_event(
        &self,
        request_id: &str,
        agent: &str,
        method: &str,
        direction: &str,
        outcome: &GateOutcome,
        spans: &[PiiSpan],
    ) {
        let action_taken = match outcome {
            GateOutcome::Blocked { .. } => "blocked",
            GateOutcome::Redacted => "redacted",
            GateOutcome::Logged => "logged",
        };
        let types = distinct_types(spans);
        let now = unix_now_ms();
        let conn = match self.inner.chronicle.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Err(e) = conn.execute(
            "INSERT INTO pii_events \
             (request_id, agent, method, direction, action_taken, \
              span_count, recorded_at_ms, types) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request_id,
                agent,
                method,
                direction,
                action_taken,
                spans.len() as i64,
                now,
                types,
            ],
        ) {
            tracing::warn!(error = %e, "pii gate: chronicle write failed");
        }
    }

    /// `pii.recent_events` — pull the newest N rows, newest-first.
    /// Optional `method` filter narrows to one capability.
    pub fn recent_events(
        &self,
        limit: usize,
        method: Option<&str>,
    ) -> Result<Vec<PiiEventRow>, PiiGateError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self
            .inner
            .chronicle
            .lock()
            .map_err(|_| PiiGateError::Lock)?;
        let rows = match method {
            Some(m) => {
                let mut stmt = conn.prepare(
                    "SELECT request_id, agent, method, direction, action_taken, \
                            span_count, recorded_at_ms, types \
                     FROM pii_events WHERE method = ?1 \
                     ORDER BY recorded_at_ms DESC, id DESC LIMIT ?2",
                )?;
                let it = stmt.query_map(rusqlite::params![m, limit], row_from_sql)?;
                let mut v = Vec::new();
                for r in it {
                    v.push(r?);
                }
                v
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT request_id, agent, method, direction, action_taken, \
                            span_count, recorded_at_ms, types \
                     FROM pii_events \
                     ORDER BY recorded_at_ms DESC, id DESC LIMIT ?1",
                )?;
                let it = stmt.query_map([limit], row_from_sql)?;
                let mut v = Vec::new();
                for r in it {
                    v.push(r?);
                }
                v
            }
        };
        Ok(rows)
    }

    /// `pii.scan_stats` — count totals by action over the last `hours`
    /// window plus the top methods.
    pub fn scan_stats(&self, hours: u32) -> Result<PiiScanStats, PiiGateError> {
        let hours = hours.clamp(1, 24 * 90);
        let cutoff = unix_now_ms() - (hours as i64) * 3_600_000;
        let conn = self
            .inner
            .chronicle
            .lock()
            .map_err(|_| PiiGateError::Lock)?;
        let (total, blocked, redacted, logged): (i64, i64, i64, i64) = conn.query_row(
            "SELECT \
                COUNT(*), \
                COALESCE(SUM(CASE WHEN action_taken = 'blocked'  THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN action_taken = 'redacted' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN action_taken = 'logged'   THEN 1 ELSE 0 END), 0) \
             FROM pii_events WHERE recorded_at_ms >= ?1",
            [cutoff],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let mut stmt = conn.prepare(
            "SELECT method, COUNT(*) as n FROM pii_events \
             WHERE recorded_at_ms >= ?1 GROUP BY method \
             ORDER BY n DESC LIMIT 10",
        )?;
        let it = stmt.query_map([cutoff], |r| {
            Ok(MethodFrequency {
                method: r.get(0)?,
                count: r.get::<_, i64>(1)? as u64,
            })
        })?;
        let mut top_methods = Vec::new();
        for r in it {
            top_methods.push(r?);
        }
        Ok(PiiScanStats {
            window_hours: hours,
            total_events: total.max(0) as u64,
            blocked: blocked.max(0) as u64,
            redacted: redacted.max(0) as u64,
            logged: logged.max(0) as u64,
            top_methods,
        })
    }
}

fn row_from_sql(r: &rusqlite::Row) -> rusqlite::Result<PiiEventRow> {
    Ok(PiiEventRow {
        request_id: r.get(0)?,
        agent: r.get(1)?,
        method: r.get(2)?,
        direction: r.get(3)?,
        action_taken: r.get(4)?,
        span_count: r.get::<_, i64>(5)? as u32,
        recorded_at_ms: r.get(6)?,
        types: r.get(7)?,
    })
}

fn distinct_types(spans: &[PiiSpan]) -> String {
    let mut seen: std::collections::BTreeSet<PiiType> = std::collections::BTreeSet::new();
    for s in spans {
        seen.insert(s.pii_type);
    }
    seen.iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn build_cause(spans: &[PiiSpan]) -> String {
    let types = distinct_types(spans);
    format!(
        "blocked by mesh PII gate ({n} span(s); types={types})",
        n = spans.len()
    )
}

fn init_schema(conn: &Connection) -> Result<(), PiiGateError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pii_events (\
             id              INTEGER PRIMARY KEY AUTOINCREMENT,\
             request_id      TEXT NOT NULL,\
             agent           TEXT NOT NULL,\
             method          TEXT NOT NULL,\
             direction       TEXT NOT NULL,\
             action_taken    TEXT NOT NULL,\
             span_count      INTEGER NOT NULL,\
             recorded_at_ms  INTEGER NOT NULL,\
             types           TEXT NOT NULL DEFAULT ''\
         );\
         CREATE INDEX IF NOT EXISTS pii_events_recorded_at \
             ON pii_events(recorded_at_ms DESC);\
         CREATE INDEX IF NOT EXISTS pii_events_method_ts \
             ON pii_events(method, recorded_at_ms DESC);\
         CREATE INDEX IF NOT EXISTS pii_events_action_ts \
             ON pii_events(action_taken, recorded_at_ms DESC);",
    )?;
    // Pre-7.28 databases (none today but defensive against future
    // schema drift) pick up the `types` column on open.
    if !column_exists(conn, "pii_events", "types")? {
        conn.execute(
            "ALTER TABLE pii_events ADD COLUMN types TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, PiiGateError> {
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

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Default location for the PII chronicle beneath a data dir.
pub fn default_pii_chronicle_path(data_dir: &Path) -> PathBuf {
    data_dir.join("pii_events.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_with(action: MeshPiiAction) -> MeshPiiGate {
        MeshPiiGate::in_memory(MeshPiiConfig {
            enabled: true,
            action,
            scan_args: true,
            scan_responses: true,
            exempt_methods: vec![],
            chronicle_path: None,
        })
        .unwrap()
    }

    #[test]
    fn redact_rewrites_args_in_place() {
        let gate = gate_with(MeshPiiAction::Redact);
        let mut buf = b"contact me at jane@example.com please".to_vec();
        let outcome = gate
            .scan_inbound("req-1", "alice", "ai.chat", &mut buf)
            .expect("PII detected");
        assert_eq!(outcome, GateOutcome::Redacted);
        let after = String::from_utf8(buf).unwrap();
        assert!(
            after.contains("[EMAIL]"),
            "expected EMAIL placeholder: {after}"
        );
        assert!(!after.contains("jane@example.com"));
    }

    #[test]
    fn block_short_circuits_with_cause() {
        let gate = gate_with(MeshPiiAction::Block);
        let mut buf = b"hi my number is +1 415 555 0100".to_vec();
        let outcome = gate
            .scan_inbound("req-2", "alice", "ai.chat", &mut buf)
            .expect("PII detected");
        match outcome {
            GateOutcome::Blocked { cause } => assert!(cause.contains("PHONE")),
            other => panic!("expected Blocked, got {other:?}"),
        }
        // Buffer should not have been mutated.
        assert!(String::from_utf8_lossy(&buf).contains("+1 415"));
    }

    #[test]
    fn log_only_passes_call_through() {
        let gate = gate_with(MeshPiiAction::LogOnly);
        let mut buf = b"email jane@example.com please".to_vec();
        let outcome = gate
            .scan_inbound("req-3", "alice", "ai.chat", &mut buf)
            .expect("PII detected");
        assert_eq!(outcome, GateOutcome::Logged);
        // Buffer unchanged.
        assert!(String::from_utf8_lossy(&buf).contains("jane@example.com"));
    }

    #[test]
    fn clean_input_returns_none() {
        let gate = gate_with(MeshPiiAction::Redact);
        let mut buf = b"hello there, how is the day going?".to_vec();
        let outcome = gate.scan_inbound("req-4", "alice", "ai.chat", &mut buf);
        assert!(outcome.is_none());
    }

    #[test]
    fn exempt_method_is_not_scanned() {
        let gate = MeshPiiGate::in_memory(MeshPiiConfig {
            enabled: true,
            action: MeshPiiAction::Block,
            scan_args: true,
            scan_responses: false,
            exempt_methods: vec!["memory.write_turn".into()],
            chronicle_path: None,
        })
        .unwrap();
        let mut buf = b"my email is jane@example.com".to_vec();
        let outcome = gate.scan_inbound("req-5", "alice", "memory.write_turn", &mut buf);
        assert!(outcome.is_none(), "exempt method should bypass scan");
        assert!(String::from_utf8_lossy(&buf).contains("jane@example.com"));
    }

    #[test]
    fn scan_responses_off_means_outbound_skipped() {
        let gate = MeshPiiGate::in_memory(MeshPiiConfig {
            enabled: true,
            action: MeshPiiAction::Redact,
            scan_args: true,
            scan_responses: false,
            exempt_methods: vec![],
            chronicle_path: None,
        })
        .unwrap();
        let mut buf = b"response with jane@example.com".to_vec();
        let outcome = gate.scan_outbound("req-6", "alice", "ai.chat", &mut buf);
        assert!(outcome.is_none(), "outbound should not be scanned");
    }

    #[test]
    fn scan_responses_on_redacts_outbound() {
        let gate = gate_with(MeshPiiAction::Redact);
        let mut buf = b"response with jane@example.com here".to_vec();
        let outcome = gate
            .scan_outbound("req-7", "alice", "ai.chat", &mut buf)
            .expect("PII detected");
        assert_eq!(outcome, GateOutcome::Redacted);
        let after = String::from_utf8(buf).unwrap();
        assert!(after.contains("[EMAIL]"));
    }

    #[test]
    fn disabled_gate_does_not_scan() {
        let gate = MeshPiiGate::in_memory(MeshPiiConfig::default()).unwrap();
        assert!(!gate.is_enabled());
        let mut buf = b"jane@example.com".to_vec();
        let outcome = gate.scan_inbound("req-8", "alice", "ai.chat", &mut buf);
        assert!(outcome.is_none(), "disabled gate must not scan");
    }

    #[test]
    fn from_config_returns_none_when_disabled() {
        let cfg = MeshPiiConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pii.sqlite");
        let result = MeshPiiGate::from_config(cfg, &path).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn from_config_opens_chronicle_when_enabled() {
        let cfg = MeshPiiConfig {
            enabled: true,
            action: MeshPiiAction::Redact,
            scan_args: true,
            scan_responses: false,
            exempt_methods: vec![],
            chronicle_path: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pii.sqlite");
        let gate = MeshPiiGate::from_config(cfg, &path).unwrap().unwrap();
        assert!(gate.is_enabled());
        assert!(path.exists());
    }

    #[test]
    fn recent_events_returns_persisted_rows() {
        let gate = gate_with(MeshPiiAction::Redact);
        let mut buf = b"jane@example.com".to_vec();
        gate.scan_inbound("req-9", "alice", "ai.chat", &mut buf);
        let rows = gate.recent_events(10, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].method, "ai.chat");
        assert_eq!(rows[0].action_taken, "redacted");
        assert!(rows[0].types.contains("EMAIL"));
        assert!(rows[0].span_count >= 1);
    }

    #[test]
    fn recent_events_filters_by_method() {
        let gate = gate_with(MeshPiiAction::Redact);
        let mut a = b"jane@example.com".to_vec();
        let mut b = b"jane@example.com".to_vec();
        gate.scan_inbound("req-a", "alice", "ai.chat", &mut a);
        gate.scan_inbound("req-b", "alice", "tool.web", &mut b);
        let filtered = gate.recent_events(10, Some("ai.chat")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].method, "ai.chat");
    }

    #[test]
    fn scan_stats_returns_counts_by_action() {
        let gate = gate_with(MeshPiiAction::Redact);
        for i in 0..3 {
            let mut buf = format!("jane{i}@example.com").into_bytes();
            gate.scan_inbound(&format!("req-{i}"), "alice", "ai.chat", &mut buf);
        }
        let block_gate = gate_with(MeshPiiAction::Block);
        let mut buf = b"jane@example.com".to_vec();
        block_gate.scan_inbound("req-b", "alice", "ai.chat", &mut buf);

        let stats = gate.scan_stats(24).unwrap();
        assert_eq!(stats.window_hours, 24);
        assert_eq!(stats.total_events, 3);
        assert_eq!(stats.redacted, 3);
        assert_eq!(stats.blocked, 0);
        assert_eq!(stats.top_methods[0].method, "ai.chat");
        assert_eq!(stats.top_methods[0].count, 3);
    }

    #[test]
    fn pii_action_round_trip_through_strings() {
        for a in [
            MeshPiiAction::Block,
            MeshPiiAction::Redact,
            MeshPiiAction::LogOnly,
        ] {
            assert_eq!(MeshPiiAction::parse(a.as_str()), Some(a));
        }
        assert_eq!(
            MeshPiiAction::parse("log-only"),
            Some(MeshPiiAction::LogOnly)
        );
        assert_eq!(MeshPiiAction::parse("nope"), None);
    }

    #[test]
    fn parses_mesh_pii_config_from_toml() {
        let text = r#"
            enabled = true
            action = "redact"
            scan_args = true
            scan_responses = true
            exempt_methods = ["memory.write_turn", "training.export"]
        "#;
        let cfg: MeshPiiConfig = toml::from_str(text).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.action, MeshPiiAction::Redact);
        assert!(cfg.scan_responses);
        assert_eq!(cfg.exempt_methods.len(), 2);
    }
}
