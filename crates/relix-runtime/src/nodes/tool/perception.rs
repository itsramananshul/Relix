//! Legacy perception entry point.
//!
//! The §7.23 perception caps (`tool.parse_document` + `tool.web_read`)
//! moved to [`super::parse_document`] in GAP 10 PART 1 / 2 when the
//! tiered cloud-fallthrough pipeline replaced the simple-tier
//! handler. This module is intentionally a near-empty stub — it
//! exists only to preserve the public-facing module name for
//! callers that referenced `nodes::tool::perception` before the
//! split (none in the workspace today; verified via grep).
//!
//! New code should call [`super::parse_document`] directly.

#[cfg(test)]
mod tests {
    /// Sanity check: the new tiered module exists and exports the
    /// `tool.parse_document` registration entry point. Pins the
    /// rename so any future refactor that removes
    /// `parse_document::register` is caught at compile time.
    #[test]
    fn parse_document_module_is_reachable() {
        let _ = super::super::parse_document::register;
        let _ = super::super::parse_document::register_web_read;
    }
}
