//! RELIX-7.7 / 7.11 GAP 2 — Channel routing layer.
//!
//! A channel-agnostic rule engine that maps an inbound message
//! from any channel (Telegram, Discord, Slack, Email) to a
//! target agent peer + capability. Rules live in
//! `[routing.rules]` in the coordinator's TOML config and are
//! evaluated top-to-bottom; the first match wins.
//!
//! Match types per rule:
//!
//! - `sender_match` — glob-pattern match on the inbound message's
//!   sender identifier (email address, Telegram username,
//!   Discord user_id, Slack user_id).
//! - `subject_match` — glob match on the subject / topic
//!   (email Subject:, Discord channel name, Slack channel
//!   name). Silently ignored for channels that have no subject
//!   concept (Telegram).
//! - `content_match` — glob match on the message body. Glob,
//!   not regex, to avoid ReDoS.
//! - `channel_type` — exact match on the originating channel
//!   ("email" | "telegram" | "discord" | "slack").
//! - `catch_all` — `true` matches every message. Must be the
//!   last rule when present.
//!
//! Each rule names a `target_agent` peer alias and optionally a
//! `capability` (default `"ai.chat"`). The router validates at
//! startup that every `target_agent` appears in the controller's
//! `[peers]` map and fails fast with a clear error otherwise.
//!
//! The router is wired as a coordinator capability,
//! `routing.resolve`, that channel nodes call before dispatching
//! their AI step. A second capability, `routing.list`, returns
//! the configured table as JSON so operators can inspect it at
//! runtime.
//!
//! The router is also re-exported by `crate::nodes::coordinator`
//! so the alert engine + future in-process consumers can
//! evaluate rules without going through the wire.

use std::collections::BTreeSet;
use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::{Deserialize, Serialize};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Default capability target when a rule does not specify one.
pub const DEFAULT_CAPABILITY: &str = "ai.chat";

/// Channel-type enum. Stored as a lower-case string in the
/// config so operator TOML stays readable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelType {
    Email,
    Telegram,
    Discord,
    Slack,
}

impl ChannelType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelType::Email => "email",
            ChannelType::Telegram => "telegram",
            ChannelType::Discord => "discord",
            ChannelType::Slack => "slack",
        }
    }

    /// Parse a case-insensitive channel name. Returns `None`
    /// for unknown channels so callers can surface a clear
    /// error instead of silently routing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "email" => Some(Self::Email),
            "telegram" => Some(Self::Telegram),
            "discord" => Some(Self::Discord),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }

    /// `true` when this channel carries a meaningful "subject"
    /// concept (email subject line, Discord/Slack channel
    /// name). Telegram doesn't — `subject_match` is silently
    /// skipped for telegram messages.
    pub fn has_subject(self) -> bool {
        !matches!(self, ChannelType::Telegram)
    }
}

/// One routing rule. Validated at construction time so the
/// runtime never holds a malformed rule.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoutingRule {
    /// Optional name for log lines + the `routing.list`
    /// projection. Defaults to `rule-<idx>` when missing.
    #[serde(default)]
    pub name: Option<String>,
    /// Match on sender identifier — case-insensitive glob.
    #[serde(default)]
    pub sender_match: Option<String>,
    /// Match on subject / topic — case-insensitive glob.
    #[serde(default)]
    pub subject_match: Option<String>,
    /// Match on body content — case-insensitive glob.
    #[serde(default)]
    pub content_match: Option<String>,
    /// Match on channel type — exact (case-insensitive).
    #[serde(default)]
    pub channel_type: Option<String>,
    /// `true` for the catch-all fallback rule.
    #[serde(default)]
    pub catch_all: bool,
    /// Peer alias to dispatch to.
    pub target_agent: String,
    /// Optional capability override. Default `"ai.chat"`.
    #[serde(default)]
    pub capability: Option<String>,
}

impl RoutingRule {
    /// Pretty name for log lines. Caller passes the rule's
    /// index in the table so unnamed rules get
    /// `rule-<idx>`.
    pub fn display_name(&self, idx: usize) -> String {
        self.name.clone().unwrap_or_else(|| format!("rule-{idx}"))
    }

    /// Resolve the channel-type filter, if present.
    pub fn channel_filter(&self) -> Option<ChannelType> {
        self.channel_type.as_deref().and_then(ChannelType::parse)
    }
}

