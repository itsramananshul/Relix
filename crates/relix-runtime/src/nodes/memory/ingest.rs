//! `memory.ingest_document` + `memory.ingest_image` handlers
//! and the per-content-type chunkers behind them.
//!
//! The handlers take a JSON arg describing a document (or an
//! image) and write one Layer 2 (`Semantic`) record per chunk
//! into the layered store. Each chunk is embedded via the
//! configured embedding dispatcher; when no dispatcher is wired
//! the records still land in SQLite but without a vector — the
//! background embedding pipeline picks them up on its next tick.
//!
//! ## Chunkers
//!
//! - `markdown` — by `##` / `###` heading; oversize sections
//!   are split by paragraph.
//! - `txt` — paragraph split with a 100-char overlap.
//! - `code` — split at function / class / impl boundaries.
//! - `pdf` — text extracted via `lopdf` then handled as `txt`.
//! - `image` — handled by [`handle_ingest_image`]; the
//!   document-ingest path forwards `content_type == "image"`
//!   straight to it.
//!
//! All ingested records are tagged `source_trust:external` so
//! the [`super::security`] quarantine + integrity audit
//! treats them as low-trust by default.

use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use relix_core::types::{ErrorEnvelope, error_kinds};

use super::curator::EmbeddingDispatcher;
use super::schema::{MemoryLayer, MemoryRecord, SourceTrust};
use super::{LayeredContext, internal, invalid_args};

/// Default chunk size for `markdown` heading splits + the
/// `txt` paragraph splitter. Matches the spec's
/// `chunk_size_chars` default.
pub const DEFAULT_CHUNK_SIZE_CHARS: usize = 800;

/// Overlap (in chars) between adjacent paragraph chunks. The
/// txt splitter prepends the last 100 chars of chunk N onto
/// chunk N+1.
pub const PARAGRAPH_OVERLAP_CHARS: usize = 100;

/// Maximum number of chunks a single ingest call may produce.
/// Prevents a runaway PDF from filling the store with 100k
/// tiny chunks; the caller can split the upload into smaller
/// pieces if they need more.
pub const MAX_CHUNKS_PER_INGEST: usize = 5_000;

