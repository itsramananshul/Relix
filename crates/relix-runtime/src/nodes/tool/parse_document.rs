//! `tool.parse_document` — tiered document parsing pipeline.
//!
//! Three tiers, evaluated in order with automatic fallthrough:
//!
//! 1. **Cloud (LlamaParse)** — best quality, paid API, handles scanned
//!    PDFs and complex layouts. Routed for PDF inputs.
//! 2. **Cloud (Jina Reader → Firecrawl)** — good quality for web content
//!    and standard PDFs, paid API. Routed for URL inputs.
//! 3. **Local** — always available. lopdf for PDFs, base64-UTF-8 decode
//!    for text/markdown/code, ToolBackend `fetch + extract` for URLs.
//!
//! A tier that returns an error or is unconfigured (env var unset) is
//! skipped silently. Plain text inputs always use the local tier — no
//! point burning cloud budget on a base64-encoded paragraph.
//!
//! ## Wire format (JSON)
//!
//! Input is a JSON object:
//!
//! ```json
//! {
//!     "kind": "text" | "markdown" | "code" | "pdf" | "url",
//!     "payload": "<base64-bytes or raw URL>",
//!     "source": "document.pdf"  // optional human label
//! }
//! ```
//!
//! Output is a JSON object:
//!
//! ```json
//! {
//!     "text": "...",
//!     "chunks_created": 0,
//!     "tier_used": "llama_parse" | "jina_reader" | "firecrawl" | "local",
//!     "source": "document.pdf"
//! }
//! ```
//!
//! `tier_used` is present on every success response so callers can tell
//! which tier handled the request. `chunks_created` is always 0 in this
//! pipeline — chunking lives in `tool.text.chunk`; the field stays for
//! schema compatibility with the §7.23 spec.
//!
//! ## Configuration `[tool.parse_document]`
//!
//! ```toml
//! [tool.parse_document]
//! enabled                 = true
//! prefer_cloud            = true
//! llama_cloud_api_key_env = "LLAMA_CLOUD_API_KEY"
//! jina_api_key_env        = "JINA_API_KEY"
//! firecrawl_api_key_env   = "FIRECRAWL_API_KEY"
//! cloud_timeout_secs      = 60
//! ```
//!
//! Setting `prefer_cloud = false` skips every cloud tier — the pipeline
//! goes straight to local. Operators opt out of cloud cost without
//! touching the env vars.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use reqwest::multipart;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::ToolBackend;
use super::pdf::PdfConfig;

const DEFAULT_MAX_OUTPUT_CHARS: usize = 200_000;
pub(crate) const LLAMA_CLOUD_ENV: &str = "LLAMA_CLOUD_API_KEY";
pub(crate) const JINA_ENV: &str = "JINA_API_KEY";
pub(crate) const FIRECRAWL_ENV: &str = "FIRECRAWL_API_KEY";

/// `[tool.parse_document]` config block.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ParseDocumentConfig {
    /// Master switch. `false` short-circuits the cap to a clear
    /// disabled error so operators know the surface is intentionally
    /// off.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// When `false`, skip every cloud tier and go straight to local.
    /// Operators opt out of cloud cost without touching env vars.
    #[serde(default = "default_prefer_cloud")]
    pub prefer_cloud: bool,
    /// Env var holding the LlamaParse API key. Set unset to "" to
    /// disable LlamaParse without changing the var name.
    #[serde(default = "default_llama_cloud_env")]
    pub llama_cloud_api_key_env: String,
    /// Env var holding the Jina Reader API key.
    #[serde(default = "default_jina_env")]
    pub jina_api_key_env: String,
    /// Env var holding the Firecrawl API key.
    #[serde(default = "default_firecrawl_env")]
    pub firecrawl_api_key_env: String,
    /// Per-request total deadline for any cloud call, in seconds.
    /// Default 60s — LlamaParse may poll several times.
    #[serde(default = "default_cloud_timeout_secs")]
    pub cloud_timeout_secs: u64,
}

impl Default for ParseDocumentConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            prefer_cloud: default_prefer_cloud(),
            llama_cloud_api_key_env: default_llama_cloud_env(),
            jina_api_key_env: default_jina_env(),
            firecrawl_api_key_env: default_firecrawl_env(),
            cloud_timeout_secs: default_cloud_timeout_secs(),
        }
    }
}

