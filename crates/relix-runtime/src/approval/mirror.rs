//! PART 9 — `DecisionMirror` adapters that close the loop
//! between [`crate::planning::approval::ApprovalStore`] and
//! [`super::ApprovalDeliveryService`].
//!
//! Two systems, one shared `id` (plan_id ↔ approval_id). When a
//! decision lands in either store, the operator expects to see
//! it in both. These adapters wrap each side so the parent can
//! call `mirror_decision` after its own write succeeds without
//! a hard dependency on the other crate's API.
//!
//! Recursion is bounded by the only-flip-pending semantics of
//! the underlying writes:
//!
//! - [`crate::planning::approval::ApprovalStore::decide`] returns
//!   `NotPending` when the row is already decided. The error is
//!   logged + swallowed in the mirror, so re-entry stops on the
//!   second hop.
//! - [`super::ApprovalRequestStore::record_decision`] silently
//!   no-ops via SQL's `WHERE status = ?1` clause when the row
//!   has already been decided.

use std::sync::Arc;

use relix_core::approval::DecisionMirror;

use crate::planning::approval::{ApprovalError, ApprovalStatus, ApprovalStore as PlanningStore};

use super::delivery::ApprovalDeliveryService;

/// Wraps the generic [`ApprovalDeliveryService`] as a
/// [`DecisionMirror`] target. Installed on the planning store so
/// `planning.approve_plan` / `planning.reject_plan` also flip
/// the matching row in `approval_delivery`.
#[derive(Clone)]
pub struct ApprovalDeliveryServiceMirror {
    service: ApprovalDeliveryService,
}

impl ApprovalDeliveryServiceMirror {
    pub fn new(service: ApprovalDeliveryService) -> Self {
        Self { service }
    }
}

impl DecisionMirror for ApprovalDeliveryServiceMirror {
    fn mirror_decision(&self, id: &str, decision: &str, note: Option<&str>) {
        match self.service.record_decision(id, decision, note) {
            Ok(()) => {
                tracing::debug!(
                    id = %id,
                    decision = %decision,
                    "decision mirror: planning → delivery flip applied"
                );
            }
            Err(e) => {
                // Most common cause: no matching row exists in
                // the delivery store because the planning flow
                // did not dispatch through the generic surface.
                // That's an expected configuration today —
                // surface as DEBUG so operators aren't spammed.
                tracing::debug!(
                    id = %id,
                    decision = %decision,
                    error = %e,
                    "decision mirror: planning → delivery write skipped"
                );
            }
        }
    }
}

/// Wraps the planning [`PlanningStore`] as a [`DecisionMirror`]
/// target. Installed on the generic delivery service so
/// `approval.record_decision` also flips the matching row in
/// `plan_approvals`.
#[derive(Clone)]
pub struct PlanningStoreMirror {
    store: PlanningStore,
}

impl PlanningStoreMirror {
    pub fn new(store: PlanningStore) -> Self {
        Self { store }
    }
}

