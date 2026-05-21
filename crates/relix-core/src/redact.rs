//! H8 — secret redaction for chronicle / audit / dashboard surfaces.
//!
//! Hermes runs `redact_sensitive_text()` over every payload it sends to
//! the summarizer LLM, on the theory that "even if the model is told
//! to ignore secrets, it might echo them back." Relix has the same
//! exposure on a different surface: operator notes, error_cause
//! strings, and intervention-audit detail blobs all end up persisted
//! to the chronicle (replayable via the dashboard) and the audit log
//! (operator-readable forever). A pasted API key in an operator's
//! "investigating prod outage" note becomes a forever-leak.
//!
//! This module provides one pure function:
//!
//! ```ignore
//! let safe = relix_core::redact::redact_secrets(input);
//! ```
//!
//! Replaces every matched secret with `[REDACTED:<KIND>]` and
//! returns a fresh String. Idempotent: re-running on
//! already-redacted text is a no-op (the replacement marker is
//! literal and contains no characters the matchers fire on).
//!
//! ## What gets redacted
//!
//! | Pattern | KIND | Source                       |
//! |---|---|---|
//! | `sk-ant-…` | `ANTHROPIC_KEY` | Anthropic API keys (40+ chars after prefix) |
//! | `sk-…`     | `OPENAI_KEY`    | OpenAI / OpenAI-compat (32+ chars after prefix) |
//! | `xoxb-…`   | `SLACK_TOKEN`   | Slack bot tokens |
//! | `ghp_…`    | `GITHUB_PAT`    | GitHub personal access tokens |
//! | `github_pat_…` | `GITHUB_PAT` | GitHub fine-grained PATs |
//! | `AKIA…` (20 char) | `AWS_KEY` | AWS access key id |
//! | `Bearer <token>` | `BEARER_TOKEN` | `Authorization: Bearer ` headers |
//! | `-----BEGIN <X> PRIVATE KEY-----` | `PRIVATE_KEY_BLOCK` | PEM blocks |
//! | `api_key=` / `apikey=` / `password=` / `secret=` / `token=` | `INLINE_SECRET` | `name=value` query-string-style inline secrets (value > 8 chars) |
//!
//! ## What is intentionally NOT redacted
//!
//! - Generic strings that *look* like high-entropy garbage (UUIDs,
//!   correlation IDs, sha hashes). The cost of false positives is
//!   high — operators searching for a specific correlation ID can't
//!   if it was stripped.
//! - Email addresses, IPs, URLs — these are not secrets and operators
//!   need them visible.
//!
//! ## Stability
//!
//! The KIND label set is stable. New patterns may be added; existing
//! KIND labels never change. Downstream parsers that grep
//! `[REDACTED:OPENAI_KEY]` will keep working across runtime versions.

const REDACTED_OPENAI: &str = "[REDACTED:OPENAI_KEY]";
const REDACTED_ANTHROPIC: &str = "[REDACTED:ANTHROPIC_KEY]";
const REDACTED_SLACK: &str = "[REDACTED:SLACK_TOKEN]";
const REDACTED_GH_PAT: &str = "[REDACTED:GITHUB_PAT]";
const REDACTED_AWS_KEY: &str = "[REDACTED:AWS_KEY]";
const REDACTED_BEARER: &str = "[REDACTED:BEARER_TOKEN]";
const REDACTED_PEM: &str = "[REDACTED:PRIVATE_KEY_BLOCK]";
const REDACTED_INLINE: &str = "[REDACTED:INLINE_SECRET]";

/// Redact known-shape secrets in `input`, returning a fresh String.
/// Safe to call on arbitrary user input — never panics.
pub fn redact_secrets(input: &str) -> String {
    // Fast path: empty input. Avoids allocating a fresh String for
    // the (common) case where there's nothing to scan.
    if input.is_empty() {
        return String::new();
    }

    // PEM private-key blocks are multi-line — scan + replace BEFORE
    // the per-token matchers since the body of the block could
    // otherwise match inline-secret rules.
    let mut work = redact_pem_blocks(input);

    // Anthropic before OpenAI: `sk-ant-…` would also match the
    // generic `sk-…` matcher, so the longer prefix wins.
    work = redact_prefixed_token(&work, "sk-ant-", 32, REDACTED_ANTHROPIC);
    work = redact_prefixed_token(&work, "sk-", 24, REDACTED_OPENAI);

    work = redact_prefixed_token(&work, "xoxb-", 16, REDACTED_SLACK);
    work = redact_prefixed_token(&work, "github_pat_", 16, REDACTED_GH_PAT);
    work = redact_prefixed_token(&work, "ghp_", 16, REDACTED_GH_PAT);

    work = redact_aws_key(&work);

    work = redact_bearer(&work);

    work = redact_inline_secret(&work);

    work
}

