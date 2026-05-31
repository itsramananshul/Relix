//! RELIX-7.18 / GAP 17 — provider-backed web search.
//!
//! The §7.18 research-backed identity pipeline needs to pull
//! open-web context about a subject. This module ships the
//! provider abstraction: a single async trait
//! [`WebSearchProvider`] + three production implementations
//! (Tavily, Brave Search, Perplexity) + an `auto`-selection
//! helper that picks the first provider whose API key the
//! operator has wired via env var.
//!
//! Every implementation:
//!
//! - reads its API key from the env var once at construction
//!   time; the key is never written to the database, never
//!   logged, never echoed back to a response body;
//! - makes real HTTPS calls via the workspace's `reqwest`
//!   posture (rustls-tls; no openssl);
//! - clamps `max_results` to a sane upper bound so a runaway
//!   caller can't ask for thousands;
//! - maps provider-side HTTP / JSON failures to a structured
//!   [`SearchError`] the caller can surface.
//!
//! Config:
//!
//! ```toml
//! [tools.web_search]
//! enabled    = true
//! max_results = 10
//! provider    = "auto"   # or "tavily" | "brave" | "perplexity"
//! ```
//!
//! `"auto"` walks `TAVILY_API_KEY` → `BRAVE_SEARCH_API_KEY` →
//! `PERPLEXITY_API_KEY` and uses the first non-empty match.
//! When every key is unset, [`build_provider_from_env`]
//! returns [`SearchError::NoProviderAvailable`] with a
//! caller-friendly message listing the three env vars.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One search hit. Optional fields stay `None` when the
/// upstream provider does not populate the field for that
/// row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// ISO-8601 publication date when the provider reports
    /// one. `None` is the common case for most search
    /// providers.
    #[serde(default)]
    pub published_at: Option<String>,
}

/// Errors surfaced by every provider.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error(
        "web search: no provider is available — set one of TAVILY_API_KEY, BRAVE_SEARCH_API_KEY, or PERPLEXITY_API_KEY"
    )]
    NoProviderAvailable,
    #[error("web search: provider `{provider}` API key env var `{env_var}` is unset or empty")]
    MissingApiKey {
        provider: &'static str,
        env_var: &'static str,
    },
    #[error("web search: provider `{0}` is unknown (expected auto | tavily | brave | perplexity)")]
    UnknownProvider(String),
    #[error("web search: HTTP error: {0}")]
    Http(String),
    #[error("web search: decode error: {0}")]
    Decode(String),
    #[error("web search: provider returned status {status}: {body}")]
    Upstream { status: u16, body: String },
}

/// Production provider trait. Implementations are `Send +
/// Sync` so they can live behind a shared `Arc`.
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    /// Short identifier shown in logs.
    fn provider_name(&self) -> &'static str;

    /// Run a single search query. `max_results` is the
    /// caller's request; implementations clamp it to the
    /// upstream provider's accepted range (typically `<= 20`).
    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError>;
}

/// `[tools.web_search]` block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct WebSearchConfig {
    /// Master switch. `false` (the default) keeps the
    /// research pipeline from wiring a provider; the
    /// `identity.research` cap returns a clear error
    /// instructing the operator to enable + configure a key.
    #[serde(default)]
    pub enabled: bool,
    /// Hard cap on results returned in one call. Clamped to
    /// `[1, 20]`. Default 10.
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    /// `auto` | `tavily` | `brave` | `perplexity`. Defaults
    /// to `auto`.
    #[serde(default = "default_provider")]
    pub provider: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_results: default_max_results(),
            provider: default_provider(),
        }
    }
}

fn default_max_results() -> usize {
    10
}

fn default_provider() -> String {
    "auto".into()
}

/// Upper bound on `max_results` clamping. Every provider's
/// public docs cap at 20.
pub const MAX_RESULTS_HARD_CAP: usize = 20;

const TAVILY_ENV: &str = "TAVILY_API_KEY";
const BRAVE_ENV: &str = "BRAVE_SEARCH_API_KEY";
const PERPLEXITY_ENV: &str = "PERPLEXITY_API_KEY";

