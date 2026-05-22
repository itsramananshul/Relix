//! PH-BROWSER-WD — live fantoccini/WebDriver backend module.
//!
//! Compiled only when `--features browser-webdriver` is set.
//! Replaces the PH-BROWSER-FEATURES scaffold with a real driver
//! that talks WebDriver-over-HTTP to an operator-supplied
//! `chromedriver` / `geckodriver` sidecar process. The operator
//! is responsible for starting the daemon; Relix points at it
//! via [`super::BrowserConfig::webdriver_url`] (defaults to
//! `http://127.0.0.1:9515`, chromedriver's default port).
//!
//! Selection at runtime is driven by `[tool.browser] backend =
//! "webdriver"` in the operator config. With the feature
//! disabled, [`super::build_backend`] returns
//! [`super::BrowserError::FeatureNotCompiled`] — no silent
//! NoneBackend fallback.
//!
//! ## Sync-from-async bridge
//!
//! The [`super::BrowserBackend`] trait is FROZEN as a sync
//! surface (see `mod.rs`). fantoccini's API is async, so every
//! sync method here bridges with
//! `tokio::task::block_in_place(|| Handle::current().block_on(fut))`.
//! The dispatch bridge invokes us from within a multi-threaded
//! tokio runtime (`#[tokio::main]` in every Relix binary
//! defaults to multi-threaded), so `block_in_place` is the
//! sound primitive — it yields the worker thread to other
//! tasks while we block on the WebDriver round-trip. Calling
//! `Handle::current().block_on(...)` directly without
//! `block_in_place` would deadlock the current worker.
//!
//! ## Error mapping
//!
//! Every fantoccini failure (connect, command, decode) maps to
//! [`BrowserError::BackendNotConnected`] with a reason string
//! that names the operator-visible cause. The most common
//! failure shape — chromedriver isn't running — surfaces as
//! `"webdriver: failed to connect to http://127.0.0.1:9515 — is
//! chromedriver running?"`. The trait error set is frozen, so
//! we don't introduce a `WebDriverDown` variant; the reason
//! string carries the diagnostic.
//!
//! ## Lazy connect
//!
//! [`try_build`] does NOT probe the driver URL. The first
//! [`open_session`] call is what attempts the HTTP connect.
//! This lets Relix start cleanly even when the operator has
//! not yet booted their driver — they only see the error when
//! they actually try to use the capability. Matches the
//! posture of the AI / HTTP tool backends.
//!
//! Trade-offs vs `headless_chrome` / `playwright` (D-008):
//! - Most standards-aligned (W3C WebDriver).
//! - Requires the operator to install + run a separate
//!   driver binary alongside the tool node.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fantoccini::{Client, ClientBuilder, Locator};
use tokio::runtime::Handle;

use super::{BrowserBackend, BrowserConfig, BrowserError, BrowserSessionView, new_session_id};

pub const NAME: &str = "webdriver";

/// Per-session state: the live fantoccini client plus the open
/// timestamp surfaced via [`list_sessions`]. The client is a
/// thin handle and is `Clone` — we keep one canonical copy in
/// the map and use `Client::close` (which consumes the value)
/// when the operator closes the session.
struct WdSession {
    client: Client,
    opened_at: i64,
}

/// PH-BROWSER-WD: live WebDriver backend. Holds the
/// operator-configured driver URL and a map of open sessions.
/// Each session owns its own fantoccini `Client` (which itself
/// owns a WebDriver session id at the wire layer).
pub struct WebDriverBackend {
    driver_url: String,
    sessions: Mutex<HashMap<String, WdSession>>,
    max_sessions: usize,
    /// Per-call deadline. Wraps every fantoccini round-trip in
    /// `tokio::time::timeout` so a hung driver can't pin a VM
    /// thread forever.
    call_timeout: Duration,
}

impl WebDriverBackend {
    fn new(cfg: &BrowserConfig) -> Self {
        Self {
            driver_url: cfg.webdriver_url.clone(),
            sessions: Mutex::new(HashMap::new()),
            max_sessions: cfg.max_sessions,
            call_timeout: Duration::from_secs(cfg.call_timeout_secs.max(1)),
        }
    }

