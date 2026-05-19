//! SSRF guard for `tool.web_fetch`.
//!
//! Two layers of defence, both before any HTTP I/O:
//!
//! 1. **Scheme check** — only `https` (always) and `http` (when `allow_http`
//!    is on) are accepted. `file://`, `ftp://`, `gopher://`, custom schemes,
//!    and missing schemes are all denied.
//! 2. **Host check** — the URL's host is examined twice:
//!    a) If the host parses as a literal IP, it is checked against the
//!    forbidden ranges directly (no DNS).
//!    b) Otherwise the hostname is resolved via the OS resolver and *every*
//!    returned address must be safe.
//!
//! This catches:
//! - direct loopback (`127.0.0.1`, `[::1]`, `localhost`)
//! - RFC 1918 (`10/8`, `172.16/12`, `192.168/16`)
//! - link-local (`169.254/16`, `fe80::/10`) — covers AWS/GCP metadata
//! - shared-address-space (`100.64/10`)
//! - benchmark / documentation (`192.0.2/24`, `198.18/15`, `203.0.113/24`,
//!   `2001:db8::/32`)
//! - multicast / broadcast / unspecified
//! - IPv6 ULA (`fc00::/7`)
//! - IPv6-mapped IPv4 addresses that would otherwise smuggle in a loopback
//!
//! ## Honest limitations
//!
//! - **DNS rebinding** is partially mitigated only. We resolve the hostname
//!   and verify every returned address is safe, but the subsequent reqwest
//!   call re-resolves and could in principle receive a different result.
//!   At Gate 2 we plan to pin the connection to a single inspected
//!   `SocketAddr` via a custom hyper resolver. Documented in
//!   `docs/tool-node-security.md`.
//! - **Network egress filtering** at the host OS level is not configured by
//!   the tool node; operators on shared hosts should add an iptables /
//!   Windows-Firewall outbound deny for RFC1918 to the tool node's UID.
//! - **Redirect targets** are validated by `reqwest`'s redirect policy
//!   (capped at `max_redirects`) but each follow is *not* re-screened by
//!   this module — a later milestone replaces reqwest's default policy with
//!   a `Policy::custom` that re-runs `validate_host`. Tracked as M11.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use reqwest::Url;

/// Outcome of the safety check on a URL.
#[derive(Debug, Clone)]
pub struct SafeUrl {
    /// Normalized URL the caller should fetch (host lowercased, default port
    /// elided). Identical to the input modulo whitespace and case.
    pub normalized_url: Url,
    /// Resolved IPs that all passed the safety check. We keep them around so
    /// later milestones can pin the dial to one of them.
    pub resolved: Vec<IpAddr>,
}

/// What can fail the safety check. All variants are caller-visible (the
/// `cause` field in the `ErrorEnvelope` is built from `Display`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SsrfError {
    /// URL did not parse.
    #[error("invalid url: {0}")]
    BadUrl(String),
    /// URL had no host component (e.g. `file:///etc/passwd`).
    #[error("url has no host: {0}")]
    NoHost(String),
    /// Scheme is not in the allowlist for this node configuration.
    #[error("scheme '{scheme}' not allowed (allow_http={allow_http})")]
    SchemeDenied { scheme: String, allow_http: bool },
    /// A literal IP in the URL was in a forbidden range.
    #[error("ip {ip} is in forbidden range '{reason}'")]
    IpForbidden { ip: IpAddr, reason: &'static str },
    /// Hostname matched a forbidden DNS name (e.g. `localhost`,
    /// `metadata.google.internal`).
    #[error("hostname '{host}' is denied ({reason})")]
    HostnameDenied { host: String, reason: &'static str },
    /// DNS resolution failed.
    #[error("dns resolution for '{host}' failed: {cause}")]
    DnsFailed { host: String, cause: String },
    /// DNS returned zero addresses.
    #[error("dns resolution for '{host}' returned no addresses")]
    DnsEmpty { host: String },
    /// At least one resolved address was forbidden — we refuse the whole URL
    /// rather than picking the "safe" one (DNS rebind defence).
    #[error("dns resolution for '{host}' included forbidden ip {ip} ({reason})")]
    DnsForbidden {
        host: String,
        ip: IpAddr,
        reason: &'static str,
    },
}

/// Validate a URL string and (if it has a hostname) resolve it. Returns
/// either a `SafeUrl` describing what is safe to fetch, or an `SsrfError`
/// that the handler surfaces as `policy_denied`.
pub async fn resolve_safe_url(raw: &str, allow_http: bool) -> Result<SafeUrl, SsrfError> {
    let url = Url::parse(raw.trim()).map_err(|e| SsrfError::BadUrl(e.to_string()))?;

    let scheme = url.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "https" => {}
        "http" if allow_http => {}
        _ => {
            return Err(SsrfError::SchemeDenied { scheme, allow_http });
        }
    }

    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::NoHost(raw.to_string()))?
        .to_string();

    // Layer 1: literal IP or forbidden DNS name? IPv4-mapped-IPv6 is
    // unwrapped so we don't accept `[::ffff:127.0.0.1]` as "external".
    if let Some(parsed) = parse_literal_ip(&host) {
        if let Some(reason) = forbidden_ip_reason(parsed) {
            return Err(SsrfError::IpForbidden { ip: parsed, reason });
        }
        return Ok(SafeUrl {
            normalized_url: url,
            resolved: vec![parsed],
        });
    }
    let lower_host = host.to_ascii_lowercase();
    if let Some(reason) = forbidden_hostname_reason(&lower_host) {
        return Err(SsrfError::HostnameDenied {
            host: lower_host,
            reason,
        });
    }

    // Layer 2: resolve. Use a blocking call inside spawn_blocking — the
    // tokio std-resolver lives behind `tokio::net::lookup_host`, but that
    // requires a port. We synthesize one (port 0 is fine for resolution).
    let host_for_lookup = lower_host.clone();
    let resolved = tokio::task::spawn_blocking(move || {
        (host_for_lookup.as_str(), 0u16)
            .to_socket_addrs()
            .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
    })
    .await
    .map_err(|e| SsrfError::DnsFailed {
        host: lower_host.clone(),
        cause: e.to_string(),
    })?
    .map_err(|e| SsrfError::DnsFailed {
        host: lower_host.clone(),
        cause: e.to_string(),
    })?;

    if resolved.is_empty() {
        return Err(SsrfError::DnsEmpty { host: lower_host });
    }
    for ip in &resolved {
        if let Some(reason) = forbidden_ip_reason(*ip) {
            return Err(SsrfError::DnsForbidden {
                host: lower_host,
                ip: *ip,
                reason,
            });
        }
    }

    Ok(SafeUrl {
        normalized_url: url,
        resolved,
    })
}