/// Inbound message envelope handed to the router. Filled in by
/// the calling channel's controller. Fields irrelevant to the
/// channel are empty strings (e.g. Telegram passes `subject =
/// ""`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InboundMessage {
    pub channel: ChannelType,
    /// Sender identifier (email addr-spec, Telegram `@username`
    /// or numeric id, Discord user_id, Slack user_id).
    #[serde(default)]
    pub sender: String,
    /// Subject / topic. Empty for telegram.
    #[serde(default)]
    pub subject: String,
    /// First N chars of the body. Bounded by the caller; the
    /// router doesn't truncate further.
    #[serde(default)]
    pub content: String,
}

/// Match outcome. Returned by `route()` and by the
/// `routing.resolve` capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub target_agent: String,
    pub capability: String,
    /// Which rule matched. `None` when no rule matched (the
    /// caller falls back to the static `ai.chat` default).
    pub matched_rule: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("routing rule {rule}: {reason}")]
    InvalidRule { rule: String, reason: String },
    #[error("routing rule {rule}: target_agent {peer:?} not in [peers]")]
    UnknownPeer { rule: String, peer: String },
    #[error("routing rule {rule}: unknown channel_type {channel:?}")]
    UnknownChannel { rule: String, channel: String },
    #[error("routing: catch_all rule {rule:?} must be the last rule")]
    CatchAllNotLast { rule: String },
}

/// In-memory routing table. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct ChannelRouter {
    inner: Arc<RouterInner>,
}

struct RouterInner {
    rules: Vec<RoutingRule>,
}

impl ChannelRouter {
    /// Build + validate a router from a rule list. `known_peers`
    /// is the set of peer aliases the controller has configured
    /// in `[peers]` — every rule's `target_agent` must appear in
    /// it.
    pub fn new(
        rules: Vec<RoutingRule>,
        known_peers: &BTreeSet<String>,
    ) -> Result<Self, RouterError> {
        for (idx, rule) in rules.iter().enumerate() {
            let display = rule.display_name(idx);
            if rule.target_agent.trim().is_empty() {
                return Err(RouterError::InvalidRule {
                    rule: display,
                    reason: "target_agent is required".into(),
                });
            }
            // channel_type, if given, must parse. Check this
            // BEFORE the has-match check so a malformed
            // channel_type surfaces its own dedicated error.
            if let Some(c) = rule.channel_type.as_deref()
                && ChannelType::parse(c).is_none()
            {
                return Err(RouterError::UnknownChannel {
                    rule: display,
                    channel: c.to_string(),
                });
            }
            // Mode: at least one match field OR catch_all must
            // be set.
            let has_match = rule.sender_match.is_some()
                || rule.subject_match.is_some()
                || rule.content_match.is_some()
                || rule.channel_filter().is_some();
            if !rule.catch_all && !has_match {
                return Err(RouterError::InvalidRule {
                    rule: display,
                    reason: "rule must set at least one of sender_match / subject_match / \
                              content_match / channel_type / catch_all"
                        .into(),
                });
            }
            // catch_all must be last when present.
            if rule.catch_all && idx + 1 < rules.len() {
                return Err(RouterError::CatchAllNotLast { rule: display });
            }
            // target_agent must exist in [peers]. Empty
            // `known_peers` means the operator hasn't wired any
            // peers — accept anything (we treat the validator
            // as advisory in that case to avoid blocking
            // bootstrap).
            if !known_peers.is_empty() && !known_peers.contains(&rule.target_agent) {
                return Err(RouterError::UnknownPeer {
                    rule: display,
                    peer: rule.target_agent.clone(),
                });
            }
        }
        Ok(Self {
            inner: Arc::new(RouterInner { rules }),
        })
    }

    /// Empty router — every `route()` call returns `None`. Used
    /// when the operator has not configured a `[routing]`
    /// section; channels fall back to the legacy fixed `("ai",
    /// "ai.chat")` target.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(RouterInner { rules: Vec::new() }),
        }
    }

    pub fn rules(&self) -> &[RoutingRule] {
        &self.inner.rules
    }

    pub fn is_empty(&self) -> bool {
        self.inner.rules.is_empty()
    }

    /// Evaluate the rules in order. Returns the first match, or
    /// `None` when no rule matched + no catch_all exists.
    pub fn route(&self, msg: &InboundMessage) -> Option<RoutingDecision> {
        for (idx, rule) in self.inner.rules.iter().enumerate() {
            if rule_matches(rule, msg) {
                let name = rule.display_name(idx);
                tracing::debug!(
                    rule = %name,
                    channel = msg.channel.as_str(),
                    sender = %msg.sender,
                    target = %rule.target_agent,
                    "routing: rule matched"
                );
                return Some(RoutingDecision {
                    target_agent: rule.target_agent.clone(),
                    capability: rule
                        .capability
                        .clone()
                        .unwrap_or_else(|| DEFAULT_CAPABILITY.to_string()),
                    matched_rule: Some(name),
                });
            }
        }
        None
    }
}

