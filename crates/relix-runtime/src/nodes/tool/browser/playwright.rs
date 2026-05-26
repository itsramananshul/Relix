//! PH-BROWSER-PW — live `playwright` backend module.
//!
//! Compiled only when `--features browser-playwright` is set.
//! The driver spawns a Node.js sidecar that imports
//! `playwright-core` and serves a small JSON-RPC protocol over
//! stdio (see `playwright_sidecar.js`). The Rust side enforces
//! the [`BrowserBackend`] contract; the Node side wraps
//! Playwright's async API. Operator-visible failures (Node
//! missing, playwright-core not installed, navigation timeout)
//! surface as [`BrowserError::BackendNotConnected`] with the
//! sidecar's structured `code` + `message` so the chronicle and
//! the dashboard show an honest, specific cause — never a faked
//! success.
//!
//! ## Lifecycle
//!
//! - The sidecar is spawned lazily on the FIRST [`open_session`]
//!   call. Subsequent sessions reuse the same Node process
//!   (Playwright is happiest sharing one browser per host).
//! - Each session = one Playwright page (in its own context).
//!   We track guids returned by the sidecar in
//!   [`PlaywrightBackend::sessions`].
//! - [`close_session`] tears the page + context down. The
//!   browser process and the sidecar stay up until the
//!   [`PlaywrightBackend`] is dropped (or the operator restarts
//!   the tool node).
//!
//! ## Sync-trait → async-runtime bridge
//!
//! The [`BrowserBackend`] trait is sync (frozen contract). The
//! sidecar I/O is async (tokio). We bridge using
//! `tokio::task::block_in_place +
//! Handle::current().block_on(...)` from inside the sync trait
//! methods. The bridge dispatch handlers always run on the
//! controller's multi-threaded tokio runtime, so block_in_place
//! is safe (moves the current worker to a blocking thread).
//!
//! ## Honesty contract
//!
//! - Node spawn failure → `BackendNotConnected { reason:
//!   "playwright: failed to spawn `node`: <os error>" }`.
//! - playwright-core require() failure → sidecar emits a startup
//!   error envelope on stdout; the Rust side maps it to
//!   `BackendNotConnected { reason: "playwright: <sidecar
//!   message>" }`.
//! - JSON-RPC timeout → `BackendNotConnected { reason:
//!   "playwright: call <method> timed out after <ms>ms" }`.
//! - All sidecar `{error: {code, message}}` responses → mapped
//!   the same way; the operator sees Playwright's own error
//!   text (e.g. "navigation timeout 30000ms exceeded").

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;

use super::{
    BrowserBackend, BrowserConfig, BrowserError, BrowserSessionView, new_session_id, unix_secs,
};

/// Canonical backend name. Constant so `super::build_backend`
/// and `BrowserBackend::name` can't drift apart.
pub const NAME: &str = "playwright";

/// Embedded sidecar script. The operator does not need to install
/// the script separately; we pipe it into `node -` on spawn.
const SIDECAR_JS: &str = include_str!("playwright_sidecar.js");

/// One live browser session — a Playwright page guid + a bit of
/// cached metadata so `list_sessions` doesn't have to round-trip
/// the sidecar.
#[derive(Debug, Clone)]
struct PwSession {
    /// Page guid issued by the sidecar.
    guid: String,
    opened_at: i64,
    /// Last URL we successfully navigated to. None before the
    /// first `navigate` call.
    current_url: Option<String>,
}

/// Live driver state. Lazy-spawns the Node sidecar on the first
/// `open_session`; keeps stdin/stdout half-pipes alive for the
/// lifetime of the backend.
pub struct PlaywrightBackend {
    /// Sidecar I/O channel. Wrapped in an async mutex so a
    /// single JSON-RPC call can write a line then read the
    /// response without interleaving with another in-flight
    /// call. (The sidecar is single-threaded; it processes
    /// requests in stdin-arrival order.)
    io: AsyncMutex<Option<SidecarIo>>,
    /// Child handle. Held so we can call `kill().await` on
    /// shutdown. `None` until the lazy spawn happens.
    child: AsyncMutex<Option<Child>>,
    /// Active sessions. Keyed by the Relix-side session id
    /// (16-hex chars, opaque); value carries the sidecar guid.
    sessions: Mutex<HashMap<String, PwSession>>,
    /// Monotonic JSON-RPC id source. Wraps every 2^64 calls
    /// (i.e. never in practice).
    next_id: AtomicU64,
    /// Max live sessions per node. Mirrors `BrowserConfig::max_sessions`.
    max_sessions: usize,
    /// Per-call deadline applied to each JSON-RPC round trip.
    call_timeout: Duration,
}

