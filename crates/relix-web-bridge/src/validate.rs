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
    // PHASE 1B: SSRF guard. Reject URLs whose host is an
    // internal target (loopback, link-local incl. the
    // 169.254.169.254 cloud-metadata IP, RFC-1918 private
    // ranges, unspecified/multicast). A hostname is RESOLVED and
    // EVERY resolved IP is re-checked, so a public-looking name
    // pointing at an internal address is blocked too.
    validate_url_ssrf(trimmed)?;
    Ok(())
}

/// PHASE 1B — extract the host portion of an `http(s)://` URL
/// without pulling a full URL parser. Handles optional
/// `user:pass@` userinfo and `[ipv6]` literals. Returns the bare
/// host (no brackets) or `None` when the URL is malformed.
fn host_from_url(url: &str) -> Option<String> {
    let after_scheme = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .filter(|rest| !rest.is_empty())?;
    // Authority ends at the first '/', '?' or '#'.
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo (everything up to and including the last '@').
    let hostport = match authority.rsplit_once('@') {
        Some((_, hp)) => hp,
        None => authority,
    };
    if hostport.is_empty() {
        return None;
    }
    // IPv6 literal: [::1] or [fe80::1]:443.
    if let Some(rest) = hostport.strip_prefix('[') {
        let host = rest.split(']').next()?;
        if host.is_empty() {
            return None;
        }
        return Some(host.to_string());
    }
    // host[:port] — split on the first ':' (IPv4 / hostname).
    let host = hostport.split(':').next().unwrap_or(hostport);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// PHASE 1B — true when `ip` is an internal / non-routable
/// target that an SSRF must not be allowed to reach: loopback,
/// link-local (169.254/16, incl. 169.254.169.254 metadata, and
/// IPv6 fe80::/10), RFC-1918 private ranges (10/8, 172.16/12,
/// 192.168/16), IPv6 unique-local fc00::/7, the unspecified
/// address, and multicast.
pub fn ip_is_blocked(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                // 100.64.0.0/10 CGNAT — also non-public.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            // An IPv4-mapped/compatible address re-checks as IPv4.
            if let Some(v4) = v6.to_ipv4() {
                return ip_is_blocked(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            // fc00::/7 unique-local OR fe80::/10 link-local.
            (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

/// PHASE 1B — SSRF host check. If the host is an IP literal it is
/// classified directly; otherwise it is RESOLVED via DNS and
/// every returned address is checked, so a hostname pointing at
/// an internal IP is blocked too. Resolution failure is treated
/// as a rejection (fail closed) — we will not hand an
/// unresolvable host to the fetch path.
pub fn validate_url_ssrf(url: &str) -> Result<(), String> {
    use std::net::{IpAddr, ToSocketAddrs};

    let host = host_from_url(url).ok_or_else(|| "url: could not parse host".to_string())?;

    // IP literal → classify without DNS.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip_is_blocked(ip) {
            return Err(format!(
                "url: host {ip} is an internal/non-routable address"
            ));
        }
        return Ok(());
    }

    // Hostname → resolve and re-check every resolved address.
    let resolved = (host.as_str(), 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("url: host `{host}` could not be resolved: {e}"))?;
    let mut any = false;
    for addr in resolved {
        any = true;
        if ip_is_blocked(addr.ip()) {
            return Err(format!(
                "url: host `{host}` resolves to internal/non-routable address {}",
                addr.ip()
            ));
        }
    }
    if !any {
        return Err(format!("url: host `{host}` resolved to no addresses"));
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

    // ── PHASE 1B: SSRF guard ──────────────────────────────────

    #[test]
    fn phase1b_validate_url_rejects_loopback_metadata_and_rfc1918() {
        // Loopback.
        assert!(validate_url("http://127.0.0.1:6379/").is_err());
        assert!(validate_url("http://[::1]/").is_err());
        // Cloud-metadata link-local address.
        assert!(validate_url("http://169.254.169.254/latest/meta-data/").is_err());
        // RFC-1918 private ranges.
        assert!(validate_url("http://10.0.0.5/").is_err());
        assert!(validate_url("http://172.16.0.1/").is_err());
        assert!(validate_url("http://192.168.1.1/admin").is_err());
    }

    #[test]
    fn phase1b_validate_url_rejects_hostname_resolving_to_internal_ip() {
        // `localhost` resolves to 127.0.0.1 / ::1 — must be
        // blocked even though it isn't an IP literal, because the
        // guard re-checks the RESOLVED address.
        assert!(
            validate_url("http://localhost/").is_err(),
            "a hostname resolving to loopback must be rejected"
        );
    }

    #[test]
    fn phase1b_validate_url_allows_public_address() {
        // A public IP literal passes the SSRF guard (no DNS, so
        // the test is hermetic). 1.1.1.1 is globally routable.
        assert!(
            validate_url("https://1.1.1.1/").is_ok(),
            "a normal public URL must still be allowed"
        );
    }

    #[test]
    fn phase1b_ip_is_blocked_classifies_ranges() {
        use std::net::IpAddr;
        for s in [
            "127.0.0.1",
            "169.254.169.254",
            "10.255.255.254",
            "172.31.0.1",
            "192.168.0.1",
            "0.0.0.0",
            "::1",
        ] {
            assert!(
                ip_is_blocked(s.parse::<IpAddr>().unwrap()),
                "{s} must be blocked"
            );
        }
        for s in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(
                !ip_is_blocked(s.parse::<IpAddr>().unwrap()),
                "{s} must be allowed"
            );
        }
    }
}
