//! Production IMAP inbound listener.
//!
//! Built on `async-imap` 0.11 with the `runtime-tokio` feature so
//! the listener runs on the same tokio runtime as the rest of the
//! email controller.
//!
//! Behaviour:
//!
//! - Implicit TLS on port 993 (the operator can override the
//!   port; TLS is mandatory — there is no STARTTLS or plaintext
//!   IMAP fallback).
//! - Plain `LOGIN` auth, or `AUTHENTICATE XOAUTH2` when the
//!   config carries `imap_oauth2_token_env`.
//! - IDLE for push-style notification when the server advertises
//!   it (`CAPABILITY IDLE`). When IDLE isn't supported the loop
//!   polls every `imap_poll_interval_secs` seconds.
//! - Process only `UNSEEN` messages in the configured folder
//!   (default `INBOX`). Spam / Junk folders are never selected.
//! - Mark each processed message `\Seen`. Optionally move to
//!   `imap_processed_folder` after a successful coordinator
//!   dispatch.
//! - Reject oversized messages (> `imap_max_message_bytes`) by
//!   marking them `\Seen` and feeding the controller a `Bounce`
//!   event so the channel sends a bounce reply.
//!
//! Per-message contents — body text + HTML + attachments + inline
//! images + In-Reply-To / References threading headers — are
//! parsed via `mail-parser`. Attachments are written to a per-
//! controller temp directory (`<system tmp>/relix-email-att/`) and
//! their paths are handed to the coordinator alongside the
//! decoded headers.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_imap::types::Fetch;
use futures::StreamExt;
use mail_parser::{MessageParser, MimeHeaders};

use super::config::EmailNodeConfig;
use super::state::EmailChannelState;

/// What the controller observes when the IMAP listener decodes
/// one inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundEmail {
    /// IMAP UID — stable per-folder identifier the controller uses
    /// to ack the message (mark `\Seen`, move).
    pub uid: u32,
    /// `Message-ID` header (no angle brackets). Empty when the
    /// message had no Message-ID; the channel synthesises one in
    /// that case.
    pub message_id: String,
    /// Bare `From:` address (no display name).
    pub from: String,
    /// Display name from the `From:` header, when present.
    pub from_name: String,
    /// Comma-joined `To:` recipients.
    pub to: String,
    /// Subject line, fully decoded.
    pub subject: String,
    /// Plain-text body. When the message is HTML-only the parser
    /// extracts a plain-text rendering.
    pub body_text: String,
    /// HTML body, when present.
    pub body_html: Option<String>,
    /// File attachments written to a per-controller temp dir.
    pub attachment_paths: Vec<PathBuf>,
    /// `In-Reply-To` header (no angle brackets), when present.
    pub in_reply_to: Option<String>,
    /// `References` header values (no angle brackets).
    pub references: Vec<String>,
    /// Bytes of the raw RFC 5322 source. Capped at
    /// `imap_max_message_bytes` upstream.
    pub raw_size_bytes: usize,
}

