//! H1 — Structured failover classifier (Hermes-inspired).
//!
//! Hermes's `classify_api_error()` returns a `FailoverReason` enum that
//! drives 17 distinct retry / failover code paths (rate-limit ladder,
//! credential rotation, image strip, beta-header disable, context
//! compression, model fallback, …). Relix's existing provider layer
//! only distinguishes `Transient` vs `Permanent` — a coarse binary
//! that loses the information needed to choose the right recovery.
//!
//! This module ships the typed classification surface without yet
//! wiring it into automatic failover. The classifier is pure
//! (input: HTTP status + response-body excerpt; output: enum) so it's
//! cheap to call everywhere and easy to test. Downstream consumers in
//! follow-up milestones:
//!
//! - **Provider quarantine** (M69): a `RateLimitGenuine` repeatedly
//!   from the same provider could trigger automatic cooldown instead
//!   of waiting for an operator to flip the quarantine switch.
//! - **Routing trace** (M77): every routing-failure entry carries the
//!   classified reason so the dashboard's provider trace timeline can
//!   show "rate-limited" vs "context overflow" vs "transport error"
//!   without parsing free-form error strings.
//! - **Circuit breaker** (deferred): cumulative `ContextOverflow`
//!   errors flag a model whose context window doesn't fit the system
//!   prompt; the operator UI surfaces the misconfiguration.
//!
//! ## Stability contract
//!
//! New `FailoverReason` variants are additive. Consumers must use
//! exhaustive matches with a default arm or call [`FailoverReason::category`]
//! to project to a smaller fixed set.
//!
//! ## What this does NOT do
//!
//! - No automatic retry. The classifier returns a *reason*; the
//!   policy of what to do next belongs to the caller.
//! - No body parsing beyond substring scans. The full body is the
//!   provider's own error JSON / HTML — we don't try to parse every
//!   shape (OpenAI vs Anthropic vs Gemini differ). Substrings are
//!   sufficient for the categories that drive different recovery.

use std::time::Duration;

/// One of N distinct failure modes the classifier can identify. The
/// runtime treats unknown failures as [`FailoverReason::Unknown`] and
/// downstream code falls back to the existing Transient / Permanent
/// behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    /// 429 with a clear rate-limit signal. Caller should respect
    /// `Retry-After` or back off; quarantine after repeated hits.
    RateLimitGenuine,
    /// 429 emitted while waiting for credentials to rotate. Same
    /// HTTP code but the recovery is "try the next API key", not
    /// "wait".
    RateLimitCredentialRotation,
    /// 5xx transient server failure. Always-safe retry, possibly
    /// against a fallback provider.
    Server5xx,
    /// 408 / 504 / connection timeout. Retry with longer deadline
    /// or switch provider.
    Timeout,
    /// 401 / 403. The credential is rejected — operator must rotate.
    AuthRejected,
    /// 400 + body mentions "context length", "maximum context",
    /// "too many tokens", etc. The request exceeds the model's window;
    /// the caller should compress history before retrying.
    ContextOverflow,
    /// 413 OR body mentions "request too large". Distinct from
    /// context overflow — the *body* is too big regardless of
    /// model. Caller should chunk or shrink.
    PayloadTooLarge,
    /// 400 + body mentions image-related rejection. Caller should
    /// strip images or downscale them.
    ImageRejected,
    /// 404 / 400 + body mentions "model not found", "does not
    /// exist", "no such model". Caller should switch model.
    ModelNotFound,
    /// 400 / 422 — caller sent malformed JSON or invalid args.
    /// Almost never retryable; usually a code bug.
    InvalidRequest,
    /// `reqwest::Error` failure before any HTTP response — DNS,
    /// TLS, RST. Almost-always-safe retry against an unrelated
    /// provider; same provider may succeed on a transient blip.
    TransportFailure,
    /// Catch-all when no other variant fits. Downstream caller
    /// keeps the existing Transient/Permanent handling.
    Unknown,
}

