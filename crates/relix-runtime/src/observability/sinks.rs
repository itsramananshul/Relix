//! Two SQLite-backed sinks plus the `ObservabilityContext`
//! that ties them together.
//!
//! Both sinks use the project's standard SQLite hygiene
//! (`crate::db::apply_pragmas`): WAL, FK on, busy timeout.
//! The schemas are independent so a Sink-B prune doesn't
//! touch the metadata trail and vice versa.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::otel::OtelExporter;
use super::provenance::ProvenanceRegistry;

/// Errors raised by either sink.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("sqlite: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(String),
    #[error("lock poisoned")]
    Lock,
}

/// One Sink-A row. Pure metadata — no prompt / response /
/// tool-arg text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataEvent {
    pub event_id: String,
    pub session_id: String,
    pub agent_id: String,
    pub event_type: String,
    pub timestamp_unix: i64,
    pub latency_ms: Option<u64>,
    pub token_count: Option<u64>,
    pub cost_cents: Option<u32>,
    pub error_type: Option<String>,
    pub tool_name: Option<String>,
    pub model_name: Option<String>,
    pub success: bool,
}

/// One Sink-B row. Linked back to the Sink-A row by
/// `event_id`. The same `event_id` can have multiple
/// content rows (prompt + response + tool output …) keyed
/// by `content_type`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentEvent {
    pub event_id: String,
    pub content_type: String,
    pub content: String,
    pub redacted: bool,
    pub timestamp_unix: i64,
}

/// Metadata sink — long retention, safe to export.
pub struct MetadataSink {
    conn: Arc<Mutex<Connection>>,
}

impl MetadataSink {
    pub fn open(path: &Path) -> Result<Self, SinkError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SinkError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_metadata_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, SinkError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_metadata_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a metadata event. Replaces on `event_id`
    /// collision so callers can re-record without worrying
    /// about constraint errors.
    pub fn record(&self, event: &MetadataEvent) -> Result<(), SinkError> {
        self.record_for_tenant(event, "default")
    }

