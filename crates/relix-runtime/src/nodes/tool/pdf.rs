//! `tool.pdf` — deterministic PDF parsing.
//!
//! Operates on caller-supplied PDF bytes (base64-encoded over the
//! UTF-8 wire — SIMP-016). No rendering, no OCR, no embedded browser,
//! no JavaScript. The capability extracts:
//!
//! - visible text (page-ordered, lightly normalised)
//! - page count
//! - document metadata (title / author / subject / creator / producer)
//!
//! Built directly on `lopdf` (pure Rust, no system deps). We deliberately
//! don't pull in `pdf-extract` because its FreeType binding would add
//! a system dependency to the workspace.
//!
//! ## Wire format (SIMP-016 alpha — UTF-8 strings)
//!
//! Arg: `<mode>|<base64_pdf>`. Modes:
//!
//! | Mode  | Returns |
//! |-------|---------|
//! | `text`  | Page-ordered visible text, normalised whitespace. |
//! | `pages` | Page count as decimal string. |
//! | `meta`  | One `name\tvalue` per line: title, author, subject, creator, producer (omitted when absent). |
//! | `all`   | Multi-line `key=value` summary: `pages=`, `meta:<k>=<v>`, then `text=` followed by the text body (capped). |
//!
//! ## Limits
//!
//! - `[tool] pdf_max_input_bytes` — default 20 MiB. Inputs over the
//!   cap reject as `invalid_args` before any parsing.
//! - `[tool] pdf_max_pages` — default 200. PDFs with more pages reject.
//! - `[tool] pdf_max_output_chars` — default 200_000. Extracted text
//!   beyond this cap is truncated with a `... [truncated]` marker.
//!
//! ## Honest limitations
//!
//! - Text extraction is a **light** implementation. PDFs are
//!   notoriously hostile to faithful text extraction — embedded font
//!   encodings, ligatures, columnar layout, complex graphics state —
//!   none of which we handle. What we DO handle:
//!   - Text shown via `Tj` (show string), `TJ` (show array of strings
//!     with kerning), `'` and `"` (next-line variants).
//!   - Literal-string and hex-string operands.
//!   - Page-level boundaries (`\n\n` between pages).
//!
//!   This covers most "PDF generated from a word processor" cases.
//!   Scanned PDFs (image-only) will yield empty text — that's an OCR
//!   problem, explicitly out of scope.
//! - Metadata reads the `/Info` dictionary only. PDF/A `/Metadata`
//!   XMP stream is not parsed.
//! - Encrypted PDFs are not supported. Rejection is silent (parser
//!   error surfaces as a generic `parse failed` cause).

use std::sync::Arc;

use base64::Engine;
use serde::Deserialize;

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency, RiskLevel};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

#[derive(Clone, Debug, Deserialize)]
pub struct PdfConfig {
    #[serde(default = "default_max_input")]
    pub max_input_bytes: usize,
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
    #[serde(default = "default_max_output_chars")]
    pub max_output_chars: usize,
}

impl Default for PdfConfig {
    fn default() -> Self {
        Self {
            max_input_bytes: default_max_input(),
            max_pages: default_max_pages(),
            max_output_chars: default_max_output_chars(),
        }
    }
}

fn default_max_input() -> usize {
    20 * 1024 * 1024
}
fn default_max_pages() -> usize {
    200
}
fn default_max_output_chars() -> usize {
    200_000
}

pub fn capability_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.pdf");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Expensive; // CPU bound to input size
    d.sensitivity_tags = vec!["parse:pdf".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Parse PDF bytes; extract page count / metadata / text. Pure-Rust (lopdf), \
         no system deps."
            .into(),
    );
    d.categories = vec!["parse".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn register(bridge: &mut DispatchBridge, cfg: Arc<PdfConfig>) {
    let cfg_for_handler = cfg.clone();
    bridge.register(
        "tool.pdf",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let cfg = cfg_for_handler.clone();
            async move { handle(&cfg, &ctx) }
        })),
    );
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Text,
    Pages,
    Meta,
    All,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "text" => Some(Self::Text),
            "pages" => Some(Self::Pages),
            "meta" => Some(Self::Meta),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

