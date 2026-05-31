//! Embedded memory surface — document ingest + text search.
//!
//! Wraps the public API of
//! [`relix_runtime::nodes::memory::schema::LayeredMemoryStore`] so a
//! host app doesn't have to assemble [`MemoryRecord`]s by hand or
//! understand the four-layer schema to use the basic ingest / search
//! happy path.
//!
//! Honest scope limits:
//!
//! - Search uses SQLite `LIKE` (the store's `text_search` helper),
//!   NOT Qdrant. That matches the embedded-mode promise of "no
//!   external binary dependencies". A host that needs proper
//!   vector search wires Qdrant via the full mesh.
//! - Ingest produces Layer 2 (`Semantic`) records, one per chunk.
//!   The chunker is paragraph-based with a 100-char overlap — same
//!   default as the runtime's `memory.ingest_document` cap.
//! - Embeddings are NOT generated here. The vector column stays
//!   `NULL`; a future pass can plumb the configured provider's
//!   `generate_embeddings` through, but the embedded chunker stays
//!   useful without it (search still hits via text LIKE).

use serde::{Deserialize, Serialize};

use relix_runtime::nodes::memory::schema::{MemoryLayer, MemoryRecord};

use crate::{EmbeddedError, RelixEmbedded};

/// Per-call chunker ceiling — guards against a runaway PDF filling
/// the store with hundreds of thousands of tiny chunks. Matches the
/// runtime's `MAX_CHUNKS_PER_INGEST` for cross-mode parity.
const MAX_CHUNKS_PER_INGEST: usize = 5_000;

/// Default char-level overlap between adjacent text chunks. 100 char
/// of trailing context on chunk N+1 preserves enough cross-chunk
/// semantics for substring search to find queries that straddle a
/// paragraph boundary.
const PARAGRAPH_OVERLAP_CHARS: usize = 100;

/// Request body for
/// [`RelixEmbedded::memory_ingest_document`](RelixEmbedded::memory_ingest_document).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemoryIngestInput {
    /// User / agent the document is about. Used as the SQLite
    /// `source` column on every resulting chunk row.
    pub subject_id: String,
    /// Verbatim text. UTF-8.
    pub content: String,
    /// `"markdown"` | `"txt"` | `"code"`. Anything else is rejected.
    pub content_type: String,
    /// Operator-visible source label appended to the `source` column
    /// as a `source:<label>` tag for downstream filtering. May be
    /// empty.
    pub source: String,
    /// PART 6 — tenant the chunks are written under. `None`
    /// or empty falls back to the runtime's
    /// [`RelixEmbedded::default_tenant_id`]. When the runtime
    /// has `tenant_isolation = true` AND neither value
    /// resolves, the call returns
    /// [`EmbeddedError::MissingTenant`].
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// Return value of `memory_ingest_document`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryIngestResult {
    /// Number of chunk rows written to the store.
    pub chunks_created: usize,
    /// `subject_id` the rows were written under (echoed from the input).
    pub subject_id: String,
    /// Operator-visible source label echoed from the input.
    pub source: String,
    /// Sanitised content type the chunker accepted.
    pub content_type: String,
}

/// Request body for
/// [`RelixEmbedded::memory_search`](RelixEmbedded::memory_search).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemorySearchInput {
    /// Free-form search string. Substring match against the chunk
    /// text.
    pub query: String,
    /// Optional filter: only return rows whose `source` matches this
    /// `subject_id`. Empty string means "any subject".
    pub subject_id: String,
    /// Max rows. Clamped to `[1, 1000]` by the store.
    pub limit: usize,
    /// PART 6 — tenant the search runs against. `None` or
    /// empty falls back to the runtime's
    /// [`RelixEmbedded::default_tenant_id`]. When the runtime
    /// has `tenant_isolation = true` AND neither value
    /// resolves, the call returns
    /// [`EmbeddedError::MissingTenant`]. When the runtime is
    /// tenant-isolated AND a tenant resolves, only rows whose
    /// `tenant_id` column matches are returned — the SQLite
    /// `WHERE tenant_id = ?` filter ships with the store's
    /// `text_search_for_tenant` method.
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// One row returned by `memory_search`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemoryHit {
    /// Stable record id assigned by the store.
    pub id: String,
    /// Verbatim chunk text.
    pub text: String,
    /// `source` column the chunk was written under (typically the
    /// caller's `subject_id`).
    pub source: String,
    /// Stringified layer (`raw` | `semantic` | `observation` | `model`).
    pub layer: String,
    /// Tags the record carried at write time.
    pub tags: Vec<String>,
    /// Unix-seconds timestamp of the original ingest.
    pub observed_at: i64,
}

