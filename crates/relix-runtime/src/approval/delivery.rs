//! Approval delivery matrix + service.
//!
//! See [`super`] for the module-level overview.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::store::{ApprovalDeliveryRow, ApprovalRequestStore, ApprovalStoreError};

/// `[approval.delivery]` config block. Operators set
/// `default_channel` (the fallback when no rule matches) and
/// any number of `rules` evaluated top-to-bottom.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ApprovalDeliveryConfig {
    /// Channel to route to when no rule matches. Defaults to
    /// `"dashboard"` so deployments without any matching rule
    /// still surface the request to operators.
    #[serde(default = "default_default_channel")]
    pub default_channel: String,
    /// Per-rule routing. Empty = every request hits the
    /// `default_channel`.
    #[serde(default)]
    pub rules: Vec<DeliveryRule>,
    /// Per-channel wire-config (auth credentials, chat ids,
    /// webhook URLs, etc.). Absent channels stay disabled —
    /// the dispatcher logs a warning if a matching rule names
    /// a channel without a configured `[approval.delivery.channels.<name>]`.
    #[serde(default)]
    pub channels: ChannelsConfig,
}

fn default_default_channel() -> String {
    "dashboard".into()
}

/// One rule in the matrix. `agent_pattern` and `action_pattern`
/// support simple glob (`*` matches anything).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DeliveryRule {
    pub agent_pattern: String,
    pub action_pattern: String,
    pub channel: String,
    /// When set + non-zero, the dispatcher arms a timer for
    /// this many seconds and escalates to
    /// `escalation_channel` if no approval decision lands by
    /// then. `0` (the default) disables escalation for this
    /// rule.
    #[serde(default)]
    pub escalation_timeout_secs: u64,
    /// Channel to escalate to. Honoured only when
    /// `escalation_timeout_secs > 0`.
    #[serde(default)]
    pub escalation_channel: Option<String>,
}

/// `[approval.delivery.channels]` body. Each variant carries
/// channel-specific wire metadata; `enabled = false` (or the
/// section being absent) keeps the channel dormant.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ChannelsConfig {
    #[serde(default)]
    pub telegram: Option<TelegramChannelCfg>,
    #[serde(default)]
    pub slack: Option<SlackChannelCfg>,
    #[serde(default)]
    pub email: Option<EmailChannelCfg>,
    #[serde(default)]
    pub dashboard: Option<DashboardChannelCfg>,
}

/// `[approval.delivery.channels.telegram]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct TelegramChannelCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Chat id to post into. Numeric Telegram ids are passed
    /// as strings so operator TOML stays readable.
    #[serde(default)]
    pub chat_id: String,
    /// Optional peer alias to dispatch the message through.
    /// Defaults to `"telegram"` so single-controller
    /// deployments don't need to repeat the value.
    #[serde(default = "default_peer_telegram")]
    pub peer: String,
}

fn default_peer_telegram() -> String {
    "telegram".into()
}

/// `[approval.delivery.channels.slack]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct SlackChannelCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Incoming-webhook URL. Required when `enabled = true`;
    /// dispatcher logs a warning and drops the message
    /// otherwise.
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default = "default_peer_slack")]
    pub peer: String,
}

fn default_peer_slack() -> String {
    "slack".into()
}

/// `[approval.delivery.channels.email]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct EmailChannelCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Recipient mailbox. SMTP details come from the existing
    /// email channel config the operator already wired for the
    /// alert-delivery system.
    #[serde(default)]
    pub to: String,
    #[serde(default = "default_peer_email")]
    pub peer: String,
}

fn default_peer_email() -> String {
    "email".into()
}

/// `[approval.delivery.channels.dashboard]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct DashboardChannelCfg {
    #[serde(default)]
    pub enabled: bool,
}

/// Channel an approval message is dispatched on. Stored on
/// the row as the lowercase tag string so operators can
/// `SELECT * WHERE delivery_channel = 'slack'` without joining.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Telegram,
    Slack,
    Email,
    Dashboard,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Slack => "slack",
            Self::Email => "email",
            Self::Dashboard => "dashboard",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "telegram" => Some(Self::Telegram),
            "slack" => Some(Self::Slack),
            "email" => Some(Self::Email),
            "dashboard" => Some(Self::Dashboard),
            _ => None,
        }
    }
}