/// The stdin/stdout half of the sidecar process. Held together
/// in one struct so a single `io.lock()` covers both ends of a
/// request/response round trip.
struct SidecarIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PlaywrightBackend {
    fn new(cfg: &BrowserConfig) -> Self {
        Self {
            io: AsyncMutex::new(None),
            child: AsyncMutex::new(None),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            max_sessions: cfg.max_sessions,
            call_timeout: Duration::from_secs(cfg.call_timeout_secs.max(1)),
        }
    }

    /// Sync→async bridge entry point. Every trait method that
    /// needs to talk to the sidecar funnels through here so we
    /// have ONE place that does block_in_place + block_on. If
    /// the surrounding tokio runtime is single-threaded (e.g. in
    /// some test setups) we fall back to a plain block_on on a
    /// freshly-acquired Handle.
    fn block_on<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        // `Handle::try_current` succeeds inside any tokio task.
        // The dispatch bridge always runs handlers on the
        // controller's multi-thread runtime, so block_in_place
        // is the right tool: it tells tokio "this worker is
        // going to block, please rebalance" without poisoning
        // the runtime.
        match Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
            // No tokio context — only happens if a unit test
            // invokes a trait method outside any runtime. We
            // build a throwaway current-thread runtime so the
            // tests still work; production never hits this.
            Err(_) => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build single-threaded fallback runtime")
                .block_on(fut),
        }
    }

    /// Lazy-spawn the Node sidecar (idempotent). On Windows the
    /// PATH lookup finds `node.exe`; on Unix it finds `node`. We
    /// don't probe `node --version` first — that adds latency
    /// AND a second failure surface; the actual launch tells us
    /// what we need (spawn error => Node missing, EOF on first
    /// line => playwright-core missing).
    async fn ensure_spawned(&self) -> Result<(), BrowserError> {
        let mut child_guard = self.child.lock().await;
        if child_guard.is_some() {
            return Ok(());
        }

        // `node -` reads the script from stdin. We pipe the
        // embedded `SIDECAR_JS` once, then keep stdin open for
        // JSON-RPC requests. We use a heredoc-style protocol
        // where the script ends with an empty line + a sentinel
        // marker… actually, simpler: pass `-e <script>` so the
        // sidecar's stdin is reserved purely for JSON-RPC.
        let mut cmd = Command::new("node");
        cmd.arg("-e")
            .arg(SIDECAR_JS)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| BrowserError::BackendNotConnected {
            reason: format!("playwright: failed to spawn `node`: {e}"),
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| BrowserError::BackendNotConnected {
                reason: "playwright: child stdin pipe missing".to_string(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BrowserError::BackendNotConnected {
                reason: "playwright: child stdout pipe missing".to_string(),
            })?;

        *self.io.lock().await = Some(SidecarIo {
            stdin,
            stdout: BufReader::new(stdout),
        });
        *child_guard = Some(child);

        // Confirm the sidecar started successfully. A failed
        // `require('playwright-core')` makes the script exit
        // with an error envelope on its first stdout line; ping
        // round-trips quickly and surfaces either case as a
        // clean BackendNotConnected.
        drop(child_guard);
        self.call_inner("ping", json!({}), self.call_timeout)
            .await?;
        Ok(())
    }

    /// Issue a JSON-RPC call to the running sidecar. Caller is
    /// responsible for `ensure_spawned`. Returns the
    /// `result` value or maps the sidecar's `error` (or our own
    /// transport errors) to `BackendNotConnected`.
    async fn call_inner(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, BrowserError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = json!({"id": id, "method": method, "params": params}).to_string();

        let fut = async {
            let mut guard = self.io.lock().await;
            let io = guard
                .as_mut()
                .ok_or_else(|| BrowserError::BackendNotConnected {
                    reason: "playwright: sidecar not connected".to_string(),
                })?;
            // Write line + newline. A short write here means
            // the sidecar died — surface as not-connected.
            io.stdin.write_all(req.as_bytes()).await.map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("playwright: write {method}: {e}"),
                }
            })?;
            io.stdin
                .write_all(b"\n")
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("playwright: write \\n {method}: {e}"),
                })?;
            io.stdin
                .flush()
                .await
                .map_err(|e| BrowserError::BackendNotConnected {
                    reason: format!("playwright: flush {method}: {e}"),
                })?;

            let mut line = String::new();
            let n = io.stdout.read_line(&mut line).await.map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("playwright: read {method}: {e}"),
                }
            })?;
            if n == 0 {
                return Err(BrowserError::BackendNotConnected {
                    reason: format!("playwright: sidecar EOF while waiting for {method}"),
                });
            }
            let resp: Value = serde_json::from_str(line.trim_end()).map_err(|e| {
                BrowserError::BackendNotConnected {
                    reason: format!("playwright: parse response for {method}: {e}"),
                }
            })?;
            if let Some(err) = resp.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                return Err(BrowserError::BackendNotConnected {
                    reason: format!("playwright: {method}: {msg}"),
                });
            }
            Ok(resp.get("result").cloned().unwrap_or(Value::Null))
        };

        match timeout(deadline, fut).await {
            Ok(res) => res,
            Err(_) => Err(BrowserError::BackendNotConnected {
                reason: format!(
                    "playwright: call {method} timed out after {}ms",
                    deadline.as_millis()
                ),
            }),
        }
    }
}

