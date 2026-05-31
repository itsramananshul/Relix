//! `relix-cli email ...` — operator surface for the email
//! channel, talking to the local bridge over HTTP.
//!
//! Three subcommands, each a thin HTTP forwarder onto the
//! `/v1/email/*` bridge endpoints:
//!
//! - `email send --to <addr> --subject <s> --body <text>` —
//!   send a one-off email.
//! - `email status` — show SMTP + IMAP connection state.
//! - `email test` — self-test: send a probe email to the
//!   configured `smtp_from` address (lifted out of `email
//!   status`).
//!
//! Every subcommand accepts `--bridge <url>` (defaults to
//! `http://127.0.0.1:19791`) and `--raw` to dump the bridge
//! JSON verbatim instead of the formatted view.

use std::time::Duration;

use clap::Subcommand;
use serde::Deserialize;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Send a one-off email via `POST /v1/email/send`.
    Send {
        /// Recipient address. Pass `--to` multiple times for
        /// several recipients.
        #[arg(long)]
        to: Vec<String>,
        /// Subject line.
        #[arg(long)]
        subject: String,
        /// Plain-text body.
        #[arg(long)]
        body: String,
        /// Optional HTML body. When set the email becomes a
        /// `multipart/alternative` with both parts.
        #[arg(long)]
        html: Option<String>,
        /// Optional `Cc` recipients (pass multiple times).
        #[arg(long)]
        cc: Vec<String>,
        /// Optional `Bcc` recipients (pass multiple times).
        #[arg(long)]
        bcc: Vec<String>,
        /// Optional `Reply-To` address.
        #[arg(long)]
        reply_to: Option<String>,
        /// Optional inbound `Message-ID` to thread under.
        #[arg(long)]
        in_reply_to: Option<String>,
        /// Override the default email peer alias.
        #[arg(long)]
        peer: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        /// Print the raw JSON response instead of the formatted
        /// view.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Show SMTP + IMAP connection status.
    Status {
        #[arg(long)]
        peer: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Self-test: send a probe email to the configured
    /// `smtp_from` address. Reads the configured address from
    /// `email status` then dispatches `email send` to it.
    Test {
        #[arg(long)]
        peer: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Send {
            to,
            subject,
            body,
            html,
            cc,
            bcc,
            reply_to,
            in_reply_to,
            peer,
            bridge,
            raw,
        } => {
            let req = SendBody {
                to,
                cc,
                bcc,
                reply_to,
                subject,
                body,
                html,
                in_reply_to,
                peer,
            };
            send(&bridge, req, raw).await
        }
        Cmd::Status { peer, bridge, raw } => status(&bridge, peer.as_deref(), raw).await,
        Cmd::Test { peer, bridge, raw } => test(&bridge, peer.as_deref(), raw).await,
    }
}

#[derive(serde::Serialize)]
struct SendBody {
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    reply_to: Option<String>,
    subject: String,
    body: String,
    html: Option<String>,
    in_reply_to: Option<String>,
    peer: Option<String>,
}

async fn send(bridge: &str, req: SendBody, raw: bool) -> Result<(), Box<dyn std::error::Error>> {
    if req.to.is_empty() {
        return Err("at least one --to address is required".into());
    }
    let url = format!("{}/v1/email/send", bridge.trim_end_matches('/'));
    let (status, body) = http_post_with_status(&url, &req).await?;
    if raw {
        print_raw(&body);
        if !status.is_success() {
            std::process::exit(2);
        }
        return Ok(());
    }
    if status.is_success() {
        let parsed: SendResponse = serde_json::from_str(&body)
            .map_err(|e| format!("decode send response: {e} (body={body})"))?;
        println!("ok\n  message_id : {}", parsed.message_id);
        Ok(())
    } else {
        let err: BridgeErr = serde_json::from_str(&body)
            .map_err(|e| format!("decode send err: {e} (body={body})"))?;
        eprintln!("send failed: {}", err.error);
        std::process::exit(2);
    }
}

async fn status(
    bridge: &str,
    peer: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = match peer {
        Some(p) => format!("{}/v1/email/status?peer={p}", bridge.trim_end_matches('/')),
        None => format!("{}/v1/email/status", bridge.trim_end_matches('/')),
    };
    let body = http_get(&url).await?;
    if raw {
        print_raw(&body);
        return Ok(());
    }
    let parsed: StatusResponse = serde_json::from_str(&body)
        .map_err(|e| format!("decode status response: {e} (body={body})"))?;
    render_status(&parsed);
    Ok(())
}

async fn test(
    bridge: &str,
    peer: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Discover the configured from-address via /status.
    let status_url = match peer {
        Some(p) => format!("{}/v1/email/status?peer={p}", bridge.trim_end_matches('/')),
        None => format!("{}/v1/email/status", bridge.trim_end_matches('/')),
    };
    let body = http_get(&status_url).await?;
    let st: StatusResponse = serde_json::from_str(&body)
        .map_err(|e| format!("decode status response: {e} (body={body})"))?;
    if st.from.trim().is_empty() {
        return Err("email node has no `smtp_from` configured; cannot self-test".into());
    }
    // 2. Strip the display name to get the bare addr-spec.
    let to_addr = extract_bare_address(&st.from);
    println!("dispatching self-test to {to_addr}");
    let req = SendBody {
        to: vec![to_addr.clone()],
        cc: Vec::new(),
        bcc: Vec::new(),
        reply_to: None,
        subject: "Relix email channel self-test".into(),
        body: "If you see this, the Relix email channel can send mail via your configured SMTP transport. This is a self-test triggered by `relix email test`.".into(),
        html: None,
        in_reply_to: None,
        peer: peer.map(|p| p.to_string()),
    };
    let url = format!("{}/v1/email/send", bridge.trim_end_matches('/'));
    let (status, body) = http_post_with_status(&url, &req).await?;
    if raw {
        print_raw(&body);
        if !status.is_success() {
            std::process::exit(2);
        }
        return Ok(());
    }
    if status.is_success() {
        let parsed: SendResponse = serde_json::from_str(&body)
            .map_err(|e| format!("decode send response: {e} (body={body})"))?;
        println!(
            "ok\n  message_id : {}\n  to         : {to_addr}",
            parsed.message_id
        );
        Ok(())
    } else {
        let err: BridgeErr = serde_json::from_str(&body)
            .map_err(|e| format!("decode send err: {e} (body={body})"))?;
        eprintln!("self-test failed: {}", err.error);
        std::process::exit(2);
    }
}

fn extract_bare_address(mailbox: &str) -> String {
    if let Some(open) = mailbox.rfind('<')
        && let Some(close) = mailbox[open + 1..].find('>')
    {
        return mailbox[open + 1..open + 1 + close].trim().to_string();
    }
    mailbox.trim().to_string()
}

fn render_status(s: &StatusResponse) {
    println!("peer            : {}", s.peer);
    println!("smtp            : {}", s.smtp);
    println!("imap            : {}", s.imap);
    println!("from            : {}", s.from);
    println!("smtp_host       : {}", s.smtp_host);
    println!("imap_host       : {}", s.imap_host);
    println!("imap_folder     : {}", s.imap_folder);
    println!("messages_seen   : {}", s.messages_seen);
    println!("messages_sent   : {}", s.messages_sent);
    println!(
        "last_send_at    : {}",
        s.last_send_at
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(never)".into())
    );
    println!(
        "last_poll_at    : {}",
        s.last_poll_at
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(never)".into())
    );
    println!(
        "last_message_at : {}",
        s.last_message_at
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(never)".into())
    );
    if let Some(e) = s.smtp_error.as_deref() {
        println!("smtp_error      : {e}");
    }
    if let Some(e) = s.imap_error.as_deref() {
        println!("imap_error      : {e}");
    }
}

// ── shared types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SendResponse {
    #[serde(default)]
    message_id: String,
}

