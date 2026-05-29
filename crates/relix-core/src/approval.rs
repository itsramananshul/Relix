//! Shared approval-delivery primitives.
//!
//! These types live in `relix-core` so the channel crates
//! (`relix-telegram`, `relix-discord`, `relix-slack`) can
//! implement the [`SingleChannelDispatch`] trait without
//! pulling in `relix-runtime` (which already depends on
//! them — the other direction would be a cycle).
//!
//! The end-to-end service ([`ApprovalDeliveryService`]),
//! SQLite-backed store, and rule-table matrix live in
//! `relix-runtime::approval`. The runtime re-exports the
//! types defined here so existing callers keep working.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Channel an approval message is dispatched on. Stored on
/// the row as the lowercase tag string so operators can
/// `SELECT * WHERE delivery_channel = 'slack'` without
/// joining tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    /// Telegram bot — long-poll or webhook delivery.
    Telegram,
    /// Slack workspace — Web API + Events / Interactivity inbound.
    Slack,
    /// Discord guild — Bot API + Gateway / Interactions inbound.
    Discord,
    /// SMTP / email — inbound parsed from provider webhooks.
    Email,
    /// Internal dashboard queue.
    Dashboard,
}

impl ChannelKind {
    /// Wire string used for SQL column comparisons.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Email => "email",
            Self::Dashboard => "dashboard",
        }
    }

    /// Parse the wire string back to a [`ChannelKind`]. Case-
    /// insensitive; whitespace is trimmed.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "telegram" => Some(Self::Telegram),
            "slack" => Some(Self::Slack),
            "discord" => Some(Self::Discord),
            "email" => Some(Self::Email),
            "dashboard" => Some(Self::Dashboard),
            _ => None,
        }
    }
}

/// `[approval.delivery.channels]` body. Each variant carries
/// channel-specific wire metadata; `enabled = false` (or the
/// section being absent) keeps the channel dormant.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ChannelsConfig {
    /// Telegram channel config. `None` = telegram dispatch
    /// disabled.
    #[serde(default)]
    pub telegram: Option<TelegramChannelCfg>,
    /// Slack channel config. `None` = slack dispatch
    /// disabled.
    #[serde(default)]
    pub slack: Option<SlackChannelCfg>,
    /// Discord channel config. `None` = discord dispatch
    /// disabled.
    #[serde(default)]
    pub discord: Option<DiscordChannelCfg>,
    /// Email channel config. `None` = email dispatch
    /// disabled.
    #[serde(default)]
    pub email: Option<EmailChannelCfg>,
    /// Dashboard channel config. `None` = dashboard dispatch
    /// keeps the bridge's built-in default-enabled behaviour.
    #[serde(default)]
    pub dashboard: Option<DashboardChannelCfg>,
}

/// `[approval.delivery.channels.telegram]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct TelegramChannelCfg {
    /// Operator-controlled feature flag. `false` keeps the
    /// channel dormant even if its credentials are set.
    #[serde(default)]
    pub enabled: bool,
    /// Numeric Telegram chat id (passed as a string so
    /// operator TOML stays readable). Required when
    /// `enabled = true`.
    #[serde(default)]
    pub chat_id: String,
    /// Optional peer alias to dispatch the message through.
    /// Defaults to `"telegram"`.
    #[serde(default = "default_peer_telegram")]
    pub peer: String,
}

fn default_peer_telegram() -> String {
    "telegram".into()
}

/// `[approval.delivery.channels.slack]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SlackChannelCfg {
    /// Operator-controlled feature flag.
    #[serde(default)]
    pub enabled: bool,
    /// Incoming-webhook URL used by `LogChannelDispatch`-style
    /// fallbacks. The production Slack dispatcher uses
    /// `chat.postMessage` directly with the bot token; this
    /// field stays for back-compat.
    #[serde(default)]
    pub webhook_url: String,
    /// Slack channel id (e.g. `C0123456789`). Required when
    /// `enabled = true`.
    #[serde(default)]
    pub channel_id: String,
    /// Operator-supplied signing secret used to verify the
    /// `x-slack-signature` HMAC on inbound interactivity
    /// payloads. Required when the operator wants buttons.
    #[serde(default)]
    pub signing_secret: String,
    /// Optional peer alias to dispatch the message through.
    #[serde(default = "default_peer_slack")]
    pub peer: String,
}

fn default_peer_slack() -> String {
    "slack".into()
}

