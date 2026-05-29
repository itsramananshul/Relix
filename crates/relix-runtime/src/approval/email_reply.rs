//! PART 4 — parsing inbound email-reply webhooks from the
//! three common email-receive providers: Mailgun, SendGrid,
//! and Postmark.
//!
//! The bridge's `/v1/channels/email/reply` route reads the
//! raw request body (form-encoded for Mailgun / SendGrid,
//! JSON for Postmark), classifies the provider, optionally
//! verifies the signature, and lifts:
//!
//! - The reply `subject` line (operator's typed-in `APPROVE`
//!   / `DENY` plus the preserved `[<approval_id>]` bracket
//!   from the original notification).
//! - The `from` address (for operator attribution on the
//!   decision row).
//!
//! Then [`parse_subject_for_decision`] resolves the actual
//! decision (`approved` / `rejected`) + approval id.
//!
//! Provider verification posture:
//!
//! - **Mailgun** ships an HMAC-SHA256 over
//!   `<timestamp><token>` with the operator's Mailgun signing
//!   key. We verify it strictly — operators paste the key into
//!   `RELIX_BRIDGE_MAILGUN_SIGNING_KEY`.
//! - **SendGrid** Inbound Parse does not sign requests
//!   server-side. The standard mitigation is path-secret + TLS
//!   client cert. We accept the parsed body but the bridge
//!   handler logs a warning so the operator's deployment posture
//!   is visible.
//! - **Postmark** ships a `BasicAuth` posture documented in
//!   their server-API console. We accept the parsed body; the
//!   bridge handler relies on the operator's reverse proxy to
//!   enforce credentials.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

/// Provider that produced the inbound webhook. Distinguished
/// from the request body shape rather than from URL path so
/// operators can wire any provider to the same single route.
#[derive(Debug, PartialEq, Eq)]
pub enum EmailProvider {
    /// Mailgun `routes` inbound webhook.
    Mailgun,
    /// SendGrid Inbound Parse webhook.
    SendGrid,
    /// Postmark inbound-stream webhook.
    Postmark,
}

/// Errors during reply parsing.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmailReplyError {
    /// Body did not match any of the three known provider
    /// shapes.
    #[error("email reply: provider not recognised")]
    UnknownProvider,
    /// Form-encoded body did not parse as UTF-8 or had a
    /// truncated percent escape.
    #[error("email reply: percent-decode failed: {0}")]
    BadUrlEncoding(String),
    /// JSON body did not parse.
    #[error("email reply: JSON parse failed: {0}")]
    BadJson(String),
    /// Body parsed but the `subject` field was missing or
    /// empty.
    #[error("email reply: missing subject field")]
    MissingSubject,
    /// Mailgun timestamp / token / signature triple was
    /// incomplete or hex-malformed.
    #[error("email reply: mailgun signature malformed: {0}")]
    MailgunSignatureMalformed(&'static str),
    /// Mailgun signature did not verify.
    #[error("email reply: mailgun signature mismatch")]
    MailgunSignatureMismatch,
}

/// Decision lifted from the reply subject.
#[derive(Debug, PartialEq, Eq)]
pub enum SubjectDecision {
    /// Subject carried `APPROVE` / `APPROVED`.
    Approved,
    /// Subject carried `DENY` / `REJECT` / `REJECTED` /
    /// `DENIED`.
    Rejected,
    /// Subject did not carry a recognised decision token.
    Unknown,
}

impl SubjectDecision {
    /// Wire string the coordinator's `approval.record_decision`
    /// cap expects — `approved` or `rejected`. Returns `None`
    /// when the subject was unrecognised.
    pub fn as_wire(self) -> Option<&'static str> {
        match self {
            SubjectDecision::Approved => Some("approved"),
            SubjectDecision::Rejected => Some("rejected"),
            SubjectDecision::Unknown => None,
        }
    }
}

/// Lifted action from an inbound reply.
#[derive(Debug, PartialEq, Eq)]
pub struct EmailReplyAction {
    /// Approval id parsed from the bracketed suffix of the
    /// subject line.
    pub approval_id: String,
    /// Decision lifted from the subject line.
    pub decision: SubjectDecision,
    /// Sender's `From:` address — used for operator
    /// attribution on the decision row.
    pub from: String,
}