    /// GROUP 6: record attributed to the caller's VERIFIED tenant.
    pub fn record_for_tenant(
        &self,
        event: &MetadataEvent,
        tenant_id: &str,
    ) -> Result<(), SinkError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        conn.execute(
            "INSERT OR REPLACE INTO metadata_events \
             (event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
              token_count, cost_cents, error_type, tool_name, model_name, success, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event.event_id,
                event.session_id,
                event.agent_id,
                event.event_type,
                event.timestamp_unix,
                event.latency_ms.map(|v| v as i64),
                event.token_count.map(|v| v as i64),
                event.cost_cents.map(|v| v as i64),
                event.error_type,
                event.tool_name,
                event.model_name,
                event.success as i32,
                tenant,
            ],
        )?;
        Ok(())
    }

    /// GROUP 6: tenant-scoped read. Counts metadata events for
    /// `tenant` in `session_id` — a caller scoped to tenant A can
    /// never observe tenant B's session timeline even with B's
    /// shared `session_id`, because `tenant_id = ?` is enforced
    /// in SQL.
    pub fn count_for_tenant_and_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<u64, SinkError> {
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM metadata_events WHERE tenant_id = ?1 AND session_id = ?2",
            params![tenant, session_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// GROUP 6: tenant-scoped paginated query — the isolation-
    /// safe form of [`Self::query`]. EVERY row is filtered by the
    /// caller's VERIFIED tenant, so the derived `session_timeline`
    /// (which calls this) can never assemble another tenant's
    /// events even for a shared `session_id`.
    pub fn query_for_tenant(
        &self,
        tenant: &str,
        session_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetadataEvent>, SinkError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match session_id {
            None => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                        token_count, cost_cents, error_type, tool_name, model_name, success \
                 FROM metadata_events WHERE tenant_id = ?2 \
                 ORDER BY timestamp_unix DESC, event_id ASC LIMIT ?1",
                vec![limit.into(), tenant.to_string().into()],
            ),
            Some(s) => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                        token_count, cost_cents, error_type, tool_name, model_name, success \
                 FROM metadata_events WHERE tenant_id = ?2 AND session_id = ?3 \
                 ORDER BY timestamp_unix DESC, event_id ASC LIMIT ?1",
                vec![
                    limit.into(),
                    tenant.to_string().into(),
                    s.to_string().into(),
                ],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_metadata,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Paginated query. Ordered newest-first so dashboards
    /// don't have to re-sort.
    pub fn query(
        &self,
        session_id: Option<&str>,
        event_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MetadataEvent>, SinkError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match (session_id, event_type)
        {
            (None, None) => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                            token_count, cost_cents, error_type, tool_name, model_name, success \
                     FROM metadata_events \
                     ORDER BY timestamp_unix DESC, event_id ASC \
                     LIMIT ?1",
                vec![limit.into()],
            ),
            (Some(s), None) => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                            token_count, cost_cents, error_type, tool_name, model_name, success \
                     FROM metadata_events WHERE session_id = ?2 \
                     ORDER BY timestamp_unix DESC, event_id ASC \
                     LIMIT ?1",
                vec![limit.into(), s.to_string().into()],
            ),
            (None, Some(t)) => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                            token_count, cost_cents, error_type, tool_name, model_name, success \
                     FROM metadata_events WHERE event_type = ?2 \
                     ORDER BY timestamp_unix DESC, event_id ASC \
                     LIMIT ?1",
                vec![limit.into(), t.to_string().into()],
            ),
            (Some(s), Some(t)) => (
                "SELECT event_id, session_id, agent_id, event_type, timestamp_unix, latency_ms, \
                            token_count, cost_cents, error_type, tool_name, model_name, success \
                     FROM metadata_events WHERE session_id = ?2 AND event_type = ?3 \
                     ORDER BY timestamp_unix DESC, event_id ASC \
                     LIMIT ?1",
                vec![limit.into(), s.to_string().into(), t.to_string().into()],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter()),
            row_to_metadata,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Delete rows older than `days`. Returns the deletion
    /// count. Operator-facing surface for the long-retention
    /// trim.
    pub fn prune_older_than(&self, days: u32) -> Result<u64, SinkError> {
        let cutoff = unix_secs() - (days as i64) * 86_400;
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let n = conn.execute(
            "DELETE FROM metadata_events WHERE timestamp_unix < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    /// Enumerate distinct sessions. Returned tuple is
    /// `(session_id, agent_id, earliest_ts, latest_ts,
    /// event_count)`. Used by the session debugger's
    /// `list_sessions`.
    pub fn list_sessions_raw(&self) -> Result<Vec<SessionRow>, SinkError> {
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT session_id, agent_id, MIN(timestamp_unix), MAX(timestamp_unix), COUNT(*) \
             FROM metadata_events \
             GROUP BY session_id, agent_id \
             ORDER BY MAX(timestamp_unix) DESC, session_id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionRow {
                session_id: r.get::<_, String>(0)?,
                agent_id: r.get::<_, String>(1)?,
                started_at: r.get::<_, i64>(2)?,
                last_event_at: r.get::<_, i64>(3)?,
                event_count: r.get::<_, i64>(4)? as usize,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

/// Used by [`MetadataSink::list_sessions_raw`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub agent_id: String,
    pub started_at: i64,
    pub last_event_at: i64,
    pub event_count: usize,
}

/// Content sink — short retention, local only.
pub struct ContentSink {
    conn: Arc<Mutex<Connection>>,
    retention_days: u32,
}

impl ContentSink {
    pub fn open(path: &Path, retention_days: u32) -> Result<Self, SinkError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SinkError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_content_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            retention_days,
        })
    }

    pub fn in_memory(retention_days: u32) -> Result<Self, SinkError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_content_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            retention_days,
        })
    }

    pub fn record(&self, event: &ContentEvent) -> Result<(), SinkError> {
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        conn.execute(
            "INSERT OR REPLACE INTO content_events \
             (event_id, content_type, content, redacted, timestamp_unix) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.event_id,
                event.content_type,
                event.content,
                event.redacted as i32,
                event.timestamp_unix,
            ],
        )?;
        Ok(())
    }

    /// Get one content row by `event_id`. When multiple
    /// content rows share an event id (e.g. prompt +
    /// response), this returns the newest one.
    pub fn get(&self, event_id: &str) -> Result<Option<ContentEvent>, SinkError> {
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let row = conn
            .query_row(
                "SELECT event_id, content_type, content, redacted, timestamp_unix \
                 FROM content_events WHERE event_id = ?1 \
                 ORDER BY timestamp_unix DESC LIMIT 1",
                params![event_id],
                row_to_content,
            )
            .optional()?;
        Ok(row)
    }

    /// Delete every row past the retention window. Returns
    /// the deletion count.
    pub fn prune_expired(&self) -> Result<u64, SinkError> {
        let cutoff = unix_secs() - (self.retention_days as i64) * 86_400;
        let conn = self.conn.lock().map_err(|_| SinkError::Lock)?;
        let n = conn.execute(
            "DELETE FROM content_events WHERE timestamp_unix < ?1",
            params![cutoff],
        )?;
        Ok(n as u64)
    }

    pub fn retention_days(&self) -> u32 {
        self.retention_days
    }
}

