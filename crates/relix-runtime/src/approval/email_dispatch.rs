//! `EmailChannelDispatch` — wire-real implementation of
//! [`SingleChannelDispatch`] for SMTP email.
//!
//! Renders an approval request as a plain-text email with a
//! standardised subject so the operator's reply can be
//! routed back to [`super::ApprovalDeliveryService::record_decision`]
//! by the bridge's `/v1/channels/email/reply` route (see
//! [`super::email_reply`]).
//!
//! Subject format:
//!
//! ```text
//! Approval Required: <capability> [<approval_id>]
//! ```
//!
//! The `[<approval_id>]` brackets survive `Re:` quoting so
//! the reply parser can lift the id out of the operator's
//! reply subject — the operator only has to type
//! `APPROVE` or `DENY` in front to vote.

use std::sync::Arc;

use async_trait::async_trait;

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

use crate::nodes::email::smtp::{EmailSendRequest, SmtpError, SmtpSender};

/// Operator-facing email body shape — rendered into the body
/// of the outbound SMTP message. Exposed for unit tests.
pub fn render_body(request: &ApprovalRequest, is_escalation: bool, reply_to: &str) -> String {
    let heading = if is_escalation {
        "ESCALATED APPROVAL REQUIRED"
    } else {
        "APPROVAL REQUIRED"
    };
    let reply_note = if reply_to.is_empty() {
        // Operator did not configure a reply-to. Document the
        // record_decision cap path so the reply still has a
        // wire-level audit trail.
        "No reply-to configured for this channel. Record the decision via \
         `relix approval-decide <approval_id> approve|deny`."
            .to_string()
    } else {
        format!(
            "Reply to {reply_to} with `APPROVE` or `DENY` in the subject line \
             to record your decision. The bracketed approval id at the end of \
             the subject is what routes your reply — do not delete it."
        )
    };
    format!(
        "{heading}\n\n\
         Agent:       {agent}\n\
         Action:      {capability}\n\
         Request:     {summary}\n\
         Session:     {session}\n\
         Approval ID: {id}\n\n\
         {reply_note}\n",
        agent = request.agent_name,
        capability = request.capability,
        summary = request.request_summary,
        session = request.session_id,
        id = request.approval_id,
    )
}

/// Render the standardised subject line. The
/// `[<approval_id>]` suffix is what the bridge's reply
/// parser keys off — the bracket characters survive `Re:`
/// quoting in every MUA we have tested (gmail, outlook,
/// apple mail, thunderbird, fastmail).
pub fn render_subject(request: &ApprovalRequest, is_escalation: bool) -> String {
    let prefix = if is_escalation {
        "ESCALATED Approval Required"
    } else {
        "Approval Required"
    };
    format!("{prefix}: {} [{}]", request.capability, request.approval_id,)
}

/// Minimal sender trait so the dispatcher can be exercised
/// without pulling lettre into every test. `SmtpSender`
/// implements it via the impl below; tests stamp a recording
/// implementation.
#[async_trait]
pub trait ApprovalEmailSender: Send + Sync {
    /// Send the rendered approval email. Returns the SMTP-
    /// assigned message id on success. Implementations are
    /// expected to be retried-within-budget at the underlying
    /// transport layer (SMTP 4xx retried, 5xx surfaced
    /// permanently).
    async fn send_approval_email(&self, req: EmailSendRequest) -> Result<String, String>;
}

#[async_trait]
impl ApprovalEmailSender for SmtpSender {
    async fn send_approval_email(&self, req: EmailSendRequest) -> Result<String, String> {
        SmtpSender::send(self, &req)
            .await
            .map_err(|e: SmtpError| e.to_string())
    }
}

/// Wire-real per-channel dispatcher. Holds the
/// [`ApprovalEmailSender`] handle behind an [`Arc`] so the
/// controller's startup wires one [`SmtpSender`] and shares it
/// with the approval pipeline.
#[derive(Clone)]
pub struct EmailChannelDispatch {
    sender: Arc<dyn ApprovalEmailSender>,
    /// Recipient address. Operator pastes this into the
    /// `[approval.delivery.channels.email] to` field.
    to: String,
    /// `Reply-To:` header value. Set to the address that the
    /// bridge's `/v1/channels/email/reply` route is listening
    /// on (via Mailgun / SendGrid / Postmark inbound webhook).
    /// Empty == "do not set reply-to" — the dispatcher will
    /// still send but the operator's email client falls back to
    /// the `From:` address.
    reply_to: String,
}

impl EmailChannelDispatch {
    /// Construct a new dispatcher.
    pub fn new(sender: Arc<dyn ApprovalEmailSender>, to: String, reply_to: String) -> Self {
        Self {
            sender,
            to,
            reply_to,
        }
    }

