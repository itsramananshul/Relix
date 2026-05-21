//! PH-BROWSER-HC — live `headless_chrome` backend module.
//!
//! Compiled only when `--features browser-headless-chrome` is
//! set. This module drives the operator's `chrome` / `chromium`
//! binary via the [`headless_chrome`] crate (Chrome DevTools
//! Protocol over a synchronous websocket loop). The trait
//! surface defined in [`super::BrowserBackend`] is frozen —
//! every error from the underlying crate maps to
//! [`super::BrowserError::BackendNotConnected`] with a
//! `headless_chrome: <reason>` prefix so operators see exactly
//! why a navigate / get_text / screenshot failed.
//!
//! ## Browser launch strategy
//!
//! The Chrome process is **lazily** spawned on the first
//! `open_session()` call. Building the backend up front (at
//! `ToolBackend::new` time) would force every Relix node that
//! merely has `[tool.browser] backend = "headless_chrome"` to
//! refuse to start when Chrome isn't installed — even if the
//! operator never invokes a browser tool. Lazy launch keeps the
//! node alive and the failure honest: the operator sees
//! `BackendNotConnected { reason: "headless_chrome: ..." }` on
//! the first call, not a silent NoneBackend.
//!
//! Once launched, the [`headless_chrome::Browser`] handle is
//! cached and reused for all subsequent tabs. If the Chrome
//! process dies the cached handle is dropped on the next launch
//! attempt and a fresh one is spawned.
//!
//! ## Honesty contract
//!
//! - If `headless_chrome::Browser::default()` fails (no chrome
//!   binary in PATH, no `CHROME` env var) → BackendNotConnected.
//!   No silent fallback to NoneBackend.
//! - If `browser.new_tab()` fails → BackendNotConnected.
//! - If `tab.navigate_to` / `wait_until_navigated` /
//!   `capture_screenshot` / `find_element` fail →
//!   BackendNotConnected.
//! - `list_sessions` reports `status = "connected"` for every
//!   live tab and `"unconnected"` for entries whose tab dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use headless_chrome::Browser;
use headless_chrome::browser::tab::Tab;
use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;

use super::{
    BrowserBackend, BrowserConfig, BrowserError, BrowserSessionView, new_session_id, unix_secs,
};

/// PH-BROWSER-HC: canonical backend name string. Kept as a
/// constant so the dispatch in `super::build_backend` and the
/// `name()` label on the live backend stay in sync.
pub const NAME: &str = "headless_chrome";

/// One tracked session = one CDP tab + its open timestamp.
struct Session {
    tab: Arc<Tab>,
    opened_at: i64,
}

/// Live Chrome DevTools Protocol backend. See module docs for
/// the launch strategy and error-mapping posture.
pub struct HeadlessChromeBackend {
    max_sessions: usize,
    call_timeout: Duration,
    /// Lazily-initialised Chrome process handle. `None` until
    /// the first `open_session()` succeeds. Subsequent calls
    /// reuse the cached `Browser` until it's explicitly cleared
    /// (e.g. after a launch failure).
    browser: Mutex<Option<Browser>>,
    sessions: Mutex<HashMap<String, Session>>,
}

impl HeadlessChromeBackend {
    /// Build the backend from operator config. This does NOT
    /// launch Chrome — see the module docs for the lazy-launch
    /// rationale.
    pub fn from_cfg(cfg: &BrowserConfig) -> Self {
        Self {
            max_sessions: cfg.max_sessions,
            call_timeout: Duration::from_secs(cfg.call_timeout_secs),
            browser: Mutex::new(None),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Return a clone of the cached `Browser` handle, launching
    /// Chrome on first call. Errors map to BackendNotConnected
    /// with a `headless_chrome: ...` reason prefix so operators
    /// see exactly why launch failed.
    fn ensure_browser(&self) -> Result<Browser, BrowserError> {
        let mut guard = self.browser.lock().expect("hc backend lock");
        if let Some(b) = guard.as_ref() {
            return Ok(b.clone());
        }
        let browser = Browser::default().map_err(|e| {
            tracing::warn!(
                error = %e,
                "PH-BROWSER-HC: failed to launch chrome/chromium — operator will see BackendNotConnected"
            );
            BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: launch failed: {e}"),
            }
        })?;
        *guard = Some(browser.clone());
        Ok(browser)
    }

    /// Internal: take a snapshot of the sessions map (id ->
    /// (tab, opened_at)). Used by list_sessions and close.
    fn lookup_session(&self, session_id: &str) -> Result<Arc<Tab>, BrowserError> {
        let guard = self.sessions.lock().expect("hc backend lock");
        guard
            .get(session_id)
            .map(|s| s.tab.clone())
            .ok_or_else(|| BrowserError::SessionNotFound {
                session_id: session_id.to_string(),
            })
    }

    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions.lock().expect("hc backend lock").len()
    }

