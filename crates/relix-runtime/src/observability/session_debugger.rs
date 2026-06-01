//! Session debugger — assembles a per-session timeline by
//! walking the metadata sink.
//!
//! The debugger is pure read-side: it touches the two sinks
//! but never mutates them. The dashboard / CLI uses it to
//! render the "what happened during this session" view, with
//! the per-event content fetched lazily through the elevated-
//! access endpoint.
//!
//! Stall detection is wall-clock-time based: when the most
//! recent event for a session is older than
//! [`STALL_WINDOW_SECS`] AND no `session` event with
//! event_type that marks a finish has landed, the session
//! is reported as `stalled` so operators can intervene.

use std::sync::Arc;

use serde::Serialize;

use super::sinks::{ContentSink, MetadataEvent, MetadataSink, SinkError};

/// Wall-clock window past which a still-running session is
/// flagged as stalled. Spec floor: 5 minutes.
pub const STALL_WINDOW_SECS: i64 = 300;

/// Operator-facing summary of one session.
#[derive(Clone, Debug, Serialize)]
pub struct SessionTimeline {
    pub session_id: String,
    pub agent_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub total_cost_cents: u32,
    pub total_tokens: u64,
    pub events: Vec<TimelineEvent>,
    pub stalled: bool,
    pub status: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TimelineEvent {
    pub event_id: String,
    pub event_type: String,
    pub timestamp_unix: i64,
    pub latency_ms: Option<u64>,
    pub success: bool,
    pub summary: String,
    pub has_content: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub agent_id: String,
    pub status: String,
    pub started_at: i64,
    pub event_count: usize,
}

/// Pure-read assembler. The two Arcs share the underlying
/// stores with the rest of the system; the debugger never
/// writes through either of them.
pub struct SessionDebugger {
    metadata: Arc<MetadataSink>,
    content: Arc<ContentSink>,
}

impl SessionDebugger {
    pub fn new(metadata: Arc<MetadataSink>, content: Arc<ContentSink>) -> Self {
        Self { metadata, content }
    }

    /// Assemble the full timeline for a session.
    /// Returns `Ok(None)` when no event exists for the
    /// session — the caller renders a "no such session" UI.
    pub fn session_timeline(&self, session_id: &str) -> Result<Option<SessionTimeline>, SinkError> {
        let events = self.metadata.query(Some(session_id), None, 1000)?;
        self.timeline_from_events(session_id, events)
    }

