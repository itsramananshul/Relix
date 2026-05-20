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
//! - **DNS rebinding** between the guard and the connect is **closed**.
//!   `ToolBackend::fetch` pins reqwest's resolver to the IPs validated by
//!   [`resolve_safe_url`] via `ClientBuilder::resolve_to_addrs`, so the
//!   TCP connect cannot diverge from the inspected address. The URL
//!   keeps the hostname so `Host` header + TLS SNI keep working. The
//!   pinned `reqwest::Client` is cached in a per-(hostname, validated-addrs)
//!   pool so repeat fetches reuse the same TLS+connection state; the
//!   cache key IS the validated route, so reuse cannot widen the
//!   permitted connect set. See `docs/tool-node-security.md`.
//! - **Per-hop redirect re-validation** is **closed**. The tool's reqwest
//!   client uses a `reqwest::redirect::Policy::custom` closure that runs
//!   [`resolve_safe_url_blocking`] on every redirect target — same-host
//!   or cross-host — before the follow. `Location:` pointing at
//!   loopback / RFC 1918 / metadata / forbidden-resolution hosts is
//!   rejected pre-connect.
//! - **Network egress filtering** at the host OS level is not configured
//!   by the tool node; operators on shared hosts should add an iptables /
//!   Windows-Firewall outbound deny for RFC 1918 to the tool node's UID.

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
///
/// The DNS lookup is delegated to a blocking thread via
/// [`tokio::task::spawn_blocking`] so we don't stall the runtime when the
/// system resolver decides to be slow. The synchronous twin
/// [`resolve_safe_url_blocking`] is used by reqwest's redirect policy
/// closure (which cannot await).
pub async fn resolve_safe_url(raw: &str, allow_http: bool) -> Result<SafeUrl, SsrfError> {
    let (url, lower_host) = match validate_url_pre_dns(raw, allow_http)? {
        ValidatedHost::LiteralIp { url, ip } => {
            return Ok(SafeUrl {
                normalized_url: url,
                resolved: vec![ip],
            });
        }
        ValidatedHost::NeedsDns { url, lower_host } => (url, lower_host),
    };

    let host_for_lookup = lower_host.clone();
    let resolved = tokio::task::spawn_blocking(move || resolve_host_blocking(&host_for_lookup))
        .await
        .map_err(|e| SsrfError::DnsFailed {
            host: lower_host.clone(),
            cause: e.to_string(),
        })??;
    finalise_dns_check(url, lower_host, resolved)
}

/// Synchronous counterpart of [`resolve_safe_url`]. Used by reqwest's
/// `redirect::Policy::custom` closure (which is sync). Blocks the calling
/// thread on the system resolver; acceptable for redirects because they
/// are rare and short-lived. Returns the same [`SafeUrl`] on success.
pub fn resolve_safe_url_blocking(raw: &str, allow_http: bool) -> Result<SafeUrl, SsrfError> {
    let (url, lower_host) = match validate_url_pre_dns(raw, allow_http)? {
        ValidatedHost::LiteralIp { url, ip } => {
            return Ok(SafeUrl {
                normalized_url: url,
                resolved: vec![ip],
            });
        }
        ValidatedHost::NeedsDns { url, lower_host } => (url, lower_host),
    };
    let resolved = resolve_host_blocking(&lower_host)?;
    finalise_dns_check(url, lower_host, resolved)
}

/// Intermediate decision from the cheap, sync, pre-DNS part of the check.
enum ValidatedHost {
    /// URL host is already a literal IP that passed the range check.
    LiteralIp { url: Url, ip: IpAddr },
    /// URL host is a hostname that passed scheme + denylist; DNS still owed.
    NeedsDns { url: Url, lower_host: String },
}

/// Cheap, sync, pre-DNS checks: parse URL, scheme allowlist, literal-IP
/// range check, hostname denylist. No I/O.
fn validate_url_pre_dns(raw: &str, allow_http: bool) -> Result<ValidatedHost, SsrfError> {
    let url = Url::parse(raw.trim()).map_err(|e| SsrfError::BadUrl(e.to_string()))?;

    let scheme = url.scheme().to_ascii_lowercase();
    match scheme.as_str() {
        "https" => {}
        "http" if allow_http => {}
        _ => return Err(SsrfError::SchemeDenied { scheme, allow_http }),
    }

    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::NoHost(raw.to_string()))?
        .to_string();

    if let Some(parsed) = parse_literal_ip(&host) {
        if let Some(reason) = forbidden_ip_reason(parsed) {
            return Err(SsrfError::IpForbidden { ip: parsed, reason });
        }
        return Ok(ValidatedHost::LiteralIp { url, ip: parsed });
    }
    let lower_host = host.to_ascii_lowercase();
    if let Some(reason) = forbidden_hostname_reason(&lower_host) {
        return Err(SsrfError::HostnameDenied {
            host: lower_host,
            reason,
        });
    }
    Ok(ValidatedHost::NeedsDns { url, lower_host })
}

