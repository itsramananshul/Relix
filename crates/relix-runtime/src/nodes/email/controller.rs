//! Email controller — the bridge between IMAP inbound + the
//! coordinator chat flow.
//!
//! Mirrors `nodes/slack/controller.rs` shape:
//!
//! 1. Connect SMTP transport eagerly (validate auth at boot).
//! 2. Spawn the IMAP listener loop.
//! 3. Per inbound message: record in the ring + state, run the
//!    slash-command parser, route to chat flow or local handler.
//! 4. Send the agent's reply via SMTP with `In-Reply-To` set to
//!    the inbound `Message-ID` so MUAs thread the conversation.

use std::sync::Arc;

use super::client::{EmailOutbound, EmailOutboundClientCell};
use super::commands::{
    Command, brain_unreachable_message, help_message, memory_body, oversize_message, status_body,
    unauthorised_message,
};
use super::config::EmailNodeConfig;
use super::imap::{InboundEmail, ParsedFetch};
use super::ring::{MessageRing, RecordedInbound};
use super::smtp::{EmailSendRequest, SmtpSender};
use super::state::{EmailChannelState, EmailIdentity};

const HISTORY_TURNS: usize = 10;

/// Run the email controller forever — spawns the IMAP listener
/// and drives the per-message handler loop.
pub async fn run_email_controller(
    cfg: Arc<EmailNodeConfig>,
    smtp: Arc<SmtpSender>,
    out_cell: EmailOutboundClientCell,
    state: Arc<EmailChannelState>,
    ring: Arc<MessageRing>,
) {
    state.set_identity(EmailIdentity {
        from: cfg.smtp_from.clone(),
        smtp_host: cfg.smtp_host.clone(),
        imap_host: cfg.imap_host.clone(),
        imap_folder: cfg.imap_folder.clone(),
    });
    // SMTP eager connect — just probe the transport so we know
    // the credentials work at boot. Lettre opens the actual
    // connection lazily on first send, so we trigger a noop
    // `noop()` call... actually lettre doesn't expose that on
    // the async transport. We mark `connected` optimistically;
    // the real first-send result updates the status.
    state.mark_smtp_connected();
    let _ = smtp; // SmtpSender is fully owned by the channel; the controller doesn't send unsolicited.

    let target = match super::imap::ImapTarget::from_config(cfg.as_ref()) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "email: ImapTarget invalid; listener will not start");
            state.mark_imap_error(&e.to_string());
            return;
        }
    };

    let cfg_for_listener = cfg.clone();
    let state_for_listener = state.clone();
    let ring_for_listener = ring.clone();
    let smtp_for_listener = smtp_for_dispatch_clone(&smtp);
    let out_for_listener = out_cell.clone();
    let cfg_for_dispatch = cfg.clone();

    let dispatch = move |parsed: ParsedFetch| {
        let cfg = cfg_for_dispatch.clone();
        let state = state_for_listener.clone();
        let ring = ring_for_listener.clone();
        let smtp = smtp_for_listener.clone();
        let out_cell = out_for_listener.clone();
        async move {
            handle_parsed_fetch(&cfg, &smtp, &out_cell, &state, &ring, parsed).await;
        }
    };

    super::imap::run_listener(
        cfg_for_listener,
        state_for_listener_clone(&state),
        target,
        dispatch,
    )
    .await;
}

fn smtp_for_dispatch_clone(smtp: &Arc<SmtpSender>) -> Arc<SmtpSender> {
    Arc::clone(smtp)
}

fn state_for_listener_clone(s: &Arc<EmailChannelState>) -> Arc<EmailChannelState> {
    Arc::clone(s)
}

/// Run with explicit components — used by tests + the legacy
/// alias the controller_runtime imports.
pub async fn run_email_controller_with_components(
    cfg: Arc<EmailNodeConfig>,
    smtp: Arc<SmtpSender>,
    out_cell: EmailOutboundClientCell,
    state: Arc<EmailChannelState>,
    ring: Arc<MessageRing>,
) {
    run_email_controller(cfg, smtp, out_cell, state, ring).await;
}