fn default_enabled() -> bool {
    true
}
fn default_prefer_cloud() -> bool {
    true
}
fn default_llama_cloud_env() -> String {
    LLAMA_CLOUD_ENV.to_string()
}
fn default_jina_env() -> String {
    JINA_ENV.to_string()
}
fn default_firecrawl_env() -> String {
    FIRECRAWL_ENV.to_string()
}
fn default_cloud_timeout_secs() -> u64 {
    60
}

/// `[tool.web_read]` config block. Shares the cloud-tier env vars
/// with `[tool.parse_document]` so an operator who pasted one key
/// gets both surfaces upgraded automatically.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WebReadConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_prefer_cloud")]
    pub prefer_cloud: bool,
    #[serde(default = "default_jina_env")]
    pub jina_api_key_env: String,
    #[serde(default = "default_firecrawl_env")]
    pub firecrawl_api_key_env: String,
    #[serde(default = "default_web_read_timeout_secs")]
    pub cloud_timeout_secs: u64,
}

impl Default for WebReadConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            prefer_cloud: default_prefer_cloud(),
            jina_api_key_env: default_jina_env(),
            firecrawl_api_key_env: default_firecrawl_env(),
            cloud_timeout_secs: default_web_read_timeout_secs(),
        }
    }
}

fn default_web_read_timeout_secs() -> u64 {
    30
}

/// Pure-data bundle of resolved cloud API keys. Constructed once at
/// startup from env (via [`ApiKeys::from_env`]) and threaded into the
/// handlers so unit tests don't touch process env (the crate forbids
/// unsafe code, which rules out `std::env::set_var` from tests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiKeys {
    pub llama_cloud: Option<String>,
    pub jina: Option<String>,
    pub firecrawl: Option<String>,
}

impl ApiKeys {
    /// Read every configured env var and stash the resulting values
    /// (empty strings → `None`).
    pub fn from_env(parse: &ParseDocumentConfig) -> Self {
        Self {
            llama_cloud: env_opt(&parse.llama_cloud_api_key_env),
            jina: env_opt(&parse.jina_api_key_env),
            firecrawl: env_opt(&parse.firecrawl_api_key_env),
        }
    }

    /// Same as [`Self::from_env`] but reads the env var names from a
    /// [`WebReadConfig`] — `llama_cloud` stays empty because
    /// LlamaParse only matters for the document pipeline.
    pub fn from_env_web_read(cfg: &WebReadConfig) -> Self {
        Self {
            llama_cloud: None,
            jina: env_opt(&cfg.jina_api_key_env),
            firecrawl: env_opt(&cfg.firecrawl_api_key_env),
        }
    }
}

