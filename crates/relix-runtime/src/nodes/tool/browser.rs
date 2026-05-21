//! CW4 — Browser-automation capability foundation.
//!
//! Hermes ships `browser_tool` (Playwright + CDP + Camofox) for
//! full headless-browser automation. Relix's CW4 foundation
//! lands the **honest scaffold**: capability descriptors,
//! session model, wire format, dispatch, error envelope, and
//! dashboard / CLI visibility — but does NOT yet ship a real
//! browser backend. Operators get a working surface that
//! advertises the gap clearly.
//!
//! ## Honesty contract
//!
//! Per the user's CW4 directive: *"If no actual browser backend
//! exists yet, do NOT fake browser execution. Create real
//! contracts and explicit backend-missing errors. No mock
//! success."*
//!
//! Concrete posture:
//!
//! - `[tool.browser] backend = "none"` (default when the
//!   section is present at all) makes every navigate /
//!   get_text / screenshot call return a typed
//!   `BackendNotConnected` error.
//! - `[tool.browser] backend = "playwright"` is reserved for a
//!   future milestone. Today selecting it returns the same
//!   `BackendNotConnected` error with a `reason` field hinting
//!   the integration is pending.
//! - `tool.browser.open_session` always succeeds in `"none"`
//!   mode — it allocates a session id, stores nothing, and
//!   lets the operator see the capability is wired. Subsequent
//!   navigate / screenshot calls against that id fail loudly.
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
//! All non-noop methods return `BackendNotConnected` until a
//! real backend ships.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node config for the browser subsystem. Lives under
/// `[tool.browser]`. When the whole section is absent the
/// capability is NOT registered (see `register()`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct BrowserConfig {
    /// Backend selector. `"none"` (the default when the section
    /// is present) wires the capability surface but every
    /// non-noop method returns BackendNotConnected. `"playwright"`
    /// is reserved for a future milestone; today selecting it
    /// returns BackendNotConnected with a different `reason`.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Maximum live browser sessions per node. Caps the
    /// session-id ring; protects future real backends from
    /// runaway allocation. Defaults to 16.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// Per-call deadline in seconds. Today returned as part of
    /// every error envelope so operators see the configured
    /// limit even though no real call ever times out yet.
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
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

/// Recognised backend names. Anything else is a config error
/// reported at startup.
const KNOWN_BACKENDS: &[&str] = &["none", "playwright"];

/// One row of [`BrowserBackend::list_sessions`] output. The
/// honesty contract above means most fields are `None` /
/// `"unconnected"` until a real backend ships.
#[derive(Debug, Clone)]
pub struct BrowserSessionView {
    pub session_id: String,
    pub opened_at: i64,
    pub current_url: Option<String>,
    pub page_title: Option<String>,
    pub status: String,
}

/// Public backend interface. The `"none"` backend ([`NoneBackend`])
/// implements all four mutating methods as `BackendNotConnected`
/// — see module docs.
pub trait BrowserBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn open_session(&self) -> Result<String, BrowserError>;
    fn close_session(&self, session_id: &str) -> Result<(), BrowserError>;
    fn navigate(&self, session_id: &str, url: &str) -> Result<(), BrowserError>;
    fn get_text(&self, session_id: &str) -> Result<String, BrowserError>;
    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError>;
    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError>;
}

/// Backend error variants. `BackendNotConnected` is the
/// honesty-contract default for every non-trivial method
/// until a real backend lands.
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
    #[error("invalid backend '{name}' (allowed: none|playwright)")]
    InvalidBackend { name: String },
}

/// The shipped backend. Allocates session ids on `open_session`
/// (and tracks them in a small in-memory map so `list_sessions`
/// surfaces what the operator opened) but refuses every
/// downstream navigate / get_text / screenshot call with a
/// `BackendNotConnected` error. Honest scaffold; future
/// `PlaywrightBackend` will satisfy the same trait.
pub struct NoneBackend {
    max_sessions: usize,
    sessions: Mutex<HashMap<String, NoneSession>>,
    reason: String,
}

#[derive(Debug, Clone)]
struct NoneSession {
    opened_at: i64,
}

impl NoneBackend {
    pub fn new(cfg: &BrowserConfig, reason: impl Into<String>) -> Self {
        Self {
            max_sessions: cfg.max_sessions,
            sessions: Mutex::new(HashMap::new()),
            reason: reason.into(),
        }
    }
}

