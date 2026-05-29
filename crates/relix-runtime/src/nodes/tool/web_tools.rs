//! `tool.web_get` + `tool.web_search` — CW3 web tools wave.
//!
//! Both capabilities are convenience compositions over capabilities the tool
//! node already exposes:
//!
//! - `tool.web_get` — one-shot fetch + HTML-extract. Reuses the
//!   [`ToolBackend::fetch`] machinery (so it inherits SSRF, DNS pin,
//!   per-hop redirect re-validation, body cap, content-type filter)
//!   then funnels the body through [`super::web_extract::extract`]
//!   without a second roundtrip.
//! - `tool.web_search` — DuckDuckGo HTML scrape (no API key, no
//!   third-party SDK). Drives the same `fetch()` against the public
//!   `html.duckduckgo.com` endpoint and parses results into stable
//!   tab-separated rows.
//!
//! No new network primitives, no new fetch surface. SSRF posture is
//! identical to `tool.web_fetch`.
//!
//! ## Wire format
//!
//! `tool.web_get`: arg is `<mode>|<url>` where `<mode>` is one of
//! `text|title|links|meta|all|raw`. `raw` returns the verbatim body
//! (same bytes you'd get from `tool.web_fetch`); the other modes
//! project the parsed [`Extracted`] view. URLs may contain `|`
//! since the split is `splitn(2, '|')`.
//!
//! Output for `text|title|links|meta|all` matches what `tool.web_extract`
//! produces in the corresponding mode. For `raw` the body is returned
//! as-is (subject to the tool node's `max_bytes` cap, like the
//! existing `tool.web_fetch`).
//!
//! `tool.web_search`: arg is `<query>` or `<query>|<max_results>`.
//! `max_results` defaults to 10 and is clamped to `[1, 20]`. The
//! result body is one row per hit:
//!
//! ```text
//! 1\t<url>\t<title>\t<snippet>
//! 2\t<url>\t<title>\t<snippet>
//! ...
//! result_count=<N>
//! ```
//!
//! Empty title / snippet are allowed; URL is always non-empty for a
//! row to be emitted. Snippets have tab characters collapsed to spaces
//! so the row format stays grep-friendly.
//!
//! ## What this does NOT do
//!
//! - **No crawl.** Per-host recursion / dedup / fairness is its own
//!   design and is deferred to a future milestone.
//! - **No POST.** Same posture as `tool.web_fetch`.
//! - **No JS.** Both capabilities use the same hand-rolled HTML state
//!   machine the existing `tool.web_extract` uses.
//! - **No alternate search engines today.** DuckDuckGo HTML was
//!   picked because it doesn't require API credentials and is
//!   directly scrape-friendly. Operators who need Google / Bing /
//!   Brave should reach for a future provider-backed capability
//!   instead.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::WebFetchOutcome;
use super::WebPostOutcome;
use super::web_extract::{Extracted, extract};
use super::{ToolBackend, is_textual_content_type};

// ─────────────────────────── Capability descriptors ─────────────────────────

/// Descriptor for `tool.web_get`. Same blast radius as `tool.web_fetch`
/// (still touches the network) plus the parse step is bounded by the
/// existing `[tool] extract_max_input_bytes` cap reused via `web_extract`.
pub fn web_get_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web_get");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "external:network".into(),
        "egress:http".into(),
        "parse:html".into(),
    ];
    d.policy_attachment_point = "tool.web_get".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Fetch a URL and project the parsed view (text/title/links/meta/all) \
         or the raw body in one capability call. Reuses tool.web_fetch's SSRF, \
         DNS pin, redirect re-validation, and content-type filter."
            .into(),
    );
    d.categories = vec!["fetch".into(), "parse".into(), "io".into()];
    d.environment_requirements = vec!["network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// Descriptor for `tool.web_search`. Egress is to a single known host
/// (`html.duckduckgo.com`) but the SSRF guards still run on every fetch;
/// the descriptor reflects the same blast radius as `tool.web_fetch` so
/// policy engines can treat it consistently.
pub fn web_search_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web_search");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "external:network".into(),
        "egress:http".into(),
        "search:web".into(),
    ];
    d.policy_attachment_point = "tool.web_search".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Web search via DuckDuckGo HTML scrape. No API key required. \
         Returns tab-separated <rank>\\t<url>\\t<title>\\t<snippet> rows."
            .into(),
    );
    d.categories = vec!["search".into(), "fetch".into()];
    d.environment_requirements = vec!["network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// PH-DASH-BLOCKLIST: descriptor for `tool.web.blocklist_summary`.
/// Pure-read of the operator-curated `[tool] blocked_hosts` set —
/// no I/O, no network, no DNS. Surfaced so the dashboard / CLI
/// can show operators what they've configured without going
/// through the config file directly. Risk Safe (the blocklist is
/// already operator-controlled; exposing its contents is no more
/// sensitive than reading the config itself, which the operator
/// already has access to).
pub fn web_blocklist_summary_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web.blocklist_summary");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["read:config".into()];
    d.policy_attachment_point = "tool.web.blocklist_summary".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Read-only snapshot of `[tool] blocked_hosts`. No args; \
         returns `count=N` then one host per line, lexicographically \
         sorted. Used by the dashboard PH-DASH-BLOCKLIST card and \
         `relix-cli web blocklist`. Pure config read — no I/O."
            .into(),
    );
    d.categories = vec!["observe".into(), "config".into()];
    d.environment_requirements = vec![];
    d.risk_level = RiskLevel::Safe;
    d
}