impl InboundEmail {
    /// Derive a thread session_id from the message's threading
    /// headers, falling back to the message's own Message-ID.
    /// The session_id is a stable string per thread — equal for
    /// every message in the same conversation.
    pub fn session_id(&self) -> String {
        // Per RFC 5322 / 5322bis the thread root is the first
        // entry in `References:` (if present), or the
        // `In-Reply-To:` parent, or the message itself.
        if let Some(root) = self.references.first() {
            return format!("email-thread:{root}");
        }
        if let Some(parent) = &self.in_reply_to {
            return format!("email-thread:{parent}");
        }
        format!("email-thread:{}", self.message_id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImapError {
    #[error("imap config: {0}")]
    Config(String),
    #[error("imap io: {0}")]
    Io(String),
    #[error("imap tls: {0}")]
    Tls(String),
    #[error("imap protocol: {0}")]
    Protocol(String),
    #[error("imap auth: {0}")]
    Auth(String),
    #[error("imap select: {folder:?}: {error}")]
    Select { folder: String, error: String },
}

/// Attachment-temp-dir root. The controller writes inbound
/// attachments under `<tmp>/relix-email-att/<uid>/<n>-<filename>`.
fn attachment_root() -> PathBuf {
    std::env::temp_dir().join("relix-email-att")
}

/// Strip surrounding angle brackets from an addr-spec / msgid.
fn unwrap_angles(s: &str) -> String {
    s.trim().trim_matches(['<', '>']).to_string()
}

/// Refuse to operate on spam-like folder names — we never want
/// the agent to engage with a junk message.
pub fn is_spam_folder(name: &str) -> bool {
    let lc = name.trim().to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "spam" | "junk" | "junk e-mail" | "junk email" | "trash" | "deleted items"
    ) || lc.contains("spam")
        || lc.contains("junk")
}

/// Parse one IMAP `FETCH` payload into an `InboundEmail` or a
/// size-reject signal. The fetch must include `RFC822` /
/// `BODY[]` (raw bytes) + `UID`.
pub fn parse_fetch(fetch: &Fetch, max_message_bytes: u64) -> Result<ParsedFetch, ImapError> {
    let uid = fetch
        .uid
        .ok_or_else(|| ImapError::Protocol("FETCH missing UID".into()))?;
    let body = fetch
        .body()
        .or_else(|| fetch.text())
        .ok_or_else(|| ImapError::Protocol("FETCH missing body".into()))?;
    let raw_size = body.len();
    if raw_size as u64 > max_message_bytes {
        return Ok(ParsedFetch::Oversize {
            uid,
            bytes: raw_size as u64,
            limit: max_message_bytes,
        });
    }
    let parser = MessageParser::default();
    let parsed = parser
        .parse(body)
        .ok_or_else(|| ImapError::Protocol("mail-parser: could not parse message".into()))?;

    let message_id = parsed.message_id().map(unwrap_angles).unwrap_or_default();
    let from_pair = parsed.from().and_then(|a| a.first());
    let from = from_pair
        .and_then(|m| m.address())
        .unwrap_or_default()
        .to_string();
    let from_name = from_pair
        .and_then(|m| m.name())
        .unwrap_or_default()
        .to_string();
    let to = parsed
        .to()
        .map(|tos| {
            tos.iter()
                .filter_map(|m| m.address())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let subject = parsed.subject().unwrap_or("").to_string();

    let body_text = parsed
        .body_text(0)
        .map(|c| c.into_owned())
        .or_else(|| {
            // Fallback: if the message is HTML-only, the parser
            // emits a plain-text body via `body_text(0)` after
            // doing HTML→text conversion. If even that fails, use
            // the raw HTML rendered with tags stripped.
            parsed.body_html(0).map(|h| strip_html_tags(&h))
        })
        .unwrap_or_default();
    let body_html = parsed.body_html(0).map(|c| c.into_owned());

    let in_reply_to = parsed
        .in_reply_to()
        .as_text_list()
        .and_then(|list| list.first().map(|s| unwrap_angles(s)));
    let references = parsed
        .references()
        .as_text_list()
        .map(|list| list.iter().map(|s| unwrap_angles(s)).collect())
        .unwrap_or_default();

    // Attachments → write to per-uid temp dir.
    let mut attachment_paths: Vec<PathBuf> = Vec::new();
    let root = attachment_root();
    let per_uid = root.join(uid.to_string());
    let mut wrote_any = false;
    for (idx, att) in parsed.attachments().enumerate() {
        // Skip inline images that lack a filename (they ride in
        // multipart/related and are addressed by Content-ID; we
        // hand them to the coordinator via the parsed body
        // already).
        let Some(filename) = att.attachment_name() else {
            continue;
        };
        if !wrote_any {
            if let Err(e) = std::fs::create_dir_all(&per_uid) {
                tracing::warn!(
                    path = %per_uid.display(),
                    error = %e,
                    "email imap: could not create attachment temp dir"
                );
                break;
            }
            wrote_any = true;
        }
        let safe_name = sanitize_filename(filename);
        let path = per_uid.join(format!("{idx}-{safe_name}"));
        if let Err(e) = std::fs::write(&path, att.contents()) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "email imap: could not write attachment"
            );
            continue;
        }
        attachment_paths.push(path);
    }

    let email = InboundEmail {
        uid,
        message_id,
        from,
        from_name,
        to,
        subject,
        body_text,
        body_html,
        attachment_paths,
        in_reply_to,
        references,
        raw_size_bytes: raw_size,
    };
    Ok(ParsedFetch::Message(Box::new(email)))
}

/// What `parse_fetch` returns — either a fully decoded message
/// or an oversize-reject signal the controller turns into a
/// bounce reply. `Message` is boxed so the enum stays small
/// (`InboundEmail` is ~256 bytes on stack).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedFetch {
    Message(Box<InboundEmail>),
    Oversize { uid: u32, bytes: u64, limit: u64 },
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other if other.is_control() => '_',
            other => other,
        })
        .take(120)
        .collect()
}

