//! Minimal RFC 6376 DKIM-Signature builder using RSA-SHA256.
//!
//! Scope, honestly:
//!
//! - Canonicalisation: `relaxed/relaxed` (the most widely-deployed
//!   choice; matches what Gmail / Yahoo / Microsoft accept).
//! - Algorithm: `rsa-sha256` only. RSA keys come from a PEM file
//!   (PKCS#1 `RSA PRIVATE KEY` or PKCS#8 `PRIVATE KEY`).
//! - Headers signed: `from`, `to`, `subject`, `date`, `message-id`,
//!   plus any other header the caller passes in via `headers_to_sign`.
//! - The signer never panics on a malformed key; it returns an
//!   error the caller can log and fall back to unsigned.
//!
//! What this is NOT: an Ed25519 signer, an ARC signer, a DMARC /
//! SPF aligner, or a DNS publisher. Those are out of scope for
//! the channel-node DKIM hook.

use std::path::Path;

use base64::Engine;
use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::Sha256 as RsaSha256;
use rsa::signature::{SignatureEncoding, SignerMut};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum DkimError {
    #[error("read private key {0}: {1}")]
    Read(String, String),
    #[error("parse private key (neither PKCS#1 nor PKCS#8 PEM): {0}")]
    Parse(String),
    #[error("sign: {0}")]
    Sign(String),
}

/// One DKIM signer, bound to a single key + selector + domain.
/// Cheap to clone — the inner key is shared via `Arc`.
#[derive(Clone)]
pub struct DkimSigner {
    inner: std::sync::Arc<DkimSignerInner>,
}

struct DkimSignerInner {
    key: RsaPrivateKey,
    selector: String,
    domain: String,
}

impl std::fmt::Debug for DkimSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DkimSigner")
            .field("selector", &self.inner.selector)
            .field("domain", &self.inner.domain)
            .finish_non_exhaustive()
    }
}

impl DkimSigner {
    /// Load a DKIM signer from a PEM key on disk. Accepts both
    /// PKCS#1 (`-----BEGIN RSA PRIVATE KEY-----`) and PKCS#8
    /// (`-----BEGIN PRIVATE KEY-----`) encodings.
    pub fn from_pem_file(
        path: impl AsRef<Path>,
        selector: impl Into<String>,
        domain: impl Into<String>,
    ) -> Result<Self, DkimError> {
        let bytes = std::fs::read(path.as_ref())
            .map_err(|e| DkimError::Read(path.as_ref().display().to_string(), e.to_string()))?;
        let pem = String::from_utf8_lossy(&bytes);
        Self::from_pem(pem.as_ref(), selector, domain)
    }

    /// Parse a PEM-encoded RSA key. Tries PKCS#1 first, then
    /// PKCS#8 — order matters only for error messages, both
    /// shapes are widely deployed.
    pub fn from_pem(
        pem: &str,
        selector: impl Into<String>,
        domain: impl Into<String>,
    ) -> Result<Self, DkimError> {
        let key = match RsaPrivateKey::from_pkcs1_pem(pem) {
            Ok(k) => k,
            Err(e1) => match RsaPrivateKey::from_pkcs8_pem(pem) {
                Ok(k) => k,
                Err(e2) => {
                    return Err(DkimError::Parse(format!("pkcs1={e1}; pkcs8={e2}")));
                }
            },
        };
        Ok(Self {
            inner: std::sync::Arc::new(DkimSignerInner {
                key,
                selector: selector.into(),
                domain: domain.into(),
            }),
        })
    }

    pub fn selector(&self) -> &str {
        &self.inner.selector
    }
    pub fn domain(&self) -> &str {
        &self.inner.domain
    }

    /// Build a complete `DKIM-Signature: ...` header for the
    /// given message. `headers` is the full ordered list of
    /// header (name, value) pairs in the outgoing message;
    /// `body` is the body bytes after the blank line separator.
    ///
    /// The returned string includes neither the trailing CRLF
    /// nor the leading `DKIM-Signature:` label — callers prepend
    /// the label and embed the result.
    ///
    /// `headers_to_sign` is the list of header names (lowercase)
    /// to include in the `h=` tag. Headers that don't appear in
    /// `headers` are silently skipped per RFC 6376 §3.5.
    pub fn sign(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        headers_to_sign: &[&str],
    ) -> Result<String, DkimError> {
        // 1. body hash (relaxed body canonicalisation).
        let canonical_body = canonicalize_body_relaxed(body);
        let mut hasher = Sha256::new();
        hasher.update(&canonical_body);
        let body_hash = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());

        // 2. Filter + canonicalise headers (relaxed header
        //    canonicalisation). Only headers that exist in the
        //    message get included in the `h=` tag.
        let mut signed_headers: Vec<(String, String)> = Vec::new();
        let mut h_tag = String::new();
        for name in headers_to_sign {
            let name_lc = name.to_ascii_lowercase();
            // Search from the bottom so multiple occurrences
            // (e.g. Received) sign in order. Per RFC 6376 we
            // should sign the FIRST occurrence when there's
            // only one; for simplicity we take all occurrences
            // in bottom-up order.
            for (hname, hval) in headers.iter().rev() {
                if hname.eq_ignore_ascii_case(&name_lc) {
                    signed_headers.push((hname.clone(), hval.clone()));
                    if !h_tag.is_empty() {
                        h_tag.push(':');
                    }
                    h_tag.push_str(&name_lc);
                    break;
                }
            }
        }

