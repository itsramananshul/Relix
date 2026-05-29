//! Email channel node — production SMTP outbound + IMAP inbound
//! that turns inbound email into chat-flow runs and posts replies
//! back to the sender.
//!
//! ## Architecture
//!
//! - `smtp.rs` — lettre-backed outbound sender (STARTTLS / TLS /
//!   plain, pooled, retry, MIME with attachments + inline images,
//!   threading headers).
//! - `imap.rs` — async-imap-backed inbound listener (IDLE + polling
//!   fallback, MIME parsing, thread detection, size limits).
//! - `dkim.rs` — RFC 6376 RSA-SHA256 / relaxed-relaxed signer.
//! - `config.rs` — `[email]` TOML schema + env-var secret indirection.
//! - `state.rs` — connection-status + counters surfaced to the
//!   dashboard via `email.status`.
//! - `ring.rs` — bounded inbound-message ring for the recent-messages
//!   widget.
//! - `controller.rs` — the inbox listener loop + per-message dispatch
//!   through the coordinator.
//! - `client.rs` — outbound mesh RPC client (memory / ai / coordinator)
//!   used by the controller.
//! - `commands.rs` — slash-command parser shared with the other
//!   channels.
//!
//! ## Capabilities registered
//!
//! Read-only (proxied by the bridge):
//!
//! - `email.status`         — connection state for both SMTP + IMAP.
//! - `email.messages_recent` — bounded inbound ring snapshot.
//!
//! Mutating (called by the coordinator to send outbound mail):
//!
//! - `email.send`           — send a plain / HTML / multipart email.
//! - `email.send_template`  — render + send a templated email.

pub mod client;
pub mod commands;
pub mod config;
pub mod controller;
pub mod dkim;
pub mod imap;
pub mod ring;
pub mod smtp;
pub mod state;

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub use client::{EmailOutboundClient, EmailOutboundClientCell};
pub use config::{
    AiPeerConfig, CoordPeerConfig, EmailNodeConfig, EmailNodeError, MemoryPeerConfig, SmtpTls,
};
pub use controller::{run_email_controller, run_email_controller_with_components};
pub use ring::{MessageRing, RecordedInbound};
pub use smtp::{Attachment_, EmailSendRequest, InlineImage, SmtpSender};
pub use state::{EmailChannelState, EmailIdentity, LinkStatus};

/// Render the `email.status` body. Pipe-delimited wire shape
/// the bridge parses at `parse_status_body`:
///
/// `smtp=<status>|imap=<status>|from=<addr>|smtp_host=<host>|imap_host=<host>|imap_folder=<f>|messages_seen=<u64>|messages_sent=<u64>|last_send_at=<i64>|last_poll_at=<i64>|last_message_at=<i64>|smtp_error=<str>|imap_error=<str>\n`
///
/// `i64` timestamps use `-1` to mean "never"; error strings are
/// empty when there is no error. Tabs / pipes / newlines in
/// error messages are replaced with spaces so the row stays
/// parseable.
pub fn render_status_body(state: &EmailChannelState) -> String {
    let id = state.identity();
    let smtp_err = state.smtp_last_error().unwrap_or_default();
    let imap_err = state.imap_last_error().unwrap_or_default();
    format!(
        "smtp={smtp}|imap={imap}|from={from}|smtp_host={sh}|imap_host={ih}|imap_folder={folder}|messages_seen={ms}|messages_sent={mse}|last_send_at={ls}|last_poll_at={lp}|last_message_at={lm}|smtp_error={se}|imap_error={ie}\n",
        smtp = state.smtp_status().as_str(),
        imap = state.imap_status().as_str(),
        from = id.from,
        sh = id.smtp_host,
        ih = id.imap_host,
        folder = id.imap_folder,
        ms = state.messages_seen(),
        mse = state.messages_sent(),
        ls = state.last_send_at().unwrap_or(-1),
        lp = state.last_poll_at().unwrap_or(-1),
        lm = state.last_message_at().unwrap_or(-1),
        se = sanitize_field(&smtp_err),
        ie = sanitize_field(&imap_err),
    )
}

/// Render the `email.messages_recent` body. One row per recorded
/// inbound, newest-first, tab-separated:
///
/// `ts\tmessage_id\tfrom\tsubject\tsession_id\tpreview\n`
pub fn render_recent_body(ring: &MessageRing, limit: usize) -> String {
    let entries = ring.snapshot();
    let take = limit.min(entries.len());
    let mut out = String::new();
    for entry in entries.iter().rev().take(take) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\n",
            entry.ts,
            sanitize_field(&entry.message_id),
            sanitize_field(&entry.from),
            sanitize_field(&entry.subject),
            sanitize_field(&entry.session_id),
            sanitize_field(&truncate_chars(&entry.preview, 200)),
        ));
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn sanitize_field(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' | '|' => ' ',
            other => other,
        })
        .collect()
}

