//! RELIX-7.30 PART 1 — Out-of-Band Approval Delivery.
//!
//! The §7.30 spec calls for a configurable delivery matrix
//! that routes operator approval requests to the right
//! channel based on who is asking and what they are asking
//! for. This module implements that matrix end-to-end:
//!
//! - [`delivery::ApprovalDeliveryMatrix`] — the rule-table
//!   resolver. Operators configure `[approval.delivery]`
//!   rules; the matrix walks them top-to-bottom on each
//!   approval request and returns the matched channel +
//!   escalation policy.
//! - [`store::ApprovalRequestStore`] — SQLite-backed
//!   per-request state. Carries the wire-friendly columns
//!   (`delivery_channel`, `escalated`, `escalation_channel`,
//!   `delivered_at_ms`, `escalated_at_ms`) the spec
//!   mandates.
//! - [`delivery::ApprovalDeliveryService`] — ties the matrix +
//!   store + a `ChannelDispatch` trait together. On
//!   `dispatch_request` it picks the channel, persists the
//!   delivery row, and arms an escalation timer; on timer fire
//!   it persists an escalation row and dispatches the escalation
//!   channel.
//! - [`caps::register`] — wires `approval.delivery_status` onto
//!   the coordinator's `DispatchBridge` so the bridge endpoint
//!   + CLI can read the current delivery state.
//!
//! This is the GENERIC operator-approval surface — not to be
//! confused with the spec-driven plan-approval flow in
//! [`crate::planning::approval`], which approves planning
//! workflows specifically.

pub mod caps;
pub mod dashboard_dispatch;
pub mod delivery;
pub mod email_dispatch;
pub mod email_reply;
pub mod mirror;
pub mod multi_dispatch;
pub mod store;
pub mod token;

pub use dashboard_dispatch::DashboardChannelDispatch;
pub use delivery::{
    ApprovalDeliveryConfig, ApprovalDeliveryMatrix, ApprovalDeliveryService, ApprovalRequest,
    ChannelDispatch, ChannelDispatchError, ChannelKind, ChannelsConfig, DashboardChannelCfg,
    DecisionMirror, DeliveryOutcome, DeliveryRule, DiscordChannelCfg, EmailChannelCfg, RuleMatch,
    SingleChannelDispatch, SlackChannelCfg, TelegramChannelCfg,
};
pub use email_dispatch::{
    ApprovalEmailSender, EmailChannelDispatch, render_body as render_email_body,
    render_subject as render_email_subject,
};
pub use email_reply::{
    EmailProvider, EmailReplyAction, EmailReplyError, ParsedReply, SubjectDecision, lift_decision,
    parse_inbound_webhook, parse_subject_for_decision, verify_mailgun_signature,
};
pub use mirror::{ApprovalDeliveryServiceMirror, PlanningStoreMirror, wire_dual_write};
pub use multi_dispatch::{
    ApprovalSendArgs, MeshSingleChannelDispatch, MultiChannelDispatch, approval_send_method,
};
pub use store::{ApprovalDeliveryRow, ApprovalRequestStore, ApprovalStoreError};
pub use token::{
    ApprovalKeySet, ApprovalSigner, ApprovalToken, SIGNING_KEY_ENV, TOKEN_VERSION, TokenError,
    compute_fingerprint, signer_from_env,
};