fn handle(cfg: &PdfConfig, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.pdf arg utf8: {e}")),
    };
    let mut parts = raw.splitn(2, '|');
    let mode_str = parts.next().unwrap_or("").trim();
    let b64 = parts.next().unwrap_or("").trim();
    if mode_str.is_empty() || b64.is_empty() {
        return invalid(
            "tool.pdf arg must be `<mode>|<base64_pdf>` (modes: text/pages/meta/all)".into(),
        );
    }
    let mode = match Mode::parse(mode_str) {
        Some(m) => m,
        None => {
            return invalid(format!(
                "tool.pdf: unknown mode '{mode_str}' (text/pages/meta/all)"
            ));
        }
    };

    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => return invalid(format!("tool.pdf base64 decode: {e}")),
    };
    if bytes.len() > cfg.max_input_bytes {
        return invalid(format!(
            "tool.pdf: input {} bytes exceeds cap {}",
            bytes.len(),
            cfg.max_input_bytes
        ));
    }

    let doc = match lopdf::Document::load_mem(&bytes) {
        Ok(d) => d,
        Err(e) => return invalid(format!("tool.pdf parse failed: {e}")),
    };

    let pages = doc.get_pages();
    let page_count = pages.len();
    if page_count > cfg.max_pages {
        return invalid(format!(
            "tool.pdf: {} pages exceeds cap {}",
            page_count, cfg.max_pages
        ));
    }

    let body = match mode {
        Mode::Pages => page_count.to_string(),
        Mode::Meta => render_meta_lines(&doc),
        Mode::Text => extract_text(&doc, &pages, cfg.max_output_chars),
        Mode::All => {
            let meta = render_meta_lines(&doc);
            let text = extract_text(&doc, &pages, cfg.max_output_chars);
            use std::fmt::Write as _;
            let mut s = String::new();
            let _ = writeln!(s, "pages={page_count}");
            for line in meta.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    let _ = writeln!(s, "meta:{k}={v}");
                }
            }
            let _ = writeln!(s, "text=");
            s.push_str(&text);
            s
        }
    };
    HandlerOutcome::Ok(body.into_bytes())
}

/// Render document metadata one `name\tvalue` per line, omitting
/// absent fields. Keys come from the PDF `/Info` dictionary.
pub(crate) fn render_meta_lines(doc: &lopdf::Document) -> String {
    let mut out = String::new();
    let fields = [
        ("title", "Title"),
        ("author", "Author"),
        ("subject", "Subject"),
        ("creator", "Creator"),
        ("producer", "Producer"),
    ];
    let Ok(info_obj_id) = doc.trailer.get(b"Info") else {
        return out;
    };
    let Some(info) = info_obj_id
        .as_reference()
        .ok()
        .and_then(|id| doc.get_object(id).ok())
        .and_then(|o| o.as_dict().ok())
    else {
        return out;
    };
    for (lower, key) in fields {
        if let Ok(v) = info.get(key.as_bytes()) {
            let s = decode_text_string(v);
            if !s.is_empty() {
                use std::fmt::Write as _;
                let _ = writeln!(out, "{lower}\t{s}");
            }
        }
    }
    out
}

/// Decode a PDF "text string" object — either a Literal (ASCII or
/// PDFDocEncoding) or a HexString. UTF-16 BOM-prefixed strings get
/// decoded; everything else falls back to lossy UTF-8.
fn decode_text_string(obj: &lopdf::Object) -> String {
    match obj {
        lopdf::Object::String(bytes, _format) => bytes_to_text(bytes),
        _ => String::new(),
    }
}

fn bytes_to_text(bytes: &[u8]) -> String {
    // UTF-16 BE BOM
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let words: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&words);
    }
    // UTF-16 LE BOM
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let words: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&words);
    }
    // Otherwise lossy UTF-8 (PDFDocEncoding is close enough for ASCII).
    String::from_utf8_lossy(bytes).into_owned()
}