        // 3. Build the DKIM-Signature header value WITH the
        //    body hash but a *placeholder* signature (empty
        //    `b=`). Signing covers exactly this canonicalised
        //    form, per RFC 6376 §3.7.
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut dkim_header = format!(
            "v=1; a=rsa-sha256; c=relaxed/relaxed; d={domain}; s={selector}; t={t}; h={h}; bh={bh}; b=",
            domain = self.inner.domain,
            selector = self.inner.selector,
            t = timestamp,
            h = h_tag,
            bh = body_hash,
        );

        // 4. Canonicalise the included headers + the
        //    DKIM-Signature header (with empty `b=`).
        let mut hasher = Sha256::new();
        for (name, value) in &signed_headers {
            let line = canonicalize_header_relaxed(name, value);
            hasher.update(line.as_bytes());
        }
        let dkim_line = canonicalize_header_relaxed("DKIM-Signature", &dkim_header);
        // Per spec the DKIM-Signature line in the hash input has
        // no trailing CRLF.
        let dkim_line = dkim_line.trim_end_matches("\r\n");
        hasher.update(dkim_line.as_bytes());
        let to_sign = hasher.finalize();

        // 5. RSA-SHA256 sign the digest.
        let mut signing_key: SigningKey<RsaSha256> = SigningKey::new(self.inner.key.clone());
        // SigningKey expects to hash the message itself; we hand
        // it the pre-image and let it run SHA-256 internally. To
        // get the right shape we call `sign` on the canonical
        // bytes we just hashed (the digest is recomputed inside,
        // but the input is identical so the signature is the
        // same).
        // Recompute the same input from the canonicalised
        // headers to feed the SigningKey:
        let mut prehash_input: Vec<u8> = Vec::new();
        for (name, value) in &signed_headers {
            prehash_input.extend_from_slice(canonicalize_header_relaxed(name, value).as_bytes());
        }
        prehash_input.extend_from_slice(dkim_line.as_bytes());
        let _ = to_sign; // pre-computed digest retained for tests / debugging
        let sig = signing_key
            .try_sign(&prehash_input)
            .map_err(|e| DkimError::Sign(e.to_string()))?;
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        dkim_header.push_str(&sig_b64);
        Ok(dkim_header)
    }
}

/// RFC 6376 §3.4.4 relaxed-body canonicalisation:
///
/// - normalise line endings to CRLF
/// - reduce runs of WSP within a line to a single SP
/// - strip trailing WSP from each line
/// - drop trailing empty lines (CRLF runs at end)
/// - ensure exactly one trailing CRLF if the body is non-empty
pub fn canonicalize_body_relaxed(body: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(body);
    // Normalise to LF first so we can split / rebuild cleanly.
    let normalised = s.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines: Vec<String> = normalised
        .split('\n')
        .map(|line| {
            // Collapse runs of WSP within the line.
            let mut out = String::with_capacity(line.len());
            let mut last_was_ws = false;
            for c in line.chars() {
                if c == ' ' || c == '\t' {
                    if !last_was_ws {
                        out.push(' ');
                        last_was_ws = true;
                    }
                } else {
                    out.push(c);
                    last_was_ws = false;
                }
            }
            // Strip trailing WSP (now just spaces after the
            // collapse).
            while out.ends_with(' ') {
                out.pop();
            }
            out
        })
        .collect();
    // Strip trailing empty lines.
    while let Some(last) = lines.last() {
        if last.is_empty() {
            lines.pop();
        } else {
            break;
        }
    }
    if lines.is_empty() {
        return Vec::new();
    }
    let mut out = lines.join("\r\n").into_bytes();
    out.extend_from_slice(b"\r\n");
    out
}

/// RFC 6376 §3.4.2 relaxed-header canonicalisation for a single
/// header. Returns the canonical line with trailing CRLF.
///
/// - lowercase header name
/// - replace WSP runs in the value with a single SP
/// - strip leading + trailing WSP from the value
/// - drop the WSP after the `:`
/// - unfold continuation lines into a single line
pub fn canonicalize_header_relaxed(name: &str, value: &str) -> String {
    let name_lc = name.to_ascii_lowercase();
    // Unfold continuation lines: a CRLF followed by WSP becomes a single SP.
    let unfolded = unfold_header_value(value);
    let mut out = String::with_capacity(name_lc.len() + unfolded.len() + 4);
    out.push_str(&name_lc);
    out.push(':');
    let collapsed = collapse_wsp_runs(&unfolded);
    let trimmed = collapsed.trim_matches(|c: char| c == ' ' || c == '\t');
    out.push_str(trimmed);
    out.push_str("\r\n");
    out
}

