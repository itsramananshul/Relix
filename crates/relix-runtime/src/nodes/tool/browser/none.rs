//! PH-BROWSER-FEATURES — `NoneBackend`: the honest scaffold.
//!
//! Allocates session ids on `open_session` (and tracks them in a
//! small in-memory map so `list_sessions` surfaces what the
//! operator opened) but refuses every downstream navigate /
//! get_text / screenshot call with a `BackendNotConnected` error.
//! The scaffold each backend feature stub returns until the live
//! impl lands — see [`super::headless_chrome`] /
//! [`super::playwright`] / [`super::webdriver`] for the
//! per-backend reason strings.
//!
//! `NoneBackend` always reports `name()` as the backend label it
//! was constructed with — so an operator who selected
//! `headless_chrome` and got a scaffold for the feature still
//! sees `headless_chrome` in the dashboard / list_sessions
//! status, not `none`. Honesty: the operator-visible label
//! matches what they configured; the BackendNotConnected reason
//! makes the scaffold posture loud.

use std::collections::HashMap;
use std::sync::Mutex;

use super::{BrowserBackend, BrowserConfig, BrowserError, BrowserSessionView, new_session_id};

#[derive(Debug, Clone)]
struct NoneSession {
    opened_at: i64,
}

pub struct NoneBackend {
    /// Backend label exposed via [`BrowserBackend::name`].
    /// Always `"none"` when the operator literally selected
    /// `backend = "none"`; for feature-stub backends each module
    /// passes its own canonical name (e.g. `"headless_chrome"`).
    name: &'static str,
    max_sessions: usize,
    sessions: Mutex<HashMap<String, NoneSession>>,
    reason: String,
}

impl NoneBackend {
    /// Construct a NoneBackend that reports `name()` as `"none"`.
    /// Used by the `backend = "none"` path; feature scaffolds use
    /// [`with_label`] to expose the selected backend name even
    /// when the implementation is still pending.
    pub fn new(cfg: &BrowserConfig, reason: impl Into<String>) -> Self {
        Self::with_label("none", cfg, reason)
    }

    /// Construct a NoneBackend that reports `name()` as
    /// `label`. PH-BROWSER-FEATURES uses this so a scaffold
    /// stub for `headless_chrome` etc. surfaces the selected
    /// backend name to operators rather than the generic
    /// `"none"`.
    pub fn with_label(label: &'static str, cfg: &BrowserConfig, reason: impl Into<String>) -> Self {
        Self {
            name: label,
            max_sessions: cfg.max_sessions,
            sessions: Mutex::new(HashMap::new()),
            reason: reason.into(),
        }
    }

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

impl BrowserBackend for NoneBackend {
    fn name(&self) -> &'static str {
        self.name
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
                opened_at: super::unix_secs(),
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