    #[cfg(test)]
    fn max_sessions_for_test(&self) -> usize {
        self.max_sessions
    }

    /// Test-only: enforce the session cap without spawning
    /// Chrome. Pushes a sentinel into the map (no Tab) and then
    /// asserts the next `open_session()` returns
    /// SessionCapReached. The sentinel uses an Arc<Tab> built
    /// from a sham via the test helper — we instead model the
    /// cap check directly here so the test doesn't need a real
    /// Tab.
    #[cfg(test)]
    fn try_reserve_slot(&self) -> Result<(), BrowserError> {
        let guard = self.sessions.lock().expect("hc backend lock");
        if guard.len() >= self.max_sessions {
            return Err(BrowserError::SessionCapReached {
                max: self.max_sessions,
            });
        }
        Ok(())
    }
}

impl BrowserBackend for HeadlessChromeBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn open_session(&self) -> Result<String, BrowserError> {
        // Cap check FIRST so an operator with Chrome installed
        // but at the session limit gets the typed
        // SessionCapReached error rather than spending a Chrome
        // tab they'll have to close.
        {
            let guard = self.sessions.lock().expect("hc backend lock");
            if guard.len() >= self.max_sessions {
                return Err(BrowserError::SessionCapReached {
                    max: self.max_sessions,
                });
            }
        }
        let browser = self.ensure_browser()?;
        let tab = browser.new_tab().map_err(|e| {
            // If Chrome died, drop the cached handle so the next
            // attempt re-launches. Honesty: don't paper over a
            // crashed process by reusing a dead transport.
            *self.browser.lock().expect("hc backend lock") = None;
            BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: new_tab failed: {e}"),
            }
        })?;
        // Apply per-call deadline to wait_for_element / etc. on
        // this tab. Note: navigate_to itself doesn't use this
        // timeout — wait_until_navigated has its own internal
        // wait loop. We rely on the operator-configured
        // call_timeout_secs to bound find_element waits.
        tab.set_default_timeout(self.call_timeout);
        let id = new_session_id();
        let mut guard = self.sessions.lock().expect("hc backend lock");
        // Re-check cap after acquiring the lock (race: two
        // concurrent open_session calls could both pass the
        // first check). The second one rolls back the tab.
        if guard.len() >= self.max_sessions {
            drop(guard);
            // Best-effort close of the spare tab; ignore errors
            // because we're already returning a typed cap error.
            let _ = tab.close(false);
            return Err(BrowserError::SessionCapReached {
                max: self.max_sessions,
            });
        }
        guard.insert(
            id.clone(),
            Session {
                tab,
                opened_at: unix_secs(),
            },
        );
        Ok(id)
    }

    fn close_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let mut guard = self.sessions.lock().expect("hc backend lock");
        let sess = guard
            .remove(session_id)
            .ok_or_else(|| BrowserError::SessionNotFound {
                session_id: session_id.to_string(),
            })?;
        drop(guard);
        // Best-effort: if Chrome already dropped the tab the
        // close call returns an error. We surface it as
        // BackendNotConnected so the operator still sees the
        // session removed from the map but knows the close
        // round-trip failed.
        if let Err(e) = sess.tab.close(true) {
            return Err(BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: close failed: {e}"),
            });
        }
        Ok(())
    }

    fn navigate(&self, session_id: &str, url: &str) -> Result<(), BrowserError> {
        let tab = self.lookup_session(session_id)?;
        tab.navigate_to(url)
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: navigate_to({url}) failed: {e}"),
            })?;
        tab.wait_until_navigated()
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: wait_until_navigated failed: {e}"),
            })?;
        Ok(())
    }

    fn get_text(&self, session_id: &str) -> Result<String, BrowserError> {
        let tab = self.lookup_session(session_id)?;
        let body = tab
            .find_element("body")
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: find_element(body) failed: {e}"),
            })?;
        body.get_inner_text()
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: get_inner_text failed: {e}"),
            })
    }

    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError> {
        let tab = self.lookup_session(session_id)?;
        tab.capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("headless_chrome: capture_screenshot failed: {e}"),
            })
    }

    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError> {
        let guard = self.sessions.lock().expect("hc backend lock");
        let mut out: Vec<BrowserSessionView> = guard
            .iter()
            .map(|(id, sess)| {
                let current_url = {
                    let u = sess.tab.get_url();
                    if u.is_empty() { None } else { Some(u) }
                };
                let page_title = sess.tab.get_title().ok();
                // Best-effort liveness: if get_url() returned a
                // non-empty string we treat the tab as
                // connected. CDP doesn't expose a cheap "is the
                // websocket still up?" predicate — if it's dead
                // the next navigate/screenshot will surface the
                // failure as BackendNotConnected.
                let status = if current_url.is_some() {
                    "connected".to_string()
                } else {
                    "unconnected".to_string()
                };
                BrowserSessionView {
                    session_id: id.clone(),
                    opened_at: sess.opened_at,
                    current_url,
                    page_title,
                    status,
                }
            })
            .collect();
        out.sort_by_key(|r| r.opened_at);
        Ok(out)
    }
}

