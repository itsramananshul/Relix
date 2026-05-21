//! PH-BROWSER-FEATURES — `webdriver` backend module.
//!
//! Compiled only when `--features browser-webdriver` is set.
//! PH-BROWSER-WD will replace the scaffold with a fantoccini
//! driver that talks WebDriver-over-HTTP to an operator-supplied
//! `chromedriver` / `geckodriver` sidecar process. Today the
//! feature compiles a pass-through that returns
//! BackendNotConnected on every non-trivial call, with a reason
//! that names the upcoming milestone tag.
//!
//! Selection at runtime is driven by `[tool.browser] backend =
//! "webdriver"` in the operator config. With the feature
//! disabled, [`super::build_backend`] returns
//! [`super::BrowserError::FeatureNotCompiled`] — no silent
//! NoneBackend fallback.
//!
//! Trade-offs vs `headless_chrome` / `playwright` (D-008):
//! - Most standards-aligned (W3C WebDriver).
//! - Requires the operator to install + run a separate
//!   driver binary alongside the tool node.

use std::sync::Arc;

use super::{BrowserBackend, BrowserConfig, BrowserError, NoneBackend};

pub const NAME: &str = "webdriver";

/// PH-BROWSER-FEATURES: scaffolded build. The live impl lands
/// in PH-BROWSER-WD. See `headless_chrome::try_build` for the
/// scaffold-shape rationale.
pub fn try_build(cfg: &BrowserConfig) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    let reason = "PH-BROWSER-WD pending: the `browser-webdriver` feature is \
                  compiled, but the live WebDriver driver has not shipped \
                  yet. The capability surface is wired; navigate / \
                  get_text / screenshot return BackendNotConnected until \
                  the driver lands. See docs/browser-tool.md."
        .to_string();
    Ok(Arc::new(NoneBackend::with_label(NAME, cfg, reason)))
}