impl DecisionMirror for PlanningStoreMirror {
    fn mirror_decision(&self, id: &str, decision: &str, note: Option<&str>) {
        let new_status = match decision {
            "approved" => ApprovalStatus::Approved,
            "rejected" => ApprovalStatus::Rejected,
            "expired" => ApprovalStatus::Expired,
            other => {
                tracing::debug!(
                    id = %id,
                    decision = %other,
                    "decision mirror: delivery → planning ignored unknown decision token"
                );
                return;
            }
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match self.store.decide(id, new_status, note, now_ms) {
            Ok(_) => {
                tracing::debug!(
                    id = %id,
                    decision = %decision,
                    "decision mirror: delivery → planning flip applied"
                );
            }
            Err(ApprovalError::NotFound(_)) | Err(ApprovalError::NotPending { .. }) => {
                // Expected when (a) no planning approval was
                // ever opened under this id, or (b) the planning
                // side has already decided (re-entry case).
                tracing::debug!(
                    id = %id,
                    decision = %decision,
                    "decision mirror: delivery → planning no-op (not pending / not found)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    id = %id,
                    decision = %decision,
                    error = %e,
                    "decision mirror: delivery → planning write failed"
                );
            }
        }
    }
}

/// Wire both sides of the dual-write loop. The controller
/// startup calls this once it has both a delivery service and
/// a planning store; afterwards a decision recorded on either
/// side flips the other.
pub fn wire_dual_write(service: &ApprovalDeliveryService, planning: &PlanningStore) {
    let planning_mirror: Arc<dyn DecisionMirror> =
        Arc::new(PlanningStoreMirror::new(planning.clone()));
    service.install_decision_mirror(planning_mirror);

    let service_mirror: Arc<dyn DecisionMirror> =
        Arc::new(ApprovalDeliveryServiceMirror::new(service.clone()));
    planning.install_decision_mirror(service_mirror);

    tracing::info!("approval: PART 9 dual-write decision mirror wired");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::delivery::DeliveryError;
    use crate::approval::{
        ApprovalDeliveryConfig, ApprovalDeliveryMatrix, ApprovalDeliveryService, ApprovalRequest,
        ApprovalRequestStore, ChannelDispatch, ChannelKind, ChannelsConfig,
    };
    use crate::planning::PlanSpec;
    use crate::planning::approval::ApprovalRecord;
    use std::sync::Mutex;

    struct NoopDispatch;

    #[async_trait::async_trait]
    impl ChannelDispatch for NoopDispatch {
        async fn send(
            &self,
            _channel: ChannelKind,
            _cfg: &ChannelsConfig,
            _request: &ApprovalRequest,
            _is_escalation: bool,
        ) -> Result<(), DeliveryError> {
            Ok(())
        }
    }

    fn fixture_service() -> ApprovalDeliveryService {
        let cfg = ApprovalDeliveryConfig {
            default_channel: "dashboard".into(),
            rules: vec![],
            channels: ChannelsConfig::default(),
        };
        let matrix = ApprovalDeliveryMatrix::new(cfg);
        let store = ApprovalRequestStore::open_in_memory().unwrap();
        ApprovalDeliveryService::new(matrix, store, Arc::new(NoopDispatch))
    }

    fn fixture_planning_store() -> PlanningStore {
        PlanningStore::open_in_memory().unwrap()
    }

    fn fixture_plan_record(id: &str) -> ApprovalRecord {
        let spec = PlanSpec {
            spec_id: id.into(),
            goal: "test goal".into(),
            ..PlanSpec::default()
        };
        ApprovalRecord {
            plan_id: id.into(),
            spec,
            workflow_yaml: "steps: []".into(),
            status: ApprovalStatus::Pending,
            created_at_ms: 1_000,
            decided_at_ms: None,
            decision_note: None,
            orchestrator_meta: serde_json::Value::Null,
            critic_meta: serde_json::Value::Null,
        }
    }

    #[derive(Default)]
    struct RecordingMirror {
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl DecisionMirror for RecordingMirror {
        fn mirror_decision(&self, id: &str, decision: &str, note: Option<&str>) {
            self.seen
                .lock()
                .unwrap()
                .push((id.into(), decision.into(), note.map(Into::into)));
        }
    }

    #[tokio::test]
    async fn delivery_service_invokes_mirror_after_successful_record_decision() {
        let service = fixture_service();
        let mirror = Arc::new(RecordingMirror::default());
        service.install_decision_mirror(mirror.clone());
        // Seed a pending row so record_decision actually flips it.
        service
            .store()
            .upsert(&crate::approval::ApprovalDeliveryRow {
                approval_id: "a1".into(),
                agent_name: "alice".into(),
                capability: "tool.fs.write".into(),
                request_summary: "x".into(),
                session_id: "s".into(),
                status: "pending".into(),
                delivery_channel: "dashboard".into(),
                escalated: false,
                escalation_channel: None,
                delivered_at_ms: None,
                escalated_at_ms: None,
                decided_at_ms: None,
                decision: None,
                decision_note: None,
                delivery_error: None,
                authorized_approvers: Vec::new(),
            })
            .unwrap();
        service
            .record_decision("a1", "approved", Some("looks fine"))
            .unwrap();
        let seen = mirror.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "a1");
        assert_eq!(seen[0].1, "approved");
        assert_eq!(seen[0].2.as_deref(), Some("looks fine"));
    }

    #[test]
    fn planning_store_invokes_mirror_after_successful_decide() {
        let store = fixture_planning_store();
        let mirror = Arc::new(RecordingMirror::default());
        store.install_decision_mirror(mirror.clone());
        let now = 1_000;
        let _ = now;
        store.insert_pending(&fixture_plan_record("p1")).unwrap();
        store
            .decide("p1", ApprovalStatus::Approved, Some("ok"), 2_000)
            .unwrap();
        let seen = mirror.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "p1");
        assert_eq!(seen[0].1, "approved");
        assert_eq!(seen[0].2.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn dual_wire_flipping_delivery_also_flips_planning() {
        let service = fixture_service();
        let planning = fixture_planning_store();
        // Seed BOTH stores with a pending row sharing the same id.
        service
            .store()
            .upsert(&crate::approval::ApprovalDeliveryRow {
                approval_id: "shared".into(),
                agent_name: "alice".into(),
                capability: "tool.fs.write".into(),
                request_summary: "x".into(),
                session_id: "s".into(),
                status: "pending".into(),
                delivery_channel: "dashboard".into(),
                escalated: false,
                escalation_channel: None,
                delivered_at_ms: None,
                escalated_at_ms: None,
                decided_at_ms: None,
                decision: None,
                decision_note: None,
                delivery_error: None,
                authorized_approvers: Vec::new(),
            })
            .unwrap();
        planning
            .insert_pending(&fixture_plan_record("shared"))
            .unwrap();
        wire_dual_write(&service, &planning);
        service
            .record_decision("shared", "approved", Some("voted via delivery"))
            .unwrap();
        let row = planning.get("shared").unwrap().unwrap();
        assert_eq!(row.status, ApprovalStatus::Approved);
        assert_eq!(row.decision_note.as_deref(), Some("voted via delivery"));
    }

    #[tokio::test]
    async fn dual_wire_flipping_planning_also_flips_delivery() {
        let service = fixture_service();
        let planning = fixture_planning_store();
        service
            .store()
            .upsert(&crate::approval::ApprovalDeliveryRow {
                approval_id: "shared".into(),
                agent_name: "alice".into(),
                capability: "tool.fs.write".into(),
                request_summary: "x".into(),
                session_id: "s".into(),
                status: "pending".into(),
                delivery_channel: "dashboard".into(),
                escalated: false,
                escalation_channel: None,
                delivered_at_ms: None,
                escalated_at_ms: None,
                decided_at_ms: None,
                decision: None,
                decision_note: None,
                delivery_error: None,
                authorized_approvers: Vec::new(),
            })
            .unwrap();
        planning
            .insert_pending(&fixture_plan_record("shared"))
            .unwrap();
        wire_dual_write(&service, &planning);
        planning
            .decide(
                "shared",
                ApprovalStatus::Rejected,
                Some("voted via planning"),
                2_000,
            )
            .unwrap();
        let row = service.store().get("shared").unwrap().unwrap();
        assert_eq!(row.status, "rejected");
        assert_eq!(row.decision_note.as_deref(), Some("voted via planning"));
    }

    #[tokio::test]
    async fn dual_wire_reentry_stops_on_second_hop_when_both_already_decided() {
        // Verifies the safety claim in the module docstring:
        // once both rows are decided, neither side re-enters
        // the other recursively.
        let service = fixture_service();
        let planning = fixture_planning_store();
        service
            .store()
            .upsert(&crate::approval::ApprovalDeliveryRow {
                approval_id: "shared".into(),
                agent_name: "alice".into(),
                capability: "tool.fs.write".into(),
                request_summary: "x".into(),
                session_id: "s".into(),
                status: "pending".into(),
                delivery_channel: "dashboard".into(),
                escalated: false,
                escalation_channel: None,
                delivered_at_ms: None,
                escalated_at_ms: None,
                decided_at_ms: None,
                decision: None,
                decision_note: None,
                delivery_error: None,
                authorized_approvers: Vec::new(),
            })
            .unwrap();
        planning
            .insert_pending(&fixture_plan_record("shared"))
            .unwrap();
        wire_dual_write(&service, &planning);
        // First decision via delivery → planning mirror flips
        // planning → second-hop mirror call into delivery is a
        // no-op because delivery has already flipped its row.
        service
            .record_decision("shared", "approved", Some("once"))
            .unwrap();
        // If the loop hadn't stopped, this would have run
        // forever — reaching here at all proves termination.
        let s = service.store().get("shared").unwrap().unwrap();
        let p = planning.get("shared").unwrap().unwrap();
        assert_eq!(s.status, "approved");
        assert_eq!(p.status, ApprovalStatus::Approved);
    }
}
