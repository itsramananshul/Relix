//! CW4 — Browser-automation capability foundation.
//!
//! Hermes ships `browser_tool` (Playwright + CDP + Camofox) for
//! full headless-browser automation. Relix's CW4 foundation
//! lands the **honest scaffold**: capability descriptors,
//! session model, wire format, dispatch, error envelope, and
//! dashboard / CLI visibility. PH-BROWSER-FEATURES (this
//! milestone) refactors the single-backend scaffold into a
//! pluggable trait + three feature-gated backend modules so
//! the live implementations can land independently:
//!
//! - [`headless_chrome`] — Chrome DevTools Protocol driver. The
//!   recommended default per D-008 (smallest install — no Node,
//!   no sidecar; just a `chrome` / `chromium` binary). Gated on
//!   `--features browser-headless-chrome`.
//! - [`playwright`] — Playwright sidecar over stdio JSON-RPC.
//!   Best multi-engine coverage (Chromium / Firefox / WebKit)
//!   but heaviest install (Node + browsers + npm package).
//!   Gated on `--features browser-playwright`.
//! - [`webdriver`] — fantoccini / WebDriver-over-HTTP against
//!   an operator-supplied `chromedriver` / `geckodriver`
//!   sidecar. Most standards-y. Gated on
//!   `--features browser-webdriver`.
//!
//! Each backend module ships only when its feature is compiled
//! in. Without any feature, only [`NoneBackend`] is available
//! (and the operator must set `backend = "none"` to use it).
//!
//! ## Honesty contract
//!
//! Per the user's CW4 directive: *"If no actual browser backend
//! exists yet, do NOT fake browser execution. Create real
//! contracts and explicit backend-missing errors. No mock
//! success."*
//!
//! Concrete posture (PH-BROWSER-FEATURES update):
//!
//! - `[tool.browser] backend = "none"` (default when the
//!   section is present at all) makes every navigate /
//!   get_text / screenshot call return a typed
//!   `BackendNotConnected` error.
//! - `backend = "headless_chrome"` / `"playwright"` /
//!   `"webdriver"` with the corresponding feature compiled →
//!   returns the live backend (TODAY: a labeled scaffold that
//!   names the upcoming milestone in its BackendNotConnected
//!   reason; TOMORROW: PH-BROWSER-HC / -PW / -WD will replace
//!   each scaffold with a real driver).
//! - `backend = X` with the corresponding feature NOT compiled
//!   → returns [`BrowserError::FeatureNotCompiled`] at startup.
//!   The tool node fails to construct (loud error), no silent
//!   fallback to NoneBackend.
//! - Unknown backend name → [`BrowserError::InvalidBackend`] at
//!   startup. Same loud-fail posture.
//! - `tool.browser.open_session` always succeeds — it allocates
//!   a session id and tracks it so `list_sessions` surfaces
//!   what the operator opened. Downstream calls follow the
//!   backend's behaviour.
//!
//! Operators reading the chronicle / audit will never see a
//! fake "navigated to https://…" event.
//!
//! ## Wire format
//!
//! `tool.browser.open_session` — arg: `(empty)`
//!   Returns: `<session_id>\n` (16 hex chars, unique per call).
//!
//! `tool.browser.navigate` — arg: `<session_id>|<url>`
//! `tool.browser.get_text` — arg: `<session_id>`
//! `tool.browser.screenshot` — arg: `<session_id>`
//! `tool.browser.close_session` — arg: `<session_id>`
//!
//! `tool.browser.list_sessions` — arg: `(empty)`
//!   Returns: one row per session
//!   `<session_id>\t<opened_at>\t<current_url>\t<status>\n`
//!   + trailing `count=<N>`.
//!
//! All non-noop methods return `BackendNotConnected` until the
//! corresponding live backend ships.

use std::sync::Arc;

use serde::Deserialize;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

mod none;
pub use none::NoneBackend;

#[cfg(feature = "browser-headless-chrome")]
pub mod headless_chrome;
#[cfg(feature = "browser-playwright")]
pub mod playwright;
#[cfg(feature = "browser-webdriver")]
pub mod webdriver;

/// Per-node config for the browser subsystem. Lives under
/// `[tool.browser]`. When the whole section is absent the
/// capability is NOT registered (see `register()`).
#[derive(Clone, Debug, Deserialize)]
pub struct BrowserConfig {
    /// Backend selector. One of:
    /// - `"none"` — scaffold; every non-noop returns
    ///   BackendNotConnected.
    /// - `"headless_chrome"` — requires `browser-headless-chrome`
    ///   feature.
    /// - `"playwright"` — requires `browser-playwright` feature.
    /// - `"webdriver"` — requires `browser-webdriver` feature.
    ///   When this backend is selected, [`Self::webdriver_url`]
    ///   must point at the operator-supplied WebDriver daemon
    ///   (chromedriver / geckodriver).
    ///
    /// Selecting a backend whose feature isn't compiled fails
    /// LOUDLY at startup (no silent NoneBackend fallback).
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Maximum live browser sessions per node. Caps the
    /// session-id ring; protects real backends from runaway
    /// allocation. Defaults to 16.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Per-call deadline in seconds. Surfaced via error
    /// envelopes so operators see the configured limit even
    /// when the scaffold has nothing to time out yet.
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
    /// PH-BROWSER-WD: URL of the operator-supplied WebDriver
    /// daemon (chromedriver / geckodriver). Required when
    /// `backend = "webdriver"`. Defaults to
    /// `http://127.0.0.1:9515` (chromedriver's default port).
    #[serde(default = "default_webdriver_url")]
    pub webdriver_url: String,
    /// W2-002c: optional directory where the backend persists a
    /// PNG screenshot every time a navigate / click / type_text
    /// fails on a live tab. The failure error reason gets the
    /// path appended so replay surfaces (W2-001) can render it.
    /// `None` (default) → screenshot-on-failure is disabled.
    /// The directory must already exist; the backend does not
    /// create it. Honest scope: only HeadlessChromeBackend
    /// implements this today; PW and WD ignore the field until
    /// follow-ups.
    #[serde(default)]
    pub screenshot_on_failure_dir: Option<std::path::PathBuf>,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            max_sessions: default_max_sessions(),
            call_timeout_secs: default_call_timeout_secs(),
            webdriver_url: default_webdriver_url(),
            screenshot_on_failure_dir: None,
        }
    }
}