/// `[approval.delivery.channels.discord]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct DiscordChannelCfg {
    /// Operator-controlled feature flag.
    #[serde(default)]
    pub enabled: bool,
    /// Discord channel id (numeric snowflake, as a string).
    #[serde(default)]
    pub channel_id: String,
    /// Operator-supplied Ed25519 public key used to verify
    /// inbound interaction signatures: the
    /// `X-Signature-Ed25519` + `X-Signature-Timestamp` pair
    /// Discord sends with every interaction payload. Required
    /// when the operator wants buttons.
    #[serde(default)]
    pub public_key_hex: String,
    /// Optional peer alias.
    #[serde(default = "default_peer_discord")]
    pub peer: String,
}

fn default_peer_discord() -> String {
    "discord".into()
}

/// `[approval.delivery.channels.email]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct EmailChannelCfg {
    /// Operator-controlled feature flag.
    #[serde(default)]
    pub enabled: bool,
    /// Recipient mailbox.
    #[serde(default)]
    pub to: String,
    /// Sender mailbox surfaced in the `From:` header.
    #[serde(default)]
    pub from: String,
    /// Reply-to address that operators send their `APPROVE`
    /// / `DENY` reply to. The bridge's `/v1/channels/email/reply`
    /// route reads inbound parses and routes the decision
    /// back to the delivery service.
    #[serde(default)]
    pub reply_to: String,
    /// Optional peer alias.
    #[serde(default = "default_peer_email")]
    pub peer: String,
}

fn default_peer_email() -> String {
    "email".into()
}

/// `[approval.delivery.channels.dashboard]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct DashboardChannelCfg {
    /// Operator-controlled feature flag. Defaults true at the
    /// matrix level when this section is absent — the
    /// dashboard is the always-on fallback channel.
    #[serde(default)]
    pub enabled: bool,
}

/// One approval request flowing into the delivery service.
/// Caller-supplied state; the service decorates it with the
/// resolver + persists it under `approval_id`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Stable identifier — used by the operator's response
    /// path to route their `approve` / `deny` reply back to
    /// the right pending row.
    pub approval_id: String,
    /// Friendly name of the agent that asked for approval.
    pub agent_name: String,
    /// Capability / method the agent asked to invoke.
    pub capability: String,
    /// Operator-readable summary of the request.
    pub request_summary: String,
    /// Originating session id — operators use this to
    /// correlate the request with the agent's conversation.
    pub session_id: String,
    /// SEC PART B: explicit allow-list of subjects authorised
    /// to record a decision on this approval (subject id hex
    /// per [`crate::types::NodeId::to_string`]). Empty list ⇒
    /// the dispatch cap falls back to the role-based check
    /// (an `operator` / `admin` role can decide); any non-empty
    /// list narrows the allowed approvers to that exact set.
    /// Defends against an agent that knows an `approval_id`
    /// approving its own request.
    #[serde(default)]
    pub authorized_approvers: Vec<String>,
}

/// Errors surfaced by a [`SingleChannelDispatch`]
/// implementation. The runtime's broader `DeliveryError`
/// converts from this so the service layer keeps its
/// existing error envelope.
#[derive(Debug, Error)]
pub enum ChannelDispatchError {
    /// Channel is wired but the operator disabled it (or the
    /// required credentials are missing). Dispatcher
    /// short-circuits with this when the call can't proceed.
    #[error("channel `{0}` is not enabled or not configured")]
    Disabled(String),
    /// Network / API failure from the channel's transport
    /// layer. Always already-retried by the underlying client
    /// when it is retryable.
    #[error("channel transport error: {0}")]
    Transport(String),
    /// Internal failure that doesn't fit the other variants
    /// (serialization mistake, missing template variable,
    /// etc.).
    #[error("channel internal error: {0}")]
    Other(String),
}