impl RelixEmbedded {
    /// Ingest a text document into Layer 2 (`Semantic`).
    ///
    /// Chunks the input by paragraph with a 100-char overlap, then
    /// writes one [`MemoryRecord`] per chunk to the store. Each
    /// record's `source` column is set to `subject_id`; a `source`
    /// tag (when supplied) is appended so a future query can scope
    /// by document.
    pub async fn memory_ingest_document(
        &self,
        input: MemoryIngestInput,
    ) -> Result<MemoryIngestResult, EmbeddedError> {
        if input.subject_id.trim().is_empty() {
            return Err(EmbeddedError::Ingest("subject_id is required".into()));
        }
        if input.content.trim().is_empty() {
            return Err(EmbeddedError::Ingest("content is empty".into()));
        }
        let content_type = input.content_type.trim().to_ascii_lowercase();
        if !matches!(
            content_type.as_str(),
            "markdown" | "md" | "txt" | "code" | "text"
        ) {
            return Err(EmbeddedError::Ingest(format!(
                "unsupported content_type: {content_type:?} (use markdown / txt / code)"
            )));
        }

        let chunk_size = self.chunk_size_chars();
        let chunks = chunk_text(&input.content, chunk_size);
        if chunks.is_empty() {
            return Err(EmbeddedError::Ingest("chunker produced zero chunks".into()));
        }
        if chunks.len() > MAX_CHUNKS_PER_INGEST {
            return Err(EmbeddedError::Ingest(format!(
                "{n} chunks exceeds the {cap} cap — split the upload",
                n = chunks.len(),
                cap = MAX_CHUNKS_PER_INGEST,
            )));
        }

        // PART 6: resolve the effective tenant + stamp every
        // chunk row's `tenant_id` so tenant-aware reads
        // (`text_search_for_tenant`, `get_for_tenant`) filter
        // correctly. `resolve_tenant` returns
        // `Err(MissingTenant)` when tenant_isolation is on
        // AND nothing resolves.
        let tenant_for_chunks =
            self.resolve_tenant(input.tenant_id.as_deref(), "memory_ingest_document")?;

        let store = self.memory_store().clone();
        let subject_id = input.subject_id.clone();
        let source_tag = input.source.clone();
        let content_type_for_closure = content_type.clone();
        let count = chunks.len();
        tokio::task::spawn_blocking(move || -> Result<(), EmbeddedError> {
            for (idx, chunk) in chunks.into_iter().enumerate() {
                let id = chunk_id(&subject_id, &source_tag, idx, &chunk);
                let mut record = MemoryRecord::new_raw(id, chunk, &subject_id);
                record.layer = MemoryLayer::Semantic;
                record.tenant_id = tenant_for_chunks.clone();
                if !source_tag.is_empty() {
                    record.tags.push(format!("source:{source_tag}"));
                }
                record
                    .tags
                    .push(format!("content_type:{content_type_for_closure}"));
                store.insert(&record)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| EmbeddedError::Config(format!("ingest task: {e}")))??;

        Ok(MemoryIngestResult {
            chunks_created: count,
            subject_id: input.subject_id,
            source: input.source,
            content_type,
        })
    }

    /// Substring search over Layer 1 + 2 records. When
    /// `subject_id` is non-empty, only rows whose `source` column
    /// equals `subject_id` are returned.
    ///
    /// PART 6: when the runtime has `tenant_isolation = true`,
    /// the search runs through
    /// [`LayeredMemoryStore::text_search_for_tenant`] so
    /// every returned row's `tenant_id` matches the
    /// resolved tenant. A missing tenant id (per-call empty
    /// AND `default_tenant_id` unset) returns
    /// [`EmbeddedError::MissingTenant`]. When isolation is
    /// off, the legacy tenant-blind path runs (every row
    /// regardless of tenant column).
    pub async fn memory_search(
        &self,
        input: MemorySearchInput,
    ) -> Result<Vec<MemoryHit>, EmbeddedError> {
        let limit = if input.limit == 0 { 5 } else { input.limit };
        // PART 6: resolve the effective tenant. Returns
        // `Err(MissingTenant)` when isolation is on AND
        // nothing resolves.
        let tenant = self.resolve_tenant(input.tenant_id.as_deref(), "memory_search")?;
        let isolation_on = self.tenant_isolation_enabled();
        let store = self.memory_store().clone();
        let query = input.query.clone();
        let subject_id = input.subject_id.clone();
        let raw = tokio::task::spawn_blocking(move || {
            if isolation_on {
                store.text_search_for_tenant(&query, limit, tenant.as_deref())
            } else {
                store.text_search(&query, limit)
            }
        })
        .await
        .map_err(|e| EmbeddedError::Config(format!("search task: {e}")))??;
        let filtered: Vec<MemoryHit> = raw
            .into_iter()
            .filter(|r| subject_id.is_empty() || r.source == subject_id)
            .map(MemoryHit::from)
            .collect();
        Ok(filtered)
    }
}

impl From<MemoryRecord> for MemoryHit {
    fn from(r: MemoryRecord) -> Self {
        Self {
            id: r.id,
            text: r.text,
            source: r.source,
            layer: r.layer.as_str().to_string(),
            tags: r.tags,
            observed_at: r.observed_at,
        }
    }
}

/// Paragraph-based chunker with a small char-level overlap. Same
/// shape as the runtime's `chunk_text` (the runtime keeps that
/// helper crate-private so we reimplement the simple version here
/// rather than wedge a `pub(crate)` accessor into the runtime).
///
/// Empty paragraphs are dropped. When a single paragraph exceeds
/// `chunk_size_chars` it is hard-split into `chunk_size_chars`-sized
/// pieces so the cap holds.
pub(crate) fn chunk_text(body: &str, chunk_size_chars: usize) -> Vec<String> {
    let target = chunk_size_chars.max(64);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in body.split("\n\n") {
        let p = paragraph.trim();
        if p.is_empty() {
            continue;
        }
        if current.is_empty() {
            current.push_str(p);
        } else if current.len() + 2 + p.len() <= target {
            current.push_str("\n\n");
            current.push_str(p);
        } else {
            chunks.extend(split_oversize(&current, target));
            // Snap the byte offset down to a char boundary: a raw
            // `len - overlap` offset can land inside a multi-byte UTF-8
            // codepoint and panic when sliced on multilingual input.
            let mut overlap_from = current.len().saturating_sub(PARAGRAPH_OVERLAP_CHARS);
            while overlap_from < current.len() && !current.is_char_boundary(overlap_from) {
                overlap_from += 1;
            }
            current = format!("{}\n\n{}", &current[overlap_from..], p);
        }
    }
    if !current.is_empty() {
        chunks.extend(split_oversize(&current, target));
    }
    chunks
}

fn split_oversize(s: &str, target: usize) -> Vec<String> {
    if s.len() <= target {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let bytes = s.as_bytes();
    while start < bytes.len() {
        // Walk a char-boundary-safe end position.
        let mut end = (start + target).min(bytes.len());
        while end < bytes.len() && (bytes[end] & 0b1100_0000) == 0b1000_0000 {
            end += 1;
        }
        out.push(s[start..end].to_string());
        start = end;
    }
    out
}

fn chunk_id(subject_id: &str, source: &str, idx: usize, body: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(subject_id.as_bytes());
    h.update(b"|");
    h.update(source.as_bytes());
    h.update(b"|");
    h.update(&(idx as u64).to_le_bytes());
    h.update(b"|");
    h.update(body.as_bytes());
    let hex = h.finalize().to_hex();
    format!("sem-{}", &hex.as_str()[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a >5 KB document of multi-byte text (Chinese +
    /// emoji, paragraphs separated by `\n\n`) must chunk without
    /// panicking. Before the boundary-safe fix, the
    /// `&current[overlap_from..]` slice in `chunk_text` panicked
    /// when `len - PARAGRAPH_OVERLAP_CHARS` landed inside a
    /// multi-byte UTF-8 codepoint.
    #[test]
    fn chunk_text_handles_multibyte_document_without_panic() {
        let paragraph = "这是一段包含中文和表情符号的测试文本😀🚀🌟，\
                         用来验证分块器在多字节输入上的字节边界处理是否正确。\
                         每个段落都足够长，以反复触发重叠拼接路径，\
                         从而暴露原先在 UTF-8 码点中间切片导致的崩溃缺陷。✨🔥💡🌈🎯";
        let mut doc = String::new();
        for _ in 0..40 {
            doc.push_str(paragraph);
            doc.push_str("\n\n");
        }
        assert!(
            doc.len() > 5 * 1024,
            "document must exceed 5KB, got {} bytes",
            doc.len()
        );

        // The actual assertion is "does not panic". A small chunk
        // size forces the overlap-stitch path repeatedly.
        let chunks = chunk_text(&doc, 256);

        assert!(!chunks.is_empty());
        // Every emitted chunk is valid UTF-8 (a `String` always is)
        // and non-empty.
        for c in &chunks {
            assert!(!c.is_empty());
        }
    }
}
