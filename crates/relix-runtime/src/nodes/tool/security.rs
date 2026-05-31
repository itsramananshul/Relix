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
//!
//! ## PH-WEB-BLOCKLIST — operator-curated host blocklist
//!
//! Beyond the hard-coded SSRF rejections above, every web entry point
//! accepts an operator-supplied [`HostBlocklist`] of hostnames the node
//! should refuse outright. The check runs:
//!
//! - **Before scheme/DNS** validation, so a blocked host never even
//!   gets to the resolver.
//! - **On every redirect target**, via the same closure that re-runs
//!   the SSRF guard.
//!
//! Matching is case-insensitive **exact** on the hostname. If an
//! operator lists `evil.example.com`, only that hostname is blocked —
//! NOT subdomains or the parent domain. To block a whole subtree the
//! operator lists each hostname explicitly. This matches the
//! per-hostname granularity of feeds like URLhaus and avoids the
//! "block example.com, accidentally lose google.example.com" footgun.
//!
//! ### Refreshing from URLhaus
//!
//! URLhaus publishes a hostnames-only feed at
//! `https://urlhaus.abuse.ch/downloads/hostfile/`. To convert into
//! the `[tool] blocked_hosts = [...]` form:
//!
//! ```text
//! curl -sSL https://urlhaus.abuse.ch/downloads/hostfile/ \
//!   | awk '/^127\.0\.0\.1[ \t]/{print $2}' \
//!   | sed 's/^/  "/;s/$/",/' > blocked_hosts.toml.fragment
//! ```
//!
//! Then paste under `[tool]`. Restart the tool node. This is
//! **operator-curated** by design — the bridge does not call URLhaus
//! live (no implicit external dependency, no leak of fetched URLs to
//! a third party, no surprise rate-limit failures). The honesty
//! posture: "we block what you told us to."

use std::collections::HashSet;
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
    /// PH-WEB-BLOCKLIST: hostname matched the operator's blocklist.
    /// Caller is the same `policy_denied` envelope shape as the
    /// hardcoded SSRF rejections — the dashboard / CLI can't
    /// distinguish, by design (we don't tell potential adversaries
    /// "you hit our blocklist").
    #[error("hostname '{host}' is on the operator blocklist")]
    HostBlocked { host: String },
    /// SEC PART 6: hostname did NOT match any pattern in the
    /// operator-supplied `[tool] url_allowlist`. Distinct from
    /// `HostBlocked` so audit log differentiates the
    /// "explicit deny" vs "not on the allow-list" verdicts.
    #[error("hostname '{host}' is not on the url_allowlist")]
    NotAllowlisted { host: String },
}

/// SEC PART 6: glob-based host allowlist. Patterns are matched
/// against the LOWERCASED host portion of a URL. A `*` in a
/// pattern matches any run of host-legal characters (including
/// the `.` separator) — operators write `*.openai.com` and
/// every subdomain matches. Exact strings without `*` match
/// only the exact host.
#[derive(Debug, Clone, Default)]
pub struct UrlAllowlist {
    patterns: std::sync::Arc<Vec<String>>,
}

