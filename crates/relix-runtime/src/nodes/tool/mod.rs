//! Tool node — first external-action capability for Relix.
//!
//! Registered capabilities on a controller with `[controller] node_type = "tool"`:
//!
//! - `tool.web_fetch` — HTTP(S) GET of a single URL, returning UTF-8 body text.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Argument is a UTF-8 string. Two forms accepted:
//!
//! | Arg | Meaning |
//! |---|---|
//! | `<url>` | GET the URL with default `max_bytes` |
//! | `<url>\|<n>` | GET the URL, cap body at `n` bytes (clamped to node `max_bytes`) |
//!
//! Returns the response body decoded as UTF-8. Non-UTF-8 bodies are an error.
//!
//! ## Security model — fail closed
//!
//! `tool.web_fetch` is a high-blast-radius capability: a chat user who can
//! reach it can ask Relix to dial arbitrary endpoints from the tool node.
//! SSRF protections live in [`security`] and run *before* any network I/O:
//!
//! 1. Scheme allowlist — `https` always, `http` only when
//!    `[tool] allow_http = true` (false by default).
//! 2. Reject any URL whose host parses as a literal IP in a forbidden range
//!    (loopback, link-local, private, unspecified, multicast, broadcast,
//!    documentation, benchmark, ULA, well-known cloud metadata endpoints).
//! 3. Resolve the hostname via the OS resolver and reject if *any* resolved
//!    address is forbidden. The fetch then targets a `SocketAddr` derived
//!    from the resolution, never the original hostname — this prevents
//!    DNS rebinding between the safety check and the actual connect.
//! 4. Enforce request/connect deadlines, a redirect cap, and a body cap.
//! 5. Refuse non-text/non-json/non-html `content-type`.
//!
//! Anything that fails returns a structured `ErrorEnvelope` (no partial body,
//! no exception, no panic). The audit log records the rejection cause.
//!
//! ## Out of scope (alpha)
//!
//! - No JS execution.
//! - No headless browser.
//! - No POST/PUT/DELETE.
//! - No streaming bodies (whole body is read into memory subject to the cap).
//! - No per-host rate limits beyond the controller's policy engine.
//!
//! These ship in later milestones if and when a flow needs them.

pub mod ask_human;
pub mod audio;
pub mod browser;
pub mod contracts;
pub mod dispatcher;
pub mod fs;
pub mod manifest;
pub mod mcp;
pub mod mcp_http;
pub mod mcp_stdio;
pub mod output_guard;
pub mod parse_document;
pub mod pdf;
pub mod perception;
pub mod registry;
pub mod sanitize;
pub mod screen;
pub mod security;
pub mod session_search_proxy;
pub mod terminal;
pub mod text_chunk;
pub mod web_extract;
pub mod web_robots;
pub mod web_search;
pub mod web_tools;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use security::{HostBlocklist, SsrfError, resolve_safe_url, resolve_safe_url_blocking};

/// Per-node tool configuration parsed from `[tool]` in the controller TOML.
///
/// SEC §14: `deny_unknown_fields` so an operator typo (e.g. the
/// legacy `max_body_bytes` instead of `max_bytes`) is a hard parse
/// error rather than a silently-dropped key. A silently-ignored
/// `max_body_bytes` left the SSRF body cap at its default while
/// the operator believed they had tightened it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolConfig {
    /// Maximum response body, bytes. Default 256 KiB; clients may request
    /// less via the `|N` arg form but never more.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    /// Per-request total deadline (connect + read), seconds. Default 15.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Max followed redirects. Default 3.
    #[serde(default = "default_max_redirects")]
    pub max_redirects: usize,
    /// Allow plain `http://`. Default `false` — fail-closed posture.
    #[serde(default)]
    pub allow_http: bool,
    /// `User-Agent` header sent with each request.
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Max bytes `tool.web_extract` will accept as input. Default 1 MiB.
    /// Pure parser — no network, no provider keys, just CPU bound to
    /// this cap.
    #[serde(default = "default_extract_max_input_bytes")]
    pub extract_max_input_bytes: usize,
    /// Optional path-jailed filesystem subsystem (B2). When `None`
    /// the four `tool.read_file` / `tool.write_file` /
    /// `tool.search_files` / `tool.patch` capabilities are NOT
    /// registered — the tool node serves only network + parse
    /// capabilities. The bringup script enables this by default.
    #[serde(default)]
    pub fs: Option<fs::FsJailConfig>,
    /// Optional PDF parser subsystem (B3). When `None` the
    /// `tool.pdf` capability is NOT registered. Always-safe to
    /// enable (pure parser, no network, no shell, no filesystem
    /// surface beyond the in-memory base64 input).
    #[serde(default)]
    pub pdf: Option<pdf::PdfConfig>,
    /// Optional terminal/shell subsystem (CW1). When `None`
    /// the `tool.terminal.run` capability is NOT registered.
    /// **High blast radius** — operators must opt in
    /// deliberately AND provide an allowlist of bare program
    /// names (no paths, no globs). See
    /// `crates/relix-runtime/src/nodes/tool/terminal.rs` for
    /// the full security model.
    #[serde(default)]
    pub terminal: Option<terminal::TerminalConfig>,
    /// CW4: optional browser-automation subsystem. When `None`
    /// the `tool.browser.*` capability surface is NOT registered.
    /// When present `backend = "none"` (default) advertises the
    /// surface but every navigate / screenshot returns
    /// BackendNotConnected — honest scaffold for a future
    /// Playwright integration. See
    /// `crates/relix-runtime/src/nodes/tool/browser.rs`.
    #[serde(default)]
    pub browser: Option<browser::BrowserConfig>,
    /// CW5: optional MCP (Model Context Protocol) registry +
    /// runtime projection. When `None` the `tool.mcp.*`
    /// capability family is NOT registered. When present the
    /// registry + discovery surface is wired; `tool.mcp.invoke`
    /// returns RuntimeNotConnected until the live client lands.
    /// See `crates/relix-runtime/src/nodes/tool/mcp.rs`.
    #[serde(default)]
    pub mcp: Option<mcp::McpConfig>,
    /// GAP 10 PART 1: tiered document-parsing pipeline. `None`
    /// uses [`parse_document::ParseDocumentConfig::default()`] which
    /// enables every cloud tier when its env var is set (Tavily-style
    /// opt-in via `scripts/setup.{sh,ps1}`).
    #[serde(default)]
    pub parse_document: Option<parse_document::ParseDocumentConfig>,
    /// GAP 10 PART 2: tiered web-read pipeline. Same env vars as the
    /// document pipeline (Jina / Firecrawl); LlamaParse is
    /// document-only.
    #[serde(default)]
    pub web_read: Option<parse_document::WebReadConfig>,
    /// GAP 10 PART 3: screen-capture surface. Default is `enabled =
    /// false` — operators must opt in explicitly because the tool
    /// captures the host's screen.
    #[serde(default)]
    pub screen: Option<screen::ScreenConfig>,
    /// PH-WEB-BLOCKLIST: operator-curated hostname blocklist.
    /// Every URL passed to `tool.web_fetch` / `tool.web_get` /
    /// `tool.web_extract` / `tool.web.post` / `tool.web.robots_check`
    /// is checked against this list (case-insensitive exact match)
    /// before scheme/DNS validation. Same check runs on every
    /// redirect target via the existing redirect policy.
    ///
    /// Honest scope: this is **operator-curated**, not a live
    /// threat feed. Refresh from URLhaus (or any other source) on
    /// whatever schedule fits — see
    /// `crates/relix-runtime/src/nodes/tool/security.rs` module
    /// doc for the curl recipe.
    #[serde(default)]
    pub blocked_hosts: Vec<String>,
    /// SEC PART 6: glob host patterns that the tool capability
    /// handlers (`tool.web_read` / `tool.web_get` /
    /// `tool.web_fetch` / `tool.browser.*`) are allowed to
    /// reach. Empty (the default) means NO allowlist filter —
    /// only the hardcoded SSRF private-range checks fire.
    /// Cloud-tier HTTP clients (LlamaParse / Jina / Firecrawl /
    /// Tavily / Brave / Perplexity) are EXEMPT from the
    /// allowlist — they still get the SSRF private-IP check.
    /// Each pattern is matched against the lowercased host
    /// portion of the URL with glob semantics (`*` matches any
    /// run of host-legal chars, including dots).
    #[serde(default)]
    pub url_allowlist: Vec<String>,
    /// SEC PART 6: master switch for the SSRF private-IP
    /// blocking. Defaults to `true` (fail-closed). When set
    /// `false` the controller startup logs a WARNING + every
    /// outbound HTTP call from a tool capability handler /
    /// cloud tier client skips the private-IP block (the URL
    /// allowlist still fires when configured). Intended for
    /// development against a local model server — production
    /// MUST leave this `true`.
    #[serde(default = "default_ssrf_protection")]
    pub ssrf_protection: bool,
}