// ─────────────────────────── per-matcher helpers ───────────────────────────

/// Replace any occurrence of `prefix` followed by `min_body_len` or
/// more characters from the secret-body charset
/// (`[A-Za-z0-9_\-]`) with `replacement`. The match consumes the
/// prefix AND all subsequent body characters greedily so the
/// dashboard never shows `sk-` followed by a partial key.
fn redact_prefixed_token(
    input: &str,
    prefix: &str,
    min_body_len: usize,
    replacement: &str,
) -> String {
    if input.len() < prefix.len() + min_body_len {
        return input.to_string();
    }
    let bytes = input.as_bytes();
    let pre = prefix.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + pre.len() <= bytes.len() && &bytes[i..i + pre.len()] == pre {
            // Measure body length.
            let body_start = i + pre.len();
            let mut j = body_start;
            while j < bytes.len() && is_secret_body_byte(bytes[j]) {
                j += 1;
            }
            let body_len = j - body_start;
            if body_len >= min_body_len {
                out.push_str(replacement);
                i = j;
                continue;
            }
        }
        // Push the next char (preserving UTF-8). Use char_indices via
        // an inline split so we don't drop bytes mid-codepoint.
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// AWS access keys are `AKIA` + 16 base32 chars (uppercase letters
///   and digits). Distinct matcher because the suffix charset is
///   uppercase-only and narrower than the generic body charset.
fn redact_aws_key(input: &str) -> String {
    let bytes = input.as_bytes();
    let pre = b"AKIA";
    if bytes.len() < pre.len() + 16 {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + pre.len() <= bytes.len() && &bytes[i..i + pre.len()] == pre {
            let body_start = i + pre.len();
            let mut j = body_start;
            while j < bytes.len() && is_aws_body_byte(bytes[j]) {
                j += 1;
            }
            let body_len = j - body_start;
            if body_len >= 16 {
                out.push_str(REDACTED_AWS_KEY);
                i = j;
                continue;
            }
        }
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// `Bearer <token>` — anywhere the literal `Bearer ` appears
/// followed by 8+ characters from the body charset. Matches HTTP
/// auth headers, curl traces, and pasted Authorization values.
fn redact_bearer(input: &str) -> String {
    let bytes = input.as_bytes();
    let pre = b"Bearer ";
    if bytes.len() < pre.len() + 8 {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + pre.len() <= bytes.len() && &bytes[i..i + pre.len()] == pre {
            let body_start = i + pre.len();
            let mut j = body_start;
            while j < bytes.len() && is_secret_body_byte(bytes[j]) {
                j += 1;
            }
            let body_len = j - body_start;
            if body_len >= 8 {
                out.push_str("Bearer ");
                out.push_str(REDACTED_BEARER);
                i = j;
                continue;
            }
        }
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// Scan for PEM private-key blocks and replace the whole block
/// (header + body + footer) with the placeholder. Handles common
/// variants: `RSA PRIVATE KEY`, `EC PRIVATE KEY`, plain
/// `PRIVATE KEY`, `OPENSSH PRIVATE KEY`.
fn redact_pem_blocks(input: &str) -> String {
    let header_prefix = "-----BEGIN ";
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let bytes = input.as_bytes();
    while i < bytes.len() {
        if let Some(rel) = find_substr(&input[i..], header_prefix) {
            let header_start = i + rel;
            // Push the prefix up to the header.
            out.push_str(&input[i..header_start]);
            // Find the end of the header line.
            let header_eol = find_byte(&bytes[header_start..], b'\n')
                .map(|off| header_start + off + 1)
                .unwrap_or(bytes.len());
            let header_line = &input[header_start..header_eol];
            if header_line.contains("PRIVATE KEY") {
                // Look for the matching footer.
                let footer_prefix = "-----END ";
                if let Some(foot_rel) = find_substr(&input[header_eol..], footer_prefix) {
                    let footer_start = header_eol + foot_rel;
                    // End of footer line (or end of input).
                    let footer_eol = find_byte(&bytes[footer_start..], b'\n')
                        .map(|off| footer_start + off + 1)
                        .unwrap_or(bytes.len());
                    out.push_str(REDACTED_PEM);
                    i = footer_eol;
                    continue;
                }
            }
            // Not a private-key block header — push it as-is and
            // continue scanning past it.
            out.push_str(header_line);
            i = header_eol;
        } else {
            out.push_str(&input[i..]);
            break;
        }
    }
    out
}

/// `name=value` inline secrets. Looks for `key`, `apikey`,
/// `api_key`, `password`, `secret`, `token` (case-insensitive)
/// followed by `=` or `:` then 8+ body chars. Replaces the value
/// only — the operator can still see WHICH field had a secret.
fn redact_inline_secret(input: &str) -> String {
    const NEEDLES: &[&str] = &[
        "api_key", "apikey", "password", "passwd", "secret", "token", "auth",
    ];
    let lower = input.to_ascii_lowercase();
    let bytes_lower = lower.as_bytes();
    let bytes_orig = input.as_bytes();
    let mut events: Vec<(usize, usize, usize)> = Vec::new(); // (key_start, val_start, val_end)
    for &n in NEEDLES {
        let needle = n.as_bytes();
        let mut from = 0;
        while from + needle.len() <= bytes_lower.len() {
            let Some(rel) = find_substr(&lower[from..], n) else {
                break;
            };
            let key_start = from + rel;
            let after = key_start + needle.len();
            // The previous char (if any) must NOT be a body char — we
            // want word boundaries so `api_keying` doesn't match.
            if key_start > 0 && is_secret_body_byte(bytes_lower[key_start - 1]) {
                from = after;
                continue;
            }
            // The next non-whitespace char must be `=` or `:`.
            let mut k = after;
            while k < bytes_lower.len() && (bytes_lower[k] == b' ' || bytes_lower[k] == b'\t') {
                k += 1;
            }
            if k >= bytes_lower.len() || (bytes_lower[k] != b'=' && bytes_lower[k] != b':') {
                from = after;
                continue;
            }
            k += 1; // skip sep
            // Skip any whitespace between separator and value.
            while k < bytes_lower.len() && (bytes_lower[k] == b' ' || bytes_lower[k] == b'\t') {
                k += 1;
            }
            // Optional quote, then body chars.
            let quote =
                if k < bytes_lower.len() && (bytes_lower[k] == b'"' || bytes_lower[k] == b'\'') {
                    let q = bytes_lower[k];
                    k += 1;
                    Some(q)
                } else {
                    None
                };
            let val_start = k;
            while k < bytes_orig.len() {
                let b = bytes_orig[k];
                if let Some(q) = quote {
                    if b == q {
                        break;
                    }
                } else if !is_secret_body_byte(b) {
                    break;
                }
                k += 1;
            }
            let val_end = k;
            if val_end - val_start >= 8 {
                events.push((key_start, val_start, val_end));
            }
            from = val_end;
        }
    }
    if events.is_empty() {
        return input.to_string();
    }
    // Sort by val_start so we can splice in one forward pass.
    events.sort_by_key(|(_, s, _)| *s);
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    for (_, vs, ve) in events {
        if vs < cursor {
            continue; // overlapped; skip
        }
        out.push_str(&input[cursor..vs]);
        out.push_str(REDACTED_INLINE);
        cursor = ve;
    }
    out.push_str(&input[cursor..]);
    out
}

// ─────────────────────────── byte helpers ───────────────────────────

fn is_secret_body_byte(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.')
}

fn is_aws_body_byte(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'2'..=b'7')
}

fn find_substr(hay: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| &h[i..i + n.len()] == n)
}

fn find_byte(hay: &[u8], byte: u8) -> Option<usize> {
    hay.iter().position(|&b| b == byte)
}

/// Return the byte offset of the next UTF-8 char boundary at or
/// after `from`. Used so the matcher loops never split a multi-byte
/// codepoint when copying through to the output.
fn next_utf8_boundary(bytes: &[u8], from: usize) -> usize {
    let mut j = from + 1;
    while j < bytes.len() && (bytes[j] & 0xC0) == 0x80 {
        j += 1;
    }
    j.min(bytes.len())
}

// ─────────────────────────── tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(redact_secrets(""), "");
    }

    #[test]
    fn no_secrets_passthrough() {
        let s = "investigating prod outage; rollback complete at 14:32 UTC";
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn openai_key_redacted() {
        let s = "use this key: FAKE_TEST_FIXTURE_REDACTED";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(!out.contains("sk-abcdef"));
    }

    #[test]
    fn anthropic_key_wins_over_openai_prefix() {
        // `sk-ant-...` starts with `sk-` but the longer prefix matches
        // first so we get ANTHROPIC_KEY not OPENAI_KEY.
        let s = "FAKE_TEST_FIXTURE_REDACTED";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"), "got: {out}");
        assert!(!out.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn github_pat_redacted() {
        let s =
            "git remote set-url origin https://x:ghp_abcdefghij1234567890@github.com/owner/repo";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
    }

    #[test]
    fn github_finegrained_pat_redacted() {
        let s = "Authorization: token FAKE_TEST_FIXTURE_REDACTED";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
    }

    #[test]
    fn slack_bot_token_redacted() {
        let s = "channel webhook: FAKE_TEST_FIXTURE_REDACTED";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:SLACK_TOKEN]"));
    }

    #[test]
    fn aws_key_redacted_only_when_full() {
        let exact = "FAKE_TEST_FIXTURE_REDACTED"; // 20 chars total
        let short = "AKIA123"; // too short
        assert!(redact_secrets(exact).contains("[REDACTED:AWS_KEY]"));
        assert_eq!(
            redact_secrets(short),
            short,
            "short token must pass through"
        );
    }

    #[test]
    fn bearer_token_redacted_keeping_prefix() {
        let s = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let out = redact_secrets(s);
        assert!(out.contains("Bearer [REDACTED:BEARER_TOKEN]"));
        assert!(!out.contains("eyJhbGc"));
    }

    #[test]
    fn pem_private_key_block_redacted() {
        let s = "context:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEvAIBADANBgkqhkiG9w0BA...\n-----END RSA PRIVATE KEY-----\nrest of message";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:PRIVATE_KEY_BLOCK]"));
        assert!(!out.contains("MIIEvAI"));
        assert!(out.contains("rest of message"));
    }

    #[test]
    fn pem_public_key_block_untouched() {
        // Public key blocks are not secrets.
        let s = "-----BEGIN PUBLIC KEY-----\nMIIBIjAN...\n-----END PUBLIC KEY-----";
        let out = redact_secrets(s);
        assert!(out.contains("MIIBIjAN"));
    }

    #[test]
    fn inline_apikey_value_redacted() {
        let s = "config: api_key=abcdef0123456789xyz";
        let out = redact_secrets(s);
        assert_eq!(out, "config: api_key=[REDACTED:INLINE_SECRET]");
    }

    #[test]
    fn inline_password_value_redacted() {
        let s = r#"password: "hunter2-and-then-some""#;
        let out = redact_secrets(s);
        assert!(out.contains("password: \"[REDACTED:INLINE_SECRET]\""));
    }

    #[test]
    fn inline_short_value_passes_through() {
        let s = "token=abc";
        // value too short — fail-closed direction is "don't redact
        // false positives".
        assert_eq!(redact_secrets(s), s);
    }

    #[test]
    fn inline_word_boundary_protected() {
        // `apifield_key=abc12345` should NOT match because the prefix
        // is part of a longer identifier.
        let s = "myapifield_key=abc12345678";
        let out = redact_secrets(s);
        assert_eq!(out, s);
    }

    #[test]
    fn redaction_is_idempotent() {
        let s = "use this key: FAKE_TEST_FIXTURE_REDACTED";
        let once = redact_secrets(s);
        let twice = redact_secrets(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn multibyte_chars_preserved_around_match() {
        let s = "context résumé FAKE_TEST_FIXTURE_REDACTED done";
        let out = redact_secrets(s);
        assert!(out.contains("résumé"));
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(out.contains("done"));
    }

    #[test]
    fn multiple_secrets_all_redacted() {
        let s = "FAKE_TEST_FIXTURE_REDACTED AND ghp_bbbbbbbbbbbbbbbbbbbb AND FAKE_TEST_FIXTURE_REDACTED";
        let out = redact_secrets(s);
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
        assert!(out.contains("[REDACTED:AWS_KEY]"));
    }
}