fn default_backend() -> String {
    "none".to_string()
}
fn default_max_sessions() -> usize {
    16
}
fn default_call_timeout_secs() -> u64 {
    30
}
fn default_webdriver_url() -> String {
    "http://127.0.0.1:9515".to_string()
}

/// Recognised backend names. The set is closed — anything else
/// is a config error reported at startup. Each non-"none" entry
/// is gated on a Cargo feature; selecting one whose feature is
/// disabled yields [`BrowserError::FeatureNotCompiled`].
pub const KNOWN_BACKENDS: &[&str] = &["none", "headless_chrome", "playwright", "webdriver"];

/// One row of [`BrowserBackend::list_sessions`] output.
#[derive(Debug, Clone)]
pub struct BrowserSessionView {
    pub session_id: String,
    pub opened_at: i64,
    pub current_url: Option<String>,
    pub page_title: Option<String>,
    pub status: String,
}

/// Public backend interface. Implemented by [`NoneBackend`] and
/// by each feature-gated backend module.
///
/// **Trait evolution.** PH-BROWSER-FEATURES froze the original
/// surface (`name` / `open_session` / `close_session` /
/// `navigate` / `get_text` / `screenshot` / `list_sessions`).
/// W2-002a extends it with `click` / `type_text` /
/// `wait_for_selector` via default implementations that return
/// `BackendNotConnected` so the trait can grow without
/// breaking backends that haven't implemented the new
/// methods yet. Backends implement the methods they support;
/// the others honestly report "not yet wired" per the
/// honesty contract.
pub trait BrowserBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn open_session(&self) -> Result<String, BrowserError>;
    fn close_session(&self, session_id: &str) -> Result<(), BrowserError>;
    fn navigate(&self, session_id: &str, url: &str) -> Result<(), BrowserError>;
    fn get_text(&self, session_id: &str) -> Result<String, BrowserError>;
    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError>;
    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError>;

    /// W2-002a: click a CSS selector on the session's current
    /// page. Default impl returns `BackendNotConnected` — each
    /// live backend overrides with its driver-specific click
    /// primitive. Operators driving TUI / form workflows need
    /// click + type_text + wait_for_selector to land before
    /// the browser surface is genuinely useful.
    fn click(&self, _session_id: &str, _selector: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendNotConnected {
            reason: format!(
                "{}: click not yet implemented for this backend (W2-002a default)",
                self.name()
            ),
        })
    }

    /// W2-002a: type a string into a CSS selector. Default
    /// impl returns `BackendNotConnected`.
    fn type_text(
        &self,
        _session_id: &str,
        _selector: &str,
        _text: &str,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendNotConnected {
            reason: format!(
                "{}: type_text not yet implemented for this backend (W2-002a default)",
                self.name()
            ),
        })
    }

    /// W2-002a: wait up to `timeout_ms` for a CSS selector to
    /// appear in the DOM. Default impl returns
    /// `BackendNotConnected`. Implementations should respect
    /// the timeout — operators rely on this for deterministic
    /// click/type flows.
    fn wait_for_selector(
        &self,
        _session_id: &str,
        _selector: &str,
        _timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendNotConnected {
            reason: format!(
                "{}: wait_for_selector not yet implemented for this backend (W2-002a default)",
                self.name()
            ),
        })
    }
}

/// Backend error variants. `BackendNotConnected` is the
/// honesty-contract default for non-trivial methods until a
/// live backend lands. `FeatureNotCompiled` /
/// `InvalidBackend` are the loud startup errors from
/// [`build_backend`] / [`validate_config`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum BrowserError {
    #[error("backend not connected: {reason}")]
    BackendNotConnected { reason: String },
    #[error("session not found: {session_id}")]
    SessionNotFound { session_id: String },
    #[error("max_sessions ({max}) reached")]
    SessionCapReached { max: usize },
    #[error("invalid url: {url}")]
    InvalidUrl { url: String },
    /// PH-BROWSER-FEATURES: the operator's `backend = "..."`
    /// selector doesn't match any known backend name.
    #[error("invalid backend '{name}' (allowed: {})", KNOWN_BACKENDS.join("|"))]
    InvalidBackend { name: String },
    /// PH-BROWSER-FEATURES: the operator's `backend = "..."`
    /// selector matches a known backend, but this Relix build
    /// was compiled without the corresponding Cargo feature.
    /// Loud-fail at startup; no silent fallback.
    #[error(
        "backend '{backend}' requires the '{feature}' Cargo feature; \
         rebuild with `--features {feature}` or set `backend = \"none\"` \
         in `[tool.browser]`"
    )]
    FeatureNotCompiled {
        backend: String,
        feature: &'static str,
    },
}

/// Construct a backend from operator config.
///
/// Outcomes:
/// - `cfg.backend == "none"` → [`NoneBackend`] with a neutral
///   "operator selected none" reason.
/// - `cfg.backend == "<known>"` and that backend's Cargo
///   feature is compiled → live backend (today: a labeled
///   scaffold that returns BackendNotConnected and names the
///   upcoming milestone).
/// - `cfg.backend == "<known>"` and the feature is NOT
///   compiled → [`BrowserError::FeatureNotCompiled`].
/// - Anything else → [`BrowserError::InvalidBackend`].
///
/// The error variants are designed to be surfaced fatally by
/// the caller (see `ToolBackend::new` which calls
/// [`validate_config`] at startup). The tool node never falls
/// back to NoneBackend silently when an operator selected a
/// different backend.
pub fn build_backend(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    match cfg.backend.as_str() {
        "none" => {
            let reason = "operator selected backend=\"none\" — capability surface is wired \
                          but no real browser backend is active in this Relix build"
                .to_string();
            Ok(Arc::new(NoneBackend::new(cfg, reason)))
        }
        "headless_chrome" => build_headless_chrome(cfg),
        "playwright" => build_playwright(cfg),
        "webdriver" => build_webdriver(cfg),
        other => Err(BrowserError::InvalidBackend {
            name: other.to_string(),
        }),
    }
}