impl UrlAllowlist {
    pub fn new<I, S>(patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries: Vec<String> = patterns
            .into_iter()
            .filter_map(|p| {
                let t = p.as_ref().trim().to_ascii_lowercase();
                if t.is_empty() {
                    None
                } else {
                    Some(strip_scheme_and_path(&t))
                }
            })
            .collect();
        Self {
            patterns: std::sync::Arc::new(entries),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.patterns.len()
    }

    /// True when `host` matches any pattern (or the list is
    /// empty — empty = "no allowlist configured" = allow). The
    /// caller decides whether to enforce empty-as-allow or
    /// empty-as-deny.
    pub fn allows(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let host_lc = host.to_ascii_lowercase();
        self.patterns.iter().any(|p| glob_match(p, &host_lc))
    }
}

/// Strip scheme / path components from an operator-supplied
/// allowlist entry. `[tool] url_allowlist = ["https://*.openai.com/*"]`
/// is normalised to `*.openai.com` so the per-request match
/// only needs to look at the host.
fn strip_scheme_and_path(raw: &str) -> String {
    let after_scheme = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let host_only = after_scheme.split('/').next().unwrap_or(after_scheme);
    host_only.to_string()
}

/// Minimal glob: `*` matches zero or more characters (including
/// `.`); everything else is a literal byte-by-byte match. We
/// don't need `?` or character classes for hostname patterns.
fn glob_match(pattern: &str, host: &str) -> bool {
    let pat = pattern.as_bytes();
    let hay = host.as_bytes();
    glob_match_inner(pat, hay)
}

fn glob_match_inner(pat: &[u8], hay: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut hi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_hi = 0usize;
    while hi < hay.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            star_pi = Some(pi);
            star_hi = hi;
            pi += 1;
        } else if pi < pat.len() && pat[pi] == hay[hi] {
            pi += 1;
            hi += 1;
        } else if let Some(spi) = star_pi {
            pi = spi + 1;
            star_hi += 1;
            hi = star_hi;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// SEC PART 6: DNS-resolution cache with TTL. The cache is
/// consulted by every outbound HTTP call from a cloud-tier
/// client so the per-request DNS hit happens at most once
/// per 5 minutes per host (or per the operator-supplied TTL).
///
/// Caching DNS in a security check has a real tradeoff: a
/// DNS-rebind attack within the TTL window goes undetected.
/// We mitigate by: (a) keeping the TTL short (300s default),
/// (b) re-resolving on cache miss synchronously (no stale
/// extension on errors), and (c) tool-capability handlers
/// continue to use the uncached `resolve_safe_url` so the
/// most attacker-reachable surface stays unaffected.
#[derive(Clone)]
pub struct DnsCache {
    inner: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, CachedResolution>>>,
    ttl_secs: u64,
}

#[derive(Clone, Debug)]
struct CachedResolution {
    ips: Vec<IpAddr>,
    expires_at_ms: i64,
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new(300)
    }
}

impl DnsCache {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            ttl_secs,
        }
    }

    /// Resolve `host` to one or more IPs. On cache hit returns
    /// the pinned list; on miss runs a blocking system resolve
    /// and stores the result with TTL.
    pub fn resolve_blocking(&self, host: &str) -> Result<Vec<IpAddr>, SsrfError> {
        let key = host.to_ascii_lowercase();
        let now_ms = unix_ms();
        if let Ok(g) = self.inner.read()
            && let Some(entry) = g.get(&key)
            && entry.expires_at_ms > now_ms
        {
            return Ok(entry.ips.clone());
        }
        let ips = resolve_host_blocking(&key)?;
        if let Ok(mut g) = self.inner.write() {
            g.insert(
                key,
                CachedResolution {
                    ips: ips.clone(),
                    expires_at_ms: now_ms + (self.ttl_secs as i64) * 1_000,
                },
            );
        }
        Ok(ips)
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// SEC PART 6: cloud-tier SSRF check. Resolves the URL's host
/// via the supplied [`DnsCache`] (5-min TTL by default), then
/// rejects with `SsrfError::DnsForbidden` / `IpForbidden`
/// when any resolved IP is in a private / link-local range.
/// Skips the `url_allowlist` (cloud-tier callers — LlamaParse,
/// Tavily, etc. — are exempt by design).
///
/// When `enabled == false` returns Ok immediately — the
/// operator opted out via `[tool] ssrf_protection = false`.
/// SEC PART 6: process-global SSRF state. Controller startup
/// installs the toggle + the DnsCache once; the cloud-tier
/// HTTP client wrappers call [`check_ssrf_cloud_tier_global`]
/// before each request so the per-provider structs don't have
/// to thread DnsCache through their constructors.
static SSRF_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static DNS_CACHE: std::sync::OnceLock<DnsCache> = std::sync::OnceLock::new();
static URL_ALLOWLIST: std::sync::OnceLock<UrlAllowlist> = std::sync::OnceLock::new();

/// Wire the process-global SSRF state. Called once by the
/// tool-node startup with the `[tool] ssrf_protection` flag,
/// the configured `url_allowlist`, and the DNS-cache TTL
/// (5min default per the spec).
pub fn install_ssrf_state(enabled: bool, allowlist: UrlAllowlist, dns_ttl_secs: u64) {
    SSRF_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
    let _ = DNS_CACHE.set(DnsCache::new(dns_ttl_secs));
    let _ = URL_ALLOWLIST.set(allowlist);
    if !enabled {
        tracing::warn!(
            "SSRF protection DISABLED at the tool-node — every outbound HTTP \
             from a tool capability / cloud-tier client will skip the \
             private-IP block. Set `[tool] ssrf_protection = true` for production."
        );
    }
}

/// Process-global cloud-tier SSRF check used by the AI tool
/// HTTP clients (Tavily / Brave / Perplexity / LlamaParse /
/// Jina / Firecrawl). The state is installed once at boot via
/// [`install_ssrf_state`]; first call after a fresh process
/// or a test that didn't install state defaults to
/// `ssrf_protection = true` + a 300s DNS cache.
pub fn check_ssrf_cloud_tier_global(raw_url: &str) -> Result<(), SsrfError> {
    let enabled = SSRF_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    let cache = DNS_CACHE.get_or_init(DnsCache::default);
    check_ssrf_cloud_tier(raw_url, enabled, cache)
}

/// Process-global tool-capability SSRF check used by
/// `tool.web_*` / `tool.browser.*`. Same state-install path as
/// [`check_ssrf_cloud_tier_global`]; additionally enforces the
/// `[tool] url_allowlist`.
pub fn check_ssrf_tool_capability_global(raw_url: &str) -> Result<(), SsrfError> {
    let enabled = SSRF_ENABLED.load(std::sync::atomic::Ordering::Relaxed);
    let cache = DNS_CACHE.get_or_init(DnsCache::default);
    let allowlist = URL_ALLOWLIST.get_or_init(UrlAllowlist::default);
    check_ssrf_tool_capability(raw_url, enabled, cache, allowlist)
}

/// SEC PART 6: enforce the process-global URL allowlist on a
/// raw URL. Returns Ok when the allowlist is empty (not
/// configured) OR the host matches. Tool capability handlers
/// call this at the top of `resolve_safe_url*`; cloud-tier
/// callers explicitly bypass via the dedicated entry points.
fn enforce_global_allowlist(raw_url: &str) -> Result<(), SsrfError> {
    let allowlist = URL_ALLOWLIST.get_or_init(UrlAllowlist::default);
    if allowlist.is_empty() {
        return Ok(());
    }
    let url = Url::parse(raw_url).map_err(|e| SsrfError::BadUrl(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::NoHost(raw_url.to_string()))?
        .to_ascii_lowercase();
    if !allowlist.allows(&host) {
        return Err(SsrfError::NotAllowlisted { host });
    }
    Ok(())
}

pub fn check_ssrf_cloud_tier(
    raw_url: &str,
    enabled: bool,
    dns_cache: &DnsCache,
) -> Result<(), SsrfError> {
    if !enabled {
        return Ok(());
    }
    let url = Url::parse(raw_url).map_err(|e| SsrfError::BadUrl(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| SsrfError::NoHost(raw_url.to_string()))?
        .to_ascii_lowercase();
    if let Some(ip) = parse_literal_ip(&host) {
        if let Some(reason) = forbidden_ip_reason(ip) {
            return Err(SsrfError::IpForbidden { ip, reason });
        }
        return Ok(());
    }
    if let Some(reason) = forbidden_hostname_reason(&host) {
        return Err(SsrfError::HostnameDenied { host, reason });
    }
    let ips = dns_cache.resolve_blocking(&host)?;
    if ips.is_empty() {
        return Err(SsrfError::DnsEmpty { host });
    }
    for ip in &ips {
        if let Some(reason) = forbidden_ip_reason(*ip) {
            return Err(SsrfError::DnsForbidden {
                host,
                ip: *ip,
                reason,
            });
        }
    }
    Ok(())
}

/// SEC PART 6: tool-capability SSRF + allowlist check. Used by
/// `tool.web_read` / `tool.web_get` / `tool.web_fetch` /
/// `tool.browser.*` BEFORE every outbound HTTP. Enforces the
/// allowlist (cloud-tier skip) on top of the SSRF private-IP
/// block.
pub fn check_ssrf_tool_capability(
    raw_url: &str,
    enabled: bool,
    dns_cache: &DnsCache,
    allowlist: &UrlAllowlist,
) -> Result<(), SsrfError> {
    if !enabled {
        // SSRF check disabled — allowlist still applies when
        // configured because the operator may want host
        // restriction even without the private-IP block.
    } else {
        check_ssrf_cloud_tier(raw_url, true, dns_cache)?;
    }
    if !allowlist.is_empty() {
        let url = Url::parse(raw_url).map_err(|e| SsrfError::BadUrl(e.to_string()))?;
        let host = url
            .host_str()
            .ok_or_else(|| SsrfError::NoHost(raw_url.to_string()))?
            .to_ascii_lowercase();
        if !allowlist.allows(&host) {
            return Err(SsrfError::NotAllowlisted { host });
        }
    }
    Ok(())
}

/// PH-WEB-BLOCKLIST: operator-curated host blocklist. Cheap-clone
/// `Arc`-backed `HashSet` so the redirect closure can hold its own
/// view without paying per-redirect copy cost. All entries
/// lowercased on construction; lookup is case-insensitive against
/// the URL's normalized host.
#[derive(Debug, Clone, Default)]
pub struct HostBlocklist {
    entries: std::sync::Arc<HashSet<String>>,
}

impl HostBlocklist {
    /// Build a blocklist from an operator-supplied slice. Empty
    /// entries are dropped; remaining entries are trimmed and
    /// lowercased. Duplicates collapse into one HashSet entry.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let entries = hosts
            .into_iter()
            .filter_map(|h| {
                let t = h.as_ref().trim().to_ascii_lowercase();
                if t.is_empty() { None } else { Some(t) }
            })
            .collect::<HashSet<String>>();
        Self {
            entries: std::sync::Arc::new(entries),
        }
    }

    /// True iff `host` is on the blocklist. Caller is expected to
    /// pass a hostname already normalized to lowercase (callers
    /// inside this module do this via `validate_url_pre_dns`).
    pub fn contains(&self, host: &str) -> bool {
        self.entries.contains(host)
    }

    /// Number of entries — useful for dashboards / log lines.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Snapshot every entry, sorted lexicographically. Used by the
    /// `tool.web.blocklist_summary` capability (PH-DASH-BLOCKLIST)
    /// — operators get a stable order regardless of HashSet's
    /// non-deterministic iteration order.
    pub fn snapshot_sorted(&self) -> Vec<String> {
        let mut out: Vec<String> = self.entries.iter().cloned().collect();
        out.sort();
        out
    }
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
pub async fn resolve_safe_url(
    raw: &str,
    allow_http: bool,
    blocklist: &HostBlocklist,
) -> Result<SafeUrl, SsrfError> {
    // SEC PART 6: enforce the process-global tool-capability
    // URL allowlist (when configured). Cloud-tier callers
    // skip this branch by calling
    // `check_ssrf_cloud_tier_global` directly.
    enforce_global_allowlist(raw)?;
    let (url, lower_host) = match validate_url_pre_dns(raw, allow_http, blocklist)? {
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
pub fn resolve_safe_url_blocking(
    raw: &str,
    allow_http: bool,
    blocklist: &HostBlocklist,
) -> Result<SafeUrl, SsrfError> {
    // SEC PART 6: see `resolve_safe_url`.
    enforce_global_allowlist(raw)?;
    let (url, lower_host) = match validate_url_pre_dns(raw, allow_http, blocklist)? {
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
/// range check, hostname denylist, **operator blocklist**. No I/O.
fn validate_url_pre_dns(
    raw: &str,
    allow_http: bool,
    blocklist: &HostBlocklist,
) -> Result<ValidatedHost, SsrfError> {
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
    let lower_host = host.to_ascii_lowercase();

    // PH-WEB-BLOCKLIST: operator-curated host blocklist check runs
    // BEFORE the IP / hostname rules — so even loopback / metadata
    // hostnames that aren't on the hardcoded list but ARE on the
    // operator list reject with HostBlocked (clearer error
    // attribution than HostnameDenied).
    if blocklist.contains(&lower_host) {
        return Err(SsrfError::HostBlocked { host: lower_host });
    }

    if let Some(parsed) = parse_literal_ip(&host) {
        if let Some(reason) = forbidden_ip_reason(parsed) {
            return Err(SsrfError::IpForbidden { ip: parsed, reason });
        }
        return Ok(ValidatedHost::LiteralIp { url, ip: parsed });
    }
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
/// and `[::1]` style. `url::Url::host_str()` returns IPv6 hosts WITH the
/// surrounding brackets (per the url crate's API), so we strip them before
/// handing the bytes to `IpAddr::from_str` — without that, `[::1]` would
/// fall through to DNS, and platforms whose libc resolver rejects bracketed
/// literals (Ubuntu, macOS) would never reach the literal-IP range check.
fn parse_literal_ip(host: &str) -> Option<IpAddr> {
    let stripped = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    stripped.parse::<IpAddr>().ok().map(|ip| match ip {
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
        let e = resolve_safe_url("https://127.0.0.1/", false, &HostBlocklist::default())
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_file_scheme() {
        let e = resolve_safe_url("file:///etc/passwd", false, &HostBlocklist::default())
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::SchemeDenied { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_http_when_not_opted_in() {
        let e = resolve_safe_url("http://example.com/", false, &HostBlocklist::default())
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::SchemeDenied { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_invalid_url() {
        let e = resolve_safe_url("not a url", false, &HostBlocklist::default())
            .await
            .expect_err("should be rejected");
        assert!(matches!(e, SsrfError::BadUrl(_)), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_localhost_hostname() {
        let e = resolve_safe_url("https://localhost/", false, &HostBlocklist::default())
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
        let e = resolve_safe_url("https://[::1]/", false, &HostBlocklist::default())
            .await
            .expect_err("bracketed v6 loopback must be rejected");
        assert_v6_rejected(&e);
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_bracketed_ipv6_link_local() {
        let e = resolve_safe_url("https://[fe80::1]/", false, &HostBlocklist::default())
            .await
            .expect_err("bracketed v6 link-local must be rejected");
        assert_v6_rejected(&e);
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_mapped_v4_loopback_in_url() {
        // `[::ffff:127.0.0.1]` should unwrap to 127.0.0.1 either at
        // parse time or after DNS resolution — both paths must
        // refuse before any I/O.
        let e = resolve_safe_url(
            "https://[::ffff:127.0.0.1]/",
            false,
            &HostBlocklist::default(),
        )
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
            let e = resolve_safe_url(&url, false, &HostBlocklist::default())
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
        let e = resolve_safe_url("https://API.INTERNAL/", false, &HostBlocklist::default())
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
        let e = resolve_safe_url(
            "https://user:pass@127.0.0.1/",
            false,
            &HostBlocklist::default(),
        )
        .await
        .expect_err("userinfo must not mask the real host");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_with_explicit_port_still_checks_host() {
        let e = resolve_safe_url(
            "https://127.0.0.1:8443/path",
            false,
            &HostBlocklist::default(),
        )
        .await
        .expect_err("port must not bypass IP check");
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_url_without_host() {
        // `data:` URLs and similar exotic schemes parse but have no
        // host. Should produce a clean SchemeDenied (or NoHost) —
        // never a panic on .host_str().unwrap().
        let e = resolve_safe_url("data:text/plain,hello", false, &HostBlocklist::default())
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

    // ── PH-WEB-BLOCKLIST: operator-curated host blocklist ──────────

    #[test]
    fn host_blocklist_lowercases_and_dedupes_on_construction() {
        let bl = HostBlocklist::new([
            "Evil.example.com",
            "  evil.example.com  ",
            "EVIL.EXAMPLE.COM",
            "other.bad",
        ]);
        assert_eq!(bl.len(), 2);
        assert!(bl.contains("evil.example.com"));
        assert!(bl.contains("other.bad"));
        assert!(!bl.contains("safe.example.com"));
    }

    #[test]
    fn host_blocklist_drops_empty_entries() {
        let bl = HostBlocklist::new(["evil.example.com", "", "   ", "other.bad"]);
        assert_eq!(bl.len(), 2);
        assert!(!bl.contains(""));
    }

    #[test]
    fn host_blocklist_empty_default_contains_nothing() {
        let bl = HostBlocklist::default();
        assert!(bl.is_empty());
        assert!(!bl.contains("anything"));
    }

    #[test]
    fn host_blocklist_does_not_match_subdomains() {
        // Honesty guard: an entry for `evil.example.com` does NOT
        // block `sub.evil.example.com`. Operators must list
        // subdomains explicitly (matches URLhaus's per-hostname
        // feed granularity). Documenting this with a test so a
        // future "make it match subdomains" diff has to consciously
        // override the contract.
        let bl = HostBlocklist::new(["evil.example.com"]);
        assert!(bl.contains("evil.example.com"));
        assert!(!bl.contains("sub.evil.example.com"));
        assert!(!bl.contains("example.com"));
    }

    #[tokio::test]
    async fn resolve_safe_url_rejects_blocked_host_before_dns() {
        // The blocklist check must fire BEFORE the DNS lookup —
        // otherwise a typo-host on the blocklist would burn a
        // DNS round-trip. We verify by using a hostname that
        // would not resolve at all (`.invalid` TLD is reserved by
        // RFC 2606) but is on the blocklist.
        let bl = HostBlocklist::new(["does-not-resolve.invalid"]);
        let e = resolve_safe_url("https://does-not-resolve.invalid/", false, &bl)
            .await
            .expect_err("blocked host must reject before DNS");
        match e {
            SsrfError::HostBlocked { host } => {
                assert_eq!(host, "does-not-resolve.invalid");
            }
            other => panic!("expected HostBlocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_safe_url_blocklist_is_case_insensitive() {
        let bl = HostBlocklist::new(["evil.example.com"]);
        // URL parser normalizes the host to lowercase already, but
        // the blocklist itself must also be case-insensitive on
        // the input side (we lowercase entries on construction).
        let e = resolve_safe_url("https://EVIL.example.com/", false, &bl)
            .await
            .expect_err("uppercase variant must still match");
        assert!(matches!(e, SsrfError::HostBlocked { .. }), "got {e:?}");
    }

    #[tokio::test]
    async fn resolve_safe_url_blocklist_does_not_widen_to_subdomain() {
        // The runtime-policy contract: exact match only. A request
        // for `safe.example.com` does NOT get blocked even if
        // `example.com` is on the list. We use a real hostname
        // that resolves so the test exercises the full path (not
        // the pre-DNS short-circuit).
        let bl = HostBlocklist::new(["example.com"]);
        // We don't actually need DNS to confirm — we just need to
        // confirm the blocklist check did not match. If the call
        // fails on DnsFailed / DnsForbidden / network unreachable
        // that's fine — what we want is "NOT HostBlocked."
        let res = resolve_safe_url("https://safe.example.com/", false, &bl).await;
        if let Err(SsrfError::HostBlocked { host }) = &res {
            panic!("subdomain should not be blocked, but got HostBlocked('{host}')");
        }
    }

    #[tokio::test]
    async fn resolve_safe_url_blocklist_fires_before_hardcoded_denylist() {
        // Edge case: `localhost` is both on the hardcoded denylist
        // AND (now) the operator blocklist. The blocklist check
        // runs first, so the operator sees HostBlocked (clearer
        // attribution: "you told us to block this").
        let bl = HostBlocklist::new(["localhost"]);
        let e = resolve_safe_url("https://localhost/", false, &bl)
            .await
            .expect_err("blocked");
        assert!(
            matches!(e, SsrfError::HostBlocked { .. }),
            "blocklist must take precedence, got {e:?}"
        );
    }

    #[test]
    fn resolve_safe_url_blocking_respects_blocklist() {
        // Sync twin (used by redirect closure) must enforce the
        // same blocklist as the async path.
        let bl = HostBlocklist::new(["evil.example.com"]);
        let e = resolve_safe_url_blocking("https://evil.example.com/", false, &bl)
            .expect_err("blocking path must reject");
        assert!(matches!(e, SsrfError::HostBlocked { .. }), "got {e:?}");
    }

    // ── SEC PART 6: glob allowlist + DNS cache + SSRF check ──

    #[test]
    fn url_allowlist_glob_matches_subdomains() {
        let al = UrlAllowlist::new(["*.openai.com", "api.anthropic.com"]);
        assert!(al.allows("api.openai.com"));
        assert!(al.allows("any.subdomain.openai.com"));
        assert!(al.allows("api.anthropic.com"));
        assert!(!al.allows("openai.com.evil.com"));
        assert!(!al.allows("api.gemini.com"));
        assert!(!al.allows("api.anthropic.com.evil.com"));
    }

    #[test]
    fn url_allowlist_strips_scheme_and_path_from_operator_input() {
        let al = UrlAllowlist::new(["https://*.openai.com/v1/*"]);
        // The operator wrote a full URL; we normalise to host
        // pattern for matching.
        assert!(al.allows("api.openai.com"));
        assert!(al.allows("foo.bar.openai.com"));
    }

    #[test]
    fn url_allowlist_empty_means_no_restriction() {
        let al = UrlAllowlist::default();
        assert!(al.is_empty());
        assert!(al.allows("anything.example"));
    }

    #[test]
    fn cloud_tier_ssrf_blocks_loopback_literal() {
        let cache = DnsCache::new(60);
        let e = check_ssrf_cloud_tier("https://127.0.0.1/x", true, &cache).unwrap_err();
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[test]
    fn cloud_tier_ssrf_blocks_aws_metadata_literal() {
        let cache = DnsCache::new(60);
        let e = check_ssrf_cloud_tier("https://169.254.169.254/latest/", true, &cache).unwrap_err();
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[test]
    fn cloud_tier_ssrf_passes_when_disabled() {
        // SSRF disabled at the operator's request: even a
        // loopback literal passes through.
        let cache = DnsCache::new(60);
        check_ssrf_cloud_tier("https://127.0.0.1/", false, &cache)
            .expect("disabled-protection must skip the IP block");
    }

    #[test]
    fn dns_cache_returns_cached_resolution_on_hit() {
        // Drive the cache with the known-good `localhost`
        // resolution. Two calls; the second hits the cache —
        // we verify the second call returns IPs without
        // re-running the resolver (which the test can't
        // distinguish externally, so we settle for "both
        // succeed AND return the same answer").
        let cache = DnsCache::new(60);
        let first = cache.resolve_blocking("localhost").unwrap();
        let second = cache.resolve_blocking("localhost").unwrap();
        assert_eq!(first, second, "cached resolution must be identical");
        assert!(!first.is_empty());
    }

    #[test]
    fn check_ssrf_tool_capability_blocks_loopback() {
        let cache = DnsCache::new(60);
        let allowlist = UrlAllowlist::default();
        let e =
            check_ssrf_tool_capability("https://127.0.0.1/", true, &cache, &allowlist).unwrap_err();
        assert!(matches!(e, SsrfError::IpForbidden { .. }), "got {e:?}");
    }

    #[test]
    fn check_ssrf_tool_capability_allowlist_blocks_unlisted_host() {
        let cache = DnsCache::new(60);
        let allowlist = UrlAllowlist::new(["*.openai.com"]);
        let e = check_ssrf_tool_capability(
            "https://api.anthropic.com/v1/messages",
            true,
            &cache,
            &allowlist,
        )
        .unwrap_err();
        assert!(matches!(e, SsrfError::NotAllowlisted { .. }), "got {e:?}");
    }
}