impl FailoverReason {
    /// Project to a coarse 3-class category for callers that don't
    /// care about every variant: `transient` (retry helps), `permanent`
    /// (retry doesn't), `compress` (the request needs to shrink first).
    /// Future operator-facing badge colour mapping uses this.
    pub fn category(&self) -> FailoverCategory {
        match self {
            Self::RateLimitGenuine
            | Self::RateLimitCredentialRotation
            | Self::Server5xx
            | Self::Timeout
            | Self::TransportFailure => FailoverCategory::Transient,
            Self::AuthRejected | Self::ModelNotFound | Self::InvalidRequest | Self::Unknown => {
                FailoverCategory::Permanent
            }
            Self::ContextOverflow | Self::PayloadTooLarge | Self::ImageRejected => {
                FailoverCategory::Compress
            }
        }
    }

    /// Hint for the per-error retry backoff, in seconds. Returns
    /// `None` when retry is not advised. Mirrors Hermes's per-class
    /// backoff: rate-limit waits the longest, transport gets the
    /// shortest pause.
    pub fn retry_after_hint(&self) -> Option<Duration> {
        match self {
            Self::RateLimitGenuine => Some(Duration::from_secs(20)),
            Self::RateLimitCredentialRotation => Some(Duration::from_millis(500)),
            Self::Server5xx => Some(Duration::from_secs(2)),
            Self::Timeout => Some(Duration::from_secs(5)),
            Self::TransportFailure => Some(Duration::from_secs(1)),
            // Compress-class: caller mutates the request before retrying.
            Self::ContextOverflow | Self::PayloadTooLarge | Self::ImageRejected => {
                Some(Duration::from_millis(0))
            }
            // Permanent / unknown: caller decides.
            Self::AuthRejected | Self::ModelNotFound | Self::InvalidRequest | Self::Unknown => None,
        }
    }

    /// Short stable label, suitable for structured logging fields and
    /// dashboard badge text. Lowercase + dashes, no whitespace.
    pub fn label(&self) -> &'static str {
        match self {
            Self::RateLimitGenuine => "rate-limit",
            Self::RateLimitCredentialRotation => "rate-limit-rotation",
            Self::Server5xx => "server-5xx",
            Self::Timeout => "timeout",
            Self::AuthRejected => "auth-rejected",
            Self::ContextOverflow => "context-overflow",
            Self::PayloadTooLarge => "payload-too-large",
            Self::ImageRejected => "image-rejected",
            Self::ModelNotFound => "model-not-found",
            Self::InvalidRequest => "invalid-request",
            Self::TransportFailure => "transport-failure",
            Self::Unknown => "unknown",
        }
    }
}

/// Coarse retry-policy category for the per-error decision matrix.
/// See [`FailoverReason::category`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverCategory {
    /// Same provider may succeed on retry after the suggested delay.
    Transient,
    /// Retry will not help — caller must change something (key,
    /// model, request shape).
    Permanent,
    /// Request needs to be smaller / different before a retry can
    /// succeed.
    Compress,
}

/// Classify an HTTP response that returned a status code. Body is
/// optional; when provided, substring scans improve precision for
/// 4xx codes (`ContextOverflow` vs `PayloadTooLarge` vs
/// `ModelNotFound`).
///
/// The classifier is intentionally provider-agnostic. It does not
/// inspect headers (callers can layer that in via separate logic);
/// it does not parse JSON (substring-on-body is enough for the
/// categories that drive different recovery).
pub fn classify_http_failure(status: u16, body_excerpt: &str) -> FailoverReason {
    let lower = body_lower_excerpt(body_excerpt);

    match status {
        429 => {
            if lower.contains("rotating") || lower.contains("rotation") {
                FailoverReason::RateLimitCredentialRotation
            } else {
                FailoverReason::RateLimitGenuine
            }
        }
        408 | 504 => FailoverReason::Timeout,
        401 | 403 => FailoverReason::AuthRejected,
        413 => FailoverReason::PayloadTooLarge,
        s if (500..600).contains(&s) => FailoverReason::Server5xx,
        400 | 422 => classify_4xx_body(&lower),
        404 => {
            if mentions_model_missing(&lower) {
                FailoverReason::ModelNotFound
            } else {
                FailoverReason::InvalidRequest
            }
        }
        _ => FailoverReason::Unknown,
    }
}