    /// Render the full [`EmailSendRequest`] for one approval.
    /// Exposed for tests so we can assert the wire shape
    /// without going through a mock transport.
    pub fn build_request(
        to: &str,
        reply_to: &str,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> EmailSendRequest {
        EmailSendRequest {
            to: vec![to.to_string()],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: if reply_to.is_empty() {
                None
            } else {
                Some(reply_to.to_string())
            },
            subject: render_subject(request, is_escalation),
            body_text: render_body(request, is_escalation, reply_to),
            body_html: None,
            in_reply_to: None,
            references: Vec::new(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
            message_id: None,
        }
    }
}

#[async_trait]
impl SingleChannelDispatch for EmailChannelDispatch {
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        if self.to.is_empty() {
            return Err(ChannelDispatchError::Disabled("email".into()));
        }
        let req = Self::build_request(&self.to, &self.reply_to, request, is_escalation);
        self.sender
            .send_approval_email(req)
            .await
            .map(|_msg_id| ())
            .map_err(|e| ChannelDispatchError::Transport(format!("email: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn fixture_request(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: id.into(),
            agent_name: "finance_alice".into(),
            capability: "tool.stripe.charge".into(),
            request_summary: "charge $100 to customer Bob".into(),
            session_id: "sess-7".into(),
            authorized_approvers: Vec::new(),
        }
    }

    #[test]
    fn subject_uses_approval_required_prefix_and_brackets_approval_id() {
        let req = fixture_request("abc-123");
        let s = render_subject(&req, false);
        assert_eq!(s, "Approval Required: tool.stripe.charge [abc-123]");
    }

    #[test]
    fn subject_uses_escalated_prefix_when_flag_is_true() {
        let req = fixture_request("xyz");
        let s = render_subject(&req, true);
        assert!(s.starts_with("ESCALATED Approval Required:"));
        assert!(s.ends_with("[xyz]"));
    }

    #[test]
    fn body_carries_every_field_and_reply_note() {
        let req = fixture_request("abc-123");
        let b = render_body(&req, false, "approvals@example.com");
        assert!(b.contains("APPROVAL REQUIRED"));
        assert!(!b.contains("ESCALATED"));
        assert!(b.contains("Agent:       finance_alice"));
        assert!(b.contains("Action:      tool.stripe.charge"));
        assert!(b.contains("Approval ID: abc-123"));
        assert!(b.contains("Reply to approvals@example.com"));
        assert!(b.contains("`APPROVE` or `DENY` in the subject line"));
    }

    #[test]
    fn body_falls_back_when_reply_to_missing() {
        let req = fixture_request("a1");
        let b = render_body(&req, false, "");
        assert!(b.contains("No reply-to configured"));
        assert!(b.contains("relix approval-decide"));
    }

    #[test]
    fn body_uses_escalated_heading_when_flag_is_true() {
        let req = fixture_request("a1");
        let b = render_body(&req, true, "approvals@example.com");
        assert!(b.contains("ESCALATED APPROVAL REQUIRED"));
    }

    #[test]
    fn build_request_carries_subject_body_reply_to_and_to() {
        let req = fixture_request("a1");
        let r = EmailChannelDispatch::build_request(
            "ops@example.com",
            "approvals@example.com",
            &req,
            false,
        );
        assert_eq!(r.to, vec!["ops@example.com"]);
        assert_eq!(r.reply_to.as_deref(), Some("approvals@example.com"));
        assert_eq!(r.subject, "Approval Required: tool.stripe.charge [a1]");
        assert!(r.body_text.contains("Approval ID: a1"));
        assert!(r.attachments.is_empty());
        assert!(r.cc.is_empty());
        assert!(r.bcc.is_empty());
    }

    #[test]
    fn build_request_omits_reply_to_when_empty() {
        let req = fixture_request("a1");
        let r = EmailChannelDispatch::build_request("ops@x", "", &req, false);
        assert!(r.reply_to.is_none());
    }

    #[derive(Default)]
    struct RecordingSender {
        sent: Mutex<Vec<EmailSendRequest>>,
        fail_next: Mutex<Option<String>>,
    }

    impl RecordingSender {
        fn fail_next(&self, msg: String) {
            *self.fail_next.lock().unwrap() = Some(msg);
        }
    }

    #[async_trait]
    impl ApprovalEmailSender for RecordingSender {
        async fn send_approval_email(&self, req: EmailSendRequest) -> Result<String, String> {
            if let Some(msg) = self.fail_next.lock().unwrap().take() {
                return Err(msg);
            }
            self.sent.lock().unwrap().push(req);
            Ok("<test-msg-id@local>".into())
        }
    }

    #[tokio::test]
    async fn send_records_email_with_blocks_to_recipient() {
        let s = Arc::new(RecordingSender::default());
        let dispatch = EmailChannelDispatch::new(s.clone(), "ops@x".into(), "approvals@x".into());
        dispatch
            .send(&fixture_request("a1"), false)
            .await
            .expect("send succeeds");
        let sent = s.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, vec!["ops@x"]);
        assert_eq!(sent[0].reply_to.as_deref(), Some("approvals@x"));
        assert!(sent[0].subject.contains("[a1]"));
    }

    #[tokio::test]
    async fn send_surfaces_transport_failure() {
        let s = Arc::new(RecordingSender::default());
        s.fail_next("smtp: 421 server unavailable".into());
        let dispatch = EmailChannelDispatch::new(s.clone(), "ops@x".into(), String::new());
        let err = dispatch
            .send(&fixture_request("a2"), false)
            .await
            .unwrap_err();
        match err {
            ChannelDispatchError::Transport(msg) => {
                assert!(msg.contains("421"), "got: {msg}");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_to_short_circuits_with_disabled() {
        let s = Arc::new(RecordingSender::default());
        let dispatch = EmailChannelDispatch::new(s.clone(), String::new(), String::new());
        let err = dispatch
            .send(&fixture_request("a3"), false)
            .await
            .unwrap_err();
        match err {
            ChannelDispatchError::Disabled(name) => assert_eq!(name, "email"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }
}