    /// Run an async block on the ambient tokio runtime, wrapped
    /// in `block_in_place` so we don't starve the worker. Used
    /// by every sync trait method.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| Handle::current().block_on(fut))
    }

    fn with_client<F, Fut, T>(&self, session_id: &str, op: F) -> Result<T, BrowserError>
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = Result<T, BrowserError>>,
    {
        // Clone the client out of the map so we hold the lock
        // for the shortest possible window (and so the async
        // closure isn't tied to the MutexGuard's lifetime).
        let client = {
            let guard = self.sessions.lock().expect("wd backend lock");
            match guard.get(session_id) {
                Some(s) => s.client.clone(),
                None => {
                    return Err(BrowserError::SessionNotFound {
                        session_id: session_id.to_string(),
                    });
                }
            }
        };
        self.block_on(async move {
            tokio::time::timeout(self.call_timeout, op(client))
                .await
                .map_err(|_| BrowserError::BackendNotConnected {
                    reason: format!(
                        "webdriver: call timed out after {}s against {}",
                        self.call_timeout.as_secs(),
                        self.driver_url
                    ),
                })?
        })
    }
}

impl BrowserBackend for WebDriverBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn open_session(&self) -> Result<String, BrowserError> {
        // Enforce the cap BEFORE we touch the network so a
        // misconfigured operator gets a clean
        // `SessionCapReached` rather than a stray connect
        // attempt. Drop the guard before block_on to keep the
        // critical section tight.
        {
            let guard = self.sessions.lock().expect("wd backend lock");
            if guard.len() >= self.max_sessions {
                return Err(BrowserError::SessionCapReached {
                    max: self.max_sessions,
                });
            }
        }

