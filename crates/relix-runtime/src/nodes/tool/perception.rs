//! `tool.parse_document` + `tool.web_read` — unified spec-named
//! perception caps.
//!
//! These are the §7.23 spec-named entry points for document +
//! web content extraction. They consolidate the existing
//! `tool.pdf` + `tool.web_get` machinery under the names the
//! roadmap calls out so SOL flows + planners can reference the
//! perception surface without juggling per-format caps.
//!
//! ## What ships now (simple tier)
//!
//! - **`tool.parse_document`** — content-kind-dispatched parser:
//!   - `text` / `markdown` / `code` — base64-decoded UTF-8 with
//!     a configurable output cap; emits the decoded text
//!     verbatim.
//!   - `pdf` — page-ordered text extraction via the existing
//!     pure-Rust lopdf pipeline (same code path `tool.pdf`
//!     uses; no system deps; no system-FreeType binding).
//! - **`tool.web_read`** — composed fetch + extract. Defers to
//!   the existing `tool.web_get` backend so SSRF / DNS-pin /
//!   per-hop redirect re-validation / content-type filter /
//!   body cap all apply unchanged.
//!
//! ## What does NOT ship (cloud + local tiers)
//!
//! The §7.23 spec calls for a tiered fallback `cloud → local →
//! simple` for both document and web reading:
//!
//! - `tool.parse_document` cloud tier (LlamaParse) and local
//!   tier (Docling, PyMuPDF) require either a paid hosted API
//!   or a Python sidecar; documented as
//!   EXTERNAL-INFRASTRUCTURE-DEFERRED in `docs/GAP_REPORT.md`.
//! - `tool.web_read` cloud tier (Crawl4AI / Jina Reader /
//!   Firecrawl) requires either a paid hosted API or a Python
//!   crawler sidecar; also
//!   EXTERNAL-INFRASTRUCTURE-DEFERRED.
//!
//! Operators wire those tiers in front of these caps via SOL
//! flows that try the external service first and fall through
//! to the simple tier on error. This module ships the simple
//! tier — the always-on, always-deterministic fallback the
//! spec mandates.

use std::sync::Arc;

use base64::Engine;
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::ToolBackend;
use super::pdf::PdfConfig;

/// Wire `tool.parse_document` + `tool.web_read` onto `bridge`.
/// Always registered; the PDF leg of `tool.parse_document`
/// silently falls through to `INVALID_ARGS` when the PDF
/// config is absent.
pub fn register(
    bridge: &mut DispatchBridge,
    backend: Arc<ToolBackend>,
    pdf_cfg: Option<Arc<PdfConfig>>,
) {
    let pdf_for_handler = pdf_cfg.clone();
    bridge.register(
        "tool.parse_document",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let pdf_cfg = pdf_for_handler.clone();
            async move { handle_parse_document(pdf_cfg.as_ref().map(|a| a.as_ref()), &ctx) }
        })),
    );
    bridge.register(
        "tool.web_read",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let b = backend.clone();
            async move { super::web_tools::handle_web_get_public(b, ctx).await }
        })),
    );
}

/// Pure-function implementation of `tool.parse_document` so the
/// unit tests can exercise every kind without standing up a
/// DispatchBridge.
pub(crate) fn handle_parse_document(
    pdf_cfg: Option<&PdfConfig>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.parse_document arg utf8: {e}")),
    };
    let mut parts = raw.splitn(2, '|');
    let kind = parts.next().unwrap_or("").trim();
    let payload = parts.next().unwrap_or("").trim();
    if kind.is_empty() || payload.is_empty() {
        return invalid(
            "tool.parse_document arg must be `<kind>|<base64_or_text>` \
             (kinds: text/markdown/code/pdf)"
                .into(),
        );
    }
    match kind {
        "text" | "markdown" | "code" => parse_text_kind(payload),
        "pdf" => match pdf_cfg {
            Some(cfg) => parse_pdf(cfg, payload),
            None => HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: "tool.parse_document: PDF parsing not enabled (set [tool.pdf])".into(),
                retry_hint: 0,
                retry_after: None,
            }),
        },
        other => invalid(format!(
            "tool.parse_document: unknown kind '{other}' (text/markdown/code/pdf)"
        )),
    }
}