#[derive(Debug, Deserialize)]
struct BridgeErr {
    #[serde(default)]
    error: String,
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    #[serde(default)]
    peer: String,
    #[serde(default)]
    smtp: String,
    #[serde(default)]
    imap: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    smtp_host: String,
    #[serde(default)]
    imap_host: String,
    #[serde(default)]
    imap_folder: String,
    #[serde(default)]
    messages_seen: u64,
    #[serde(default)]
    messages_sent: u64,
    #[serde(default)]
    last_send_at: Option<i64>,
    #[serde(default)]
    last_poll_at: Option<i64>,
    #[serde(default)]
    last_message_at: Option<i64>,
    #[serde(default)]
    smtp_error: Option<String>,
    #[serde(default)]
    imap_error: Option<String>,
}

// ── http helpers (kept local to mirror the workflow CLI) ──

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest::Client builds")
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let body = http_client().get(url).send().await?.text().await?;
    Ok(body)
}

async fn http_post_with_status<B: serde::Serialize>(
    url: &str,
    body: &B,
) -> Result<(reqwest::StatusCode, String), Box<dyn std::error::Error>> {
    let resp = http_client().post(url).json(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    Ok((status, text))
}

fn print_raw(body: &str) {
    println!("{}", body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bare_address_handles_envelope_form() {
        assert_eq!(
            extract_bare_address("Relix <bot@example.com>"),
            "bot@example.com"
        );
    }

    #[test]
    fn extract_bare_address_handles_bare_form() {
        assert_eq!(extract_bare_address("bot@example.com"), "bot@example.com");
    }

    #[test]
    fn extract_bare_address_trims_whitespace() {
        assert_eq!(
            extract_bare_address("   bot@example.com   "),
            "bot@example.com"
        );
    }

    #[test]
    fn extract_bare_address_returns_input_when_brackets_unmatched() {
        // Unmatched `<` with no `>` → falls back to trimmed input.
        assert_eq!(
            extract_bare_address("garbage <but@unclosed"),
            "garbage <but@unclosed"
        );
    }

    #[test]
    fn status_response_renders_with_optional_errors() {
        let s = StatusResponse {
            peer: "email".into(),
            smtp: "connected".into(),
            imap: "connected".into(),
            from: "bot@e".into(),
            smtp_host: "smtp.e".into(),
            imap_host: "imap.e".into(),
            imap_folder: "INBOX".into(),
            messages_seen: 1,
            messages_sent: 2,
            last_send_at: Some(100),
            last_poll_at: None,
            last_message_at: Some(200),
            smtp_error: None,
            imap_error: Some("idle disconnected".into()),
        };
        // We can't easily capture stdout in a unit test, but
        // exercising render_status() ensures the function
        // doesn't panic on the Some/None combinations.
        render_status(&s);
    }
}