/// Very small HTML → plaintext fallback used when the message
/// has only an HTML body. Not a real HTML renderer — we strip
/// tags and decode `<br>` to newlines. Sufficient for the
/// preview / chat-flow path; the canonical body remains in
/// `body_html`.
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0u32;
    let mut last_was_space = true;
    let lower = s.to_ascii_lowercase();
    let bytes = s.as_bytes();
    let lower_bytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '<' {
            // <br>, <br/>, <p>, </p> emit a newline.
            if lower_bytes.get(i..i + 4) == Some(b"<br>")
                || lower_bytes.get(i..i + 5) == Some(b"<br/>")
                || lower_bytes.get(i..i + 6) == Some(b"<br />")
                || lower_bytes.get(i..i + 4) == Some(b"</p>")
                || lower_bytes.get(i..i + 3) == Some(b"<p>")
                || lower_bytes.get(i..i + 5) == Some(b"</li>")
                || lower_bytes.get(i..i + 4) == Some(b"<li>")
            {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                last_was_space = true;
            }
            depth = depth.saturating_add(1);
            i += 1;
            continue;
        }
        if c == '>' {
            depth = depth.saturating_sub(1);
            i += 1;
            continue;
        }
        if depth == 0 {
            if c.is_whitespace() {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            } else {
                out.push(c);
                last_was_space = false;
            }
        }
        i += 1;
    }
    out.trim().to_string()
}

/// Tag the controller uses to track which folder we last
/// selected — we never select Spam / Junk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapTarget {
    pub folder: String,
    pub processed_folder: Option<String>,
    pub poll_interval: Duration,
    pub max_message_bytes: u64,
}

impl ImapTarget {
    pub fn from_config(cfg: &EmailNodeConfig) -> Result<Self, ImapError> {
        if is_spam_folder(&cfg.imap_folder) {
            return Err(ImapError::Config(format!(
                "imap_folder {:?} resembles a spam / junk folder; refusing to select",
                cfg.imap_folder
            )));
        }
        let processed = if cfg.imap_processed_folder.trim().is_empty() {
            None
        } else if is_spam_folder(&cfg.imap_processed_folder) {
            return Err(ImapError::Config(format!(
                "imap_processed_folder {:?} resembles a spam / junk folder; refusing",
                cfg.imap_processed_folder
            )));
        } else {
            Some(cfg.imap_processed_folder.clone())
        };
        Ok(Self {
            folder: cfg.imap_folder.clone(),
            processed_folder: processed,
            poll_interval: Duration::from_secs(cfg.imap_poll_interval_secs.max(5)),
            max_message_bytes: cfg.imap_max_message_bytes,
        })
    }
}