fn env_opt(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    match std::env::var(name) {
        Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// One parse request decoded off the wire.
#[derive(Debug, Deserialize)]
pub struct ParseRequest {
    pub kind: String,
    pub payload: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// Wire-shaped response. `tier_used` carries the name of whichever
/// tier produced `text`; `source` echoes the request label so the
/// caller doesn't have to track it.
///
/// SEC PART 1: `text` carries content pulled from an external
/// document, URL, or OCR tier — i.e. attacker-controllable
/// bytes. In-process consumers that feed this value into an
/// LLM prompt MUST wrap it via
/// `relix_core::types::UntrustedText::new(resp.text).wrap_for_prompt()`
/// (or route it through `ai.perception_extract` for the full
/// two-stage isolation) so the planning model treats it as
/// inert data, not as instructions. The field is kept as
/// `String` so wire consumers (the dashboard, the chronicle,
/// storage) can read it unchanged; the boundary is enforced
/// at prompt-construction time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseResponse {
    pub text: String,
    pub chunks_created: u32,
    pub tier_used: String,
    pub source: String,
}

#[derive(Error, Debug)]
pub enum ParseError {
    #[error("parse_document disabled (set [tool.parse_document] enabled = true)")]
    Disabled,
    #[error("invalid args: {0}")]
    InvalidArgs(String),
    #[error("base64 decode: {0}")]
    Base64(String),
    #[error("utf-8 decode: {0}")]
    Utf8(String),
    #[error("pdf engine not enabled (set [tool.pdf])")]
    PdfUnavailable,
    #[error("pdf parse: {0}")]
    PdfParse(String),
    #[error("cloud http: {0}")]
    Http(String),
    #[error("cloud upstream {status}: {body}")]
    Upstream { status: u16, body: String },
    #[error("cloud timeout after {0}s")]
    Timeout(u64),
    #[error("cloud decode: {0}")]
    Decode(String),
    #[error("local url tier needs a ToolBackend, none was supplied")]
    BackendMissing,
    #[error("local url tier: {0}")]
    LocalUrl(String),
    #[error("no tier could handle the request")]
    NoTier,
}

/// Wire `tool.parse_document` onto `bridge`. The cap is registered
/// even when `[tool.parse_document]` is missing — in that case
/// [`ParseDocumentConfig::default()`] applies (enabled = true,
/// prefer_cloud = true) and cloud tiers silently no-op when their
/// env vars are absent.
pub fn register(
    bridge: &mut DispatchBridge,
    backend: Arc<ToolBackend>,
    pdf_cfg: Option<Arc<PdfConfig>>,
    parse_cfg: Arc<ParseDocumentConfig>,
) {
    let keys = Arc::new(ApiKeys::from_env(parse_cfg.as_ref()));
    bridge.register(
        "tool.parse_document",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let pdf = pdf_cfg.clone();
            let backend = backend.clone();
            let cfg = parse_cfg.clone();
            let keys = keys.clone();
            async move {
                match handle_parse_document(&cfg, &keys, pdf.as_deref(), Some(&backend), &ctx).await
                {
                    Ok(resp) => match serde_json::to_vec(&resp) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => invalid(format!("encode response: {e}")),
                    },
                    Err(e) => match e {
                        ParseError::Disabled => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("tool.parse_document: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                        ParseError::InvalidArgs(_)
                        | ParseError::Base64(_)
                        | ParseError::Utf8(_)
                        | ParseError::PdfUnavailable => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::INVALID_ARGS,
                            cause: format!("tool.parse_document: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                        _ => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("tool.parse_document: {e}"),
                            retry_hint: 1,
                            retry_after: None,
                        }),
                    },
                }
            }
        })),
    );
}