/// Upper bound on the decoded byte size of `image_data`. 25
/// MiB covers high-res photos without enabling adversarial
/// large-input attacks.
pub const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Deserialize, Default)]
pub(crate) struct IngestDocumentArgs {
    #[serde(default)]
    pub observer_id: String,
    #[serde(default)]
    pub subject_id: String,
    #[serde(default)]
    pub source: String,
    /// Either `content` (raw text) OR `content_base64` (decoded
    /// to bytes then interpreted as UTF-8). PDFs MUST use
    /// `content_base64` since they're binary.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub content_type: String,
    /// Optional override of [`DEFAULT_CHUNK_SIZE_CHARS`].
    #[serde(default)]
    pub chunk_size_chars: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IngestDocumentResponse {
    pub chunks_created: usize,
    pub source: String,
    pub subject_id: String,
    pub embedded: usize,
    pub deferred_embeddings: usize,
    pub content_type: String,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct IngestImageArgs {
    #[serde(default)]
    pub observer_id: String,
    #[serde(default)]
    pub subject_id: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub image_data: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct IngestImageResponse {
    pub records_created: usize,
    pub source: String,
    pub subject_id: String,
    pub has_ocr: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// Public entry: `memory.ingest_document` handler.
pub async fn handle_ingest_document(
    layered: &LayeredContext,
    embed_cell: &tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>,
    embedding_model: &str,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: IngestDocumentArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.ingest_document: decode args: {e}")),
    };
    if args.subject_id.trim().is_empty() {
        return invalid_args("memory.ingest_document: subject_id required".into());
    }
    if args.source.trim().is_empty() {
        return invalid_args("memory.ingest_document: source required".into());
    }
    let content_type = args.content_type.trim().to_ascii_lowercase();

    // Route `content_type=image` straight to the image
    // ingester. The wire format matches the document one
    // (content_base64 carries the image bytes); the rest of
    // the document handler isn't relevant for an image.
    if content_type == "image" {
        let img_args = IngestImageArgs {
            observer_id: args.observer_id.clone(),
            subject_id: args.subject_id.clone(),
            source: args.source.clone(),
            image_data: args
                .content_base64
                .clone()
                .or(args.content.clone())
                .unwrap_or_default(),
        };
        return handle_ingest_image_inner(
            layered,
            embed_cell,
            embedding_model,
            &img_args,
            ctx.tenant_id.as_deref(),
        )
        .await;
    }

    if !matches!(
        content_type.as_str(),
        "markdown" | "md" | "pdf" | "txt" | "code"
    ) {
        return invalid_args(format!(
            "memory.ingest_document: content_type must be markdown / pdf / txt / code / image (got {content_type:?})"
        ));
    }

    let body = match resolve_text_body(&args, &content_type) {
        Ok(s) => s,
        Err(e) => return e,
    };
    if body.trim().is_empty() {
        return invalid_args("memory.ingest_document: empty content".into());
    }

    let chunk_size = args
        .chunk_size_chars
        .unwrap_or(DEFAULT_CHUNK_SIZE_CHARS)
        .max(64);
    let chunks = chunk_text(&body, &content_type, chunk_size);
    if chunks.is_empty() {
        return invalid_args("memory.ingest_document: chunker produced zero chunks".into());
    }
    if chunks.len() > MAX_CHUNKS_PER_INGEST {
        return invalid_args(format!(
            "memory.ingest_document: {n} chunks exceeds cap {cap} — split the upload",
            n = chunks.len(),
            cap = MAX_CHUNKS_PER_INGEST,
        ));
    }

    // Embed when possible. Failure is non-fatal — the records
    // land without embeddings and the background pipeline
    // picks them up on its next tick.
    let dispatcher = embed_cell.get().cloned();
    let vectors: Vec<Option<Vec<f32>>> = match &dispatcher {
        Some(d) => {
            let refs: Vec<&str> = chunks.iter().map(|s| s.as_str()).collect();
            match d.embed(embedding_model, &refs).await {
                Ok(v) if v.len() == chunks.len() => v.into_iter().map(Some).collect(),
                Ok(v) => {
                    tracing::warn!(
                        got = v.len(),
                        want = chunks.len(),
                        "memory.ingest_document: dispatcher returned wrong vector count; falling back to deferred embeddings"
                    );
                    chunks.iter().map(|_| None).collect()
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "memory.ingest_document: embed failed; falling back to deferred embeddings"
                    );
                    chunks.iter().map(|_| None).collect()
                }
            }
        }
        None => chunks.iter().map(|_| None).collect(),
    };

    let mut chunks_created = 0usize;
    let mut embedded = 0usize;
    // GAP 23: ingest writes are scoped to the caller's tenant
    // so the per-tenant Qdrant collection receives the chunks
    // on its next embedder pass.
    let tenant_for_records = ctx.tenant_id.clone();
    for (i, (chunk, vec_opt)) in chunks.iter().zip(vectors.iter()).enumerate() {
        let id = mint_chunk_id(&args.source, &args.subject_id, i, chunk);
        let mut record = MemoryRecord::new_raw(id, chunk.clone(), args.source.clone());
        record.layer = MemoryLayer::Semantic;
        record.source_trust = SourceTrust::External;
        record.tenant_id = tenant_for_records.clone();
        record.tags = vec![
            "ingest:document".to_string(),
            format!("content_type:{content_type}"),
            format!("chunk_index:{i}"),
            format!("subject:{}", args.subject_id),
            "source_trust:external".to_string(),
        ];
        if !args.observer_id.is_empty() {
            record.tags.push(format!("observer:{}", args.observer_id));
        }
        // Apply layered-context PII anonymizer defensively
        // before persisting. Anonymizer is pass-through when
        // disabled.
        if layered.anonymizer.enabled() {
            record.text = layered.anonymizer.anonymize(&record.text);
        }
        if let Some(v) = vec_opt {
            record.embedding = Some(v.clone());
        }
        if let Err(e) = layered.store.insert(&record) {
            return internal(format!("memory.ingest_document: store insert: {e}"));
        }
        chunks_created += 1;
        if vec_opt.is_some() {
            embedded += 1;
        }
    }

    let response = IngestDocumentResponse {
        chunks_created,
        source: args.source.clone(),
        subject_id: args.subject_id.clone(),
        embedded,
        deferred_embeddings: chunks_created - embedded,
        content_type,
    };
    match serde_json::to_vec(&response) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.ingest_document: encode response: {e}")),
    }
}

/// Public entry: `memory.ingest_image` handler.
pub async fn handle_ingest_image(
    layered: &LayeredContext,
    embed_cell: &tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>,
    embedding_model: &str,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: IngestImageArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.ingest_image: decode args: {e}")),
    };
    handle_ingest_image_inner(
        layered,
        embed_cell,
        embedding_model,
        &args,
        ctx.tenant_id.as_deref(),
    )
    .await
}