/// PART 9 — mirror trait for the decision-unification surface.
///
/// Relix has two parallel approval systems that need to stay in
/// lock-step: `planning::approval` (gates planning workflows
/// keyed by `plan_id`) and `runtime::approval` (generic operator
/// approvals keyed by `approval_id`). Operators get confused
/// when a decision recorded in one system doesn't show up in
/// the other. The dual-write contract closes that gap: after a
/// decision lands in the primary store, the parent calls
/// [`DecisionMirror::mirror_decision`] which writes the SAME
/// decision into the secondary store best-effort.
///
/// Recursion is avoided by the "only-flip-pending" semantics of
/// both backing stores — the mirror call on a non-pending row
/// is a no-op, so a → b → a re-entry short-circuits on the
/// second hop. Failures are NOT propagated to the primary
/// decision — operators see a one-shot WARN log and can fix the
/// systems-out-of-sync state via the standard reconciliation
/// caps (`approval.failed_deliveries`, `planning.list_approvals`).
pub trait DecisionMirror: Send + Sync {
    /// Best-effort mirror write. `id` is the shared identifier
    /// the two stores agree on (today: `plan_id == approval_id`
    /// for plan approvals). `decision` is the wire-string
    /// (`approved | rejected | expired`). `note` is the
    /// operator-supplied free-form note from the primary side.
    fn mirror_decision(&self, id: &str, decision: &str, note: Option<&str>);
}

/// Plumbing trait every per-channel dispatcher implements.
/// The runtime's multi-channel router calls into this when
/// it has resolved a [`ChannelKind`] to a concrete sink.
///
/// Implementors live in their respective channel crate:
///
/// - `relix-telegram::TelegramChannelDispatch`
/// - `relix-slack::SlackChannelDispatch`
/// - `relix-discord::DiscordChannelDispatch`
/// - `relix-runtime::approval::EmailChannelDispatch`
/// - `relix-runtime::approval::DashboardChannelDispatch`
#[async_trait]
pub trait SingleChannelDispatch: Send + Sync {
    /// Send `request` to this channel's operator audience.
    /// `is_escalation = true` means this is the second
    /// notification after the initial delivery timed out;
    /// implementations are expected to render an "ESCALATED"
    /// banner so operators see the urgency change.
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_kind_round_trips_via_serde() {
        for k in [
            ChannelKind::Telegram,
            ChannelKind::Slack,
            ChannelKind::Discord,
            ChannelKind::Email,
            ChannelKind::Dashboard,
        ] {
            let s = serde_json::to_string(&k).expect("serialize");
            let back: ChannelKind = serde_json::from_str(&s).expect("deserialize");
            assert_eq!(k, back);
        }
    }

    #[test]
    fn channel_kind_parse_is_case_insensitive() {
        assert_eq!(ChannelKind::parse("Telegram"), Some(ChannelKind::Telegram));
        assert_eq!(ChannelKind::parse("  SLACK  "), Some(ChannelKind::Slack));
        assert_eq!(ChannelKind::parse("discord"), Some(ChannelKind::Discord));
        assert_eq!(ChannelKind::parse("email"), Some(ChannelKind::Email));
        assert_eq!(
            ChannelKind::parse("DASHBOARD"),
            Some(ChannelKind::Dashboard)
        );
        assert_eq!(ChannelKind::parse(""), None);
        assert_eq!(ChannelKind::parse("sms"), None);
    }

    #[test]
    fn channel_kind_as_str_matches_wire_lowercase() {
        assert_eq!(ChannelKind::Telegram.as_str(), "telegram");
        assert_eq!(ChannelKind::Slack.as_str(), "slack");
        assert_eq!(ChannelKind::Discord.as_str(), "discord");
        assert_eq!(ChannelKind::Email.as_str(), "email");
        assert_eq!(ChannelKind::Dashboard.as_str(), "dashboard");
    }

    #[test]
    fn channels_config_defaults_are_all_none() {
        let cfg = ChannelsConfig::default();
        assert!(cfg.telegram.is_none());
        assert!(cfg.slack.is_none());
        assert!(cfg.discord.is_none());
        assert!(cfg.email.is_none());
        assert!(cfg.dashboard.is_none());
    }

    #[test]
    fn telegram_channel_cfg_defaults_peer_to_telegram() {
        let cfg: TelegramChannelCfg = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(cfg.peer, "telegram");
    }

    #[test]
    fn discord_channel_cfg_defaults_peer_to_discord() {
        let cfg: DiscordChannelCfg = serde_json::from_str(r#"{"enabled":true}"#).unwrap();
        assert_eq!(cfg.peer, "discord");
    }

    #[test]
    fn channel_dispatch_error_displays_each_variant() {
        let e = ChannelDispatchError::Disabled("telegram".into());
        assert_eq!(
            e.to_string(),
            "channel `telegram` is not enabled or not configured"
        );
        let e = ChannelDispatchError::Transport("HTTP 502".into());
        assert_eq!(e.to_string(), "channel transport error: HTTP 502");
        let e = ChannelDispatchError::Other("bad template".into());
        assert_eq!(e.to_string(), "channel internal error: bad template");
    }
}