/// Pure-function pipeline entry point. Backend is `Option<&Arc<…>>`
/// so unit tests for non-URL kinds can pass `None`.
pub async fn handle_parse_document(
    cfg: &ParseDocumentConfig,
    keys: &ApiKeys,
    pdf_cfg: Option<&PdfConfig>,
    backend: Option<&Arc<ToolBackend>>,
    ctx: &InvocationCtx,
) -> Result<ParseResponse, ParseError> {
    if !cfg.enabled {
        return Err(ParseError::Disabled);
    }
    let req: ParseRequest = serde_json::from_slice(&ctx.args)
        .map_err(|e| ParseError::InvalidArgs(format!("decode JSON: {e}")))?;
    if req.kind.trim().is_empty() {
        return Err(ParseError::InvalidArgs("kind must be non-empty".into()));
    }
    if req.payload.trim().is_empty() {
        return Err(ParseError::InvalidArgs("payload must be non-empty".into()));
    }
    let source = req.source.clone().unwrap_or_default();
    let kind = req.kind.trim().to_ascii_lowercase();
    let timeout = Duration::from_secs(cfg.cloud_timeout_secs.max(5));

    match kind.as_str() {
        "text" | "markdown" | "code" => {
            // Plain text never touches the cloud — pointless cost.
            let text = decode_text_kind(&req.payload)?;
            Ok(ParseResponse {
                text,
                chunks_created: 0,
                tier_used: "local".into(),
                source,
            })
        }
        "pdf" => {
            if cfg.prefer_cloud
                && let Some(key) = keys.llama_cloud.as_deref()
            {
                match try_llama_parse(key, &req.payload, timeout).await {
                    Ok(text) => {
                        return Ok(ParseResponse {
                            text,
                            chunks_created: 0,
                            tier_used: "llama_parse".into(),
                            source,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "tool.parse_document: LlamaParse tier failed; falling through to local",
                        );
                    }
                }
            }
            match pdf_cfg {
                Some(cfg_pdf) => {
                    let text = decode_local_pdf(cfg_pdf, &req.payload)?;
                    Ok(ParseResponse {
                        text,
                        chunks_created: 0,
                        tier_used: "local".into(),
                        source,
                    })
                }
                None => Err(ParseError::PdfUnavailable),
            }
        }
        "url" => {
            let url = req.payload.trim().to_string();
            if cfg.prefer_cloud
                && let Some(key) = keys.jina.as_deref()
            {
                match try_jina_reader(key, &url, timeout).await {
                    Ok(text) => {
                        return Ok(ParseResponse {
                            text,
                            chunks_created: 0,
                            tier_used: "jina_reader".into(),
                            source,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "tool.parse_document: Jina Reader tier failed; falling through",
                        );
                    }
                }
            }
            if cfg.prefer_cloud
                && let Some(key) = keys.firecrawl.as_deref()
            {
                match try_firecrawl(key, &url, timeout).await {
                    Ok(text) => {
                        return Ok(ParseResponse {
                            text,
                            chunks_created: 0,
                            tier_used: "firecrawl".into(),
                            source,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "tool.parse_document: Firecrawl tier failed; falling through",
                        );
                    }
                }
            }
            match backend {
                Some(b) => {
                    let text = local_url_fetch_extract(b.as_ref(), &url).await?;
                    Ok(ParseResponse {
                        text,
                        chunks_created: 0,
                        tier_used: "local".into(),
                        source,
                    })
                }
                None => Err(ParseError::BackendMissing),
            }
        }
        other => Err(ParseError::InvalidArgs(format!(
            "unknown kind '{other}' (text/markdown/code/pdf/url)"
        ))),
    }
}

// ── Local tier helpers ─────────────────────────────────────────

pub(crate) fn decode_text_kind(payload: &str) -> Result<String, ParseError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| ParseError::Base64(e.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|e| ParseError::Utf8(e.to_string()))?;
    Ok(truncate(text, DEFAULT_MAX_OUTPUT_CHARS))
}

pub(crate) fn decode_local_pdf(cfg: &PdfConfig, payload: &str) -> Result<String, ParseError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| ParseError::Base64(e.to_string()))?;
    if bytes.len() > cfg.max_input_bytes {
        return Err(ParseError::InvalidArgs(format!(
            "pdf input {} bytes exceeds cap {}",
            bytes.len(),
            cfg.max_input_bytes
        )));
    }
    let doc = lopdf::Document::load_mem(&bytes).map_err(|e| ParseError::PdfParse(e.to_string()))?;
    let pages = doc.get_pages();
    if pages.len() > cfg.max_pages {
        return Err(ParseError::InvalidArgs(format!(
            "pdf {} pages exceeds cap {}",
            pages.len(),
            cfg.max_pages
        )));
    }
    Ok(super::pdf::extract_text(&doc, &pages, cfg.max_output_chars))
}

async fn local_url_fetch_extract(backend: &ToolBackend, url: &str) -> Result<String, ParseError> {
    use super::WebFetchOutcome;
    use super::web_extract::extract;
    let outcome = backend.fetch(url, usize::MAX).await;
    let body = match outcome {
        WebFetchOutcome::Ok { body, .. } => body,
        WebFetchOutcome::Rejected(e) => return Err(ParseError::LocalUrl(format!("ssrf: {e}"))),
        WebFetchOutcome::TooLarge {
            declared_bytes,
            cap,
        } => {
            return Err(ParseError::LocalUrl(format!(
                "body too large declared={declared_bytes} cap={cap}"
            )));
        }
        WebFetchOutcome::HttpStatus { status, final_url } => {
            return Err(ParseError::LocalUrl(format!(
                "http {status} for {final_url}"
            )));
        }
        WebFetchOutcome::ContentTypeRejected {
            content_type,
            final_url,
        } => {
            return Err(ParseError::LocalUrl(format!(
                "content-type {content_type} for {final_url}"
            )));
        }
        WebFetchOutcome::Transport(e) => {
            return Err(ParseError::LocalUrl(format!("transport: {e}")));
        }
        WebFetchOutcome::NotUtf8 { final_url } => {
            return Err(ParseError::LocalUrl(format!(
                "body not utf-8 for {final_url}"
            )));
        }
    };
    let parsed = extract(&body);
    Ok(truncate(parsed.text, DEFAULT_MAX_OUTPUT_CHARS))
}

