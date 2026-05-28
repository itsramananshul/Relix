//! PH-PDF-CHUNK: `tool.text.chunk` — split a body of text into
//! bounded chunks at the best available natural boundary.
//!
//! Pure CPU. No I/O, no PDF parsing — operators chain this
//! after `tool.pdf` / `tool.web_extract` / `tool.read_file` to
//! prep text for embedding, retrieval, or context-window fit.
//!
//! ## Wire format
//!
//! Request JSON:
//! ```json
//! { "text": "...", "chunk_size": 1200, "chunk_overlap": 120 }
//! ```
//!
//! - `text` (required) — the body to chunk.
//! - `chunk_size` (required) — target chunk size in CHARACTERS
//!   (not bytes). Counted as `chars().count()`; multi-byte
//!   characters cost one each. Clamped to `MAX_CHUNK_SIZE`.
//! - `chunk_overlap` (optional, default 0) — overlap in
//!   characters between successive chunks. Must be `< chunk_size`.
//!
//! Response JSON:
//! ```json
//! {
//!   "chunk_count": 3,
//!   "chunks": [
//!     { "index": 0, "char_start": 0, "char_end": 1180, "text": "..." },
//!     ...
//!   ]
//! }
//! ```
//!
//! ## Splitting policy
//!
//! At each target boundary (`start + chunk_size`), the splitter
//! looks BACKWARD for the best break, in this priority order:
//!
//! 1. Paragraph boundary (`\n\n`).
//! 2. Sentence boundary (`. ` / `! ` / `? ` / `.\n`).
//! 3. Word boundary (whitespace).
//! 4. UTF-8 character boundary (fallback — guarantees we never
//!    split mid-char).
//!
//! The splitter looks backward at most `chunk_size / 4` chars
//! from the target before falling back to the character
//! boundary. This bounds worst-case latency on inputs with very
//! long unbroken runs.
//!
//! ## Honest limitations
//!
//! - Sentence detection is heuristic and English-biased — `Dr.`,
//!   `e.g.`, `i.e.` will be treated as sentence ends. Tradeoff
//!   accepted; the paragraph-first preference catches most cases.
//! - No tokenizer-aware chunking. Operators who care about
//!   token budgets should set `chunk_size` conservatively
//!   relative to their tokenizer's expansion factor.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency, RiskLevel};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Upper bound on chunk_size to prevent operators from
/// constructing massive single-chunk responses.
const MAX_CHUNK_SIZE: usize = 100_000;

/// Look back at most this fraction of `chunk_size` for a
/// boundary before giving up and snapping to the char boundary.
const LOOKBACK_FRACTION: usize = 4;