/// Parsed inbound webhook body.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedReply {
    /// Which provider's webhook this came from.
    pub provider: EmailProvider,
    /// Reply subject line as the provider gave it (already
    /// percent-decoded for the form-encoded providers).
    pub subject: String,
    /// Sender's address.
    pub from: String,
}

/// Detect the provider and parse the body. `content_type` is
/// the value of the `Content-Type` request header (lowercased
/// at the call site).
pub fn parse_inbound_webhook(
    content_type: &str,
    body: &[u8],
) -> Result<ParsedReply, EmailReplyError> {
    let ct = content_type.trim().to_ascii_lowercase();
    if ct.starts_with("application/json") {
        return parse_postmark_json(body);
    }
    if ct.starts_with("application/x-www-form-urlencoded") || ct.starts_with("multipart/form-data")
    {
        // Form-encoded — could be Mailgun or SendGrid. The
        // multipart wrappers from SendGrid are stripped to a
        // simple body=field for our purposes.
        return parse_form_webhook(body);
    }
    // Last-resort attempt — try JSON then form. Some providers
    // omit the Content-Type or set it to text/plain.
    if let Ok(p) = parse_postmark_json(body) {
        return Ok(p);
    }
    parse_form_webhook(body)
}

/// Parse a Mailgun- or SendGrid-shaped form body. Mailgun
/// fields are lowercase (`subject`, `sender`, `from`,
/// `signature`, `timestamp`, `token`); SendGrid uses
/// lowercase too (`subject`, `from`, `email`, `text`). We
/// keep both and prefer Mailgun's distinguishing field
/// (`signature`) for classification.
fn parse_form_webhook(body: &[u8]) -> Result<ParsedReply, EmailReplyError> {
    let fields = parse_form(body)?;
    let has_mailgun_sig = fields.iter().any(|(k, _)| k == "signature");
    let has_mailgun_token = fields.iter().any(|(k, _)| k == "token");
    let provider = if has_mailgun_sig && has_mailgun_token {
        EmailProvider::Mailgun
    } else {
        EmailProvider::SendGrid
    };
    // Mailgun uses both `sender` and `from`; SendGrid only
    // ships `from`. Mailgun's `sender` is the SMTP envelope
    // address; we prefer the human-readable `From:` header
    // because that's what shows up in the operator's reply.
    let subject = field(&fields, "subject").ok_or(EmailReplyError::MissingSubject)?;
    let from = field(&fields, "from")
        .or_else(|| field(&fields, "sender"))
        .unwrap_or_default();
    Ok(ParsedReply {
        provider,
        subject,
        from,
    })
}

/// Parse a Postmark JSON body. Postmark uses PascalCase
/// fields (`Subject`, `From`, `TextBody`).
fn parse_postmark_json(body: &[u8]) -> Result<ParsedReply, EmailReplyError> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| EmailReplyError::BadJson(e.to_string()))?;
    let subject = v
        .get("Subject")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or(EmailReplyError::MissingSubject)?
        .to_string();
    let from = v
        .get("From")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(ParsedReply {
        provider: EmailProvider::Postmark,
        subject,
        from,
    })
}

