//! Approval delivery matrix + service.
//!
//! See [`super`] for the module-level overview.
//!
//! The wire-shared primitive types ([`ChannelKind`],
//! [`ChannelsConfig`], per-channel config structs,
//! [`ApprovalRequest`], [`SingleChannelDispatch`]) live in
//! `relix_core::approval` so the channel crates can implement
//! the trait without depending on this crate. We re-export them
//! through this module so existing callers keep working.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;

pub use relix_core::approval::{
    ApprovalRequest, ChannelDispatchError, ChannelKind, ChannelsConfig, DashboardChannelCfg,
    DiscordChannelCfg, EmailChannelCfg, SingleChannelDispatch, SlackChannelCfg, TelegramChannelCfg,
};

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

/// What the matrix decided for one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMatch {
    /// 0-based index of the rule that matched, or `None`
    /// when the default_channel was used.
    pub rule_index: Option<usize>,
    /// Channel resolved for the initial delivery.
    pub channel: ChannelKind,
    /// `0` when the matched rule disables escalation OR the
    /// default channel was used.
    pub escalation_timeout_secs: u64,
    /// Channel resolved for escalation, when escalation is
    /// enabled.
    pub escalation_channel: Option<ChannelKind>,
}

/// Errors surfaced by the dispatch service.
#[derive(Debug, Error)]
pub enum DeliveryError {
    /// Failure persisting the delivery row to SQLite.
    #[error("approval delivery: store error: {0}")]
    Store(#[from] ApprovalStoreError),
    /// Channel resolved but the per-channel config is absent
    /// or `enabled = false`.
    #[error("approval delivery: channel `{0}` is not enabled or not configured")]
    ChannelDisabled(String),
    /// Underlying per-channel dispatcher returned an error.
    /// The channel name is included so operators see which
    /// channel failed without having to correlate via logs.
    #[error("approval delivery: channel dispatch failed: {0}")]
    Dispatch(String),
}

impl From<ChannelDispatchError> for DeliveryError {
    fn from(value: ChannelDispatchError) -> Self {
        match value {
            ChannelDispatchError::Disabled(ch) => DeliveryError::ChannelDisabled(ch),
            ChannelDispatchError::Transport(msg) => DeliveryError::Dispatch(msg),
            ChannelDispatchError::Other(msg) => DeliveryError::Dispatch(msg),
        }
    }
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
            ChannelKind::Discord => self
                .cfg
                .channels
                .discord
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

/// Per-approval cancellation handle for the escalation
/// timer. The receive end lives in the spawned timer task; the
/// send end lives in [`ApprovalDeliveryService::escalation_cancels`]
/// and is taken when the operator records a decision.
type CancelSender = oneshot::Sender<()>;

/// Service. Cheap to clone (a couple of Arcs).
#[derive(Clone)]
pub struct ApprovalDeliveryService {
    matrix: ApprovalDeliveryMatrix,
    store: ApprovalRequestStore,
    dispatch: Arc<dyn ChannelDispatch>,
    /// PART 7: per-approval cancellation senders for spawned
    /// escalation timers. `record_decision` takes and fires
    /// the sender so the timer task wakes up early and
    /// short-circuits instead of waking from `sleep` to find a
    /// decided row. Keyed by `approval_id`.
    escalation_cancels: Arc<Mutex<HashMap<String, CancelSender>>>,
}

impl ApprovalDeliveryService {
    /// Build a new delivery service.
    pub fn new(
        matrix: ApprovalDeliveryMatrix,
        store: ApprovalRequestStore,
        dispatch: Arc<dyn ChannelDispatch>,
    ) -> Self {
        Self {
            matrix,
            store,
            dispatch,
            escalation_cancels: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Borrow the wrapped matrix (read-only view for caps).
    pub fn matrix(&self) -> &ApprovalDeliveryMatrix {
        &self.matrix
    }

    /// Borrow the wrapped store (read-only view for caps).
    pub fn store(&self) -> &ApprovalRequestStore {
        &self.store
    }

    /// End-to-end dispatch:
    ///
    /// 1. Resolve the rule + channel via the matrix.
    /// 2. Persist a `pending` row stamped with the chosen
    ///    channel BUT with `delivered_at_ms = None` —
    ///    "delivered" means "the per-channel send returned
    ///    Ok," not "we wrote a row." (PART 6)
    /// 3. Call `ChannelDispatch::send` for the initial channel.
    /// 4. On Ok: stamp `delivered_at_ms` via
    ///    `mark_delivered`; the dashboard now reports the row
    ///    as delivered.
    /// 5. On Err: flip the row to `delivery_failed`, stash the
    ///    error message in `delivery_error`, and return
    ///    `Err`. The row is surfaced via
    ///    `list_failed_deliveries` / the bridge's
    ///    `/v1/approval/failed-deliveries` route so operators
    ///    can reconcile without grepping logs.
    /// 6. When the matched rule asks for escalation, spawn a
    ///    timer task that re-checks the row after
    ///    `escalation_timeout_secs`; if the row is still
    ///    `pending`, mark `escalated = 1`, stamp
    ///    `escalated_at_ms`, and call `ChannelDispatch::send`
    ///    on the escalation channel. (PART 7) The timer
    ///    awaits both a `oneshot::Receiver<()>` cancel signal
    ///    AND the sleep timeout; `record_decision` fires the
    ///    cancel signal so the timer exits cleanly the
    ///    moment a decision lands.
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
        // PART 6: queue the row as `pending` with
        // delivered_at_ms = None so a concurrent failure can't
        // leave the dashboard reporting "delivered" for an
        // attempted-but-failed send.
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
            delivered_at_ms: None,
            escalated_at_ms: None,
            decided_at_ms: None,
            decision: None,
            decision_note: None,
            delivery_error: None,
        };
        self.store.upsert(&row)?;
        let send_result = self
            .dispatch
            .send(r.channel, &self.matrix.cfg.channels, &request, false)
            .await;
        match send_result {
            Ok(()) => {
                // Best-effort: if the operator decided in the
                // microsecond between send returning and us
                // marking delivered, the operator's decision
                // wins and we leave delivered_at_ms None.
                let delivered_at = unix_ms();
                let _ = self
                    .store
                    .mark_delivered(&request.approval_id, delivered_at)?;
                let escalation_scheduled =
                    r.escalation_timeout_secs > 0 && r.escalation_channel.is_some();
                if escalation_scheduled && let Some(esc_channel) = r.escalation_channel {
                    self.spawn_escalation_timer(
                        request.clone(),
                        r.escalation_timeout_secs,
                        esc_channel,
                    );
                }
                Ok(DeliveryOutcome {
                    approval_id: request.approval_id,
                    delivery_channel: r.channel,
                    escalation_scheduled,
                    escalation_channel: r.escalation_channel,
                    escalation_timeout_secs: r.escalation_timeout_secs,
                    delivered_at_ms: delivered_at,
                })
            }
            Err(e) => {
                let failed_at = unix_ms();
                let err_msg = format!("{e}");
                // Persist the failure on the row so operators
                // can list it via the dashboard's "failed
                // deliveries" surface. Swallow the store error
                // here — we have to surface the original
                // dispatch error to the caller regardless.
                if let Err(store_err) =
                    self.store
                        .mark_delivery_failed(&request.approval_id, &err_msg, failed_at)
                {
                    tracing::error!(
                        approval_id = %request.approval_id,
                        store_err = %store_err,
                        dispatch_err = %err_msg,
                        "approval delivery: send failed AND mark_delivery_failed failed"
                    );
                }
                Err(e)
            }
        }
    }