/// Bundle of the three operator-supplied API keys. `None`
/// means "not set"; an empty string is treated the same way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiKeys {
    pub tavily: Option<String>,
    pub brave: Option<String>,
    pub perplexity: Option<String>,
}

impl ApiKeys {
    /// Read every key from the process env in one shot.
    pub fn from_env() -> Self {
        Self {
            tavily: env_var_nonempty(TAVILY_ENV),
            brave: env_var_nonempty(BRAVE_ENV),
            perplexity: env_var_nonempty(PERPLEXITY_ENV),
        }
    }
}

/// Pick a provider from operator config + a fully-resolved
/// [`ApiKeys`] bundle. Returns `Ok(None)` when
/// `cfg.enabled = false` so the caller can render a clean
/// "search disabled" surface; returns `Err(NoProviderAvailable)`
/// when `auto` was requested but no key is set.
pub fn build_provider(
    cfg: &WebSearchConfig,
    keys: &ApiKeys,
) -> Result<Option<Arc<dyn WebSearchProvider>>, SearchError> {
    if !cfg.enabled {
        return Ok(None);
    }
    let choice = cfg.provider.trim().to_ascii_lowercase();
    match choice.as_str() {
        "auto" => {
            if let Some(key) = keys.tavily.clone().filter(|s| !s.trim().is_empty()) {
                return Ok(Some(Arc::new(TavilyProvider::new(key))));
            }
            if let Some(key) = keys.brave.clone().filter(|s| !s.trim().is_empty()) {
                return Ok(Some(Arc::new(BraveProvider::new(key))));
            }
            if let Some(key) = keys.perplexity.clone().filter(|s| !s.trim().is_empty()) {
                return Ok(Some(Arc::new(PerplexityProvider::new(key))));
            }
            Err(SearchError::NoProviderAvailable)
        }
        "tavily" => match keys.tavily.clone().filter(|s| !s.trim().is_empty()) {
            Some(k) => Ok(Some(Arc::new(TavilyProvider::new(k)))),
            None => Err(SearchError::MissingApiKey {
                provider: "tavily",
                env_var: TAVILY_ENV,
            }),
        },
        "brave" => match keys.brave.clone().filter(|s| !s.trim().is_empty()) {
            Some(k) => Ok(Some(Arc::new(BraveProvider::new(k)))),
            None => Err(SearchError::MissingApiKey {
                provider: "brave",
                env_var: BRAVE_ENV,
            }),
        },
        "perplexity" => match keys.perplexity.clone().filter(|s| !s.trim().is_empty()) {
            Some(k) => Ok(Some(Arc::new(PerplexityProvider::new(k)))),
            None => Err(SearchError::MissingApiKey {
                provider: "perplexity",
                env_var: PERPLEXITY_ENV,
            }),
        },
        other => Err(SearchError::UnknownProvider(other.to_string())),
    }
}

/// Convenience wrapper that reads the env directly. Used by
/// the controller startup path.
pub fn build_provider_from_env(
    cfg: &WebSearchConfig,
) -> Result<Option<Arc<dyn WebSearchProvider>>, SearchError> {
    build_provider(cfg, &ApiKeys::from_env())
}

fn env_var_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn clamp_max_results(n: usize) -> usize {
    n.clamp(1, MAX_RESULTS_HARD_CAP)
}

fn http_client() -> Result<reqwest::Client, SearchError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("relix-research/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| SearchError::Http(format!("build client: {e}")))
}

// ─── Tavily ──────────────────────────────────────────────────

/// Tavily Search API client. `POST https://api.tavily.com/search`
/// with the API key in the JSON body. `search_depth = "advanced"`
/// gives the LLM-research-friendly content the pipeline expects.
pub struct TavilyProvider {
    /// SEC PART 2: Zeroizing wrapper; API key bytes wiped on drop.
    api_key: zeroize::Zeroizing<String>,
}

impl TavilyProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key: zeroize::Zeroizing::new(api_key),
        }
    }
}