fn truncate(mut s: String, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s;
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push_str("\n... [truncated]\n");
    s.clear();
    out
}

// ── Cloud tier clients ─────────────────────────────────────────

/// LlamaParse: upload-then-poll. The upload endpoint returns a job
/// id; the result endpoint returns markdown once the job status is
/// `SUCCESS`. Polls every 2s up to `timeout`.
async fn try_llama_parse(
    api_key: &str,
    b64_bytes: &str,
    timeout: Duration,
) -> Result<String, ParseError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64_bytes)
        .map_err(|e| ParseError::Base64(format!("llama_parse decode input: {e}")))?;
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ParseError::Http(e.to_string()))?;

    let part = multipart::Part::bytes(bytes)
        .file_name("document.pdf")
        .mime_str("application/pdf")
        .map_err(|e| ParseError::Http(format!("multipart mime: {e}")))?;
    let form = multipart::Form::new().part("file", part);
    let upload_url = "https://api.cloud.llamaindex.ai/api/parsing/upload";
    // SEC PART 6: cloud-tier SSRF check.
    super::security::check_ssrf_cloud_tier_global(upload_url)
        .map_err(|e| ParseError::Http(format!("llamaparse ssrf: {e}")))?;
    let resp = client
        .post(upload_url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| ParseError::Http(format!("upload: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ParseError::Decode(format!("upload body: {e}")))?;
    if !status.is_success() {
        return Err(ParseError::Upstream {
            status: status.as_u16(),
            body,
        });
    }
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| ParseError::Decode(format!("upload json: {e}; body={body}")))?;
    let job_id = v
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ParseError::Decode(format!("upload missing id: {body}")))?
        .to_string();

    // Poll. Each tick is 2s; total polling time clamped to `timeout`.
    let result_url = format!(
        "https://api.cloud.llamaindex.ai/api/parsing/job/{}/result/markdown",
        job_id
    );
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(ParseError::Timeout(timeout.as_secs()));
        }
        // SEC PART 6 — re-check on each poll (DNS-cached).
        super::security::check_ssrf_cloud_tier_global(&result_url)
            .map_err(|e| ParseError::Http(format!("llamaparse poll ssrf: {e}")))?;
        let resp = client
            .get(&result_url)
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| ParseError::Http(format!("poll: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ParseError::Decode(format!("poll body: {e}")))?;
        if status.as_u16() == 404 || status.as_u16() == 202 {
            // Still working — server returns pending. Sleep.
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        if !status.is_success() {
            return Err(ParseError::Upstream {
                status: status.as_u16(),
                body,
            });
        }
        // SUCCESS path: response is JSON `{"markdown": "..."}` or the
        // raw markdown text. Try JSON first.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
            && let Some(md) = v.get("markdown").and_then(|x| x.as_str())
        {
            return Ok(truncate(md.to_string(), DEFAULT_MAX_OUTPUT_CHARS));
        }
        return Ok(truncate(body, DEFAULT_MAX_OUTPUT_CHARS));
    }
}

/// Jina Reader: GET `https://r.jina.ai/{url}` with a bearer-auth
/// header. Returns clean markdown for the target URL.
async fn try_jina_reader(
    api_key: &str,
    url: &str,
    timeout: Duration,
) -> Result<String, ParseError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ParseError::Http(e.to_string()))?;
    let endpoint = format!("https://r.jina.ai/{url}");
    // SEC PART 6.
    super::security::check_ssrf_cloud_tier_global(&endpoint)
        .map_err(|e| ParseError::Http(format!("jina ssrf: {e}")))?;
    let resp = client
        .get(&endpoint)
        .bearer_auth(api_key)
        .header("Accept", "text/plain")
        .send()
        .await
        .map_err(|e| ParseError::Http(format!("jina_reader: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ParseError::Decode(format!("jina_reader body: {e}")))?;
    if !status.is_success() {
        return Err(ParseError::Upstream {
            status: status.as_u16(),
            body,
        });
    }
    Ok(truncate(body, DEFAULT_MAX_OUTPUT_CHARS))
}

/// Firecrawl scrape: POST `/v1/scrape` with `{url, formats:["markdown"]}`
/// and `Authorization: Bearer ...`. Response body carries
/// `data.markdown`.
async fn try_firecrawl(api_key: &str, url: &str, timeout: Duration) -> Result<String, ParseError> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| ParseError::Http(e.to_string()))?;
    let payload = serde_json::json!({ "url": url, "formats": ["markdown"] });
    let firecrawl_url = "https://api.firecrawl.dev/v1/scrape";
    // SEC PART 6.
    super::security::check_ssrf_cloud_tier_global(firecrawl_url)
        .map_err(|e| ParseError::Http(format!("firecrawl ssrf: {e}")))?;
    let resp = client
        .post(firecrawl_url)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|e| ParseError::Http(format!("firecrawl: {e}")))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ParseError::Decode(format!("firecrawl body: {e}")))?;
    if !status.is_success() {
        return Err(ParseError::Upstream {
            status: status.as_u16(),
            body,
        });
    }
    parse_firecrawl_body(&body)
}