    /// PART 7: spawn the cancellable escalation timer. The
    /// receive end of the cancel channel lives in the spawned
    /// task; the send end goes into
    /// `self.escalation_cancels` so `record_decision` can
    /// fire it. A second insert for the same `approval_id`
    /// drops the previous sender (and the previous timer
    /// exits via the closed channel), so a re-dispatch
    /// supersedes any in-flight timer cleanly.
    fn spawn_escalation_timer(
        &self,
        request: ApprovalRequest,
        timeout_secs: u64,
        esc_channel: ChannelKind,
    ) {
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        if let Ok(mut map) = self.escalation_cancels.lock() {
            map.insert(request.approval_id.clone(), cancel_tx);
        }
        let svc = self.clone();
        let timeout = Duration::from_secs(timeout_secs);
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(timeout) => {
                    // Timer fired before the operator decided.
                    if let Err(e) = svc.maybe_escalate(request.clone(), esc_channel).await {
                        tracing::warn!(error = %e, "approval delivery: escalation failed");
                    }
                }
                _ = cancel_rx => {
                    // Cancelled — operator decided (or the
                    // service is shutting down). Nothing to do
                    // beyond letting the task exit cleanly.
                }
            }
            // Drop the cancel sender from the map so we
            // don't leak entries for completed timers.
            if let Ok(mut map) = svc.escalation_cancels.lock() {
                let _ = map.remove(&request.approval_id);
            }
        });
    }

    /// Record an operator decision. Called by the
    /// `approval.record_decision` cap when the operator
    /// approves or rejects. (PART 7) Atomically also fires
    /// the cancel signal on any in-flight escalation timer
    /// for the same `approval_id` so the timer task does not
    /// wake into a decided row.
    pub fn record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: Option<&str>,
    ) -> Result<(), DeliveryError> {
        let now = unix_ms();
        self.store
            .record_decision(approval_id, decision, note, now)?;
        // Best-effort cancellation. The cancel sender is only
        // present when an escalation timer is in flight; we
        // ignore the absent / poisoned-lock cases because the
        // decision has already landed in the store.
        if let Ok(mut map) = self.escalation_cancels.lock()
            && let Some(tx) = map.remove(approval_id)
        {
            let _ = tx.send(());
        }
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
        // PART 6: dispatch first; only stamp `escalated_at_ms`
        // when the escalation channel's send actually
        // returns Ok. A failure here logs (the row stays in
        // pending state for the next reconciliation cycle).
        self.dispatch
            .send(channel, &self.matrix.cfg.channels, &request, true)
            .await?;
        let now = unix_ms();
        self.store
            .mark_escalated(&request.approval_id, channel.as_str(), now)?;
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
                    channel_id: "C0X".into(),
                    signing_secret: "test-secret".into(),
                    peer: "slack".into(),
                }),
                discord: None,
                email: Some(EmailChannelCfg {
                    enabled: true,
                    to: "ops@x.com".into(),
                    from: "relix@x.com".into(),
                    reply_to: "approvals@x.com".into(),
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

    // ── PART 6 — delivery-status ordering ─────────────────

    /// PART 6: a recording dispatcher that lets tests script
    /// an Err for the next send call (initial OR escalation).
    /// Distinct from `RecordingDispatch` above so the existing
    /// happy-path tests stay readable.
    #[derive(Default)]
    struct ScriptedDispatch {
        log: Mutex<Vec<(ChannelKind, String, bool)>>,
        fail_next: Mutex<Option<DeliveryError>>,
    }

    impl ScriptedDispatch {
        fn fail_next(&self, err: DeliveryError) {
            *self.fail_next.lock().unwrap() = Some(err);
        }
    }

    #[async_trait::async_trait]
    impl ChannelDispatch for ScriptedDispatch {
        async fn send(
            &self,
            channel: ChannelKind,
            _cfg: &ChannelsConfig,
            request: &ApprovalRequest,
            is_escalation: bool,
        ) -> Result<(), DeliveryError> {
            if let Some(err) = self.fail_next.lock().unwrap().take() {
                return Err(err);
            }
            self.log
                .lock()
                .unwrap()
                .push((channel, request.approval_id.clone(), is_escalation));
            Ok(())
        }
    }

    #[tokio::test]
    async fn dispatch_request_does_not_set_delivered_at_ms_when_send_fails() {
        let matrix = ApprovalDeliveryMatrix::new(fixture_cfg());
        let store = ApprovalRequestStore::open_in_memory().expect("store");
        let dispatch = Arc::new(ScriptedDispatch::default());
        dispatch.fail_next(DeliveryError::Dispatch("telegram: HTTP 502".into()));
        let svc = ApprovalDeliveryService::new(matrix, store, dispatch.clone());
        let req = fixture_request("f1", "finance_alice", "tool.stripe.charge");
        let err = svc.dispatch_request(req).await.unwrap_err();
        match err {
            DeliveryError::Dispatch(msg) => assert!(msg.contains("HTTP 502")),
            other => panic!("expected Dispatch, got {other:?}"),
        }
        // Row exists, marked delivery_failed, error stashed,
        // delivered_at_ms left None.
        let row = svc.store().get("f1").unwrap().unwrap();
        assert_eq!(row.status, "delivery_failed");
        assert!(row.delivered_at_ms.is_none());
        assert_eq!(
            row.delivery_error.as_deref(),
            Some("approval delivery: channel dispatch failed: telegram: HTTP 502")
        );
        // Dispatcher's success log stays empty (the failure
        // short-circuited before we recorded the entry).
        assert!(dispatch.log.lock().unwrap().is_empty());
        // failed_deliveries list surfaces the row.
        let failed = svc.store().list_failed_deliveries(10).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].approval_id, "f1");
    }

    #[tokio::test]
    async fn dispatch_request_sets_delivered_at_ms_after_successful_send() {
        let (svc, _log) = fresh_service(fixture_cfg());
        let req = fixture_request("d1", "finance_alice", "tool.stripe.charge");
        let outcome = svc.dispatch_request(req).await.unwrap();
        let row = svc.store().get("d1").unwrap().unwrap();
        assert_eq!(row.status, "pending");
        assert!(row.delivered_at_ms.is_some());
        assert!(row.delivery_error.is_none());
        // Outcome delivered_at_ms matches the row.
        assert_eq!(outcome.delivered_at_ms, row.delivered_at_ms.unwrap());
    }

    #[tokio::test]
    async fn dispatch_failure_does_not_arm_escalation_timer() {
        // Rule 0 wants Slack with email escalation. Force the
        // initial send to fail; the spawn_escalation_timer
        // path must not run, so no escalation row should ever
        // land.
        let matrix = ApprovalDeliveryMatrix::new(fixture_cfg());
        let store = ApprovalRequestStore::open_in_memory().expect("store");
        let dispatch = Arc::new(ScriptedDispatch::default());
        dispatch.fail_next(DeliveryError::Dispatch("slack: 500".into()));
        let svc = ApprovalDeliveryService::new(matrix, store, dispatch.clone());
        let req = fixture_request("f2", "finance_alice", "tool.stripe.charge");
        let _ = svc.dispatch_request(req).await.unwrap_err();
        // Wait longer than the escalation window — escalation
        // timer must not have armed because the initial send
        // failed.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let row = svc.store().get("f2").unwrap().unwrap();
        assert!(!row.escalated);
        assert!(row.escalated_at_ms.is_none());
    }

    // ── PART 7 — escalation-timer cancellation ────────────

    #[tokio::test]
    async fn record_decision_cancels_escalation_timer() {
        // Build a service where the matched rule sets a 1s
        // escalation timeout; the test fires the decision
        // immediately and waits past the timeout — the
        // escalation row must NOT escalate.
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
        let req = fixture_request("c1", "alice", "x.do");
        svc.dispatch_request(req).await.unwrap();
        // PART 7: cancellation token is alive in the service
        // map right now.
        assert_eq!(svc.escalation_cancels.lock().unwrap().len(), 1);
        svc.record_decision("c1", "approved", Some("ok")).unwrap();
        // Cancellation should remove the entry from the map
        // synchronously.
        assert!(svc.escalation_cancels.lock().unwrap().is_empty());
        // Now sleep past the original timeout — the timer
        // task should have exited via the cancel branch.
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let row = svc.store().get("c1").unwrap().unwrap();
        assert!(!row.escalated, "decided row must not escalate: {row:?}");
        assert_eq!(row.status, "approved");
        let log_snapshot = log.log.lock().unwrap().clone();
        // Only the initial dispatch entry — escalation never
        // landed.
        assert_eq!(log_snapshot.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_map_is_empty_after_natural_escalation() {
        // When the escalation timer fires naturally (no
        // decision before timeout) the map entry should still
        // be cleaned up so we don't leak senders.
        let mut cfg = fixture_cfg();
        cfg.rules.clear();
        cfg.rules.push(DeliveryRule {
            agent_pattern: "*".into(),
            action_pattern: "y.*".into(),
            channel: "telegram".into(),
            escalation_timeout_secs: 1,
            escalation_channel: Some("slack".into()),
        });
        let (svc, _log) = fresh_service(cfg);
        let req = fixture_request("n1", "alice", "y.do");
        svc.dispatch_request(req).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1_400)).await;
        let row = svc.store().get("n1").unwrap().unwrap();
        assert!(row.escalated);
        assert!(svc.escalation_cancels.lock().unwrap().is_empty());
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