fn default_ssrf_protection() -> bool {
    true
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_max_bytes(),
            timeout_secs: default_timeout_secs(),
            max_redirects: default_max_redirects(),
            allow_http: false,
            user_agent: default_user_agent(),
            extract_max_input_bytes: default_extract_max_input_bytes(),
            fs: None,
            pdf: None,
            terminal: None,
            browser: None,
            mcp: None,
            parse_document: None,
            web_read: None,
            screen: None,
            blocked_hosts: Vec::new(),
            url_allowlist: Vec::new(),
            ssrf_protection: default_ssrf_protection(),
        }
    }
}

fn default_max_bytes() -> usize {
    256 * 1024
}
fn default_timeout_secs() -> u64 {
    15
}
fn default_max_redirects() -> usize {
    3
}
fn default_user_agent() -> String {
    format!("Relix-tool/{}", env!("CARGO_PKG_VERSION"))
}
fn default_extract_max_input_bytes() -> usize {
    1024 * 1024
}

// ─────────────────────────── Client construction ───────────────────────────

/// Construct a reqwest client with the configured deadlines / redirect cap,
/// optionally pinning a hostname to a specific set of socket addresses (the
/// M9 DNS-pinning lever). When `pin` is `None` the resulting client behaves
/// like the default OS resolver — used only when the URL host is already a
/// literal IP (so there's nothing to resolve in the first place) or for the
/// startup probe.
///
/// Free function so [`PinnedClientPool`] can call it without holding a
/// reference to the `ToolBackend` while constructing.
fn build_client(
    cfg: &ToolConfig,
    pin: Option<(&str, &[SocketAddr])>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut b = reqwest::Client::builder()
        .user_agent(cfg.user_agent.clone())
        .timeout(Duration::from_secs(cfg.timeout_secs))
        .connect_timeout(Duration::from_secs(cfg.timeout_secs.min(10)))
        .redirect(ssrf_redirect_policy(cfg));
    if let Some((host, addrs)) = pin {
        b = b.resolve_to_addrs(host, addrs);
    }
    b.build()
}

/// Build a `reqwest::redirect::Policy::custom` that:
///
/// 1. Enforces `cfg.max_redirects` as a hard cap.
/// 2. Re-runs the SSRF guard against every redirect target via
///    [`resolve_safe_url_blocking`]. Closure is sync; DNS lookup blocks
///    the calling thread briefly. Redirects are rare; cost is small.
///
/// On rejection the closure returns `Action::error`, which surfaces from
/// `client.get(...).send()` as a `reqwest::Error` and the handler maps it
/// to a `transport`-class error envelope.
///
/// **The same policy is baked into every pooled Client**, so per-hop SSRF
/// re-validation runs on every hop regardless of which pooled Client
/// served the request.
fn ssrf_redirect_policy(cfg: &ToolConfig) -> reqwest::redirect::Policy {
    let max_redirects = cfg.max_redirects;
    let allow_http = cfg.allow_http;
    // PH-WEB-BLOCKLIST: re-check every redirect target against the
    // operator blocklist. Clone is cheap (Arc<HashSet>) so each
    // redirect doesn't re-build the lookup structure.
    let blocklist = HostBlocklist::new(cfg.blocked_hosts.iter().cloned());
    reqwest::redirect::Policy::custom(move |attempt| {
        let target_str = attempt.url().to_string();
        let previous_count = attempt.previous().len();
        let origin = attempt
            .previous()
            .first()
            .map(|u| u.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());

        if previous_count >= max_redirects {
            tracing::warn!(
                target_url = %target_str,
                origin_url = %origin,
                hops = previous_count,
                cap = max_redirects,
                "tool.web_fetch: redirect cap reached; refusing follow"
            );
            return attempt.error(SsrfError::BadUrl(format!(
                "tool.web_fetch: redirect cap ({max_redirects}) reached"
            )));
        }
        match resolve_safe_url_blocking(&target_str, allow_http, &blocklist) {
            Ok(_) => {
                tracing::debug!(
                    target_url = %target_str,
                    origin_url = %origin,
                    hops = previous_count,
                    "tool.web_fetch: redirect re-validated; following"
                );
                attempt.follow()
            }
            Err(e) => {
                tracing::warn!(
                    target_url = %target_str,
                    origin_url = %origin,
                    hops = previous_count,
                    reason = %e,
                    "tool.web_fetch: redirect ssrf-rejected; refusing follow"
                );
                attempt.error(e)
            }
        }
    })
}

// ─────────────────────────── Pinned client pool ────────────────────────────

/// Maximum number of (hostname, addrs) entries cached in the pool before we
/// start logging warnings. In practice the pool only ever holds as many
/// entries as the set of unique safe destinations a single Relix process
/// has fetched, which is operator-bounded.
const POOL_SOFT_CAP: usize = 256;

/// Key for [`PinnedClientPool`]: the hostname *and* the sorted set of
/// validated socket addrs. Two requests sharing both share a `Client`.
/// If DNS for the same hostname later returns a different safe address
/// set, the cache miss naturally creates a new entry — the old entry
/// lingers but only ever serves the IPs it was originally pinned to.
/// **This is the security invariant**: a pooled `Client` can never be
/// reused for a route that wasn't validated when it was created.
#[derive(Clone, Hash, PartialEq, Eq)]
struct PoolKey {
    hostname: String,
    /// Sorted to give a deterministic key even when DNS reorders addresses.
    addrs: Vec<SocketAddr>,
}

/// Per-(safe-route) cache of pinned reqwest clients + one shared unpinned
/// client for IP-literal URLs.
struct PinnedClientPool {
    cfg: ToolConfig,
    /// Pinned clients keyed by validated route.
    pinned: RwLock<HashMap<PoolKey, Arc<reqwest::Client>>>,
    /// Single shared client for IP-literal URLs (no DNS happens for those,
    /// so default-resolver behaviour is correct and no pin is needed).
    unpinned: Arc<reqwest::Client>,
    /// Observability counters. Atomic loads are cheap; we log periodic
    /// summaries via the tracing line on each miss so the log stream
    /// reflects pool health without standing telemetry.
    hits: AtomicU64,
    misses: AtomicU64,
}

impl PinnedClientPool {
    fn new(cfg: ToolConfig, unpinned: Arc<reqwest::Client>) -> Self {
        Self {
            cfg,
            pinned: RwLock::new(HashMap::new()),
            unpinned,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Return the shared no-pin client for IP-literal URLs.
    fn unpinned(&self) -> Arc<reqwest::Client> {
        // Same Arc is handed out to every caller; reqwest's connection
        // pool inside that Client takes care of cross-call connection
        // reuse for hosts that look like IP literals.
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.unpinned.clone()
    }

    /// Return a pinned client for (`hostname`, `addrs`), creating one on
    /// cache miss. Standard double-checked locking — on a miss we build
    /// outside the write lock and may race another thread building the
    /// same key (the duplicate gets dropped; no harm done).
    fn pinned(
        &self,
        hostname: &str,
        addrs: &[SocketAddr],
    ) -> Result<Arc<reqwest::Client>, reqwest::Error> {
        let mut sorted_addrs: Vec<SocketAddr> = addrs.to_vec();
        sorted_addrs.sort();
        sorted_addrs.dedup();
        let key = PoolKey {
            hostname: hostname.to_string(),
            addrs: sorted_addrs,
        };

        // Fast path: read lock, lookup, hit.
        if let Some(c) = self.pinned.read().expect("pool read lock").get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(c.clone());
        }

        // Slow path: build outside any lock so concurrent misses don't
        // serialise on TLS init.
        let fresh = build_client(&self.cfg, Some((hostname, &key.addrs)))?;
        let fresh_arc = Arc::new(fresh);

        let mut guard = self.pinned.write().expect("pool write lock");
        // Another thread might have inserted while we were building; in
        // that case use theirs and let ours drop. Either way we count
        // exactly one miss for this lookup.
        let stored = guard
            .entry(key.clone())
            .or_insert_with(|| fresh_arc.clone())
            .clone();
        let total = guard.len();
        let miss_count = self.misses.fetch_add(1, Ordering::Relaxed) + 1;
        drop(guard);

        tracing::info!(
            hostname = %hostname,
            pinned_addrs = ?key.addrs,
            pool_entries = total,
            pool_hits = self.hits.load(Ordering::Relaxed),
            pool_misses = miss_count,
            "tool.web_fetch: pool miss; built new pinned client"
        );
        if total >= POOL_SOFT_CAP {
            tracing::warn!(
                pool_entries = total,
                soft_cap = POOL_SOFT_CAP,
                "tool.web_fetch: pinned client pool exceeded soft cap (no eviction in alpha)"
            );
        }
        Ok(stored)
    }

    /// Snapshot observability counters. Used by tests.
    #[cfg(test)]
    fn counters(&self) -> (u64, u64, usize) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
            self.pinned.read().expect("pool read lock").len(),
        )
    }
}