/// Try to parse the host string as a literal IP. Accepts both `127.0.0.1`
/// and `[::1]` style (bracketed v6 — `url::Url::host_str()` strips brackets,
/// so we don't need to here; we just parse).
fn parse_literal_ip(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>().ok().map(|ip| match ip {
        // Unwrap IPv4-mapped IPv6 so `::ffff:127.0.0.1` is treated as v4.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    })
}

/// Static list of hostnames we refuse without bothering to resolve. Cheap
/// pre-filter; DNS for any of these would resolve to a forbidden IP anyway,
/// but rejecting early gives clearer error messages.
fn forbidden_hostname_reason(host: &str) -> Option<&'static str> {
    // Exact matches.
    let exact: &[(&str, &str)] = &[
        ("localhost", "loopback hostname"),
        ("ip6-localhost", "loopback hostname"),
        ("ip6-loopback", "loopback hostname"),
        // Cloud metadata endpoints (the IPs are caught by ip rules too, but
        // operators may DNS them via internal resolvers).
        ("metadata.google.internal", "gcp metadata"),
        ("metadata.goog", "gcp metadata"),
        ("metadata", "cloud metadata"),
    ];
    for (n, r) in exact {
        if host == *n {
            return Some(r);
        }
    }
    // Suffix matches (block any subdomain of these).
    let suffix: &[(&str, &str)] = &[
        (".localhost", "loopback hostname"),
        (".local", "mdns/private suffix"),
        (".internal", "private internal suffix"),
        (".intranet", "private internal suffix"),
        (".lan", "private lan suffix"),
        (".corp", "corporate private suffix"),
        (".home", "home private suffix"),
        (".private", "explicitly-private suffix"),
    ];
    for (s, r) in suffix {
        if host.ends_with(s) {
            return Some(r);
        }
    }
    None
}

/// Decide whether an IP is in any of the forbidden ranges. Returns a short
/// reason string when forbidden, `None` when safe to dial.
///
/// The match is intentionally aggressive: documentation / benchmark / shared
/// ranges are also denied because they have no legitimate egress use case.
pub(crate) fn forbidden_ip_reason(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => forbidden_ipv4_reason(v4),
        IpAddr::V6(v6) => forbidden_ipv6_reason(v6),
    }
}

fn forbidden_ipv4_reason(ip: Ipv4Addr) -> Option<&'static str> {
    if ip.is_unspecified() {
        return Some("ipv4 unspecified (0.0.0.0)");
    }
    if ip.is_loopback() {
        return Some("ipv4 loopback (127/8)");
    }
    if ip.is_private() {
        return Some("ipv4 rfc1918 private");
    }
    if ip.is_link_local() {
        return Some("ipv4 link-local (169.254/16)");
    }
    if ip.is_broadcast() {
        return Some("ipv4 broadcast");
    }
    if ip.is_multicast() {
        return Some("ipv4 multicast");
    }
    if ip.is_documentation() {
        return Some("ipv4 documentation");
    }
    let octets = ip.octets();
    // Carrier-grade NAT / shared address space (RFC 6598): 100.64.0.0/10.
    if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
        return Some("ipv4 shared address space (100.64/10)");
    }
    // Benchmark testing: 198.18.0.0/15.
    if octets[0] == 198 && (octets[1] == 18 || octets[1] == 19) {
        return Some("ipv4 benchmark (198.18/15)");
    }
    // Reserved (240/4) — would already fail to route, but be explicit.
    if octets[0] >= 240 {
        return Some("ipv4 reserved (240/4)");
    }
    None
}