        let url = self.driver_url.clone();
        let timeout = self.call_timeout;
        let client_res: Result<Client, BrowserError> = self.block_on(async move {
            let builder =
                ClientBuilder::rustls().map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: rustls connector init failed: {e}"),
                })?;
            tokio::time::timeout(timeout, builder.connect(&url))
                .await
                .map_err(|_| BrowserError::BackendNotConnected {
                    reason: format!(
                        "webdriver: connect to {url} timed out after {}s",
                        timeout.as_secs()
                    ),
                })?
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!(
                        "webdriver: failed to connect to {url} — is chromedriver running? ({e})"
                    ),
                })
        });
        let client = client_res?;

        // Re-check the cap after the async connect (could have
        // raced if multiple callers open simultaneously).
        let id = new_session_id();
        let mut guard = self.sessions.lock().expect("wd backend lock");
        if guard.len() >= self.max_sessions {
            // We over-allocated; close the freshly-built
            // client out-of-band so the driver isn't holding
            // an orphan session. Best-effort — ignore errors.
            let evict = client.clone();
            drop(guard);
            self.block_on(async move {
                let _ = evict.close().await;
            });
            return Err(BrowserError::SessionCapReached {
                max: self.max_sessions,
            });
        }
        guard.insert(
            id.clone(),
            WdSession {
                client,
                opened_at: super::unix_secs(),
            },
        );
        Ok(id)
    }

    fn close_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let session = {
            let mut guard = self.sessions.lock().expect("wd backend lock");
            guard
                .remove(session_id)
                .ok_or(BrowserError::SessionNotFound {
                    session_id: session_id.to_string(),
                })?
        };
        let timeout = self.call_timeout;
        let url = self.driver_url.clone();
        let close_result: Result<(), BrowserError> = self.block_on(async move {
            tokio::time::timeout(timeout, session.client.close())
                .await
                .map_err(|_| BrowserError::BackendNotConnected {
                    reason: format!(
                        "webdriver: close timed out after {}s against {url}",
                        timeout.as_secs()
                    ),
                })?
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: close session: {e}"),
                })
        });
        close_result
    }

    fn navigate(&self, session_id: &str, url: &str) -> Result<(), BrowserError> {
        let url_owned = url.to_string();
        self.with_client(session_id, move |client| async move {
            client
                .goto(&url_owned)
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: goto({url_owned}): {e}"),
                })
        })
    }

    fn get_text(&self, session_id: &str) -> Result<String, BrowserError> {
        self.with_client(session_id, |client| async move {
            let body = client.find(Locator::Css("body")).await.map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("webdriver: find(body): {e}"),
                }
            })?;
            body.text()
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: body.text(): {e}"),
                })
        })
    }

    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError> {
        self.with_client(session_id, |client| async move {
            client
                .screenshot()
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: screenshot: {e}"),
                })
        })
    }

    // W2-002e: live click / type_text / wait_for_selector via
    // fantoccini. Each method bridges sync→async through the
    // existing `with_client` helper which already wraps the call
    // in tokio::time::timeout against `call_timeout`. Errors
    // map to BackendNotConnected with a webdriver: prefix.

    fn click(&self, session_id: &str, selector: &str) -> Result<(), BrowserError> {
        let selector = selector.to_string();
        self.with_client(session_id, move |client| async move {
            let el = client.find(Locator::Css(&selector)).await.map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("webdriver: click find({selector}): {e}"),
                }
            })?;
            el.click()
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: click({selector}): {e}"),
                })?;
            Ok(())
        })
    }

    fn type_text(&self, session_id: &str, selector: &str, text: &str) -> Result<(), BrowserError> {
        let selector = selector.to_string();
        let text = text.to_string();
        self.with_client(session_id, move |client| async move {
            let el = client.find(Locator::Css(&selector)).await.map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("webdriver: type_text find({selector}): {e}"),
                }
            })?;
            // fantoccini's `send_keys` is the WebDriver equivalent
            // of "type into focused element"; the element click
            // ahead of it focuses the input (mirrors the HC
            // backend's focus-click semantics).
            el.click()
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("webdriver: type_text focus-click({selector}): {e}"),
                })?;
            el.send_keys(&text)
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!(
                        "webdriver: send_keys({selector}, {} chars): {e}",
                        text.chars().count()
                    ),
                })?;
            Ok(())
        })
    }

    fn wait_for_selector(
        &self,
        session_id: &str,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        // Override the with_client wrap so the operator-supplied
        // timeout governs the wait (NOT the backend-level
        // call_timeout). This matters: a 60_000ms wait is a
        // legitimate request that the backend-level 30s budget
        // would clip.
        let client = {
            let guard = self.sessions.lock().expect("wd backend lock");
            match guard.get(session_id) {
                Some(s) => s.client.clone(),
                None => {
                    return Err(BrowserError::SessionNotFound {
                        session_id: session_id.to_string(),
                    });
                }
            }
        };
        let selector = selector.to_string();
        let timeout = std::time::Duration::from_millis(timeout_ms);
        self.block_on(async move {
            tokio::time::timeout(
                timeout,
                client
                    .wait()
                    .at_most(timeout)
                    .for_element(Locator::Css(&selector)),
            )
            .await
            .map_err(|_| BrowserError::BackendNotConnected {
                reason: format!(
                    "webdriver: wait_for_selector({selector}, {timeout_ms}ms) outer timeout"
                ),
            })?
            .map(|_| ())
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("webdriver: wait_for_selector({selector}, {timeout_ms}ms): {e}"),
            })
        })
    }

    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError> {
        // Snapshot the (id, client clone, opened_at) tuples
        // under the lock; release before we issue WebDriver
        // queries so we don't serialize all clients on the map
        // lock during slow round-trips.
        let snapshot: Vec<(String, Client, i64)> = {
            let guard = self.sessions.lock().expect("wd backend lock");
            guard
                .iter()
                .map(|(id, s)| (id.clone(), s.client.clone(), s.opened_at))
                .collect()
        };

        let timeout = self.call_timeout;
        let rows = self.block_on(async move {
            let mut rows = Vec::with_capacity(snapshot.len());
            for (id, client, opened_at) in snapshot {
                // current_url + title; both best-effort.
                // Failures fall back to None / "connected" so
                // list_sessions never errors just because one
                // tab is mid-navigation.
                let current_url = tokio::time::timeout(timeout, client.current_url())
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .map(|u| u.to_string());
                let page_title = tokio::time::timeout(timeout, client.title())
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .filter(|s| !s.is_empty());
                rows.push(BrowserSessionView {
                    session_id: id,
                    opened_at,
                    current_url,
                    page_title,
                    status: "connected".to_string(),
                });
            }
            rows.sort_by_key(|r| r.opened_at);
            rows
        });
        Ok(rows)
    }
}

