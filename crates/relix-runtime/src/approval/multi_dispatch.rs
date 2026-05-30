//! PART 8 — multi-channel approval dispatch + remote-channel
//! mesh adapter.
//!
//! The coordinator owns the [`super::ApprovalDeliveryService`]
//! but doesn't host the channel transports — telegram, slack,
//! discord and email run on separate channel nodes in a typical
//! deployment. This module bridges the two:
//!
//! - [`MultiChannelDispatch`] implements the wire-level
//!   `ChannelDispatch` trait by routing on [`ChannelKind`] to a
//!   per-channel [`SingleChannelDispatch`]. The dashboard slot is
//!   wired in-process (the store row IS the dashboard delivery
//!   surface — see [`super::DashboardChannelDispatch`]); the four
//!   remote slots are wired to mesh adapters that call the
//!   channel node's `<channel>.approval_send` cap.
//! - [`MeshSingleChannelDispatch`] is that adapter. It holds the
//!   shared [`AlertMeshCell`] used by the existing
//!   `MultiChannelAlertSink` so the operator does not have to
//!   wire a second mesh cell, plus the channel-specific peer
//!   alias + channel id / chat id the cap needs to dispatch.
//!
//! On the channel node, [`crate::nodes::telegram::register`] et
//! al register a `<channel>.approval_send` cap that deserializes
//! the [`ApprovalRequest`] + channel target id from the args and
//! invokes the channel's local [`SingleChannelDispatch`] (which
//! IS interactive — buttons/blocks/components — because the per-
//! channel crate owns the rich rendering).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

use crate::dispatch::{build_request, decode_response};
use crate::metrics::AlertMeshCell;
use crate::transport::envelope::ResponseResult;

use super::delivery::{ChannelDispatch, ChannelKind, ChannelsConfig, DeliveryError};

/// Outbound deadline applied to one cap call. Matches the value
/// the alert mesh sink uses (see
/// `crate::metrics::alert_delivery::SEND_DEADLINE_SECS`) — keeps
/// the two fan-out surfaces on the same operator-tunable budget.
const SEND_DEADLINE_SECS: i64 = 30;

/// Routes approval requests to per-[`ChannelKind`]
/// [`SingleChannelDispatch`] implementations. Cheap to clone.
#[derive(Clone, Default)]
pub struct MultiChannelDispatch {
    channels: HashMap<ChannelKind, Arc<dyn SingleChannelDispatch>>,
}

impl MultiChannelDispatch {
    /// Construct an empty dispatcher. Add per-channel slots via
    /// [`Self::with_channel`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a per-channel handler. Re-installing the same
    /// [`ChannelKind`] replaces the previous handler.
    pub fn with_channel(
        mut self,
        kind: ChannelKind,
        dispatch: Arc<dyn SingleChannelDispatch>,
    ) -> Self {
        self.channels.insert(kind, dispatch);
        self
    }

    /// True when the multi-dispatcher has at least one wired
    /// channel beyond the dashboard fallback. Used by the
    /// controller startup log so operators can verify they
    /// actually wired real channels.
    pub fn configured_channel_count(&self) -> usize {
        self.channels.len()
    }
}

#[async_trait]
impl ChannelDispatch for MultiChannelDispatch {
    async fn send(
        &self,
        channel: ChannelKind,
        _cfg: &ChannelsConfig,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), DeliveryError> {
        let Some(handler) = self.channels.get(&channel) else {
            return Err(DeliveryError::ChannelDisabled(format!(
                "{} (no dispatcher wired on the multi-channel router)",
                channel.as_str()
            )));
        };
        handler
            .send(request, is_escalation)
            .await
            .map_err(DeliveryError::from)
    }
}