impl std::fmt::Debug for ChannelRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelRouter")
            .field("rules", &self.inner.rules.len())
            .finish_non_exhaustive()
    }
}

fn rule_matches(rule: &RoutingRule, msg: &InboundMessage) -> bool {
    if rule.catch_all {
        return true;
    }
    // Every set filter must match. Unset filters are
    // wildcards.
    if let Some(channel_str) = rule.channel_type.as_deref()
        && let Some(want) = ChannelType::parse(channel_str)
        && want != msg.channel
    {
        return false;
    }
    if let Some(pat) = rule.sender_match.as_deref()
        && !glob_match(pat, &msg.sender)
    {
        return false;
    }
    if let Some(pat) = rule.subject_match.as_deref() {
        // Channels without a subject concept don't get matched
        // by a subject_match rule — the absence of the subject
        // can't satisfy the operator's intent.
        if !msg.channel.has_subject() || !glob_match(pat, &msg.subject) {
            return false;
        }
    }
    if let Some(pat) = rule.content_match.as_deref()
        && !glob_match(pat, &msg.content)
    {
        return false;
    }
    true
}

/// Case-insensitive glob match. Supports the two operators
/// most operators expect: `*` (zero-or-more any-char) and `?`
/// (exactly one any-char). Brackets / regex are NOT supported
/// — keeps the matcher ReDoS-immune.
pub fn glob_match(pattern: &str, candidate: &str) -> bool {
    let p: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_ascii_lowercase().chars().collect();
    glob_match_chars(&p, 0, &c, 0)
}

fn glob_match_chars(p: &[char], pi: usize, c: &[char], ci: usize) -> bool {
    if pi == p.len() {
        return ci == c.len();
    }
    match p[pi] {
        '*' => {
            // Match zero or more characters. Try every
            // candidate split — fine because patterns are
            // bounded (operator-authored, < ~128 chars).
            for k in ci..=c.len() {
                if glob_match_chars(p, pi + 1, c, k) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ci >= c.len() {
                return false;
            }
            glob_match_chars(p, pi + 1, c, ci + 1)
        }
        ch => {
            if ci >= c.len() || c[ci] != ch {
                return false;
            }
            glob_match_chars(p, pi + 1, c, ci + 1)
        }
    }
}

/// Top-level `[routing]` config block.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub rules: Vec<RoutingRule>,
}

/// Register `routing.resolve` + `routing.list` on the dispatch
/// bridge. The router is constructed once at startup and the
/// same `Arc` is shared between both handlers.
pub fn register(bridge: &mut DispatchBridge, router: ChannelRouter) {
    register_resolve(bridge, router.clone());
    register_list(bridge, router);
}