#[async_trait]
impl WebSearchProvider for TavilyProvider {
    fn provider_name(&self) -> &'static str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let client = http_client()?;
        let n = clamp_max_results(max_results);
        let body = serde_json::json!({
            "api_key": self.api_key.as_str(),
            "query": query,
            "max_results": n,
            "search_depth": "advanced",
        });
        let url = "https://api.tavily.com/search";
        // SEC PART 6: cloud-tier SSRF check (DNS-cached 5 min).
        super::security::check_ssrf_cloud_tier_global(url)
            .map_err(|e| SearchError::Http(format!("tavily ssrf: {e}")))?;
        let resp = client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SearchError::Http(format!("tavily: {e}")))?;
        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| SearchError::Http(format!("tavily body: {e}")))?;
        if !status.is_success() {
            return Err(SearchError::Upstream {
                status: status.as_u16(),
                body: raw,
            });
        }
        parse_tavily_body(&raw)
    }
}

pub(crate) fn parse_tavily_body(raw: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| SearchError::Decode(format!("tavily: {e}")))?;
    let arr = v
        .get("results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        out.push(SearchResult {
            title: r
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            url: r
                .get("url")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            snippet: r
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            published_at: r
                .get("published_date")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}

// ─── Brave Search ────────────────────────────────────────────

/// Brave Search API client. `GET https://api.search.brave.com/res/v1/web/search`
/// with the API key in the `X-Subscription-Token` header.
pub struct BraveProvider {
    /// SEC PART 2: Zeroizing wrapper; API key bytes wiped on drop.
    api_key: zeroize::Zeroizing<String>,
}

impl BraveProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key: zeroize::Zeroizing::new(api_key),
        }
    }
}

#[async_trait]
impl WebSearchProvider for BraveProvider {
    fn provider_name(&self) -> &'static str {
        "brave"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let client = http_client()?;
        let n = clamp_max_results(max_results);
        let url = "https://api.search.brave.com/res/v1/web/search";
        // SEC PART 6.
        super::security::check_ssrf_cloud_tier_global(url)
            .map_err(|e| SearchError::Http(format!("brave ssrf: {e}")))?;
        let resp = client
            .get(url)
            .query(&[("q", query.to_string()), ("count", n.to_string())])
            .header("X-Subscription-Token", self.api_key.as_str())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| SearchError::Http(format!("brave: {e}")))?;
        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| SearchError::Http(format!("brave body: {e}")))?;
        if !status.is_success() {
            return Err(SearchError::Upstream {
                status: status.as_u16(),
                body: raw,
            });
        }
        parse_brave_body(&raw)
    }
}

pub(crate) fn parse_brave_body(raw: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| SearchError::Decode(format!("brave: {e}")))?;
    let arr = v
        .pointer("/web/results")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        out.push(SearchResult {
            title: r
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            url: r
                .get("url")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            snippet: r
                .get("description")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            published_at: r.get("age").and_then(|x| x.as_str()).map(|s| s.to_string()),
        });
    }
    Ok(out)
}

// ─── Perplexity ──────────────────────────────────────────────

/// Perplexity client. The Perplexity API is a chat-completions
/// surface that emits the search results in the assistant
/// response; we ask for a strict JSON array of
/// `{title, url, snippet}` entries.
pub struct PerplexityProvider {
    /// SEC PART 2: Zeroizing wrapper; API key bytes wiped on drop.
    api_key: zeroize::Zeroizing<String>,
}

impl PerplexityProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key: zeroize::Zeroizing::new(api_key),
        }
    }
}

#[async_trait]
impl WebSearchProvider for PerplexityProvider {
    fn provider_name(&self) -> &'static str {
        "perplexity"
    }

    async fn search(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let client = http_client()?;
        let n = clamp_max_results(max_results);
        let content = format!(
            "Search for: {query}. Return the top {n} results as a JSON array \
             with fields title, url, snippet. Reply with ONLY the JSON array, \
             no preamble, no markdown fences.",
        );
        let body = serde_json::json!({
            "model": "sonar",
            "messages": [
                { "role": "user", "content": content }
            ]
        });
        let url = "https://api.perplexity.ai/chat/completions";
        // SEC PART 6.
        super::security::check_ssrf_cloud_tier_global(url)
            .map_err(|e| SearchError::Http(format!("perplexity ssrf: {e}")))?;
        let resp = client
            .post(url)
            .bearer_auth(self.api_key.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|e| SearchError::Http(format!("perplexity: {e}")))?;
        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| SearchError::Http(format!("perplexity body: {e}")))?;
        if !status.is_success() {
            return Err(SearchError::Upstream {
                status: status.as_u16(),
                body: raw,
            });
        }
        parse_perplexity_body(&raw)
    }
}

