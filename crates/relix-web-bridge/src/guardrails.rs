//! Bridge surface for the AI-side guardrails — currently the
//! multi-agent handoff audit ring.
//!
//! Architecture: every guardrail decision the AI controllers
//! make is logged at `tracing::info!` (the operator's existing
//! log pipeline catches them). When richer / dashboard-style
//! consumption is needed, callers POST the structured
//! [`HandoffAuditEvent`] to the bridge, which keeps the last
//! `HANDOFF_RING_CAP` of them in memory; the dashboard then
//! GETs `/v1/guardrails/handoffs` to render the audit view.
//!
//! No persistence — the ring resets on bridge restart. The
//! definitive audit trail is the tracing log; this ring is an
//! ergonomic surface for the dashboard, not a system of
//! record.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use relix_runtime::nodes::ai::guardrails::HandoffAuditEvent;
use serde::Serialize;

use crate::config::AppState;

/// In-memory ring capacity. Keeps the surface bounded so a
/// runaway audit producer can't blow up the bridge's memory.
pub const HANDOFF_RING_CAP: usize = 100;

/// Bridge-side audit ring. Cheap to clone (`Arc<Mutex<…>>`).
#[derive(Clone, Default)]
pub struct HandoffAuditRing {
    inner: Arc<Mutex<VecDeque<HandoffAuditEvent>>>,
}

impl HandoffAuditRing {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(HANDOFF_RING_CAP))),
        }
    }

    /// Push one event; drops the oldest when at capacity.
    pub fn push(&self, event: HandoffAuditEvent) {
        let mut buf = self.inner.lock().unwrap();
        if buf.len() >= HANDOFF_RING_CAP {
            buf.pop_front();
        }
        buf.push_back(event);
    }

    /// Snapshot the ring contents newest-first.
    pub fn snapshot(&self) -> Vec<HandoffAuditEvent> {
        let buf = self.inner.lock().unwrap();
        let mut out: Vec<HandoffAuditEvent> = buf.iter().cloned().collect();
        out.reverse();
        out
    }

    #[allow(dead_code)] // Used by tests; exposed for symmetry with `is_empty`.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct HandoffsResponse {
    pub events: Vec<HandoffAuditEvent>,
    pub count: usize,
}

type HandlerError = (StatusCode, Json<ApiError>);

pub(crate) fn handoffs_logic(ring: &HandoffAuditRing) -> HandoffsResponse {
    let events = ring.snapshot();
    HandoffsResponse {
        count: events.len(),
        events,
    }
}

pub(crate) fn record_logic(
    ring: &HandoffAuditRing,
    event: HandoffAuditEvent,
) -> Result<HandoffAuditEvent, HandlerError> {
    if event.sending_agent.trim().is_empty() || event.receiving_agent.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "sending_agent and receiving_agent must be non-empty".into(),
            }),
        ));
    }
    ring.push(event.clone());
    Ok(event)
}

/// `GET /v1/guardrails/handoffs` — newest-first snapshot of
/// the bridge's audit ring.
pub async fn handoffs(State(state): State<AppState>) -> Json<HandoffsResponse> {
    Json(handoffs_logic(&state.handoff_audit))
}

/// `POST /v1/guardrails/handoffs` — append a handoff audit
/// event to the ring. Body is the JSON form of
/// [`HandoffAuditEvent`].
pub async fn record(
    State(state): State<AppState>,
    Json(event): Json<HandoffAuditEvent>,
) -> Result<Json<HandoffAuditEvent>, HandlerError> {
    record_logic(&state.handoff_audit, event).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evt(send: &str, recv: &str, clean: bool) -> HandoffAuditEvent {
        HandoffAuditEvent {
            ts: 0,
            sending_agent: send.into(),
            receiving_agent: recv.into(),
            clean,
            injection_detected: !clean,
            scope_violation: false,
            reason: None,
        }
    }

    #[test]
    fn ring_caps_at_documented_size() {
        let ring = HandoffAuditRing::new();
        for i in 0..(HANDOFF_RING_CAP + 25) {
            ring.push(evt(&format!("a{i}"), "b", true));
        }
        assert_eq!(ring.len(), HANDOFF_RING_CAP);
        // Oldest entries were dropped.
        let snap = ring.snapshot();
        assert!(snap.iter().all(|e| !e.sending_agent.starts_with("a0,")));
        assert_eq!(
            snap[0].sending_agent,
            format!("a{}", HANDOFF_RING_CAP + 25 - 1)
        );
    }

    #[test]
    fn snapshot_is_newest_first() {
        let ring = HandoffAuditRing::new();
        ring.push(evt("a1", "b", true));
        ring.push(evt("a2", "b", true));
        ring.push(evt("a3", "b", true));
        let snap = ring.snapshot();
        assert_eq!(snap[0].sending_agent, "a3");
        assert_eq!(snap[2].sending_agent, "a1");
    }

    #[test]
    fn handoffs_logic_returns_snapshot() {
        let ring = HandoffAuditRing::new();
        ring.push(evt("alice", "bob", true));
        let resp = handoffs_logic(&ring);
        assert_eq!(resp.count, 1);
        assert_eq!(resp.events[0].sending_agent, "alice");
    }

    #[test]
    fn record_logic_rejects_empty_agent_names() {
        let ring = HandoffAuditRing::new();
        let err = record_logic(&ring, evt("", "bob", true)).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        let err = record_logic(&ring, evt("alice", "", true)).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(ring.is_empty());
    }

    #[test]
    fn record_logic_appends_and_returns_event() {
        let ring = HandoffAuditRing::new();
        let event = evt("alice", "bob", true);
        let echoed = record_logic(&ring, event).unwrap();
        assert_eq!(echoed.sending_agent, "alice");
        assert_eq!(ring.len(), 1);
    }
}
