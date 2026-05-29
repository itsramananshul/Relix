//! `DashboardChannelDispatch` — wire-real implementation of
//! [`SingleChannelDispatch`] for the always-on internal
//! dashboard channel.
//!
//! Unlike the external transports (telegram / slack / discord /
//! email), the dashboard does not have a separate "send" — the
//! row that [`super::ApprovalDeliveryService::dispatch_request`]
//! already wrote into the [`super::ApprovalRequestStore`] IS the
//! dashboard delivery surface. The bridge's
//! `GET /v1/approval/pending` endpoint queries the store directly
//! and the dashboard UI renders the pending rows from the
//! response.
//!
//! Implementing `SingleChannelDispatch` for the dashboard keeps
//! the channel matrix uniform: `MultiChannelDispatch` resolves
//! `ChannelKind::Dashboard` to this dispatcher exactly like it
//! resolves the external channels to their crates, and the
//! `delivered_at_ms` / failed-delivery accounting stays consistent
//! across every channel. The send body itself is a no-op that
//! returns Ok unless the operator explicitly disabled the
//! channel via `[approval.delivery.channels.dashboard] enabled =
//! false`, in which case it returns `Disabled("dashboard")` so
//! the service flips the row to `delivery_failed` with an
//! intelligible reason.

use async_trait::async_trait;

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

/// Wire-real dashboard dispatcher.
#[derive(Clone, Debug)]
pub struct DashboardChannelDispatch {
    enabled: bool,
}

impl DashboardChannelDispatch {
    /// Construct a new dispatcher. `enabled = false` makes
    /// every `send` short-circuit with `Disabled("dashboard")`
    /// so the matrix is honoured even when the operator
    /// disables the always-on channel.
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Construct a dispatcher in the default-on state.
    /// Equivalent to `Self::new(true)`. Spelt out so the
    /// controller startup can read "enabled by default" at the
    /// call site.
    pub fn enabled() -> Self {
        Self::new(true)
    }
}

impl Default for DashboardChannelDispatch {
    fn default() -> Self {
        Self::enabled()
    }
}

#[async_trait]
impl SingleChannelDispatch for DashboardChannelDispatch {
    async fn send(
        &self,
        _request: &ApprovalRequest,
        _is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        if !self.enabled {
            return Err(ChannelDispatchError::Disabled("dashboard".into()));
        }
        // No external transport — the store row already exists
        // (the service writes it before calling `dispatch.send`)
        // and the dashboard endpoint reads it directly. Returning
        // Ok here lets the service stamp `delivered_at_ms` so the
        // row reads as "delivered to dashboard" in the UI.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_request() -> ApprovalRequest {
        ApprovalRequest {
            approval_id: "a1".into(),
            agent_name: "alice".into(),
            capability: "tool.fs.write".into(),
            request_summary: "writes a sensitive file".into(),
            session_id: "sess1".into(),
            authorized_approvers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn enabled_dispatcher_returns_ok_on_send() {
        let d = DashboardChannelDispatch::enabled();
        d.send(&fixture_request(), false)
            .await
            .expect("send succeeds when enabled");
    }

    #[tokio::test]
    async fn enabled_dispatcher_returns_ok_on_escalation() {
        let d = DashboardChannelDispatch::enabled();
        d.send(&fixture_request(), true)
            .await
            .expect("escalation send succeeds when enabled");
    }

    #[tokio::test]
    async fn disabled_dispatcher_short_circuits_with_disabled_dashboard() {
        let d = DashboardChannelDispatch::new(false);
        let err = d.send(&fixture_request(), false).await.unwrap_err();
        match err {
            ChannelDispatchError::Disabled(name) => assert_eq!(name, "dashboard"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    #[test]
    fn default_is_enabled() {
        let d = DashboardChannelDispatch::default();
        assert!(d.enabled);
    }

    #[test]
    fn clone_preserves_enabled_state() {
        let d = DashboardChannelDispatch::new(false);
        let c = d.clone();
        assert!(!c.enabled);
        let d = DashboardChannelDispatch::new(true);
        let c = d.clone();
        assert!(c.enabled);
    }
}
