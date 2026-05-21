//! PH-BROWSER-FEATURES — `headless_chrome` backend module.
//!
//! Compiled only when `--features browser-headless-chrome` is
//! set. PH-BROWSER-HC will replace the scaffold with a live
//! Chrome DevTools Protocol driver against the operator's
//! `chrome` / `chromium` binary. Today the feature compiles a
//! pass-through that returns BackendNotConnected on every
//! non-trivial call, with a reason that names the upcoming
//! milestone tag so operators reading the error see exactly
//! what's pending.
//!
//! Selection at runtime is driven by `[tool.browser] backend =
//! "headless_chrome"` in the operator config. With the feature
//! disabled, [`super::build_backend`] returns
//! [`super::BrowserError::FeatureNotCompiled`] — no silent
//! NoneBackend fallback.
//!
//! Recommended default per D-008: this backend has the
//! smallest install footprint (no Node, no Playwright npm
//! package, no sidecar driver). Operators who don't want to
//! install extra runtimes should pick this one.

use std::sync::Arc;

use super::{BrowserBackend, BrowserConfig, BrowserError, NoneBackend};

/// PH-BROWSER-FEATURES: canonical backend name string. Kept as
/// a constant so the dispatch in `super::build_backend` and the
/// `name()` label on the scaffold stay in sync.
pub const NAME: &str = "headless_chrome";

/// PH-BROWSER-FEATURES: scaffolded build. The live impl lands
/// in PH-BROWSER-HC. Returning a labeled NoneBackend here means:
/// 1. The capability surface is wired (dispatch works).
/// 2. `name()` reports `"headless_chrome"` so the dashboard /
///    list_sessions reflect the operator's choice.
/// 3. Every call past `open_session` / `close_session` /
///    `list_sessions` returns BackendNotConnected with a
///    reason that names the milestone tag.
///
/// When PH-BROWSER-HC ships, replace this body with the real
/// driver wiring. The trait surface stays frozen.
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    let reason = "PH-BROWSER-HC pending: the `browser-headless-chrome` feature is \
                  compiled, but the live Chrome DevTools Protocol driver has not \
                  shipped yet. The capability surface is wired (open_session / \
                  close_session / list_sessions work as scaffolds); navigate / \
                  get_text / screenshot return BackendNotConnected until the \
                  driver lands. See docs/browser-tool.md."
        .to_string();
    Ok(Arc::new(NoneBackend::with_label(NAME, cfg, reason)))
}
