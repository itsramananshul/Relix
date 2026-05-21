//! PH-BROWSER-FEATURES — `playwright` backend module.
//!
//! Compiled only when `--features browser-playwright` is set.
//! PH-BROWSER-PW will replace the scaffold with a Playwright
//! sidecar driver (Node + Playwright npm package over stdio
//! JSON-RPC). Today the feature compiles a pass-through that
//! returns BackendNotConnected on every non-trivial call, with a
//! reason that names the upcoming milestone tag.
//!
//! Selection at runtime is driven by `[tool.browser] backend =
//! "playwright"` in the operator config. With the feature
//! disabled, [`super::build_backend`] returns
//! [`super::BrowserError::FeatureNotCompiled`] — no silent
//! NoneBackend fallback.
//!
//! Trade-offs vs `headless_chrome` (D-008):
//! - Heavier install (Node + browsers + Playwright npm).
//! - Best multi-engine coverage (Chromium / Firefox / WebKit).
//! - Most mature automation API.

use std::sync::Arc;

use super::{BrowserBackend, BrowserConfig, BrowserError, NoneBackend};

pub const NAME: &str = "playwright";

/// PH-BROWSER-FEATURES: scaffolded build. The live impl lands
/// in PH-BROWSER-PW. See `headless_chrome::try_build` for the
/// scaffold-shape rationale.
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    let reason = "PH-BROWSER-PW pending: the `browser-playwright` feature is \
                  compiled, but the live Playwright sidecar driver has not \
                  shipped yet. The capability surface is wired; navigate / \
                  get_text / screenshot return BackendNotConnected until the \
                  driver lands. See docs/browser-tool.md."
        .to_string();
    Ok(Arc::new(NoneBackend::with_label(NAME, cfg, reason)))
}