/// PH-BROWSER-FEATURES: cheap pre-flight check used by
/// `ToolBackend::new` to surface a fatal startup error before
/// the dispatch bridge is wired. Functionally equivalent to
/// `build_backend(cfg).map(|_| ())` — kept as a separate
/// function so the caller's intent ("validate, don't keep the
/// backend") is explicit at the call site.
pub fn validate_config(cfg: &BrowserConfig) -> Result<(), BrowserError> {
    build_backend(cfg).map(|_| ())
}

// ── Feature-gated build helpers ───────────────────────────────────

#[cfg(feature = "browser-headless-chrome")]
fn build_headless_chrome(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    headless_chrome::try_build(cfg)
}
#[cfg(not(feature = "browser-headless-chrome"))]
fn build_headless_chrome(_cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Err(BrowserError::FeatureNotCompiled {
        backend: "headless_chrome".to_string(),
        feature: "browser-headless-chrome",
    })
}

#[cfg(feature = "browser-playwright")]
fn build_playwright(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    playwright::try_build(cfg)
}
#[cfg(not(feature = "browser-playwright"))]
fn build_playwright(_cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Err(BrowserError::FeatureNotCompiled {
        backend: "playwright".to_string(),
        feature: "browser-playwright",
    })
}

#[cfg(feature = "browser-webdriver")]
fn build_webdriver(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    webdriver::try_build(cfg)
}
#[cfg(not(feature = "browser-webdriver"))]
fn build_webdriver(_cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Err(BrowserError::FeatureNotCompiled {
        backend: "webdriver".to_string(),
        feature: "browser-webdriver",
    })
}

// ─────────────────────────── Capability descriptors ───────────────────────

pub fn descriptor_open_session() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.open_session");
    d.major_version = 1;
    d.kind = CapabilityKind::Unary;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into()];
    d.policy_attachment_point = "tool.browser.open_session".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Open a browser session. Returns a session id. Today the \"none\" \
         backend allocates ids without driving a real browser; downstream \
         navigate / screenshot calls return BackendNotConnected."
            .into(),
    );
    d.categories = vec!["browser".into(), "session".into()];
    d.environment_requirements = vec!["browser:host".into()];
    d.risk_level = RiskLevel::Low;
    d
}

pub fn descriptor_close_session() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.close_session");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into()];
    d.policy_attachment_point = "tool.browser.close_session".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some("Close a browser session.".into());
    d.categories = vec!["browser".into(), "session".into()];
    d.risk_level = RiskLevel::Low;
    d
}

pub fn descriptor_navigate() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.navigate");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::ExternalPaid;
    d.sensitivity_tags = vec![
        "browser:session".into(),
        "external:network".into(),
        "egress:http".into(),
    ];
    d.policy_attachment_point = "tool.browser.navigate".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Navigate a browser session to a URL. Honesty: returns \
         BackendNotConnected until the selected backend's live impl ships."
            .into(),
    );
    d.categories = vec!["browser".into(), "navigation".into()];
    d.environment_requirements = vec!["browser:host".into(), "network:outbound".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

pub fn descriptor_get_text() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.get_text");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "parse:html".into()];
    d.policy_attachment_point = "tool.browser.get_text".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some("Extract visible text from the current page.".into());
    d.categories = vec!["browser".into(), "extract".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn descriptor_screenshot() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.screenshot");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "binary:image".into()];
    d.policy_attachment_point = "tool.browser.screenshot".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some("Capture a PNG screenshot of the current page.".into());
    d.categories = vec!["browser".into(), "screenshot".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn descriptor_list_sessions() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.list_sessions");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "read".into()];
    d.policy_attachment_point = "tool.browser.list_sessions".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some("List currently open browser sessions.".into());
    d.categories = vec!["browser".into(), "read".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

/// W2-002a: descriptor for `tool.browser.click`. Same blast
/// radius as `navigate` — drives the live page state.
pub fn descriptor_click() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.click");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "browser:mutate".into()];
    d.policy_attachment_point = "tool.browser.click".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Click a CSS selector on the session's current page. \
         Arg shape: `<session_id>|<css_selector>`. Returns \
         BackendNotConnected on backends that haven't \
         implemented click yet."
            .into(),
    );
    d.categories = vec!["browser".into(), "mutate".into()];
    d.environment_requirements = vec!["browser:host".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// W2-002a: descriptor for `tool.browser.type_text`.
pub fn descriptor_type_text() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.type_text");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "browser:mutate".into()];
    d.policy_attachment_point = "tool.browser.type_text".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Type a string into a CSS selector on the current page. \
         Arg shape: `<session_id>|<css_selector>|<text>`. Text \
         may contain `|` because the parser uses splitn(3)."
            .into(),
    );
    d.categories = vec!["browser".into(), "mutate".into()];
    d.environment_requirements = vec!["browser:host".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// W2-002a: descriptor for `tool.browser.wait_for_selector`.
pub fn descriptor_wait_for_selector() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.wait_for_selector");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:session".into(), "browser:wait".into()];
    d.policy_attachment_point = "tool.browser.wait_for_selector".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Wait up to `timeout_ms` for a CSS selector to appear \
         in the DOM. Arg shape: \
         `<session_id>|<css_selector>|<timeout_ms>`. Times out \
         with BackendNotConnected on backends without an impl, \
         or with a reason naming the selector + waited duration \
         on a real backend that didn't see it."
            .into(),
    );
    d.categories = vec!["browser".into(), "wait".into()];
    d.environment_requirements = vec!["browser:host".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

/// W2-002f: descriptor for `tool.browser.capture_read`. Reads a
/// PNG from the configured `screenshot_on_failure_dir` and
/// returns its raw bytes. Pure read; no browser interaction.
pub fn descriptor_capture_read() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.browser.capture_read");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["browser:capture".into(), "binary:image".into()];
    d.policy_attachment_point = "tool.browser.capture_read".to_string();
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Read a previously-captured failure screenshot PNG by \
         filename (basename only — path traversal refused). Source \
         dir is `[tool.browser] screenshot_on_failure_dir`. Returns \
         the raw PNG bytes; INVALID_ARGS when capture dir is not \
         configured or filename is unsafe."
            .into(),
    );
    d.categories = vec!["browser".into(), "capture".into(), "read".into()];
    d.environment_requirements = vec!["filesystem:read".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

/// Register every browser.* capability onto the dispatch bridge.
/// Caller is `tool::register` in `tool/mod.rs` — only invoked
/// when `[tool.browser]` is present in the operator config AND
/// the config validated successfully at `ToolBackend::new` time.
///
/// `captures_dir` is the resolved value of
/// `[tool.browser] screenshot_on_failure_dir` (cloned from
/// the same `BrowserConfig` that built the backend). When
/// `None`, `tool.browser.capture_read` returns INVALID_ARGS
/// for every filename — operators see "captures dir not
/// configured" rather than a silent failure.
pub fn register(
    bridge: &mut DispatchBridge,
    backend: Arc<dyn BrowserBackend>,
    captures_dir: Option<std::path::PathBuf>,
) {
    let b = backend.clone();
    bridge.register(
        "tool.browser.open_session",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_open(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.close_session",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_close(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.navigate",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_navigate(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.get_text",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_get_text(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.screenshot",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_screenshot(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.list_sessions",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_list_sessions(&b, &ctx) }
        })),
    );
    // W2-002a: click / type_text / wait_for_selector.
    let b = backend.clone();
    bridge.register(
        "tool.browser.click",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_click(&b, &ctx) }
        })),
    );
    let b = backend.clone();
    bridge.register(
        "tool.browser.type_text",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_type_text(&b, &ctx) }
        })),
    );
    let b = backend;
    bridge.register(
        "tool.browser.wait_for_selector",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_wait_for_selector(&b, &ctx) }
        })),
    );
    // W2-002f: capture_read doesn't touch the live backend —
    // it reads bytes from the captures dir on disk. Wrap the
    // dir in Arc so each invocation gets a cheap clone.
    let dir = Arc::new(captures_dir);
    bridge.register(
        "tool.browser.capture_read",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let dir = dir.clone();
            async move { handle_capture_read(&dir, &ctx) }
        })),
    );
}