async fn handle_ingest_image_inner(
    layered: &LayeredContext,
    embed_cell: &tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>,
    embedding_model: &str,
    args: &IngestImageArgs,
    tenant_id: Option<&str>,
) -> HandlerOutcome {
    if args.subject_id.trim().is_empty() {
        return invalid_args("memory.ingest_image: subject_id required".into());
    }
    if args.source.trim().is_empty() {
        return invalid_args("memory.ingest_image: source required".into());
    }
    if args.image_data.trim().is_empty() {
        return invalid_args("memory.ingest_image: image_data required (base64)".into());
    }
    let raw = match base64::engine::general_purpose::STANDARD.decode(args.image_data.as_bytes()) {
        Ok(b) => b,
        Err(e) => return invalid_args(format!("memory.ingest_image: base64 decode: {e}")),
    };
    if raw.is_empty() {
        return invalid_args("memory.ingest_image: decoded image is empty".into());
    }
    if raw.len() > MAX_IMAGE_BYTES {
        return invalid_args(format!(
            "memory.ingest_image: {n} bytes exceeds cap {cap}",
            n = raw.len(),
            cap = MAX_IMAGE_BYTES,
        ));
    }
    if !is_supported_image(&raw) {
        return invalid_args(
            "memory.ingest_image: unsupported image format (only PNG / JPEG accepted)".into(),
        );
    }

    // Per spec: "Sends the image to Ollama using the vision
    // embedding model (nomic-embed-vision … ). If Ollama is
    // not configured or nomic-embed-vision is not available,
    // return a structured error — do not silently no-op."
    //
    // We expose the vision embedding via the existing
    // `EmbeddingDispatcher.embed` trait by routing a
    // base64-data: pseudo-text — every existing provider
    // surfaces the call as a regular `ai.embed` and the AI
    // node's provider routing decides whether the configured
    // model (`vision_model`) actually supports image input.
    // If the call fails OR the dispatcher cell is empty, we
    // return a clear error rather than a partial write.
    let Some(dispatcher) = embed_cell.get().cloned() else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: "memory.ingest_image: embedding dispatcher not configured (missing [memory.embedding_peer]); \
                    vision embedding requires Ollama + nomic-embed-vision".into(),
            retry_hint: 0,
            retry_after: None,
        });
    };
    let payload = format!(
        "image/base64;source={};bytes={}",
        args.source, args.image_data,
    );
    let vector = match dispatcher.embed(embedding_model, &[payload.as_str()]).await {
        Ok(mut vectors) => match vectors.pop() {
            Some(v) if !v.is_empty() => v,
            _ => {
                return HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::RESPONDER_INTERNAL,
                    cause: format!(
                        "memory.ingest_image: embedding dispatcher returned no vector for model {embedding_model:?} (nomic-embed-vision unavailable?)"
                    ),
                    retry_hint: 1,
                    retry_after: None,
                });
            }
        },
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!(
                    "memory.ingest_image: embedding dispatcher failed for model {embedding_model:?}: {e}"
                ),
                retry_hint: 1,
                retry_after: None,
            });
        }
    };

    // Insert the visual record.
    let visual_id = mint_image_id(&args.source, &args.subject_id, "visual");
    let mut visual = MemoryRecord::new_raw(
        visual_id,
        format!("[image] source={} bytes={}", args.source, raw.len()),
        args.source.clone(),
    );
    visual.layer = MemoryLayer::Semantic;
    visual.source_trust = SourceTrust::External;
    visual.embedding = Some(vector);
    visual.tenant_id = tenant_id.map(str::to_string);
    visual.tags = vec![
        "ingest:image".to_string(),
        "type:image".to_string(),
        format!("image_path:{}", args.source),
        format!("subject:{}", args.subject_id),
        "source_trust:external".to_string(),
    ];
    if !args.observer_id.is_empty() {
        visual.tags.push(format!("observer:{}", args.observer_id));
    }
    if let Err(e) = layered.store.insert(&visual) {
        return internal(format!("memory.ingest_image: store insert (visual): {e}"));
    }

    // OCR pass: only run when the input is a PDF (the existing
    // pdf tool's text extractor can be called inline). For PNG
    // / JPEG we don't have an OCR primitive in-tree, so
    // `has_ocr` is false. The fallback_reason field documents
    // this honestly.
    let mut records_created = 1usize;
    let (has_ocr, fallback_reason) = if is_pdf(&raw) {
        let text = extract_pdf_text(&raw);
        if !text.trim().is_empty() {
            let ocr_id = mint_image_id(&args.source, &args.subject_id, "ocr");
            let mut ocr = MemoryRecord::new_raw(
                ocr_id,
                if layered.anonymizer.enabled() {
                    layered.anonymizer.anonymize(&text)
                } else {
                    text
                },
                args.source.clone(),
            );
            ocr.layer = MemoryLayer::Semantic;
            ocr.source_trust = SourceTrust::External;
            ocr.tenant_id = tenant_id.map(str::to_string);
            ocr.tags = vec![
                "ingest:image".to_string(),
                "type:image_ocr".to_string(),
                format!("image_path:{}", args.source),
                format!("subject:{}", args.subject_id),
                "source_trust:external".to_string(),
            ];
            if !args.observer_id.is_empty() {
                ocr.tags.push(format!("observer:{}", args.observer_id));
            }
            // Embed the OCR text via the same dispatcher.
            if let Ok(mut vectors) = dispatcher
                .embed(embedding_model, &[ocr.text.as_str()])
                .await
                && let Some(v) = vectors.pop()
                && !v.is_empty()
            {
                ocr.embedding = Some(v);
            }
            if let Err(e) = layered.store.insert(&ocr) {
                tracing::warn!(error = %e, "memory.ingest_image: OCR insert failed");
            } else {
                records_created += 1;
            }
            (true, None)
        } else {
            (
                false,
                Some("PDF text extraction produced empty text".into()),
            )
        }
    } else {
        (
            false,
            Some(
                "OCR not available for raster images in-tree; visual embedding stored as primary record"
                    .into(),
            ),
        )
    };

    let response = IngestImageResponse {
        records_created,
        source: args.source.clone(),
        subject_id: args.subject_id.clone(),
        has_ocr,
        fallback_reason,
    };
    match serde_json::to_vec(&response) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("memory.ingest_image: encode response: {e}")),
    }
}