/// Walk the document page-by-page, pulling the content stream and
/// rendering text-show operators.
pub(crate) fn extract_text(
    doc: &lopdf::Document,
    pages: &std::collections::BTreeMap<u32, lopdf::ObjectId>,
    max_chars: usize,
) -> String {
    let mut out = String::new();
    let mut first = true;
    for &page_num in pages.keys() {
        if out.chars().count() >= max_chars {
            out.push_str("\n... [truncated]");
            break;
        }
        if !first {
            out.push_str("\n\n");
        }
        first = false;
        match doc.extract_text(&[page_num]) {
            Ok(t) => out.push_str(&t),
            Err(_) => {
                // Page failed to extract — skip but note it. We do not
                // bail the whole run; one bad page shouldn't blank a
                // whole document.
                out.push_str("[page extraction failed]");
            }
        }
    }
    // Final char-count cap with truncation marker.
    if out.chars().count() > max_chars {
        let head: String = out.chars().take(max_chars).collect();
        return format!("{head}\n... [truncated]");
    }
    out
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid PDF in memory using lopdf so the xref
    /// offsets are always correct. One page, a Type1 font, a
    /// "Hello, World!" `Tj` show. No font embedded; lopdf does the
    /// serialisation.
    fn build_minimal_pdf() -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{Document, Object, Stream, dictionary};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 24.into()]),
                Operation::new("Td", vec![100.into(), 700.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello, World!")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Contents" => content_id,
            "Resources" => resources_id,
        });
        let pages = dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "Resources" => resources_id,
        };
        doc.objects.insert(pages_id, Object::Dictionary(pages));
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        let info_id = doc.add_object(dictionary! {
            "Title" => Object::string_literal("Test Title"),
            "Author" => Object::string_literal("Relix Tests"),
        });
        doc.trailer.set("Root", catalog_id);
        doc.trailer.set("Info", info_id);
        doc.compress();
        let mut out = Vec::new();
        doc.save_to(&mut out).expect("save pdf");
        out
    }

    fn ctx(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    fn encode_pdf(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn page_count_on_minimal_pdf() {
        let cfg = PdfConfig::default();
        let pdf = build_minimal_pdf();
        let arg = format!("pages|{}", encode_pdf(&pdf));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), "1"),
            HandlerOutcome::Err(e) => panic!("pages failed: {}", e.cause),
        }
    }

    #[test]
    fn text_extracts_hello_world() {
        let cfg = PdfConfig::default();
        let pdf = build_minimal_pdf();
        let arg = format!("text|{}", encode_pdf(&pdf));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("Hello"), "expected 'Hello' in text, got: {s}");
                assert!(s.contains("World"), "expected 'World' in text, got: {s}");
            }
            HandlerOutcome::Err(e) => panic!("text failed: {}", e.cause),
        }
    }

    #[test]
    fn metadata_extracted_from_info_dict() {
        let cfg = PdfConfig::default();
        let pdf = build_minimal_pdf();
        let arg = format!("meta|{}", encode_pdf(&pdf));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(
                    s.contains("title\tTest Title"),
                    "expected title in meta, got: {s}"
                );
                assert!(
                    s.contains("author\tRelix Tests"),
                    "expected author in meta, got: {s}"
                );
            }
            HandlerOutcome::Err(e) => panic!("meta failed: {}", e.cause),
        }
    }

    #[test]
    fn all_mode_combines_pages_meta_text() {
        let cfg = PdfConfig::default();
        let pdf = build_minimal_pdf();
        let arg = format!("all|{}", encode_pdf(&pdf));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("pages=1"), "missing pages=, got: {s}");
                assert!(
                    s.contains("meta:title=Test Title"),
                    "missing meta:, got: {s}"
                );
                assert!(s.contains("text="), "missing text= section, got: {s}");
                assert!(s.contains("Hello"), "missing body, got: {s}");
            }
            HandlerOutcome::Err(e) => panic!("all failed: {}", e.cause),
        }
    }

    #[test]
    fn invalid_base64_rejected() {
        let cfg = PdfConfig::default();
        match handle(&cfg, &ctx(b"text|!!!not base64!!!")) {
            HandlerOutcome::Err(e) => {
                assert!(
                    e.cause.contains("base64") || e.cause.contains("decode"),
                    "got: {}",
                    e.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("expected base64 rejection"),
        }
    }

    #[test]
    fn malformed_pdf_rejected_cleanly() {
        let cfg = PdfConfig::default();
        let arg = format!("text|{}", encode_pdf(b"not actually a pdf"));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("parse failed"), "got: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected parse rejection"),
        }
    }

    #[test]
    fn oversize_input_rejected_before_parse() {
        let cfg = PdfConfig {
            max_input_bytes: 100,
            ..Default::default()
        };
        let big = vec![b'x'; 200];
        let arg = format!("text|{}", encode_pdf(&big));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("exceeds cap"), "got: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected oversize rejection"),
        }
    }

    #[test]
    fn unknown_mode_rejected() {
        let cfg = PdfConfig::default();
        let arg = format!("rasterize|{}", encode_pdf(&build_minimal_pdf()));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("unknown mode"), "got: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected unknown-mode rejection"),
        }
    }

    #[test]
    fn descriptor_is_expensive_and_tagged() {
        let d = capability_descriptor();
        assert_eq!(d.method_name, "tool.pdf");
        assert!(matches!(d.cost_class, CostClass::Expensive));
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(d.sensitivity_tags.iter().any(|t| t == "parse:pdf"));
    }

    // ── Track 6 hardening: malformed inputs do not panic ──

    #[test]
    fn empty_pdf_bytes_rejected_cleanly() {
        let cfg = PdfConfig::default();
        let arg = format!("text|{}", encode_pdf(b""));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(e) => {
                // Either "parse failed" or "empty" is acceptable;
                // what matters is it surfaces as INVALID_ARGS without
                // panicking.
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            HandlerOutcome::Ok(_) => panic!("expected rejection on empty input"),
        }
    }

    #[test]
    fn truncated_pdf_header_rejected_cleanly() {
        // `%PDF` without a proper version + xref + trailer.
        let cfg = PdfConfig::default();
        let arg = format!("text|{}", encode_pdf(b"%PDF-1.4\n"));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected rejection on truncated header"),
        }
    }

    #[test]
    fn pdf_with_high_bit_garbage_after_header_rejected() {
        // Header looks like a PDF but the rest is random bytes
        // including non-UTF-8 sequences. lopdf must surface an error,
        // not panic on UTF-8 conversion later.
        let mut bytes = b"%PDF-1.4\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0xfd, 0xfc, 0x00, 0x01]);
        let cfg = PdfConfig::default();
        let arg = format!("text|{}", encode_pdf(&bytes));
        match handle(&cfg, &ctx(arg.as_bytes())) {
            HandlerOutcome::Err(_) => {} // either parse or extract failure is fine
            HandlerOutcome::Ok(_) => panic!("expected rejection on garbage PDF"),
        }
    }

    #[test]
    fn mode_without_separator_rejected() {
        let cfg = PdfConfig::default();
        // Missing the `|` between mode and base64 body — should be
        // INVALID_ARGS, not a panic on str slicing.
        match handle(&cfg, &ctx(b"text")) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected rejection on malformed arg"),
        }
    }

    /// PH-RISK-PIN-ALL: tool.pdf is a pure parser over
    /// caller-supplied base64 bytes — no network, no host I/O,
    /// no shell. Safe tier.
    #[test]
    fn pdf_descriptor_has_safe_risk() {
        let d = capability_descriptor();
        assert_ne!(d.risk_level, RiskLevel::Unknown);
        assert_eq!(d.risk_level, RiskLevel::Safe);
    }
}