/// PH-WEB-POST: descriptor for `tool.web.post`. Same blast radius
/// as `tool.web_fetch` (network egress, SSRF gate, DNS pin) plus
/// the caller-supplied body is forwarded verbatim. Cookies are
/// passed as a raw header value — no jar / parsing on Relix's
/// side. Set-Cookie headers from the responder are returned
/// verbatim so SOL flows can stitch session tokens across calls.
pub fn web_post_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.web.post");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "external:network".into(),
        "egress:http".into(),
        "http:method:post".into(),
    ];
    d.policy_attachment_point = "tool.web.post".to_string();
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "HTTP POST against an external URL. Request JSON: \
         {url, body?, content_type?, cookie?, max_bytes?}. Returns JSON: \
         {body, final_url, content_type, set_cookies}. SSRF + DNS pin + \
         redirect re-validation identical to tool.web_fetch. Cookie field \
         is forwarded as a raw `Cookie:` header (no jar)."
            .into(),
    );
    d.categories = vec!["fetch".into(), "io".into(), "mutate".into()];
    d.environment_requirements = vec!["network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

// ─────────────────────────── Register ───────────────────────────

/// Wire both CW3 capabilities onto the dispatch bridge. Caller is the
/// tool-node `register()` in `mod.rs`.
pub fn register(bridge: &mut DispatchBridge, backend: Arc<ToolBackend>) {
    let b_get = backend.clone();
    bridge.register(
        "tool.web_get",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = b_get.clone();
            async move { handle_web_get(backend, ctx).await }
        })),
    );

    let b_search = backend.clone();
    bridge.register(
        "tool.web_search",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = b_search.clone();
            async move { handle_web_search(backend, ctx).await }
        })),
    );

    let b_post = backend.clone();
    bridge.register(
        "tool.web.post",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = b_post.clone();
            async move { handle_web_post(backend, ctx).await }
        })),
    );

    let b_bl = backend;
    bridge.register(
        "tool.web.blocklist_summary",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = b_bl.clone();
            async move { handle_blocklist_summary(backend, ctx) }
        })),
    );
}

// ─────────────────────────── tool.web.blocklist_summary ─────────────

/// PH-DASH-BLOCKLIST: handle `tool.web.blocklist_summary`. Arg is
/// ignored (caller may send empty or any bytes). Returns a body
/// of:
///
/// ```text
/// count=N
/// host-1
/// host-2
/// …
/// ```
///
/// Entries are sorted lexicographically (the HostBlocklist stores
/// them in a HashSet; sort here gives operators a stable order
/// regardless of insertion sequence).
fn handle_blocklist_summary(backend: Arc<ToolBackend>, _ctx: InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let bl = backend.blocklist();
    let hosts = bl.snapshot_sorted();
    let mut buf = String::new();
    let _ = writeln!(buf, "count={}", hosts.len());
    for h in hosts {
        let _ = writeln!(buf, "{h}");
    }
    HandlerOutcome::Ok(buf.into_bytes())
}

// ─────────────────────────── tool.web_get ───────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GetMode {
    Text,
    Title,
    Links,
    Meta,
    All,
    Raw,
}

impl GetMode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "title" => Some(Self::Title),
            "links" => Some(Self::Links),
            "meta" => Some(Self::Meta),
            "all" => Some(Self::All),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

// `handle_web_get_public` was the bridge used by `perception::register`
// to expose `tool.web_get`'s pipeline under the `tool.web_read` alias.
// GAP 10 PART 2 replaced that shim with the tiered pipeline in
// `parse_document::register_web_read`, which keeps the local fallback
// in-house. The shim was removed alongside the perception delegation.
async fn handle_web_get(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("tool.web_get arg utf8: {e}")),
    };
    let mut parts = raw.splitn(2, '|');
    let mode_str = parts.next().unwrap_or("").trim();
    let url = parts.next().unwrap_or("").trim();
    if mode_str.is_empty() {
        return invalid_args(
            "tool.web_get: mode required (text/title/links/meta/all/raw); arg: `<mode>|<url>`"
                .into(),
        );
    }
    let mode = match GetMode::parse(mode_str) {
        Some(m) => m,
        None => {
            return invalid_args(format!(
                "tool.web_get: unknown mode '{mode_str}' (text/title/links/meta/all/raw)"
            ));
        }
    };
    if url.is_empty() {
        return invalid_args("tool.web_get: url required after mode".into());
    }

    // Reuse the tool backend's full SSRF + pin + redirect + cap pipeline.
    // `max_bytes_request = usize::MAX` lets fetch() fall back to the
    // configured node default cap. Equivalent to calling `tool.web_fetch`
    // without the `|<n>` suffix.
    let outcome = backend.fetch(url, usize::MAX).await;
    let body = match map_fetch_for_get(outcome) {
        Ok(body) => body,
        Err(env) => return HandlerOutcome::Err(env),
    };

    let rendered = match mode {
        GetMode::Raw => body,
        _ => {
            let extracted = extract(&body);
            render_extracted(&extracted, mode)
        }
    };
    HandlerOutcome::Ok(rendered.into_bytes())
}

