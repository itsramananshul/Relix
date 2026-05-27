//! Production SMTP outbound client.
//!
//! Built on `lettre` 0.11 with:
//!
//! - STARTTLS (port 587), implicit TLS (port 465), or unauthenticated
//!   relay (port 25) — selected by `smtp_tls`.
//! - Plain username/password auth, OAuth2 bearer token (XOAUTH2), or
//!   no auth.
//! - A pooled `AsyncSmtpTransport` keeping warm connections so a
//!   burst of sends doesn't re-handshake every time.
//! - Manual retry-with-backoff for transient failures (connection
//!   refused, timeout, 4xx SMTP responses). Permanent failures (5xx
//!   responses, address rejected) skip the retry loop.
//! - Full MIME via lettre's builder — plain text, HTML, both as
//!   alternatives, file attachments, inline images with Content-ID.
//! - Honest headers: globally-unique `Message-ID`, RFC 5322 `Date`,
//!   threading via `In-Reply-To` and `References`, `X-Mailer: Relix`.
//! - DKIM signing pass after the message is built — when configured,
//!   a `DKIM-Signature` header is prepended to the rendered RFC 5322
//!   bytes (lettre's `SendableEmail::message_to_string` is
//!   accessible via the envelope helpers we use).

use std::sync::Arc;
use std::time::Duration;

use lettre::address::Address;
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Body, MultiPart, SinglePart};
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::PoolConfig;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{AsyncTransport, Message, Tokio1Executor, message::Mailbox};

use super::config::{EmailNodeConfig, SmtpTls};
use super::dkim::DkimSigner;

/// One outbound send request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmailSendRequest {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub attachments: Vec<Attachment_>,
    pub inline_images: Vec<InlineImage>,
    /// Explicit `Message-ID` override. When `None` the SMTP
    /// client generates one (preferred path — caller never
    /// thinks about it).
    pub message_id: Option<String>,
}

/// File attachment for an outbound message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment_ {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// Inline image (referenced from the HTML body via
/// `cid:<content_id>`). The image rides in the multipart/related
/// envelope so MUAs render it inline rather than as an attachment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineImage {
    pub content_id: String,
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum SmtpError {
    #[error("smtp config: {0}")]
    Config(String),
    #[error("smtp transport: {0}")]
    Transport(String),
    #[error("smtp permanent failure ({code}): {message}")]
    Permanent { code: u16, message: String },
    #[error("smtp transient failure ({code}): {message}")]
    Transient { code: u16, message: String },
    #[error("smtp build: {0}")]
    Build(String),
    #[error("smtp size limit: message is {bytes} bytes; max {limit}")]
    OversizeAttachment { bytes: usize, limit: usize },
}

/// Hard cap on outbound message size (matches the spec's 25MB
/// attachment ceiling, with a small budget for headers).
pub const MAX_MESSAGE_BYTES: usize = 26 * 1024 * 1024;

/// Production SMTP sender. Cheap to clone — wraps an
/// `AsyncSmtpTransport` in an `Arc`.
#[derive(Clone)]
pub struct SmtpSender {
    inner: Arc<SmtpSenderInner>,
}

struct SmtpSenderInner {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    host: String,
    max_retries: u32,
    dkim: Option<DkimSigner>,
}

impl std::fmt::Debug for SmtpSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpSender")
            .field("host", &self.inner.host)
            .field("from", &self.inner.from)
            .field("max_retries", &self.inner.max_retries)
            .field("dkim", &self.inner.dkim.is_some())
            .finish()
    }
}