fn unfold_header_value(value: &str) -> String {
    // Replace `\r\n WSP` (continuation) with a single space;
    // bare `\n WSP` too since some sources don't use CRLF.
    let mut out = String::with_capacity(value.len());
    let chars: Vec<char> = value.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if (c == '\r' || c == '\n')
            && i + 1 < chars.len()
            && (chars[i + 1] == ' ' || chars[i + 1] == '\t')
        {
            out.push(' ');
            i += 2; // skip the newline + the WSP
            continue;
        }
        if c == '\r' && i + 1 < chars.len() && chars[i + 1] == '\n' {
            // Bare CRLF (not a continuation) — keep as-is so
            // downstream collapse step replaces it with one SP.
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '\n' || c == '\r' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn collapse_wsp_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_ws = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            if !last_was_ws {
                out.push(' ');
                last_was_ws = true;
            }
        } else {
            out.push(c);
            last_was_ws = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test DKIM key from RFC 8463 (the rsa-2048 key in §3 is
    /// huge; we'll use a small RSA-1024 generated specifically
    /// for unit tests — generated once, embedded here for
    /// reproducibility). NOT a real production key.
    const TEST_KEY_PEM: &str = include_str!("test-dkim-key.pem");

    #[test]
    fn body_canonicalisation_normalises_line_endings_and_strips_trailing_empty_lines() {
        let body = b"hello world\r\n\r\n\r\n";
        let out = canonicalize_body_relaxed(body);
        assert_eq!(out, b"hello world\r\n");
    }

    #[test]
    fn body_canonicalisation_collapses_internal_wsp_and_strips_trailing_wsp() {
        let body = b"hello    world   \r\nsecond\tline\t\r\n";
        let out = canonicalize_body_relaxed(body);
        assert_eq!(out, b"hello world\r\nsecond line\r\n");
    }

    #[test]
    fn body_canonicalisation_empty_body_returns_empty_bytes() {
        assert!(canonicalize_body_relaxed(b"").is_empty());
        assert!(canonicalize_body_relaxed(b"\r\n\r\n").is_empty());
    }

    #[test]
    fn header_canonicalisation_lowercases_name_and_collapses_value() {
        let out = canonicalize_header_relaxed("Subject", "  Hello    World  ");
        assert_eq!(out, "subject:Hello World\r\n");
    }

    #[test]
    fn header_canonicalisation_unfolds_continuation_lines() {
        let out = canonicalize_header_relaxed("Received", "from x\r\n\tby y");
        assert_eq!(out, "received:from x by y\r\n");
    }

    #[test]
    fn dkim_signer_loads_from_test_key() {
        let signer =
            DkimSigner::from_pem(TEST_KEY_PEM, "relix", "example.com").expect("test key parses");
        assert_eq!(signer.selector(), "relix");
        assert_eq!(signer.domain(), "example.com");
    }

    #[test]
    fn dkim_signer_rejects_garbage() {
        let err = DkimSigner::from_pem("not a pem", "s", "d").unwrap_err();
        match err {
            DkimError::Parse(_) => {}
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[test]
    fn dkim_signer_produces_signature_header() {
        let signer = DkimSigner::from_pem(TEST_KEY_PEM, "relix", "example.com").unwrap();
        let headers = vec![
            ("From".to_string(), "bot@example.com".to_string()),
            ("To".to_string(), "alice@example.com".to_string()),
            ("Subject".to_string(), "test".to_string()),
            (
                "Date".to_string(),
                "Thu, 14 Jan 2027 00:00:00 +0000".to_string(),
            ),
            ("Message-ID".to_string(), "<m1@example.com>".to_string()),
        ];
        let body = b"hello world\r\n";
        let dkim = signer
            .sign(
                &headers,
                body,
                &["from", "to", "subject", "date", "message-id"],
            )
            .expect("sign");
        assert!(dkim.contains("v=1"));
        assert!(dkim.contains("a=rsa-sha256"));
        assert!(dkim.contains("c=relaxed/relaxed"));
        assert!(dkim.contains("d=example.com"));
        assert!(dkim.contains("s=relix"));
        assert!(dkim.contains("h=from:to:subject:date:message-id"));
        assert!(dkim.contains("bh="));
        // Signature is non-empty.
        let b_part = dkim.rsplit_once("b=").unwrap().1;
        assert!(!b_part.is_empty());
    }

    #[test]
    fn dkim_skips_headers_absent_from_message() {
        let signer = DkimSigner::from_pem(TEST_KEY_PEM, "relix", "example.com").unwrap();
        let headers = vec![
            ("From".to_string(), "bot@example.com".to_string()),
            ("Subject".to_string(), "x".to_string()),
        ];
        let dkim = signer
            .sign(&headers, b"hi", &["from", "to", "subject"])
            .unwrap();
        // `to` is absent — h tag must NOT include it.
        let h_tag = dkim
            .split(';')
            .find_map(|p| p.trim().strip_prefix("h="))
            .unwrap();
        assert!(!h_tag.contains("to"));
        assert!(h_tag.contains("from"));
        assert!(h_tag.contains("subject"));
    }
}