/// Two-sink bundle plus the provenance registry. Cheap to
/// clone (three / four Arcs).
#[derive(Clone)]
pub struct ObservabilityContext {
    pub metadata: Arc<MetadataSink>,
    pub content: Arc<ContentSink>,
    pub provenance: Arc<ProvenanceRegistry>,
    /// W7: optional OTLP exporter. `Some(...)` when the
    /// `[observability.otel]` config block is enabled +
    /// pointed at an endpoint. `record_event` pushes the
    /// metadata row onto the exporter's buffer so the periodic
    /// flush task ships it as an OTLP span.
    pub otel: Option<Arc<OtelExporter>>,
}

impl ObservabilityContext {
    pub fn new(
        metadata: Arc<MetadataSink>,
        content: Arc<ContentSink>,
        provenance: Arc<ProvenanceRegistry>,
    ) -> Self {
        Self {
            metadata,
            content,
            provenance,
            otel: None,
        }
    }

    /// W7: attach an OTLP exporter to an existing context.
    /// Cheap; returns `Self` so callers can chain.
    pub fn with_otel(mut self, exporter: Arc<OtelExporter>) -> Self {
        self.otel = Some(exporter);
        self
    }

    /// In-memory triple for tests. Content retention is 7
    /// days to match the default config.
    pub fn in_memory() -> Self {
        Self {
            metadata: Arc::new(MetadataSink::in_memory().expect("in-memory metadata sink opens")),
            content: Arc::new(ContentSink::in_memory(7).expect("in-memory content sink opens")),
            provenance: Arc::new(
                ProvenanceRegistry::in_memory().expect("in-memory provenance registry opens"),
            ),
            otel: None,
        }
    }

    /// Record one event. Metadata always lands; content is
    /// optional and stored only when supplied. The two
    /// rows share `event_id` so callers can join later. When
    /// an OTel exporter is attached, the metadata row is also
    /// forwarded as a span — the exporter's own
    /// `enabled_events` whitelist decides what actually
    /// buffers; Sink B content is never forwarded.
    pub fn record_event(&self, meta: MetadataEvent, content: Option<ContentEvent>) {
        if let Err(e) = self.metadata.record(&meta) {
            tracing::warn!(error = %e, event_id = %meta.event_id, "observability: metadata record failed");
        }
        if let Some(otel) = self.otel.as_ref() {
            otel.record_event(&meta);
        }
        if let Some(c) = content
            && let Err(e) = self.content.record(&c)
        {
            tracing::warn!(error = %e, event_id = %c.event_id, "observability: content record failed");
        }
    }
}

fn init_metadata_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata_events (\
             event_id        TEXT PRIMARY KEY,\
             session_id      TEXT NOT NULL,\
             agent_id        TEXT NOT NULL,\
             event_type      TEXT NOT NULL,\
             timestamp_unix  INTEGER NOT NULL,\
             latency_ms      INTEGER,\
             token_count     INTEGER,\
             cost_cents      INTEGER,\
             error_type      TEXT,\
             tool_name       TEXT,\
             model_name      TEXT,\
             success         INTEGER NOT NULL,\
             tenant_id       TEXT NOT NULL DEFAULT 'default'\
         );\
         CREATE INDEX IF NOT EXISTS metadata_events_session_ts \
             ON metadata_events(session_id, timestamp_unix DESC);\
         CREATE INDEX IF NOT EXISTS metadata_events_event_type \
             ON metadata_events(event_type);\
         CREATE INDEX IF NOT EXISTS metadata_events_ts \
             ON metadata_events(timestamp_unix DESC);",
    )?;
    // GROUP 6: tenant isolation. The CREATE above carries
    // `tenant_id` for fresh DBs; existing pre-migration DBs need
    // the additive ALTER. Idempotent via the column probe;
    // pre-migration rows default to the reserved 'default' tenant.
    let mut has_tenant = false;
    {
        let mut stmt = conn.prepare("PRAGMA table_info(metadata_events)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for r in rows {
            if r? == "tenant_id" {
                has_tenant = true;
            }
        }
    }
    if !has_tenant {
        conn.execute_batch(
            "ALTER TABLE metadata_events ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';",
        )?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS metadata_events_tenant \
             ON metadata_events(tenant_id, session_id);",
    )?;
    Ok(())
}