/// Classify a transport-level failure (no HTTP response, e.g. DNS,
/// TLS, RST mid-stream). The reason here is `TransportFailure` —
/// the caller decides whether to retry on the same provider or
/// route to a fallback.
pub fn classify_transport_failure(_err_str: &str) -> FailoverReason {
    // Today we don't sub-classify (could distinguish DNS vs TLS in
    // the future). The label is enough for operator surfacing.
    FailoverReason::TransportFailure
}

// ─────────────────────────── helpers ───────────────────────────

/// Cap body excerpt + lowercase for substring scans. 4 KiB is plenty
/// for error-shape detection without quadratic-cost scans on huge
/// bodies.
fn body_lower_excerpt(body: &str) -> String {
    let mut cap = 4096.min(body.len());
    // Snap the cap down to a char boundary: response bodies are
    // network input and 4096 can land inside a multi-byte codepoint.
    while cap > 0 && !body.is_char_boundary(cap) {
        cap -= 1;
    }
    body[..cap].to_ascii_lowercase()
}

fn classify_4xx_body(lower: &str) -> FailoverReason {
    if mentions_context_overflow(lower) {
        return FailoverReason::ContextOverflow;
    }
    if mentions_payload_too_large(lower) {
        return FailoverReason::PayloadTooLarge;
    }
    if mentions_image_rejected(lower) {
        return FailoverReason::ImageRejected;
    }
    if mentions_model_missing(lower) {
        return FailoverReason::ModelNotFound;
    }
    FailoverReason::InvalidRequest
}

fn mentions_context_overflow(lower: &str) -> bool {
    lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("too many tokens")
        || lower.contains("max_tokens")
        || lower.contains("token limit")
}

fn mentions_payload_too_large(lower: &str) -> bool {
    lower.contains("request too large")
        || lower.contains("payload too large")
        || lower.contains("entity too large")
        || lower.contains("body exceeds")
}

fn mentions_image_rejected(lower: &str) -> bool {
    lower.contains("image too large")
        || lower.contains("image size")
        || lower.contains("invalid image")
        || lower.contains("image format")
        || lower.contains("vision not supported")
}

fn mentions_model_missing(lower: &str) -> bool {
    lower.contains("model not found")
        || lower.contains("does not exist")
        || lower.contains("no such model")
        || lower.contains("unknown model")
        || lower.contains("invalid model")
}