fn register_resolve(bridge: &mut DispatchBridge, router: ChannelRouter) {
    bridge.register(
        "routing.resolve",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let router = router.clone();
            async move {
                let msg: InboundMessage = match serde_json::from_slice(&ctx.args) {
                    Ok(m) => m,
                    Err(e) => {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("routing.resolve: decode args: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        });
                    }
                };
                let decision = router.route(&msg);
                let body = serde_json::json!({
                    "decision": decision,
                    "rules_evaluated": router.rules().len(),
                });
                match serde_json::to_vec(&body) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("routing.resolve: encode response: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn register_list(bridge: &mut DispatchBridge, router: ChannelRouter) {
    bridge.register(
        "routing.list",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let router = router.clone();
            async move {
                let rules: Vec<RoutingRule> = router.rules().to_vec();
                match serde_json::to_vec(&rules) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("routing.list: encode response: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peers(set: &[&str]) -> BTreeSet<String> {
        set.iter().map(|s| s.to_string()).collect()
    }

    fn msg(channel: ChannelType, sender: &str, subject: &str, content: &str) -> InboundMessage {
        InboundMessage {
            channel,
            sender: sender.into(),
            subject: subject.into(),
            content: content.into(),
        }
    }

    fn rule(target: &str) -> RoutingRule {
        RoutingRule {
            name: None,
            sender_match: None,
            subject_match: None,
            content_match: None,
            channel_type: None,
            catch_all: false,
            target_agent: target.into(),
            capability: None,
        }
    }

    #[test]
    fn glob_matches_exact_string() {
        assert!(glob_match("alice@example.com", "alice@example.com"));
        assert!(!glob_match("alice@example.com", "bob@example.com"));
    }

    #[test]
    fn glob_is_case_insensitive() {
        assert!(glob_match("ALICE@EXAMPLE.COM", "alice@example.com"));
    }

    #[test]
    fn glob_star_matches_any_run() {
        assert!(glob_match("*@trusted.com", "alice@trusted.com"));
        assert!(glob_match("*@trusted.com", "@trusted.com"));
        assert!(glob_match("URGENT:*", "URGENT: server down"));
        assert!(!glob_match("URGENT:*", "regular subject"));
    }

    #[test]
    fn glob_question_matches_single_char() {
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("a?c", "axc"));
        assert!(!glob_match("a?c", "abbc"));
    }

    #[test]
    fn sender_match_with_exact_address() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                sender_match: Some("alice@example.com".into()),
                target_agent: "alice-agent".into(),
                ..rule("alice-agent")
            }],
            &peers(&["alice-agent"]),
        )
        .unwrap();
        let d = r
            .route(&msg(ChannelType::Email, "alice@example.com", "", ""))
            .unwrap();
        assert_eq!(d.target_agent, "alice-agent");
        assert_eq!(d.capability, "ai.chat");
        assert!(
            r.route(&msg(ChannelType::Email, "bob@example.com", "", ""))
                .is_none()
        );
    }

    #[test]
    fn sender_match_with_glob() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                sender_match: Some("*@trusted-domain.com".into()),
                target_agent: "work-agent".into(),
                ..rule("work-agent")
            }],
            &peers(&["work-agent"]),
        )
        .unwrap();
        assert_eq!(
            r.route(&msg(
                ChannelType::Email,
                "anyone@trusted-domain.com",
                "",
                ""
            ))
            .unwrap()
            .target_agent,
            "work-agent"
        );
        assert!(
            r.route(&msg(ChannelType::Email, "anyone@other.com", "", ""))
                .is_none()
        );
    }

    #[test]
    fn subject_match_with_prefix() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                subject_match: Some("URGENT:*".into()),
                target_agent: "urgent-handler".into(),
                capability: Some("ai.chat".into()),
                ..rule("urgent-handler")
            }],
            &peers(&["urgent-handler"]),
        )
        .unwrap();
        assert!(
            r.route(&msg(
                ChannelType::Email,
                "x@y",
                "URGENT: server crash",
                "details"
            ))
            .is_some()
        );
        assert!(
            r.route(&msg(ChannelType::Email, "x@y", "weekly update", "details"))
                .is_none()
        );
    }

    #[test]
    fn subject_match_is_ignored_for_telegram() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                subject_match: Some("anything".into()),
                target_agent: "x".into(),
                ..rule("x")
            }],
            &peers(&["x"]),
        )
        .unwrap();
        // Telegram has no subject — subject_match never matches.
        assert!(
            r.route(&msg(ChannelType::Telegram, "@user", "", "hi"))
                .is_none()
        );
    }

    #[test]
    fn channel_type_filters_by_origin() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                channel_type: Some("telegram".into()),
                target_agent: "tg-agent".into(),
                ..rule("tg-agent")
            }],
            &peers(&["tg-agent"]),
        )
        .unwrap();
        assert_eq!(
            r.route(&msg(ChannelType::Telegram, "@u", "", "hi"))
                .unwrap()
                .target_agent,
            "tg-agent"
        );
        assert!(
            r.route(&msg(ChannelType::Email, "@u", "", "hi")).is_none(),
            "channel_type=telegram must not match email"
        );
    }

    #[test]
    fn catch_all_matches_everything() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                catch_all: true,
                target_agent: "default".into(),
                ..rule("default")
            }],
            &peers(&["default"]),
        )
        .unwrap();
        let cases = [
            msg(ChannelType::Email, "x@y", "subj", "body"),
            msg(ChannelType::Telegram, "@u", "", "hi"),
            msg(ChannelType::Discord, "u_001", "#general", "body"),
            msg(ChannelType::Slack, "U001", "#general", "body"),
        ];
        for m in &cases {
            assert_eq!(r.route(m).unwrap().target_agent, "default");
        }
    }

    #[test]
    fn first_match_wins_in_evaluation_order() {
        let r = ChannelRouter::new(
            vec![
                RoutingRule {
                    sender_match: Some("alice@*".into()),
                    target_agent: "alice-handler".into(),
                    ..rule("alice-handler")
                },
                RoutingRule {
                    catch_all: true,
                    target_agent: "default".into(),
                    ..rule("default")
                },
            ],
            &peers(&["alice-handler", "default"]),
        )
        .unwrap();
        let d = r
            .route(&msg(ChannelType::Email, "alice@example.com", "", ""))
            .unwrap();
        // The sender rule sits before the catch_all in the
        // table; the matcher MUST stop at the first match
        // rather than walking the catch_all.
        assert_eq!(d.target_agent, "alice-handler");
    }

    #[test]
    fn unknown_peer_fails_validation_with_peer_name_in_error() {
        let err = ChannelRouter::new(
            vec![RoutingRule {
                catch_all: true,
                target_agent: "missing-peer".into(),
                ..rule("missing-peer")
            }],
            &peers(&["other-peer"]),
        )
        .unwrap_err();
        match err {
            RouterError::UnknownPeer { peer, .. } => {
                assert_eq!(peer, "missing-peer");
            }
            other => panic!("expected UnknownPeer, got {other:?}"),
        }
    }

    #[test]
    fn catch_all_must_be_last() {
        let err = ChannelRouter::new(
            vec![
                RoutingRule {
                    catch_all: true,
                    target_agent: "default".into(),
                    ..rule("default")
                },
                RoutingRule {
                    sender_match: Some("alice@*".into()),
                    target_agent: "alice".into(),
                    ..rule("alice")
                },
            ],
            &peers(&["default", "alice"]),
        )
        .unwrap_err();
        assert!(matches!(err, RouterError::CatchAllNotLast { .. }));
    }

    #[test]
    fn empty_rule_without_catch_all_is_rejected() {
        let err = ChannelRouter::new(
            vec![RoutingRule {
                target_agent: "x".into(),
                ..rule("x")
            }],
            &peers(&["x"]),
        )
        .unwrap_err();
        assert!(matches!(err, RouterError::InvalidRule { .. }));
    }

    #[test]
    fn unknown_channel_type_string_is_rejected() {
        let err = ChannelRouter::new(
            vec![RoutingRule {
                channel_type: Some("smoke_signal".into()),
                target_agent: "x".into(),
                ..rule("x")
            }],
            &peers(&["x"]),
        )
        .unwrap_err();
        assert!(matches!(err, RouterError::UnknownChannel { .. }));
    }

    #[test]
    fn empty_router_returns_none_on_every_route() {
        let r = ChannelRouter::empty();
        assert!(r.is_empty());
        assert!(r.route(&msg(ChannelType::Email, "x@y", "s", "b")).is_none());
    }

    #[test]
    fn rule_with_capability_override_returns_that_capability() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                sender_match: Some("alice@*".into()),
                target_agent: "alice".into(),
                capability: Some("my.custom.cap".into()),
                ..rule("alice")
            }],
            &peers(&["alice"]),
        )
        .unwrap();
        let d = r
            .route(&msg(ChannelType::Email, "alice@x", "", ""))
            .unwrap();
        assert_eq!(d.capability, "my.custom.cap");
    }

    #[test]
    fn routing_list_returns_rules_as_json() {
        // Serde round-trip via the same path the dispatch
        // handler uses.
        let rules = vec![
            RoutingRule {
                name: Some("trusted".into()),
                sender_match: Some("*@trusted.com".into()),
                target_agent: "trusted-agent".into(),
                ..rule("trusted-agent")
            },
            RoutingRule {
                catch_all: true,
                target_agent: "default".into(),
                ..rule("default")
            },
        ];
        let json = serde_json::to_string(&rules).unwrap();
        assert!(json.contains("trusted-agent"));
        assert!(json.contains("\"catch_all\":true"));
    }

    /// Empty `known_peers` must be permissive — controllers
    /// without a `[peers]` map at bootstrap time still parse
    /// their rules. Production deployments always set peers,
    /// so the real validation fires there.
    #[test]
    fn empty_known_peers_skips_peer_validation() {
        let r = ChannelRouter::new(
            vec![RoutingRule {
                catch_all: true,
                target_agent: "any-peer".into(),
                ..rule("any-peer")
            }],
            &peers(&[]),
        )
        .unwrap();
        assert_eq!(
            r.route(&msg(ChannelType::Email, "x", "", ""))
                .unwrap()
                .target_agent,
            "any-peer"
        );
    }
}