fn forbidden_ipv6_reason(ip: Ipv6Addr) -> Option<&'static str> {
    if ip.is_unspecified() {
        return Some("ipv6 unspecified (::)");
    }
    if ip.is_loopback() {
        return Some("ipv6 loopback (::1)");
    }
    if ip.is_multicast() {
        return Some("ipv6 multicast");
    }
    let segments = ip.segments();
    // Link-local: fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return Some("ipv6 link-local (fe80::/10)");
    }
    // Unique local (ULA): fc00::/7
    if (segments[0] & 0xfe00) == 0xfc00 {
        return Some("ipv6 unique local (fc00::/7)");
    }
    // Site-local (deprecated by RFC 3879 but block anyway): fec0::/10
    if (segments[0] & 0xffc0) == 0xfec0 {
        return Some("ipv6 deprecated site-local (fec0::/10)");
    }
    // IPv4-mapped (::ffff:0:0/96) — should have been unwrapped upstream, but
    // belt-and-braces: refuse if the embedded v4 is forbidden.
    if let Some(v4) = ip.to_ipv4_mapped()
        && forbidden_ipv4_reason(v4).is_some()
    {
        return Some("ipv6 maps to forbidden ipv4");
    }
    // IPv4-compatible (::a.b.c.d/96) — historical, but treat the same way.
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0
    {
        let v4 = Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
            (segments[7] >> 8) as u8,
            (segments[7] & 0xff) as u8,
        );
        if !v4.is_unspecified() && forbidden_ipv4_reason(v4).is_some() {
            return Some("ipv6-compat maps to forbidden ipv4");
        }
    }
    // Documentation: 2001:db8::/32
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return Some("ipv6 documentation (2001:db8::/32)");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn ipv4_loopback_and_private_denied() {
        assert!(forbidden_ip_reason("127.0.0.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("10.0.0.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("172.16.0.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("192.168.1.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("169.254.169.254".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("0.0.0.0".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("100.64.0.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("198.18.0.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("224.0.0.1".parse().unwrap()).is_some());
        // public — must be allowed.
        assert!(forbidden_ip_reason("8.8.8.8".parse().unwrap()).is_none());
        assert!(forbidden_ip_reason("1.1.1.1".parse().unwrap()).is_none());
    }

    #[test]
    fn ipv6_loopback_link_local_denied() {
        assert!(forbidden_ip_reason("::1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("fe80::1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("fc00::1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("fec0::1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("2001:db8::1".parse().unwrap()).is_some());
        // Public v6 (Cloudflare's 2606:4700::1111).
        assert!(forbidden_ip_reason("2606:4700:4700::1111".parse().unwrap()).is_none());
    }

    #[test]
    fn ipv6_mapped_ipv4_loopback_denied() {
        // ::ffff:127.0.0.1
        let mapped: Ipv6Addr = "::ffff:7f00:0001".parse().unwrap();
        let reason = forbidden_ip_reason(IpAddr::V6(mapped));
        assert!(reason.is_some(), "mapped v4 loopback must be denied");
    }

    #[test]
    fn hostname_denylist() {
        assert!(forbidden_hostname_reason("localhost").is_some());
        assert!(forbidden_hostname_reason("foo.local").is_some());
        assert!(forbidden_hostname_reason("api.internal").is_some());
        assert!(forbidden_hostname_reason("metadata.google.internal").is_some());
        assert!(forbidden_hostname_reason("example.com").is_none());
        assert!(forbidden_hostname_reason("api.github.com").is_none());
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_loopback_literal() {
        let e = resolve_safe_url("https://127.0.0.1/", false)
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_file_scheme() {
        let e = resolve_safe_url("file:///etc/passwd", false)
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::SchemeDenied { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_http_when_not_opted_in() {
        let e = resolve_safe_url("http://example.com/", false)
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::SchemeDenied { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_invalid_url() {
        let e = resolve_safe_url("not a url", false)
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::BadUrl(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_localhost_hostname() {
        let e = resolve_safe_url("https://localhost/", false)
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::HostnameDenied { .. }), "got {e:?}");
    }

    #[test]
    fn parse_literal_ip_unwraps_mapped() {
        let ip = parse_literal_ip("::ffff:127.0.0.1").expect("parse");
        match ip {
            IpAddr::V4(v) => assert_eq!(v, Ipv4Addr::new(127, 0, 0, 1)),
            other => panic!("expected v4-unwrapped, got {other:?}"),
        }
    }
}