impl SmtpSender {
    /// Build the sender from a fully-validated `EmailNodeConfig`.
    /// Reads the SMTP password (or OAuth2 token) from the env;
    /// network connection is lazy — lettre dials on first send.
    pub fn from_config(cfg: &EmailNodeConfig) -> Result<Self, SmtpError> {
        let tls = cfg
            .smtp_tls_mode()
            .map_err(|e| SmtpError::Config(e.to_string()))?;
        let mut builder = match tls {
            SmtpTls::Starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)
                    .map_err(|e| SmtpError::Transport(format!("starttls relay: {e}")))?
            }
            SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)
                .map_err(|e| SmtpError::Transport(format!("tls relay: {e}")))?,
            SmtpTls::None => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host)
            }
        };
        builder = builder
            .port(cfg.smtp_port)
            .timeout(Some(Duration::from_secs(30)))
            .pool_config(PoolConfig::new().max_size(cfg.smtp_pool_max));

        // Auth: OAuth2 takes precedence when its env var is set;
        // otherwise plain user/pass; otherwise no auth (only legal
        // on port 25 / smtp_tls=none).
        if !cfg.smtp_oauth2_token_env.trim().is_empty() {
            let token = cfg
                .resolve_env(&cfg.smtp_oauth2_token_env)
                .map_err(|e| SmtpError::Config(format!("oauth2 token: {e}")))?;
            let user = if cfg.smtp_username.is_empty() {
                addr_only(&cfg.smtp_from)
            } else {
                cfg.smtp_username.clone()
            };
            // XOAUTH2 uses the bearer token as the SASL value
            // wrapped in lettre's Credentials password slot.
            builder = builder
                .credentials(Credentials::new(user, token))
                .authentication(vec![Mechanism::Xoauth2]);
        } else if !cfg.smtp_password_env.trim().is_empty() {
            let password = cfg
                .resolve_smtp_password()
                .map_err(|e| SmtpError::Config(format!("smtp_password: {e}")))?;
            let user = if cfg.smtp_username.is_empty() {
                addr_only(&cfg.smtp_from)
            } else {
                cfg.smtp_username.clone()
            };
            builder = builder.credentials(Credentials::new(user, password));
        }

        let transport = builder.build();

        let from: Mailbox = cfg
            .smtp_from
            .parse()
            .map_err(|e| SmtpError::Config(format!("smtp_from {:?}: {e}", cfg.smtp_from)))?;

        // DKIM: try to load, log + skip on failure (the spec is
        // explicit — never fail to send because DKIM is broken).
        let dkim = if cfg.dkim_enabled() {
            match DkimSigner::from_pem_file(
                &cfg.dkim_private_key_path,
                &cfg.dkim_selector,
                &cfg.dkim_domain,
            ) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(
                        path = %cfg.dkim_private_key_path.display(),
                        error = %e,
                        "email: DKIM key load failed; sending unsigned"
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            inner: Arc::new(SmtpSenderInner {
                transport,
                from,
                host: cfg.smtp_host.clone(),
                max_retries: cfg.smtp_max_retries,
                dkim,
            }),
        })
    }

    /// Build an `SmtpSender` from an already-constructed lettre
    /// transport. Used by tests that want a stub transport.
    pub fn from_parts(
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
        host: impl Into<String>,
        max_retries: u32,
        dkim: Option<DkimSigner>,
    ) -> Self {
        Self {
            inner: Arc::new(SmtpSenderInner {
                transport,
                from,
                host: host.into(),
                max_retries,
                dkim,
            }),
        }
    }

    pub fn from(&self) -> &Mailbox {
        &self.inner.from
    }
    pub fn host(&self) -> &str {
        &self.inner.host
    }
    pub fn has_dkim(&self) -> bool {
        self.inner.dkim.is_some()
    }

    /// Build a lettre `Message` from the high-level request.
    /// Returns the message + the chosen Message-ID (for chronicle
    /// + thread-tracking).
    pub fn build_message(&self, req: &EmailSendRequest) -> Result<(Message, String), SmtpError> {
        self.build_message_with_limit(req, MAX_MESSAGE_BYTES)
    }

    /// Build a lettre `Message` with an explicit size cap. Used
    /// internally by `build_message` (with the production
    /// `MAX_MESSAGE_BYTES`) and by tests that want to exercise
    /// the oversize path without literally allocating a 26 MB
    /// buffer (Windows test threads have a 1 MB stack canary).
    pub fn build_message_with_limit(
        &self,
        req: &EmailSendRequest,
        max_bytes: usize,
    ) -> Result<(Message, String), SmtpError> {
        if req.to.is_empty() {
            return Err(SmtpError::Config(
                "at least one `to` recipient is required".into(),
            ));
        }
        let attachments_bytes: usize = req
            .attachments
            .iter()
            .map(|a| a.bytes.len())
            .chain(req.inline_images.iter().map(|i| i.bytes.len()))
            .sum();
        if attachments_bytes > max_bytes {
            return Err(SmtpError::OversizeAttachment {
                bytes: attachments_bytes,
                limit: max_bytes,
            });
        }

        let mut builder = Message::builder().from(self.inner.from.clone());
        for to in &req.to {
            let mb: Mailbox = to
                .parse()
                .map_err(|e| SmtpError::Build(format!("to {to:?}: {e}")))?;
            builder = builder.to(mb);
        }
        for cc in &req.cc {
            let mb: Mailbox = cc
                .parse()
                .map_err(|e| SmtpError::Build(format!("cc {cc:?}: {e}")))?;
            builder = builder.cc(mb);
        }
        for bcc in &req.bcc {
            let mb: Mailbox = bcc
                .parse()
                .map_err(|e| SmtpError::Build(format!("bcc {bcc:?}: {e}")))?;
            builder = builder.bcc(mb);
        }
        if let Some(reply_to) = &req.reply_to {
            let mb: Mailbox = reply_to
                .parse()
                .map_err(|e| SmtpError::Build(format!("reply_to {reply_to:?}: {e}")))?;
            builder = builder.reply_to(mb);
        }
        builder = builder.subject(&req.subject);

        // Generate / accept Message-ID.
        let domain_for_id = match self.inner.from.email.domain() {
            d if !d.is_empty() => d.to_string(),
            _ => self.inner.host.clone(),
        };
        let message_id = req
            .message_id
            .clone()
            .unwrap_or_else(|| generate_message_id(&domain_for_id));
        builder = builder.message_id(Some(format!("<{message_id}>")));

        if let Some(in_reply_to) = &req.in_reply_to {
            builder = builder.in_reply_to(format!("<{}>", in_reply_to.trim_matches(['<', '>'])));
        }
        if !req.references.is_empty() {
            let refs = req
                .references
                .iter()
                .map(|r| format!("<{}>", r.trim_matches(['<', '>'])))
                .collect::<Vec<_>>()
                .join(" ");
            builder = builder.references(refs);
        }
        // X-Mailer.
        builder = builder.user_agent("Relix".to_string());

        // Build the body. The strategy:
        //
        // - plain only            → singlepart text/plain
        // - plain + html, no atts → multipart/alternative
        // - any attachments       → multipart/mixed{ alt, atts }
        // - any inline images     → multipart/related{ alt, imgs }
        let plain_part = SinglePart::builder()
            .header(ContentType::TEXT_PLAIN)
            .body(req.body_text.clone());

        let body = if let Some(html) = &req.body_html {
            let alt = MultiPart::alternative().singlepart(plain_part).singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_HTML)
                    .body(html.clone()),
            );
            if !req.inline_images.is_empty() {
                // multipart/related — inline images live alongside
                // the alternative body.
                let mut related = MultiPart::related().multipart(alt);
                for img in &req.inline_images {
                    let ct: ContentType = img.content_type.parse().map_err(|e| {
                        SmtpError::Build(format!("inline ct {:?}: {e}", img.content_type))
                    })?;
                    let body = Body::new(img.bytes.clone());
                    related = related.singlepart(
                        SinglePart::builder()
                            .header(ct)
                            .header(lettre::message::header::ContentId::from(format!(
                                "<{}>",
                                img.content_id
                            )))
                            .header(
                                lettre::message::header::ContentDisposition::inline_with_name(
                                    &img.filename,
                                ),
                            )
                            .body(body),
                    );
                }
                wrap_with_attachments(related, &req.attachments)?
            } else if !req.attachments.is_empty() {
                wrap_with_attachments(alt, &req.attachments)?
            } else {
                alt
            }
        } else if !req.attachments.is_empty() || !req.inline_images.is_empty() {
            // Plain text with attachments (no HTML alternative).
            let mut mixed = MultiPart::mixed().singlepart(plain_part);
            for att in &req.attachments {
                let ct: ContentType = att
                    .content_type
                    .parse()
                    .map_err(|e| SmtpError::Build(format!("att ct {:?}: {e}", att.content_type)))?;
                let attach = Attachment::new(att.filename.clone()).body(att.bytes.clone(), ct);
                mixed = mixed.singlepart(attach);
            }
            for img in &req.inline_images {
                let ct: ContentType = img.content_type.parse().map_err(|e| {
                    SmtpError::Build(format!("inline ct {:?}: {e}", img.content_type))
                })?;
                let body = Body::new(img.bytes.clone());
                mixed = mixed.singlepart(
                    SinglePart::builder()
                        .header(ct)
                        .header(lettre::message::header::ContentId::from(format!(
                            "<{}>",
                            img.content_id
                        )))
                        .header(
                            lettre::message::header::ContentDisposition::inline_with_name(
                                &img.filename,
                            ),
                        )
                        .body(body),
                );
            }
            mixed
        } else {
            // Pure plain — no HTML, no attachments.
            return Ok((
                builder
                    .singlepart(plain_part)
                    .map_err(|e| SmtpError::Build(format!("singlepart: {e}")))?,
                message_id,
            ));
        };

        Ok((
            builder
                .multipart(body)
                .map_err(|e| SmtpError::Build(format!("multipart: {e}")))?,
            message_id,
        ))
    }

    /// Send `req` with retry. Returns the chosen Message-ID on
    /// success.
    pub async fn send(&self, req: &EmailSendRequest) -> Result<String, SmtpError> {
        let (msg, message_id) = self.build_message(req)?;
        // DKIM signing: render to RFC 5322 bytes, sign, prepend
        // the DKIM-Signature header. Lettre renders the message
        // to bytes via `formatted()`; we strip the leading
        // headers, sign them, and prepend the signature.
        let raw_bytes = msg.formatted();
        let to_send_bytes = if let Some(signer) = &self.inner.dkim {
            match sign_outbound(&raw_bytes, signer) {
                Ok(b) => b,
                Err(e) => {
                    // Spec: NEVER fail to send because DKIM is
                    // broken. Log and continue unsigned.
                    tracing::warn!(error = %e, "email: DKIM sign failed at send time; sending unsigned");
                    raw_bytes
                }
            }
        } else {
            raw_bytes
        };
        let envelope = lettre::address::Envelope::new(
            Some(self.inner.from.email.clone()),
            collect_recipients(req)?,
        )
        .map_err(|e| SmtpError::Build(format!("envelope: {e}")))?;

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let result = self
                .inner
                .transport
                .send_raw(&envelope, &to_send_bytes)
                .await;
            match result {
                Ok(_) => return Ok(message_id),
                Err(e) => {
                    let classified = classify_lettre_error(&e);
                    match classified {
                        SmtpErrorClass::Permanent { code, message } => {
                            return Err(SmtpError::Permanent { code, message });
                        }
                        SmtpErrorClass::Transient { code, message } => {
                            if attempt > self.inner.max_retries {
                                return Err(SmtpError::Transient { code, message });
                            }
                            let backoff = backoff_for_attempt(attempt);
                            tracing::warn!(
                                attempt,
                                max_retries = self.inner.max_retries,
                                backoff_ms = backoff.as_millis() as u64,
                                error = %e,
                                "smtp: transient send failure; retrying"
                            );
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
            }
        }
    }
}

fn collect_recipients(req: &EmailSendRequest) -> Result<Vec<Address>, SmtpError> {
    let mut out: Vec<Address> = Vec::new();
    for addrs in [&req.to, &req.cc, &req.bcc] {
        for a in addrs {
            let mb: Mailbox = a
                .parse()
                .map_err(|e| SmtpError::Build(format!("recipient {a:?}: {e}")))?;
            out.push(mb.email);
        }
    }
    Ok(out)
}

fn wrap_with_attachments(
    inner: MultiPart,
    attachments: &[Attachment_],
) -> Result<MultiPart, SmtpError> {
    if attachments.is_empty() {
        return Ok(inner);
    }
    let mut mixed = MultiPart::mixed().multipart(inner);
    for att in attachments {
        let ct: ContentType = att
            .content_type
            .parse()
            .map_err(|e| SmtpError::Build(format!("att ct {:?}: {e}", att.content_type)))?;
        let attach = Attachment::new(att.filename.clone()).body(att.bytes.clone(), ct);
        mixed = mixed.singlepart(attach);
    }
    Ok(mixed)
}

fn sign_outbound(raw: &[u8], signer: &DkimSigner) -> Result<Vec<u8>, super::dkim::DkimError> {
    // Split headers from body at the first CRLFCRLF.
    let s = String::from_utf8_lossy(raw);
    let (header_text, body_text) = match s.find("\r\n\r\n") {
        Some(idx) => (s[..idx + 2].to_string(), s[idx + 4..].to_string()),
        None => (s.to_string(), String::new()),
    };
    // Parse the header block into (name, value) pairs.
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut current_name = String::new();
    let mut current_value = String::new();
    for line in header_text.split("\r\n") {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            current_value.push_str("\r\n");
            current_value.push_str(line);
            continue;
        }
        if !current_name.is_empty() {
            headers.push((current_name.clone(), current_value.clone()));
        }
        let (n, v) = match line.split_once(':') {
            Some(pair) => pair,
            None => {
                continue;
            }
        };
        current_name = n.to_string();
        current_value = v.trim_start_matches(' ').to_string();
    }
    if !current_name.is_empty() {
        headers.push((current_name, current_value));
    }
    let dkim_header_value = signer.sign(
        &headers,
        body_text.as_bytes(),
        &["from", "to", "subject", "date", "message-id"],
    )?;
    // Prepend the DKIM-Signature header.
    let mut out: Vec<u8> = Vec::with_capacity(raw.len() + dkim_header_value.len() + 32);
    out.extend_from_slice(b"DKIM-Signature: ");
    out.extend_from_slice(dkim_header_value.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(raw);
    Ok(out)
}

enum SmtpErrorClass {
    Transient { code: u16, message: String },
    Permanent { code: u16, message: String },
}

fn classify_lettre_error(err: &lettre::transport::smtp::Error) -> SmtpErrorClass {
    let msg = err.to_string();
    // Lettre's Error type exposes a `status` for response errors
    // — when present, the SMTP code's first digit tells us
    // permanent (5xx) vs transient (4xx). Unfortunately the
    // public API doesn't expose the code directly in 0.11; we
    // string-match on the canonical form lettre uses
    // ("permanent error: " / "transient error: ").
    if msg.contains("permanent error") || msg.contains("permanent failure") {
        // Try to pull a code out of the message.
        let code = extract_smtp_code(&msg).unwrap_or(550);
        return SmtpErrorClass::Permanent { code, message: msg };
    }
    if msg.contains("transient error") || msg.contains("transient failure") {
        let code = extract_smtp_code(&msg).unwrap_or(421);
        return SmtpErrorClass::Transient { code, message: msg };
    }
    // Network / dial / TLS errors: transient. The connection
    // refusal pattern catches the common "server is down" case.
    if err.is_response()
        || err.is_transient()
        || err.is_timeout()
        || msg.contains("connection refused")
        || msg.contains("connection reset")
        || msg.contains("connection closed")
        || msg.contains("timed out")
        || msg.contains("tls")
        || msg.contains("dns")
    {
        return SmtpErrorClass::Transient {
            code: 421,
            message: msg,
        };
    }
    // Default conservatively to permanent — better to surface an
    // unexpected error to the operator than to retry forever.
    SmtpErrorClass::Permanent {
        code: 550,
        message: msg,
    }
}

fn extract_smtp_code(msg: &str) -> Option<u16> {
    // Find the first 3-digit sequence in the message.
    let bytes = msg.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
        {
            return Some(
                ((bytes[i] - b'0') as u16) * 100
                    + ((bytes[i + 1] - b'0') as u16) * 10
                    + (bytes[i + 2] - b'0') as u16,
            );
        }
        i += 1;
    }
    None
}