pub(crate) fn parse_perplexity_body(raw: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| SearchError::Decode(format!("perplexity: {e}")))?;
    let content = v
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if content.is_empty() {
        return Ok(Vec::new());
    }
    // The assistant occasionally wraps the JSON in markdown
    // fences despite the prompt. Strip them defensively.
    let trimmed = strip_json_fences(content);
    let arr: Vec<serde_json::Value> = serde_json::from_str(&trimmed)
        .map_err(|e| SearchError::Decode(format!("perplexity json: {e} (body={trimmed})")))?;
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        out.push(SearchResult {
            title: r
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            url: r
                .get("url")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            snippet: r
                .get("snippet")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            published_at: r
                .get("published_at")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}

fn strip_json_fences(s: &str) -> String {
    let mut t = s.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        t = rest.trim_start();
    } else if let Some(rest) = t.strip_prefix("```") {
        t = rest.trim_start();
    }
    if let Some(rest) = t.strip_suffix("```") {
        t = rest.trim_end();
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(tavily: Option<&str>, brave: Option<&str>, perplexity: Option<&str>) -> ApiKeys {
        ApiKeys {
            tavily: tavily.map(|s| s.to_string()),
            brave: brave.map(|s| s.to_string()),
            perplexity: perplexity.map(|s| s.to_string()),
        }
    }

    #[test]
    fn default_config_is_disabled_with_auto_provider_and_ten_results() {
        let cfg = WebSearchConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.provider, "auto");
        assert_eq!(cfg.max_results, 10);
    }

    #[test]
    fn build_provider_returns_none_when_disabled() {
        let cfg = WebSearchConfig::default();
        assert!(matches!(
            build_provider(&cfg, &ApiKeys::default()),
            Ok(None)
        ));
    }

    #[test]
    fn auto_picks_tavily_first_when_key_present() {
        let cfg = WebSearchConfig {
            enabled: true,
            ..WebSearchConfig::default()
        };
        let all = keys(Some("t"), Some("b"), Some("p"));
        let provider = build_provider(&cfg, &all).unwrap().unwrap();
        assert_eq!(provider.provider_name(), "tavily");
    }

    #[test]
    fn auto_falls_through_to_brave_when_tavily_missing() {
        let cfg = WebSearchConfig {
            enabled: true,
            ..WebSearchConfig::default()
        };
        let provider = build_provider(&cfg, &keys(None, Some("b"), None))
            .unwrap()
            .unwrap();
        assert_eq!(provider.provider_name(), "brave");
    }

    #[test]
    fn auto_falls_through_to_perplexity_when_only_perplexity_set() {
        let cfg = WebSearchConfig {
            enabled: true,
            ..WebSearchConfig::default()
        };
        let provider = build_provider(&cfg, &keys(None, None, Some("p")))
            .unwrap()
            .unwrap();
        assert_eq!(provider.provider_name(), "perplexity");
    }

    #[test]
    fn auto_treats_empty_keys_as_missing() {
        let cfg = WebSearchConfig {
            enabled: true,
            ..WebSearchConfig::default()
        };
        let provider = build_provider(&cfg, &keys(Some("   "), Some(""), Some("perplexity-key")))
            .unwrap()
            .unwrap();
        assert_eq!(provider.provider_name(), "perplexity");
    }

    #[test]
    fn auto_returns_no_provider_available_when_every_key_missing() {
        let cfg = WebSearchConfig {
            enabled: true,
            ..WebSearchConfig::default()
        };
        match build_provider(&cfg, &ApiKeys::default()) {
            Err(SearchError::NoProviderAvailable) => {
                // Re-render the error and inspect the message
                // body — the surface MUST name every env var
                // so the operator sees exactly what to set.
                let msg = SearchError::NoProviderAvailable.to_string();
                assert!(msg.contains(TAVILY_ENV));
                assert!(msg.contains(BRAVE_ENV));
                assert!(msg.contains(PERPLEXITY_ENV));
            }
            Err(other) => panic!("expected NoProviderAvailable, got {other:?}"),
            Ok(_) => panic!("expected NoProviderAvailable, got Ok"),
        }
    }

    #[test]
    fn explicit_provider_demands_matching_key() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: "brave".into(),
            ..WebSearchConfig::default()
        };
        match build_provider(&cfg, &ApiKeys::default()) {
            Err(SearchError::MissingApiKey { provider, env_var }) => {
                assert_eq!(provider, "brave");
                assert_eq!(env_var, BRAVE_ENV);
            }
            Err(other) => panic!("expected MissingApiKey, got {other:?}"),
            Ok(_) => panic!("expected MissingApiKey, got Ok"),
        }
    }

    #[test]
    fn unknown_provider_string_errors() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: "google".into(),
            ..WebSearchConfig::default()
        };
        match build_provider(&cfg, &keys(Some("t"), Some("b"), Some("p"))) {
            Err(SearchError::UnknownProvider(_)) => {}
            Err(other) => panic!("expected UnknownProvider, got {other:?}"),
            Ok(_) => panic!("expected UnknownProvider, got Ok"),
        }
    }

    #[test]
    fn clamp_max_results_pins_to_one_through_twenty() {
        assert_eq!(clamp_max_results(0), 1);
        assert_eq!(clamp_max_results(10), 10);
        assert_eq!(clamp_max_results(20), 20);
        assert_eq!(clamp_max_results(1000), 20);
    }

    #[test]
    fn parse_tavily_handles_typical_payload() {
        let raw = r#"{
            "results": [
                {"title":"Alpha","url":"https://a","content":"snip a","published_date":"2024-01-02"},
                {"title":"Beta","url":"https://b","content":"snip b"}
            ]
        }"#;
        let out = parse_tavily_body(raw).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Alpha");
        assert_eq!(out[0].url, "https://a");
        assert_eq!(out[0].snippet, "snip a");
        assert_eq!(out[0].published_at.as_deref(), Some("2024-01-02"));
        assert!(out[1].published_at.is_none());
    }

    #[test]
    fn parse_brave_handles_nested_payload() {
        let raw = r#"{
            "web": {
                "results": [
                    {"title":"Foo","url":"https://foo","description":"snip foo","age":"2 days ago"},
                    {"title":"Bar","url":"https://bar","description":"snip bar"}
                ]
            }
        }"#;
        let out = parse_brave_body(raw).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Foo");
        assert_eq!(out[0].snippet, "snip foo");
        assert_eq!(out[0].published_at.as_deref(), Some("2 days ago"));
        assert_eq!(out[1].url, "https://bar");
        assert!(out[1].published_at.is_none());
    }

    #[test]
    fn parse_perplexity_extracts_inner_json_even_with_code_fences() {
        let raw = r#"{
            "choices": [
                {
                    "message": {
                        "content": "```json\n[{\"title\":\"X\",\"url\":\"https://x\",\"snippet\":\"x snip\"}]\n```"
                    }
                }
            ]
        }"#;
        let out = parse_perplexity_body(raw).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "X");
        assert_eq!(out[0].url, "https://x");
        assert_eq!(out[0].snippet, "x snip");
    }

    #[test]
    fn parse_perplexity_with_empty_content_returns_empty_vec() {
        let raw = r#"{"choices":[{"message":{"content":""}}]}"#;
        let out = parse_perplexity_body(raw).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn search_result_missing_optional_fields_round_trips() {
        let r = SearchResult {
            title: "t".into(),
            url: "u".into(),
            snippet: "s".into(),
            published_at: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: SearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