fn init_content_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_events (\
             id              INTEGER PRIMARY KEY AUTOINCREMENT,\
             event_id        TEXT NOT NULL,\
             content_type    TEXT NOT NULL,\
             content         TEXT NOT NULL,\
             redacted        INTEGER NOT NULL DEFAULT 0,\
             timestamp_unix  INTEGER NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS content_events_event \
             ON content_events(event_id);\
         CREATE INDEX IF NOT EXISTS content_events_ts \
             ON content_events(timestamp_unix DESC);",
    )?;
    Ok(())
}

fn row_to_metadata(r: &rusqlite::Row<'_>) -> rusqlite::Result<MetadataEvent> {
    Ok(MetadataEvent {
        event_id: r.get(0)?,
        session_id: r.get(1)?,
        agent_id: r.get(2)?,
        event_type: r.get(3)?,
        timestamp_unix: r.get(4)?,
        latency_ms: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        token_count: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        cost_cents: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        error_type: r.get(8)?,
        tool_name: r.get(9)?,
        model_name: r.get(10)?,
        success: r.get::<_, i64>(11)? != 0,
    })
}

fn row_to_content(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContentEvent> {
    Ok(ContentEvent {
        event_id: r.get(0)?,
        content_type: r.get(1)?,
        content: r.get(2)?,
        redacted: r.get::<_, i64>(3)? != 0,
        timestamp_unix: r.get(4)?,
    })
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod group6_tenant_tests {
    use super::*;

    fn ev(id: &str, session: &str) -> MetadataEvent {
        MetadataEvent {
            event_id: id.to_string(),
            session_id: session.to_string(),
            agent_id: "a".to_string(),
            event_type: "step".to_string(),
            timestamp_unix: 1,
            latency_ms: None,
            token_count: None,
            cost_cents: None,
            error_type: None,
            tool_name: None,
            model_name: None,
            success: true,
        }
    }

    #[test]
    fn group6_observability_reads_are_isolated_by_verified_tenant() {
        // Two tenants emit events under the SAME session_id (the
        // cross-tenant shared key). A read scoped to tenant A must
        // see ONLY A's event in that session timeline.
        let sink = MetadataSink::in_memory().unwrap();
        sink.record_for_tenant(&ev("e-a", "shared-session"), "tenant-a")
            .unwrap();
        sink.record_for_tenant(&ev("e-b", "shared-session"), "tenant-b")
            .unwrap();
        assert_eq!(
            sink.count_for_tenant_and_session("tenant-a", "shared-session")
                .unwrap(),
            1,
            "tenant A must see only its own event in the shared session timeline"
        );
        assert_eq!(
            sink.count_for_tenant_and_session("tenant-b", "shared-session")
                .unwrap(),
            1
        );
        assert_eq!(
            sink.count_for_tenant_and_session("tenant-c", "shared-session")
                .unwrap(),
            0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta_event(event_id: &str, session: &str, event_type: &str, ts: i64) -> MetadataEvent {
        MetadataEvent {
            event_id: event_id.into(),
            session_id: session.into(),
            agent_id: "alice".into(),
            event_type: event_type.into(),
            timestamp_unix: ts,
            latency_ms: Some(42),
            token_count: Some(100),
            cost_cents: Some(5),
            error_type: None,
            tool_name: None,
            model_name: Some("gpt-4o-mini".into()),
            success: true,
        }
    }

    fn content_event(event_id: &str, ty: &str, content: &str, ts: i64) -> ContentEvent {
        ContentEvent {
            event_id: event_id.into(),
            content_type: ty.into(),
            content: content.into(),
            redacted: false,
            timestamp_unix: ts,
        }
    }

    #[test]
    fn metadata_sink_records_and_queries_round_trip() {
        let sink = MetadataSink::in_memory().unwrap();
        sink.record(&meta_event("e1", "s1", "model_call", 100))
            .unwrap();
        sink.record(&meta_event("e2", "s1", "tool_call", 200))
            .unwrap();
        sink.record(&meta_event("e3", "s2", "model_call", 150))
            .unwrap();
        let all = sink.query(None, None, 10).unwrap();
        assert_eq!(all.len(), 3);
        // Newest-first ordering.
        assert_eq!(all[0].event_id, "e2");
        // session filter.
        let s1 = sink.query(Some("s1"), None, 10).unwrap();
        assert_eq!(s1.len(), 2);
        assert!(s1.iter().all(|e| e.session_id == "s1"));
        // event_type filter.
        let model = sink.query(None, Some("model_call"), 10).unwrap();
        assert_eq!(model.len(), 2);
    }

    #[test]
    fn metadata_query_filters_by_session_and_type_together() {
        let sink = MetadataSink::in_memory().unwrap();
        sink.record(&meta_event("e1", "s1", "model_call", 100))
            .unwrap();
        sink.record(&meta_event("e2", "s1", "tool_call", 200))
            .unwrap();
        sink.record(&meta_event("e3", "s2", "model_call", 150))
            .unwrap();
        let hits = sink.query(Some("s1"), Some("tool_call"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event_id, "e2");
    }

    #[test]
    fn metadata_prune_older_than_removes_rows() {
        let sink = MetadataSink::in_memory().unwrap();
        // Insert a row with a very old timestamp so the
        // 1-day window prunes it.
        let now = unix_secs();
        let old = meta_event("old", "s1", "model_call", now - 30 * 86_400);
        let fresh = meta_event("fresh", "s1", "model_call", now);
        sink.record(&old).unwrap();
        sink.record(&fresh).unwrap();
        let n = sink.prune_older_than(7).unwrap();
        assert_eq!(n, 1);
        let remaining = sink.query(None, None, 10).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].event_id, "fresh");
    }

    #[test]
    fn content_sink_records_and_retrieves_by_id() {
        let sink = ContentSink::in_memory(7).unwrap();
        sink.record(&content_event("e1", "prompt", "hello", 100))
            .unwrap();
        let got = sink.get("e1").unwrap().expect("present");
        assert_eq!(got.content, "hello");
        assert_eq!(got.content_type, "prompt");
        assert!(sink.get("missing").unwrap().is_none());
    }

    #[test]
    fn content_sink_returns_newest_row_per_event_id() {
        let sink = ContentSink::in_memory(7).unwrap();
        sink.record(&content_event("e1", "prompt", "earlier", 100))
            .unwrap();
        sink.record(&content_event("e1", "response", "later", 200))
            .unwrap();
        let got = sink.get("e1").unwrap().unwrap();
        assert_eq!(got.content, "later");
        assert_eq!(got.content_type, "response");
    }

    #[test]
    fn content_prune_expired_drops_old_rows() {
        let sink = ContentSink::in_memory(3).unwrap();
        let now = unix_secs();
        sink.record(&content_event(
            "old",
            "prompt",
            "ancient",
            now - 10 * 86_400,
        ))
        .unwrap();
        sink.record(&content_event("fresh", "prompt", "new", now))
            .unwrap();
        let n = sink.prune_expired().unwrap();
        assert_eq!(n, 1);
        assert!(sink.get("old").unwrap().is_none());
        assert!(sink.get("fresh").unwrap().is_some());
    }

    #[test]
    fn observability_context_records_to_both_sinks() {
        let ctx = ObservabilityContext::in_memory();
        let now = unix_secs();
        ctx.record_event(
            meta_event("e1", "s1", "model_call", now),
            Some(content_event("e1", "prompt", "hello", now)),
        );
        let meta = ctx.metadata.query(None, None, 10).unwrap();
        assert_eq!(meta.len(), 1);
        let content = ctx.content.get("e1").unwrap();
        assert!(content.is_some());
        assert_eq!(content.unwrap().content, "hello");
    }

    #[test]
    fn observability_context_records_metadata_only_when_no_content() {
        let ctx = ObservabilityContext::in_memory();
        let now = unix_secs();
        ctx.record_event(meta_event("e1", "s1", "session", now), None);
        assert_eq!(ctx.metadata.query(None, None, 10).unwrap().len(), 1);
        assert!(ctx.content.get("e1").unwrap().is_none());
    }

    #[test]
    fn list_sessions_raw_groups_by_session_id() {
        let sink = MetadataSink::in_memory().unwrap();
        sink.record(&meta_event("e1", "s1", "model_call", 100))
            .unwrap();
        sink.record(&meta_event("e2", "s1", "tool_call", 200))
            .unwrap();
        sink.record(&meta_event("e3", "s2", "model_call", 150))
            .unwrap();
        let sessions = sink.list_sessions_raw().unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest-last-event session comes first.
        assert_eq!(sessions[0].session_id, "s1");
        assert_eq!(sessions[0].event_count, 2);
        assert_eq!(sessions[1].session_id, "s2");
        assert_eq!(sessions[1].event_count, 1);
    }
}