/// Adapter that makes a remote channel node's
/// `<channel>.approval_send` cap look like a local
/// [`SingleChannelDispatch`].
///
/// On `send` it (a) reads the mesh + identity from the shared
/// [`AlertMeshCell`] — same cell the existing
/// `MultiChannelAlertSink` reads — (b) encodes the
/// [`ApprovalRequest`] + the channel target id as the cap's
/// JSON args, and (c) invokes the cap on the operator-configured
/// peer alias.
///
/// Returns [`ChannelDispatchError::Disabled`] when the mesh cell
/// is still empty (the cap was registered but the mesh client
/// hasn't been wired yet — this is the same fail-soft pattern
/// the alert sink uses). Any responder error or transport
/// failure surfaces as [`ChannelDispatchError::Transport`].
#[derive(Clone)]
pub struct MeshSingleChannelDispatch {
    cell: AlertMeshCell,
    peer: String,
    channel: ChannelKind,
    /// Channel-target id the cap expects. Telegram → numeric chat
    /// id as a string (the cap parses to i64). Slack → channel id
    /// (`C…`). Discord → snowflake. Email → recipient mailbox.
    target_id: String,
    /// Optional secondary id — only used by email today (the
    /// `Reply-To:` header). Other channels leave this empty.
    target_extra: String,
}

impl MeshSingleChannelDispatch {
    /// Construct a new mesh adapter. `peer` is the alias the
    /// coordinator dialed in `[peers]`; `target_id` is the chat
    /// / channel / recipient id; `target_extra` is the channel-
    /// specific extra field (today: email reply-to).
    pub fn new(
        cell: AlertMeshCell,
        peer: String,
        channel: ChannelKind,
        target_id: String,
        target_extra: String,
    ) -> Self {
        Self {
            cell,
            peer,
            channel,
            target_id,
            target_extra,
        }
    }
}

/// Wire shape of `<channel>.approval_send` cap args. Shared with
/// the channel-node cap handlers so the encode + decode stay in
/// lock-step.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalSendArgs {
    pub approval_id: String,
    pub agent_name: String,
    pub capability: String,
    #[serde(default)]
    pub request_summary: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub is_escalation: bool,
    /// Channel-target id. Telegram → numeric-as-string chat id;
    /// Slack → channel id `C…`; Discord → snowflake; Email →
    /// recipient mailbox.
    pub target_id: String,
    /// Channel-specific extra. Today only used by email (the
    /// `Reply-To:` header). Other channels ignore.
    #[serde(default)]
    pub target_extra: String,
    /// DEFERRED 2: explicit authorised-approver allow-list
    /// (subject id hex). Carried across the channel-node hop
    /// so the rich rendering layer can show the operator-facing
    /// card "approvers: X, Y" AND the lift back to an
    /// [`ApprovalRequest`] preserves the security boundary
    /// downstream caps depend on. Empty ⇒ role-based fallback
    /// (only `operator` / `admin` roles may decide).
    #[serde(default)]
    pub authorized_approvers: Vec<String>,
}

impl ApprovalSendArgs {
    /// Lift the deserialised args back into an [`ApprovalRequest`]
    /// — exposed for the channel-node cap handlers so the
    /// wire-shape contract has one source of truth.
    ///
    /// DEFERRED 2: `authorized_approvers` is preserved through
    /// the lift. Previously this field was zeroed out at the
    /// channel-node boundary, which would silently degrade
    /// "only ops can decide" to "anyone in operator role can
    /// decide" if a future feature ever consulted the field on
    /// the channel side.
    pub fn to_request(&self) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: self.approval_id.clone(),
            agent_name: self.agent_name.clone(),
            capability: self.capability.clone(),
            request_summary: self.request_summary.clone(),
            session_id: self.session_id.clone(),
            authorized_approvers: self.authorized_approvers.clone(),
        }
    }
}

/// Cap method name for one [`ChannelKind`]. Pulled out so the
/// encode + the channel-node cap registration agree.
pub fn approval_send_method(channel: ChannelKind) -> &'static str {
    match channel {
        ChannelKind::Telegram => "telegram.approval_send",
        ChannelKind::Slack => "slack.approval_send",
        ChannelKind::Discord => "discord.approval_send",
        ChannelKind::Email => "email.approval_send",
        ChannelKind::Dashboard => "approval.list_pending",
    }
}