fn render_extracted(extracted: &Extracted, mode: GetMode) -> String {
    match mode {
        GetMode::Text => extracted.text.clone(),
        GetMode::Title => extracted.title.clone().unwrap_or_default(),
        GetMode::Links => extracted.links.join("\n"),
        GetMode::Meta => extracted
            .meta
            .iter()
            .map(|(k, v)| format!("{k}\t{v}"))
            .collect::<Vec<_>>()
            .join("\n"),
        GetMode::All => render_all(extracted),
        GetMode::Raw => unreachable!("raw is handled before render_extracted"),
    }
}

fn render_all(extracted: &Extracted) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "title={}", extracted.title.as_deref().unwrap_or(""));
    let _ = writeln!(out, "link_count={}", extracted.links.len());
    let _ = writeln!(out, "meta_count={}", extracted.meta.len());
    let _ = write!(out, "text={}", extracted.text);
    out
}

fn map_fetch_for_get(outcome: WebFetchOutcome) -> Result<String, ErrorEnvelope> {
    match outcome {
        WebFetchOutcome::Ok { body, .. } => Ok(body),
        WebFetchOutcome::Rejected(e) => Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!("tool.web_get ssrf-rejected: {e}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::TooLarge {
            declared_bytes,
            cap,
        } => Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_get body too large: declared={declared_bytes}B cap={cap}B"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::HttpStatus { status, final_url } => Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.web_get http {status} for {final_url}"),
            retry_hint: 1,
            retry_after: None,
        }),
        WebFetchOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.web_get content-type not text-like: '{content_type}' for {final_url}"
            ),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::NotUtf8 { final_url } => Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web_get body not utf-8 for {final_url}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebFetchOutcome::Transport(c) => Err(ErrorEnvelope {
            kind: error_kinds::TRANSPORT,
            cause: format!("tool.web_get transport: {c}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

// ─────────────────────────── tool.web_search ───────────────────────────

const SEARCH_DEFAULT_RESULTS: usize = 10;
const SEARCH_MAX_RESULTS: usize = 20;
const DDG_HTML_BASE: &str = "https://html.duckduckgo.com/html/";

async fn handle_web_search(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid_args(format!("tool.web_search arg utf8: {e}")),
    };
    let (query, requested) = match raw.rsplit_once('|') {
        Some((q, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (q.trim(), n_str.trim().parse::<usize>().unwrap_or(0))
        }
        _ => (raw.trim(), SEARCH_DEFAULT_RESULTS),
    };
    if query.is_empty() {
        return invalid_args(
            "tool.web_search: query required (arg: `<query>` or `<query>|<max_results>`)".into(),
        );
    }
    let max_results = requested.clamp(1, SEARCH_MAX_RESULTS);

    let url = format!("{}?q={}", DDG_HTML_BASE, percent_encode_query(query));

    // Search results can be sizable; bound the fetch at 512 KiB to give
    // the parser plenty of room without letting a hostile responder
    // feed us megabytes. Backend's own max_bytes still applies.
    let outcome = backend.fetch(&url, 512 * 1024).await;
    let body = match map_fetch_for_get(outcome) {
        Ok(b) => b,
        Err(mut env) => {
            // Rewrite the cause prefix so operators see which capability
            // emitted it.
            env.cause = env.cause.replacen("tool.web_get", "tool.web_search", 1);
            return HandlerOutcome::Err(env);
        }
    };

    let results = parse_ddg_results(&body, max_results);
    let rendered = render_search_results(&results);
    HandlerOutcome::Ok(rendered.into_bytes())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

/// Render search hits in the documented tab-separated row format with a
/// trailing `result_count=<N>` line.
pub fn render_search_results(hits: &[SearchHit]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}",
            i + 1,
            tab_safe(&h.url),
            tab_safe(&h.title),
            tab_safe(&h.snippet),
        );
    }
    let _ = writeln!(out, "result_count={}", hits.len());
    out
}

/// DuckDuckGo HTML result extractor. Walks the rendered page, captures
/// result anchors + their associated snippet body.
///
/// DDG's HTML result format is intentionally simple: each hit is a
/// `<a class="result__a" href="REAL_URL">TITLE</a>` followed by a
/// `<a class="result__snippet" ...>SNIPPET</a>` (or a `<div class=
/// "result__snippet">`). We don't try to be a real HTML5 parser — the
/// same `extract()` state machine the existing `tool.web_extract` uses
/// gives us a clean text projection, but to keep `<rank>\t<url>\t<title>
/// \t<snippet>` correlated we walk the raw bytes once.
pub fn parse_ddg_results(html: &str, max_results: usize) -> Vec<SearchHit> {
    let mut out: Vec<SearchHit> = Vec::new();
    let bytes = html.as_bytes();
    let mut cursor = 0;

    while out.len() < max_results {
        // Find the next result anchor. Two CSS classes have shipped on
        // the HTML endpoint historically; accept both.
        let result_a_idx = find_class_anchor(&bytes[cursor..], b"result__a");
        let next = match result_a_idx {
            Some(rel) => cursor + rel,
            None => break,
        };

        // Anchor opening tag spans from `<a ...>`. Read the href value.
        let tag_end = match memchr_byte_from(bytes, next, b'>') {
            Some(i) => i,
            None => break,
        };
        let tag_inner = &bytes[next..tag_end];
        let href = read_href_value(tag_inner).unwrap_or_default();
        let title_start = tag_end + 1;
        // Title runs until `</a>`.
        let title_close_rel =
            find_subslice_ci(&bytes[title_start..], b"</a>").unwrap_or(bytes.len() - title_start);
        let raw_title = &bytes[title_start..title_start + title_close_rel];
        let title = clean_html_text(raw_title);
        cursor = title_start
            + title_close_rel
            + b"</a>"
                .len()
                .min(bytes.len() - (title_start + title_close_rel));

        // Snippet — search forward, but stop if we run into the next
        // result anchor. Honor either `result__snippet` (an anchor or
        // a div).
        let next_result_rel = find_class_anchor(&bytes[cursor..], b"result__a");
        let scan_end = next_result_rel
            .map(|rel| cursor + rel)
            .unwrap_or(bytes.len());
        let snippet_open_rel = find_class_in_window(&bytes[cursor..scan_end], b"result__snippet");
        let snippet = if let Some(open_rel) = snippet_open_rel {
            let open_abs = cursor + open_rel;
            let open_tag_end = memchr_byte_from(bytes, open_abs, b'>').unwrap_or(open_abs);
            let snip_start = open_tag_end + 1;
            // Snippets are inside either `<a>...</a>` or `<div>...</div>`.
            // Try both close tags and pick whichever closes first.
            let close_a = find_subslice_ci(&bytes[snip_start..scan_end], b"</a>");
            let close_d = find_subslice_ci(&bytes[snip_start..scan_end], b"</div>");
            let close_rel = match (close_a, close_d) {
                (Some(a), Some(d)) => Some(a.min(d)),
                (Some(a), None) => Some(a),
                (None, Some(d)) => Some(d),
                (None, None) => None,
            };
            match close_rel {
                Some(rel) => clean_html_text(&bytes[snip_start..snip_start + rel]),
                None => String::new(),
            }
        } else {
            String::new()
        };

        let decoded_href = decode_ddg_href(&href);
        if !decoded_href.is_empty() {
            out.push(SearchHit {
                url: decoded_href,
                title,
                snippet,
            });
        }

        // Advance cursor past the snippet area (or at least past this
        // result anchor) so the next iteration finds a *different* hit.
        if let Some(open_rel) = snippet_open_rel {
            cursor += open_rel + 1;
        } else if cursor + 1 < bytes.len() {
            cursor += 1;
        } else {
            break;
        }
    }
    out
}

/// DuckDuckGo wraps result URLs in `//duckduckgo.com/l/?uddg=<encoded>&...`
/// for click tracking. Unwrap the `uddg` parameter when present so the
/// row contains the real target. Falls back to the raw href.
fn decode_ddg_href(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let needle = "uddg=";
    if let Some(start) = trimmed.find(needle) {
        let tail = &trimmed[start + needle.len()..];
        let end = tail.find('&').unwrap_or(tail.len());
        let encoded = &tail[..end];
        return percent_decode(encoded);
    }
    // Normalize protocol-relative `//host/...` to `https://host/...`.
    if let Some(rest) = trimmed.strip_prefix("//") {
        return format!("https://{rest}");
    }
    trimmed.to_string()
}

// ─────────────────────────── tool.web.post (PH-WEB-POST) ─────────

#[derive(Debug, Deserialize)]
struct WebPostRequest {
    url: String,
    #[serde(default)]
    body: String,
    /// `Content-Type` header. Default `application/json`. Empty
    /// string is treated as "no Content-Type header" — useful
    /// when the server infers from body shape.
    #[serde(default = "default_post_content_type")]
    content_type: String,
    /// Raw `Cookie:` header value (e.g. `"sid=abc; user=bob"`).
    /// No jar / parsing on Relix's side; operators thread it
    /// through manually.
    #[serde(default)]
    cookie: String,
    /// Per-call body cap. Defaults to the tool node's
    /// `[tool] max_bytes`. Clamped at the responder side.
    #[serde(default)]
    max_bytes: Option<usize>,
}

fn default_post_content_type() -> String {
    "application/json".to_string()
}

#[derive(Debug, Serialize)]
struct WebPostResponseBody {
    body: String,
    final_url: String,
    content_type: String,
    /// Set-Cookie headers returned by the responder, verbatim.
    /// Empty when none were sent.
    set_cookies: Vec<String>,
}

async fn handle_web_post(backend: Arc<ToolBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let req: WebPostRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.web.post: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if req.url.trim().is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.web.post: url required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let max_bytes = req.max_bytes.unwrap_or(backend.max_bytes());
    let outcome = backend
        .post(
            &req.url,
            &req.body,
            &req.content_type,
            &req.cookie,
            max_bytes,
        )
        .await;
    match outcome {
        WebPostOutcome::Ok {
            body,
            final_url,
            content_type,
            set_cookies,
        } => {
            let resp = WebPostResponseBody {
                body,
                final_url,
                content_type,
                set_cookies,
            };
            HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
        }
        WebPostOutcome::Rejected(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!("tool.web.post: rejected: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
        WebPostOutcome::TooLarge {
            declared_bytes,
            cap,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web.post: response {declared_bytes} bytes exceeds cap {cap}"),
            retry_hint: 0,
            retry_after: None,
        }),
        WebPostOutcome::HttpStatus {
            status,
            final_url,
            set_cookies: _,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.web.post: HTTP {status} from {final_url}"),
            retry_hint: 2,
            retry_after: None,
        }),
        WebPostOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.web.post: non-text content-type '{content_type}' from {final_url}"
            ),
            retry_hint: 0,
            retry_after: None,
        }),
        WebPostOutcome::NotUtf8 {
            final_url,
            set_cookies: _,
        } => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("tool.web.post: response body from {final_url} is not UTF-8"),
            retry_hint: 0,
            retry_after: None,
        }),
        WebPostOutcome::Transport(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.web.post: transport: {e}"),
            retry_hint: 2,
            retry_after: None,
        }),
    }
}