/// Register the email node's capabilities on the dispatch bridge.
///
/// Read capabilities (`email.status`, `email.messages_recent`)
/// project the in-memory state + ring. Mutating capabilities
/// (`email.send`, `email.send_template`) accept JSON args and
/// dispatch through the SMTP sender. Templates render via
/// `commands::render_template`.
pub fn register(
    bridge: &mut DispatchBridge,
    state: Arc<EmailChannelState>,
    ring: Arc<MessageRing>,
    smtp: Arc<SmtpSender>,
) {
    {
        let state = state.clone();
        bridge.register(
            "email.status",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let state = state.clone();
                async move {
                    let body = render_status_body(&state);
                    HandlerOutcome::Ok(body.into_bytes())
                }
            })),
        );
    }
    {
        let ring = ring.clone();
        bridge.register(
            "email.messages_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let ring = ring.clone();
                async move {
                    let text = String::from_utf8_lossy(&ctx.args);
                    let limit = text
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .unwrap_or(20);
                    let body = render_recent_body(&ring, limit);
                    HandlerOutcome::Ok(body.into_bytes())
                }
            })),
        );
    }
    {
        let smtp = smtp.clone();
        let state = state.clone();
        bridge.register(
            "email.send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let smtp = smtp.clone();
                let state = state.clone();
                async move { handle_email_send(ctx.args, smtp, state).await }
            })),
        );
    }
    {
        let smtp = smtp.clone();
        let state = state.clone();
        bridge.register(
            "email.send_template",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let smtp = smtp.clone();
                let state = state.clone();
                async move { handle_email_send_template(ctx.args, smtp, state).await }
            })),
        );
    }
    // PART 8: rich approval dispatch. Hosts the approval-rendering
    // `EmailChannelDispatch` so the coordinator can route approval
    // emails (subject `Approval Required: <cap> [<id>]`) through
    // the email node's SmtpSender.
    {
        let smtp = smtp.clone();
        bridge.register(
            "email.approval_send",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let smtp = smtp.clone();
                async move { handle_approval_send(ctx.args, smtp).await }
            })),
        );
    }
}