#[async_trait]
impl SingleChannelDispatch for MeshSingleChannelDispatch {
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        let ctx = self.cell.get().cloned().ok_or_else(|| {
            ChannelDispatchError::Disabled(format!(
                "{}: mesh client not initialised; the approval mesh \
                 cell will populate post-startup",
                self.channel.as_str()
            ))
        })?;
        let args = ApprovalSendArgs {
            approval_id: request.approval_id.clone(),
            agent_name: request.agent_name.clone(),
            capability: request.capability.clone(),
            request_summary: request.request_summary.clone(),
            session_id: request.session_id.clone(),
            is_escalation,
            target_id: self.target_id.clone(),
            target_extra: self.target_extra.clone(),
            // DEFERRED 2: forward the authorised-approver
            // allow-list across the channel-node boundary.
            authorized_approvers: request.authorized_approvers.clone(),
        };
        let arg_bytes = serde_json::to_vec(&args).map_err(|e| {
            ChannelDispatchError::Other(format!("approval mesh dispatch: encode args: {e}"))
        })?;
        let method = approval_send_method(self.channel);
        let envelope = build_request(method, arg_bytes, ctx.identity.clone(), SEND_DEADLINE_SECS);
        let raw = tokio::time::timeout(
            Duration::from_secs(SEND_DEADLINE_SECS as u64 + 5),
            ctx.mesh.call(&self.peer, envelope),
        )
        .await
        .map_err(|_| ChannelDispatchError::Transport(format!("{method}: peer timeout")))?
        .map_err(|e| ChannelDispatchError::Transport(format!("{method}: {e}")))?;
        let resp = decode_response(&raw).map_err(|e| {
            ChannelDispatchError::Transport(format!("{method}: decode response: {e}"))
        })?;
        match resp.res {
            ResponseResult::Ok(_) => Ok(()),
            ResponseResult::Err(env) => Err(ChannelDispatchError::Transport(format!(
                "{method}: responder err kind={} cause={}",
                env.kind, env.cause
            ))),
            ResponseResult::StreamHandle(_) => Err(ChannelDispatchError::Transport(format!(
                "{method}: unexpected stream response"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

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

    #[derive(Default)]
    struct Recording {
        seen: Mutex<Vec<(bool, String)>>,
        fail_with: Mutex<Option<ChannelDispatchError>>,
    }

    #[async_trait]
    impl SingleChannelDispatch for Recording {
        async fn send(
            &self,
            request: &ApprovalRequest,
            is_escalation: bool,
        ) -> Result<(), ChannelDispatchError> {
            if let Some(err) = self.fail_with.lock().unwrap().take() {
                return Err(err);
            }
            self.seen
                .lock()
                .unwrap()
                .push((is_escalation, request.approval_id.clone()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn routes_per_channel_kind_to_installed_handler() {
        let dash = Arc::new(Recording::default());
        let tg = Arc::new(Recording::default());
        let multi = MultiChannelDispatch::new()
            .with_channel(ChannelKind::Dashboard, dash.clone())
            .with_channel(ChannelKind::Telegram, tg.clone());
        let cfg = ChannelsConfig::default();
        multi
            .send(ChannelKind::Dashboard, &cfg, &fixture_request(), false)
            .await
            .expect("dashboard send");
        multi
            .send(ChannelKind::Telegram, &cfg, &fixture_request(), true)
            .await
            .expect("telegram send");
        assert_eq!(
            dash.seen.lock().unwrap().clone(),
            vec![(false, "a1".into())]
        );
        assert_eq!(tg.seen.lock().unwrap().clone(), vec![(true, "a1".into())]);
    }

    #[tokio::test]
    async fn unwired_kind_surfaces_channel_disabled() {
        let multi = MultiChannelDispatch::new();
        let cfg = ChannelsConfig::default();
        let err = multi
            .send(ChannelKind::Slack, &cfg, &fixture_request(), false)
            .await
            .unwrap_err();
        match err {
            DeliveryError::ChannelDisabled(msg) => assert!(msg.contains("slack")),
            other => panic!("expected ChannelDisabled, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handler_transport_error_propagates_as_delivery_dispatch_error() {
        let rec = Arc::new(Recording::default());
        *rec.fail_with.lock().unwrap() = Some(ChannelDispatchError::Transport("HTTP 502".into()));
        let multi = MultiChannelDispatch::new().with_channel(ChannelKind::Telegram, rec.clone());
        let cfg = ChannelsConfig::default();
        let err = multi
            .send(ChannelKind::Telegram, &cfg, &fixture_request(), false)
            .await
            .unwrap_err();
        match err {
            DeliveryError::Dispatch(msg) => assert!(msg.contains("HTTP 502")),
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn configured_channel_count_counts_installed_slots() {
        let m = MultiChannelDispatch::new();
        assert_eq!(m.configured_channel_count(), 0);
        let m = m.with_channel(ChannelKind::Dashboard, Arc::new(Recording::default()));
        assert_eq!(m.configured_channel_count(), 1);
        let m = m.with_channel(ChannelKind::Slack, Arc::new(Recording::default()));
        assert_eq!(m.configured_channel_count(), 2);
        let m = m.with_channel(ChannelKind::Slack, Arc::new(Recording::default()));
        assert_eq!(m.configured_channel_count(), 2, "re-install replaces");
    }

    #[test]
    fn approval_send_method_covers_every_channel_kind() {
        assert_eq!(
            approval_send_method(ChannelKind::Telegram),
            "telegram.approval_send"
        );
        assert_eq!(
            approval_send_method(ChannelKind::Slack),
            "slack.approval_send"
        );
        assert_eq!(
            approval_send_method(ChannelKind::Discord),
            "discord.approval_send"
        );
        assert_eq!(
            approval_send_method(ChannelKind::Email),
            "email.approval_send"
        );
    }

    #[test]
    fn approval_send_args_to_request_preserves_fields() {
        let a = ApprovalSendArgs {
            approval_id: "id".into(),
            agent_name: "agent".into(),
            capability: "cap".into(),
            request_summary: "summary".into(),
            session_id: "sess".into(),
            is_escalation: true,
            target_id: "C0".into(),
            target_extra: "x".into(),
            authorized_approvers: vec!["subj-A".into(), "subj-B".into()],
        };
        let r = a.to_request();
        assert_eq!(r.approval_id, "id");
        assert_eq!(r.agent_name, "agent");
        assert_eq!(r.capability, "cap");
        assert_eq!(r.request_summary, "summary");
        assert_eq!(r.session_id, "sess");
        // DEFERRED 2: lifted ApprovalRequest carries the
        // approver list (was Vec::new() before the fix).
        assert_eq!(
            r.authorized_approvers,
            vec!["subj-A".to_string(), "subj-B".to_string()]
        );
    }

    #[test]
    fn approval_send_args_json_round_trips_with_default_fields() {
        let a = ApprovalSendArgs {
            approval_id: "id".into(),
            agent_name: "agent".into(),
            capability: "cap".into(),
            request_summary: String::new(),
            session_id: String::new(),
            is_escalation: false,
            target_id: "C0".into(),
            target_extra: String::new(),
            authorized_approvers: Vec::new(),
        };
        let bytes = serde_json::to_vec(&a).unwrap();
        let back: ApprovalSendArgs = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.approval_id, a.approval_id);
        assert_eq!(back.is_escalation, a.is_escalation);
        // DEFERRED 2: empty allow-list round-trips cleanly via
        // serde-default so older callers that don't set the
        // field continue to deserialise.
        assert!(back.authorized_approvers.is_empty());
    }

    #[tokio::test]
    async fn mesh_adapter_returns_disabled_when_cell_empty() {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let adapter = MeshSingleChannelDispatch::new(
            cell,
            "telegram".into(),
            ChannelKind::Telegram,
            "12345".into(),
            String::new(),
        );
        let err = adapter.send(&fixture_request(), false).await.unwrap_err();
        match err {
            ChannelDispatchError::Disabled(msg) => assert!(msg.contains("telegram")),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }
}