/// Tool backend. Owns a [`PinnedClientPool`] so each fetch reuses an
/// already-built reqwest `Client` for `(hostname, validated-addrs)` it has
/// seen before — paying the TLS init + hyper connector cost once per safe
/// route instead of once per request. The pool's keying invariant
/// (`(hostname, sorted_safe_addrs)`) guarantees the same `Client` only
/// serves requests whose validated route matches what's pinned inside it.
pub struct ToolBackend {
    cfg: ToolConfig,
    pool: PinnedClientPool,
    /// PH-WEB-BLOCKLIST: operator-curated host blocklist. Built
    /// once at `new()` from `cfg.blocked_hosts`. Cheap-clone for
    /// the redirect closure.
    blocklist: HostBlocklist,
}

impl ToolBackend {
    /// Build the backend. Probes a client up-front so any TLS / config
    /// problem (e.g. an unusable root store) surfaces at startup, not on
    /// the first request. The probe client also seeds the unpinned slot of
    /// the pool, which serves IP-literal URLs (where no DNS happens).
    pub fn new(cfg: ToolConfig) -> Result<Self, ToolError> {
        let probe =
            build_client(&cfg, None).map_err(|e| ToolError::Build(format!("client probe: {e}")))?;
        let blocklist = HostBlocklist::new(cfg.blocked_hosts.iter().cloned());
        // PH-BROWSER-FEATURES: fail-fast at construction time if
        // `[tool.browser]` selects an uncompiled / unknown
        // backend. Honest startup error — no silent NoneBackend
        // fallback in `register()`.
        if let Some(br_cfg) = &cfg.browser {
            browser::validate_config(br_cfg).map_err(|e| {
                ToolError::Build(format!(
                    "[tool.browser] config rejected: {e}. \
                     Fix `backend = \"<name>\"` (one of {}) or rebuild Relix with the required feature.",
                    browser::KNOWN_BACKENDS.join("|")
                ))
            })?;
        }
        // PH-TERM-PTY: same posture as [tool.browser] — loud
        // startup error when `[tool.terminal] pty = true` is set
        // without the `terminal-pty` Cargo feature compiled.
        // The pipe-mode default (pty = false) is always accepted.
        if let Some(term_cfg) = &cfg.terminal {
            terminal::validate_config(term_cfg).map_err(ToolError::Build)?;
        }
        Ok(Self {
            cfg: cfg.clone(),
            pool: PinnedClientPool::new(cfg, Arc::new(probe)),
            blocklist,
        })
    }

    /// PH-WEB-BLOCKLIST: read-only accessor for the operator
    /// blocklist. Used by tests + future dashboard surfaces. The
    /// returned clone shares the underlying `Arc<HashSet>`.
    pub fn blocklist(&self) -> HostBlocklist {
        self.blocklist.clone()
    }

    /// Accessor used by [`register`] to forward the operator's
    /// `[tool] extract_max_input_bytes` to the `tool.web_extract`
    /// capability without making the whole `cfg` field public.
    pub fn extract_max_input_bytes(&self) -> usize {
        self.cfg.extract_max_input_bytes
    }

    /// PH-WEB-POST: accessor for the body-size cap. Used by
    /// `tool.web.post` when the caller doesn't supply
    /// `max_bytes`. Reads the same `[tool] max_bytes` knob the
    /// fetch path uses.
    pub fn max_bytes(&self) -> usize {
        self.cfg.max_bytes
    }

    /// Accessor for the optional `[tool.fs]` subsystem config. When
    /// `None`, [`register`] does not register the fs capabilities.
    pub fn fs_config(&self) -> Option<fs::FsJailConfig> {
        self.cfg.fs.clone()
    }

    /// Accessor for the optional `[tool.pdf]` subsystem config. When
    /// `None`, [`register`] does not register `tool.pdf`.
    pub fn pdf_config(&self) -> Option<pdf::PdfConfig> {
        self.cfg.pdf.clone()
    }

    /// Borrow the full `[tool]` config. Used by the GAP 10 register
    /// path so it can read `[tool.parse_document]` / `[tool.web_read]`
    /// / `[tool.screen]` without an extra plumbing arg.
    pub fn tool_config(&self) -> &ToolConfig {
        &self.cfg
    }

    /// Accessor for the optional `[tool.terminal]` subsystem config
    /// (CW1). When `None`, [`register`] does not register
    /// `tool.terminal.run`. High-blast-radius capability — operators
    /// must opt in deliberately AND supply an allowlist.
    pub fn terminal_config(&self) -> Option<terminal::TerminalConfig> {
        self.cfg.terminal.clone()
    }

    /// Accessor for the optional `[tool.browser]` subsystem config
    /// (CW4). When `None`, [`register`] does not register the
    /// `tool.browser.*` capability surface.
    pub fn browser_config(&self) -> Option<browser::BrowserConfig> {
        self.cfg.browser.clone()
    }

    /// Accessor for the optional `[tool.mcp]` subsystem config
    /// (CW5). When `None`, [`register`] does not register the
    /// `tool.mcp.*` capability surface.
    pub fn mcp_config(&self) -> Option<mcp::McpConfig> {
        self.cfg.mcp.clone()
    }

    /// Run the configured capability against a single URL.
    ///
    /// Order of operations matters for safety:
    ///
    /// 1. **Validate** scheme & host with `security::resolve_safe_url`. The
    ///    resolver returns every IP DNS gave us and rejects if *any* of
    ///    them is in a forbidden range (no DNS-rebind "pick the safe one"
    ///    smuggling).
    /// 2. **Look up** a pinned `Client` in [`PinnedClientPool`] keyed by
    ///    `(hostname, sorted_validated_addrs)`. On a hit we reuse the
    ///    existing TLS connector + connection pool; on a miss we build
    ///    a new `Client` with `resolve_to_addrs(hostname, validated_addrs)`
    ///    baked in and cache it. The keying invariant guarantees a Client
    ///    can only serve requests whose validated route matches what's
    ///    pinned inside it — DNS pinning is preserved across reuse.
    /// 3. **Send** the request. The URL still contains the hostname →
    ///    `Host` header and TLS SNI keep pointing at the original origin.
    /// 4. **Stream** the body into a bounded buffer; abort if the response
    ///    exceeds the cap. `content-type` is filtered to text-like.
    ///
    /// Per-hop redirect re-validation runs inside every pooled Client via
    /// the same `Policy::custom` closure (see [`ssrf_redirect_policy`]),
    /// so reuse never widens the redirect surface.
    pub async fn fetch(&self, raw_url: &str, max_bytes_request: usize) -> WebFetchOutcome {
        let cap = max_bytes_request.min(self.cfg.max_bytes).max(1);

        let target = match resolve_safe_url(raw_url, self.cfg.allow_http, &self.blocklist).await {
            Ok(t) => t,
            Err(e) => return WebFetchOutcome::Rejected(e),
        };

        // M9 DNS pinning: pre-compute the SocketAddrs we will allow the
        // hostname to resolve to. For an IP-literal URL there is nothing to
        // pin (reqwest does not run the resolver in that case).
        let host_str = target
            .normalized_url
            .host_str()
            .expect("resolve_safe_url guarantees a host")
            .to_string();
        let port = target.normalized_url.port_or_known_default().unwrap_or(
            if target.normalized_url.scheme() == "https" {
                443
            } else {
                80
            },
        );
        let pinned_addrs: Vec<SocketAddr> = target
            .resolved
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        let is_ip_literal = host_str.parse::<IpAddr>().is_ok();

        let client = if is_ip_literal {
            self.pool.unpinned()
        } else {
            match self.pool.pinned(host_str.as_str(), pinned_addrs.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    return WebFetchOutcome::Transport(format!("client build with pin: {e}"));
                }
            }
        };