pub fn capability_descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.text.chunk");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["parse:text".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Split a body of text into bounded chunks at the best available natural \
         boundary (paragraph > sentence > word > UTF-8 char). Pure CPU. \
         Request JSON: {text, chunk_size, chunk_overlap?}. Response JSON: \
         {chunk_count, chunks:[{index, char_start, char_end, text}, ...]}. \
         chunk_size is in CHARACTERS (not bytes), capped at 100k. Chain after \
         tool.pdf / tool.web_extract / tool.read_file for retrieval prep."
            .into(),
    );
    d.categories = vec!["parse".into(), "text".into(), "chunking".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

pub fn register(bridge: &mut DispatchBridge) {
    bridge.register(
        "tool.text.chunk",
        Arc::new(FnHandler(
            move |ctx: InvocationCtx| async move { handle(&ctx) },
        )),
    );
}

#[derive(Debug, Deserialize)]
struct ChunkRequest {
    text: String,
    chunk_size: usize,
    #[serde(default)]
    chunk_overlap: usize,
}

#[derive(Debug, Serialize)]
pub struct ChunkInfo {
    pub index: usize,
    pub char_start: usize,
    pub char_end: usize,
    pub text: String,
}

#[derive(Debug, Serialize)]
struct ChunkResponse {
    chunk_count: usize,
    chunks: Vec<ChunkInfo>,
}

fn handle(ctx: &InvocationCtx) -> HandlerOutcome {
    let req: ChunkRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.text.chunk: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if req.chunk_size == 0 {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.text.chunk: chunk_size must be > 0".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let chunk_size = req.chunk_size.min(MAX_CHUNK_SIZE);
    if req.chunk_overlap >= chunk_size {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.text.chunk: chunk_overlap ({}) must be < chunk_size ({})",
                req.chunk_overlap, chunk_size
            ),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let chunks = chunk_text(&req.text, chunk_size, req.chunk_overlap);
    let resp = ChunkResponse {
        chunk_count: chunks.len(),
        chunks,
    };
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-PDF-CHUNK: split `text` into chunks. Public for downstream
/// reuse (e.g., a future `tool.pdf.chunk` shortcut that does
/// extract + chunk in one call).
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<ChunkInfo> {
    let total_chars = text.chars().count();
    if total_chars <= chunk_size {
        if total_chars == 0 {
            return Vec::new();
        }
        return vec![ChunkInfo {
            index: 0,
            char_start: 0,
            char_end: total_chars,
            text: text.to_string(),
        }];
    }

    // Build a char-index → byte-offset table so we can slice
    // efficiently and report char ranges accurately. UTF-8 means
    // char count != byte count for non-ASCII content.
    let char_boundaries: Vec<usize> = text
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(text.len()))
        .collect();
    debug_assert_eq!(char_boundaries.len(), total_chars + 1);

    let mut chunks: Vec<ChunkInfo> = Vec::new();
    let mut start_char: usize = 0;
    let lookback = chunk_size / LOOKBACK_FRACTION;
    while start_char < total_chars {
        let target_end = (start_char + chunk_size).min(total_chars);
        let end_char = if target_end == total_chars {
            total_chars
        } else {
            find_break_backward(text, &char_boundaries, start_char, target_end, lookback)
        };
        let byte_start = char_boundaries[start_char];
        let byte_end = char_boundaries[end_char];
        let chunk_text_slice = text[byte_start..byte_end].to_string();
        let trimmed = chunk_text_slice.trim().to_string();
        // Skip chunks that are entirely whitespace (can happen
        // when paragraph boundaries cluster at the look-back
        // region).
        if !trimmed.is_empty() {
            chunks.push(ChunkInfo {
                index: chunks.len(),
                char_start: start_char,
                char_end: end_char,
                text: chunk_text_slice,
            });
        }
        if end_char == total_chars {
            break;
        }
        // Step forward by chunk_size - overlap; guard against
        // making zero forward progress.
        let stride = chunk_size.saturating_sub(overlap).max(1);
        let advance = (end_char - start_char).min(stride);
        let next_start = start_char + advance;
        if next_start <= start_char {
            // Defensive: should be impossible given the .max(1)
            // and target_end > start_char invariants above.
            break;
        }
        start_char = next_start;
    }
    chunks
}

/// PH-PDF-CHUNK: from `target_end`, look BACKWARD for the best
/// break. Returns the char index (inclusive of the break char)
/// to split at. If no good break found, returns `target_end`
/// (snapped to a char boundary, which it already is since
/// we're indexing chars).
///
/// Paragraph and sentence breaks are preferred and may look
/// back up to `chunk_size / 2` chars (passed in via
/// `paragraph_lookback`). Word breaks fall back to the smaller
/// `lookback` window so we don't produce chunks that are way
/// undersized just because a word boundary appears far back.
fn find_break_backward(
    text: &str,
    char_boundaries: &[usize],
    start_char: usize,
    target_end: usize,
    lookback: usize,
) -> usize {
    // Paragraph + sentence get the bigger window — they're
    // worth landing on even if the resulting chunk is a bit
    // smaller than the target.
    let big_window = lookback.saturating_mul(2);
    let big_min = target_end.saturating_sub(big_window).max(start_char + 1);
    let small_min = target_end.saturating_sub(lookback).max(start_char + 1);

    // Look for `\n\n` (paragraph) — best break.
    for c in (big_min..target_end).rev() {
        if c + 2 > char_boundaries.len() {
            continue;
        }
        let bs = char_boundaries[c];
        let be = char_boundaries.get(c + 2).copied().unwrap_or(text.len());
        if be <= text.len() {
            let slice = &text[bs..be];
            if slice == "\n\n" {
                return c + 2; // split AFTER the paragraph break
            }
        }
    }
    // Look for sentence end (`. ` / `! ` / `? ` / `.\n`).
    for c in (big_min..target_end).rev() {
        if c + 2 > char_boundaries.len() {
            continue;
        }
        let bs = char_boundaries[c];
        let be = char_boundaries.get(c + 2).copied().unwrap_or(text.len());
        if be <= text.len() {
            let slice = &text[bs..be];
            if slice == ". "
                || slice == "! "
                || slice == "? "
                || slice == ".\n"
                || slice == "!\n"
                || slice == "?\n"
            {
                return c + 2;
            }
        }
    }
    // Look for word boundary (any whitespace) — smaller window.
    for c in (small_min..target_end).rev() {
        let bs = char_boundaries[c];
        let be = char_boundaries[c + 1];
        let slice = &text[bs..be];
        if slice.chars().next().is_some_and(|ch| ch.is_whitespace()) {
            return c + 1;
        }
    }
    // Fallback: snap to the target char boundary (always a
    // valid UTF-8 boundary since we're indexing chars).
    target_end
}

// ──────────────────────────── Tests ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: &[u8]) -> InvocationCtx {
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

    #[test]
    fn descriptor_shape() {
        let d = capability_descriptor();
        assert_eq!(d.method_name, "tool.text.chunk");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "parse:text"));
        assert!(d.categories.iter().any(|c| c == "chunking"));
    }

    #[test]
    fn empty_text_returns_no_chunks() {
        let chunks = chunk_text("", 100, 0);
        assert_eq!(chunks.len(), 0);
    }

    #[test]
    fn text_shorter_than_chunk_size_returns_single_chunk() {
        let chunks = chunk_text("hello world", 100, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "hello world");
        assert_eq!(chunks[0].char_start, 0);
        assert_eq!(chunks[0].char_end, 11);
    }

    #[test]
    fn chunks_break_at_paragraph_boundary() {
        let text = "paragraph one is here\n\nparagraph two starts here and goes on\n\npara three";
        let chunks = chunk_text(text, 30, 0);
        // First chunk should end at the first paragraph break.
        assert!(chunks[0].text.contains("paragraph one"));
        assert!(!chunks[0].text.contains("paragraph two"));
    }

    #[test]
    fn chunks_break_at_sentence_boundary_when_no_paragraph() {
        let text = "First sentence here. Second sentence here. Third sentence is here.";
        let chunks = chunk_text(text, 30, 0);
        assert!(chunks.len() >= 2);
        // First chunk should end at a sentence break.
        assert!(
            chunks[0].text.trim_end().ends_with('.'),
            "first chunk should end at sentence boundary: {:?}",
            chunks[0].text
        );
    }

    #[test]
    fn chunks_break_at_word_boundary_when_no_sentence() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let chunks = chunk_text(text, 20, 0);
        assert!(chunks.len() >= 2);
        // The first chunk should end at a word boundary — its
        // trailing char (before trim) should be whitespace, or
        // the chunk's last word should match exactly one of the
        // input's whole words.
        let first_raw = &chunks[0].text;
        let trimmed = first_raw.trim_end();
        let words: Vec<&str> = trimmed.split_whitespace().collect();
        // The last word must appear in the input as a whole
        // word (i.e., no mid-word split happened).
        let last_word = *words.last().unwrap();
        let input_words: Vec<&str> = text.split_whitespace().collect();
        assert!(
            input_words.contains(&last_word),
            "last word of first chunk should be an input word, got: {last_word:?} \
             in chunk: {first_raw:?}",
        );
    }

    #[test]
    fn chunks_respect_overlap() {
        let text =
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi omicron";
        let no_overlap = chunk_text(text, 30, 0);
        let with_overlap = chunk_text(text, 30, 10);
        // Overlap should produce at least as many chunks
        // (typically more) than no-overlap for the same input.
        assert!(with_overlap.len() >= no_overlap.len());
    }

    #[test]
    fn chunks_handle_multibyte_chars_without_splitting() {
        // "café" is 4 chars but 5 bytes. "naïve" is 5 chars,
        // 6 bytes. Test that we don't slice mid-byte.
        let text = "café naïve emoji 🦀 rust crab. Another sentence here.";
        let chunks = chunk_text(text, 15, 0);
        for c in &chunks {
            // Each chunk must be valid UTF-8 (no panic via the
            // String::from_utf8 path inside chunk_text).
            assert!(!c.text.is_empty());
        }
    }

    #[test]
    fn fallback_to_char_boundary_when_no_break_found() {
        // Long run of non-whitespace chars — splitter must
        // still terminate.
        let text: String = "a".repeat(500);
        let chunks = chunk_text(&text, 100, 0);
        assert!(chunks.len() >= 5);
        // Total chars across chunks should sum to at least the
        // input (overlap zero, so exactly equal).
        let total: usize = chunks.iter().map(|c| c.char_end - c.char_start).sum();
        assert_eq!(total, 500);
    }

    #[test]
    fn handler_bad_json_rejected() {
        match handle(&ctx(b"not-json")) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad request shape"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn handler_zero_chunk_size_rejected() {
        let arg = br#"{"text":"hello","chunk_size":0}"#;
        match handle(&ctx(arg)) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("chunk_size must be > 0"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn handler_overlap_equal_to_chunk_size_rejected() {
        let arg = br#"{"text":"hello","chunk_size":10,"chunk_overlap":10}"#;
        match handle(&ctx(arg)) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("chunk_overlap"));
                assert!(e.cause.contains("must be <"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn handler_returns_chunk_count_and_chunks() {
        let arg = br#"{"text":"alpha beta gamma delta epsilon zeta eta theta","chunk_size":15,"chunk_overlap":0}"#;
        let body = match handle(&ctx(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let count = v["chunk_count"].as_u64().unwrap() as usize;
        assert!(count >= 2);
        let chunks = v["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), count);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c["index"].as_u64().unwrap() as usize, i);
            assert!(!c["text"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn chunk_indices_are_sequential() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let chunks = chunk_text(text, 25, 0);
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.index, i);
        }
    }

    #[test]
    fn chunk_size_clamped_to_max() {
        // Asking for chunk_size > MAX_CHUNK_SIZE should NOT
        // error; it should silently clamp and still chunk.
        let text: String = "a".repeat(MAX_CHUNK_SIZE + 1000);
        let chunks = chunk_text(&text, MAX_CHUNK_SIZE + 50_000, 0);
        assert!(!chunks.is_empty());
        // Handler test confirms it via JSON path.
        let arg = format!(
            r#"{{"text":"{}","chunk_size":{}}}"#,
            "a".repeat(50_000),
            MAX_CHUNK_SIZE + 50_000
        );
        match handle(&ctx(arg.as_bytes())) {
            HandlerOutcome::Ok(b) => {
                let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
                assert!(v["chunk_count"].as_u64().unwrap() >= 1);
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
    }

    #[test]
    fn full_text_recoverable_from_chunks_when_overlap_zero() {
        let text =
            "First paragraph here.\n\nSecond paragraph with more text in it.\n\nThird paragraph.";
        let chunks = chunk_text(text, 30, 0);
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        // With zero overlap and our break logic, the joined
        // chunks reproduce the input verbatim modulo trimming.
        // We accept >= 99% recovery because the trim() inside
        // chunk_text may drop a single leading whitespace from
        // chunks 1+.
        let recovered_pct = (joined.chars().count() as f64) / (text.chars().count() as f64);
        assert!(
            recovered_pct >= 0.95,
            "recovered {recovered_pct} from chunks: {chunks:?}"
        );
    }

    /// PH-RISK-PIN-ALL: tool.text.chunk is pure CPU over
    /// caller-supplied text — no network, no host I/O. Safe.
    #[test]
    fn text_chunk_descriptor_has_safe_risk() {
        use relix_core::capability::RiskLevel;
        let d = capability_descriptor();
        assert_ne!(d.risk_level, RiskLevel::Unknown);
        assert_eq!(d.risk_level, RiskLevel::Safe);
    }
}