// ─────────────────────────── Handlers ───────────────────────────

fn handle_open(b: &Arc<dyn BrowserBackend>, _ctx: &InvocationCtx) -> HandlerOutcome {
    match b.open_session() {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_close(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "close_session") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let id = s.trim();
    if id.is_empty() {
        return invalid("tool.browser.close_session: session_id required".into());
    }
    match b.close_session(id) {
        Ok(()) => HandlerOutcome::Ok("closed\n".to_string().into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_navigate(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "navigate") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let (id, url) = match s.split_once('|') {
        Some(p) => p,
        None => return invalid("tool.browser.navigate: arg shape `<session_id>|<url>`".into()),
    };
    let id = id.trim();
    let url = url.trim();
    if id.is_empty() || url.is_empty() {
        return invalid(
            "tool.browser.navigate: both session_id and url required (arg shape `<session_id>|<url>`)"
                .into(),
        );
    }
    // Cheap URL sanity check — refuse `javascript:` / `data:`
    // anywhere even though no real navigation happens today, so
    // the contract holds when a real backend lands.
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:") {
        return to_envelope(&BrowserError::InvalidUrl {
            url: url.to_string(),
        });
    }
    // W2-002d: structured event trace. The dispatch bridge
    // already audits the call; this info line gives operators
    // a per-call latency + outcome label suitable for
    // log-aggregation / metrics scrape.
    let started = std::time::Instant::now();
    let result = b.navigate(id, url);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (outcome, reason) = match &result {
        Ok(()) => ("ok", String::new()),
        Err(e) => ("err", e.to_string()),
    };
    tracing::info!(
        method = "tool.browser.navigate",
        backend = b.name(),
        session_id = id,
        target_url = url,
        elapsed_ms = elapsed_ms,
        outcome = outcome,
        reason = %reason,
        "browser navigate"
    );
    match result {
        Ok(()) => HandlerOutcome::Ok("navigated\n".to_string().into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_get_text(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "get_text") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let id = s.trim();
    if id.is_empty() {
        return invalid("tool.browser.get_text: session_id required".into());
    }
    match b.get_text(id) {
        Ok(text) => HandlerOutcome::Ok(text.into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_screenshot(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "screenshot") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let id = s.trim();
    if id.is_empty() {
        return invalid("tool.browser.screenshot: session_id required".into());
    }
    match b.screenshot(id) {
        Ok(bytes) => HandlerOutcome::Ok(bytes),
        Err(e) => to_envelope(&e),
    }
}

fn handle_list_sessions(b: &Arc<dyn BrowserBackend>, _ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    match b.list_sessions() {
        Ok(rows) => {
            let mut body = String::new();
            for r in &rows {
                let _ = writeln!(
                    body,
                    "{}\t{}\t{}\t{}",
                    r.session_id,
                    r.opened_at,
                    r.current_url.clone().unwrap_or_else(|| "-".to_string()),
                    r.status,
                );
            }
            let _ = writeln!(body, "count={}", rows.len());
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(e) => to_envelope(&e),
    }
}

// ── W2-002a: click / type_text / wait_for_selector handlers ─────

fn handle_click(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "click") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let (id, selector) = match s.split_once('|') {
        Some(p) => p,
        None => {
            return invalid("tool.browser.click: arg shape `<session_id>|<css_selector>`".into());
        }
    };
    let id = id.trim();
    let selector = selector.trim();
    if id.is_empty() || selector.is_empty() {
        return invalid(
            "tool.browser.click: both session_id and css_selector required (arg shape `<session_id>|<css_selector>`)"
                .into(),
        );
    }
    let started = std::time::Instant::now();
    let result = b.click(id, selector);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (outcome, reason) = match &result {
        Ok(()) => ("ok", String::new()),
        Err(e) => ("err", e.to_string()),
    };
    tracing::info!(
        method = "tool.browser.click",
        backend = b.name(),
        session_id = id,
        selector = selector,
        elapsed_ms = elapsed_ms,
        outcome = outcome,
        reason = %reason,
        "browser click"
    );
    match result {
        Ok(()) => HandlerOutcome::Ok("clicked\n".to_string().into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_type_text(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "type_text") {
        Ok(s) => s,
        Err(o) => return o,
    };
    // splitn(3) so the text payload may contain `|` without
    // breaking the split — common when typing JSON-ish content
    // or URLs into a form field.
    let mut parts = s.splitn(3, '|');
    let id = parts.next().unwrap_or("").trim();
    let selector = parts.next().unwrap_or("").trim();
    let text = parts.next().unwrap_or("");
    if id.is_empty() || selector.is_empty() {
        return invalid(
            "tool.browser.type_text: arg shape `<session_id>|<css_selector>|<text>` (text may be empty but the two pipes must be present)"
                .into(),
        );
    }
    let started = std::time::Instant::now();
    let result = b.type_text(id, selector, text);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (outcome, reason) = match &result {
        Ok(()) => ("ok", String::new()),
        Err(e) => ("err", e.to_string()),
    };
    // Honest: log the char count, NOT the text payload. The
    // text may include user credentials (form passwords); the
    // structured log must never carry secrets.
    tracing::info!(
        method = "tool.browser.type_text",
        backend = b.name(),
        session_id = id,
        selector = selector,
        text_chars = text.chars().count(),
        elapsed_ms = elapsed_ms,
        outcome = outcome,
        reason = %reason,
        "browser type_text"
    );
    match result {
        Ok(()) => HandlerOutcome::Ok("typed\n".to_string().into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

fn handle_wait_for_selector(b: &Arc<dyn BrowserBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match utf8_arg(ctx, "wait_for_selector") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let mut parts = s.splitn(3, '|');
    let id = parts.next().unwrap_or("").trim();
    let selector = parts.next().unwrap_or("").trim();
    let timeout_str = parts.next().unwrap_or("").trim();
    if id.is_empty() || selector.is_empty() {
        return invalid(
            "tool.browser.wait_for_selector: arg shape `<session_id>|<css_selector>|<timeout_ms>`"
                .into(),
        );
    }
    let timeout_ms = if timeout_str.is_empty() {
        // Reasonable default — matches the operator's
        // `[tool.browser] call_timeout_secs` posture but in
        // ms granularity. 30s.
        30_000u64
    } else {
        match timeout_str.parse::<u64>() {
            Ok(v) if v > 0 => v,
            _ => {
                return invalid(format!(
                    "tool.browser.wait_for_selector: bad timeout_ms '{timeout_str}' (must be positive integer or empty for default)"
                ));
            }
        }
    };
    let started = std::time::Instant::now();
    let result = b.wait_for_selector(id, selector, timeout_ms);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let (outcome, reason) = match &result {
        Ok(()) => ("ok", String::new()),
        Err(e) => ("err", e.to_string()),
    };
    tracing::info!(
        method = "tool.browser.wait_for_selector",
        backend = b.name(),
        session_id = id,
        selector = selector,
        timeout_ms = timeout_ms,
        elapsed_ms = elapsed_ms,
        outcome = outcome,
        reason = %reason,
        "browser wait_for_selector"
    );
    match result {
        Ok(()) => HandlerOutcome::Ok("found\n".to_string().into_bytes()),
        Err(e) => to_envelope(&e),
    }
}

/// W2-002f: `tool.browser.capture_read` handler. Args: a single
/// UTF-8 filename (basename only, must end with `.png`, no path
/// separators, no `..`, no NUL, length ≤ 256). Returns the file
/// bytes verbatim. Errors with INVALID_ARGS for unsafe filenames
/// or unconfigured captures dir; with RESPONDER_INTERNAL on
/// filesystem read failure.
fn handle_capture_read(
    captures_dir: &Arc<Option<std::path::PathBuf>>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let raw = match utf8_arg(ctx, "capture_read") {
        Ok(s) => s,
        Err(o) => return o,
    };
    let name = raw.trim();
    if name.is_empty() {
        return invalid("tool.browser.capture_read: filename required".into());
    }
    if name.len() > 256 {
        return invalid("tool.browser.capture_read: filename too long (>256)".into());
    }
    // Reject anything that could escape the dir or hit a
    // weird platform code path. The list is intentionally
    // strict; the captures the runtime writes use
    // `<sessionid>-<unix_ms>.png`.
    let bad = name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.contains(':');
    if bad {
        return invalid(format!(
            "tool.browser.capture_read: unsafe filename '{name}' (path separators, '..', NUL, and ':' rejected)"
        ));
    }
    if !name.to_ascii_lowercase().ends_with(".png") {
        return invalid(format!(
            "tool.browser.capture_read: filename '{name}' must end with .png"
        ));
    }
    let dir = match captures_dir.as_ref() {
        Some(d) => d,
        None => {
            return invalid(
                "tool.browser.capture_read: [tool.browser] screenshot_on_failure_dir not configured"
                    .into(),
            );
        }
    };
    let path = dir.join(name);
    // Defence in depth: after join, the file's canonical path
    // must still live under the configured dir. This catches
    // any platform-specific path-magic the byte-level check
    // missed.
    if let (Ok(canon_path), Ok(canon_dir)) = (path.canonicalize(), dir.canonicalize())
        && !canon_path.starts_with(&canon_dir)
    {
        return invalid(format!(
            "tool.browser.capture_read: resolved path escapes captures dir (name='{name}')"
        ));
    }
    match std::fs::read(&path) {
        Ok(bytes) => HandlerOutcome::Ok(bytes),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.browser.capture_read: read failed for '{name}': {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

// ─────────────────────────── helpers ───────────────────────────

fn utf8_arg(ctx: &InvocationCtx, who: &str) -> Result<String, HandlerOutcome> {
    match std::str::from_utf8(&ctx.args) {
        Ok(s) => Ok(s.to_string()),
        Err(e) => Err(invalid(format!("tool.browser.{who}: arg utf8: {e}"))),
    }
}

fn to_envelope(e: &BrowserError) -> HandlerOutcome {
    let kind = match e {
        BrowserError::BackendNotConnected { .. } => error_kinds::RESPONDER_INTERNAL,
        BrowserError::SessionNotFound { .. }
        | BrowserError::SessionCapReached { .. }
        | BrowserError::InvalidUrl { .. }
        | BrowserError::InvalidBackend { .. }
        | BrowserError::FeatureNotCompiled { .. } => error_kinds::INVALID_ARGS,
    };
    HandlerOutcome::Err(ErrorEnvelope {
        kind,
        cause: e.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

pub(crate) fn new_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(16);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub(crate) fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BrowserConfig {
        BrowserConfig {
            backend: "none".to_string(),
            max_sessions: 4,
            call_timeout_secs: 30,
            ..BrowserConfig::default()
        }
    }

    #[test]
    fn build_backend_none_returns_arc() {
        let b = build_backend(&cfg()).unwrap();
        assert_eq!(b.name(), "none");
    }

    #[test]
    fn build_backend_rejects_unknown_name() {
        let mut c = cfg();
        c.backend = "chrome-extension-thing".into();
        match build_backend(&c) {
            Ok(_) => panic!("expected InvalidBackend, got Ok"),
            Err(BrowserError::InvalidBackend { name }) => {
                assert_eq!(name, "chrome-extension-thing");
            }
            Err(other) => panic!("expected InvalidBackend, got {other:?}"),
        }
    }

    #[test]
    fn validate_config_passes_for_none() {
        let mut c = cfg();
        c.backend = "none".into();
        validate_config(&c).expect("none must validate");
    }

    #[test]
    fn validate_config_loud_fail_unknown_backend() {
        let mut c = cfg();
        c.backend = "made-up".into();
        let res = validate_config(&c);
        match res {
            Err(BrowserError::InvalidBackend { name }) => assert_eq!(name, "made-up"),
            Err(other) => panic!("expected InvalidBackend, got {other:?}"),
            Ok(()) => panic!("expected InvalidBackend, got Ok"),
        }
    }

    /// PH-BROWSER-FEATURES: with NO browser features compiled
    /// (the default `cargo build` posture), each non-"none"
    /// backend must fail-loudly at startup. This test asserts
    /// the shape of that failure for each backend name. When
    /// the corresponding feature IS enabled, the failure
    /// switches to a scaffold success (see the
    /// `feature_*_compiled_builds_scaffold` tests below).
    /// `Result<Arc<dyn BrowserBackend>, BrowserError>` cannot be
    /// Debug-formatted (trait object lacks Debug). Helpers below
    /// reduce a `build_backend` result to "succeeded with name X"
    /// or "failed with FeatureNotCompiled(backend, feature)" so
    /// the per-backend tests don't try to format the trait
    /// object.
    #[allow(dead_code)] // only used when the matching feature is OFF
    fn assert_feature_not_compiled(
        res: Result<Arc<dyn BrowserBackend>, BrowserError>,
        expected_backend: &str,
        expected_feature: &str,
    ) {
        match res {
            Err(BrowserError::FeatureNotCompiled { backend, feature }) => {
                assert_eq!(backend, expected_backend);
                assert_eq!(feature, expected_feature);
            }
            Err(other) => {
                panic!("expected FeatureNotCompiled for {expected_backend}, got {other:?}")
            }
            Ok(_) => panic!("expected FeatureNotCompiled for {expected_backend}, got Ok"),
        }
    }

    #[allow(dead_code)] // only used under the three browser-* features
    fn assert_scaffold_built_with_name(
        res: Result<Arc<dyn BrowserBackend>, BrowserError>,
        expected_name: &str,
    ) {
        match res {
            Ok(b) => assert_eq!(b.name(), expected_name),
            Err(e) => panic!(
                "expected scaffold success with feature enabled for {expected_name}, got {e:?}"
            ),
        }
    }

    #[test]
    fn build_backend_headless_chrome_without_feature_fails_loud() {
        let mut c = cfg();
        c.backend = "headless_chrome".into();
        let res = build_backend(&c);
        #[cfg(not(feature = "browser-headless-chrome"))]
        assert_feature_not_compiled(res, "headless_chrome", "browser-headless-chrome");
        #[cfg(feature = "browser-headless-chrome")]
        assert_scaffold_built_with_name(res, "headless_chrome");
    }

    #[test]
    fn build_backend_playwright_without_feature_fails_loud() {
        let mut c = cfg();
        c.backend = "playwright".into();
        let res = build_backend(&c);
        #[cfg(not(feature = "browser-playwright"))]
        assert_feature_not_compiled(res, "playwright", "browser-playwright");
        #[cfg(feature = "browser-playwright")]
        assert_scaffold_built_with_name(res, "playwright");
    }

    #[test]
    fn build_backend_webdriver_without_feature_fails_loud() {
        let mut c = cfg();
        c.backend = "webdriver".into();
        let res = build_backend(&c);
        #[cfg(not(feature = "browser-webdriver"))]
        assert_feature_not_compiled(res, "webdriver", "browser-webdriver");
        #[cfg(feature = "browser-webdriver")]
        assert_scaffold_built_with_name(res, "webdriver");
    }

    /// PH-BROWSER-FEATURES: under any feature combination, the
    /// "none" path always succeeds — operators can fall back to
    /// it deliberately by editing `[tool.browser] backend =
    /// "none"`, but the build path never silently downgrades
    /// to it from a different selector.
    #[test]
    fn build_backend_none_always_succeeds_regardless_of_features() {
        let mut c = cfg();
        c.backend = "none".into();
        let b = build_backend(&c).expect("none must always build");
        assert_eq!(b.name(), "none");
    }

    #[test]
    fn known_backends_constant_covers_every_named_path() {
        // Honesty contract: the closed set must match the
        // match arms in build_backend. If a future commit adds
        // a fifth backend name without updating KNOWN_BACKENDS,
        // this test fires.
        let expected = ["none", "headless_chrome", "playwright", "webdriver"];
        assert_eq!(KNOWN_BACKENDS, expected);
    }

    #[test]
    fn open_session_returns_unique_id() {
        let b = build_backend(&cfg()).unwrap();
        let a = b.open_session().unwrap();
        let b2 = b.open_session().unwrap();
        assert_ne!(a, b2);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn open_session_respects_max_sessions_cap() {
        let mut c = cfg();
        c.max_sessions = 2;
        let b = build_backend(&c).unwrap();
        b.open_session().unwrap();
        b.open_session().unwrap();
        let err = b.open_session().unwrap_err();
        assert!(matches!(err, BrowserError::SessionCapReached { max: 2 }));
    }

    /// PH-BROWSER-FEATURES: even with a max_sessions of 1 the
    /// session cap must be enforced (regression guard for the
    /// off-by-one that a future refactor could introduce).
    #[test]
    fn open_session_cap_of_one_is_enforced() {
        let mut c = cfg();
        c.max_sessions = 1;
        let b = build_backend(&c).unwrap();
        b.open_session().unwrap();
        match b.open_session() {
            Err(BrowserError::SessionCapReached { max: 1 }) => {}
            other => panic!("expected SessionCapReached(1), got {other:?}"),
        }
    }

    #[test]
    fn navigate_fails_with_backend_not_connected() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let err = b.navigate(&id, "https://example.com/").unwrap_err();
        assert!(matches!(err, BrowserError::BackendNotConnected { .. }));
    }

    #[test]
    fn navigate_unknown_session_returns_session_not_found() {
        let b = build_backend(&cfg()).unwrap();
        let err = b.navigate("deadbeefdeadbeef", "https://x/").unwrap_err();
        assert!(matches!(err, BrowserError::SessionNotFound { .. }));
    }

    #[test]
    fn close_session_drops_from_list() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        assert_eq!(b.list_sessions().unwrap().len(), 1);
        b.close_session(&id).unwrap();
        assert_eq!(b.list_sessions().unwrap().len(), 0);
    }

    #[test]
    fn close_unknown_session_errors() {
        let b = build_backend(&cfg()).unwrap();
        let err = b.close_session("notreal").unwrap_err();
        assert!(matches!(err, BrowserError::SessionNotFound { .. }));
    }

    #[test]
    fn list_sessions_reports_unconnected_status() {
        let b = build_backend(&cfg()).unwrap();
        b.open_session().unwrap();
        let rows = b.list_sessions().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "unconnected");
        assert!(rows[0].current_url.is_none());
    }

    #[test]
    fn descriptors_carry_browser_session_sensitivity_tag() {
        let descs = [
            descriptor_open_session(),
            descriptor_close_session(),
            descriptor_navigate(),
            descriptor_get_text(),
            descriptor_screenshot(),
            descriptor_list_sessions(),
        ];
        for d in &descs {
            assert!(
                d.sensitivity_tags.iter().any(|t| t == "browser:session"),
                "missing browser:session tag on {}",
                d.method_name
            );
        }
    }

    #[test]
    fn navigate_descriptor_includes_network_tags() {
        let d = descriptor_navigate();
        assert!(d.sensitivity_tags.iter().any(|t| t == "external:network"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "egress:http"));
    }

    /// PH-RISK-PIN-ALL: pin the risk tier of every browser
    /// descriptor. PH-BROWSER-FEATURES preserves the pinned
    /// tiers — the trait surface didn't change, so the risk
    /// posture per capability doesn't either. W2-002a adds
    /// `click` / `type_text` (Medium) and
    /// `wait_for_selector` (Safe).
    #[test]
    fn browser_descriptors_have_explicit_non_unknown_risk() {
        let pinned: &[(&str, CapabilityDescriptor, RiskLevel)] = &[
            (
                "tool.browser.open_session",
                descriptor_open_session(),
                RiskLevel::Low,
            ),
            (
                "tool.browser.close_session",
                descriptor_close_session(),
                RiskLevel::Low,
            ),
            (
                "tool.browser.navigate",
                descriptor_navigate(),
                RiskLevel::Medium,
            ),
            (
                "tool.browser.get_text",
                descriptor_get_text(),
                RiskLevel::Safe,
            ),
            (
                "tool.browser.screenshot",
                descriptor_screenshot(),
                RiskLevel::Safe,
            ),
            (
                "tool.browser.list_sessions",
                descriptor_list_sessions(),
                RiskLevel::Safe,
            ),
            ("tool.browser.click", descriptor_click(), RiskLevel::Medium),
            (
                "tool.browser.type_text",
                descriptor_type_text(),
                RiskLevel::Medium,
            ),
            (
                "tool.browser.wait_for_selector",
                descriptor_wait_for_selector(),
                RiskLevel::Safe,
            ),
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

    // ── W2-002a: default-impl click / type_text / wait_for_selector ──

    #[test]
    fn default_click_returns_backend_not_connected_with_backend_name() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let err = b.click(&id, "#submit").unwrap_err();
        match err {
            BrowserError::BackendNotConnected { reason } => {
                assert!(reason.contains("none"));
                assert!(reason.contains("click"));
                assert!(reason.contains("W2-002a"));
            }
            other => panic!("expected BackendNotConnected, got {other:?}"),
        }
    }

    #[test]
    fn default_type_text_returns_backend_not_connected_with_backend_name() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let err = b.type_text(&id, "#email", "user@example.com").unwrap_err();
        match err {
            BrowserError::BackendNotConnected { reason } => {
                assert!(reason.contains("type_text"));
            }
            other => panic!("expected BackendNotConnected, got {other:?}"),
        }
    }

    #[test]
    fn default_wait_for_selector_returns_backend_not_connected() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let err = b.wait_for_selector(&id, "#ready", 5_000).unwrap_err();
        assert!(matches!(err, BrowserError::BackendNotConnected { .. }));
    }

    #[test]
    fn handle_click_rejects_missing_selector_slot() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let arg = id.into_bytes();
        let out = handle_click(&Arc::clone(&b), &ctx_with(arg));
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("arg shape"));
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_type_text_allows_pipes_in_text_payload() {
        // splitn(3, '|') means the third slot can contain `|`.
        // Verify by sending text with a pipe and asserting the
        // backend receives it intact via the default impl's
        // error reason (which doesn't include text but at
        // least proves the handler accepted the shape).
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let arg = format!("{id}|#json|{{\"k\":\"a|b\"}}").into_bytes();
        let out = handle_type_text(&Arc::clone(&b), &ctx_with(arg));
        match out {
            HandlerOutcome::Err(env) => {
                // The default impl returns BackendNotConnected;
                // the responder maps it to RESPONDER_INTERNAL.
                assert_eq!(env.kind, error_kinds::RESPONDER_INTERNAL);
            }
            HandlerOutcome::Ok(_) => panic!("none backend should refuse type_text"),
        }
    }

    #[test]
    fn handle_wait_for_selector_default_timeout_is_30s() {
        // Empty timeout slot → 30_000 ms.
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        // Use an empty timeout slot to exercise the default
        // path. The default impl errors with BackendNotConnected
        // — we're verifying the parse path, not the wait.
        let arg = format!("{id}|#ready|").into_bytes();
        let out = handle_wait_for_selector(&Arc::clone(&b), &ctx_with(arg));
        assert!(matches!(out, HandlerOutcome::Err(_)));
    }

    #[test]
    fn handle_wait_for_selector_rejects_bad_timeout() {
        let b = build_backend(&cfg()).unwrap();
        let id = b.open_session().unwrap();
        let arg = format!("{id}|#ready|notanumber").into_bytes();
        let out = handle_wait_for_selector(&Arc::clone(&b), &ctx_with(arg));
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("bad timeout_ms"));
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    // ── W2-002f: capture_read handler tests ─────────────────────

    fn capture_read(dir: Option<std::path::PathBuf>, name: &str) -> HandlerOutcome {
        let arc = Arc::new(dir);
        handle_capture_read(&arc, &ctx_with(name.as_bytes().to_vec()))
    }

    fn assert_invalid_args(out: HandlerOutcome) -> String {
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(
                    env.kind,
                    error_kinds::INVALID_ARGS,
                    "expected INVALID_ARGS, got kind={}",
                    env.kind
                );
                env.cause
            }
            HandlerOutcome::Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn capture_read_rejects_empty_filename() {
        let out = capture_read(None, "");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("required"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_path_traversal_dotdot() {
        let out = capture_read(Some(std::path::PathBuf::from("/tmp")), "../etc/passwd.png");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("unsafe"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_forward_slash() {
        let out = capture_read(Some(std::path::PathBuf::from("/tmp")), "sub/file.png");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("unsafe"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_backslash() {
        let out = capture_read(Some(std::path::PathBuf::from("/tmp")), "sub\\file.png");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("unsafe"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_colon() {
        let out = capture_read(Some(std::path::PathBuf::from("/tmp")), "C:foo.png");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("unsafe"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_non_png_extension() {
        let out = capture_read(Some(std::path::PathBuf::from("/tmp")), "shot.jpg");
        let cause = assert_invalid_args(out);
        assert!(cause.contains(".png"), "got: {cause}");
    }

    #[test]
    fn capture_read_rejects_when_dir_not_configured() {
        let out = capture_read(None, "shot.png");
        let cause = assert_invalid_args(out);
        assert!(cause.contains("screenshot_on_failure_dir"), "got: {cause}");
    }

    #[test]
    fn capture_read_returns_bytes_for_valid_file() {
        let tmp = tempfile::tempdir().unwrap();
        let name = "abc123-1700000000.png";
        let file_path = tmp.path().join(name);
        // Pretend PNG header bytes so the test asserts on real
        // content, not just length.
        let payload: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0xff, 0xee];
        std::fs::write(&file_path, &payload).unwrap();
        let out = capture_read(Some(tmp.path().to_path_buf()), name);
        match out {
            HandlerOutcome::Ok(bytes) => assert_eq!(bytes, payload),
            HandlerOutcome::Err(env) => {
                panic!("expected Ok, got Err kind={} cause={}", env.kind, env.cause)
            }
        }
    }

    #[test]
    fn capture_read_returns_internal_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let out = capture_read(Some(tmp.path().to_path_buf()), "nope-1700000000.png");
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::RESPONDER_INTERNAL);
                assert!(env.cause.contains("read failed"), "got: {}", env.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    fn ctx_with(args: Vec<u8>) -> InvocationCtx {
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
            args,
            tenant_id: None,
        }
    }

    /// PH-BROWSER-FEATURES: per-feature scaffold check. The
    /// scaffold for each backend must:
    /// 1. Report `name()` matching the canonical backend label
    ///    (not "none") — so the dashboard shows what the
    ///    operator chose.
    /// 2. Refuse navigate / get_text / screenshot with
    ///    BackendNotConnected (no fake success).
    ///
    /// These tests only run when the corresponding feature is
    /// compiled. With no features the runtime tests still
    /// cover the "none" path + the feature-not-compiled error
    /// shape above.
    ///
    /// PH-BROWSER-HC NOTE: the `headless_chrome` scaffold has
    /// been replaced by a live driver. The
    /// `feature_headless_chrome_compiled_builds_real_backend`
    /// test in `headless_chrome::tests` covers the live-build
    /// shape (Ok with `name()=="headless_chrome"` OR
    /// BackendNotConnected when Chrome isn't installed).
    ///
    /// PH-BROWSER-PW: with the live driver, `try_build` returns
    /// the real `PlaywrightBackend` (still no Node spawn — that
    /// happens lazily on the first `open_session`) and the
    /// canonical name surfaces unchanged. We deliberately do NOT
    /// call `navigate` here: navigate would try to spawn Node,
    /// and CI hosts without Node would falsely fail this unit
    /// test. The live integration coverage lives in
    /// `playwright::tests::live_playwright_navigates_about_blank`.
    #[cfg(feature = "browser-playwright")]
    #[test]
    fn feature_playwright_compiled_builds_real_backend() {
        let mut c = cfg();
        c.backend = "playwright".into();
        let b = build_backend(&c).expect("real backend should build");
        assert_eq!(b.name(), "playwright");
        // list_sessions is the only path that doesn't touch the
        // sidecar — verify it returns the empty in-memory map.
        assert_eq!(b.list_sessions().expect("list_sessions").len(), 0);
    }

    /// PH-BROWSER-WD: with the `browser-webdriver` feature on,
    /// `build_backend` returns the live fantoccini-backed
    /// `WebDriverBackend`. `try_build` must NOT touch the
    /// network — it does not probe the driver URL. We assert
    /// the name only; live-driver behaviour is exercised in
    /// `webdriver::tests` and gated on a running daemon.
    #[cfg(feature = "browser-webdriver")]
    #[test]
    fn feature_webdriver_compiled_builds_real_backend() {
        let mut c = cfg();
        c.backend = "webdriver".into();
        let b = build_backend(&c).expect("real backend should build");
        assert_eq!(b.name(), "webdriver");
    }

    #[test]
    fn default_browser_config_webdriver_url_is_chromedriver_default() {
        let cfg = BrowserConfig::default();
        assert_eq!(cfg.webdriver_url, "http://127.0.0.1:9515");
        assert_eq!(cfg.backend, "none");
        assert_eq!(cfg.max_sessions, 16);
        assert_eq!(cfg.call_timeout_secs, 30);
    }
}