        let url = target.normalized_url.clone();
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => return WebFetchOutcome::Transport(e.to_string()),
        };

        let status = resp.status();
        if !status.is_success() {
            return WebFetchOutcome::HttpStatus {
                status: status.as_u16(),
                final_url: resp.url().to_string(),
            };
        }

        // Reject content-types that obviously aren't text. We do not try to
        // sniff bodies — refuse anything that isn't text/* application/json
        // or application/xhtml+xml.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !is_textual_content_type(&content_type) {
            return WebFetchOutcome::ContentTypeRejected {
                content_type,
                final_url: resp.url().to_string(),
            };
        }

        // Respect server-supplied Content-Length cap as a fast reject.
        if let Some(len) = resp.content_length()
            && (len as usize) > cap
        {
            return WebFetchOutcome::TooLarge {
                declared_bytes: len,
                cap,
            };
        }

        let final_url = resp.url().to_string();

        // Bounded read.
        let mut acc: Vec<u8> = Vec::with_capacity(cap.min(16 * 1024));
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => return WebFetchOutcome::Transport(e.to_string()),
            };
            if acc.len() + bytes.len() > cap {
                return WebFetchOutcome::TooLarge {
                    declared_bytes: (acc.len() + bytes.len()) as u64,
                    cap,
                };
            }
            acc.extend_from_slice(&bytes);
        }

        match String::from_utf8(acc) {
            Ok(body) => WebFetchOutcome::Ok {
                body,
                final_url,
                content_type,
            },
            Err(_) => WebFetchOutcome::NotUtf8 { final_url },
        }
    }

    /// PH-WEB-POST: HTTP POST against an external URL with the
    /// same SSRF + DNS pin + redirect re-validation pipeline as
    /// [`Self::fetch`]. The body is sent verbatim; the optional
    /// `cookie` parameter is forwarded as a raw `Cookie:` header
    /// (no jar / parsing / expiry tracking on Relix's side —
    /// operators thread cookies through manually for now).
    /// Returns the responder's `Set-Cookie` headers verbatim in
    /// the success / non-2xx / not-utf8 paths.
    pub async fn post(
        &self,
        raw_url: &str,
        body: &str,
        content_type: &str,
        cookie: &str,
        max_bytes_request: usize,
    ) -> WebPostOutcome {
        let cap = max_bytes_request.min(self.cfg.max_bytes).max(1);

        let target = match resolve_safe_url(raw_url, self.cfg.allow_http, &self.blocklist).await {
            Ok(t) => t,
            Err(e) => return WebPostOutcome::Rejected(e),
        };

        let host_str = target
            .normalized_url
            .host_str()
            .expect("resolve_safe_url guarantees a host")
            .to_string();
        let port = target.normalized_url.port_or_known_default().unwrap_or(
            if target.normalized_url.scheme() == "https" {
                443
            } else {
                80
            },
        );
        let pinned_addrs: Vec<SocketAddr> = target
            .resolved
            .iter()
            .map(|ip| SocketAddr::new(*ip, port))
            .collect();
        let is_ip_literal = host_str.parse::<IpAddr>().is_ok();

        let client = if is_ip_literal {
            self.pool.unpinned()
        } else {
            match self.pool.pinned(host_str.as_str(), pinned_addrs.as_slice()) {
                Ok(c) => c,
                Err(e) => {
                    return WebPostOutcome::Transport(format!("client build with pin: {e}"));
                }
            }
        };

        let url = target.normalized_url.clone();
        let mut req = client.post(url).body(body.to_string());
        if !content_type.is_empty() {
            req = req.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if !cookie.is_empty() {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return WebPostOutcome::Transport(e.to_string()),
        };

        // Collect Set-Cookie headers before consuming the body.
        let set_cookies: Vec<String> = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(|s| s.to_string()))
            .collect();

        let status = resp.status();
        let final_url = resp.url().to_string();
        if !status.is_success() {
            return WebPostOutcome::HttpStatus {
                status: status.as_u16(),
                final_url,
                set_cookies,
            };
        }

        let content_type_resp = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !is_textual_content_type(&content_type_resp) {
            return WebPostOutcome::ContentTypeRejected {
                content_type: content_type_resp,
                final_url,
            };
        }
        if let Some(len) = resp.content_length()
            && (len as usize) > cap
        {
            return WebPostOutcome::TooLarge {
                declared_bytes: len,
                cap,
            };
        }

        let mut acc: Vec<u8> = Vec::with_capacity(cap.min(16 * 1024));
        let mut stream = resp.bytes_stream();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(e) => return WebPostOutcome::Transport(e.to_string()),
            };
            if acc.len() + bytes.len() > cap {
                return WebPostOutcome::TooLarge {
                    declared_bytes: (acc.len() + bytes.len()) as u64,
                    cap,
                };
            }
            acc.extend_from_slice(&bytes);
        }

        match String::from_utf8(acc) {
            Ok(body) => WebPostOutcome::Ok {
                body,
                final_url,
                content_type: content_type_resp,
                set_cookies,
            },
            Err(_) => WebPostOutcome::NotUtf8 {
                final_url,
                set_cookies,
            },
        }
    }
}

fn is_textual_content_type(ct: &str) -> bool {
    let lower = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if lower.is_empty() {
        // Be conservative: when the server omits a content-type, still allow
        // (many small static endpoints do this). Body must still be valid UTF-8.
        return true;
    }
    lower.starts_with("text/")
        || lower == "application/json"
        || lower == "application/ld+json"
        || lower == "application/xml"
        || lower == "application/xhtml+xml"
        || lower.ends_with("+json")
        || lower.ends_with("+xml")
}

/// Outcome the handler maps to either `Ok` or a typed `ErrorEnvelope`.
#[derive(Debug, Clone)]
pub enum WebFetchOutcome {
    /// Successful fetch, body decoded as UTF-8.
    Ok {
        body: String,
        final_url: String,
        content_type: String,
    },
    /// SSRF / scheme / host rejection — never touched the network at all
    /// (or only resolved DNS).
    Rejected(SsrfError),
    /// Body or declared `Content-Length` exceeded the cap.
    TooLarge { declared_bytes: u64, cap: usize },
    /// Non-2xx response.
    HttpStatus { status: u16, final_url: String },
    /// Server returned a non-text content type.
    ContentTypeRejected {
        content_type: String,
        final_url: String,
    },
    /// Body bytes did not decode as UTF-8.
    NotUtf8 { final_url: String },
    /// Transport-level failure (DNS during reqwest, TLS, RST, etc.).
    Transport(String),
}

/// PH-WEB-POST: outcome of [`ToolBackend::post`]. Parallel to
/// [`WebFetchOutcome`] but the success path additionally
/// carries the responder's `Set-Cookie` headers verbatim, so
/// SOL flows / chat sessions can stitch a session token across
/// calls without losing the raw bytes.
#[derive(Debug)]
pub enum WebPostOutcome {
    Ok {
        body: String,
        final_url: String,
        content_type: String,
        set_cookies: Vec<String>,
    },
    Rejected(SsrfError),
    TooLarge {
        declared_bytes: u64,
        cap: usize,
    },
    HttpStatus {
        status: u16,
        final_url: String,
        set_cookies: Vec<String>,
    },
    ContentTypeRejected {
        content_type: String,
        final_url: String,
    },
    NotUtf8 {
        final_url: String,
        set_cookies: Vec<String>,
    },
    Transport(String),
}

/// Capability descriptor for `tool.terminal.run` (CW1).
/// Only added to the manifest when `[tool.terminal]` is
/// configured AND the allowlist validates. Sensitivity
/// tags reflect the high blast radius so policy engines +
/// dashboard surfaces can treat it specially.
pub fn terminal_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.run");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "shell:execute".into(),
        "host:local".into(),
        "destructive:potential".into(),
    ];
    d.policy_attachment_point = "tool.terminal.run".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Sandboxed shell command execution with operator allowlist, \
         no shell interpretation, hard timeout, and stdout/stderr caps."
            .into(),
    );
    d.categories = vec!["shell".into(), "execute".into(), "io".into()];
    d.environment_requirements = vec!["host:exec".into()];
    d.risk_level = RiskLevel::High;
    d
}