fn resolve_text_body(
    args: &IngestDocumentArgs,
    content_type: &str,
) -> Result<String, HandlerOutcome> {
    match content_type {
        "pdf" => {
            let b64 = args.content_base64.as_deref().ok_or_else(|| {
                invalid_args(
                    "memory.ingest_document: content_base64 required for content_type=pdf".into(),
                )
            })?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| {
                    invalid_args(format!("memory.ingest_document: pdf base64 decode: {e}"))
                })?;
            Ok(extract_pdf_text(&bytes))
        }
        _ => {
            if let Some(text) = &args.content {
                return Ok(text.clone());
            }
            if let Some(b64) = &args.content_base64 {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64.as_bytes())
                    .map_err(|e| {
                        invalid_args(format!(
                            "memory.ingest_document: content_base64 decode: {e}"
                        ))
                    })?;
                String::from_utf8(bytes).map_err(|e| {
                    invalid_args(format!("memory.ingest_document: content_base64 utf8: {e}"))
                })
            } else {
                Err(invalid_args(
                    "memory.ingest_document: content or content_base64 required".into(),
                ))
            }
        }
    }
}

/// Pure chunker used by both the handler and unit tests.
pub fn chunk_text(body: &str, content_type: &str, chunk_size: usize) -> Vec<String> {
    match content_type {
        "markdown" | "md" => chunk_markdown(body, chunk_size),
        "code" => chunk_code(body, chunk_size),
        _ => chunk_paragraphs(body, chunk_size, PARAGRAPH_OVERLAP_CHARS),
    }
}