// ─────────────────────────── HTML helpers (local) ───────────────────────────

/// Find the byte offset of an `<a ...>` whose attributes contain
/// `class="...<needle>..."`. We don't model attribute parsing — we
/// search for `class="` and then look for the needle within the
/// attribute value, which is enough for DDG's stable markup.
fn find_class_anchor(hay: &[u8], needle: &[u8]) -> Option<usize> {
    find_class_in_window(hay, needle).and_then(|cls_idx| {
        // Walk backward to the nearest `<a` so the caller can read its
        // attribute set as a whole. Includes position 0 (the anchor may
        // be the very first byte of the search window).
        let mut i = cls_idx;
        loop {
            if hay[i] == b'<' && i + 2 <= hay.len() {
                let after = &hay[i + 1..];
                let is_a = !after.is_empty()
                    && (after[0] == b'a' || after[0] == b'A')
                    && after.get(1).is_some_and(|c| !c.is_ascii_alphanumeric());
                if is_a {
                    return Some(i);
                }
            }
            if i == 0 {
                return None;
            }
            i -= 1;
        }
    })
}

fn find_class_in_window(hay: &[u8], needle: &[u8]) -> Option<usize> {
    // Look for `class="` then a window containing `needle`.
    let class_attr = b"class=\"";
    let mut search_from = 0;
    while let Some(rel) = find_subslice(&hay[search_from..], class_attr) {
        let value_start = search_from + rel + class_attr.len();
        let value_end = memchr_byte_from(hay, value_start, b'"').unwrap_or(hay.len());
        if find_subslice(&hay[value_start..value_end], needle).is_some() {
            return Some(value_start);
        }
        search_from = value_end + 1;
        if search_from >= hay.len() {
            break;
        }
    }
    None
}