/// What the matrix decided for one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    /// 0-based index of the rule that matched, or `None`
    /// when the default_channel was used.
    pub rule_index: Option<usize>,
    pub channel: ChannelKind,
    /// `0` when the matched rule disables escalation OR the
    /// default channel was used.
    pub escalation_timeout_secs: u64,
    pub escalation_channel: Option<ChannelKind>,
}

/// One approval request flowing into the delivery service.
/// Caller-supplied state; the service decorates it with the
/// resolver + persists it under `approval_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub agent_name: String,
    pub capability: String,
    pub request_summary: String,
    pub session_id: String,
}

/// Errors surfaced by the dispatch service.
#[derive(Debug, Error)]
pub enum DeliveryError {
    #[error("approval delivery: store error: {0}")]
    Store(#[from] ApprovalStoreError),
    #[error("approval delivery: channel `{0}` is not enabled or not configured")]
    ChannelDisabled(String),
    #[error("approval delivery: channel dispatch failed: {0}")]
    Dispatch(String),
}

/// Outcome returned by `ApprovalDeliveryService::dispatch_request`.
/// Surfaces enough state for the cap response without re-
/// reading the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryOutcome {
    pub approval_id: String,
    pub delivery_channel: ChannelKind,
    pub escalation_scheduled: bool,
    pub escalation_channel: Option<ChannelKind>,
    pub escalation_timeout_secs: u64,
    pub delivered_at_ms: i64,
}

/// Pure rule-table resolver. Cheap to clone (one Arc).
#[derive(Clone)]
pub struct ApprovalDeliveryMatrix {
    cfg: Arc<ApprovalDeliveryConfig>,
}

impl ApprovalDeliveryMatrix {
    pub fn new(cfg: ApprovalDeliveryConfig) -> Self {
        Self { cfg: Arc::new(cfg) }
    }

    /// Snapshot the config for the cap surface.
    pub fn config(&self) -> &ApprovalDeliveryConfig {
        &self.cfg
    }

    /// Walk the rules top-to-bottom. First matching rule wins.
    /// When nothing matches, the default channel is used.
    pub fn resolve(&self, agent: &str, action: &str) -> RuleMatch {
        for (i, rule) in self.cfg.rules.iter().enumerate() {
            if glob_match(&rule.agent_pattern, agent) && glob_match(&rule.action_pattern, action) {
                let channel = ChannelKind::parse(&rule.channel).unwrap_or(ChannelKind::Dashboard);
                let escalation_channel = rule
                    .escalation_channel
                    .as_deref()
                    .and_then(ChannelKind::parse);
                return RuleMatch {
                    rule_index: Some(i),
                    channel,
                    escalation_timeout_secs: rule.escalation_timeout_secs,
                    escalation_channel,
                };
            }
        }
        let channel =
            ChannelKind::parse(&self.cfg.default_channel).unwrap_or(ChannelKind::Dashboard);
        RuleMatch {
            rule_index: None,
            channel,
            escalation_timeout_secs: 0,
            escalation_channel: None,
        }
    }

    /// `true` when the channel is enabled in the config.
    /// Disabled channels return a `DeliveryError::ChannelDisabled`
    /// at dispatch time so operators see the wire reason
    /// instead of silent drops.
    pub fn channel_enabled(&self, channel: ChannelKind) -> bool {
        match channel {
            ChannelKind::Telegram => self
                .cfg
                .channels
                .telegram
                .as_ref()
                .map(|c| c.enabled)
                .unwrap_or(false),
            ChannelKind::Slack => self
                .cfg
                .channels
                .slack
                .as_ref()
                .map(|c| c.enabled)
                .unwrap_or(false),
            ChannelKind::Email => self
                .cfg
                .channels
                .email
                .as_ref()
                .map(|c| c.enabled)
                .unwrap_or(false),
            ChannelKind::Dashboard => {
                // Dashboard is always available — it's just an
                // internal queue write. Operators disable it
                // explicitly via `enabled = false`.
                self.cfg
                    .channels
                    .dashboard
                    .as_ref()
                    .map(|c| c.enabled)
                    .unwrap_or(true)
            }
        }
    }
}