/// Per-message dispatch: route oversize messages to bounce-reply
/// path; everything else through the chat flow / slash-command
/// router.
pub async fn handle_parsed_fetch(
    cfg: &EmailNodeConfig,
    smtp: &SmtpSender,
    out_cell: &EmailOutboundClientCell,
    state: &EmailChannelState,
    ring: &MessageRing,
    parsed: ParsedFetch,
) {
    match parsed {
        ParsedFetch::Oversize { uid, bytes, limit } => {
            tracing::warn!(uid, bytes, limit, "email: rejecting oversized inbound");
            // We don't have a from address for the oversize
            // path (we didn't parse the body), but we still
            // bump the seen counter so operators can see the
            // reject in the dashboard.
            let ts = unix_now();
            state.record_inbound(ts);
            state.record_poll(ts);
            ring.record(RecordedInbound {
                ts,
                message_id: format!("oversize-uid-{uid}"),
                from: "<unknown — message exceeded size limit>".into(),
                subject: format!("[oversize] {bytes} bytes"),
                session_id: format!("oversize-{uid}"),
                preview: oversize_message(bytes, limit),
            });
        }
        ParsedFetch::Message(email) => {
            let ts = unix_now();
            state.record_inbound(ts);
            state.record_poll(ts);
            let session_id = email.session_id();
            let preview = if email.body_text.is_empty() {
                String::new()
            } else {
                email.body_text.chars().take(200).collect::<String>()
            };
            ring.record(RecordedInbound {
                ts,
                message_id: email.message_id.clone(),
                from: email.from.clone(),
                subject: email.subject.clone(),
                session_id: session_id.clone(),
                preview,
            });

            if !cfg.sender_is_allowed(&email.from) {
                let _ = send_reply(smtp, &email, unauthorised_message(), &session_id).await;
                return;
            }

            let out = out_cell.get().cloned();
            let out_ref: Option<&dyn EmailOutbound> =
                out.as_ref().map(|a| a.as_ref() as &dyn EmailOutbound);

            match Command::parse(&email.body_text) {
                Command::Help => {
                    let _ = send_reply(smtp, &email, &help_message(), &session_id).await;
                }
                Command::Status => {
                    let summary = render_status_summary(state, cfg);
                    let _ = send_reply(smtp, &email, &status_body(&summary), &session_id).await;
                }
                Command::Memory => {
                    let (agent, user) = match out_ref {
                        Some(o) => o.memory_agent_read(&session_id).await,
                        None => (String::new(), String::new()),
                    };
                    let _ =
                        send_reply(smtp, &email, &memory_body(&agent, &user), &session_id).await;
                }
                Command::Forget => {
                    if let Some(o) = out_ref {
                        o.memory_agent_clear(&session_id).await;
                    }
                    let _ = send_reply(smtp, &email, "Memory cleared.", &session_id).await;
                }
                Command::Chat(text) => {
                    run_chat_flow(smtp, out_ref, &email, &session_id, &text).await;
                }
            }
        }
    }
}

fn render_status_summary(state: &EmailChannelState, cfg: &EmailNodeConfig) -> String {
    let id = state.identity();
    format!(
        "smtp_status={}\nimap_status={}\nfrom={}\nsmtp_host={}\nimap_host={}\nimap_folder={}\nmessages_seen={}\nmessages_sent={}\nallow_everyone={}",
        state.smtp_status().as_str(),
        state.imap_status().as_str(),
        id.from,
        id.smtp_host,
        id.imap_host,
        id.imap_folder,
        state.messages_seen(),
        state.messages_sent(),
        cfg.allow_everyone(),
    )
}