pub(crate) fn parse_firecrawl_body(body: &str) -> Result<String, ParseError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ParseError::Decode(format!("firecrawl json: {e}; body={body}")))?;
    let markdown = v
        .get("data")
        .and_then(|d| d.get("markdown"))
        .and_then(|m| m.as_str())
        .or_else(|| v.get("markdown").and_then(|m| m.as_str()))
        .ok_or_else(|| ParseError::Decode(format!("firecrawl missing markdown: {body}")))?;
    Ok(truncate(markdown.to_string(), DEFAULT_MAX_OUTPUT_CHARS))
}

// ── tool.web_read tiered handler ───────────────────────────────

/// Wire-shape request for `tool.web_read`.
#[derive(Debug, Deserialize)]
pub struct WebReadRequest {
    pub url: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// Register `tool.web_read` with the tiered Jina → Firecrawl → local
/// fallthrough. Wire format is identical to `tool.parse_document`
/// (JSON in, JSON out with `tier_used`).
pub fn register_web_read(
    bridge: &mut DispatchBridge,
    backend: Arc<ToolBackend>,
    cfg: Arc<WebReadConfig>,
) {
    let keys = Arc::new(ApiKeys::from_env_web_read(cfg.as_ref()));
    bridge.register(
        "tool.web_read",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let backend = backend.clone();
            let cfg = cfg.clone();
            let keys = keys.clone();
            async move {
                match handle_web_read(&cfg, &keys, &backend, &ctx).await {
                    Ok(resp) => match serde_json::to_vec(&resp) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => invalid(format!("encode web_read response: {e}")),
                    },
                    Err(e) => match e {
                        ParseError::Disabled | ParseError::InvalidArgs(_) | ParseError::Utf8(_) => {
                            HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::INVALID_ARGS,
                                cause: format!("tool.web_read: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            })
                        }
                        _ => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("tool.web_read: {e}"),
                            retry_hint: 1,
                            retry_after: None,
                        }),
                    },
                }
            }
        })),
    );
}