/// Verify a Mailgun webhook signature. Mailgun signs
/// `<timestamp><token>` with HMAC-SHA256 using the operator's
/// signing key (found under "Sending → Webhooks" in the
/// Mailgun console). `timestamp` is unix seconds (string) and
/// `token` is a Mailgun-generated nonce. Constant-time at the
/// underlying primitive level.
pub fn verify_mailgun_signature(signing_key: &str, body: &[u8]) -> Result<(), EmailReplyError> {
    if signing_key.is_empty() {
        return Err(EmailReplyError::MailgunSignatureMalformed(
            "signing_key is empty",
        ));
    }
    let fields = parse_form(body)?;
    let timestamp = field(&fields, "timestamp").ok_or(
        EmailReplyError::MailgunSignatureMalformed("missing timestamp field"),
    )?;
    let token = field(&fields, "token").ok_or(EmailReplyError::MailgunSignatureMalformed(
        "missing token field",
    ))?;
    let signature_hex = field(&fields, "signature").ok_or(
        EmailReplyError::MailgunSignatureMalformed("missing signature field"),
    )?;
    let sig_bytes = hex::decode(&signature_hex)
        .map_err(|_| EmailReplyError::MailgunSignatureMalformed("signature hex decode failed"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .map_err(|_| EmailReplyError::MailgunSignatureMalformed("hmac key init failed"))?;
    mac.update(timestamp.as_bytes());
    mac.update(token.as_bytes());
    mac.verify_slice(&sig_bytes)
        .map_err(|_| EmailReplyError::MailgunSignatureMismatch)
}

/// Resolve the operator's vote from the subject line. Strips
/// leading `Re:` and `Fwd:` prefixes, then requires the
/// decision word to be the **first** token (so a forwarded
/// chain mentioning "PRE-APPROVED-LIST" cannot trigger an
/// approved decision). Extracts the bracketed approval id
/// (or the trailing token when no brackets) from anywhere in
/// the cleaned subject.
pub fn parse_subject_for_decision(subject: &str) -> EmailReplyAction {
    let cleaned = strip_reply_prefixes(subject);
    let approval_id = extract_bracketed_id(&cleaned)
        .or_else(|| extract_trailing_token(&cleaned))
        .unwrap_or_default();
    // First whitespace-separated token, normalised. The
    // operator's reply must lead with `APPROVE` / `DENY` /
    // etc. — anywhere else in the subject is treated as
    // discussion context.
    let first_token = cleaned
        .split_whitespace()
        .next()
        .map(|w| w.trim_matches(|c: char| !c.is_ascii_alphabetic()))
        .map(|w| w.to_ascii_uppercase())
        .unwrap_or_default();
    let decision = match first_token.as_str() {
        "APPROVE" | "APPROVED" => SubjectDecision::Approved,
        "DENY" | "DENIED" | "REJECT" | "REJECTED" => SubjectDecision::Rejected,
        _ => SubjectDecision::Unknown,
    };
    EmailReplyAction {
        approval_id,
        decision,
        from: String::new(),
    }
}

/// Combine the inbound webhook parse + subject decision lift
/// into one call site. This is what the bridge handler uses.
pub fn lift_decision(parsed: &ParsedReply) -> EmailReplyAction {
    let mut action = parse_subject_for_decision(&parsed.subject);
    action.from = parsed.from.clone();
    action
}

// ── small helpers ──────────────────────────────────────────

fn strip_reply_prefixes(s: &str) -> String {
    let mut current = s.trim().to_string();
    loop {
        let lower = current.to_ascii_lowercase();
        let stripped = lower
            .strip_prefix("re:")
            .or_else(|| lower.strip_prefix("fwd:"))
            .or_else(|| lower.strip_prefix("fw:"));
        match stripped {
            Some(rest) => {
                let trimmed = rest.trim_start();
                let offset = current.len() - trimmed.len();
                current = current[offset..].to_string();
            }
            None => return current,
        }
    }
}

fn extract_bracketed_id(s: &str) -> Option<String> {
    let start = s.rfind('[')?;
    let end = s[start..].find(']')?;
    let id = &s[start + 1..start + end];
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

fn extract_trailing_token(s: &str) -> Option<String> {
    let last = s.split_whitespace().next_back()?;
    let trimmed = last.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Tiny `application/x-www-form-urlencoded` parser. Returns
/// the field list in order.
fn parse_form(body: &[u8]) -> Result<Vec<(String, String)>, EmailReplyError> {
    let body_str =
        std::str::from_utf8(body).map_err(|e| EmailReplyError::BadUrlEncoding(e.to_string()))?;
    let mut out = Vec::new();
    for kv in body_str.split('&') {
        if kv.is_empty() {
            continue;
        }
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let k = percent_decode(k).map_err(EmailReplyError::BadUrlEncoding)?;
        let v = percent_decode(v).map_err(EmailReplyError::BadUrlEncoding)?;
        out.push((k, v));
    }
    Ok(out)
}

fn field(fields: &[(String, String)], name: &str) -> Option<String> {
    fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

fn percent_decode(input: &str) -> Result<String, String> {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let h = bytes.next().ok_or("truncated % escape")?;
                let l = bytes.next().ok_or("truncated % escape")?;
                let hi = from_hex(h)?;
                let lo = from_hex(l)?;
                out.push(hi * 16 + lo);
            }
            _ => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

fn from_hex(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("bad hex digit {b:#x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── subject parsing ──────────────────────────────────

    #[test]
    fn approve_subject_with_bracketed_id_decodes_approved() {
        let a = parse_subject_for_decision("APPROVE [abc-123]");
        assert_eq!(a.decision, SubjectDecision::Approved);
        assert_eq!(a.approval_id, "abc-123");
    }

    #[test]
    fn deny_subject_with_bracketed_id_decodes_rejected() {
        let a = parse_subject_for_decision("DENY [abc-123]");
        assert_eq!(a.decision, SubjectDecision::Rejected);
        assert_eq!(a.approval_id, "abc-123");
    }

    #[test]
    fn auto_quoted_reply_subject_lifts_decision_and_id() {
        // Operator typed APPROVE in front of an auto-quoted
        // reply from the original notification.
        let a = parse_subject_for_decision(
            "APPROVE Re: Approval Required: tool.stripe.charge [abc-123]",
        );
        assert_eq!(a.decision, SubjectDecision::Approved);
        assert_eq!(a.approval_id, "abc-123");
    }

    #[test]
    fn case_insensitive_decision_match() {
        let a = parse_subject_for_decision("approve [a1]");
        assert_eq!(a.decision, SubjectDecision::Approved);
        let a = parse_subject_for_decision("Reject [a1]");
        assert_eq!(a.decision, SubjectDecision::Rejected);
    }

    #[test]
    fn rejected_word_recognised() {
        let a = parse_subject_for_decision("REJECTED [a1]");
        assert_eq!(a.decision, SubjectDecision::Rejected);
    }

    #[test]
    fn denied_word_recognised() {
        let a = parse_subject_for_decision("Denied [a1]");
        assert_eq!(a.decision, SubjectDecision::Rejected);
    }

    #[test]
    fn unknown_decision_token_returns_unknown() {
        let a = parse_subject_for_decision("Hmm [a1]");
        assert_eq!(a.decision, SubjectDecision::Unknown);
        assert_eq!(a.approval_id, "a1");
    }

    #[test]
    fn trailing_token_used_when_no_brackets() {
        let a = parse_subject_for_decision("APPROVE abc-123");
        assert_eq!(a.decision, SubjectDecision::Approved);
        assert_eq!(a.approval_id, "abc-123");
    }

    #[test]
    fn word_boundary_match_does_not_pick_up_substrings() {
        // The word "APPROVED" appears inside "PRE-APPROVED-LIST"
        // but we MUST match on word boundaries — the original
        // subject is "Discussion of PRE-APPROVED-LIST".
        let a = parse_subject_for_decision("Discussion of PRE-APPROVED-LIST");
        assert_eq!(a.decision, SubjectDecision::Unknown);
    }

    #[test]
    fn empty_brackets_fall_back_to_trailing_token() {
        let a = parse_subject_for_decision("APPROVE [] a1");
        assert_eq!(a.approval_id, "a1");
    }

    #[test]
    fn strip_reply_prefixes_handles_nested_re_fwd() {
        assert_eq!(strip_reply_prefixes("Re: Fwd: Re: Foo"), "Foo");
        assert_eq!(strip_reply_prefixes("RE:RE:  bar"), "bar");
        assert_eq!(strip_reply_prefixes("plain"), "plain");
    }

    // ── form / json provider parsing ─────────────────────

    fn form_encode(value: &str) -> String {
        let mut out = String::new();
        for b in value.bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }

    #[test]
    fn parse_mailgun_form_body_classifies_as_mailgun() {
        let body = format!(
            "timestamp=1700000000&token=tok123&signature=abc&\
             subject={}&from={}&sender={}&body-plain=Yes",
            form_encode("APPROVE [a1]"),
            form_encode("ops@example.com"),
            form_encode("ops@example.com")
        );
        let p =
            parse_inbound_webhook("application/x-www-form-urlencoded", body.as_bytes()).unwrap();
        assert_eq!(p.provider, EmailProvider::Mailgun);
        assert_eq!(p.subject, "APPROVE [a1]");
        assert_eq!(p.from, "ops@example.com");
    }

    #[test]
    fn parse_sendgrid_form_body_classifies_as_sendgrid() {
        let body = format!(
            "subject={}&from={}&text=Yes&email={}",
            form_encode("DENY [a2]"),
            form_encode("ops@example.com"),
            form_encode("multipart-omitted")
        );
        let p =
            parse_inbound_webhook("application/x-www-form-urlencoded", body.as_bytes()).unwrap();
        assert_eq!(p.provider, EmailProvider::SendGrid);
        assert_eq!(p.subject, "DENY [a2]");
    }

    #[test]
    fn parse_postmark_json_body_classifies_as_postmark() {
        let body = serde_json::json!({
            "Subject": "APPROVE [a3]",
            "From": "ops@example.com",
            "TextBody": "Yes"
        });
        let p = parse_inbound_webhook("application/json", body.to_string().as_bytes()).unwrap();
        assert_eq!(p.provider, EmailProvider::Postmark);
        assert_eq!(p.subject, "APPROVE [a3]");
        assert_eq!(p.from, "ops@example.com");
    }

    #[test]
    fn missing_subject_field_rejected() {
        let body = "from=ops@example.com&text=Yes";
        let err = parse_inbound_webhook("application/x-www-form-urlencoded", body.as_bytes())
            .unwrap_err();
        assert_eq!(err, EmailReplyError::MissingSubject);
    }

    #[test]
    fn malformed_json_rejected() {
        let err = parse_inbound_webhook("application/json", b"{not json").unwrap_err();
        assert!(matches!(err, EmailReplyError::BadJson(_)));
    }

    #[test]
    fn missing_content_type_falls_back_to_form() {
        let body = format!(
            "subject={}&from={}",
            form_encode("APPROVE [a1]"),
            form_encode("ops@example.com")
        );
        let p = parse_inbound_webhook("", body.as_bytes()).unwrap();
        assert_eq!(p.subject, "APPROVE [a1]");
    }

    // ── mailgun signature verification ───────────────────

    fn sign_mailgun(secret: &str, timestamp: &str, token: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.as_bytes());
        mac.update(token.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn valid_mailgun_signature_passes() {
        let key = "sig-key";
        let ts = "1700000000";
        let token = "tok-1";
        let sig = sign_mailgun(key, ts, token);
        let body = format!("timestamp={ts}&token={token}&signature={sig}&subject=APPROVE+%5Ba1%5D");
        assert!(verify_mailgun_signature(key, body.as_bytes()).is_ok());
    }

    #[test]
    fn tampered_mailgun_signature_rejected() {
        let key = "sig-key";
        let ts = "1700000000";
        let token = "tok-1";
        let sig = sign_mailgun(key, ts, token);
        let body = format!("timestamp={ts}&token=other&signature={sig}&subject=APPROVE+%5Ba1%5D");
        let err = verify_mailgun_signature(key, body.as_bytes()).unwrap_err();
        assert_eq!(err, EmailReplyError::MailgunSignatureMismatch);
    }

    #[test]
    fn missing_signature_field_rejected() {
        let body = "timestamp=1700000000&token=tok";
        let err = verify_mailgun_signature("k", body.as_bytes()).unwrap_err();
        assert!(matches!(err, EmailReplyError::MailgunSignatureMalformed(_)));
    }

    #[test]
    fn empty_signing_key_is_malformed() {
        let body = "timestamp=1&token=2&signature=ab";
        let err = verify_mailgun_signature("", body.as_bytes()).unwrap_err();
        assert!(matches!(err, EmailReplyError::MailgunSignatureMalformed(_)));
    }

    // ── lift_decision integration ────────────────────────

    #[test]
    fn lift_decision_propagates_from_address() {
        let parsed = ParsedReply {
            provider: EmailProvider::Postmark,
            subject: "APPROVE [a1]".into(),
            from: "ops@example.com".into(),
        };
        let action = lift_decision(&parsed);
        assert_eq!(action.decision, SubjectDecision::Approved);
        assert_eq!(action.approval_id, "a1");
        assert_eq!(action.from, "ops@example.com");
    }
}