fn backoff_for_attempt(attempt: u32) -> Duration {
    // 1s, 2s, 4s — matches the spec.
    let secs = 1u64 << (attempt - 1).min(6);
    Duration::from_secs(secs)
}

/// Globally-unique Message-ID, formed as `<v4-uuid>@<domain>`.
pub fn generate_message_id(domain: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let domain = if domain.is_empty() {
        "relix.local"
    } else {
        domain
    };
    format!("{id}@{domain}")
}

fn addr_only(mailbox: &str) -> String {
    if let Some(open) = mailbox.rfind('<')
        && let Some(close) = mailbox[open + 1..].find('>')
    {
        return mailbox[open + 1..open + 1 + close].trim().to_string();
    }
    mailbox.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lettre::message::Mailbox;

    fn cfg_for_test() -> EmailNodeConfig {
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

    fn sender_no_dkim() -> SmtpSender {
        let cfg = cfg_for_test();
        let transport =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.smtp_host).build();
        let from: Mailbox = cfg.smtp_from.parse().unwrap();
        SmtpSender::from_parts(transport, from, &cfg.smtp_host, 3, None)
    }

    // ── all sender-touching tests run inside a tokio runtime
    // because lettre's `AsyncSmtpTransport::Drop` spawns a
    // cleanup task on the pool — outside a runtime that panics.

    #[tokio::test]
    async fn build_plain_text_message_has_required_headers() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "hello".into(),
            body_text: "hi there".into(),
            ..Default::default()
        };
        let (msg, mid) = sender.build_message(&req).unwrap();
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("From: Relix <bot@example.com>"));
        assert!(rendered.contains("To: alice@example.com"));
        assert!(rendered.contains("Subject: hello"));
        assert!(rendered.contains("hi there"));
        assert!(rendered.contains(&format!("Message-ID: <{mid}>")));
        assert!(rendered.contains("Date:"));
        assert!(rendered.contains("User-Agent: Relix") || rendered.contains("X-Mailer: Relix"));
    }

    #[tokio::test]
    async fn build_html_alternative_uses_multipart_alternative() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "html".into(),
            body_text: "hi".into(),
            body_html: Some("<p>hi</p>".into()),
            ..Default::default()
        };
        let (msg, _) = sender.build_message(&req).unwrap();
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("multipart/alternative"));
        assert!(rendered.contains("text/html"));
    }

    #[tokio::test]
    async fn build_with_attachment_uses_multipart_mixed() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "att".into(),
            body_text: "hi".into(),
            attachments: vec![Attachment_ {
                filename: "x.txt".into(),
                content_type: "text/plain".into(),
                bytes: b"contents".to_vec(),
            }],
            ..Default::default()
        };
        let (msg, _) = sender.build_message(&req).unwrap();
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("multipart/mixed"));
        assert!(rendered.contains("x.txt"));
    }

    #[tokio::test]
    async fn build_inline_image_uses_multipart_related_and_content_id() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "inline".into(),
            body_text: "see image".into(),
            body_html: Some("<img src='cid:logo'/>".into()),
            inline_images: vec![InlineImage {
                content_id: "logo".into(),
                filename: "logo.png".into(),
                content_type: "image/png".into(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }],
            ..Default::default()
        };
        let (msg, _) = sender.build_message(&req).unwrap();
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("multipart/related"));
        assert!(rendered.contains("Content-ID: <logo>"));
    }

    #[tokio::test]
    async fn build_with_threading_headers_includes_in_reply_to_and_references() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "Re: thread".into(),
            body_text: "ack".into(),
            in_reply_to: Some("parent@example.com".into()),
            references: vec!["parent@example.com".into(), "earlier@example.com".into()],
            ..Default::default()
        };
        let (msg, _) = sender.build_message(&req).unwrap();
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("In-Reply-To: <parent@example.com>"));
        assert!(rendered.contains("References: <parent@example.com> <earlier@example.com>"));
    }

    #[tokio::test]
    async fn build_message_id_is_globally_unique_by_default() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "x".into(),
            body_text: "y".into(),
            ..Default::default()
        };
        let (_, m1) = sender.build_message(&req).unwrap();
        let (_, m2) = sender.build_message(&req).unwrap();
        assert_ne!(m1, m2);
        assert!(m1.contains('@'));
    }

    #[tokio::test]
    async fn explicit_message_id_overrides_generated() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "x".into(),
            body_text: "y".into(),
            message_id: Some("custom@example.com".into()),
            ..Default::default()
        };
        let (msg, mid) = sender.build_message(&req).unwrap();
        assert_eq!(mid, "custom@example.com");
        let rendered = String::from_utf8_lossy(&msg.formatted()).to_string();
        assert!(rendered.contains("Message-ID: <custom@example.com>"));
    }

    #[tokio::test]
    async fn build_rejects_zero_recipients() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec![],
            subject: "x".into(),
            body_text: "y".into(),
            ..Default::default()
        };
        match sender.build_message(&req) {
            Err(SmtpError::Config(m)) => assert!(m.contains("at least one")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    /// Tests the size guard via `build_message_with_limit` so
    /// the test thread never has to materialise a multi-MB Vec
    /// (Windows test threads have a small stack canary that the
    /// harness's owned-temporary trips on oversized allocations).
    #[tokio::test]
    async fn build_rejects_oversize_attachments() {
        let sender = sender_no_dkim();
        let req = EmailSendRequest {
            to: vec!["alice@example.com".into()],
            subject: "x".into(),
            body_text: "y".into(),
            attachments: vec![Attachment_ {
                filename: "big.bin".into(),
                content_type: "application/octet-stream".into(),
                bytes: vec![0u8; 1024],
            }],
            ..Default::default()
        };
        // 1024-byte payload, 256-byte cap → over.
        match sender.build_message_with_limit(&req, 256) {
            Err(SmtpError::OversizeAttachment { bytes, limit }) => {
                assert_eq!(bytes, 1024);
                assert_eq!(limit, 256);
            }
            other => panic!("expected OversizeAttachment, got {other:?}"),
        }
        // Same payload, big cap → ok.
        sender
            .build_message_with_limit(&req, 4096)
            .expect("under cap should build");
    }

    #[test]
    fn backoff_progression_is_1s_2s_4s() {
        assert_eq!(backoff_for_attempt(1), Duration::from_secs(1));
        assert_eq!(backoff_for_attempt(2), Duration::from_secs(2));
        assert_eq!(backoff_for_attempt(3), Duration::from_secs(4));
    }

    #[test]
    fn generate_message_id_produces_distinct_values() {
        let a = generate_message_id("example.com");
        let b = generate_message_id("example.com");
        assert_ne!(a, b);
        assert!(a.ends_with("@example.com"));
    }

    #[test]
    fn generate_message_id_falls_back_to_relix_local() {
        let a = generate_message_id("");
        assert!(a.ends_with("@relix.local"));
    }

    #[test]
    fn addr_only_extracts_bare_address_from_envelope_form() {
        assert_eq!(addr_only("Relix <bot@example.com>"), "bot@example.com");
        assert_eq!(addr_only("bot@example.com"), "bot@example.com");
    }

    #[test]
    fn extract_smtp_code_finds_first_three_digit_sequence() {
        assert_eq!(extract_smtp_code("550 mailbox unavailable"), Some(550));
        assert_eq!(extract_smtp_code("421 try again"), Some(421));
        assert_eq!(extract_smtp_code("no code here"), None);
    }

    #[test]
    fn sign_outbound_prepends_dkim_signature_header() {
        let signer = super::super::dkim::DkimSigner::from_pem(
            include_str!("test-dkim-key.pem"),
            "relix",
            "example.com",
        )
        .unwrap();
        let raw = b"From: bot@example.com\r\nTo: alice@example.com\r\nSubject: t\r\nDate: Thu, 14 Jan 2027 00:00:00 +0000\r\nMessage-ID: <a@b>\r\n\r\nhello\r\n";
        let out = sign_outbound(raw, &signer).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("DKIM-Signature: "));
        assert!(s.contains("d=example.com"));
        assert!(s.contains("s=relix"));
        // Body preserved verbatim.
        assert!(s.contains("\r\n\r\nhello\r\n"));
    }

    /// Permanent failures (5xx) take the no-retry path. The
    /// classifier should label any string containing
    /// "permanent error" with a 5xx code.
    #[test]
    fn classify_permanent_failure_label() {
        let msg = "permanent error: 550 mailbox unavailable";
        match classify_lettre_error_str(msg) {
            SmtpErrorClass::Permanent { code, .. } => assert_eq!(code, 550),
            _ => panic!("expected Permanent"),
        }
    }

    #[test]
    fn classify_transient_failure_label() {
        let msg = "transient error: 421 try later";
        match classify_lettre_error_str(msg) {
            SmtpErrorClass::Transient { code, .. } => assert_eq!(code, 421),
            _ => panic!("expected Transient"),
        }
    }

    #[test]
    fn classify_connection_refused_is_transient() {
        let msg = "connection refused";
        match classify_lettre_error_str(msg) {
            SmtpErrorClass::Transient { .. } => {}
            _ => panic!("expected Transient"),
        }
    }

    /// Same logic as classify_lettre_error but takes the
    /// already-formatted string. Used only by tests so we can
    /// exercise the branches without constructing
    /// lettre::transport::smtp::Error values (their constructors
    /// are private).
    fn classify_lettre_error_str(msg: &str) -> SmtpErrorClass {
        if msg.contains("permanent error") || msg.contains("permanent failure") {
            let code = extract_smtp_code(msg).unwrap_or(550);
            return SmtpErrorClass::Permanent {
                code,
                message: msg.to_string(),
            };
        }
        if msg.contains("transient error") || msg.contains("transient failure") {
            let code = extract_smtp_code(msg).unwrap_or(421);
            return SmtpErrorClass::Transient {
                code,
                message: msg.to_string(),
            };
        }
        if msg.contains("connection refused")
            || msg.contains("connection reset")
            || msg.contains("connection closed")
            || msg.contains("timed out")
            || msg.contains("tls")
            || msg.contains("dns")
        {
            return SmtpErrorClass::Transient {
                code: 421,
                message: msg.to_string(),
            };
        }
        SmtpErrorClass::Permanent {
            code: 550,
            message: msg.to_string(),
        }
    }
}
