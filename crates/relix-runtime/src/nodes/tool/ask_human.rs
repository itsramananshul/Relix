//! `tool.ask_human` — first-class capability for asking the
//! operator a question mid-execution.
//!
//! The capability takes a question (and optional context +
//! timeout) and delegates to an operator-supplied sender
//! that posts the message to the configured operator channel
//! (Telegram today, the dashboard's intervention queue
//! tomorrow). The sender returns the operator's reply, or
//! `None` on timeout — the handler folds that into a JSON
//! `{ "timeout": true }` reply so callers can branch
//! deterministically.

use std::sync::Arc;
use std::time::Duration;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::HandlerOutcome;

/// Operator-facing channel the `tool.ask_human` handler uses
/// to post a question and await a reply.
///
/// Implementations bridge to the configured surface — a
/// Telegram approval queue, the dashboard's intervention
/// surface, or any future operator channel. The trait keeps
/// the tool node decoupled from the concrete channel so a
/// future wave can land each one without re-shaping the
/// `tool.ask_human` handler.
///
/// Contract: `ask` posts `question` to the operator and
/// returns `Some(reply)` when the operator answers within the
/// timeout window, or `None` on timeout / no operator
/// available. Implementations must NOT panic; transport
/// errors fold into `None` so the handler returns
/// `{"timeout": true}` to the caller.
#[async_trait::async_trait]
pub trait OperatorChannel: Send + Sync {
    async fn ask(&self, question: String, timeout_secs: u64) -> Option<String>;
}

/// Channel that always replies with the configured string
/// after a brief delay. Useful for tests, for the
/// `RELIX_TEST_OPERATOR_REPLY` smoke path, and as a fixed
/// canned reply when an operator opts into "auto-approve
/// everything" mode for development.
pub struct CannedReplyChannel {
    pub reply: String,
}

#[async_trait::async_trait]
impl OperatorChannel for CannedReplyChannel {
    async fn ask(&self, _question: String, _timeout_secs: u64) -> Option<String> {
        Some(self.reply.clone())
    }
}

/// "No operator wired" stub. Always returns None so the
/// handler surfaces `{"timeout": true}`. Same behaviour as
/// the pre-W3 default but expressed as a real trait impl so
/// the tool node's registration consults the OnceCell
/// uniformly.
pub struct NoOperatorChannel;

#[async_trait::async_trait]
impl OperatorChannel for NoOperatorChannel {
    async fn ask(&self, _question: String, _timeout_secs: u64) -> Option<String> {
        None
    }
}

/// Process-wide handle for the tool node's operator channel.
///
/// Wrapped as `Arc<OnceCell<...>>` so the controller can
/// populate it post-startup (after the channel-specific
/// config + identity + transport are up). The
/// `tool.ask_human` handler reads it on every call so an
/// operator-channel wiring landing mid-process becomes
/// effective immediately.
pub type OperatorChannelHandle = Arc<tokio::sync::OnceCell<Arc<dyn OperatorChannel>>>;

/// Default wait time for the operator's reply, seconds. Five
/// minutes is the spec floor; operators can override per-
/// call via the `timeout_secs` field.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// Arguments accepted by `tool.ask_human`. Parsed from JSON
/// so the planner's args field carries them through the
/// dispatch pipeline.
#[derive(Clone, Debug, Deserialize)]
pub struct AskHumanArgs {
    pub question: String,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Static handler surface — no state of its own.
pub struct AskHumanTool;

impl AskHumanTool {
    /// Capability descriptor for `tool.ask_human`. Registered
    /// by the tool node alongside the rest of the
    /// `tool.*` capabilities.
    pub fn descriptor() -> CapabilityDescriptor {
        let mut d = CapabilityDescriptor::unary("tool.ask_human");
        d.major_version = 1;
        d.description = Some(
            "Ask the operator a question and wait up to `timeout_secs` (default 300) for a \
             reply. Returns `{\"answer\": \"...\"}` on success or `{\"timeout\": true}` when no \
             reply arrives in the window."
                .to_string(),
        );
        d.kind = CapabilityKind::Unary;
        d.risk_level = RiskLevel::Medium;
        // The call itself is cheap on Relix's side, but it
        // blocks on an operator's response — `Expensive` is
        // the closest documented bucket for "may take a long
        // while in wall-clock time."
        d.cost_class = CostClass::Expensive;
        d.idempotency = Idempotency::AtMostOnce;
        d.sensitivity_tags = vec!["human-in-the-loop".into(), "operator".into()];
        d.categories = vec!["interaction".into(), "approval".into()];
        d
    }