/// Blocking DNS resolver. Used by both the async (via spawn_blocking) and
/// the sync entry points.
fn resolve_host_blocking(lower_host: &str) -> Result<Vec<IpAddr>, SsrfError> {
    (lower_host, 0u16)
        .to_socket_addrs()
        .map(|iter| iter.map(|sa| sa.ip()).collect::<Vec<_>>())
        .map_err(|e| SsrfError::DnsFailed {
            host: lower_host.to_string(),
            cause: e.to_string(),
        })
}

/// Final post-DNS range check shared by both sync and async paths.
fn finalise_dns_check(
    url: Url,
    lower_host: String,
    resolved: Vec<IpAddr>,
) -> Result<SafeUrl, SsrfError> {
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

    // ── Track 6 SSRF hardening: edge cases ────────────────────────────

    /// Either rejection path (literal-IP OR resolved-DNS) is
    /// acceptable for bracketed IPv6 URLs — what matters is that
    /// the URL is refused before any I/O.
    fn assert_v6_rejected(err: &SsrfError) {
        match err {
            SsrfError::IpForbidden { .. } | SsrfError::DnsForbidden { .. } => {}
            other => panic!("bracketed v6 must be rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_bracketed_ipv6_loopback() {
        let e = resolve_safe_url("https://[::1]/", false)
            .await
            .expect_err("bracketed v6 loopback must be rejected");
        assert_v6_rejected(&e);
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_bracketed_ipv6_link_local() {
        let e = resolve_safe_url("https://[fe80::1]/", false)
            .await
            .expect_err("bracketed v6 link-local must be rejected");
        assert_v6_rejected(&e);
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_mapped_v4_loopback_in_url() {
        // `[::ffff:127.0.0.1]` should unwrap to 127.0.0.1 either at
        // parse time or after DNS resolution — both paths must
        // refuse before any I/O.
        let e = resolve_safe_url("https://[::ffff:127.0.0.1]/", false)
            .await
            .expect_err("mapped v4 loopback must be rejected");
        assert_v6_rejected(&e);
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_localhost_case_variants() {
        // Hostname denylist must be case-insensitive. An attacker
        // who knows the denylist will try `LOCALHOST`, `LocalHost`,
        // etc.
        for variant in ["LOCALHOST", "LocalHost", "lOcAlHoSt"] {
            let url = format!("https://{variant}/");
            let e = resolve_safe_url(&url, false)
                .await
                .expect_err(&format!("variant {variant} must be denied"));
            assert!(
                matches!(e, SsrfError::HostnameDenied { .. }),
                "variant {variant} got {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_internal_suffix_case_variants() {
        let e = resolve_safe_url("https://API.INTERNAL/", false)
            .await
            .expect_err("INTERNAL suffix variant must be denied");
        assert!(matches!(e, SsrfError::HostnameDenied { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_with_userinfo_does_not_smuggle() {
        // `https://safe.example@127.0.0.1/` — naive parsing might
        // see `safe.example` as the host. URL spec says userinfo
        // is before the `@`, host is after. The literal-IP check
        // must operate on the actual host (`127.0.0.1`).
        let e = resolve_safe_url("https://user:pass@127.0.0.1/", false)
            .await
            .expect_err("userinfo must not mask the real host");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_with_explicit_port_still_checks_host() {
        let e = resolve_safe_url("https://127.0.0.1:8443/path", false)
            .await
            .expect_err("port must not bypass IP check");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_url_without_host() {
        // `data:` URLs and similar exotic schemes parse but have no
        // host. Should produce a clean SchemeDenied (or NoHost) —
        // never a panic on .host_str().unwrap().
        let e = resolve_safe_url("data:text/plain,hello", false)
            .await
            .expect_err("data: URL must be refused");
        assert!(
            matches!(e, SsrfError::SchemeDenied { .. } | SsrfError::NoHost(_)),
            "got {e:?}"
        );
    }

    #[test]
    fn forbidden_ip_reason_covers_documentation_range() {
        // RFC 5737 documentation ranges should be denied. A handler
        // that reaches "192.0.2.1" means a misconfiguration; better
        // to fail loudly than silently dial nothing.
        assert!(forbidden_ip_reason("192.0.2.1".parse().unwrap()).is_some());
        assert!(forbidden_ip_reason("203.0.113.1".parse().unwrap()).is_some());
    }
}