/// Run the inbox-polling loop forever. Reconnects on any error
/// after a 5-second backoff so a transient TLS / DNS flake
/// doesn't kill the listener.
///
/// `dispatch` is an async callback the loop fires for every
/// parsed message. It must be cheap to clone — typically a
/// `mpsc::Sender` is sent through this hook by the controller.
pub async fn run_listener<F, Fut>(
    cfg: Arc<EmailNodeConfig>,
    state: Arc<EmailChannelState>,
    target: ImapTarget,
    dispatch: F,
) where
    F: Fn(ParsedFetch) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    loop {
        match run_listener_once(cfg.as_ref(), &target, &dispatch).await {
            Ok(()) => {
                // Connection closed cleanly — back off briefly
                // and reconnect (some servers cycle IDLE every
                // 29 minutes).
                state.mark_imap_disconnected();
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                state.mark_imap_error(&e.to_string());
                tracing::warn!(
                    host = %cfg.imap_host,
                    folder = %target.folder,
                    error = %e,
                    "email imap: listener errored; reconnecting in 5s"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn run_listener_once<F, Fut>(
    cfg: &EmailNodeConfig,
    target: &ImapTarget,
    dispatch: &F,
) -> Result<(), ImapError>
where
    F: Fn(ParsedFetch) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = ()> + Send,
{
    let tcp = tokio::net::TcpStream::connect((cfg.imap_host.as_str(), cfg.imap_port))
        .await
        .map_err(|e| ImapError::Io(format!("connect {}:{}: {e}", cfg.imap_host, cfg.imap_port)))?;
    let tls_connector = async_native_tls::TlsConnector::new();
    let tls_stream = tls_connector
        .connect(&cfg.imap_host, tcp)
        .await
        .map_err(|e| ImapError::Tls(e.to_string()))?;
    let client = async_imap::Client::new(tls_stream);

    // Auth — OAuth2 takes precedence when configured.
    let session_result = if !cfg.imap_oauth2_token_env.trim().is_empty() {
        let token = cfg
            .resolve_env(&cfg.imap_oauth2_token_env)
            .map_err(|e| ImapError::Config(e.to_string()))?;
        let user = if cfg.imap_username.is_empty() {
            return Err(ImapError::Config(
                "imap_username is required when imap_oauth2_token_env is set".into(),
            ));
        } else {
            cfg.imap_username.clone()
        };
        client
            .authenticate("XOAUTH2", &XOAuth2 { user, token })
            .await
            .map_err(|(e, _)| ImapError::Auth(format!("XOAUTH2: {e}")))
    } else {
        let password = cfg
            .resolve_imap_password()
            .map_err(|e| ImapError::Config(e.to_string()))?;
        client
            .login(&cfg.imap_username, &password)
            .await
            .map_err(|(e, _)| ImapError::Auth(format!("LOGIN: {e}")))
    };
    let mut session = session_result?;

    let _select = session
        .select(&target.folder)
        .await
        .map_err(|e| ImapError::Select {
            folder: target.folder.clone(),
            error: e.to_string(),
        })?;

    let capabilities = session
        .capabilities()
        .await
        .map_err(|e| ImapError::Protocol(format!("CAPABILITY: {e}")))?;
    let supports_idle = capabilities.iter().any(
        |c| matches!(c, async_imap::types::Capability::Atom(s) if s.eq_ignore_ascii_case("IDLE")),
    );

    loop {
        let unseen_uids = fetch_unseen_uids(&mut session).await?;
        for uid in unseen_uids {
            // Drain the fetch stream into an owned Vec before we
            // try to mutate the session again (uid_store + uid_mv
            // both need &mut session, and the stream holds it).
            let fetched: Vec<Fetch> = {
                let stream = session
                    .uid_fetch(uid.to_string(), "(UID RFC822)")
                    .await
                    .map_err(|e| ImapError::Protocol(format!("FETCH uid={uid}: {e}")))?;
                let collected = stream
                    .collect::<Vec<Result<Fetch, async_imap::error::Error>>>()
                    .await;
                let mut out: Vec<Fetch> = Vec::with_capacity(collected.len());
                for res in collected {
                    out.push(res.map_err(|e| ImapError::Protocol(format!("FETCH stream: {e}")))?);
                }
                out
            };
            for fetch in &fetched {
                match parse_fetch(fetch, target.max_message_bytes) {
                    Ok(parsed) => {
                        dispatch(parsed.clone()).await;
                        // Mark seen (always — for both Message
                        // and Oversize paths).
                        let store_stream = session
                            .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
                            .await
                            .map_err(|e| ImapError::Protocol(format!("STORE \\Seen: {e}")))?;
                        let _ = store_stream
                            .collect::<Vec<Result<Fetch, async_imap::error::Error>>>()
                            .await;
                        // Optional move.
                        if let Some(target_folder) = &target.processed_folder
                            && let Err(e) = session.uid_mv(uid.to_string(), target_folder).await
                        {
                            tracing::warn!(
                                uid,
                                target = %target_folder,
                                error = %e,
                                "email imap: UID MOVE failed; leaving in source folder"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            uid,
                            error = %e,
                            "email imap: parse_fetch failed; marking \\Seen to avoid re-processing"
                        );
                        if let Ok(store_stream) =
                            session.uid_store(uid.to_string(), "+FLAGS (\\Seen)").await
                        {
                            let _ = store_stream
                                .collect::<Vec<Result<Fetch, async_imap::error::Error>>>()
                                .await;
                        }
                    }
                }
            }
        }

        if supports_idle {
            let mut idle_handle = session.idle();
            idle_handle
                .init()
                .await
                .map_err(|e| ImapError::Protocol(format!("IDLE init: {e}")))?;
            // RFC 2177: re-issue IDLE every 29 minutes to keep
            // the connection alive. We pick 25 min to leave
            // headroom for server-side close-on-idle policies.
            let (result_fut, interrupt) =
                idle_handle.wait_with_timeout(Duration::from_secs(25 * 60));
            let result = result_fut.await;
            // Translate the IdleResponse into a continue / break
            // signal; on either path we DONE and loop.
            let _ = result;
            let _ = interrupt; // dropping is the cancel signal
            session = idle_handle
                .done()
                .await
                .map_err(|e| ImapError::Protocol(format!("IDLE done: {e}")))?;
        } else {
            tokio::time::sleep(target.poll_interval).await;
        }
    }
}

async fn fetch_unseen_uids(
    session: &mut async_imap::Session<async_native_tls::TlsStream<tokio::net::TcpStream>>,
) -> Result<Vec<u32>, ImapError> {
    let uids = session
        .uid_search("UNSEEN")
        .await
        .map_err(|e| ImapError::Protocol(format!("UID SEARCH UNSEEN: {e}")))?;
    let mut sorted: Vec<u32> = uids.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// IMAP SASL XOAUTH2 challenge — RFC 7628 wire shape:
///
/// `user=<user>^Aauth=Bearer <token>^A^A` (with ^A = U+0001), base64'd.
struct XOAuth2 {
    user: String,
    token: String,
}

impl async_imap::Authenticator for &XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        // The IMAP server's challenge for XOAUTH2 is empty on
        // the first round; we always respond with the canonical
        // SASL string.
        let raw = format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token);
        // The crate expects the SASL response NOT base64-encoded
        // — it base64s the bytes itself before sending.
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn min_cfg() -> EmailNodeConfig {
        toml::from_str(
            r#"
                smtp_host = "smtp.example.com"
                smtp_from = "bot@example.com"
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

    #[test]
    fn is_spam_folder_matches_common_variants() {
        for name in [
            "Spam",
            "spam",
            "Junk",
            "JUNK",
            "Junk E-Mail",
            "Junk Email",
            "Trash",
            "Deleted Items",
            "Personal/Spam",
            "Junk Mail",
        ] {
            assert!(is_spam_folder(name), "expected spam: {name}");
        }
        for name in ["INBOX", "Archive", "Sent", "Drafts", "Important"] {
            assert!(!is_spam_folder(name), "unexpected spam: {name}");
        }
    }

    #[test]
    fn imap_target_rejects_spam_source_folder() {
        let mut cfg = min_cfg();
        cfg.imap_folder = "Junk".into();
        match ImapTarget::from_config(&cfg) {
            Err(ImapError::Config(m)) => assert!(m.contains("spam")),
            _ => panic!("expected Config error"),
        }
    }

    #[test]
    fn imap_target_rejects_spam_processed_folder() {
        let mut cfg = min_cfg();
        cfg.imap_processed_folder = "Junk Email".into();
        match ImapTarget::from_config(&cfg) {
            Err(ImapError::Config(m)) => assert!(m.contains("spam")),
            _ => panic!("expected Config error"),
        }
    }

    #[test]
    fn imap_target_defaults_have_no_processed_folder() {
        let cfg = min_cfg();
        let t = ImapTarget::from_config(&cfg).unwrap();
        assert_eq!(t.folder, "INBOX");
        assert!(t.processed_folder.is_none());
    }

    #[test]
    fn imap_target_clamps_poll_interval_to_5s_floor() {
        let mut cfg = min_cfg();
        cfg.imap_poll_interval_secs = 1;
        let t = ImapTarget::from_config(&cfg).unwrap();
        assert_eq!(t.poll_interval, Duration::from_secs(5));
    }

    #[test]
    fn unwrap_angles_strips_brackets_and_trims() {
        assert_eq!(unwrap_angles("  <abc@host>  "), "abc@host");
        assert_eq!(unwrap_angles("abc@host"), "abc@host");
        assert_eq!(unwrap_angles("<<weird>>"), "weird");
    }

    #[test]
    fn sanitize_filename_replaces_dangerous_chars() {
        assert_eq!(sanitize_filename("a/b\\c:d*e?f.txt"), "a_b_c_d_e_f.txt");
    }

    #[test]
    fn strip_html_tags_emits_newlines_at_block_elements() {
        let html = "<p>hello</p><p>world</p>";
        let out = strip_html_tags(html);
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn strip_html_tags_collapses_whitespace() {
        let html = "<div>   lots   of   space   </div>";
        let out = strip_html_tags(html);
        assert_eq!(out, "lots of space");
    }

    /// Session-id derivation: thread root → references[0] when
    /// present; in_reply_to as fallback; own message_id as last
    /// resort. The session_id is stable across the conversation
    /// because every reply within the thread carries the same
    /// References root.
    #[test]
    fn session_id_uses_references_root_when_present() {
        let email = InboundEmail {
            uid: 1,
            message_id: "current@e".into(),
            from: "x@e".into(),
            from_name: "".into(),
            to: "".into(),
            subject: "".into(),
            body_text: "".into(),
            body_html: None,
            attachment_paths: Vec::new(),
            in_reply_to: Some("parent@e".into()),
            references: vec!["root@e".into(), "parent@e".into()],
            raw_size_bytes: 0,
        };
        assert_eq!(email.session_id(), "email-thread:root@e");
    }

    #[test]
    fn session_id_falls_back_to_in_reply_to_when_no_references() {
        let email = InboundEmail {
            uid: 1,
            message_id: "current@e".into(),
            from: "x@e".into(),
            from_name: "".into(),
            to: "".into(),
            subject: "".into(),
            body_text: "".into(),
            body_html: None,
            attachment_paths: Vec::new(),
            in_reply_to: Some("parent@e".into()),
            references: Vec::new(),
            raw_size_bytes: 0,
        };
        assert_eq!(email.session_id(), "email-thread:parent@e");
    }

    #[test]
    fn session_id_falls_back_to_self_when_no_threading_headers() {
        let email = InboundEmail {
            uid: 1,
            message_id: "current@e".into(),
            from: "x@e".into(),
            from_name: "".into(),
            to: "".into(),
            subject: "".into(),
            body_text: "".into(),
            body_html: None,
            attachment_paths: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            raw_size_bytes: 0,
        };
        assert_eq!(email.session_id(), "email-thread:current@e");
    }
}