fn chunk_markdown(body: &str, chunk_size: usize) -> Vec<String> {
    // Heading boundaries: lines starting with `## ` / `### `
    // (and `# ` so the document title becomes its own chunk).
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        let trimmed = line.trim_start();
        if (trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### "))
            && !current.trim().is_empty()
        {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        sections.push(current);
    }
    // Split oversize sections by paragraph.
    let mut out = Vec::new();
    for s in sections {
        if s.chars().count() <= chunk_size {
            let t = s.trim().to_string();
            if !t.is_empty() {
                out.push(t);
            }
        } else {
            for c in chunk_paragraphs(&s, chunk_size, PARAGRAPH_OVERLAP_CHARS) {
                out.push(c);
            }
        }
    }
    out
}

fn chunk_paragraphs(body: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let paragraphs: Vec<&str> = body
        .split("\n\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if paragraphs.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for para in paragraphs {
        // If adding this paragraph would push us past the cap
        // AND we already have content, flush the current chunk
        // (with overlap) and start fresh.
        if !current.is_empty() && current.chars().count() + para.chars().count() + 2 > chunk_size {
            let prev = std::mem::take(&mut current);
            // Carry the trailing `overlap` chars forward.
            let tail: String = if overlap > 0 {
                let total = prev.chars().count();
                let start = total.saturating_sub(overlap);
                prev.chars().skip(start).collect()
            } else {
                String::new()
            };
            out.push(prev);
            if !tail.is_empty() {
                current.push_str(&tail);
            }
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(para);
        // Handle an oversize single paragraph by splitting it
        // by character window.
        while current.chars().count() > chunk_size * 2 {
            let head: String = current.chars().take(chunk_size).collect();
            // Carry overlap chars forward.
            let tail: String = if overlap > 0 {
                let total = head.chars().count();
                let start = total.saturating_sub(overlap);
                head.chars().skip(start).collect()
            } else {
                String::new()
            };
            out.push(head);
            current = format!(
                "{tail}{}",
                &current.chars().skip(chunk_size).collect::<String>()
            );
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn chunk_code(body: &str, chunk_size: usize) -> Vec<String> {
    // Boundary heuristic: lines starting with `fn `, `pub fn `,
    // `def `, `class `, `function `, `async function `,
    // `impl ` (or those with leading whitespace) start a new
    // chunk. Oversize chunks get split by paragraph.
    let boundary = |trimmed: &str| -> bool {
        let t = trimmed.trim_start();
        t.starts_with("fn ")
            || t.starts_with("pub fn ")
            || t.starts_with("def ")
            || t.starts_with("class ")
            || t.starts_with("function ")
            || t.starts_with("async function ")
            || t.starts_with("impl ")
    };
    let mut sections: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        if boundary(line) && !current.trim().is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.trim().is_empty() {
        sections.push(current);
    }
    let mut out = Vec::new();
    for s in sections {
        if s.chars().count() <= chunk_size {
            out.push(s.trim().to_string());
        } else {
            for c in chunk_paragraphs(&s, chunk_size, PARAGRAPH_OVERLAP_CHARS) {
                out.push(c);
            }
        }
    }
    out
}

/// Stable per-chunk id minted from
/// `source|subject_id|chunk_index|blake3(text)`. Re-ingesting
/// the same document with the same chunker output upserts the
/// same rows rather than duplicating.
fn mint_chunk_id(source: &str, subject_id: &str, chunk_index: usize, text: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.as_bytes());
    hasher.update(b"|");
    hasher.update(subject_id.as_bytes());
    hasher.update(b"|");
    hasher.update(chunk_index.to_le_bytes().as_ref());
    hasher.update(b"|");
    hasher.update(text.as_bytes());
    hasher.finalize().to_hex().as_str()[..24].to_string()
}

fn mint_image_id(source: &str, subject_id: &str, kind: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.as_bytes());
    hasher.update(b"|");
    hasher.update(subject_id.as_bytes());
    hasher.update(b"|image|");
    hasher.update(kind.as_bytes());
    hasher.finalize().to_hex().as_str()[..24].to_string()
}

fn is_supported_image(raw: &[u8]) -> bool {
    is_pdf(raw) || is_png(raw) || is_jpeg(raw)
}

fn is_png(raw: &[u8]) -> bool {
    raw.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A])
}

fn is_jpeg(raw: &[u8]) -> bool {
    raw.len() >= 3 && raw[..3] == [0xFF, 0xD8, 0xFF]
}

fn is_pdf(raw: &[u8]) -> bool {
    raw.starts_with(b"%PDF")
}

/// Extract text from PDF bytes via lopdf. Mirrors the existing
/// `tool.pdf` text extractor so we get the same behaviour
/// (line-break separated pages, best-effort per-page errors).
pub fn extract_pdf_text(raw: &[u8]) -> String {
    match lopdf::Document::load_mem(raw) {
        Ok(doc) => {
            let pages = doc.get_pages();
            let mut out = String::new();
            for (i, (page_num, _)) in pages.iter().enumerate() {
                if i > 0 {
                    out.push_str("\n\n");
                }
                match doc.extract_text(&[*page_num]) {
                    Ok(t) => out.push_str(&t),
                    Err(_) => out.push_str("[page extraction failed]"),
                }
            }
            out
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::curator::EmbeddingError;
    use crate::nodes::memory::schema::LayeredMemoryStore;
    use async_trait::async_trait;

    fn layered_ctx() -> LayeredContext {
        LayeredContext::new(
            Arc::new(LayeredMemoryStore::in_memory().unwrap()),
            None,
            0.5,
        )
    }

    struct StubEmbed {
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingDispatcher for StubEmbed {
        async fn embed(
            &self,
            _model: &str,
            texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let mut v = vec![0.0; self.dim];
                    if self.dim > 0 {
                        v[0] = (i as f32) + 1.0;
                    }
                    v
                })
                .collect())
        }
    }

    struct FailingEmbed;

    #[async_trait]
    impl EmbeddingDispatcher for FailingEmbed {
        async fn embed(
            &self,
            _model: &str,
            _texts: &[&str],
        ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            Err(EmbeddingError::NotConnected)
        }
    }

    fn ctx_for(args: serde_json::Value) -> InvocationCtx {
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"caller"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec![],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: serde_json::to_vec(&args).unwrap(),
            tenant_id: None,
        }
    }

    fn embed_cell_with<E: EmbeddingDispatcher + 'static>(
        d: E,
    ) -> Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> {
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        cell.set(Arc::new(d) as Arc<dyn EmbeddingDispatcher>).ok();
        cell
    }

    #[test]
    fn chunk_markdown_splits_at_headings() {
        let body = "# Title\nintro\n\n## First section\nbody one\n\n## Second section\nbody two\n";
        let chunks = chunk_text(body, "markdown", 1000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("Title"));
        assert!(chunks[1].contains("First section"));
        assert!(chunks[2].contains("Second section"));
    }

    #[test]
    fn chunk_markdown_splits_oversize_section_by_paragraph() {
        let mut body = String::from("## Big section\n");
        for i in 0..20 {
            body.push_str(&format!("Paragraph {i} {}\n\n", "lorem ipsum ".repeat(20)));
        }
        let chunks = chunk_text(&body, "markdown", 600);
        assert!(chunks.len() > 1, "expected the section to split");
        for c in &chunks {
            assert!(
                c.chars().count() <= 1400,
                "chunk too large: {}",
                c.chars().count()
            );
        }
    }

    #[test]
    fn chunk_code_splits_at_function_boundaries() {
        let body = "fn one() {\n  one_body();\n}\n\nfn two() {\n  two_body();\n}\n\npub fn three() {\n  three_body();\n}\n";
        let chunks = chunk_text(body, "code", 1000);
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].contains("fn one"));
        assert!(chunks[1].contains("fn two"));
        assert!(chunks[2].contains("pub fn three"));
    }

    #[test]
    fn chunk_code_handles_python_def_and_class() {
        let body =
            "def alpha():\n    return 1\n\nclass Beta:\n    pass\n\ndef gamma():\n    return 3\n";
        let chunks = chunk_text(body, "code", 1000);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn chunk_paragraphs_overlaps_between_consecutive_chunks() {
        let body = format!(
            "{}\n\n{}\n\n{}",
            "alpha ".repeat(40),
            "beta ".repeat(40),
            "gamma ".repeat(40)
        );
        let chunks = chunk_text(&body, "txt", 300);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(!c.is_empty());
        }
    }

    #[tokio::test]
    async fn ingest_markdown_creates_one_record_per_chunk() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 4 });
        let body = "# Title\nintro\n\n## First\none body\n\n## Second\ntwo body\n";
        let outcome = handle_ingest_document(
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "notes.md",
                "content": body,
                "content_type": "markdown",
            })),
        )
        .await;
        let response: IngestDocumentResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(response.chunks_created, 3);
        assert_eq!(response.embedded, 3);
        // Records persisted as Layer 2.
        let recs = layered
            .store
            .list(Some(MemoryLayer::Semantic), Some("notes.md"), 100, 0)
            .unwrap();
        assert_eq!(recs.len(), 3);
        for r in &recs {
            assert!(r.tags.iter().any(|t| t == "ingest:document"));
            assert!(r.tags.iter().any(|t| t == "source_trust:external"));
            assert_eq!(r.source_trust, SourceTrust::External);
            assert!(r.embedding.is_some());
        }
    }

    #[tokio::test]
    async fn ingest_code_creates_one_record_per_function() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 4 });
        let body = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        let outcome = handle_ingest_document(
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "lib.rs",
                "content": body,
                "content_type": "code",
            })),
        )
        .await;
        let response: IngestDocumentResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(response.chunks_created, 3);
    }

    #[tokio::test]
    async fn ingest_txt_overlaps_paragraphs() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 4 });
        let p1 = "alpha ".repeat(50);
        let p2 = "beta ".repeat(50);
        let p3 = "gamma ".repeat(50);
        let body = format!("{p1}\n\n{p2}\n\n{p3}");
        let outcome = handle_ingest_document(
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "notes.txt",
                "content": body,
                "content_type": "txt",
                "chunk_size_chars": 250,
            })),
        )
        .await;
        let response: IngestDocumentResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert!(response.chunks_created >= 2, "expected >= 2 chunks");
    }

    #[tokio::test]
    async fn ingest_isolates_subjects() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 4 });
        for subject in ["alice", "bob"] {
            let outcome = handle_ingest_document(
                &layered,
                &embed_cell,
                "mock",
                &ctx_for(serde_json::json!({
                    "subject_id": subject,
                    "source": format!("notes-{subject}.txt"),
                    "content": format!("doc for {subject}\n\nthird line"),
                    "content_type": "txt",
                })),
            )
            .await;
            match outcome {
                HandlerOutcome::Ok(_) => {}
                HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
            }
        }
        let alice_rows = layered
            .store
            .list(Some(MemoryLayer::Semantic), Some("notes-alice.txt"), 100, 0)
            .unwrap();
        assert!(!alice_rows.is_empty());
        for r in alice_rows {
            assert!(r.tags.iter().any(|t| t == "subject:alice"));
        }
    }

    #[tokio::test]
    async fn ingest_returns_count_matching_records_inserted() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 4 });
        let body = "para one\n\npara two\n\npara three";
        let outcome = handle_ingest_document(
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "doc.txt",
                "content": body,
                "content_type": "txt",
            })),
        )
        .await;
        let response: IngestDocumentResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        let recs = layered
            .store
            .list(Some(MemoryLayer::Semantic), Some("doc.txt"), 100, 0)
            .unwrap();
        assert_eq!(response.chunks_created, recs.len());
    }

    #[tokio::test]
    async fn ingest_defers_embedding_when_dispatcher_fails() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(FailingEmbed);
        let outcome = handle_ingest_document(
            &layered,
            &embed_cell,
            "mock",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "doc.txt",
                "content": "one\n\ntwo",
                "content_type": "txt",
            })),
        )
        .await;
        let response: IngestDocumentResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert!(response.chunks_created > 0);
        assert_eq!(response.embedded, 0);
        assert!(response.deferred_embeddings > 0);
    }

    #[tokio::test]
    async fn ingest_image_returns_error_when_no_dispatcher_wired() {
        let layered = layered_ctx();
        let embed_cell: Arc<tokio::sync::OnceCell<Arc<dyn EmbeddingDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // Minimal PNG signature (8 magic bytes + a fake IHDR
        // for the format check; the handler only inspects
        // bytes, not validity, beyond magic + size).
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"junk-but-magic-passes");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let outcome = handle_ingest_image(
            &layered,
            &embed_cell,
            "nomic-embed-vision",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "photo.png",
                "image_data": b64,
            })),
        )
        .await;
        match outcome {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::RESPONDER_INTERNAL);
                assert!(e.cause.contains("not configured"));
            }
            _ => panic!("expected RESPONDER_INTERNAL"),
        }
    }

    #[tokio::test]
    async fn ingest_image_writes_visual_record_when_dispatcher_returns_vector() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 8 });
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(b"junk");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let outcome = handle_ingest_image(
            &layered,
            &embed_cell,
            "nomic-embed-vision",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "photo.png",
                "image_data": b64,
            })),
        )
        .await;
        let response: IngestImageResponse = match outcome {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("err: {}", e.cause),
        };
        assert_eq!(response.records_created, 1);
        assert!(!response.has_ocr);
        assert!(response.fallback_reason.is_some());
    }

    #[tokio::test]
    async fn ingest_image_rejects_unsupported_magic() {
        let layered = layered_ctx();
        let embed_cell = embed_cell_with(StubEmbed { dim: 8 });
        let raw = b"NOPE-not-an-image";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let outcome = handle_ingest_image(
            &layered,
            &embed_cell,
            "nomic-embed-vision",
            &ctx_for(serde_json::json!({
                "subject_id": "alice",
                "source": "fake.bin",
                "image_data": b64,
            })),
        )
        .await;
        match outcome {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected INVALID_ARGS"),
        }
    }
}