    /// Handle one call. `args` is a UTF-8 JSON object
    /// matching [`AskHumanArgs`]; `operator_sender` is the
    /// async function that posts the question to the
    /// operator channel and waits for the reply.
    ///
    /// The handler enforces the timeout via
    /// `tokio::time::timeout`; whatever the sender does
    /// internally, the outer cap is what callers see.
    pub async fn handle<F, Fut>(args: &str, operator_sender: F) -> HandlerOutcome
    where
        F: FnOnce(String, u64) -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        let parsed: AskHumanArgs = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => {
                return invalid_args(format!("tool.ask_human arg parse: {e}"));
            }
        };
        if parsed.question.trim().is_empty() {
            return invalid_args("tool.ask_human: question must be non-empty".into());
        }
        let timeout = parsed
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, 24 * 3600);
        let prompt = match parsed.context.as_deref() {
            Some(c) if !c.trim().is_empty() => format!("{}\n\nContext: {}", parsed.question, c),
            _ => parsed.question.clone(),
        };
        let send = operator_sender(prompt, timeout);
        let answer = match tokio::time::timeout(Duration::from_secs(timeout), send).await {
            Ok(Some(a)) => Some(a),
            Ok(None) => None,
            Err(_) => None, // outer timeout
        };
        let body = match answer {
            Some(text) => serde_json::json!({ "answer": text }).to_string(),
            None => serde_json::json!({ "timeout": true }).to_string(),
        };
        HandlerOutcome::Ok(body.into_bytes())
    }
}

fn invalid_args(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handle_returns_answer_when_sender_returns_some() {
        let args = r#"{"question": "deploy now?", "timeout_secs": 5}"#;
        let outcome = AskHumanTool::handle(args, |q, _t| async move {
            assert!(q.contains("deploy now?"));
            Some("yes please".to_string())
        })
        .await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got {}", e.cause),
        };
        assert!(body.contains("\"answer\":\"yes please\""));
        assert!(!body.contains("timeout"));
    }

    #[tokio::test]
    async fn handle_returns_timeout_when_sender_returns_none() {
        let args = r#"{"question": "deploy now?", "timeout_secs": 1}"#;
        let outcome = AskHumanTool::handle(args, |_q, _t| async move { None }).await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got {}", e.cause),
        };
        assert!(body.contains("\"timeout\":true"));
    }

    #[tokio::test]
    async fn handle_appends_context_to_the_prompt() {
        let args = r#"{"question": "ok?", "context": "deployment plan v2"}"#;
        let outcome = AskHumanTool::handle(args, |q, _t| async move {
            // The handler should compose question + context
            // so the operator sees the full picture.
            assert!(q.contains("ok?"));
            assert!(q.contains("Context: deployment plan v2"));
            Some("ack".into())
        })
        .await;
        assert!(matches!(outcome, HandlerOutcome::Ok(_)));
    }

    #[tokio::test]
    async fn handle_rejects_empty_question() {
        let args = r#"{"question": ""}"#;
        let outcome = AskHumanTool::handle(args, |_q, _t| async move { Some("x".into()) }).await;
        match outcome {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("non-empty"));
            }
            HandlerOutcome::Ok(_) => panic!("empty question should be rejected"),
        }
    }

    #[tokio::test]
    async fn handle_rejects_invalid_json_args() {
        let outcome =
            AskHumanTool::handle("{not-json", |_q, _t| async move { Some("x".into()) }).await;
        match outcome {
            HandlerOutcome::Err(env) => assert_eq!(env.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("malformed args should be rejected"),
        }
    }

    #[test]
    fn descriptor_carries_documented_metadata() {
        let d = AskHumanTool::descriptor();
        assert_eq!(d.method_name, "tool.ask_human");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.kind, CapabilityKind::Unary));
        assert!(d.sensitivity_tags.iter().any(|t| t == "human-in-the-loop"));
        assert!(d.categories.iter().any(|c| c == "interaction"));
    }

    #[tokio::test]
    async fn outer_timeout_fires_when_sender_hangs() {
        // Use a 1-second timeout and have the sender hang
        // indefinitely. The outer `tokio::time::timeout`
        // must drop the future and return the timeout reply.
        let args = r#"{"question": "?", "timeout_secs": 1}"#;
        let outcome = AskHumanTool::handle(args, |_q, _t| async move {
            // Sleep for far longer than the timeout.
            tokio::time::sleep(Duration::from_secs(60)).await;
            Some("late".into())
        })
        .await;
        let body = match outcome {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got {}", e.cause),
        };
        assert!(body.contains("\"timeout\":true"));
    }
}