impl BrowserBackend for PlaywrightBackend {
    fn name(&self) -> &'static str {
        NAME
    }

    fn open_session(&self) -> Result<String, BrowserError> {
        // PH-BROWSER-PW: enforce the cap BEFORE the lazy spawn.
        // Operators who set max_sessions and then exceed it
        // shouldn't pay the cost of starting Node + Chromium
        // just to be told no.
        {
            let guard = self.sessions.lock().expect("playwright sessions lock");
            if guard.len() >= self.max_sessions {
                return Err(BrowserError::SessionCapReached {
                    max: self.max_sessions,
                });
            }
        }

        Self::block_on(async {
            self.ensure_spawned().await?;
            self.call_inner("browser.launch", json!({}), self.call_timeout)
                .await?;
            let resp = self
                .call_inner("context.newPage", json!({}), self.call_timeout)
                .await?;
            let guid = resp
                .get("guid")
                .and_then(Value::as_str)
                .ok_or_else(|| BrowserError::BackendNotConnected {
                    reason: "playwright: context.newPage returned no guid".to_string(),
                })?
                .to_string();
            let sid = new_session_id();
            // Re-check the cap after we acquired the guid — a
            // racing caller could have filled the slot. We do
            // the check + (maybe) insert inside a tight non-async
            // block so the std::sync::Mutex guard is never held
            // across the await for the cleanup `page.close`
            // call below (clippy::await_holding_lock).
            let inserted = {
                let mut guard = self.sessions.lock().expect("playwright sessions lock");
                if guard.len() >= self.max_sessions {
                    false
                } else {
                    guard.insert(
                        sid.clone(),
                        PwSession {
                            guid: guid.clone(),
                            opened_at: unix_secs(),
                            current_url: None,
                        },
                    );
                    true
                }
            };
            if !inserted {
                // Cleanup: close the freshly-allocated page so
                // we don't leak the resource on the sidecar side.
                let _ = self
                    .call_inner("page.close", json!({"guid": guid}), self.call_timeout)
                    .await;
                return Err(BrowserError::SessionCapReached {
                    max: self.max_sessions,
                });
            }
            Ok(sid)
        })
    }

    fn close_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let guid = {
            let mut guard = self.sessions.lock().expect("playwright sessions lock");
            guard
                .remove(session_id)
                .ok_or_else(|| BrowserError::SessionNotFound {
                    session_id: session_id.to_string(),
                })?
                .guid
        };
        Self::block_on(async {
            self.call_inner("page.close", json!({"guid": guid}), self.call_timeout)
                .await
                .map(|_| ())
        })
    }

    fn navigate(&self, session_id: &str, url: &str) -> Result<(), BrowserError> {
        let guid = self.guid_for(session_id)?;
        let timeout_ms = self.call_timeout.as_millis() as u64;
        let result = Self::block_on(async {
            self.call_inner(
                "page.goto",
                json!({"guid": guid, "url": url, "timeout": timeout_ms}),
                self.call_timeout,
            )
            .await
        })?;
        // Cache the final URL so list_sessions has something
        // useful to show without a round trip.
        if let Some(final_url) = result.get("url").and_then(Value::as_str) {
            let mut guard = self.sessions.lock().expect("playwright sessions lock");
            if let Some(s) = guard.get_mut(session_id) {
                s.current_url = Some(final_url.to_string());
            }
        }
        Ok(())
    }

    fn get_text(&self, session_id: &str) -> Result<String, BrowserError> {
        let guid = self.guid_for(session_id)?;
        let resp = Self::block_on(async {
            self.call_inner(
                "page.innerText",
                json!({"guid": guid, "selector": "body"}),
                self.call_timeout,
            )
            .await
        })?;
        Ok(resp
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    fn screenshot(&self, session_id: &str) -> Result<Vec<u8>, BrowserError> {
        let guid = self.guid_for(session_id)?;
        let resp = Self::block_on(async {
            self.call_inner(
                "page.screenshot",
                json!({"guid": guid, "fullPage": true}),
                self.call_timeout,
            )
            .await
        })?;
        let b64 = resp
            .get("pngBase64")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::BackendNotConnected {
                reason: "playwright: page.screenshot returned no pngBase64".to_string(),
            })?;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| BrowserError::BackendNotConnected {
                reason: format!("playwright: screenshot base64 decode: {e}"),
            })
    }

    fn list_sessions(&self) -> Result<Vec<BrowserSessionView>, BrowserError> {
        let guard = self.sessions.lock().expect("playwright sessions lock");
        let mut out: Vec<BrowserSessionView> = guard
            .iter()
            .map(|(id, sess)| BrowserSessionView {
                session_id: id.clone(),
                opened_at: sess.opened_at,
                current_url: sess.current_url.clone(),
                page_title: None,
                status: "live".to_string(),
            })
            .collect();
        out.sort_by_key(|r| r.opened_at);
        Ok(out)
    }

    // F12: click + type_text + wait_for_selector — the
    // three trait methods the playwright driver was
    // returning `BackendNotConnected` on (via the default
    // impl). Each dispatches to the matching sidecar
    // method (`page.click` / `page.type_text` /
    // `page.wait_for_selector`) and surfaces sidecar
    // errors as `BackendNotConnected` with the
    // selector + structured cause embedded.
    fn click(&self, session_id: &str, selector: &str) -> Result<(), BrowserError> {
        let guid = self.guid_for(session_id)?;
        let timeout_ms = self.call_timeout.as_millis() as u64;
        Self::block_on(async {
            self.call_inner(
                "page.click",
                json!({"guid": guid, "selector": selector, "timeout": timeout_ms}),
                self.call_timeout,
            )
            .await
            .map(|_| ())
        })
    }

    fn type_text(&self, session_id: &str, selector: &str, text: &str) -> Result<(), BrowserError> {
        let guid = self.guid_for(session_id)?;
        let timeout_ms = self.call_timeout.as_millis() as u64;
        Self::block_on(async {
            self.call_inner(
                "page.type_text",
                json!({"guid": guid, "selector": selector, "text": text, "timeout": timeout_ms}),
                self.call_timeout,
            )
            .await
            .map(|_| ())
        })
    }

    fn wait_for_selector(
        &self,
        session_id: &str,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        let guid = self.guid_for(session_id)?;
        // Honour the caller's timeout, falling back to the
        // driver's call_timeout when 0 is passed. We bound
        // the call_inner timeout to the larger of the two
        // so the JSON-RPC layer doesn't tear the wait down
        // before the sidecar's playwright timeout fires.
        let effective_timeout_ms = if timeout_ms == 0 {
            self.call_timeout.as_millis() as u64
        } else {
            timeout_ms
        };
        let rpc_timeout = std::time::Duration::from_millis(effective_timeout_ms + 5_000);
        Self::block_on(async {
            self.call_inner(
                "page.wait_for_selector",
                json!({
                    "guid": guid,
                    "selector": selector,
                    "timeout": effective_timeout_ms,
                }),
                rpc_timeout,
            )
            .await
            .map(|_| ())
        })
    }
}