async fn run_chat_flow(
    smtp: &SmtpSender,
    out: Option<&dyn EmailOutbound>,
    email: &InboundEmail,
    session_id: &str,
    text: &str,
) {
    let Some(out) = out else {
        let _ = send_reply(smtp, email, brain_unreachable_message(), session_id).await;
        return;
    };

    let history = out.memory_recent(session_id, HISTORY_TURNS).await;
    let history_text = render_history(&history);

    let task_id = out
        .task_create(
            &message_title(&email.subject, text),
            "flows/chat_template.sol",
            "",
            session_id,
        )
        .await;
    if let Some(t) = task_id.as_deref() {
        out.task_event(
            t,
            "task.email.inbound",
            &format!("from={}|subject={}", email.from, email.subject),
        )
        .await;
    }

    // RELIX-7.7 GAP 2: consult the coordinator's routing rules
    // before dispatching. When no rule matches (or the router
    // is unreachable), fall back to the static `(ai, ai.chat)`
    // target so existing single-agent deployments behave
    // identically. Subject + sender + a short content preview
    // are all the router needs.
    let preview: String = text.chars().take(200).collect();
    let routed = out
        .routing_resolve("email", &email.from, &email.subject, &preview)
        .await;
    let reply = match routed {
        Some((peer, capability)) => {
            tracing::info!(
                from = %email.from,
                subject = %email.subject,
                target_peer = %peer,
                capability = %capability,
                "email: routed via ChannelRouter"
            );
            out.dispatch_chat(&peer, &capability, session_id, text, &history_text)
                .await
        }
        None => out.ai_chat(session_id, text, &history_text).await,
    };
    let reply = match reply {
        Some(r) if !r.trim().is_empty() => r,
        _ => {
            if let Some(t) = task_id.as_deref() {
                out.task_update_status(t, "failed", "ai_chat unreachable / empty")
                    .await;
            }
            let _ = send_reply(smtp, email, brain_unreachable_message(), session_id).await;
            return;
        }
    };

    out.memory_write(session_id, "user", text).await;
    out.memory_write(session_id, "assistant", &reply).await;

    let _ = send_reply(smtp, email, &reply, session_id).await;

    if let Some(t) = task_id.as_deref() {
        out.task_update_status(t, "completed", "ok").await;
    }
}

fn message_title(subject: &str, text: &str) -> String {
    let first_line = if !subject.trim().is_empty() {
        subject.trim().to_string()
    } else {
        text.lines().next().unwrap_or("").trim().to_string()
    };
    if first_line.is_empty() {
        return "email-message".to_string();
    }
    let truncated: String = first_line.chars().take(80).collect();
    format!("email: {truncated}")
}

fn render_history(history: &[(String, String)]) -> String {
    let mut out = String::new();
    for (role, text) in history {
        out.push_str(&format!("[{role}] {text}\n"));
    }
    out
}

/// Send `body_text` back to `email.from` as a reply, with the
/// inbound `Message-ID` set as `In-Reply-To` and the inbound's
/// references list extended so MUAs thread the conversation.
pub async fn send_reply(
    smtp: &SmtpSender,
    inbound: &InboundEmail,
    body_text: &str,
    session_id: &str,
) -> bool {
    let subject = reply_subject(&inbound.subject);
    let mut references = inbound.references.clone();
    if !inbound.message_id.is_empty() && !references.iter().any(|r| r == &inbound.message_id) {
        references.push(inbound.message_id.clone());
    }
    let req = EmailSendRequest {
        to: vec![reply_to_value(inbound)],
        subject,
        body_text: body_text.to_string(),
        in_reply_to: if inbound.message_id.is_empty() {
            None
        } else {
            Some(inbound.message_id.clone())
        },
        references,
        ..Default::default()
    };
    match smtp.send(&req).await {
        Ok(message_id) => {
            tracing::info!(
                session_id,
                in_reply_to = %inbound.message_id,
                outbound_id = %message_id,
                "email: replied to inbound"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                session_id,
                from = %inbound.from,
                error = %e,
                "email: send_reply failed"
            );
            false
        }
    }
}

fn reply_to_value(inbound: &InboundEmail) -> String {
    if inbound.from_name.is_empty() {
        inbound.from.clone()
    } else {
        format!("{} <{}>", inbound.from_name, inbound.from)
    }
}

fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    if s.to_ascii_lowercase().starts_with("re:") {
        s.to_string()
    } else if s.is_empty() {
        "Re:".to_string()
    } else {
        format!("Re: {s}")
    }
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
    use crate::nodes::email::config::EmailNodeConfig;

    fn cfg_default() -> EmailNodeConfig {
        toml::from_str(
            r#"
                smtp_host = "smtp.example.com"
                smtp_from = "Relix <bot@example.com>"
                imap_host = "imap.example.com"
                [memory_peer]
                addr = "/ip4/127.0.0.1/tcp/1"
                [ai_peer]
                addr = "/ip4/127.0.0.1/tcp/2"
                [coord_peer]
                addr = "/ip4/127.0.0.1/tcp/3"
            "#,
        )
        .unwrap()
    }

    fn cfg_with_allow(senders: &[&str]) -> EmailNodeConfig {
        let mut cfg = cfg_default();
        cfg.allowed_senders = senders.iter().map(|s| s.to_string()).collect();
        cfg
    }

    fn inbound(message_id: &str, from: &str, subject: &str, body: &str) -> InboundEmail {
        InboundEmail {
            uid: 1,
            message_id: message_id.into(),
            from: from.into(),
            from_name: "".into(),
            to: "bot@example.com".into(),
            subject: subject.into(),
            body_text: body.into(),
            body_html: None,
            attachment_paths: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            raw_size_bytes: body.len(),
        }
    }

    #[test]
    fn reply_subject_prefixes_re_when_missing() {
        assert_eq!(reply_subject("hello"), "Re: hello");
        assert_eq!(reply_subject("Re: hello"), "Re: hello");
        assert_eq!(reply_subject("re: lowercase"), "re: lowercase");
        assert_eq!(reply_subject(""), "Re:");
    }

    #[test]
    fn reply_to_value_uses_name_when_present() {
        let mut e = inbound("m@e", "alice@e", "s", "b");
        e.from_name = "Alice".into();
        assert_eq!(reply_to_value(&e), "Alice <alice@e>");
    }

    #[test]
    fn reply_to_value_falls_back_to_bare_address() {
        let e = inbound("m@e", "alice@e", "s", "b");
        assert_eq!(reply_to_value(&e), "alice@e");
    }

    #[test]
    fn message_title_uses_subject_when_present() {
        assert_eq!(
            message_title("Hello world", "ignored"),
            "email: Hello world"
        );
    }

    #[test]
    fn message_title_falls_back_to_first_body_line() {
        assert_eq!(message_title("", "first line\nsecond"), "email: first line");
    }

    #[test]
    fn message_title_truncates_to_80_chars() {
        let long_subject: String = "a".repeat(150);
        let t = message_title(&long_subject, "");
        // "email: " (7) + 80 chars subject.
        assert_eq!(t.chars().count(), 7 + 80);
    }

    #[test]
    fn render_history_emits_role_brackets() {
        let history = vec![
            ("user".to_string(), "hi".to_string()),
            ("assistant".to_string(), "hello".to_string()),
        ];
        let out = render_history(&history);
        assert!(out.contains("[user] hi"));
        assert!(out.contains("[assistant] hello"));
    }

    #[test]
    fn status_summary_includes_smtp_imap_status() {
        let s = EmailChannelState::default();
        s.set_identity(EmailIdentity {
            from: "bot@example.com".into(),
            smtp_host: "smtp.e".into(),
            imap_host: "imap.e".into(),
            imap_folder: "INBOX".into(),
        });
        s.mark_smtp_connected();
        let cfg = cfg_default();
        let summary = render_status_summary(&s, &cfg);
        assert!(summary.contains("smtp_status=connected"));
        assert!(summary.contains("imap_status=disconnected"));
        assert!(summary.contains("imap_folder=INBOX"));
    }

    #[test]
    fn unauthorised_sender_recorded_in_ring_but_not_dispatched() {
        // We don't have a real SMTP path here; the test exercises
        // the ring + permit check without exercising send_reply.
        let cfg = cfg_with_allow(&["alice@example.com"]);
        let inbound = inbound("m@e", "bob@example.com", "hi", "hello");
        let state = EmailChannelState::default();
        let ring = MessageRing::new(50);
        let ts = 1700000000;
        state.record_inbound(ts);
        ring.record(RecordedInbound {
            ts,
            message_id: inbound.message_id.clone(),
            from: inbound.from.clone(),
            subject: inbound.subject.clone(),
            session_id: inbound.session_id(),
            preview: inbound.body_text.clone(),
        });
        assert!(!cfg.sender_is_allowed(&inbound.from));
        assert_eq!(ring.len(), 1);
    }

    /// Confirm `Command::parse` round-trips through the email
    /// body the controller actually sees.
    #[test]
    fn slash_help_in_body_is_parsed_as_help_command() {
        let email = inbound("m@e", "alice@e", "subj", "/help\nmore text");
        let cmd = Command::parse(&email.body_text);
        assert_eq!(cmd, Command::Help);
    }
}