/// Simple glob match: `*` matches zero-or-more chars. Used by
/// the matrix for both `agent_pattern` and `action_pattern`.
/// Anchored at both ends so `tool.fs.*` does NOT match
/// `prefix.tool.fs.write`.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let mut pi = pattern.chars().peekable();
    let mut vi = value.chars().peekable();
    glob_inner(&mut pi, &mut vi)
}

fn glob_inner(
    pi: &mut std::iter::Peekable<std::str::Chars<'_>>,
    vi: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> bool {
    loop {
        match pi.peek().copied() {
            None => return vi.peek().is_none(),
            Some('*') => {
                pi.next();
                // Collapse runs of `*` so `**` doesn't blow up.
                while pi.peek() == Some(&'*') {
                    pi.next();
                }
                if pi.peek().is_none() {
                    return true;
                }
                loop {
                    let mut pi_clone = pi.clone();
                    let mut vi_clone = vi.clone();
                    if glob_inner(&mut pi_clone, &mut vi_clone) {
                        return true;
                    }
                    if vi.next().is_none() {
                        return false;
                    }
                }
            }
            Some(c) => match vi.next() {
                Some(v) if v == c => {
                    pi.next();
                }
                _ => return false,
            },
        }
    }
}

/// Plumbing trait the service calls to actually send the
/// formatted message. The default impl in tests is a recorder;
/// production deployments wire this to the existing
/// `MultiChannelAlertSink` or to per-channel `*.send` caps.
#[async_trait::async_trait]
pub trait ChannelDispatch: Send + Sync {
    async fn send(
        &self,
        channel: ChannelKind,
        cfg: &ChannelsConfig,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), DeliveryError>;
}

/// Logging-only dispatcher — emits a structured `tracing::info`
/// line per delivery and writes the row to the store. Used as
/// the default backend when operators haven't wired a real
/// channel sink yet, plus as the recorder in unit tests.
#[derive(Clone, Default)]
pub struct LogChannelDispatch;

#[async_trait::async_trait]
impl ChannelDispatch for LogChannelDispatch {
    async fn send(
        &self,
        channel: ChannelKind,
        _cfg: &ChannelsConfig,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), DeliveryError> {
        tracing::info!(
            channel = channel.as_str(),
            approval_id = %request.approval_id,
            agent = %request.agent_name,
            capability = %request.capability,
            escalation = is_escalation,
            "approval delivery: log-only dispatch"
        );
        Ok(())
    }
}

/// Service. Cheap to clone (a couple of Arcs).
#[derive(Clone)]
pub struct ApprovalDeliveryService {
    matrix: ApprovalDeliveryMatrix,
    store: ApprovalRequestStore,
    dispatch: Arc<dyn ChannelDispatch>,
}

impl ApprovalDeliveryService {
    pub fn new(
        matrix: ApprovalDeliveryMatrix,
        store: ApprovalRequestStore,
        dispatch: Arc<dyn ChannelDispatch>,
    ) -> Self {
        Self {
            matrix,
            store,
            dispatch,
        }
    }

    pub fn matrix(&self) -> &ApprovalDeliveryMatrix {
        &self.matrix
    }

    pub fn store(&self) -> &ApprovalRequestStore {
        &self.store
    }

    /// End-to-end dispatch:
    ///
    /// 1. Resolve the rule + channel via the matrix.
    /// 2. Persist a `pending` row in the store stamped with
    ///    the chosen channel + `delivered_at_ms = now`.
    /// 3. Call `ChannelDispatch::send` for the initial channel.
    /// 4. When the matched rule asks for escalation, spawn a
    ///    timer task that re-checks the row after
    ///    `escalation_timeout_secs`; if the row is still
    ///    `pending`, mark `escalated = 1`, stamp
    ///    `escalated_at_ms`, and call `ChannelDispatch::send`
    ///    on the escalation channel.
    pub async fn dispatch_request(
        &self,
        request: ApprovalRequest,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        let r = self
            .matrix
            .resolve(&request.agent_name, &request.capability);
        if !self.matrix.channel_enabled(r.channel) {
            return Err(DeliveryError::ChannelDisabled(
                r.channel.as_str().to_string(),
            ));
        }
        let now = unix_ms();
        let row = ApprovalDeliveryRow {
            approval_id: request.approval_id.clone(),
            agent_name: request.agent_name.clone(),
            capability: request.capability.clone(),
            request_summary: request.request_summary.clone(),
            session_id: request.session_id.clone(),
            status: "pending".into(),
            delivery_channel: r.channel.as_str().to_string(),
            escalated: false,
            escalation_channel: r.escalation_channel.map(|c| c.as_str().to_string()),
            delivered_at_ms: Some(now),
            escalated_at_ms: None,
            decided_at_ms: None,
            decision: None,
            decision_note: None,
        };
        self.store.upsert(&row)?;
        self.dispatch
            .send(r.channel, &self.matrix.cfg.channels, &request, false)
            .await?;
        let escalation_scheduled = r.escalation_timeout_secs > 0 && r.escalation_channel.is_some();
        if escalation_scheduled {
            let svc = self.clone();
            let req = request.clone();
            let timeout = Duration::from_secs(r.escalation_timeout_secs);
            let esc_channel = r.escalation_channel.expect("checked above");
            tokio::spawn(async move {
                tokio::time::sleep(timeout).await;
                if let Err(e) = svc.maybe_escalate(req, esc_channel).await {
                    tracing::warn!(error = %e, "approval delivery: escalation failed");
                }
            });
        }
        Ok(DeliveryOutcome {
            approval_id: request.approval_id,
            delivery_channel: r.channel,
            escalation_scheduled,
            escalation_channel: r.escalation_channel,
            escalation_timeout_secs: r.escalation_timeout_secs,
            delivered_at_ms: now,
        })
    }

    /// Record an operator decision. Called by the existing
    /// approval cap when the operator approves or rejects;
    /// the escalation timer reads this state to decide
    /// whether to fire.
    pub fn record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: Option<&str>,
    ) -> Result<(), DeliveryError> {
        let now = unix_ms();
        self.store
            .record_decision(approval_id, decision, note, now)?;
        Ok(())
    }

    async fn maybe_escalate(
        &self,
        request: ApprovalRequest,
        channel: ChannelKind,
    ) -> Result<(), DeliveryError> {
        let row = match self.store.get(&request.approval_id)? {
            Some(r) => r,
            None => return Ok(()),
        };
        if row.status != "pending" || row.escalated {
            return Ok(());
        }
        if !self.matrix.channel_enabled(channel) {
            tracing::warn!(
                channel = channel.as_str(),
                "approval delivery: escalation channel disabled; skipping"
            );
            return Ok(());
        }
        let now = unix_ms();
        self.store
            .mark_escalated(&request.approval_id, channel.as_str(), now)?;
        self.dispatch
            .send(channel, &self.matrix.cfg.channels, &request, true)
            .await?;
        Ok(())
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn glob_match_handles_star_anchors() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("tool.fs.*", "tool.fs.write"));
        assert!(glob_match("tool.fs.*", "tool.fs."));
        assert!(!glob_match("tool.fs.*", "prefix.tool.fs.write"));
        assert!(glob_match("finance_*", "finance_payments"));
        assert!(!glob_match("finance_*", "ops_team"));
        assert!(glob_match("tool.*.write", "tool.fs.write"));
        assert!(!glob_match("tool.*.write", "tool.fs.read"));
    }

    fn fixture_cfg() -> ApprovalDeliveryConfig {
        ApprovalDeliveryConfig {
            default_channel: "telegram".into(),
            rules: vec![
                DeliveryRule {
                    agent_pattern: "finance_*".into(),
                    action_pattern: "tool.stripe.*".into(),
                    channel: "slack".into(),
                    escalation_timeout_secs: 300,
                    escalation_channel: Some("email".into()),
                },
                DeliveryRule {
                    agent_pattern: "*".into(),
                    action_pattern: "tool.terminal.*".into(),
                    channel: "telegram".into(),
                    escalation_timeout_secs: 120,
                    escalation_channel: Some("slack".into()),
                },
            ],
            channels: ChannelsConfig {
                telegram: Some(TelegramChannelCfg {
                    enabled: true,
                    chat_id: "123".into(),
                    peer: "telegram".into(),
                }),
                slack: Some(SlackChannelCfg {
                    enabled: true,
                    webhook_url: "https://hooks.slack.com/x".into(),
                    peer: "slack".into(),
                }),
                email: Some(EmailChannelCfg {
                    enabled: true,
                    to: "ops@x.com".into(),
                    peer: "email".into(),
                }),
                dashboard: Some(DashboardChannelCfg { enabled: true }),
            },
        }
    }

    #[test]
    fn matrix_routes_finance_stripe_to_slack() {
        let m = ApprovalDeliveryMatrix::new(fixture_cfg());
        let r = m.resolve("finance_alice", "tool.stripe.charge");
        assert_eq!(r.channel, ChannelKind::Slack);
        assert_eq!(r.rule_index, Some(0));
        assert_eq!(r.escalation_channel, Some(ChannelKind::Email));
        assert_eq!(r.escalation_timeout_secs, 300);
    }

    #[test]
    fn matrix_routes_wildcard_terminal_to_telegram() {
        let m = ApprovalDeliveryMatrix::new(fixture_cfg());
        let r = m.resolve("ops_carol", "tool.terminal.run");
        assert_eq!(r.channel, ChannelKind::Telegram);
        assert_eq!(r.rule_index, Some(1));
        assert_eq!(r.escalation_channel, Some(ChannelKind::Slack));
    }

    #[test]
    fn matrix_first_matching_rule_wins() {
        // finance_* + tool.terminal.* should match rule 1, NOT
        // rule 0, because rule 0's action_pattern doesn't apply.
        let m = ApprovalDeliveryMatrix::new(fixture_cfg());
        let r = m.resolve("finance_alice", "tool.terminal.run");
        assert_eq!(r.rule_index, Some(1));
        assert_eq!(r.channel, ChannelKind::Telegram);
    }

    #[test]
    fn matrix_falls_back_to_default_channel_when_no_rule_matches() {
        let m = ApprovalDeliveryMatrix::new(fixture_cfg());
        let r = m.resolve("research_dave", "memory.bulk_export");
        assert_eq!(r.rule_index, None);
        assert_eq!(r.channel, ChannelKind::Telegram);
        assert_eq!(r.escalation_timeout_secs, 0);
        assert_eq!(r.escalation_channel, None);
    }

    #[test]
    fn matrix_default_channel_falls_back_to_dashboard_when_unparseable() {
        let mut cfg = fixture_cfg();
        cfg.rules.clear();
        cfg.default_channel = "garbage".into();
        let m = ApprovalDeliveryMatrix::new(cfg);
        let r = m.resolve("a", "b");
        assert_eq!(r.channel, ChannelKind::Dashboard);
    }

    #[test]
    fn channel_enabled_honours_per_channel_flag() {
        let mut cfg = fixture_cfg();
        cfg.channels.email = Some(EmailChannelCfg {
            enabled: false,
            ..Default::default()
        });
        let m = ApprovalDeliveryMatrix::new(cfg);
        assert!(!m.channel_enabled(ChannelKind::Email));
        assert!(m.channel_enabled(ChannelKind::Slack));
    }

    #[derive(Default)]
    struct RecordingDispatch {
        log: Mutex<Vec<(ChannelKind, String, bool)>>,
    }

    #[async_trait::async_trait]
    impl ChannelDispatch for RecordingDispatch {
        async fn send(
            &self,
            channel: ChannelKind,
            _cfg: &ChannelsConfig,
            request: &ApprovalRequest,
            is_escalation: bool,
        ) -> Result<(), DeliveryError> {
            self.log
                .lock()
                .unwrap()
                .push((channel, request.approval_id.clone(), is_escalation));
            Ok(())
        }
    }

    fn fresh_service(
        cfg: ApprovalDeliveryConfig,
    ) -> (ApprovalDeliveryService, Arc<RecordingDispatch>) {
        let matrix = ApprovalDeliveryMatrix::new(cfg);
        let store = ApprovalRequestStore::open_in_memory().expect("store");
        let dispatch = Arc::new(RecordingDispatch::default());
        let svc = ApprovalDeliveryService::new(matrix, store, dispatch.clone());
        (svc, dispatch)
    }

    fn fixture_request(id: &str, agent: &str, action: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: id.into(),
            agent_name: agent.into(),
            capability: action.into(),
            request_summary: "test".into(),
            session_id: "sess1".into(),
        }
    }

    #[tokio::test]
    async fn dispatch_request_persists_row_and_calls_initial_channel() {
        let (svc, log) = fresh_service(fixture_cfg());
        let req = fixture_request("a1", "finance_alice", "tool.stripe.charge");
        let outcome = svc.dispatch_request(req.clone()).await.unwrap();
        assert_eq!(outcome.delivery_channel, ChannelKind::Slack);
        assert_eq!(outcome.escalation_channel, Some(ChannelKind::Email));
        assert!(outcome.escalation_scheduled);
        let row = svc.store().get("a1").unwrap().unwrap();
        assert_eq!(row.delivery_channel, "slack");
        assert_eq!(row.escalation_channel.as_deref(), Some("email"));
        assert!(row.delivered_at_ms.is_some());
        assert_eq!(row.status, "pending");
        let log_snapshot = log.log.lock().unwrap().clone();
        assert_eq!(log_snapshot.len(), 1);
        assert_eq!(log_snapshot[0].0, ChannelKind::Slack);
        assert!(!log_snapshot[0].2);
    }

    #[tokio::test]
    async fn escalation_fires_after_timeout_when_not_decided() {
        let mut cfg = fixture_cfg();
        // Make escalation fire after 50ms so the test stays fast.
        cfg.rules[0].escalation_timeout_secs = 0;
        cfg.rules[1].escalation_timeout_secs = 0;
        cfg.rules.insert(
            0,
            DeliveryRule {
                agent_pattern: "*".into(),
                action_pattern: "fast_escalate.*".into(),
                channel: "telegram".into(),
                escalation_timeout_secs: 1, // 1 second is the minimum
                escalation_channel: Some("slack".into()),
            },
        );
        let (svc, log) = fresh_service(cfg);
        let req = fixture_request("e1", "ops", "fast_escalate.do");
        let outcome = svc.dispatch_request(req).await.unwrap();
        assert!(outcome.escalation_scheduled);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let row = svc.store().get("e1").unwrap().unwrap();
        assert!(row.escalated, "escalation timer should have fired: {row:?}");
        assert_eq!(row.escalation_channel.as_deref(), Some("slack"));
        assert!(row.escalated_at_ms.is_some());
        let log_snapshot = log.log.lock().unwrap().clone();
        assert_eq!(log_snapshot.len(), 2);
        assert_eq!(log_snapshot[1].0, ChannelKind::Slack);
        assert!(log_snapshot[1].2);
    }

    #[tokio::test]
    async fn escalation_skipped_when_decision_recorded_before_timer() {
        let mut cfg = fixture_cfg();
        cfg.rules.clear();
        cfg.rules.push(DeliveryRule {
            agent_pattern: "*".into(),
            action_pattern: "x.*".into(),
            channel: "telegram".into(),
            escalation_timeout_secs: 1,
            escalation_channel: Some("slack".into()),
        });
        let (svc, log) = fresh_service(cfg);
        let req = fixture_request("d1", "alice", "x.do");
        svc.dispatch_request(req).await.unwrap();
        svc.record_decision("d1", "approved", Some("ok")).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let row = svc.store().get("d1").unwrap().unwrap();
        assert!(
            !row.escalated,
            "should not escalate after decision: {row:?}"
        );
        assert_eq!(row.status, "approved");
        let log_snapshot = log.log.lock().unwrap().clone();
        assert_eq!(log_snapshot.len(), 1, "only initial dispatch should fire");
    }

    #[tokio::test]
    async fn dispatch_rejects_when_channel_disabled() {
        let mut cfg = fixture_cfg();
        cfg.channels.slack = Some(SlackChannelCfg {
            enabled: false,
            ..Default::default()
        });
        let (svc, _) = fresh_service(cfg);
        let req = fixture_request("x1", "finance_alice", "tool.stripe.charge");
        let err = svc.dispatch_request(req).await.unwrap_err();
        match err {
            DeliveryError::ChannelDisabled(c) => assert_eq!(c, "slack"),
            other => panic!("expected ChannelDisabled, got {other:?}"),
        }
    }
}