// ─────────────────────────── Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_genuine_from_429_no_rotation() {
        let r = classify_http_failure(429, r#"{"error":"rate limit exceeded"}"#);
        assert_eq!(r, FailoverReason::RateLimitGenuine);
        assert_eq!(r.category(), FailoverCategory::Transient);
        assert!(r.retry_after_hint().is_some());
    }

    #[test]
    fn rate_limit_credential_rotation_from_429_with_rotation_hint() {
        let r = classify_http_failure(429, "rotating credentials, please retry");
        assert_eq!(r, FailoverReason::RateLimitCredentialRotation);
        assert_eq!(r.category(), FailoverCategory::Transient);
    }

    #[test]
    fn server_5xx() {
        assert_eq!(classify_http_failure(500, ""), FailoverReason::Server5xx);
        assert_eq!(classify_http_failure(502, ""), FailoverReason::Server5xx);
        assert_eq!(classify_http_failure(503, ""), FailoverReason::Server5xx);
        // 504 is timeout, distinct from generic 5xx.
        assert_eq!(classify_http_failure(504, ""), FailoverReason::Timeout);
        assert_eq!(classify_http_failure(599, ""), FailoverReason::Server5xx);
    }

    #[test]
    fn timeout_codes() {
        assert_eq!(classify_http_failure(408, ""), FailoverReason::Timeout);
        assert_eq!(classify_http_failure(504, ""), FailoverReason::Timeout);
    }

    #[test]
    fn auth_rejected() {
        assert_eq!(classify_http_failure(401, ""), FailoverReason::AuthRejected);
        assert_eq!(classify_http_failure(403, ""), FailoverReason::AuthRejected);
        assert_eq!(
            classify_http_failure(401, "").category(),
            FailoverCategory::Permanent
        );
    }

    #[test]
    fn payload_too_large_413() {
        assert_eq!(
            classify_http_failure(413, ""),
            FailoverReason::PayloadTooLarge
        );
        assert_eq!(
            classify_http_failure(413, "").category(),
            FailoverCategory::Compress
        );
    }

    #[test]
    fn context_overflow_via_body() {
        let body = r#"{"error":"This model's maximum context length is 8192"}"#;
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::ContextOverflow
        );
        assert_eq!(
            classify_http_failure(400, body).category(),
            FailoverCategory::Compress
        );
    }

    #[test]
    fn payload_too_large_via_body() {
        let body = "Request too large for upstream service";
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::PayloadTooLarge
        );
    }

    #[test]
    fn image_rejected_via_body() {
        let body = "image too large for vision pipeline";
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::ImageRejected
        );
    }

    #[test]
    fn model_not_found_via_404() {
        let body = r#"{"error":"model not found"}"#;
        assert_eq!(
            classify_http_failure(404, body),
            FailoverReason::ModelNotFound
        );
    }

    #[test]
    fn model_not_found_via_400_body() {
        let body = "The model `gpt-fictional-9` does not exist";
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::ModelNotFound
        );
    }

    #[test]
    fn invalid_request_default_400() {
        let body = r#"{"error":"missing required field 'messages'"}"#;
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::InvalidRequest
        );
    }

    #[test]
    fn invalid_request_422() {
        assert_eq!(
            classify_http_failure(422, ""),
            FailoverReason::InvalidRequest
        );
    }

    #[test]
    fn unknown_status() {
        assert_eq!(classify_http_failure(418, ""), FailoverReason::Unknown);
    }

    #[test]
    fn transport_failure_label() {
        let r = classify_transport_failure("dns lookup failed");
        assert_eq!(r, FailoverReason::TransportFailure);
        assert_eq!(r.label(), "transport-failure");
        assert!(r.retry_after_hint().is_some());
    }

    #[test]
    fn labels_are_stable_kebab_case() {
        // Sanity: no whitespace, no uppercase, dashes only.
        for v in [
            FailoverReason::RateLimitGenuine,
            FailoverReason::RateLimitCredentialRotation,
            FailoverReason::Server5xx,
            FailoverReason::Timeout,
            FailoverReason::AuthRejected,
            FailoverReason::ContextOverflow,
            FailoverReason::PayloadTooLarge,
            FailoverReason::ImageRejected,
            FailoverReason::ModelNotFound,
            FailoverReason::InvalidRequest,
            FailoverReason::TransportFailure,
            FailoverReason::Unknown,
        ] {
            let l = v.label();
            assert!(!l.is_empty());
            assert!(
                l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "label has invalid char: {l}"
            );
        }
    }

    #[test]
    fn retry_hint_only_for_retryable_and_compress_classes() {
        for v in [
            FailoverReason::RateLimitGenuine,
            FailoverReason::RateLimitCredentialRotation,
            FailoverReason::Server5xx,
            FailoverReason::Timeout,
            FailoverReason::TransportFailure,
            FailoverReason::ContextOverflow,
            FailoverReason::PayloadTooLarge,
            FailoverReason::ImageRejected,
        ] {
            assert!(v.retry_after_hint().is_some(), "expected hint for {v:?}");
        }
        for v in [
            FailoverReason::AuthRejected,
            FailoverReason::ModelNotFound,
            FailoverReason::InvalidRequest,
            FailoverReason::Unknown,
        ] {
            assert!(v.retry_after_hint().is_none(), "expected NO hint for {v:?}");
        }
    }

    #[test]
    fn body_lower_excerpt_handles_large_bodies() {
        let huge = "X".repeat(100_000);
        let r = classify_http_failure(400, &huge);
        // Just don't blow up; classification falls into InvalidRequest
        // because the body has no recognizable substrings.
        assert_eq!(r, FailoverReason::InvalidRequest);
    }

    #[test]
    fn body_lower_excerpt_handles_unicode() {
        // Non-ASCII char in the body — to_ascii_lowercase keeps
        // multi-byte chars intact (it only lowercases ASCII bytes).
        let body = "résumé: context length exceeded";
        assert_eq!(
            classify_http_failure(400, body),
            FailoverReason::ContextOverflow
        );
    }
}
