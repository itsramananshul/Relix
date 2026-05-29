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
pub mod delivery;
pub mod store;

pub use delivery::{
    ApprovalDeliveryConfig, ApprovalDeliveryMatrix, ApprovalDeliveryService, ApprovalRequest,
    ChannelDispatch, ChannelKind, ChannelsConfig, DashboardChannelCfg, DeliveryOutcome,
    DeliveryRule, EmailChannelCfg, RuleMatch, SlackChannelCfg, TelegramChannelCfg,
};
pub use store::{ApprovalDeliveryRow, ApprovalRequestStore, ApprovalStoreError};