impl BrowserBackend for NoneBackend {
    fn name(&self) -> &'static str {
        "none"
    }

    fn open_session(&self) -> Result<String, BrowserError> {
        let mut guard = self.sessions.lock().expect("none backend lock");
        if guard.len() >= self.max_sessions {
            return Err(BrowserError::SessionCapReached {
                max: self.max_sessions,
            });
        }
        let id = new_session_id();
        guard.insert(
            id.clone(),
            NoneSession {
                opened_at: unix_secs(),
            },
        );
        Ok(id)
    }

    fn close_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let mut guard = self.sessions.lock().expect("none backend lock");
        guard
            .remove(session_id)
            .map(|_| ())
            .ok_or(BrowserError::SessionNotFound {
                session_id: session_id.to_string(),
            })
    }

    fn navigate(&self, session_id: &str, _url: &str) -> Result<(), BrowserError> {
        self.require_session(session_id)?;
        Err(BrowserError::BackendNotConnected {
            reason: self.reason.clone(),
        })
    }

    fn get_text(&self, session_id: &str) -> Result<String, BrowserError> {
        self.require_session(session_id)?;
        Err(BrowserError::BackendNotConnected {
            reason: self.reason.clone(),
        })
    }

    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError> {
        self.require_session(session_id)?;
        Err(BrowserError::BackendNotConnected {
            reason: self.reason.clone(),
        })
    }

    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError> {
        let guard = self.sessions.lock().expect("none backend lock");
        let mut out: Vec<BrowserSessionView> = guard
            .iter()
            .map(|(id, sess)| BrowserSessionView {
                session_id: id.clone(),
                opened_at: sess.opened_at,
                current_url: None,
                page_title: None,
                status: "unconnected".to_string(),
            })
            .collect();
        out.sort_by_key(|r| r.opened_at);
        Ok(out)
    }
}

impl NoneBackend {
    fn require_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let guard = self.sessions.lock().expect("none backend lock");
        if guard.contains_key(session_id) {
            Ok(())
        } else {
            Err(BrowserError::SessionNotFound {
                session_id: session_id.to_string(),
            })
        }
    }
}

/// Construct a backend from operator config. `cfg.backend`
/// values:
/// - `"none"` (default) → [`NoneBackend`] with a neutral reason
/// - `"playwright"`     → [`NoneBackend`] with a reason that
///   names the missing integration (operators see the gap; we
///   never silently downgrade)
/// - anything else      → `InvalidBackend` config error
pub fn build_backend(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    if !KNOWN_BACKENDS.contains(&cfg.backend.as_str()) {
        return Err(BrowserError::InvalidBackend {
            name: cfg.backend.clone(),
        });
    }
    let reason = match cfg.backend.as_str() {
        "none" => "operator selected backend=\"none\" — capability surface is wired \
                   but no real browser backend ships in this Relix build yet"
            .to_string(),
        "playwright" => "backend=\"playwright\" is reserved for a future CW4 follow-up milestone; \
             today the surface is wired but the live integration is not connected. \
             See docs/browser-tool.md."
            .to_string(),
        _ => unreachable!("KNOWN_BACKENDS check above"),
    };
    Ok(Arc::new(NoneBackend::new(cfg, reason)))
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
         BackendNotConnected today; a future milestone wires a real backend."
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

/// Register every browser.* capability onto the dispatch bridge.
/// Caller is `tool::register` in `mod.rs` — only invoked when
/// `[tool.browser]` is present in the operator config.
pub fn register(bridge: &mut DispatchBridge, backend: Arc<dyn BrowserBackend>) {
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
    let b = backend;
    bridge.register(
        "tool.browser.list_sessions",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = b.clone();
            async move { handle_list_sessions(&b, &ctx) }
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
    // Cheap URL sanity check — refuse `javascript:` / data:
    // anywhere even though no real navigation happens today,
    // so the contract holds when a real backend lands.
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:") {
        return to_envelope(&BrowserError::InvalidUrl {
            url: url.to_string(),
        });
    }
    match b.navigate(id, url) {
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
        BrowserError::SessionNotFound { .. } => error_kinds::INVALID_ARGS,
        BrowserError::SessionCapReached { .. } => error_kinds::INVALID_ARGS,
        BrowserError::InvalidUrl { .. } => error_kinds::INVALID_ARGS,
        BrowserError::InvalidBackend { .. } => error_kinds::INVALID_ARGS,
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

fn new_session_id() -> String {
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

fn unix_secs() -> i64 {
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
        }
    }

    #[test]
    fn build_backend_none_returns_arc() {
        let b = build_backend(&cfg()).unwrap();
        assert_eq!(b.name(), "none");
    }

    #[test]
    fn build_backend_playwright_returns_unconnected_none() {
        let mut c = cfg();
        c.backend = "playwright".into();
        // Today playwright resolves to a NoneBackend (with a
        // pointed reason). When the real impl ships it would
        // return a PlaywrightBackend wired up.
        let b = build_backend(&c).unwrap();
        assert_eq!(b.name(), "none");
    }

    #[test]
    fn build_backend_rejects_unknown_name() {
        let mut c = cfg();
        c.backend = "chrome-extension-thing".into();
        // build_backend returns Result<Arc<dyn ...>, BrowserError>;
        // Arc<dyn trait> doesn't derive Debug, so explicit match.
        match build_backend(&c) {
            Ok(_) => panic!("expected InvalidBackend, got Ok"),
            Err(BrowserError::InvalidBackend { .. }) => {}
            Err(other) => panic!("expected InvalidBackend, got {other:?}"),
        }
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
    /// descriptor. Pure reads (get_text / screenshot / list)
    /// are Safe; session lifecycle (open / close — internal
    /// allocation only) is Low; navigate (network egress) is
    /// Medium. Tiers reflect the EVENTUAL backend behavior;
    /// today the NoneBackend returns BackendNotConnected, but
    /// when the live backend lands (D-008) the tiers already
    /// describe what each capability does — no scaffold→live
    /// transition surprise.
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