/// Capability descriptor for `tool.web_fetch`. Exposed so future manifest
/// exchange (M10) can broadcast it. Today it's read by [`register`] only.
pub fn capability_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web_fetch");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    // Tool calls touch the outside world. Treat as non-idempotent: the same
    // URL may return different bodies on each fetch.
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec!["external:network".into(), "egress:http".into()];
    d.policy_attachment_point = "tool.web_fetch".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some("Fetch a URL with SSRF + DNS pin + per-hop redirect re-check.".into());
    d.categories = vec!["fetch".into(), "io".into()];
    d.environment_requirements = vec!["network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// SEC §14: the full set of capability descriptors a tool node
/// advertises for `cfg`. This is the SINGLE source of truth the
/// controller iterates when building the manifest (see
/// `register_node_type_handlers`), and the policy-coverage contract
/// test diffs `configs/policies/tool.toml` against it — so a tool
/// capability added here without a matching policy rule fails that
/// test. The conditional subsystems (fs / pdf / terminal / browser /
/// mcp) advertise exactly when their config block is present AND
/// validates, matching the handler registration so consumers never
/// see a phantom capability.
pub fn advertised_capabilities(cfg: &ToolConfig) -> Vec<CapabilityDescriptor> {
    let mut caps = vec![
        capability_descriptor(),
        web_extract::capability_descriptor(),
        web_tools::web_get_descriptor(),
        web_tools::web_search_descriptor(),
        web_tools::web_post_descriptor(),
        web_tools::web_blocklist_summary_descriptor(),
        web_robots::robots_check_descriptor(),
        text_chunk::capability_descriptor(),
        ask_human::AskHumanTool::descriptor(),
        session_search_proxy::descriptor(),
    ];
    if cfg.fs.is_some() {
        caps.extend([
            fs::descriptor_read(),
            fs::descriptor_write(),
            fs::descriptor_search(),
            fs::descriptor_patch(),
            fs::descriptor_list(),
            fs::descriptor_append(),
            fs::descriptor_patch_preview(),
            fs::descriptor_binary_sniff(),
            fs::descriptor_audit_recent(),
            fs::descriptor_fuzzy_replace(),
            fs::descriptor_tree(),
            fs::descriptor_stat(),
        ]);
    }
    if cfg.pdf.is_some() {
        caps.push(pdf::capability_descriptor());
    }
    if let Some(term_cfg) = cfg.terminal.as_ref()
        && terminal::TerminalBackend::new(term_cfg.clone()).is_ok()
    {
        caps.extend([
            terminal_descriptor(),
            terminal::descriptor_sessions(),
            terminal::descriptor_audit_recent(),
            terminal::descriptor_cancel(),
            terminal::descriptor_tail(),
            terminal::descriptor_spawn(),
            terminal::descriptor_shell_open(),
            terminal::descriptor_shell_input(),
            terminal::descriptor_shell_close(),
            terminal::descriptor_shell_control(),
        ]);
    }
    if let Some(br_cfg) = cfg.browser.as_ref()
        && browser::build_backend(br_cfg).is_ok()
    {
        caps.extend([
            browser::descriptor_open_session(),
            browser::descriptor_close_session(),
            browser::descriptor_navigate(),
            browser::descriptor_get_text(),
            browser::descriptor_screenshot(),
            browser::descriptor_list_sessions(),
            browser::descriptor_click(),
            browser::descriptor_type_text(),
            browser::descriptor_wait_for_selector(),
            browser::descriptor_capture_read(),
        ]);
    }
    if let Some(mcp_cfg) = cfg.mcp.as_ref()
        && mcp::validate_config(mcp_cfg).is_ok()
    {
        caps.extend([
            mcp::descriptor_list_servers(),
            mcp::descriptor_list_tools(),
            mcp::descriptor_invoke(),
        ]);
    }
    caps
}

/// Register tool capabilities on the dispatch bridge. Wires every
/// `tool.*` capability the node exposes; today: `tool.web_fetch` (M9)
/// and `tool.web_extract` (B1).
pub fn register(
    bridge: &mut DispatchBridge,
    backend: Arc<ToolBackend>,
    operator_channel: ask_human::OperatorChannelHandle,
    memory_session_search: session_search_proxy::MemorySessionSearchProxyHandle,
) {
    let backend_for_handler = backend.clone();
    bridge.register(
        "tool.web_fetch",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = backend_for_handler.clone();
            async move { handle_web_fetch(b, ctx).await }
        })),
    );

    // tool.web_extract — pure HTML parser, no network surface. Shares
    // the tool node's existing identity / admission / audit setup.
    let extract_cfg = Arc::new(web_extract::WebExtractConfig {
        max_input_bytes: backend.extract_max_input_bytes(),
    });
    web_extract::register(bridge, extract_cfg);

    // CW3: tool.web_get + tool.web_search — composed over the same
    // ToolBackend.fetch() pipeline so SSRF / DNS pin / per-hop
    // redirect re-validation / content-type filter all apply
    // unchanged. No new credential surface, no new egress primitive.
    tracing::info!("tool node: registering tool.web_get + tool.web_search (CW3)");
    web_tools::register(bridge, backend.clone());

    // PH-WEB-ROBOTS: tool.web.robots_check — robots.txt sniff over
    // the same ToolBackend.fetch() pipeline so SSRF + pin + redirect
    // re-validation all apply. Pure safety surface — does NOT enforce.
    tracing::info!("tool node: registering tool.web.robots_check (PH-WEB-ROBOTS)");
    web_robots::register(bridge, backend.clone());

    // RELIX-GAP-10: tool.parse_document + tool.web_read — the
    // spec-named perception caps with the cloud → local tier
    // fallthrough (LlamaParse / Jina Reader / Firecrawl). When a
    // tier's env var is unset or returns an error, the pipeline
    // silently falls through to the next tier and ultimately to
    // the always-on local tier (lopdf for PDFs, ToolBackend
    // fetch+extract for URLs). See
    // `nodes/tool/parse_document.rs` module docs.
    tracing::info!("tool node: registering tool.parse_document + tool.web_read (GAP 10 tiered)");
    let pdf_cfg_for_perception = backend.pdf_config().map(Arc::new);
    let parse_cfg = Arc::new(
        backend
            .tool_config()
            .parse_document
            .clone()
            .unwrap_or_default(),
    );
    parse_document::register(bridge, backend.clone(), pdf_cfg_for_perception, parse_cfg);
    let web_read_cfg = Arc::new(backend.tool_config().web_read.clone().unwrap_or_default());
    parse_document::register_web_read(bridge, backend.clone(), web_read_cfg);

    // RELIX-GAP-10 PART 3: tool.screen — cross-platform screen
    // capture (scrot/imagemagick on Linux, screencapture on macOS,
    // PowerShell on Windows). Default is `enabled = false` — the
    // surface is opt-in because it captures the host's screen.
    if let Some(screen_cfg) = backend.tool_config().screen.clone() {
        if screen_cfg.enabled {
            tracing::info!("tool node: registering tool.screen (GAP 10 PART 3)");
        } else {
            tracing::debug!(
                "tool node: tool.screen registered but [tool.screen] enabled = false; calls will surface a disabled error"
            );
        }
        screen::register(bridge, Arc::new(screen_cfg));
    } else {
        // Register a default-disabled instance so calls get a
        // structured error instead of UNKNOWN_METHOD.
        screen::register(bridge, Arc::new(screen::ScreenConfig::default()));
    }

    // PH-PDF-CHUNK: tool.text.chunk — pure CPU text chunker. No
    // config gating; always registered alongside the parsing
    // capabilities since it's stateless and decision-free.
    tracing::info!("tool node: registering tool.text.chunk (PH-PDF-CHUNK)");
    text_chunk::register(bridge);

    // memory.session_search — proxy from the tool node to the
    // memory peer so agents can search their own chat-turn
    // history during task execution. Registered unconditionally
    // (descriptor advertises in the manifest); the handler
    // returns PEER_UNREACHABLE until [tool.memory_peer] is
    // configured and the controller populates the proxy cell.
    tracing::info!("tool node: registering memory.session_search (W7-SEARCH)");
    let session_search_handle = memory_session_search.clone();
    bridge.register(
        session_search_proxy::descriptor().method_name.as_str(),
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let cell = session_search_handle.clone();
            async move { session_search_proxy::handle(cell.as_ref(), &ctx).await }
        })),
    );

    // tool.ask_human — first-class "ask the operator" capability.
    // Registered unconditionally so planners + flows can call it
    // from anywhere on the mesh. The W3 OperatorChannel handle
    // is consulted on every call; when a channel is wired the
    // closure forwards the question + awaits the reply. When
    // unwired the handler surfaces `{"timeout": true}` to keep
    // the deterministic operator-not-available contract.
    tracing::info!("tool node: registering tool.ask_human");
    let operator_for_handler = operator_channel.clone();
    bridge.register(
        ask_human::AskHumanTool::descriptor().method_name.as_str(),
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let chan = operator_for_handler.clone();
            async move {
                let args = match std::str::from_utf8(&ctx.args) {
                    Ok(s) => s.to_string(),
                    Err(e) => {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("tool.ask_human arg utf8: {e}"),
                            retry_hint: 2,
                            retry_after: None,
                        });
                    }
                };
                ask_human::AskHumanTool::handle(&args, |question, timeout_secs| async move {
                    match chan.get() {
                        Some(c) => c.ask(question, timeout_secs).await,
                        None => None,
                    }
                })
                .await
            }
        })),
    );

    // B2: tool.read_file / write_file / search_files / patch.
    // Only registered when the operator opted in by setting
    // `[tool.fs]` in the controller TOML. Bringup script enables this
    // by default under `dev-data/<run>/fs-jail`. When None, the four
    // capabilities are not registered and the tool node serves only
    // network + parse.
    if let Some(fs_cfg) = backend.fs_config() {
        match fs::FsJail::new(fs_cfg) {
            Ok(jail) => {
                tracing::info!("tool node: registering fs subsystem (read/write/search/patch)");
                fs::register(bridge, Arc::new(jail));
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "tool node: [tool.fs] config invalid; fs subsystem disabled"
                );
            }
        }
    } else {
        tracing::info!(
            "tool node: [tool.fs] not configured; fs subsystem disabled (read/write/search/patch unavailable)"
        );
    }

    // B3: tool.pdf — pure parser over base64-encoded bytes. Opt-in
    // via [tool.pdf]; bringup script enables it by default.
    if let Some(pdf_cfg) = backend.pdf_config() {
        tracing::info!(
            max_input_bytes = pdf_cfg.max_input_bytes,
            max_pages = pdf_cfg.max_pages,
            max_output_chars = pdf_cfg.max_output_chars,
            "tool node: registering tool.pdf"
        );
        pdf::register(bridge, Arc::new(pdf_cfg));
    }

    // CW1: tool.terminal.run — sandboxed shell execution.
    // Opt-in via [tool.terminal] AND requires an allowlist
    // (see terminal.rs). Construction fails closed; we log
    // the rejection clearly so operators see exactly what
    // their config got wrong.
    if let Some(term_cfg) = backend.terminal_config() {
        match terminal::TerminalBackend::new(term_cfg) {
            Ok(tb) => {
                tracing::info!(
                    allowed = ?tb_allowed_summary(&tb),
                    "tool node: registering tool.terminal.run (CW1)"
                );
                terminal::register(bridge, Arc::new(tb));
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "tool node: [tool.terminal] present but invalid; capability NOT registered"
                );
            }
        }
    } else {
        tracing::info!(
            "tool node: [tool.terminal] not configured; terminal subsystem disabled (tool.terminal.run unavailable)"
        );
    }

    // CW4: tool.browser.* — browser-automation surface. Opt-in
    // via [tool.browser]. Today the only working backend is
    // "none" (returns BackendNotConnected on every non-noop
    // call) — see browser.rs honesty contract. The capability
    // surface is registered so operators see the wired methods
    // in the manifest + dashboard, and so a future Playwright
    // backend slots in without touching this register path.
    if let Some(br_cfg) = backend.browser_config() {
        // PH-BROWSER-FEATURES: ToolBackend::new validated the
        // browser config at startup, so build_backend cannot
        // fail here under normal control flow. The defensive
        // branch logs + skips registration rather than
        // panicking — but the failure path should never fire.
        match browser::build_backend(&br_cfg) {
            Ok(bb) => {
                tracing::info!(
                    backend = bb.name(),
                    max_sessions = br_cfg.max_sessions,
                    "tool node: registering tool.browser.* (PH-BROWSER-FEATURES — see browser/mod.rs)"
                );
                // W2-002f: thread the captures dir through so the
                // new `tool.browser.capture_read` capability can
                // serve previously-failed screenshots back to the
                // operator dashboard.
                browser::register(bridge, bb, br_cfg.screenshot_on_failure_dir.clone());
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "tool node: browser::build_backend failed AFTER validate_config passed; \
                     this should be unreachable — file a bug"
                );
            }
        }
    } else {
        tracing::info!(
            "tool node: [tool.browser] not configured; browser subsystem disabled (tool.browser.* unavailable)"
        );
    }

    // CW5 / F13: tool.mcp.* — MCP registry + live stdio AND
    // http runtime. Opt-in via [tool.mcp]. stdio servers
    // spawn lazily on first invoke / list_tools; http
    // servers run a boot-time `tools/list` probe that warms
    // the connection and surfaces the discovered tool
    // count in the startup log. Boot-time HTTP failures are
    // logged but do NOT fail the tool node — a down server
    // is still recoverable on the next call.
    if let Some(mcp_cfg) = backend.mcp_config() {
        match mcp::McpRegistry::new(mcp_cfg) {
            Ok(reg) => {
                tracing::info!(
                    servers = reg.server_count(),
                    "tool node: registering tool.mcp.* (registry + live stdio + http runtime)"
                );
                let reg = Arc::new(reg);
                // Spawn boot-time HTTP discovery so the
                // tool node doesn't block startup on a slow
                // / down server. The clone is cheap (Arc).
                let reg_for_discovery = reg.clone();
                tokio::spawn(async move {
                    let ok = reg_for_discovery.discover_http_tools().await;
                    tracing::info!(
                        http_servers_warmed = ok,
                        "tool node: mcp http boot-time discovery complete"
                    );
                });
                mcp::register(bridge, reg);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "tool node: [tool.mcp] present but invalid; mcp surface NOT registered"
                );
            }
        }
    } else {
        tracing::info!(
            "tool node: [tool.mcp] not configured; mcp subsystem disabled (tool.mcp.* unavailable)"
        );
    }
}

