//! Input validation against the SIMP-018 substitution boundary.
//!
//! SOL string literals are `"..."` with no escape sequences (SIMP-016).
//! Anything that breaks out of the literal or collides with the SIMP-016
//! pipe-delimiter is rejected. Production-typed flow inputs (Gate 2)
//! supersede this.

/// Reject inputs that would corrupt the rendered SOL string literal.
pub fn validate_input(session_id: &str, message: &str) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Err("session_id required".into());
    }
    if message.trim().is_empty() {
        return Err("message required".into());
    }
    for (field_name, field) in [("session_id", session_id), ("message", message)] {
        for ch in field.chars() {
            match ch {
                '"' => {
                    return Err(format!(
                        "{field_name}: '\"' forbidden (SOL has no string escapes)"
                    ));
                }
                '|' => {
                    return Err(format!(
                        "{field_name}: '|' forbidden (collides with wire delimiter)"
                    ));
                }
                '\r' | '\n' => {
                    return Err(format!("{field_name}: newline forbidden"));
                }
                _ => {}
            }
        }
    }
    if session_id.len() > 256 || message.len() > 4096 {
        return Err("input too long".into());
    }
    Ok(())
}

/// Validate a URL string supplied to the `chat_with_tool` flow.
///
/// This is *only* a substitution-boundary check — the real security gate is
/// the tool node's SSRF guard (`relix_runtime::nodes::tool::security`).
/// Rejecting here is purely defensive so the URL string we splice into the
/// rendered SOL literal cannot escape it.
///
/// Rules:
///   * Must be `http://` or `https://` (scheme allowlist re-checked on the
///     tool node, which also enforces `allow_http`).
///   * No `"`, no `|`, no whitespace, no control characters.
///   * Length cap at 2048 bytes.
pub fn validate_url(url: &str) -> Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("url required".into());
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return Err("url must start with http:// or https://".into());
    }
    if trimmed.len() > 2048 {
        return Err("url too long (max 2048 bytes)".into());
    }
    for ch in trimmed.chars() {
        match ch {
            '"' => return Err("url: '\"' forbidden (SOL has no string escapes)".into()),
            '|' => return Err("url: '|' forbidden (collides with wire delimiter)".into()),
            c if c.is_whitespace() => return Err("url: whitespace forbidden".into()),
            c if (c as u32) < 0x20 => return Err("url: control characters forbidden".into()),
            _ => {}
        }
    }
    Ok(())
}

/// Detect the first http(s) URL inside a free-form message. Returns the URL
/// substring if found *and* it passes [`validate_url`]; otherwise None. The
/// OpenAI shim uses this to auto-route to the tool flow when the user pastes
/// a link.
pub fn detect_url_in_message(msg: &str) -> Option<String> {
    for token in msg.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            // Strip common trailing punctuation that users include but that
            // is rarely part of the URL itself.
            let cleaned = token.trim_end_matches(|c: char| {
                matches!(c, '.' | ',' | ';' | ')' | ']' | '!' | '?' | '>')
            });
            if validate_url(cleaned).is_ok() {
                return Some(cleaned.to_string());
            }
        }
    }
    None
}

/// Best-effort sanitiser for inputs arriving through the OpenAI-compatible
/// shim, where multi-line user content is common.
///
/// Rules (intentionally narrow so callers stay aware of the boundary):
///   * `\r\n` and `\n` ⇒ single space.
///   * Tabs ⇒ single space.
///   * `"` and `|` are still rejected — silently rewriting either would
///     surprise the user (their message would no longer say what they typed).
pub fn sanitize_openai_message(s: &str) -> Result<String, String> {
    if s.contains('"') {
        return Err(
            "message contains '\"' (SOL has no string escapes; ask client to remove)".into(),
        );
    }
    if s.contains('|') {
        return Err("message contains '|' (collides with wire delimiter)".into());
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\r' | '\n' | '\t' => out.push(' '),
            other => out.push(other),
        }
    }
    Ok(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_input_rejects_empty() {
        assert!(validate_input("", "x").is_err());
        assert!(validate_input("s", "").is_err());
        assert!(validate_input("   ", "x").is_err());
    }

    #[test]
    fn validate_input_rejects_quotes_pipes_and_newlines() {
        assert!(validate_input(r#"s"x"#, "msg").is_err());
        assert!(validate_input("s|x", "msg").is_err());
        assert!(validate_input("s\nx", "msg").is_err());
        assert!(validate_input("session", r#"msg"with"quote"#).is_err());
        assert!(validate_input("session", "msg|delim").is_err());
        assert!(validate_input("session", "msg\nline").is_err());
    }

    #[test]
    fn validate_input_rejects_too_long() {
        let long = "a".repeat(257);
        assert!(validate_input(&long, "x").is_err());
        let long_msg = "b".repeat(4097);
        assert!(validate_input("s", &long_msg).is_err());
    }

    #[test]
    fn validate_input_accepts_normal_text() {
        assert!(validate_input("demo-session", "hello world").is_ok());
        assert!(validate_input("s_1", "punctuation? yes!").is_ok());
    }

    #[test]
    fn sanitize_openai_message_replaces_newlines_and_tabs() {
        let s = "line one\nline two\r\nline three\tindented";
        let out = sanitize_openai_message(s).expect("ok");
        assert_eq!(out, "line one line two  line three indented");
    }

    #[test]
    fn sanitize_openai_message_rejects_quotes_and_pipes() {
        assert!(sanitize_openai_message(r#"hi "there""#).is_err());
        assert!(sanitize_openai_message("a|b").is_err());
    }

    #[test]
    fn sanitize_openai_message_trims_outer_whitespace() {
        let out = sanitize_openai_message("   hello   ").expect("ok");
        assert_eq!(out, "hello");
    }

    #[test]
    fn validate_url_accepts_https_and_http() {
        assert!(validate_url("https://example.com/").is_ok());
        assert!(validate_url("http://example.com/path?q=1").is_ok());
    }

    #[test]
    fn validate_url_rejects_non_http_schemes() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://example.com/").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("").is_err());
    }

    #[test]
    fn validate_url_rejects_quote_pipe_whitespace_control() {
        assert!(validate_url("https://example.com/\"x\"").is_err());
        assert!(validate_url("https://example.com/a|b").is_err());
        assert!(validate_url("https://example.com/ space").is_err());
        assert!(validate_url("https://example.com/\nfoo").is_err());
    }

    #[test]
    fn detect_url_in_message_finds_first_http_url() {
        let msg = "Please fetch https://example.com/foo and summarize.";
        assert_eq!(
            detect_url_in_message(msg).as_deref(),
            Some("https://example.com/foo")
        );
    }

    #[test]
    fn detect_url_in_message_strips_trailing_punctuation() {
        let msg = "look at https://example.com/blog/post.";
        assert_eq!(
            detect_url_in_message(msg).as_deref(),
            Some("https://example.com/blog/post")
        );
    }

    #[test]
    fn detect_url_in_message_returns_none_when_no_url() {
        assert_eq!(detect_url_in_message("hello world"), None);
    }
}