/// PART 8: dispatch one approval request via the rendering
/// `EmailChannelDispatch`. `target_id` is the recipient mailbox
/// (`[approval.delivery.channels.email] to`); `target_extra`
/// carries the `Reply-To:` header so the operator's reply lands
/// on the bridge's `/v1/channels/email/reply` route.
pub async fn handle_approval_send(args: Vec<u8>, smtp: Arc<SmtpSender>) -> HandlerOutcome {
    let parsed: crate::approval::ApprovalSendArgs = match serde_json::from_slice(&args) {
        Ok(v) => v,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("email.approval_send: decode args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    if parsed.target_id.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "email.approval_send: target_id (recipient mailbox) must be non-empty".into(),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let request = parsed.to_request();
    let is_escalation = parsed.is_escalation;
    let sender: Arc<dyn crate::approval::ApprovalEmailSender> = smtp;
    let dispatch =
        crate::approval::EmailChannelDispatch::new(sender, parsed.target_id, parsed.target_extra);
    use relix_core::approval::SingleChannelDispatch;
    match dispatch.send(&request, is_escalation).await {
        Ok(()) => HandlerOutcome::Ok(b"{\"ok\":true}".to_vec()),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("email.approval_send: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// Decode the JSON args for `email.send`, dispatch via SMTP, and
/// return the chosen Message-ID on success.
pub async fn handle_email_send(
    args: Vec<u8>,
    smtp: Arc<SmtpSender>,
    state: Arc<EmailChannelState>,
) -> HandlerOutcome {
    let req: SendArgs = match serde_json::from_slice(&args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("decode email.send args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let smtp_req = req.into_smtp_request();
    match smtp.send(&smtp_req).await {
        Ok(message_id) => {
            let ts = unix_now();
            state.record_send(ts);
            state.mark_smtp_connected();
            let body = serde_json::json!({ "message_id": message_id });
            HandlerOutcome::Ok(body.to_string().into_bytes())
        }
        Err(e) => {
            state.mark_smtp_error(&e.to_string());
            HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("smtp send: {e}"),
                retry_hint: 0,
                retry_after: None,
            })
        }
    }
}

/// Decode the JSON args for `email.send_template`, render the
/// template, dispatch via SMTP, and return the Message-ID.
pub async fn handle_email_send_template(
    args: Vec<u8>,
    smtp: Arc<SmtpSender>,
    state: Arc<EmailChannelState>,
) -> HandlerOutcome {
    let req: SendTemplateArgs = match serde_json::from_slice(&args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("decode email.send_template args: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let template = match commands::find_template(&req.template_name) {
        Some(t) => t,
        None => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("template not found: {}", req.template_name),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let rendered_subject = commands::render_template(&template.subject, &req.variables);
    let rendered_body = commands::render_template(&template.body, &req.variables);
    let rendered_html = template
        .html
        .as_ref()
        .map(|h| commands::render_template(h, &req.variables));

    let smtp_req = EmailSendRequest {
        to: req.to,
        cc: req.cc,
        bcc: req.bcc,
        reply_to: req.reply_to,
        subject: rendered_subject,
        body_text: rendered_body,
        body_html: rendered_html,
        in_reply_to: req.in_reply_to,
        references: req.references.unwrap_or_default(),
        attachments: Vec::new(),
        inline_images: Vec::new(),
        message_id: None,
    };
    match smtp.send(&smtp_req).await {
        Ok(message_id) => {
            let ts = unix_now();
            state.record_send(ts);
            state.mark_smtp_connected();
            let body = serde_json::json!({
                "message_id": message_id,
                "template": req.template_name,
            });
            HandlerOutcome::Ok(body.to_string().into_bytes())
        }
        Err(e) => {
            state.mark_smtp_error(&e.to_string());
            HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("smtp send (template): {e}"),
                retry_hint: 0,
                retry_after: None,
            })
        }
    }
}

/// JSON shape accepted by `email.send` (the coordinator forwards
/// the bridge's `POST /v1/email/send` body verbatim). All
/// addresses are RFC 5322 mailboxes (`"alice@example.com"` or
/// `"Alice <alice@example.com>"`).
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct SendArgs {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub attachments: Vec<SendArgsAttachment>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct SendArgsAttachment {
    /// Filesystem path the email node reads at send time. For
    /// pure-data attachments callers can pass `bytes_base64`
    /// instead.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub bytes_base64: Option<String>,
    /// Display filename in the MIME part.
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_attachment_ct")]
    pub content_type: String,
}

fn default_attachment_ct() -> String {
    "application/octet-stream".to_string()
}

impl SendArgs {
    /// Project the wire-format args into the typed SMTP request.
    /// Attachments are read from disk synchronously here — the
    /// caller is the coordinator dispatch task on a tokio runtime
    /// so the std::fs read is OK for small attachments. For very
    /// large attachments callers should pass `bytes_base64`.
    pub fn into_smtp_request(self) -> EmailSendRequest {
        let mut attachments: Vec<Attachment_> = Vec::new();
        for a in self.attachments {
            let bytes = if let Some(b64) = a.bytes_base64.as_ref() {
                match base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    b64.as_bytes(),
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "email.send: dropping attachment with invalid base64 body"
                        );
                        continue;
                    }
                }
            } else if let Some(path) = a.path.as_ref() {
                match std::fs::read(path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            path = %path,
                            error = %e,
                            "email.send: dropping attachment that could not be read from disk"
                        );
                        continue;
                    }
                }
            } else {
                tracing::warn!("email.send: attachment without `path` or `bytes_base64`; dropping");
                continue;
            };
            let filename = a
                .filename
                .clone()
                .or_else(|| {
                    a.path.as_ref().and_then(|p| {
                        std::path::Path::new(p)
                            .file_name()
                            .map(|f| f.to_string_lossy().into_owned())
                    })
                })
                .unwrap_or_else(|| "attachment.bin".to_string());
            attachments.push(Attachment_ {
                filename,
                content_type: a.content_type,
                bytes,
            });
        }
        EmailSendRequest {
            to: self.to,
            cc: self.cc,
            bcc: self.bcc,
            reply_to: self.reply_to,
            subject: self.subject,
            body_text: self.body,
            body_html: self.html,
            in_reply_to: self.in_reply_to,
            references: self.references.unwrap_or_default(),
            attachments,
            inline_images: Vec::new(),
            message_id: None,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct SendTemplateArgs {
    pub template_name: String,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub variables: std::collections::BTreeMap<String, String>,
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_status_body_disconnected_default_shape() {
        let s = EmailChannelState::default();
        let body = render_status_body(&s);
        assert!(body.contains("smtp=disconnected"));
        assert!(body.contains("imap=disconnected"));
        assert!(body.contains("messages_seen=0"));
        assert!(body.contains("last_send_at=-1"));
        assert!(body.contains("last_poll_at=-1"));
        assert!(body.contains("smtp_error="));
        assert!(body.ends_with('\n'));
    }

    #[test]
    fn render_status_body_after_connect_includes_state() {
        let s = EmailChannelState::default();
        s.set_identity(EmailIdentity {
            from: "Relix <bot@example.com>".into(),
            smtp_host: "smtp.example.com".into(),
            imap_host: "imap.example.com".into(),
            imap_folder: "INBOX".into(),
        });
        s.mark_smtp_connected();
        s.mark_imap_connected();
        s.record_send(1700000000);
        s.record_poll(1700000010);
        s.record_inbound(1700000020);
        let body = render_status_body(&s);
        assert!(body.contains("smtp=connected"));
        assert!(body.contains("imap=connected"));
        assert!(body.contains("from=Relix <bot@example.com>"));
        assert!(body.contains("last_send_at=1700000000"));
        assert!(body.contains("last_poll_at=1700000010"));
        assert!(body.contains("last_message_at=1700000020"));
    }

    #[test]
    fn render_recent_body_returns_newest_first() {
        let ring = MessageRing::new(200);
        ring.record(RecordedInbound {
            ts: 100,
            message_id: "m1@e".into(),
            from: "alice@e".into(),
            subject: "old".into(),
            session_id: "s1".into(),
            preview: "x".into(),
        });
        ring.record(RecordedInbound {
            ts: 200,
            message_id: "m2@e".into(),
            from: "bob@e".into(),
            subject: "new".into(),
            session_id: "s2".into(),
            preview: "y".into(),
        });
        let body = render_recent_body(&ring, 20);
        let lines: Vec<&str> = body.trim_end().split('\n').collect();
        assert!(lines[0].contains("\tbob@e\t"));
        assert!(lines[1].contains("\talice@e\t"));
    }

    #[test]
    fn render_recent_body_truncates_preview_to_200_chars() {
        let ring = MessageRing::new(200);
        let long: String = "a".repeat(500);
        ring.record(RecordedInbound {
            ts: 10,
            message_id: "m".into(),
            from: "f".into(),
            subject: "s".into(),
            session_id: "x".into(),
            preview: long,
        });
        let body = render_recent_body(&ring, 5);
        let preview = body.split('\t').nth(5).unwrap().trim_end_matches('\n');
        assert_eq!(preview.chars().count(), 200);
    }

    #[test]
    fn sanitize_field_replaces_separators_with_spaces() {
        assert_eq!(sanitize_field("a\tb\nc|d\re"), "a b c d e");
    }

    #[test]
    fn send_args_decodes_minimal_payload() {
        let payload = serde_json::json!({
            "to": ["alice@example.com"],
            "subject": "hi",
            "body": "hello",
        });
        let req: SendArgs = serde_json::from_value(payload).unwrap();
        assert_eq!(req.to, vec!["alice@example.com".to_string()]);
        assert_eq!(req.subject, "hi");
        assert_eq!(req.body, "hello");
        assert!(req.html.is_none());
        assert!(req.attachments.is_empty());
    }

    #[test]
    fn send_args_into_smtp_drops_attachments_without_source() {
        let args = SendArgs {
            to: vec!["a@b.c".into()],
            subject: "s".into(),
            body: "b".into(),
            attachments: vec![SendArgsAttachment {
                path: None,
                bytes_base64: None,
                filename: Some("x".into()),
                content_type: "application/octet-stream".into(),
            }],
            ..Default::default()
        };
        let smtp_req = args.into_smtp_request();
        assert!(smtp_req.attachments.is_empty());
    }

    #[test]
    fn send_args_into_smtp_decodes_base64_attachment() {
        let args = SendArgs {
            to: vec!["a@b.c".into()],
            subject: "s".into(),
            body: "b".into(),
            attachments: vec![SendArgsAttachment {
                path: None,
                bytes_base64: Some(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    b"hello",
                )),
                filename: Some("x.txt".into()),
                content_type: "text/plain".into(),
            }],
            ..Default::default()
        };
        let smtp_req = args.into_smtp_request();
        assert_eq!(smtp_req.attachments.len(), 1);
        assert_eq!(smtp_req.attachments[0].bytes, b"hello");
        assert_eq!(smtp_req.attachments[0].filename, "x.txt");
    }
}
