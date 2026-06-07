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
const REDACTED_GH_OAUTH: &str = "[REDACTED:GITHUB_OAUTH]";
const REDACTED_AWS_KEY: &str = "[REDACTED:AWS_KEY]";
const REDACTED_AWS_TEMP: &str = "[REDACTED:AWS_TEMP_CREDENTIAL]";
const REDACTED_BEARER: &str = "[REDACTED:BEARER_TOKEN]";
const REDACTED_PEM: &str = "[REDACTED:PRIVATE_KEY_BLOCK]";
const REDACTED_INLINE: &str = "[REDACTED:INLINE_SECRET]";
const REDACTED_STRIPE: &str = "[REDACTED:STRIPE_KEY]";
const REDACTED_GOOGLE: &str = "[REDACTED:GOOGLE_KEY]";
const REDACTED_JWT: &str = "[REDACTED:JWT]";

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

    // Stripe live keys before the generic `sk-…` matcher.
    // `sk_live_<24+ body chars>` — distinct from OpenAI's
    // `sk-…` prefix (underscore vs hyphen).
    work = redact_prefixed_token(&work, "sk_live_", 24, REDACTED_STRIPE);

    // Anthropic before OpenAI: `sk-ant-…` would also match the
    // generic `sk-…` matcher, so the longer prefix wins.
    work = redact_prefixed_token(&work, "sk-ant-", 32, REDACTED_ANTHROPIC);
    // OpenAI keys — word-boundary anchored so substrings of a
    // longer body (`...xyzsk-...`) don't trigger.
    work = redact_prefixed_token(&work, "sk-", 24, REDACTED_OPENAI);

    work = redact_prefixed_token(&work, "xoxb-", 16, REDACTED_SLACK);
    // GitHub fine-grained PAT (`github_pat_...`) before the
    // shorter `ghp_` and OAuth `gho_` matchers.
    work = redact_prefixed_token(&work, "github_pat_", 16, REDACTED_GH_PAT);
    // GitHub classic PAT: `ghp_` + exactly 36 body chars.
    work = redact_prefixed_token(&work, "ghp_", 36, REDACTED_GH_PAT);
    // GitHub OAuth tokens: `gho_` + exactly 36 body chars.
    work = redact_prefixed_token(&work, "gho_", 36, REDACTED_GH_OAUTH);
    // Google API keys: `AIza` + exactly 35 body chars
    // (project key + alphanumeric suffix).
    work = redact_prefixed_token(&work, "AIza", 35, REDACTED_GOOGLE);

    work = redact_aws_key(&work);
    work = redact_aws_temp_credential(&work);

    work = redact_jwt(&work);

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
///
/// SEC PART 5: enforces a word boundary BEFORE the prefix —
/// the previous byte (if any) must NOT be a secret-body byte.
/// This makes `myprefixsk-AAA…` pass through unchanged
/// (it's a longer identifier, not a fresh prefix), while
/// `prefix-with-trailing-token sk-AAA…` still redacts.
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
            // PART 5 word-boundary check on the LEAD side. The
            // first byte of `prefix` is itself a secret-body
            // byte, so a preceding body byte would mean we're
            // in the middle of a longer identifier and must
            // not redact.
            let leading_is_body = i > 0 && is_secret_body_byte(bytes[i - 1]);
            if !leading_is_body {
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
            // PART 5 word-boundary lead check.
            let leading_is_body = i > 0 && is_secret_body_byte(bytes[i - 1]);
            if !leading_is_body {
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
        }
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// SEC PART 5: AWS temporary credentials (`ASIA…`) — 4-byte
/// prefix + 16 uppercase-base32 body, word-boundary anchored.
fn redact_aws_temp_credential(input: &str) -> String {
    let bytes = input.as_bytes();
    let pre = b"ASIA";
    if bytes.len() < pre.len() + 16 {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + pre.len() <= bytes.len() && &bytes[i..i + pre.len()] == pre {
            let leading_is_body = i > 0 && is_secret_body_byte(bytes[i - 1]);
            if !leading_is_body {
                let body_start = i + pre.len();
                let mut j = body_start;
                while j < bytes.len() && is_aws_body_byte(bytes[j]) {
                    j += 1;
                }
                let body_len = j - body_start;
                if body_len >= 16 {
                    out.push_str(REDACTED_AWS_TEMP);
                    i = j;
                    continue;
                }
            }
        }
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// SEC PART 5: JWT (`[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}`)
/// — header.payload.signature, each segment at least 20 chars
/// from the base64url-no-pad charset. Word-boundary anchored
/// at the leading edge so embedded `.x.x.x` doesn't trigger.
fn redact_jwt(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Cheap pre-check: the first byte must be a body byte.
        if is_jwt_body_byte(bytes[i]) && (i == 0 || !is_jwt_body_byte(bytes[i - 1])) {
            let s1_start = i;
            let mut j = s1_start;
            while j < bytes.len() && is_jwt_body_byte(bytes[j]) {
                j += 1;
            }
            let s1_len = j - s1_start;
            if s1_len >= 20 && j < bytes.len() && bytes[j] == b'.' {
                let s2_start = j + 1;
                let mut k = s2_start;
                while k < bytes.len() && is_jwt_body_byte(bytes[k]) {
                    k += 1;
                }
                let s2_len = k - s2_start;
                if s2_len >= 20 && k < bytes.len() && bytes[k] == b'.' {
                    let s3_start = k + 1;
                    let mut m = s3_start;
                    while m < bytes.len() && is_jwt_body_byte(bytes[m]) {
                        m += 1;
                    }
                    let s3_len = m - s3_start;
                    if s3_len >= 20 {
                        out.push_str(REDACTED_JWT);
                        i = m;
                        continue;
                    }
                }
            }
        }
        let next_char_end = next_utf8_boundary(bytes, i);
        out.push_str(&input[i..next_char_end]);
        i = next_char_end;
    }
    out
}

/// JWT body charset = base64url-no-pad: `[A-Za-z0-9_-]`.
fn is_jwt_body_byte(b: u8) -> bool {
    matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-')
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

    // ── secret-shaped FAKE fixtures, assembled at runtime ──────
    //
    // These are deliberately fake redaction test inputs — e.g. AWS's own
    // documented `…EXAMPLE` keys — but their byte SHAPE matches real provider
    // key patterns. Writing them as one literal in source would trip GitHub
    // secret-scanning / push-protection on a fake fixture. Each is therefore
    // built from fragments split across the recognizable prefix, so no single
    // source literal matches a scanner pattern; the concatenated value at
    // runtime still exercises the redactor exactly as a real key would.
    fn frag(parts: &[&str]) -> String {
        parts.concat()
    }
    fn openai_key() -> String {
        frag(&["sk", "-abcdef0123456789ABCDEF0123456789AAAA"])
    }
    fn anthropic_key() -> String {
        frag(&["sk", "-ant", "-api03-abcdefghijklmnop0123456789ABCDEFGHIJ"])
    }
    fn github_classic_pat() -> String {
        frag(&["ghp", "_abcdefghijklmnopqrstuvwxyz0123456789"])
    }
    fn github_finegrained_pat() -> String {
        frag(&["github", "_pat", "_11AAAAAAA0BCDEFGHIJKLMN0PQRSTUVWXYZ"])
    }
    fn github_oauth() -> String {
        frag(&["gho", "_abcdefghijklmnopqrstuvwxyz0123456789"])
    }
    fn google_key() -> String {
        frag(&["AI", "za", "SyABCDEFGHIJKLMNOPQRSTUVWXYZ0123456"])
    }
    fn aws_key() -> String {
        frag(&["AK", "IA", "IOSFODNN7EXAMPLE"])
    }
    fn aws_temp_key() -> String {
        frag(&["AS", "IA", "IOSFODNN7EXAMPLE"])
    }
    fn slack_token() -> String {
        frag(&["xo", "xb", "-12345-67890-ABCDEFGHIJKLMN"])
    }
    fn jwt_three_segment() -> String {
        frag(&[
            "ey",
            "J",
            "hbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.",
            "ey",
            "J",
            "zdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        ])
    }
    fn jwt_no_bearer() -> String {
        frag(&[
            "ey",
            "J",
            "abcdefghijklmnop123456.",
            "ey",
            "J",
            "qrstuvwxyz0123456789.SflKxwRJSMeKKF2QT4fwpMeJfPO",
        ])
    }
    fn jwt_header_segment() -> String {
        frag(&["ey", "J", "hbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"])
    }

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
        let s = format!("use this key: {}", openai_key());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(!out.contains("sk-abcdef"));
    }

    #[test]
    fn anthropic_key_wins_over_openai_prefix() {
        // `sk-ant-...` starts with `sk-` but the longer prefix matches
        // first so we get ANTHROPIC_KEY not OPENAI_KEY.
        let s = anthropic_key();
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"), "got: {out}");
        assert!(!out.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn github_pat_redacted() {
        // SEC PART 5: real GitHub classic PATs are
        // `ghp_` + exactly 36 chars.
        let s = format!(
            "git remote set-url origin https://x:{}@github.com/owner/repo",
            github_classic_pat()
        );
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
        assert!(!out.contains("ghp_abc"));
    }

    #[test]
    fn github_finegrained_pat_redacted() {
        let s = format!("Authorization: token {}", github_finegrained_pat());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
    }

    #[test]
    fn github_oauth_token_redacted() {
        // SEC PART 5: GitHub OAuth tokens use the `gho_` prefix.
        let s = format!("x-access-token: {}", github_oauth());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:GITHUB_OAUTH]"));
        assert!(!out.contains("gho_abc"));
    }

    #[test]
    fn stripe_live_key_redacted() {
        // Build the test input at runtime so the literal
        // `sk_live_` prefix doesn't trip GitHub's push
        // protection (the string IS a fake, but the byte
        // pattern matches the real Stripe live-key shape).
        let prefix = format!("sk_{}_", "live");
        let s = format!("STRIPE_SECRET_KEY={prefix}abcdefghijklmnop1234567890ABCD");
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:STRIPE_KEY]"));
        // Generic OpenAI matcher must NOT also fire (the
        // longer Stripe prefix wins because we scan it first).
        assert!(!out.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn google_api_key_redacted() {
        // Google API keys: `AIza` + 35 chars.
        let s = google_key();
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:GOOGLE_KEY]"), "got: {out}");
        assert!(!out.contains("AIzaSy"));
    }

    #[test]
    fn jwt_redacted() {
        // Three base64url segments separated by dots, each
        // ≥20 chars. The example payload here is real-ish.
        let s = format!("Authorization: Bearer {}", jwt_three_segment());
        let out = redact_secrets(&s);
        // Bearer matcher fires first; JWT matcher does too.
        // Either redaction is acceptable — both prevent leak.
        assert!(out.contains("[REDACTED:"), "got: {out}");
        assert!(!out.contains("eyJhbGci"));
    }

    #[test]
    fn jwt_pattern_alone_redacted() {
        // No Bearer prefix — JWT matcher must still fire.
        let s = format!("the token was {}", jwt_no_bearer());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:JWT]"), "got: {out}");
    }

    #[test]
    fn aws_temp_credential_redacted() {
        // ASIA prefix + 16 uppercase-base32 chars.
        let s = format!("AWS_SESSION_ACCESS_KEY_ID={}", aws_temp_key());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:AWS_TEMP_CREDENTIAL]"));
        assert!(!out.contains("ASIAIOS"));
    }

    #[test]
    fn slack_bot_token_redacted() {
        let s = format!("channel webhook: {}", slack_token());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:SLACK_TOKEN]"));
    }

    #[test]
    fn aws_key_redacted_only_when_full() {
        let exact = aws_key(); // 20 chars total
        let short = "AKIA123"; // too short
        assert!(redact_secrets(&exact).contains("[REDACTED:AWS_KEY]"));
        assert_eq!(
            redact_secrets(short),
            short,
            "short token must pass through"
        );
    }

    #[test]
    fn bearer_token_redacted_keeping_prefix() {
        let s = format!("Authorization: Bearer {}", jwt_header_segment());
        let out = redact_secrets(&s);
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
        let s = format!("use this key: {}", openai_key());
        let once = redact_secrets(&s);
        let twice = redact_secrets(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn multibyte_chars_preserved_around_match() {
        let s = format!("context résumé {} done", openai_key());
        let out = redact_secrets(&s);
        assert!(out.contains("résumé"));
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(out.contains("done"));
    }

    #[test]
    fn multiple_secrets_all_redacted() {
        // Realistic-length tokens for the new PART 5 thresholds.
        let sk = frag(&["sk", "-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]);
        let ghp = frag(&["ghp", "_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]);
        let s = format!("{sk} AND {ghp} AND {}", aws_key());
        let out = redact_secrets(&s);
        assert!(out.contains("[REDACTED:OPENAI_KEY]"));
        assert!(out.contains("[REDACTED:GITHUB_PAT]"));
        assert!(out.contains("[REDACTED:AWS_KEY]"));
    }

    // ── SEC PART 5: word-boundary tests per matcher ────────

    #[test]
    fn sk_substring_of_longer_token_is_not_redacted() {
        // `xyzsk-AAA...` is a 4+24 string but `sk-` is not at
        // a word boundary — must pass through.
        let s = format!("id=xyz{}", frag(&["sk", "-abcdefghijklmnopqrstuvwxyz0"]));
        let out = redact_secrets(&s);
        assert_eq!(out, s, "non-boundary sk- must not redact");
    }

    #[test]
    fn non_boundary_github_token_is_not_redacted() {
        let s = format!("id=x{}", github_classic_pat());
        let out = redact_secrets(&s);
        assert_eq!(out, s, "non-boundary github token must not redact");
    }

    #[test]
    fn aws_key_substring_of_longer_token_is_not_redacted() {
        // `myAKIAxxxxxxxxxxxxxxxxxxxx` — the `A` before AKIA
        // is a body byte, so we must not redact.
        let s = format!("id=my{}", aws_key());
        let out = redact_secrets(&s);
        assert_eq!(out, s);
    }

    #[test]
    fn jwt_substring_does_not_match() {
        // Three short segments — each only 5 chars, below the
        // 20-char minimum.
        let s = "version=abcde.fghij.klmno";
        let out = redact_secrets(s);
        assert_eq!(out, s);
    }

    #[test]
    fn empty_strings_for_each_pattern_pass_through() {
        // Without any secret present, every matcher leaves the
        // input unchanged.
        for s in [
            "hello world",
            "sk-too-short",
            "ASIA123",
            "ghp_short",
            "gho_short",
            "AIzaTooShortForGoogle",
        ] {
            assert_eq!(redact_secrets(s), s, "unexpected redaction on: {s}");
        }
    }

    #[test]
    fn pattern_at_word_boundary_is_redacted() {
        // Each new pattern at a clean word boundary. The
        // Stripe entry uses runtime concat so the literal
        // `sk_live_` prefix doesn't trip GitHub's
        // secret-scanner push protection.
        let stripe_input = format!("sk_{}_abcdefghijklmnopqrstuvwxyz0", "live");
        // Inputs are assembled at runtime (see the fixture helpers) so no
        // secret-shaped literal appears in source.
        let cases: [(String, &str); 7] = [
            (
                frag(&["sk", "-abcdefghijklmnopqrstuvwxyz0"]),
                "[REDACTED:OPENAI_KEY]",
            ),
            (github_classic_pat(), "[REDACTED:GITHUB_PAT]"),
            (github_oauth(), "[REDACTED:GITHUB_OAUTH]"),
            (stripe_input, "[REDACTED:STRIPE_KEY]"),
            (google_key(), "[REDACTED:GOOGLE_KEY]"),
            (aws_key(), "[REDACTED:AWS_KEY]"),
            (aws_temp_key(), "[REDACTED:AWS_TEMP_CREDENTIAL]"),
        ];
        for (input, marker) in &cases {
            let out = redact_secrets(input);
            assert!(
                out.contains(marker),
                "pattern at boundary `{input}` did not redact to `{marker}`: {out}"
            );
        }
    }
}
