//! RELIX-7.16 — typed chronicle events for the knowledge-
//! transfer surface.
//!
//! Every share / broadcast / revoke / auto-share event lands
//! on the coordinator's chronicle so operators can audit
//! exactly what knowledge moved between agents. The payload
//! is JSON-serialised and stored alongside the existing task
//! ledger via [`crate::nodes::coordinator::TaskStore`].
//!
//! The chronicle hook is intentionally `Option`-shaped: when
//! the coordinator hasn't wired a `TaskStore` (e.g. unit
//! tests), the service falls back to tracing-only and the
//! caller still sees a successful share. The audit trail is
//! best-effort by design — refusing the share because the
//! chronicle is unreachable would be worse than recording
//! the event in `tracing::info!` only.

use serde::{Deserialize, Serialize};

/// Discriminant for the chronicle event payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeEventKind {
    /// `knowledge.share` ran: one or more observations were
    /// copied from `source_agent` to `target_agent`.
    Shared,
    /// An auto-share tick propagated an observation.
    AutoShared,
    /// `knowledge.group_broadcast` propagated to a group.
    GroupBroadcast,
    /// `knowledge.revoke` invalidated a received copy.
    Revoked,
    /// A share / broadcast attempt was rejected by the
    /// trust checker. Recorded so operators can audit
    /// rejections without grepping logs.
    Rejected,
}

impl KnowledgeEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::AutoShared => "auto_shared",
            Self::GroupBroadcast => "group_broadcast",
            Self::Revoked => "revoked",
            Self::Rejected => "rejected",
        }
    }
}

/// One chronicle event payload. Serialises as
/// `{"event_kind": "...", "source_agent": "...", ...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeEvent {
    pub kind: KnowledgeEventKind,
    /// The agent that owned the source observation (`None`
    /// only for `Revoked` events where the operator drops a
    /// copy on the receiver — we still record who shared it
    /// originally if we can).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_agent: Option<String>,
    /// The agent that received the copy. `None` on
    /// `GroupBroadcast` envelopes where we record one event
    /// per (group, target) pair separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_agent: Option<String>,
    /// Optional sharing-group name when the share ran via a
    /// group path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub observation_ids: Vec<String>,
    /// Operator-supplied note attached to a `knowledge.share`
    /// call. `None` on `auto_shared` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Structured rejection reason. Populated only on
    /// `Rejected` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    /// Wall-clock unix-secs timestamp the event was recorded.
    pub recorded_at: i64,
}

impl KnowledgeEvent {
    pub fn shared(
        source: String,
        target: String,
        observation_ids: Vec<String>,
        message: Option<String>,
        group: Option<String>,
    ) -> Self {
        Self {
            kind: KnowledgeEventKind::Shared,
            source_agent: Some(source),
            target_agent: Some(target),
            group,
            observation_ids,
            message,
            rejection_reason: None,
            recorded_at: unix_now(),
        }
    }

    pub fn auto_shared(
        source: String,
        target: String,
        group: String,
        observation_ids: Vec<String>,
    ) -> Self {
        Self {
            kind: KnowledgeEventKind::AutoShared,
            source_agent: Some(source),
            target_agent: Some(target),
            group: Some(group),
            observation_ids,
            message: None,
            rejection_reason: None,
            recorded_at: unix_now(),
        }
    }

    pub fn revoked(
        source: Option<String>,
        target: Option<String>,
        observation_ids: Vec<String>,
    ) -> Self {
        Self {
            kind: KnowledgeEventKind::Revoked,
            source_agent: source,
            target_agent: target,
            group: None,
            observation_ids,
            message: None,
            rejection_reason: None,
            recorded_at: unix_now(),
        }
    }

    pub fn rejected(
        source: String,
        target: String,
        observation_ids: Vec<String>,
        reason: String,
        group: Option<String>,
    ) -> Self {
        Self {
            kind: KnowledgeEventKind::Rejected,
            source_agent: Some(source),
            target_agent: Some(target),
            group,
            observation_ids,
            message: None,
            rejection_reason: Some(reason),
            recorded_at: unix_now(),
        }
    }

    /// JSON representation suitable for stamping on a task
    /// event payload.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// String wire form used as the event_type column in the
    /// chronicle.
    pub fn event_type(&self) -> String {
        format!("knowledge.{}", self.kind.as_str())
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_event_has_kind_and_agents_populated() {
        let e = KnowledgeEvent::shared(
            "alice".into(),
            "bob".into(),
            vec!["obs-1".into()],
            Some("note".into()),
            Some("research".into()),
        );
        assert_eq!(e.kind, KnowledgeEventKind::Shared);
        assert_eq!(e.source_agent.as_deref(), Some("alice"));
        assert_eq!(e.target_agent.as_deref(), Some("bob"));
        assert_eq!(e.observation_ids, vec!["obs-1".to_string()]);
        assert_eq!(e.event_type(), "knowledge.shared");
        let v = e.to_json();
        assert_eq!(v["kind"], "shared");
        assert_eq!(v["source_agent"], "alice");
    }

    #[test]
    fn rejected_event_carries_structured_reason() {
        let e = KnowledgeEvent::rejected(
            "alice".into(),
            "carol".into(),
            vec!["obs-bad".into()],
            "not_in_shared_group".into(),
            None,
        );
        assert_eq!(e.kind, KnowledgeEventKind::Rejected);
        assert_eq!(e.rejection_reason.as_deref(), Some("not_in_shared_group"));
        assert_eq!(e.event_type(), "knowledge.rejected");
    }

    #[test]
    fn revoked_event_with_unknown_source_serialises_cleanly() {
        let e = KnowledgeEvent::revoked(None, Some("bob".into()), vec!["x".into()]);
        let v = e.to_json();
        // skip_serializing_if drops the missing source_agent.
        assert!(v.get("source_agent").is_none());
        assert_eq!(v["target_agent"], "bob");
    }
}