fn read_href_value(tag_inner: &[u8]) -> Option<String> {
    let needle = b"href=\"";
    let rel = find_subslice(tag_inner, needle)?;
    let start = rel + needle.len();
    let end = memchr_byte_from(tag_inner, start, b'"').unwrap_or(tag_inner.len());
    Some(
        std::str::from_utf8(&tag_inner[start..end])
            .ok()?
            .to_string(),
    )
}

fn memchr_byte_from(hay: &[u8], from: usize, byte: u8) -> Option<usize> {
    hay.iter()
        .enumerate()
        .skip(from)
        .find(|&(_, &b)| b == byte)
        .map(|(i, _)| i)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn find_subslice_ci(hay: &[u8], needle_lower: &[u8]) -> Option<usize> {
    if needle_lower.is_empty() || hay.len() < needle_lower.len() {
        return None;
    }
    'outer: for i in 0..=hay.len() - needle_lower.len() {
        for (j, &want) in needle_lower.iter().enumerate() {
            let got = hay[i + j].to_ascii_lowercase();
            if got != want {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// Strip tags + decode a small set of HTML entities + collapse whitespace.
fn clean_html_text(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    let mut last_was_space = true;
    while i < raw.len() {
        let b = raw[i];
        if b == b'<' {
            if let Some(end_rel) = memchr_byte_from(raw, i, b'>') {
                i = end_rel + 1;
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
                continue;
            } else {
                break;
            }
        }
        if b == b'&'
            && let Some(end_rel) = memchr_byte_from(raw, i, b';')
        {
            let entity = &raw[i + 1..end_rel];
            if let Some(replacement) = decode_entity(entity) {
                for ch in replacement.chars() {
                    push_collapsed(&mut out, ch, &mut last_was_space);
                }
                i = end_rel + 1;
                continue;
            }
        }
        if b.is_ascii_whitespace() {
            push_collapsed(&mut out, ' ', &mut last_was_space);
            i += 1;
            continue;
        }
        // UTF-8 char fast path.
        let ch_len = utf8_char_len(b);
        let end = (i + ch_len).min(raw.len());
        let slice = &raw[i..end];
        if let Ok(s) = std::str::from_utf8(slice) {
            for ch in s.chars() {
                push_collapsed(&mut out, ch, &mut last_was_space);
            }
        }
        i = end;
    }
    out.trim().to_string()
}

fn push_collapsed(out: &mut String, ch: char, last_was_space: &mut bool) {
    if ch.is_whitespace() {
        if !*last_was_space {
            out.push(' ');
            *last_was_space = true;
        }
    } else {
        out.push(ch);
        *last_was_space = false;
    }
}

fn utf8_char_len(first: u8) -> usize {
    match first {
        0..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn decode_entity(entity: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(entity).ok()?;
    let named = match s {
        "amp" => Some("&"),
        "lt" => Some("<"),
        "gt" => Some(">"),
        "quot" => Some("\""),
        "apos" => Some("'"),
        "nbsp" => Some(" "),
        "copy" => Some("©"),
        "reg" => Some("®"),
        "trade" => Some("™"),
        "hellip" => Some("…"),
        "mdash" => Some("—"),
        "ndash" => Some("–"),
        "middot" => Some("·"),
        _ => None,
    };
    if let Some(n) = named {
        return Some(n.to_string());
    }
    if let Some(rest) = s.strip_prefix('#') {
        let code: u32 = if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X'))
        {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            rest.parse().ok()?
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    None
}

/// Replace tabs/newlines in a row cell so the tab-separated output stays
/// machine-parseable. Empty inputs are passed through unchanged.
fn tab_safe(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Percent-encode a query parameter using the conservative
/// unreserved set [A-Za-z0-9 - _ . ~]; everything else becomes `%HH`.
/// Space becomes `+` (matches `application/x-www-form-urlencoded`).
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Decode the small URL-encoded slice DDG uses inside the `uddg=` param.
/// Best-effort: invalid `%` sequences pass through unchanged.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        if b == b'%' && i + 2 < bytes.len() {
            let h1 = (bytes[i + 1] as char).to_digit(16);
            let h2 = (bytes[i + 2] as char).to_digit(16);
            if let (Some(a), Some(b)) = (h1, h2) {
                out.push(((a as u8) << 4) | b as u8);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn invalid_args(msg: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg,
        retry_hint: 2,
        retry_after: None,
    })
}

// Suppress dead_code on `is_textual_content_type` re-export — referenced
// for symmetry with the documented invariants even though `web_get` lets
// the backend enforce the filter.
#[allow(dead_code)]
fn _content_type_filter_reference() {
    let _ = is_textual_content_type;
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_get_descriptor_shape() {
        let d = web_get_descriptor();
        assert_eq!(d.method_name, "tool.web_get");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(matches!(d.cost_class, CostClass::ExternalPaid));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "parse:html"));
    }

    #[test]
    fn web_search_descriptor_shape() {
        let d = web_search_descriptor();
        assert_eq!(d.method_name, "tool.web_search");
        assert!(d.sensitivity_tags.iter().any(|t| t == "search:web"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
    }

    #[test]
    fn get_mode_parse() {
        assert_eq!(GetMode::parse("text"), Some(GetMode::Text));
        assert_eq!(GetMode::parse("title"), Some(GetMode::Title));
        assert_eq!(GetMode::parse("links"), Some(GetMode::Links));
        assert_eq!(GetMode::parse("meta"), Some(GetMode::Meta));
        assert_eq!(GetMode::parse("all"), Some(GetMode::All));
        assert_eq!(GetMode::parse("raw"), Some(GetMode::Raw));
        assert!(GetMode::parse("bogus").is_none());
    }

    #[test]
    fn percent_encode_query_basic() {
        assert_eq!(percent_encode_query("hello world"), "hello+world");
        assert_eq!(percent_encode_query("a=b&c"), "a%3Db%26c");
        assert_eq!(percent_encode_query("UTF-8 ☃"), "UTF-8+%E2%98%83");
        assert_eq!(percent_encode_query(""), "");
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello+world"), "hello world");
        assert_eq!(percent_decode("a%3Db%26c"), "a=b&c");
        assert_eq!(percent_decode("%E2%98%83"), "☃");
        assert_eq!(percent_decode("plain"), "plain");
        // Invalid sequence passes through unchanged.
        assert_eq!(percent_decode("%G0"), "%G0");
    }

    #[test]
    fn ddg_href_unwrap() {
        let wrapped = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpath%3Fa%3D1&rut=foo";
        assert_eq!(decode_ddg_href(wrapped), "https://example.com/path?a=1");
        // Already-real href: keep as-is (with https:// prepended for
        // protocol-relative).
        assert_eq!(decode_ddg_href("//example.com/x"), "https://example.com/x");
        assert_eq!(
            decode_ddg_href("https://example.com/y"),
            "https://example.com/y"
        );
        assert_eq!(decode_ddg_href(""), "");
    }

    #[test]
    fn parse_ddg_results_basic() {
        // Minimal synthetic DDG-style markup; verifies the parser
        // walks anchor → snippet pairs without depending on the real
        // DDG payload.
        let html = r#"
<div>
<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2F1">Title A</a>
<a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2F1">Snippet A goes here</a>
<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fb.example%2F2">Title B</a>
<div class="result__snippet">Snippet B body</div>
</div>"#;
        let hits = parse_ddg_results(html, 5);
        assert_eq!(hits.len(), 2, "got {hits:#?}");
        assert_eq!(hits[0].url, "https://a.example/1");
        assert_eq!(hits[0].title, "Title A");
        assert_eq!(hits[0].snippet, "Snippet A goes here");
        assert_eq!(hits[1].url, "https://b.example/2");
        assert_eq!(hits[1].title, "Title B");
        assert_eq!(hits[1].snippet, "Snippet B body");
    }

    #[test]
    fn parse_ddg_results_respects_max() {
        let mut html = String::from("<div>");
        for i in 0..5 {
            html.push_str(&format!(
                "<a class=\"result__a\" href=\"https://x.example/{i}\">T{i}</a>\
                 <div class=\"result__snippet\">S{i}</div>"
            ));
        }
        html.push_str("</div>");
        let hits = parse_ddg_results(&html, 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].title, "T0");
        assert_eq!(hits[2].title, "T2");
    }

    #[test]
    fn parse_ddg_results_handles_entities() {
        let html = r#"
<a class="result__a" href="https://a.example/q">AT&amp;T &mdash; case</a>
<div class="result__snippet">snip &lt;b&gt;text&lt;/b&gt; here</div>"#;
        let hits = parse_ddg_results(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "AT&T — case");
        assert_eq!(hits[0].snippet, "snip <b>text</b> here");
    }

    #[test]
    fn parse_ddg_results_skips_anchors_without_href() {
        let html = r#"
<a class="result__a">no href here</a>
<a class="result__a" href="https://ok.example/2">good</a>
<div class="result__snippet">snip</div>"#;
        let hits = parse_ddg_results(html, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://ok.example/2");
    }

    #[test]
    fn render_search_results_format() {
        let hits = vec![
            SearchHit {
                url: "https://a.example/x".into(),
                title: "Title One".into(),
                snippet: "first snippet".into(),
            },
            SearchHit {
                url: "https://b.example/y".into(),
                title: "Title Two".into(),
                snippet: "second\tsnippet".into(), // embedded tab must be sanitised
            },
        ];
        let body = render_search_results(&hits);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3); // 2 rows + trailer
        assert_eq!(lines[0], "1\thttps://a.example/x\tTitle One\tfirst snippet");
        assert_eq!(
            lines[1],
            "2\thttps://b.example/y\tTitle Two\tsecond snippet"
        );
        assert_eq!(lines[2], "result_count=2");
    }

    #[test]
    fn render_all_mode_format() {
        let extracted = Extracted {
            title: Some("My Page".into()),
            text: "Hello body.".into(),
            links: vec!["https://a/".into(), "https://b/".into()],
            meta: vec![("description".into(), "A page.".into())],
        };
        let s = render_extracted(&extracted, GetMode::All);
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines[0], "title=My Page");
        assert_eq!(lines[1], "link_count=2");
        assert_eq!(lines[2], "meta_count=1");
        assert_eq!(lines[3], "text=Hello body.");
    }

    #[test]
    fn render_links_and_meta_modes() {
        let extracted = Extracted {
            title: None,
            text: String::new(),
            links: vec!["https://a/".into(), "https://b/".into()],
            meta: vec![
                ("description".into(), "A page".into()),
                ("og:title".into(), "Hi".into()),
            ],
        };
        assert_eq!(
            render_extracted(&extracted, GetMode::Links),
            "https://a/\nhttps://b/"
        );
        assert_eq!(
            render_extracted(&extracted, GetMode::Meta),
            "description\tA page\nog:title\tHi"
        );
    }

    #[test]
    fn clean_html_text_collapses_and_strips() {
        let raw = b"  hello   <b>world</b>\n\nfoo &amp; bar  ";
        assert_eq!(clean_html_text(raw), "hello world foo & bar");
    }

    #[test]
    fn tab_safe_replaces_separators() {
        assert_eq!(tab_safe("a\tb\nc\rd"), "a b c d");
        assert_eq!(tab_safe("clean"), "clean");
    }

    #[test]
    fn ddg_search_query_url_shape() {
        // Exercise the URL we'd construct, without making a real fetch.
        let q = "rust async tutorials";
        let url = format!("{}?q={}", DDG_HTML_BASE, percent_encode_query(q));
        assert_eq!(
            url,
            "https://html.duckduckgo.com/html/?q=rust+async+tutorials"
        );
    }

    // ── PH-WEB-POST: descriptor + request shape ────────────────────

    #[test]
    fn web_post_descriptor_shape() {
        let d = web_post_descriptor();
        assert_eq!(d.method_name, "tool.web.post");
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(matches!(d.cost_class, CostClass::ExternalPaid));
        assert!(d.sensitivity_tags.iter().any(|t| t == "http:method:post"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(matches!(d.risk_level, RiskLevel::Medium));
    }

    /// PH-WEB-POST-RISK-CROSS: pin the risk tier of every shipped
    /// web-tools descriptor as a sharp equality assertion (not
    /// `matches!`). Catches drift if a future commit accidentally
    /// regrades any of them — the descriptors are operator-facing
    /// and the tier affects `--risk` filter behavior + the
    /// validator's audit posture. None of them should ever be
    /// Unknown (that would surface as a deployment warning).
    #[test]
    fn web_tools_descriptors_have_explicit_non_unknown_risk() {
        for d in [
            web_get_descriptor(),
            web_search_descriptor(),
            web_post_descriptor(),
        ] {
            assert_ne!(
                d.risk_level,
                RiskLevel::Unknown,
                "{} unexpectedly defaulted to Unknown risk",
                d.method_name
            );
            // Every web-tools capability touches the network →
            // every one is Medium tier (controlled side effect
            // outside the responder, gated by SSRF + DNS pin).
            assert_eq!(
                d.risk_level,
                RiskLevel::Medium,
                "{} should be Medium tier (network side-effect under SSRF gate)",
                d.method_name
            );
        }
    }

    #[test]
    fn web_post_request_minimal_decodes() {
        // Only `url` is required; every other field has a default.
        let arg = br#"{"url":"https://example.com/api"}"#;
        let req: WebPostRequest = serde_json::from_slice(arg).unwrap();
        assert_eq!(req.url, "https://example.com/api");
        assert_eq!(req.body, "");
        assert_eq!(req.content_type, "application/json");
        assert_eq!(req.cookie, "");
        assert!(req.max_bytes.is_none());
    }

    #[test]
    fn web_post_request_full_decodes() {
        let arg = br#"{
            "url":"https://example.com/api",
            "body":"{\"q\":\"rust\"}",
            "content_type":"text/plain",
            "cookie":"sid=abc; user=bob",
            "max_bytes":4096
        }"#;
        let req: WebPostRequest = serde_json::from_slice(arg).unwrap();
        assert_eq!(req.body, r#"{"q":"rust"}"#);
        assert_eq!(req.content_type, "text/plain");
        assert_eq!(req.cookie, "sid=abc; user=bob");
        assert_eq!(req.max_bytes, Some(4096));
    }

    #[test]
    fn web_post_request_content_type_default_is_application_json() {
        // Even when explicitly omitted, the default kicks in.
        let arg = br#"{"url":"https://example.com/api","body":"x"}"#;
        let req: WebPostRequest = serde_json::from_slice(arg).unwrap();
        assert_eq!(req.content_type, "application/json");
    }

    #[test]
    fn web_post_response_serializes_set_cookies_array() {
        let r = WebPostResponseBody {
            body: "ok".into(),
            final_url: "https://example.com/api".into(),
            content_type: "application/json".into(),
            set_cookies: vec!["sid=abc; Path=/".into(), "csrf=xyz".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""set_cookies":["sid=abc; Path=/","csrf=xyz"]"#));
        assert!(s.contains(r#""final_url":"https://example.com/api""#));
    }

    // ── PH-DASH-BLOCKLIST: tool.web.blocklist_summary ────────────────

    /// Build a `ToolConfig` with the given blocked hosts. Avoids
    /// pulling in `default_max_bytes` etc. — uses `Default` and
    /// then overrides the blocklist.
    fn cfg_with_blocked(hosts: &[&str]) -> super::super::ToolConfig {
        super::super::ToolConfig {
            blocked_hosts: hosts.iter().map(|s| (*s).to_string()).collect(),
            ..super::super::ToolConfig::default()
        }
    }

    fn bl_ctx(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    fn unwrap_ok_body(out: HandlerOutcome) -> String {
        if let HandlerOutcome::Ok(body) = out {
            String::from_utf8(body).unwrap()
        } else {
            // `HandlerOutcome` does not impl Debug, so we can't
            // include the value in the panic. If this fires the
            // outcome was Err / StreamHandle — neither is expected
            // for this read-only capability under any input.
            panic!("expected HandlerOutcome::Ok, got Err or StreamHandle");
        }
    }

    #[test]
    fn blocklist_summary_descriptor_shape() {
        let d = web_blocklist_summary_descriptor();
        assert_eq!(d.method_name, "tool.web.blocklist_summary");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(matches!(d.risk_level, RiskLevel::Safe));
        // Read-config sensitivity tag; no network egress tag.
        assert!(d.sensitivity_tags.iter().any(|t| t == "read:config"));
        assert!(!d.sensitivity_tags.iter().any(|t| t == "egress:http"));
    }

    #[test]
    fn blocklist_summary_handler_renders_empty_ring() {
        let backend = Arc::new(super::super::ToolBackend::new(cfg_with_blocked(&[])).unwrap());
        let s = unwrap_ok_body(handle_blocklist_summary(backend, bl_ctx(b"")));
        assert_eq!(s, "count=0\n");
    }

    #[test]
    fn blocklist_summary_handler_renders_sorted_entries() {
        let backend = Arc::new(
            super::super::ToolBackend::new(cfg_with_blocked(&[
                "zebra.example.com",
                "alpha.example.com",
                "MIDDLE.example.com",
            ]))
            .unwrap(),
        );
        let body = unwrap_ok_body(handle_blocklist_summary(backend, bl_ctx(b"")));
        // Sorted lexicographically, lowercase-normalized.
        let expected = "count=3\nalpha.example.com\nmiddle.example.com\nzebra.example.com\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn blocklist_summary_ignores_args() {
        // The handler intentionally accepts and ignores arg bytes —
        // callers shouldn't need to send anything but operators
        // running `relix-cli capability invoke ... '<anything>'`
        // shouldn't see a 400 either.
        let backend =
            Arc::new(super::super::ToolBackend::new(cfg_with_blocked(&["only.host"])).unwrap());
        let s = unwrap_ok_body(handle_blocklist_summary(
            backend,
            bl_ctx(b"some unrelated garbage"),
        ));
        assert!(s.starts_with("count=1\n"));
        assert!(s.contains("only.host"));
    }

    #[test]
    fn web_post_response_empty_set_cookies_renders_as_empty_array() {
        let r = WebPostResponseBody {
            body: "".into(),
            final_url: "https://example.com/".into(),
            content_type: "text/plain".into(),
            set_cookies: vec![],
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""set_cookies":[]"#));
    }
}