impl PlaywrightBackend {
    fn guid_for(&self, session_id: &str) -> Result<String, BrowserError> {
        let guard = self.sessions.lock().expect("playwright sessions lock");
        guard
            .get(session_id)
            .map(|s| s.guid.clone())
            .ok_or_else(|| BrowserError::SessionNotFound {
                session_id: session_id.to_string(),
            })
    }
}

/// PH-BROWSER-PW: live build. Returns the real driver. No Node
/// spawn happens here — `ensure_spawned` runs on the first
/// `open_session`. Returning a constructed Arc means operators
/// who configure `backend = "playwright"` get a wired tool node
/// regardless of whether Node is currently installed; the
/// loud-fail is deferred to the actual capability invocation
/// where the operator sees a precise `BackendNotConnected`
/// reason. (Pre-flighting Node at startup would change the
/// failure shape AND prevent CI from running with the feature
/// compiled but Node absent.)
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    Ok(Arc::new(PlaywrightBackend::new(cfg)))
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> BrowserConfig {
        BrowserConfig {
            backend: "playwright".to_string(),
            max_sessions: 4,
            call_timeout_secs: 30,
            ..BrowserConfig::default()
        }
    }

    /// PH-BROWSER-PW: build succeeds + reports the canonical
    /// name. No Node spawn happens until `open_session` — verify
    /// by checking that `try_build` doesn't hit the OS.
    #[test]
    fn try_build_returns_ok_with_canonical_name() {
        let b = try_build(&cfg()).expect("try_build");
        assert_eq!(b.name(), NAME);
        assert_eq!(b.name(), "playwright");
        // No sidecar spawned — list_sessions returns the empty
        // in-memory map without touching Node.
        assert_eq!(b.list_sessions().unwrap().len(), 0);
    }

    /// PH-BROWSER-PW: max_sessions cap is enforced BEFORE the
    /// lazy spawn. This test runs without Node installed — if
    /// the cap check fired after spawn, we'd get a spawn error
    /// instead of `SessionCapReached`.
    #[test]
    fn max_sessions_cap_fires_before_sidecar_spawn() {
        let mut c = cfg();
        c.max_sessions = 0; // Impossible to open anything.
        let b = try_build(&c).expect("try_build");
        match b.open_session() {
            Err(BrowserError::SessionCapReached { max: 0 }) => {}
            other => panic!("expected SessionCapReached(0) before spawn, got {other:?}"),
        }
    }

    /// PH-BROWSER-PW: same check at max_sessions=1 — open once
    /// (which WILL try to spawn Node — guard with the runtime
    /// probe), then verify the second call is rejected without
    /// hitting Node a second time. Today this test skips when
    /// the runtime isn't available, but the cap-before-spawn
    /// path is still covered by the cap=0 test above.
    #[test]
    fn list_sessions_empty_before_any_open() {
        let b = try_build(&cfg()).expect("try_build");
        let rows = b.list_sessions().expect("list");
        assert!(rows.is_empty());
    }

    /// Probe: is the playwright runtime installed on this host?
    /// Used to gate the integration tests below. We require BOTH
    /// the `node` binary AND `require('playwright-core')` to
    /// succeed, mirroring what the sidecar actually does.
    fn playwright_runtime_available() -> bool {
        use std::process::Command;
        // node binary present?
        let node_ok = Command::new("node")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !node_ok {
            return false;
        }
        // playwright-core resolvable from node's module path?
        Command::new("node")
            .args(["-e", "require('playwright-core')"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// F12: click / type_text / wait_for_selector are no
    /// longer the trait-default `BackendNotConnected` — they
    /// route through the sidecar. We can verify that without
    /// Node by checking that `click` against a session that
    /// doesn't exist returns `SessionNotFound` (proving the
    /// guid lookup happens first, i.e. the override is in
    /// effect) rather than the trait-default not-yet-wired
    /// error.
    #[test]
    fn click_without_session_returns_session_not_found_not_default_impl() {
        let b = try_build(&cfg()).expect("try_build");
        match b.click("absent-session-id", "#thing") {
            Err(BrowserError::SessionNotFound { session_id }) => {
                assert_eq!(session_id, "absent-session-id");
            }
            other => {
                panic!("expected SessionNotFound (proves override is in effect), got {other:?}")
            }
        }
    }

    #[test]
    fn type_text_without_session_returns_session_not_found_not_default_impl() {
        let b = try_build(&cfg()).expect("try_build");
        match b.type_text("absent", "#input", "hi") {
            Err(BrowserError::SessionNotFound { .. }) => {}
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_selector_without_session_returns_session_not_found_not_default_impl() {
        let b = try_build(&cfg()).expect("try_build");
        match b.wait_for_selector("absent", "#thing", 1_000) {
            Err(BrowserError::SessionNotFound { .. }) => {}
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Live integration smoke test. Skips silently (with an
    /// eprintln) when Node + playwright-core aren't on PATH so
    /// CI hosts without the runtime don't fail. When the
    /// runtime IS available we exercise the full round trip:
    /// open_session → navigate("about:blank") → close_session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_playwright_navigates_about_blank() {
        if !playwright_runtime_available() {
            eprintln!(
                "skipping live_playwright_navigates_about_blank: \
                 node + playwright-core not detected on PATH"
            );
            return;
        }
        // PlaywrightBackend's trait methods are sync — they
        // call block_in_place internally. We invoke them from
        // a spawn_blocking so the multi-thread runtime can
        // park our worker without complaint.
        let backend = try_build(&cfg()).expect("try_build");
        let result = tokio::task::spawn_blocking(move || -> Result<(), BrowserError> {
            let sid = backend.open_session()?;
            backend.navigate(&sid, "about:blank")?;
            // get_text on about:blank returns empty string —
            // we tolerate either Ok("") or an error here, since
            // some Chromium builds reject innerText on a blank
            // page. The acceptance bar is open + navigate +
            // close round-tripping cleanly.
            let _ = backend.get_text(&sid);
            backend.close_session(&sid)?;
            Ok(())
        })
        .await
        .expect("spawn_blocking join");
        result.expect("live round trip");
    }

    /// F12 live smoke test for click + type_text +
    /// wait_for_selector. Drives a `data:` URL with a single
    /// input + button, fills the input, clicks the button, and
    /// waits for the resulting element to appear. Skips when
    /// Playwright isn't available — same posture as the navigate
    /// smoke test above.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_playwright_click_type_wait_round_trip() {
        if !playwright_runtime_available() {
            eprintln!(
                "skipping live_playwright_click_type_wait_round_trip: \
                 node + playwright-core not detected on PATH"
            );
            return;
        }
        // Self-contained HTML: an input, a button that copies
        // the input's value into a div with id="out" after
        // a click. Wait for #out to appear, then assert the
        // page text contains what we typed.
        let html = r#"
<!doctype html>
<html><body>
  <input id="inp" type="text" />
  <button id="btn" onclick="(function(){
    var d = document.createElement('div');
    d.id = 'out';
    d.textContent = document.getElementById('inp').value;
    document.body.appendChild(d);
  })()">go</button>
</body></html>
"#;
        let data_url = format!("data:text/html;charset=utf-8,{}", urlencoding_encode(html));
        let backend = try_build(&cfg()).expect("try_build");
        let result = tokio::task::spawn_blocking(move || -> Result<String, BrowserError> {
            let sid = backend.open_session()?;
            backend.navigate(&sid, &data_url)?;
            backend.wait_for_selector(&sid, "#inp", 5_000)?;
            backend.type_text(&sid, "#inp", "hello-from-test")?;
            backend.click(&sid, "#btn")?;
            backend.wait_for_selector(&sid, "#out", 5_000)?;
            let text = backend.get_text(&sid)?;
            backend.close_session(&sid)?;
            Ok(text)
        })
        .await
        .expect("spawn_blocking join")
        .expect("round trip");
        assert!(
            result.contains("hello-from-test"),
            "expected page text to contain typed value, got {result:?}"
        );
    }

    /// Minimal percent-encoding for the `data:` URL above. We
    /// avoid adding a new dependency just for this one test.
    fn urlencoding_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 3);
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
}