/// Helper used only by the registration tracing call above.
/// Reads the validated backend's allowlist size as a quick
/// "what did we enable" signal in the startup log without
/// dumping every binary name (operators have the TOML).
fn tb_allowed_summary(_b: &terminal::TerminalBackend) -> String {
    // The allowlist is private to the backend; for logging
    // purposes the size is enough — operators reading the
    // log don't need every binary name (the TOML is the
    // authoritative reference).
    "configured".to_string()
}

async fn handle_web_fetch(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.web_fetch arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // `<url>` or `<url>|<n>`. URLs are not allowed to contain `|`.
    let (raw_url, max_bytes) = match s.rsplit_once('|') {
        Some((url, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (url.trim(), n_str.trim().parse::<usize>().unwrap_or(0))
        }
        _ => (s.trim(), backend.cfg.max_bytes),
    };
    if raw_url.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.web_fetch: url required (arg: `<url>` or `<url>|<n>`)".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }

    match backend.fetch(raw_url, max_bytes).await {
        WebFetchOutcome::Ok { body, .. } => HandlerOutcome::Ok(body.into_bytes()),
        WebFetchOutcome::Rejected(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!("tool.web_fetch ssrf-rejected: {e}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::TooLarge {
            declared_bytes,
            cap,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_fetch body too large: declared={declared_bytes}B cap={cap}B"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::HttpStatus { status, final_url } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.web_fetch http {status} for {final_url}"),
            retry_hint: 1,
            retry_after: None,
        }),
        WebFetchOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.web_fetch content-type not text-like: '{content_type}' for {final_url}"
            ),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::NotUtf8 { final_url } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_fetch body not utf-8 for {final_url}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::Transport(c) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::TRANSPORT,
            cause: format!("tool.web_fetch transport: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

/// Tool-node errors surfaced at construction time.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// HTTP client could not be built (TLS init, etc.).
    #[error("build: {0}")]
    Build(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // W3 — ask_human registration. The first test confirms the
    // capability appears in the bridge's handler table at all;
    // the second drives a request through the admission
    // pipeline end-to-end and asserts AskHumanTool::handle
    // returned the documented `{"timeout": true}` JSON shape
    // (i.e. the call routed to the handler, not back as a
    // CapabilityNotFound).

    fn fresh_bridge_for_tool() -> (
        crate::dispatch::DispatchBridge,
        ed25519_dalek::SigningKey,
        tempfile::TempDir,
    ) {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let dir = tempfile::TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = relix_core::policy::PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "anyone_ask_human"
            method = "tool.ask_human"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = crate::dispatch::DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, org_root, dir)
    }

    fn aic_for(
        org_root: &ed25519_dalek::SigningKey,
        name: &str,
        groups: &[&str],
    ) -> bundle::Bundle {
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = relix_core::identity::IdentityBundle {
            subject_id: relix_core::types::NodeId::from_pubkey(
                &caller_key.verifying_key().to_bytes(),
            ),
            name: name.into(),
            org_id: relix_core::types::NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        relix_core::identity::issue_identity(id, org_root, 3600).unwrap()
    }

    use relix_core::bundle;

    #[tokio::test]
    async fn tool_ask_human_is_registered_by_tool_register() {
        let (mut bridge, _org_root, _dir) = fresh_bridge_for_tool();
        let backend = Arc::new(ToolBackend::new(ToolConfig::default()).unwrap());
        register(
            &mut bridge,
            backend,
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
        );
        assert!(
            bridge.has_handler("tool.ask_human"),
            "tool.ask_human must be registered after tool::register"
        );
    }

    #[tokio::test]
    async fn tool_ask_human_returns_canned_reply_when_operator_channel_is_wired() {
        use crate::transport::envelope::ResponseResult;
        let (mut bridge, org_root, _dir) = fresh_bridge_for_tool();
        let backend = Arc::new(ToolBackend::new(ToolConfig::default()).unwrap());
        let channel: ask_human::OperatorChannelHandle =
            std::sync::Arc::new(tokio::sync::OnceCell::new());
        if channel
            .set(std::sync::Arc::new(ask_human::CannedReplyChannel {
                reply: "approved by stub operator".into(),
            }))
            .is_err()
        {
            panic!("channel OnceCell already set");
        }
        register(
            &mut bridge,
            backend,
            channel,
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
        );
        let aic = aic_for(&org_root, "alice", &["chat-users"]);
        let args = br#"{"question":"deploy now?","timeout_secs":5}"#.to_vec();
        let envelope = crate::dispatch::build_request("tool.ask_human", args, aic, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = crate::dispatch::decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(body) => {
                let s = String::from_utf8(body.into_vec()).unwrap();
                assert!(
                    s.contains("\"answer\":\"approved by stub operator\""),
                    "expected canned reply, got {s}"
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_session_search_is_registered_by_tool_register() {
        let (mut bridge, _, _dir) = fresh_bridge_for_tool();
        let backend = Arc::new(ToolBackend::new(ToolConfig::default()).unwrap());
        register(
            &mut bridge,
            backend,
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
        );
        assert!(
            bridge.has_handler("memory.session_search"),
            "memory.session_search must be registered after tool::register"
        );
    }

    #[tokio::test]
    async fn memory_session_search_routes_through_proxy_cell_when_wired() {
        use crate::transport::envelope::ResponseResult;
        let (bridge, org_root, _dir) = fresh_bridge_for_tool();
        let backend = Arc::new(ToolBackend::new(ToolConfig::default()).unwrap());
        // Patch the cell with a stub that returns a canned JSON
        // body so the bridge round-trip lands real data.
        struct StubProxy;
        #[async_trait::async_trait]
        impl session_search_proxy::MemorySessionSearchProxy for StubProxy {
            async fn call(&self, args: &str) -> Result<String, String> {
                assert_eq!(args, "alice|needle|10");
                Ok(
                    r#"[{"session_id":"sess-A","role":"user","content":"hello needle"}]"#
                        .to_string(),
                )
            }
        }
        let cell: session_search_proxy::MemorySessionSearchProxyHandle =
            std::sync::Arc::new(tokio::sync::OnceCell::new());
        cell.set(std::sync::Arc::new(StubProxy)
            as std::sync::Arc<
                dyn session_search_proxy::MemorySessionSearchProxy,
            >)
        .ok();
        // Open a writable policy so the test's identity passes
        // admission for memory.session_search.
        let policy = relix_core::policy::PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "anyone_session_search"
            method = "memory.session_search"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        // Re-create bridge with the open policy.
        let _ = bridge; // discard the strict one
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        let responder = SigningKey::generate(&mut OsRng);
        let dir = tempfile::TempDir::new().unwrap();
        let mut bridge = crate::dispatch::DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        register(
            &mut bridge,
            backend,
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
            cell,
        );
        let aic = aic_for(&org_root, "alice", &["chat-users"]);
        let envelope = crate::dispatch::build_request(
            "memory.session_search",
            b"alice|needle|10".to_vec(),
            aic,
            30,
        );
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = crate::dispatch::decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(body) => {
                let s = String::from_utf8(body.into_vec()).unwrap();
                assert!(s.contains("sess-A"));
                assert!(s.contains("hello needle"));
            }
            ResponseResult::Err(e) => panic!("unexpected err: kind={} cause={}", e.kind, e.cause),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_ask_human_routes_through_mesh_to_handle_not_capability_not_found() {
        use crate::transport::envelope::ResponseResult;
        let (mut bridge, org_root, _dir) = fresh_bridge_for_tool();
        let backend = Arc::new(ToolBackend::new(ToolConfig::default()).unwrap());
        register(
            &mut bridge,
            backend,
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
            std::sync::Arc::new(tokio::sync::OnceCell::new()),
        );
        let aic = aic_for(&org_root, "alice", &["chat-users"]);
        // The default stub sender returns None immediately, so
        // we pick `timeout_secs = 1` to keep the test fast. The
        // outer timeout fires; the handler returns
        // `{"timeout": true}`.
        let args = br#"{"question":"deploy now?","timeout_secs":1}"#.to_vec();
        let envelope = crate::dispatch::build_request("tool.ask_human", args, aic, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = crate::dispatch::decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(body) => {
                let s = String::from_utf8(body.into_vec()).unwrap();
                assert!(
                    s.contains("\"timeout\":true"),
                    "expected timeout reply (no operator channel wired); got {s}"
                );
            }
            ResponseResult::Err(e) => {
                // The handler must not surface UNKNOWN_METHOD —
                // that would mean the registration is missing.
                assert_ne!(
                    e.kind,
                    relix_core::types::error_kinds::UNKNOWN_METHOD,
                    "tool.ask_human surfaced UNKNOWN_METHOD — registration gap"
                );
                panic!(
                    "unexpected handler error: kind={}, cause={}",
                    e.kind, e.cause
                );
            }
            other => panic!("unexpected response variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_loopback() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://127.0.0.1/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_localhost() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://localhost/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_ipv6_loopback() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://[::1]/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_rfc1918() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        for u in &[
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
        ] {
            let r = backend.fetch(u, 1024).await;
            assert!(matches!(r, WebFetchOutcome::Rejected(_)), "{u} got {:?}", r);
        }
    }

    #[tokio::test]
    async fn rejects_link_local_metadata() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend
            .fetch("http://169.254.169.254/latest/meta-data/", 1024)
            .await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_file_scheme() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("file:///etc/passwd", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_ftp_scheme() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("ftp://example.com/foo", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn rejects_http_by_default() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("http://example.com/", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    #[tokio::test]
    async fn allows_http_when_opted_in_via_config() {
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();
        // Resolution should pass scheme check; what comes next (DNS or remote
        // server state) is not asserted here. The point is: we did NOT
        // reject for scheme.
        let r = backend.fetch("http://example.com/", 1024).await;
        if let WebFetchOutcome::Rejected(SsrfError::SchemeDenied { .. }) = r {
            panic!("expected http to pass scheme check when allow_http=true");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_url() {
        let backend = ToolBackend::new(ToolConfig::default()).unwrap();
        let r = backend.fetch("not a url", 1024).await;
        assert!(matches!(r, WebFetchOutcome::Rejected(_)), "got {:?}", r);
    }

    // ──────────────────── DNS pinning live tests ──────────────────────────
    //
    // Strategy: bring up a tiny axum HTTP server on a random loopback port,
    // then exercise `build_client` with a synthetic hostname that has no
    // real DNS. If pinning works, reqwest connects to the loopback server
    // (we get our test body back); if it didn't, reqwest's resolver would
    // fail with NXDOMAIN. The test proves the post-validation connect goes
    // to the pinned address, not whatever the resolver returns at request
    // time — i.e. defeats DNS rebinding between guard and connect.

    /// Spawn a one-shot axum server returning a fixed body. Returns the
    /// bound `SocketAddr`. Drops with the test scope.
    async fn spawn_loopback_server(body: &'static str) -> SocketAddr {
        use axum::{Router, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/", get(move || async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        addr
    }

    #[tokio::test]
    async fn pin_forces_connect_to_validated_ip_not_dns() {
        // Bring up loopback server.
        let addr = spawn_loopback_server("pin-works\n").await;

        // Build a client pinned to a hostname that almost certainly does
        // NOT resolve via the system resolver (`.invalid` is RFC 2606
        // reserved). If pinning didn't work, reqwest would fail with
        // NXDOMAIN. If it does, reqwest connects to 127.0.0.1:addr.port.
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let pin: &[SocketAddr] = &[addr];
        let client = build_client(&cfg, Some(("rebind.invalid", pin))).expect("client builds");
        let url = format!("http://rebind.invalid:{}/", addr.port());
        let resp = client.get(&url).send().await.expect("connect via pin");
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert_eq!(body, "pin-works\n");
    }

    #[tokio::test]
    async fn pin_to_one_ip_ignores_other_addresses_in_dns() {
        // This test simulates a rebinding-style trick: the URL hostname
        // *could* in principle resolve to both a loopback (forbidden) and
        // a public IP at request time. We pin to ONLY the validated
        // (public) IP and confirm reqwest connects there. We use a second
        // loopback server as the "validated" target to keep the test
        // hermetic, and supply an unrelated forbidden-looking address in
        // the URL host (which wouldn't normally resolve at all).
        let validated_addr = spawn_loopback_server("validated-host\n").await;
        let _decoy_addr = spawn_loopback_server("decoy-NEVER-SHOULD-SEE\n").await;

        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        // Pin maps "example.invalid" *only* to the validated socket. Even
        // if a later resolver returned the decoy or a true rebind IP,
        // reqwest will only use entries in this pin list.
        let pin: &[SocketAddr] = &[validated_addr];
        let client = build_client(&cfg, Some(("example.invalid", pin))).expect("client builds");
        let url = format!("http://example.invalid:{}/", validated_addr.port());
        let body = client
            .get(&url)
            .send()
            .await
            .expect("send")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "validated-host\n");
    }

    #[tokio::test]
    async fn redirect_to_loopback_literal_is_rejected_per_hop() {
        use axum::{Router, response::Redirect, routing::get};
        // First hop returns 302 -> http://127.0.0.1:9/  (the literal IP
        // is in a forbidden range; the closure's literal-IP check rejects
        // synchronously without needing DNS for the redirect target).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://127.0.0.1:9/loopback-target") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let cfg = ToolConfig {
            allow_http: true,
            max_redirects: 3,
            ..ToolConfig::default()
        };
        let client =
            build_client(&cfg, Some(("redirector.invalid", &[addr]))).expect("client builds");
        let url = format!("http://redirector.invalid:{}/", addr.port());
        let r = client.get(&url).send().await;
        assert!(
            r.is_err(),
            "redirect to loopback literal must be rejected, got: {:?}",
            r
        );
        let err_str = format!("{:?}", r.err().unwrap());
        // reqwest wraps our SsrfError; the chain should still mention the
        // SSRF reason somewhere. We assert a substring rather than an
        // enum variant to avoid coupling tests to reqwest's error wrapping.
        assert!(
            err_str.contains("loopback") || err_str.contains("forbidden"),
            "expected SSRF reason in redirect error, got: {err_str}"
        );
    }

    #[tokio::test]
    async fn redirect_to_rfc1918_literal_is_rejected_per_hop() {
        use axum::{Router, response::Redirect, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://10.0.0.1/private") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let cfg = ToolConfig {
            allow_http: true,
            max_redirects: 3,
            ..ToolConfig::default()
        };
        let client =
            build_client(&cfg, Some(("private-redirect.invalid", &[addr]))).expect("client builds");
        let url = format!("http://private-redirect.invalid:{}/", addr.port());
        let r = client.get(&url).send().await;
        assert!(
            r.is_err(),
            "redirect to rfc1918 literal must be rejected, got: {:?}",
            r
        );
    }

    #[tokio::test]
    async fn redirect_cap_zero_blocks_any_redirect() {
        use axum::{Router, response::Redirect, routing::get};
        // Server redirects to another safe-ish literal URL; the cap should
        // fire before the SSRF check runs on the target. We pick a public
        // literal IP for the redirect target so a "broken cap" wouldn't be
        // hidden by an SSRF rejection of a forbidden target.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(|| async { Redirect::temporary("http://1.1.1.1/somepath") }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });

        let cfg = ToolConfig {
            allow_http: true,
            max_redirects: 0,
            ..ToolConfig::default()
        };
        let client =
            build_client(&cfg, Some(("nofollow.invalid", &[addr]))).expect("client builds");
        let url = format!("http://nofollow.invalid:{}/", addr.port());
        let r = client.get(&url).send().await;
        assert!(
            r.is_err(),
            "max_redirects=0 must block any redirect, got: {:?}",
            r
        );
        let err_str = format!("{:?}", r.err().unwrap());
        assert!(
            err_str.contains("redirect cap"),
            "expected 'redirect cap' in error, got: {err_str}"
        );
    }

    // ─────────────────── Pinned client pool reuse tests ────────────────
    //
    // These exercise the pool directly: two backends, multiple fetches,
    // checking the (hits, misses, entries) tuple advertised by the
    // #[cfg(test)] `counters()` helper. The pool's hit/miss accounting
    // also drives the structured tracing line, so these tests double as
    // observability coverage.

    #[tokio::test]
    async fn pool_reuses_same_client_for_same_host_and_addrs() {
        let addr = spawn_loopback_server("hello-from-pool\n").await;
        let mut cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        // Use a hostname that resolves via the system resolver - we cannot
        // exercise resolve_safe_url for `.invalid`. Instead drive the pool
        // by-hand and assert reuse via `counters()`.
        cfg.user_agent = "Relix-tool/test-pool".into();
        let backend = ToolBackend::new(cfg.clone()).unwrap();

        // First call: miss.
        let c1 = backend
            .pool
            .pinned("pool.invalid", &[addr])
            .expect("client");
        let (h1, m1, n1) = backend.pool.counters();
        assert_eq!((m1, n1), (1, 1), "first call should miss");

        // Second call same (host, addrs): hit, no new entry.
        let c2 = backend
            .pool
            .pinned("pool.invalid", &[addr])
            .expect("client");
        let (h2, m2, n2) = backend.pool.counters();
        assert_eq!(h2, h1 + 1, "second call must hit");
        assert_eq!(m2, m1, "second call must not miss");
        assert_eq!(n2, n1, "pool size unchanged");
        assert!(Arc::ptr_eq(&c1, &c2), "must return the same Arc");
    }

    #[tokio::test]
    async fn pool_creates_new_client_when_addrs_change() {
        let addr_a = spawn_loopback_server("a\n").await;
        let addr_b = spawn_loopback_server("b\n").await;
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();

        let c1 = backend
            .pool
            .pinned("dns-changed.invalid", &[addr_a])
            .expect("c1");
        let c2 = backend
            .pool
            .pinned("dns-changed.invalid", &[addr_b])
            .expect("c2");
        assert!(
            !Arc::ptr_eq(&c1, &c2),
            "different validated addrs must yield different pooled clients"
        );
        let (_, misses, entries) = backend.pool.counters();
        assert_eq!(misses, 2);
        assert_eq!(entries, 2);
    }

    #[tokio::test]
    async fn pool_separates_clients_per_hostname() {
        let addr = spawn_loopback_server("c\n").await;
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();

        let c1 = backend
            .pool
            .pinned("host-one.invalid", &[addr])
            .expect("c1");
        let c2 = backend
            .pool
            .pinned("host-two.invalid", &[addr])
            .expect("c2");
        assert!(
            !Arc::ptr_eq(&c1, &c2),
            "different hostnames must NOT share a pooled client (pin is per-host)"
        );
    }

    #[tokio::test]
    async fn pool_addr_set_is_canonicalised_so_dns_reordering_still_hits() {
        let a = spawn_loopback_server("x\n").await;
        let b = spawn_loopback_server("y\n").await;
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();

        // Insert with [a, b]; lookup with [b, a] should hit the same entry
        // because pool keys sort the addrs into canonical order.
        let c1 = backend.pool.pinned("multi.invalid", &[a, b]).expect("c1");
        let c2 = backend.pool.pinned("multi.invalid", &[b, a]).expect("c2");
        assert!(
            Arc::ptr_eq(&c1, &c2),
            "DNS-reordered addrs must hit the canonicalised pool key"
        );
        let (_, misses, entries) = backend.pool.counters();
        assert_eq!(misses, 1);
        assert_eq!(entries, 1);
    }

    #[tokio::test]
    async fn pool_unpinned_client_is_a_single_shared_arc() {
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();
        let u1 = backend.pool.unpinned();
        let u2 = backend.pool.unpinned();
        assert!(
            Arc::ptr_eq(&u1, &u2),
            "ip-literal path must reuse a single Client"
        );
        // No pinned entries created by the unpinned fetches.
        let (_, _, entries) = backend.pool.counters();
        assert_eq!(entries, 0);
    }

    #[tokio::test]
    async fn pool_reuse_actually_carries_real_traffic() {
        // End-to-end via fetch() to prove reuse works for actual fetches,
        // not just direct pool calls. Two fetches to the same pinned host
        // should result in 2 hits + 1 miss.
        let addr = spawn_loopback_server("body-from-real-traffic\n").await;
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let backend = ToolBackend::new(cfg).unwrap();

        // We can't go through `fetch()` directly because resolve_safe_url
        // would try real DNS for `.invalid`. Instead drive the pool path
        // by hand: get a pinned client for our synthetic host, hit it
        // twice, and confirm the counters reflect reuse.
        let c1 = backend.pool.pinned("traffic.invalid", &[addr]).expect("c1");
        let r1 = c1
            .get(format!("http://traffic.invalid:{}/", addr.port()))
            .send()
            .await
            .expect("send 1")
            .text()
            .await
            .expect("body 1");
        assert_eq!(r1, "body-from-real-traffic\n");

        let c2 = backend.pool.pinned("traffic.invalid", &[addr]).expect("c2");
        let r2 = c2
            .get(format!("http://traffic.invalid:{}/", addr.port()))
            .send()
            .await
            .expect("send 2")
            .text()
            .await
            .expect("body 2");
        assert_eq!(r2, "body-from-real-traffic\n");

        let (hits, misses, entries) = backend.pool.counters();
        assert_eq!(misses, 1, "only the first lookup should miss");
        assert!(hits >= 1, "subsequent lookup(s) should hit");
        assert_eq!(entries, 1);
    }

    #[tokio::test]
    async fn unpinned_hostname_fails_dns_proving_pin_is_load_bearing() {
        // Sanity test: without a pin, the same `.invalid` hostname fails.
        // If this ever started succeeding it would mean either (a) the
        // test environment poisoned its DNS, or (b) reqwest grew an
        // implicit fallback — either way our pin assumption needs to be
        // re-examined.
        let cfg = ToolConfig {
            allow_http: true,
            ..ToolConfig::default()
        };
        let client = build_client(&cfg, None).expect("client builds");
        let url = "http://rebind-control.invalid:9/";
        let r = client.get(url).send().await;
        assert!(
            r.is_err(),
            "expected DNS failure without pin (got success — pin test is meaningless)"
        );
    }

    #[test]
    fn descriptor_is_external_paid_and_admission_tagged() {
        let d = capability_descriptor();
        assert_eq!(d.method_name, "tool.web_fetch");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.cost_class, CostClass::ExternalPaid));
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(d.requires_groups.iter().any(|g| g == "chat-users"));
    }

    #[test]
    fn content_type_filter() {
        assert!(is_textual_content_type("text/html"));
        assert!(is_textual_content_type("text/html; charset=utf-8"));
        assert!(is_textual_content_type("application/json"));
        assert!(is_textual_content_type("application/ld+json"));
        assert!(is_textual_content_type("application/atom+xml"));
        assert!(is_textual_content_type(""));
        assert!(!is_textual_content_type("application/octet-stream"));
        assert!(!is_textual_content_type("image/png"));
        assert!(!is_textual_content_type("application/pdf"));
    }

    /// PH-RISK-PIN-ALL: pin the risk tier of the two
    /// tool-node-level descriptors that live in mod.rs.
    /// terminal_descriptor (tool.terminal.run) is High —
    /// allowlisted shell execution. capability_descriptor
    /// (tool.web_fetch) is Medium — SSRF-gated network egress.
    #[test]
    fn mod_level_descriptors_have_explicit_non_unknown_risk() {
        let pinned: &[(&str, CapabilityDescriptor, RiskLevel)] = &[
            ("tool.terminal.run", terminal_descriptor(), RiskLevel::High),
            ("tool.web_fetch", capability_descriptor(), RiskLevel::Medium),
        ];
        for (name, d, expected) in pinned {
            assert_ne!(
                d.risk_level,
                RiskLevel::Unknown,
                "{name} defaulted to Unknown risk"
            );
            assert_eq!(
                d.risk_level, *expected,
                "{name} risk tier drifted (expected {expected:?})"
            );
        }
    }
}