/// Pure-function `tool.web_read` handler — accepts either the new
/// JSON envelope `{ "url": "...", "source"?: "..." }` OR the legacy
/// `<mode>|<url>` shape consumed by the §7.23 simple-tier closure
/// that perception.rs previously delegated to. The legacy shape is
/// preserved so SOL flows that already point at `tool.web_read`
/// keep working; new callers get tier_used metadata.
pub async fn handle_web_read(
    cfg: &WebReadConfig,
    keys: &ApiKeys,
    backend: &Arc<ToolBackend>,
    ctx: &InvocationCtx,
) -> Result<ParseResponse, ParseError> {
    if !cfg.enabled {
        return Err(ParseError::Disabled);
    }
    let raw = std::str::from_utf8(&ctx.args).map_err(|e| ParseError::Utf8(e.to_string()))?;
    let trimmed = raw.trim();
    let (url, source) = if trimmed.starts_with('{') {
        let req: WebReadRequest = serde_json::from_str(trimmed)
            .map_err(|e| ParseError::InvalidArgs(format!("decode JSON: {e}")))?;
        if req.url.trim().is_empty() {
            return Err(ParseError::InvalidArgs("url must be non-empty".into()));
        }
        (req.url.trim().to_string(), req.source.unwrap_or_default())
    } else {
        // Legacy `<mode>|<url>` — we only honour the URL portion (cloud
        // tiers return clean markdown; the legacy modes are local-only).
        let mut parts = trimmed.splitn(2, '|');
        let _mode = parts.next().unwrap_or("").trim();
        let url = parts.next().unwrap_or("").trim();
        if url.is_empty() {
            return Err(ParseError::InvalidArgs(
                "legacy `<mode>|<url>` shape requires non-empty url".into(),
            ));
        }
        (url.to_string(), String::new())
    };
    let timeout = Duration::from_secs(cfg.cloud_timeout_secs.max(5));

    if cfg.prefer_cloud
        && let Some(key) = keys.jina.as_deref()
    {
        match try_jina_reader(key, &url, timeout).await {
            Ok(text) => {
                return Ok(ParseResponse {
                    text,
                    chunks_created: 0,
                    tier_used: "jina_reader".into(),
                    source,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "tool.web_read: Jina tier failed; falling through");
            }
        }
    }
    if cfg.prefer_cloud
        && let Some(key) = keys.firecrawl.as_deref()
    {
        match try_firecrawl(key, &url, timeout).await {
            Ok(text) => {
                return Ok(ParseResponse {
                    text,
                    chunks_created: 0,
                    tier_used: "firecrawl".into(),
                    source,
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "tool.web_read: Firecrawl tier failed; falling through");
            }
        }
    }
    let text = local_url_fetch_extract(backend.as_ref(), &url).await?;
    Ok(ParseResponse {
        text,
        chunks_created: 0,
        tier_used: "local".into(),
        source,
    })
}

fn invalid(msg: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg,
        retry_hint: 0,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx_for(body: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: body.to_vec(),
            tenant_id: None,
        }
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    fn req(kind: &str, payload: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "kind": kind,
            "payload": payload,
            "source": "test.input",
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn text_kind_always_uses_local_tier_regardless_of_keys() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys {
            llama_cloud: Some("fake-key".into()),
            jina: Some("fake-key".into()),
            firecrawl: Some("fake-key".into()),
        };
        let body = req("text", &b64("hello world"));
        let resp = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap();
        assert_eq!(resp.text, "hello world");
        assert_eq!(resp.tier_used, "local");
        assert_eq!(resp.source, "test.input");
        assert_eq!(resp.chunks_created, 0);
    }

    #[tokio::test]
    async fn markdown_kind_uses_local_tier() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let body = req("markdown", &b64("# Title\n\n- a\n- b\n"));
        let resp = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap();
        assert_eq!(resp.text, "# Title\n\n- a\n- b\n");
        assert_eq!(resp.tier_used, "local");
    }

    #[tokio::test]
    async fn code_kind_uses_local_tier() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let body = req("code", &b64("fn main(){}\n"));
        let resp = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap();
        assert_eq!(resp.text, "fn main(){}\n");
        assert_eq!(resp.tier_used, "local");
    }

    #[tokio::test]
    async fn pdf_kind_without_llama_key_falls_through_to_local() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default(); // llama_cloud = None
        let body = req("pdf", &b64("not-a-pdf"));
        // PdfConfig is also None — so we get PdfUnavailable, which proves
        // that the LlamaParse tier was NOT attempted (no env key).
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        assert!(matches!(err, ParseError::PdfUnavailable));
    }

    #[tokio::test]
    async fn prefer_cloud_false_skips_cloud_even_when_keys_set() {
        let cfg = ParseDocumentConfig {
            prefer_cloud: false,
            ..ParseDocumentConfig::default()
        };
        let keys = ApiKeys {
            llama_cloud: Some("fake-key".into()),
            ..ApiKeys::default()
        };
        let body = req("pdf", &b64("not-a-pdf"));
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        // No PDF backend either, but the key point is: error is
        // PdfUnavailable, NOT a cloud error. So cloud was skipped.
        assert!(matches!(err, ParseError::PdfUnavailable));
    }

    #[tokio::test]
    async fn disabled_returns_clear_error() {
        let cfg = ParseDocumentConfig {
            enabled: false,
            ..ParseDocumentConfig::default()
        };
        let keys = ApiKeys::default();
        let body = req("text", &b64("hi"));
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        assert!(matches!(err, ParseError::Disabled));
    }

    #[tokio::test]
    async fn missing_payload_rejects_with_invalid_args() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let body = serde_json::to_vec(&serde_json::json!({"kind": "text", "payload": ""})).unwrap();
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        assert!(matches!(err, ParseError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn unknown_kind_rejects() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let body = req("video", &b64("xxx"));
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        match err {
            ParseError::InvalidArgs(m) => assert!(m.contains("unknown kind")),
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_json_rejects() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(b"not json"))
            .await
            .unwrap_err();
        assert!(matches!(err, ParseError::InvalidArgs(_)));
    }

    #[tokio::test]
    async fn url_kind_without_backend_or_keys_errors_with_backend_missing() {
        let cfg = ParseDocumentConfig::default();
        let keys = ApiKeys::default();
        let body = req("url", "https://example.invalid/");
        let err = handle_parse_document(&cfg, &keys, None, None, &ctx_for(&body))
            .await
            .unwrap_err();
        assert!(matches!(err, ParseError::BackendMissing));
    }

    #[test]
    fn parse_firecrawl_body_extracts_data_markdown() {
        let body = r##"{"data": {"markdown": "# hi\n"}}"##;
        let text = parse_firecrawl_body(body).unwrap();
        assert_eq!(text, "# hi\n");
    }

    #[test]
    fn parse_firecrawl_body_extracts_top_level_markdown() {
        let body = r#"{"markdown": "hello"}"#;
        let text = parse_firecrawl_body(body).unwrap();
        assert_eq!(text, "hello");
    }

    #[test]
    fn parse_firecrawl_body_rejects_missing_markdown() {
        let err = parse_firecrawl_body(r#"{"data": {}}"#).unwrap_err();
        assert!(matches!(err, ParseError::Decode(_)));
    }

    #[test]
    fn truncate_marks_overflow_with_trailing_notice() {
        let s = "x".repeat(DEFAULT_MAX_OUTPUT_CHARS + 100);
        let t = truncate(s, DEFAULT_MAX_OUTPUT_CHARS);
        assert!(t.contains("[truncated]"));
        assert!(t.chars().count() < DEFAULT_MAX_OUTPUT_CHARS + 100);
    }

    #[test]
    fn truncate_leaves_short_input_alone() {
        let t = truncate("hi".to_string(), 10);
        assert_eq!(t, "hi");
    }

    #[test]
    fn api_keys_from_env_treats_empty_var_name_as_none() {
        let cfg = ParseDocumentConfig {
            llama_cloud_api_key_env: String::new(),
            jina_api_key_env: String::new(),
            firecrawl_api_key_env: String::new(),
            ..ParseDocumentConfig::default()
        };
        let keys = ApiKeys::from_env(&cfg);
        assert!(keys.llama_cloud.is_none());
        assert!(keys.jina.is_none());
        assert!(keys.firecrawl.is_none());
    }

    #[test]
    fn parse_request_decodes_optional_source() {
        let v: ParseRequest =
            serde_json::from_str(r#"{"kind":"text","payload":"aGVsbG8="}"#).unwrap();
        assert_eq!(v.kind, "text");
        assert_eq!(v.payload, "aGVsbG8=");
        assert!(v.source.is_none());
        let with_src: ParseRequest =
            serde_json::from_str(r#"{"kind":"text","payload":"x","source":"docA"}"#).unwrap();
        assert_eq!(with_src.source.as_deref(), Some("docA"));
    }

    #[test]
    fn parse_response_round_trips_through_serde() {
        let r = ParseResponse {
            text: "body".into(),
            chunks_created: 3,
            tier_used: "llama_parse".into(),
            source: "docA".into(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: ParseResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }
}