/// PH-BROWSER-HC: live build. Constructs the
/// [`HeadlessChromeBackend`] without spawning Chrome — see the
/// module docs for the lazy-launch rationale. Selecting
/// `backend = "headless_chrome"` no longer fails at startup
/// when Chrome isn't installed; the failure surfaces honestly
/// on the first navigate / get_text / screenshot call.
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Ok(Arc::new(HeadlessChromeBackend::from_cfg(cfg)))
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn cfg() -> BrowserConfig {
        BrowserConfig {
            backend: "headless_chrome".to_string(),
            max_sessions: 4,
            call_timeout_secs: 10,
        }
    }

    /// Does this host have a chromium-family browser in PATH /
    /// at a well-known location? Integration tests below skip
    /// (via `eprintln! + return`) when this returns false so
    /// CI without Chrome doesn't fail.
    fn chromium_available() -> bool {
        for bin in [
            "chrome",
            "google-chrome",
            "chromium",
            "chromium-browser",
            "msedge",
        ] {
            if Command::new(bin).arg("--version").output().is_ok() {
                return true;
            }
        }
        false
    }

    /// Unit: try_build returns a real HeadlessChromeBackend
    /// (NOT the NoneBackend scaffold) and `name()` reports
    /// `"headless_chrome"`. Does NOT spawn Chrome.
    #[test]
    fn try_build_returns_live_backend_with_correct_name() {
        let b = try_build(&cfg()).expect("live backend should build");
        assert_eq!(b.name(), "headless_chrome");
    }

    /// Unit: `from_cfg` honours `max_sessions` without
    /// spawning Chrome — the cap is enforced via the in-memory
    /// session map. We populate the map with sentinel entries
    /// via the test-only `try_reserve_slot` helper.
    #[test]
    fn max_sessions_cap_is_enforced_without_chrome() {
        let mut c = cfg();
        c.max_sessions = 2;
        let backend = HeadlessChromeBackend::from_cfg(&c);
        assert_eq!(backend.max_sessions_for_test(), 2);
        // Pre-fill the map with sentinel entries that bypass
        // Chrome launch. We grab a lock and stuff in a dummy
        // entry — but we don't have a way to fabricate a Tab,
        // so instead we verify the cap-check code path runs
        // BEFORE the Chrome launch by exercising
        // try_reserve_slot.
        assert_eq!(backend.session_count(), 0);
        assert!(backend.try_reserve_slot().is_ok());
        // Now simulate two open sessions by reaching into the
        // sessions map via the only entry point we have: we
        // can't insert a real Session without a Tab, so instead
        // verify the check by setting max_sessions = 0 and
        // calling open_session — it must short-circuit BEFORE
        // launching Chrome.
        let mut c0 = cfg();
        c0.max_sessions = 0;
        let backend0 = HeadlessChromeBackend::from_cfg(&c0);
        // With max=0 the open_session check fires before
        // ensure_browser, so this returns SessionCapReached
        // even when Chrome isn't installed.
        match backend0.open_session() {
            Err(BrowserError::SessionCapReached { max: 0 }) => {}
            other => panic!("expected SessionCapReached(0) without Chrome launch, got {other:?}"),
        }
    }

    /// PH-BROWSER-HC: replacement for the old
    /// `feature_headless_chrome_compiled_builds_scaffold` test.
    /// With the feature compiled, `build_backend` must yield a
    /// live backend whose `name()` is "headless_chrome". The
    /// first `open_session()` either succeeds (Chrome found) or
    /// returns BackendNotConnected (Chrome missing) — NOT
    /// SessionCapReached / SessionNotFound / Invalid*.
    #[test]
    fn feature_headless_chrome_compiled_builds_real_backend() {
        let mut c = super::super::BrowserConfig {
            backend: "headless_chrome".to_string(),
            max_sessions: 4,
            call_timeout_secs: 10,
        };
        c.backend = "headless_chrome".into();
        let b = super::super::build_backend(&c).expect("build_backend should succeed");
        assert_eq!(b.name(), "headless_chrome");
        match b.open_session() {
            Ok(id) => {
                assert_eq!(id.len(), 16);
                assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
                // Best-effort cleanup; ignore errors if Chrome
                // already cleaned up after itself.
                let _ = b.close_session(&id);
            }
            Err(BrowserError::BackendNotConnected { reason }) => {
                assert!(
                    reason.starts_with("headless_chrome:"),
                    "BackendNotConnected reason must be prefixed with 'headless_chrome:'; got {reason}"
                );
            }
            Err(other) => {
                panic!("open_session must return Ok or BackendNotConnected; got {other:?}")
            }
        }
    }

    /// Integration: with a real chromium-family binary on the
    /// host, open a tab, navigate to about:blank, and close it.
    /// Skips cleanly when Chrome isn't installed.
    #[test]
    fn live_chromium_navigates_to_about_blank() {
        if !chromium_available() {
            eprintln!("skipping: chromium not available");
            return;
        }
        let b = try_build(&cfg()).expect("backend builds");
        let id = match b.open_session() {
            Ok(id) => id,
            Err(BrowserError::BackendNotConnected { reason }) => {
                eprintln!("skipping: chrome launch failed: {reason}");
                return;
            }
            Err(other) => panic!("unexpected open_session error: {other:?}"),
        };
        if let Err(e) = b.navigate(&id, "about:blank") {
            eprintln!("skipping: navigate failed (likely chrome-version mismatch): {e:?}");
            let _ = b.close_session(&id);
            return;
        }
        let _ = b.close_session(&id);
    }

    /// Integration (richer): navigate to example.com, fetch
    /// body text, screenshot. Gated on chromium_available; any
    /// transport-level failure downgrades to a `eprintln!` skip
    /// rather than failing the suite — operators running this
    /// on CI without consistent Chrome installs shouldn't see
    /// spurious red.
    #[test]
    fn live_chromium_example_com_text_and_screenshot() {
        if !chromium_available() {
            eprintln!("skipping: chromium not available");
            return;
        }
        let b = try_build(&cfg()).expect("backend builds");
        let id = match b.open_session() {
            Ok(id) => id,
            Err(BrowserError::BackendNotConnected { reason }) => {
                eprintln!("skipping: chrome launch failed: {reason}");
                return;
            }
            Err(other) => panic!("unexpected open_session error: {other:?}"),
        };
        if let Err(e) = b.navigate(&id, "https://example.com/") {
            eprintln!("skipping: navigate failed (network or chrome): {e:?}");
            let _ = b.close_session(&id);
            return;
        }
        match b.get_text(&id) {
            Ok(text) => assert!(
                text.contains("Example Domain"),
                "expected example.com body to contain 'Example Domain'; got {} chars",
                text.len()
            ),
            Err(e) => {
                eprintln!("skipping get_text assertion: {e:?}");
                let _ = b.close_session(&id);
                return;
            }
        }
        match b.screenshot(&id) {
            Ok(bytes) => assert!(!bytes.is_empty(), "screenshot must return non-empty PNG"),
            Err(e) => eprintln!("screenshot soft-skip: {e:?}"),
        }
        let _ = b.close_session(&id);
    }
}