    /// GROUP 6: tenant-scoped timeline. Reads ONLY the verified
    /// tenant's events for `session_id`, so a caller scoped to
    /// tenant A can never assemble tenant B's session timeline
    /// even when supplying B's `session_id`.
    pub fn session_timeline_for_tenant(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Option<SessionTimeline>, SinkError> {
        let events = self
            .metadata
            .query_for_tenant(tenant, Some(session_id), 1000)?;
        self.timeline_from_events(session_id, events)
    }

    fn timeline_from_events(
        &self,
        session_id: &str,
        mut events: Vec<crate::observability::sinks::MetadataEvent>,
    ) -> Result<Option<SessionTimeline>, SinkError> {
        if events.is_empty() {
            return Ok(None);
        }
        // Newest-first from the sink; chronological reads
        // better in the dashboard so reverse here.
        events.reverse();
        let started_at = events[0].timestamp_unix;
        let last_ts = events[events.len() - 1].timestamp_unix;
        let agent_id = events[0].agent_id.clone();
        let ended_at = events
            .iter()
            .rev()
            .find(|e| e.event_type == "session" && e.success)
            .map(|e| e.timestamp_unix);
        // Without an explicit end event, fall back to the
        // last event we saw — operators reading a live
        // session still get a duration.
        let duration_ms = ended_at
            .or(Some(last_ts))
            .map(|end| ((end - started_at).max(0) as u64) * 1000);
        let total_cost_cents: u32 = events.iter().filter_map(|e| e.cost_cents).sum();
        let total_tokens: u64 = events.iter().filter_map(|e| e.token_count).sum();
        let now = unix_secs();
        let stalled = ended_at.is_none() && (now - last_ts) > STALL_WINDOW_SECS;
        let status = if ended_at.is_some() {
            "completed".to_string()
        } else if stalled {
            "stalled".to_string()
        } else {
            "running".to_string()
        };
        let timeline_events: Vec<TimelineEvent> = events
            .into_iter()
            .map(|e| timeline_event(&e, self.content.as_ref()))
            .collect();
        Ok(Some(SessionTimeline {
            session_id: session_id.to_string(),
            agent_id,
            started_at,
            ended_at,
            duration_ms,
            total_cost_cents,
            total_tokens,
            events: timeline_events,
            stalled,
            status,
        }))
    }

    /// List sessions, optionally filtered by status. The
    /// session-level status is derived the same way
    /// `session_timeline` derives it.
    pub fn list_sessions(
        &self,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSummary>, SinkError> {
        let rows = self.metadata.list_sessions_raw()?;
        let now = unix_secs();
        let mut out: Vec<SessionSummary> = Vec::new();
        for r in rows {
            // To know whether a session ended we need to
            // peek for a successful `session` event. The
            // sink already provides timestamps; one cheap
            // query per session keeps this O(N session
            // events) which is fine for operator scale.
            let session_events = self
                .metadata
                .query(Some(&r.session_id), Some("session"), 5)?;
            let ended = session_events.iter().any(|e| e.success);
            let stalled = !ended && (now - r.last_event_at) > STALL_WINDOW_SECS;
            let status = if ended {
                "completed".to_string()
            } else if stalled {
                "stalled".to_string()
            } else {
                "running".to_string()
            };
            if let Some(filter) = status_filter
                && filter != status
            {
                continue;
            }
            out.push(SessionSummary {
                session_id: r.session_id,
                agent_id: r.agent_id,
                status,
                started_at: r.started_at,
                event_count: r.event_count,
            });
            if out.len() >= limit.max(1) {
                break;
            }
        }
        Ok(out)
    }
}

fn timeline_event(e: &MetadataEvent, content: &ContentSink) -> TimelineEvent {
    let has_content = content
        .get(&e.event_id)
        .map(|o| o.is_some())
        .unwrap_or(false);
    let summary = render_summary(e);
    TimelineEvent {
        event_id: e.event_id.clone(),
        event_type: e.event_type.clone(),
        timestamp_unix: e.timestamp_unix,
        latency_ms: e.latency_ms,
        success: e.success,
        summary,
        has_content,
    }
}

fn render_summary(e: &MetadataEvent) -> String {
    let model = e.model_name.as_deref().unwrap_or("");
    let tool = e.tool_name.as_deref().unwrap_or("");
    let latency = e
        .latency_ms
        .map(|v| format!(" ({v}ms)"))
        .unwrap_or_default();
    let cost = e
        .cost_cents
        .map(|v| format!(" cost={v}c"))
        .unwrap_or_default();
    match e.event_type.as_str() {
        "model_call" => format!("model_call{latency}{cost} model={model}"),
        "tool_call" => format!("tool_call{latency} tool={tool}"),
        "memory_op" => format!("memory_op{latency}"),
        "approval" => "approval".to_string(),
        "session" => {
            if e.success {
                "session ended".to_string()
            } else {
                "session started".to_string()
            }
        }
        "error" => format!(
            "error{latency} kind={}",
            e.error_type.as_deref().unwrap_or("unknown")
        ),
        "cost" => format!("cost{cost}"),
        other => other.to_string(),
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
    use crate::observability::sinks::{ContentEvent, MetadataEvent};

    fn ctx() -> (Arc<MetadataSink>, Arc<ContentSink>) {
        (
            Arc::new(MetadataSink::in_memory().unwrap()),
            Arc::new(ContentSink::in_memory(7).unwrap()),
        )
    }

    fn evt(
        event_id: &str,
        session: &str,
        ty: &str,
        ts: i64,
        cost_cents: Option<u32>,
        tokens: Option<u64>,
    ) -> MetadataEvent {
        MetadataEvent {
            event_id: event_id.into(),
            session_id: session.into(),
            agent_id: "alice".into(),
            event_type: ty.into(),
            timestamp_unix: ts,
            latency_ms: Some(50),
            token_count: tokens,
            cost_cents,
            error_type: None,
            tool_name: None,
            model_name: Some("gpt-4o-mini".into()),
            success: true,
        }
    }

    #[test]
    fn group6_session_timeline_is_isolated_by_verified_tenant() {
        // Two tenants emit events under the SAME session_id. The
        // tenant-scoped timeline read must assemble ONLY the
        // calling tenant's events — never the other tenant's.
        let (meta, content) = ctx();
        meta.record_for_tenant(
            &evt("a", "shared", "model_call", 100, Some(1), Some(1)),
            "tenant-a",
        )
        .unwrap();
        meta.record_for_tenant(
            &evt("b", "shared", "model_call", 200, Some(1), Some(1)),
            "tenant-b",
        )
        .unwrap();
        let debugger = SessionDebugger::new(meta, content);
        let a = debugger
            .session_timeline_for_tenant("tenant-a", "shared")
            .unwrap()
            .unwrap();
        assert_eq!(
            a.events.len(),
            1,
            "tenant A sees only its own event in the shared session, never B's"
        );
        let b = debugger
            .session_timeline_for_tenant("tenant-b", "shared")
            .unwrap()
            .unwrap();
        assert_eq!(b.events.len(), 1);
        // A tenant with no events gets an empty timeline.
        assert!(
            debugger
                .session_timeline_for_tenant("tenant-c", "shared")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn timeline_assembles_chronological_events_with_totals() {
        let (meta, content) = ctx();
        meta.record(&evt("a", "s1", "model_call", 100, Some(3), Some(10)))
            .unwrap();
        meta.record(&evt("b", "s1", "tool_call", 150, Some(2), Some(5)))
            .unwrap();
        meta.record(&evt("c", "s1", "model_call", 200, Some(5), Some(20)))
            .unwrap();
        // Content row for the middle event.
        content
            .record(&ContentEvent {
                event_id: "b".into(),
                content_type: "tool_args".into(),
                content: "args".into(),
                redacted: false,
                timestamp_unix: 150,
            })
            .unwrap();
        let debugger = SessionDebugger::new(meta, content);
        let tl = debugger.session_timeline("s1").unwrap().unwrap();
        assert_eq!(tl.session_id, "s1");
        assert_eq!(tl.events.len(), 3);
        // Chronological order: oldest first.
        assert_eq!(tl.events[0].event_id, "a");
        assert_eq!(tl.events[2].event_id, "c");
        // Totals.
        assert_eq!(tl.total_cost_cents, 10);
        assert_eq!(tl.total_tokens, 35);
        // Content presence flag.
        assert!(tl.events[1].has_content);
        assert!(!tl.events[0].has_content);
    }

    #[test]
    fn stalled_session_detected_when_last_event_older_than_window() {
        let (meta, content) = ctx();
        let now = unix_secs();
        // Most recent event > 5 min ago AND no session-end
        // event — must be reported as stalled.
        meta.record(&evt("a", "s1", "model_call", now - 1000, Some(3), Some(10)))
            .unwrap();
        let debugger = SessionDebugger::new(meta, content);
        let tl = debugger.session_timeline("s1").unwrap().unwrap();
        assert!(tl.stalled, "session must be flagged stalled");
        assert_eq!(tl.status, "stalled");
        assert!(tl.ended_at.is_none());
    }

    #[test]
    fn session_marked_completed_when_session_end_event_seen() {
        let (meta, content) = ctx();
        let now = unix_secs();
        meta.record(&evt("a", "s1", "model_call", now - 600, Some(3), Some(10)))
            .unwrap();
        let mut end = evt("b", "s1", "session", now - 500, None, None);
        end.success = true;
        meta.record(&end).unwrap();
        let debugger = SessionDebugger::new(meta, content);
        let tl = debugger.session_timeline("s1").unwrap().unwrap();
        assert_eq!(tl.status, "completed");
        assert_eq!(tl.ended_at, Some(now - 500));
    }

    #[test]
    fn timeline_returns_none_for_unknown_session() {
        let (meta, content) = ctx();
        let debugger = SessionDebugger::new(meta, content);
        assert!(debugger.session_timeline("nope").unwrap().is_none());
    }

    #[test]
    fn list_sessions_filters_by_status() {
        let (meta, content) = ctx();
        let now = unix_secs();
        // s1: running (recent activity, no end event).
        meta.record(&evt("a", "s1", "model_call", now - 30, Some(1), None))
            .unwrap();
        // s2: completed (has session-end event).
        meta.record(&evt("c", "s2", "model_call", now - 60, Some(1), None))
            .unwrap();
        let mut end = evt("d", "s2", "session", now - 20, None, None);
        end.success = true;
        meta.record(&end).unwrap();
        // s3: stalled.
        meta.record(&evt(
            "e",
            "s3",
            "model_call",
            now - STALL_WINDOW_SECS - 100,
            Some(1),
            None,
        ))
        .unwrap();
        let debugger = SessionDebugger::new(meta, content);
        let completed = debugger.list_sessions(Some("completed"), 10).unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].session_id, "s2");
        let stalled = debugger.list_sessions(Some("stalled"), 10).unwrap();
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].session_id, "s3");
        let running = debugger.list_sessions(Some("running"), 10).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].session_id, "s1");
        // No filter → all three.
        let all = debugger.list_sessions(None, 10).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn list_sessions_respects_limit() {
        let (meta, content) = ctx();
        let now = unix_secs();
        for i in 0..5 {
            meta.record(&evt(
                &format!("e{i}"),
                &format!("s{i}"),
                "model_call",
                now - i64::from(i),
                Some(1),
                None,
            ))
            .unwrap();
        }
        let debugger = SessionDebugger::new(meta, content);
        let limited = debugger.list_sessions(None, 2).unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn render_summary_includes_model_and_tool_info() {
        let mut e = MetadataEvent {
            event_id: "x".into(),
            session_id: "s".into(),
            agent_id: "a".into(),
            event_type: "model_call".into(),
            timestamp_unix: 0,
            latency_ms: Some(123),
            token_count: None,
            cost_cents: Some(7),
            error_type: None,
            tool_name: None,
            model_name: Some("gpt-test".into()),
            success: true,
        };
        let s = render_summary(&e);
        assert!(s.contains("model_call"));
        assert!(s.contains("gpt-test"));
        assert!(s.contains("123ms"));
        assert!(s.contains("cost=7c"));
        // Tool variant.
        e.event_type = "tool_call".into();
        e.tool_name = Some("web.fetch".into());
        let s = render_summary(&e);
        assert!(s.contains("tool_call"));
        assert!(s.contains("web.fetch"));
    }
}