fn parse_text_kind(payload: &str) -> HandlerOutcome {
    // Default cap mirrors the PDF output cap so dashboards see
    // consistent maxima. Operators who need more pass the raw
    // file content through `tool.text.chunk` after.
    const MAX_OUTPUT_CHARS: usize = 200_000;
    let bytes = match base64::engine::general_purpose::STANDARD.decode(payload) {
        Ok(b) => b,
        Err(e) => return invalid(format!("tool.parse_document base64 decode: {e}")),
    };
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.parse_document utf-8: {e}")),
    };
    let out = if text.chars().count() > MAX_OUTPUT_CHARS {
        let mut s: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
        s.push_str("\n... [truncated]\n");
        s
    } else {
        text
    };
    HandlerOutcome::Ok(out.into_bytes())
}

fn parse_pdf(cfg: &PdfConfig, payload: &str) -> HandlerOutcome {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(payload) {
        Ok(b) => b,
        Err(e) => return invalid(format!("tool.parse_document base64 decode: {e}")),
    };
    if bytes.len() > cfg.max_input_bytes {
        return invalid(format!(
            "tool.parse_document: input {} bytes exceeds cap {}",
            bytes.len(),
            cfg.max_input_bytes
        ));
    }
    let doc = match lopdf::Document::load_mem(&bytes) {
        Ok(d) => d,
        Err(e) => return invalid(format!("tool.parse_document pdf parse: {e}")),
    };
    let pages = doc.get_pages();
    if pages.len() > cfg.max_pages {
        return invalid(format!(
            "tool.parse_document: {} pages exceeds cap {}",
            pages.len(),
            cfg.max_pages
        ));
    }
    let text = super::pdf::extract_text(&doc, &pages, cfg.max_output_chars);
    HandlerOutcome::Ok(text.into_bytes())
}

fn invalid(msg: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg,
        retry_hint: 0,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    fn b64(s: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    }

    #[test]
    fn parse_document_text_returns_decoded_utf8() {
        let payload = format!("text|{}", b64("hello world"));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), "hello world"),
            HandlerOutcome::Err(e) => panic!("expected Ok, got Err: {:?}", e.cause),
        }
    }

    #[test]
    fn parse_document_markdown_round_trips_through_base64() {
        let md = "# Title\n\n- bullet one\n- bullet two\n";
        let payload = format!("markdown|{}", b64(md));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), md),
            HandlerOutcome::Err(e) => panic!("got Err: {:?}", e.cause),
        }
    }

    #[test]
    fn parse_document_code_kind_works() {
        let code = "fn main() { println!(\"hi\"); }\n";
        let payload = format!("code|{}", b64(code));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), code),
            HandlerOutcome::Err(_) => panic!("code kind should succeed"),
        }
    }

    #[test]
    fn parse_document_pdf_kind_rejects_without_pdf_config() {
        let payload = format!("pdf|{}", b64("fake-not-a-pdf"));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("PDF parsing not enabled"));
            }
            HandlerOutcome::Ok(_) => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn parse_document_rejects_unknown_kind() {
        let payload = format!("video|{}", b64("xxx"));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("unknown kind"));
            }
            HandlerOutcome::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn parse_document_rejects_malformed_args() {
        // Missing pipe.
        let outcome = handle_parse_document(None, &ctx(b"text"));
        match outcome {
            HandlerOutcome::Err(env) => assert_eq!(env.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn parse_document_rejects_invalid_base64() {
        let outcome = handle_parse_document(None, &ctx(b"text|!!!not-base64!!!"));
        match outcome {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("base64"));
            }
            HandlerOutcome::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn parse_document_truncates_oversize_text_output() {
        let mut huge = String::with_capacity(300_000);
        for _ in 0..300_000 {
            huge.push('x');
        }
        let payload = format!("text|{}", b64(&huge));
        let outcome = handle_parse_document(None, &ctx(payload.as_bytes()));
        match outcome {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("truncated"), "expected truncation marker");
                assert!(s.chars().count() < huge.chars().count());
            }
            HandlerOutcome::Err(e) => panic!("got Err: {:?}", e.cause),
        }
    }
}