/// PH-BROWSER-WD: live build. Constructs the backend without
/// probing the driver URL — the first `open_session` call is
/// what attempts to connect. This means a misconfigured driver
/// URL fails at use-time, not start-time; the start-time guard
/// is the `validate_config` Cargo-feature / backend-name check.
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Ok(Arc::new(WebDriverBackend::new(cfg)))
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BrowserConfig {
        BrowserConfig {
            backend: "webdriver".to_string(),
            max_sessions: 4,
            call_timeout_secs: 5,
            webdriver_url: "http://127.0.0.1:9515".to_string(),
            ..BrowserConfig::default()
        }
    }

    /// PH-BROWSER-WD: try_build returns a live backend (not a
    /// labeled NoneBackend) and does NOT touch the network.
    /// The `name()` label matches the operator-configured
    /// backend so the dashboard / list_sessions reflects the
    /// selection.
    #[test]
    fn try_build_returns_real_backend_named_webdriver() {
        let b = try_build(&cfg()).expect("try_build should not fail without driver");
        assert_eq!(b.name(), "webdriver");
    }

    /// try_build must NOT probe the driver URL — an operator
    /// who hasn't started chromedriver yet still gets a clean
    /// startup. We "prove" this by pointing at a clearly-dead
    /// URL and asserting try_build still succeeds.
    #[test]
    fn try_build_does_not_connect_to_driver() {
        let mut c = cfg();
        // A reserved-for-documentation IP that will not have a
        // WebDriver on it; if try_build were probing, the test
        // would either hang or return an error. We expect Ok.
        c.webdriver_url = "http://203.0.113.1:65530".to_string();
        let b = try_build(&c).expect("try_build must be lazy");
        assert_eq!(b.name(), "webdriver");
    }

    /// PH-BROWSER-WD: max_sessions cap is enforced BEFORE the
    /// network connect. We verify by inspecting the field
    /// directly — there's no public getter on the trait, so we
    /// downcast through the concrete type.
    #[test]
    fn max_sessions_cap_is_stored_for_pre_network_check() {
        let mut c = cfg();
        c.max_sessions = 3;
        let backend = WebDriverBackend::new(&c);
        assert_eq!(backend.max_sessions, 3);
        // Sanity: simulate "already at cap" via direct map
        // mutation and confirm open_session would short-circuit.
        // We don't call open_session because that would attempt
        // a network connect on an unbound port and depend on OS
        // timing; instead we just verify the guard variable.
    }

    /// PH-BROWSER-WD: the default `webdriver_url` matches
    /// chromedriver's default port so a vanilla setup works
    /// without any operator config edits.
    #[test]
    fn default_webdriver_url_is_chromedriver_default_port() {
        let c = BrowserConfig::default();
        assert_eq!(c.webdriver_url, "http://127.0.0.1:9515");
    }

    /// PH-BROWSER-WD: a custom `webdriver_url` is honored — the
    /// backend stores exactly what the operator configured.
    #[test]
    fn custom_webdriver_url_is_honored() {
        let mut c = cfg();
        c.webdriver_url = "http://10.0.0.5:4444".to_string();
        let backend = WebDriverBackend::new(&c);
        assert_eq!(backend.driver_url, "http://10.0.0.5:4444");
    }

    /// Cap-of-one regression guard: the cap field is correctly
    /// plumbed from BrowserConfig.
    #[test]
    fn max_sessions_of_one_is_honored() {
        let mut c = cfg();
        c.max_sessions = 1;
        let backend = WebDriverBackend::new(&c);
        assert_eq!(backend.max_sessions, 1);
    }

    // ── Integration test: gated on a running WebDriver daemon ──
    //
    // We avoid the network entirely unless an operator has
    // started chromedriver / geckodriver on the default port.
    // `webdriver_available` is a cheap /status probe; if it
    // returns false the test prints a skip line and passes,
    // matching the project's posture for live-driver tests.

    async fn webdriver_available(url: &str) -> bool {
        reqwest::get(format!("{url}/status"))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// PH-BROWSER-WD: exercise the full open → navigate →
    /// close round-trip against a running driver. Skips
    /// silently when no driver is available so CI without a
    /// chromedriver sidecar stays green.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_webdriver_navigates_about_blank() {
        let cfg = BrowserConfig::default();
        if !webdriver_available(&cfg.webdriver_url).await {
            eprintln!(
                "skipping live_webdriver_navigates_about_blank: no WebDriver at {}",
                cfg.webdriver_url
            );
            return;
        }
        // The trait methods are sync, so we spawn them on
        // `spawn_blocking` so the inner `block_in_place` is
        // legal on this test's multi-thread runtime.
        let backend: Arc<dyn BrowserBackend> = try_build(&cfg).expect("try_build");
        let backend_clone = backend.clone();
        let id = tokio::task::spawn_blocking(move || backend_clone.open_session())
            .await
            .expect("open join")
            .expect("open_session");

        let backend_clone = backend.clone();
        let id_for_nav = id.clone();
        tokio::task::spawn_blocking(move || backend_clone.navigate(&id_for_nav, "about:blank"))
            .await
            .expect("nav join")
            .expect("navigate");

        let backend_clone = backend.clone();
        let id_for_close = id.clone();
        tokio::task::spawn_blocking(move || backend_clone.close_session(&id_for_close))
            .await
            .expect("close join")
            .expect("close_session");
    }
}
